//! Presenting a rendered frame to the X server: the MIT-SHM shared-memory
//! segment and the zero-copy `ShmPutImage` blit. Pure transport — the pixels
//! themselves come from `crate::render`; layout decides *when* to blit.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};

use super::types::{Wm, WmError, R};

/// A memfd-backed shared memory segment attached to the server via
/// `ShmAttachFd`. The fd is handed to the server at attach time; the local
/// mapping outlives it.
///
/// The mapping is split into two halves used as alternating frame buffers:
/// the blit path writes frame N+1 into one half while the server may still
/// be reading frame N from the other, so no per-frame round trip is needed
/// to serialise reuse (see `Wm::blit_fb`).
pub struct ShmSeg {
    /// Server-side segment id (XID).
    seg: u32,
    ptr: *mut u8,
    len: usize,
    /// Which half the next frame is written into.
    half: usize,
    /// Per-half: whether an unconfirmed `ShmPutImage` reading that half is
    /// (potentially) still in flight. Set on put, cleared by the round trip
    /// `Wm::blit_fb` performs before overwriting a pending half.
    pending: [bool; 2],
}

impl ShmSeg {
    /// # Safety
    /// `ptr` must be the start of a live `MAP_SHARED` mapping of at least
    /// `len` bytes that this `ShmSeg` uniquely owns: `slice()` will hand out
    /// `&mut [u8]` views of it and `Drop` will `munmap(ptr, len)`.
    unsafe fn new(seg: u32, ptr: *mut u8, len: usize) -> Self {
        Self {
            seg,
            ptr,
            len,
            half: 0,
            pending: [false; 2],
        }
    }

    /// Byte capacity of one half (a frame must fit in this).
    fn half_len(&self) -> usize {
        self.len / 2
    }

    /// Byte offset of the current half within the segment.
    fn offset(&self) -> usize {
        self.half * self.half_len()
    }

    /// The first `len` bytes of the *current half* of the mapping (callers
    /// size frames to fit; see `half_len`).
    fn slice(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.half_len());
        // SAFETY: ptr/len describe a live MAP_SHARED mapping owned by self,
        // and offset + len stays within it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(self.offset()), len) }
    }
}

impl Drop for ShmSeg {
    fn drop(&mut self) {
        // SAFETY: mapping was created by mmap with this exact ptr/len.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

impl Wm {
    // --- frame blits (MIT-SHM, required) ---

    /// Blit a rendered framebuffer to a drawable: the pixels are presented
    /// straight into the shared segment and shipped as one zero-copy
    /// `ShmPutImage`. The segment holds two frame-sized halves used
    /// alternately: the put goes out unchecked (errors surface as
    /// `Event::Error` like every other unchecked request), and reuse of a
    /// half is serialised by a round trip before overwriting it while a put
    /// reading it may still be in flight. In steady state that costs one
    /// round trip every other blit, and (X being FIFO) the reply queues
    /// behind the immediately preceding put — an intentional pacing point:
    /// rendering never gets more than one full frame ahead of the server.
    pub(crate) fn blit_fb(&mut self, drawable: Window, fb: &pixel_graphics::Framebuffer) -> R<()> {
        let (w, h) = (fb.width as u16, fb.height as u16);
        let len = fb.width * fb.height * 4;
        self.ensure_shm(len)?;
        let seg = self.shm.as_mut().expect("ensure_shm succeeded");
        if seg.pending[seg.half] {
            // Any round trip confirms every earlier request (the X
            // stream is FIFO), including both halves' puts.
            self.conn.get_input_focus()?.reply()?;
            seg.pending = [false; 2];
        }
        self.renderer.present_into_slice(fb, seg.slice(len));
        let (seg_id, offset) = (seg.seg, seg.offset());
        use x11rb::protocol::shm::ConnectionExt as _;
        self.conn.shm_put_image(
            drawable,
            self.gc,
            w,
            h,
            0,
            0,
            w,
            h,
            0,
            0,
            self.depth,
            u8::from(ImageFormat::Z_PIXMAP),
            false,
            seg_id,
            offset as u32,
        )?;
        let seg = self.shm.as_mut().expect("checked above");
        seg.pending[seg.half] = true;
        seg.half ^= 1;
        Ok(())
    }

    /// Make sure the SHM segment exists and each of its two halves holds at
    /// least `len` bytes, creating it on first use and recreating it when a
    /// frame outgrows it (`RandR` growth). There is no fallback: a server
    /// without MIT-SHM 1.2 fd-passing can't run splitwm, and the error from
    /// the session's first blit (inside the startup arrange) is what says
    /// so and exits.
    fn ensure_shm(&mut self, len: usize) -> R<()> {
        if self.shm.as_ref().is_some_and(|seg| seg.half_len() >= len) {
            return Ok(());
        }
        if let Some(seg) = self.shm.take() {
            use x11rb::protocol::shm::ConnectionExt as _;
            // Detach the outgrown segment server-side; the mapping itself is
            // unmapped by `ShmSeg`'s Drop.
            let _ = self.conn.shm_detach(seg.seg);
        }
        // Size to the workarea when that's bigger, so the common full-screen
        // frame never triggers a second create right after a small one.
        // Doubled: the segment holds two alternating frame halves.
        let wa = self.wa();
        let len = 2 * len.max((wa.w.max(1) as usize) * (wa.h.max(1) as usize) * 4);
        let seg = self
            .create_shm(len)
            .map_err(|e| WmError::from(format!("MIT-SHM required: {e}")))?;
        self.shm = Some(seg);
        Ok(())
    }

    /// Create a memfd-backed shared segment of `len` bytes, map it, and
    /// attach it to the server with `ShmAttachFd` (MIT-SHM 1.2's fd-passing
    /// attach: no `SysV` shm ids, no /dev/shm files to leak). The fd is owned
    /// by the attach request once sent; the local mapping stays valid.
    fn create_shm(&self, len: usize) -> R<ShmSeg> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use x11rb::connection::RequestConnection;
        use x11rb::protocol::shm::{self, ConnectionExt as _};
        if self
            .conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err("MIT-SHM extension not present".into());
        }
        // Version probe doubles as an fd-passing capability check: attach-fd
        // needs 1.2, and a server that old enough to lack it errors here.
        let v = self.conn.shm_query_version()?.reply()?;
        if (v.major_version, v.minor_version) < (1, 2) {
            return Err(format!("MIT-SHM {}.{} < 1.2", v.major_version, v.minor_version).into());
        }
        let raw = unsafe { libc::memfd_create(c"splitwm-shm".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // From here the fd is owned (closed on any early return).
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        // Owns the mapping until ShmSeg takes over: the error paths below
        // (`generate_id`, a refused attach) must munmap it, not leak it.
        struct MapGuard(*mut libc::c_void, usize);
        impl Drop for MapGuard {
            fn drop(&mut self) {
                // SAFETY: mapping was created by mmap with this exact ptr/len.
                unsafe {
                    libc::munmap(self.0, self.1);
                }
            }
        }
        let guard = MapGuard(ptr, len);
        let seg = self.conn.generate_id()?;
        // Checked: an attach refusal (e.g. an SSH-forwarded display) must
        // surface here, where the caller can fall back, not as a later
        // async error on the first blit.
        self.conn.shm_attach_fd(seg, fd, false)?.check()?;
        std::mem::forget(guard);
        // SAFETY: ptr is a fresh MAP_SHARED mapping of exactly `len` bytes,
        // owned solely by the returned ShmSeg (the fd was moved into the
        // server attach; only the mapping remains on our side).
        Ok(unsafe { ShmSeg::new(seg, ptr.cast(), len) })
    }
}
