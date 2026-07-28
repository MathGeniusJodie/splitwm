//! wlr-screencopy-unstable-v1: how screenshot and recording clients read
//! the screen. A client asks for a frame of the output — the whole thing or
//! a region — and the next redraw composites that same scene once more into
//! an offscreen buffer, then hands the pixels back in the client's shm
//! buffer. grim is the client `Mod4+S` drives (`theme::SCREENSHOT_CMD`);
//! wf-recorder and wayshot speak the same protocol.
//!
//! A capture renders its own frame rather than reading back the one just
//! presented: the presented framebuffer belongs to whichever backend owns
//! it (a DRM scanout buffer, a winit surface, the headless renderbuffer),
//! while the single GLES renderer they all composite through is reachable
//! from here on every backend.
//!
//! Only shm buffers are served — the frame's `linux_dmabuf` event is never
//! sent, so clients that could import a dmabuf fall back to shm.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::{Bind as _, Color32F, ExportMem as _, Offscreen as _};
use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::{
    self, ZwlrScreencopyManagerV1,
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource as _,
};
use smithay::utils::{
    Buffer as BufferCoord, Clock, Logical, Monotonic, Physical, Point, Rectangle, Size, Transform,
};
use smithay::wayland::shm::BufferData;

use super::{cursor, scene, Comp};

/// Protocol version published. v3 adds the `buffer_done` handshake, which
/// older clients neither expect nor get (`frame.version()` gates it).
const VERSION: u32 = 3;

/// The one shm format captures are served in. Every screenshot tool
/// understands it, and it is what a wlroots session hands out on the
/// hardware splitwm runs on, so it is the path clients exercise most.
const SHM_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// What the GLES renderer reads a framebuffer back as: `RGBA` bytes, the
/// one 8-bit layout GLES guarantees for `glReadPixels`. `write_shm`
/// reorders them into `SHM_FORMAT`'s.
const READ_FORMAT: Fourcc = Fourcc::Abgr8888;

/// Bytes per pixel, shared by both formats above.
const BPP: i32 = 4;

/// A frame object's state. A frame is created armed with everything its
/// capture needs and stays that way until its one `copy` — or it is born
/// failed (an empty region), which no copy can revive.
pub enum FrameData {
    Armed {
        /// The output pixels to grab, already clipped to the output.
        region: Rectangle<i32, Physical>,
        /// Composite the pointer into the capture (`overlay_cursor`).
        overlay_cursor: bool,
        /// Latches on the first copy: a second one is `already_used`.
        /// Atomic because resource data is shared across threads by the
        /// protocol machinery, never because two threads copy at once.
        used: AtomicBool,
    },
    Failed,
}

/// A copy request waiting for the next composited frame.
struct Capture {
    frame: ZwlrScreencopyFrameV1,
    /// The client's shm buffer, validated against `region` at copy time.
    buffer: WlBuffer,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
    /// `copy_with_damage`: the client wants a `damage` event before
    /// `ready`, and waits for the screen to change rather than asking for
    /// a frame of its own.
    with_damage: bool,
}

impl Capture {
    /// Tell the client this capture will never arrive.
    fn fail(&self) {
        self.frame.failed();
    }

    /// Hand the client the frame it asked for: the rows are top-down, so
    /// no `y_invert` flag rides along.
    fn ready(&self) {
        self.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
        if self.with_damage {
            let size = self.region.size;
            self.frame.damage(0, 0, size.w as u32, size.h as u32);
        }
        let now = Duration::from(Clock::<Monotonic>::new().now());
        let secs = now.as_secs();
        self.frame
            .ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
    }
}

/// The published screencopy global and the captures queued against it.
pub struct Screencopy {
    /// Never read, but dropping it would unpublish the screencopy global.
    #[allow(dead_code)]
    global: GlobalId,
    /// Copies waiting for the next composited frame, in request order.
    pending: Vec<Capture>,
    /// The offscreen frame captures render into, kept between them so a
    /// recorder's stream doesn't allocate one per frame.
    target: Option<GlesRenderbuffer>,
}

/// The disjoint `Comp` borrows a capture render needs, so serving can run
/// inside `redraw`'s live scene borrows.
pub struct CaptureCtx<'a> {
    pub renderer: &'a mut GlesRenderer,
    pub scene: &'a scene::Scene<'a>,
    pub clear: Color32F,
    /// Where the seat pointer is and what it draws as — composited in only
    /// for the captures that asked for the cursor.
    pub pointer_loc: Point<f64, Logical>,
    pub cursor_status: &'a CursorImageStatus,
    pub cursors: &'a mut cursor::CursorCache,
}

impl Screencopy {
    pub fn new(dh: &DisplayHandle) -> Screencopy {
        Screencopy {
            global: dh.create_global::<Comp, ZwlrScreencopyManagerV1, _>(VERSION, ()),
            pending: Vec::new(),
            target: None,
        }
    }

    /// Serve every capture queued since the last frame from `ctx`'s scene,
    /// failing the ones the renderer or the client's buffer can't satisfy.
    /// Called once per redraw, right after the backend presented.
    pub fn serve(&mut self, ctx: &mut CaptureCtx<'_>) {
        for capture in std::mem::take(&mut self.pending) {
            if !capture.frame.is_alive() {
                continue;
            }
            if self.grab(ctx, &capture) {
                capture.ready();
            } else {
                capture.fail();
            }
        }
    }

    /// Composite the scene offscreen and copy `capture`'s region into its
    /// client buffer. `false` on any renderer or buffer failure.
    fn grab(&mut self, ctx: &mut CaptureCtx<'_>, capture: &Capture) -> bool {
        let size = output_rect(ctx.scene.output).size;
        if !Rectangle::from_size(size).contains_rect(capture.region) {
            // The output resized under a capture promised against the old
            // size; the client's buffer no longer fits what it asked for.
            return false;
        }
        // Cursor elements first: the capture stacks them over the scene,
        // exactly where the backends composite the pointer.
        let mut elements = if capture.overlay_cursor {
            cursor::cursor_elements(
                ctx.renderer,
                ctx.scene.indexed,
                ctx.pointer_loc,
                ctx.cursor_status,
                ctx.cursors,
            )
        } else {
            Vec::new()
        };
        elements.extend(scene::output_elements(ctx.renderer, ctx.scene));

        let Some(target) = target(&mut self.target, ctx.renderer, size) else {
            return false;
        };
        let mut fb = match ctx.renderer.bind(target) {
            Ok(fb) => fb,
            Err(err) => {
                tracing::error!("capture bind: {err}");
                return false;
            }
        };
        // A tracker per capture, so every capture is a full redraw of its
        // own buffer rather than a damage-diff against someone else's.
        let mut damage = OutputDamageTracker::new(size, 1.0, Transform::Normal);
        if let Err(err) = damage.render_output(ctx.renderer, &mut fb, 0, &elements, ctx.clear) {
            tracing::error!("capture render: {err:?}");
            return false;
        }

        // Rows arrive top-down: the renderer draws offscreen targets
        // y-inverted, which exactly cancels glReadPixels' bottom-up order
        // (the same readback `backend::headless` does for the harness).
        let region = Rectangle::<i32, BufferCoord>::new(
            (capture.region.loc.x, capture.region.loc.y).into(),
            (capture.region.size.w, capture.region.size.h).into(),
        );
        let mapping = match ctx.renderer.copy_framebuffer(&fb, region, READ_FORMAT) {
            Ok(mapping) => mapping,
            Err(err) => {
                tracing::error!("capture readback: {err}");
                return false;
            }
        };
        match ctx.renderer.map_texture(&mapping) {
            Ok(rgba) => write_shm(&capture.buffer, capture.region.size, rgba),
            Err(err) => {
                tracing::error!("capture map: {err}");
                false
            }
        }
    }
}

/// The offscreen frame captures render into, (re)created whenever the
/// output size changed under it.
fn target<'a>(
    slot: &'a mut Option<GlesRenderbuffer>,
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
) -> Option<&'a mut GlesRenderbuffer> {
    let size = Size::<i32, BufferCoord>::from((size.w, size.h));
    if slot.as_ref().is_none_or(|buffer| buffer.size() != size) {
        *slot = renderer
            .create_buffer(READ_FORMAT, size)
            .inspect_err(|err| tracing::error!("capture buffer: {err}"))
            .ok();
    }
    slot.as_mut()
}

/// The output's pixel rect. Captures are requested in output *logical*
/// coordinates; splitwm composites at scale 1 with no transform (every
/// element places with `to_physical(1)`), so the two coincide.
fn output_rect(output: &Output) -> Rectangle<i32, Physical> {
    Rectangle::from_size(output.current_mode().expect("output has a mode").size)
}

/// Whether the pool behind `data` actually covers `size` rows of pixels —
/// the bound `write_shm`'s writes rely on.
fn covers(data: &BufferData, len: usize, size: Size<i32, Physical>) -> bool {
    let last_row_end = i64::from(data.offset)
        + i64::from(data.stride) * i64::from(size.h - 1)
        + i64::from(size.w) * i64::from(BPP);
    data.offset >= 0 && data.stride >= size.w * BPP && last_row_end <= len as i64
}

/// Whether `buffer` is the shm buffer the frame's `buffer` event asked
/// for. A larger stride is fine; a different format or size is not.
fn shm_matches(buffer: &WlBuffer, size: Size<i32, Physical>) -> bool {
    smithay::wayland::shm::with_buffer_contents(buffer, |_ptr, len, data| {
        data.format == SHM_FORMAT
            && data.width == size.w
            && data.height == size.h
            && covers(&data, len, size)
    })
    .unwrap_or(false)
}

/// Copy the readback's `RGBA` rows into the client's `SHM_FORMAT` buffer.
/// `false` if the buffer's pool no longer covers what it promised.
fn write_shm(buffer: &WlBuffer, size: Size<i32, Physical>, rgba: &[u8]) -> bool {
    let row = (size.w * BPP) as usize;
    if rgba.len() < row * size.h as usize {
        return false;
    }
    smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        if !covers(&data, len, size) {
            return false;
        }
        for y in 0..size.h as usize {
            let src = &rgba[y * row..][..row];
            // SAFETY: `covers` proved the pool spans every row at this
            // offset and stride, and the pool stays mapped for the call.
            let dst = unsafe {
                std::slice::from_raw_parts_mut(
                    ptr.add(data.offset as usize + y * data.stride as usize),
                    row,
                )
            };
            for (dst, src) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                // `RGBA` bytes into `Xrgb8888`'s little-endian `BGRX`. The
                // ignored byte goes out opaque, so a tool that reads it as
                // alpha anyway gets a visible image.
                dst.copy_from_slice(&[src[2], src[1], src[0], 0xff]);
            }
        }
        true
    })
    .unwrap_or(false)
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Comp {
    fn bind(
        _state: &mut Comp,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Comp>,
    ) {
        data_init.init(manager, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Comp {
    fn request(
        state: &mut Comp,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Comp>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        // Whichever output the client named, it is ours: splitwm drives one
        // output by design (the same assumption `comp::layers` makes).
        let full = output_rect(&state.output);
        let (frame, overlay_cursor, region) = match request {
            Request::CaptureOutput {
                frame,
                overlay_cursor,
                ..
            } => (frame, overlay_cursor, Some(full)),
            Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                x,
                y,
                width,
                height,
                ..
            } => {
                let asked = (width > 0 && height > 0)
                    .then(|| Rectangle::new((x, y).into(), (width, height).into()));
                (
                    frame,
                    overlay_cursor,
                    asked.and_then(|asked| full.intersection(asked)),
                )
            }
            _ => return,
        };
        let Some(region) = region else {
            // Nothing of the output is in view: the frame exists only to
            // carry the failure back.
            data_init.init(frame, FrameData::Failed).failed();
            return;
        };
        let frame = data_init.init(
            frame,
            FrameData::Armed {
                region,
                overlay_cursor: overlay_cursor != 0,
                used: AtomicBool::new(false),
            },
        );
        frame.buffer(
            SHM_FORMAT,
            region.size.w as u32,
            region.size.h as u32,
            (region.size.w * BPP) as u32,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Comp {
    fn request(
        state: &mut Comp,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Comp>,
    ) {
        use zwlr_screencopy_frame_v1::Request;
        let (buffer, with_damage) = match request {
            Request::Copy { buffer } => (buffer, false),
            Request::CopyWithDamage { buffer } => (buffer, true),
            // Destroy: `destroyed` below drops whatever this frame queued.
            _ => return,
        };
        let FrameData::Armed {
            region,
            overlay_cursor,
            used,
        } = data
        else {
            // Born failed; the client was told so and owes us only a
            // destroy.
            return;
        };
        if used.swap(true, Ordering::Relaxed) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "frame has already copied a buffer",
            );
            return;
        }
        if !shm_matches(&buffer, region.size) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "buffer is not the shm buffer this frame asked for",
            );
            return;
        }
        state.screencopy.pending.push(Capture {
            frame: frame.clone(),
            buffer,
            region: *region,
            overlay_cursor: *overlay_cursor,
            with_damage,
        });
        // A plain copy wants the next frame whether or not anything else
        // is dirty, so it is its own reason to composite; a
        // `copy_with_damage` client is asking to wait for real damage
        // instead, and rides whatever redraw that damage queues.
        if !with_damage {
            state.queue_redraw();
        }
    }

    fn destroyed(
        state: &mut Comp,
        _client: ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &FrameData,
    ) {
        state.screencopy.pending.retain(|c| c.frame != *frame);
    }
}
