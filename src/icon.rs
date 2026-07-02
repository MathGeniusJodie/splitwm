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
}

/// Decode a PNG file (e.g. a launcher icon resolved from the icon theme)
/// into an `Icon`.
pub fn load_png(path: &std::path::Path) -> Option<Icon> {
    let (w, h, pixels) = pixel_graphics::decode_png_with_size(path.to_str()?).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let argb = pixels
        .iter()
        .map(|p| {
            (u32::from(p.a) << 24)
                | (u32::from(p.r) << 16)
                | (u32::from(p.g) << 8)
                | u32::from(p.b)
        })
        .collect();
    Some(Icon {
        w: w as u32,
        h: h as u32,
        argb,
    })
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
    Icon {
        w: icon.w,
        h: icon.h,
        argb: icon.argb.iter().map(|&px| f(px)).collect(),
    }
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
    (a << 24)
        | (u32::from(snapped.r) << 16)
        | (u32::from(snapped.g) << 8)
        | u32::from(snapped.b)
}
