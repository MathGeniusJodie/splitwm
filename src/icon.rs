//! Application-icon colour pipeline: snapping fetched icons onto the na16
//! palette and the OKLCH hue-rotation used for same-app disambiguation.
//! Both run once per icon fetch / assignment (see `Wm::fetch_icon` and
//! `Wm::refresh_icon_rotations`), never per frame — the per-pixel OKLCH
//! math is far too heavy for the blit path.

#![allow(clippy::cast_possible_truncation)]

use pixel_graphics::{Palette, Rgb};

/// A decoded application icon (non-premultiplied ARGB pixels, row-major).
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub argb: Vec<u32>,
    /// Process-unique id, used as a render-cache key. A raw pointer is not
    /// usable for that: icons are dropped and reallocated (e.g. every
    /// `_NET_WM_ICON` refresh), and the allocator can hand a new icon the
    /// old address, silently serving the dead icon's cached pixels.
    id: u64,
}

impl Icon {
    pub fn new(w: u32, h: u32, mut argb: Vec<u32>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Renderers index `argb[y*w + x]` unchecked, so `len >= w*h` must
        // hold by construction. Icon data can originate from
        // client-controlled properties: pad a short buffer with transparent
        // pixels rather than panic.
        let need = (w as usize).saturating_mul(h as usize);
        if argb.len() < need {
            argb.resize(need, 0);
        }
        Self {
            w,
            h,
            argb,
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Widest icon dimension worth decoding: anything larger than this is not a
/// plausible app icon, and a hostile PNG header can otherwise demand a
/// multi-gigabyte pixel allocation before we ever see the size.
const MAX_ICON_DIM: u32 = 2048;

/// The width/height a PNG file *declares* in its IHDR chunk (always the
/// first chunk: bytes 16..24, big-endian), read without decoding any pixel
/// data — the cheap pre-decode size check for untrusted files. Verifies the
/// PNG signature and the IHDR chunk tag first, so arbitrary non-PNG bytes
/// aren't misread as dimensions.
pub fn png_declared_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !bytes.starts_with(&PNG_SIG) || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

/// Decode a PNG file (e.g. a launcher icon resolved from the icon theme)
/// into an `Icon`. Icon paths come from `.desktop` `Icon=` entries — found
/// on disk rather than trusted — so the declared size is checked before the
/// decoder is allowed to allocate for it.
pub fn load_png(path: &std::path::Path) -> Option<Icon> {
    let bytes = std::fs::read(path).ok()?;
    let (dw, dh) = png_declared_dims(&bytes)?;
    if dw == 0 || dh == 0 || dw > MAX_ICON_DIM || dh > MAX_ICON_DIM {
        return None;
    }
    let (w, h, pixels) = pixel_graphics::decode_png_bytes(&bytes).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let argb = pixels
        .iter()
        .map(|p| {
            (u32::from(p.a) << 24) | (u32::from(p.r) << 16) | (u32::from(p.g) << 8) | u32::from(p.b)
        })
        .collect();
    Some(Icon::new(w as u32, h as u32, argb))
}

/// Snap every non-transparent pixel in `icon` to the nearest na16 palette
/// colour (alpha is kept as-is), so app icons render as flat pixel art
/// matching the rest of the UI's 16-colour chrome.
pub fn quantize(palette: &Palette, icon: &Icon) -> Icon {
    map_argb(icon, |px| quantize_argb(palette, px))
}

/// Hue-rotate a whole icon by `deg` degrees (OKLCH) and re-quantize, so a
/// rotated icon stays as flatly pixel-art as the un-rotated source.
pub fn rotate(palette: &Palette, icon: &Icon, deg: f32) -> Icon {
    map_argb(icon, |px| {
        quantize_argb(palette, crate::oklch::rotate_hue_argb(px, deg))
    })
}

fn map_argb(icon: &Icon, f: impl Fn(u32) -> u32) -> Icon {
    Icon::new(icon.w, icon.h, icon.argb.iter().map(|&px| f(px)).collect())
}

fn quantize_argb(palette: &Palette, px: u32) -> u32 {
    let a = px >> 24;
    if a == 0 {
        return px;
    }
    let rgb = Rgb {
        r: ((px >> 16) & 0xff) as u8,
        g: ((px >> 8) & 0xff) as u8,
        b: (px & 0xff) as u8,
    };
    let snapped = palette.color(palette.nearest_index(rgb));
    (a << 24) | (u32::from(snapped.r) << 16) | (u32::from(snapped.g) << 8) | u32::from(snapped.b)
}

#[cfg(test)]
mod tests {
    use super::png_declared_dims;

    #[test]
    fn png_dims_reject_non_png_bytes() {
        // Arbitrary bytes long enough to reach 16..24 must not be misread
        // as dimensions.
        assert_eq!(png_declared_dims(&[0u8; 64]), None);
        assert_eq!(
            png_declared_dims(b"JFIF-not-a-png-but-long-enough-data"),
            None
        );
    }

    #[test]
    fn png_dims_read_a_real_header() {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend([0, 0, 0, 13]); // IHDR length
        bytes.extend(*b"IHDR");
        bytes.extend(16u32.to_be_bytes());
        bytes.extend(32u32.to_be_bytes());
        assert_eq!(png_declared_dims(&bytes), Some((16, 32)));
    }
}
