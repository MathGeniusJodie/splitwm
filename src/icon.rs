//! Application-icon colour pipeline: snapping fetched icons onto the na16
//! palette and the OKLCH hue-rotation used for same-app disambiguation.
//! Both run once per icon fetch / assignment (see `Wm::fetch_icon` and
//! `Wm::refresh_icon_rotations`), never per frame — the per-pixel OKLCH
//! math is far too heavy for the blit path.

#![allow(clippy::cast_possible_truncation)]

use pixel_graphics::{Rgb, Rgba};

use crate::oklch::OklabPalette;

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

/// Widest icon dimension worth keeping: anything larger than this is not a
/// plausible app icon.
const MAX_ICON_DIM: usize = 2048;

/// Decode any image file (PNG, SVG, JPEG, WebP, …) to straight
/// (non-premultiplied) 8-bit RGBA with ImageMagick (`magick`, falling back
/// to the IM6 `convert` name) — image decoding deliberately lives in the
/// magick process, not this binary, whose own resource limits also bound
/// what a hostile file can demand. `-background none` keeps SVG/transparent
/// sources' alpha instead of flattening onto white. Runs on user-chosen
/// files (wallpaper, theme icons), never per frame; a missing ImageMagick
/// just means that image is skipped, with a hint on stderr. Images wider or
/// taller than `max_dim` are rejected before their pixels are copied.
pub(crate) fn magick_decode_rgba(
    path: &str,
    max_dim: usize,
) -> Option<(usize, usize, Vec<Rgba>)> {
    for prog in ["magick", "convert"] {
        match std::process::Command::new(prog)
            .args(["-background", "none"])
            .arg(path)
            // Force RGBA at 8 bits so the PAM header below is the only
            // shape magick can emit (grayscale sources would otherwise
            // come out GRAYSCALE_ALPHA).
            .args(["-colorspace", "sRGB", "-type", "truecoloralpha", "-depth", "8", "pam:-"])
            .output()
        {
            Ok(out) if out.status.success() => return parse_pam_rgba(&out.stdout, max_dim),
            Ok(out) => {
                eprintln!(
                    "splitwm: {prog} failed on {path}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return None;
            }
            // Not installed under this name; try the next.
            Err(_) => {}
        }
    }
    eprintln!("splitwm: decoding {path} needs ImageMagick (magick/convert) installed");
    None
}

/// Parse a PAM (P7) image as produced by `magick ... pam:-`: an ASCII
/// header of `KEY value` lines up to `ENDHDR`, then raw samples. Hand-rolled
/// rather than a decoder dependency because the binary intentionally has no
/// image decoding — this header is a handful of ASCII lines, and the RGBA
/// shape is forced by the magick invocation, so anything else is rejected.
/// The `max_dim` cap is enforced here, on the declared WIDTH/HEIGHT, so an
/// implausibly large image never gets its pixel buffer copied at all.
fn parse_pam_rgba(bytes: &[u8], max_dim: usize) -> Option<(usize, usize, Vec<Rgba>)> {
    let mut rest = bytes.strip_prefix(b"P7\n")?;
    let (mut w, mut h) = (None, None);
    loop {
        let nl = rest.iter().position(|&b| b == b'\n')?;
        let line = std::str::from_utf8(&rest[..nl]).ok()?;
        rest = &rest[nl + 1..];
        match line.split_once(' ') {
            _ if line == "ENDHDR" => break,
            Some(("WIDTH", v)) => w = v.trim().parse::<usize>().ok(),
            Some(("HEIGHT", v)) => h = v.trim().parse::<usize>().ok(),
            Some(("DEPTH", "4") | ("MAXVAL", "255") | ("TUPLTYPE", "RGB_ALPHA")) => {}
            Some(("DEPTH" | "MAXVAL" | "TUPLTYPE", _)) => return None,
            _ => {}
        }
    }
    let (w, h) = (w?, h?);
    if w == 0 || h == 0 || w > max_dim || h > max_dim || rest.len() < w * h * 4 {
        return None;
    }
    let pixels = rest[..w * h * 4]
        .chunks_exact(4)
        .map(|p| Rgba {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
        .collect();
    Some((w, h, pixels))
}

/// Decode an icon image file (e.g. a launcher icon resolved from the icon
/// theme, found on disk via `.desktop` `Icon=` entries rather than trusted)
/// into an `Icon` via `magick_decode_rgba`.
pub fn load_image(path: &std::path::Path) -> Option<Icon> {
    let (w, h, rgba) = magick_decode_rgba(&path.to_string_lossy(), MAX_ICON_DIM)?;
    let argb = rgba
        .iter()
        .map(|p| {
            (u32::from(p.a) << 24)
                | (u32::from(p.r) << 16)
                | (u32::from(p.g) << 8)
                | u32::from(p.b)
        })
        .collect();
    Some(Icon::new(w as u32, h as u32, argb))
}

/// Snap every non-transparent pixel in `icon` to the nearest na16 palette
/// colour (alpha is kept as-is), so app icons render as flat pixel art
/// matching the rest of the UI's 16-colour chrome.
pub fn quantize(palette: &OklabPalette, icon: &Icon) -> Icon {
    map_argb(icon, |px| quantize_argb(palette, px))
}

/// Hue-rotate a whole icon by `deg` degrees (OKLCH) and re-quantize, so a
/// rotated icon stays as flatly pixel-art as the un-rotated source.
pub fn rotate(palette: &OklabPalette, icon: &Icon, deg: f32) -> Icon {
    map_argb(icon, |px| {
        quantize_argb(palette, crate::oklch::rotate_hue_argb(px, deg))
    })
}

fn map_argb(icon: &Icon, f: impl Fn(u32) -> u32) -> Icon {
    Icon::new(icon.w, icon.h, icon.argb.iter().map(|&px| f(px)).collect())
}

fn quantize_argb(palette: &OklabPalette, px: u32) -> u32 {
    let a = px >> 24;
    if a == 0 {
        return px;
    }
    let rgb = Rgb {
        r: ((px >> 16) & 0xff) as u8,
        g: ((px >> 8) & 0xff) as u8,
        b: (px & 0xff) as u8,
    };
    let snapped = palette.inner().color(palette.nearest_index(rgb));
    (a << 24) | (u32::from(snapped.r) << 16) | (u32::from(snapped.g) << 8) | u32::from(snapped.b)
}

#[cfg(test)]
mod tests {
    use super::parse_pam_rgba;

    fn pam(header: &str, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn pam_parses_the_shape_magick_emits() {
        let bytes = pam(
            "P7\nWIDTH 2\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
            &[1, 2, 3, 4, 5, 6, 7, 8],
        );
        let px = |r, g, b, a| pixel_graphics::Rgba { r, g, b, a };
        assert_eq!(
            parse_pam_rgba(&bytes, 2048),
            Some((2, 1, vec![px(1, 2, 3, 4), px(5, 6, 7, 8)]))
        );
    }

    #[test]
    fn pam_rejects_non_rgba_truncated_oversized_and_non_pam_input() {
        // Wrong tuple type / depth / maxval.
        for hdr in [
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 2\nMAXVAL 255\nTUPLTYPE GRAYSCALE_ALPHA\nENDHDR\n",
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 65535\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
        ] {
            assert_eq!(parse_pam_rgba(&pam(hdr, &[0; 8]), 2048), None);
        }
        // Fewer samples than WIDTH*HEIGHT*4 claims.
        let hdr = "P7\nWIDTH 2\nHEIGHT 2\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n";
        assert_eq!(parse_pam_rgba(&pam(hdr, &[0; 15]), 2048), None);
        // Declared dimensions above the cap are rejected before any copy.
        assert_eq!(parse_pam_rgba(&pam(hdr, &[0; 16]), 1), None);
        // Arbitrary non-PAM bytes.
        assert_eq!(parse_pam_rgba(b"JFIF-not-a-pam", 2048), None);
        assert_eq!(parse_pam_rgba(&[0u8; 64], 2048), None);
    }
}
