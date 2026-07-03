//! OKLab/OKLCH <-> sRGB conversions (Björn Ottosson's matrices).
//!
//! Used to hue-rotate app icon bitmaps for same-app window disambiguation
//! (`theme::icon_hue_rotation`, `Wm::icon_hue`) — a real per-pixel hue shift
//! of the icon's own colors, not a swatch/overlay.

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    let c = c.max(0.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055_f32.mul_add(c.powf(1.0 / 2.4), -0.055)
    }
}

fn linear_to_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let l = 0.412_221_47f32.mul_add(r, 0.536_332_54f32.mul_add(g, 0.051_445_99 * b));
    let m = 0.211_903_5f32.mul_add(r, 0.680_699_5_f32.mul_add(g, 0.107_396_96 * b));
    let s = 0.088_302_46f32.mul_add(r, 0.281_718_84f32.mul_add(g, 0.629_978_7 * b));
    let (l_, m_, s_) = (l.cbrt(), m.cbrt(), s.cbrt());
    (
        0.210_454_26f32.mul_add(l_, 0.793_617_8f32.mul_add(m_, -0.004_072_047 * s_)),
        1.977_998_5f32.mul_add(l_, (-2.428_592_2f32).mul_add(m_, 0.450_593_7 * s_)),
        0.025_904_037f32.mul_add(l_, 0.782_771_77f32.mul_add(m_, -0.808_675_77 * s_)),
    )
}

fn oklab_to_linear(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = 0.396_337_78f32.mul_add(a, l) + 0.215_803_76 * b;
    let m_ = (-0.105_561_34f32).mul_add(a, l) - 0.063_854_17 * b;
    let s_ = (-0.089_484_18f32).mul_add(a, l) - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        4.076_741_7f32.mul_add(l3, (-3.307_711_6f32).mul_add(m3, 0.230_969_9 * s3)),
        (-1.268_438f32).mul_add(l3, 2.609_757_4f32.mul_add(m3, -0.341_319_4 * s3)),
        (-0.004_196_1f32).mul_add(l3, (-0.703_418_6f32).mul_add(m3, 1.707_614_7 * s3)),
    )
}

/// Rotate a straight (non-premultiplied) ARGB pixel's hue by `degrees` in
/// OKLCH space, preserving lightness, chroma and alpha. Near-grey pixels
/// (chroma ~0) have no hue to rotate and pass through unchanged, so icon
/// outlines/shading stay put while the saturated art rotates.
pub fn rotate_hue_argb(argb: u32, degrees: f32) -> u32 {
    let a = argb >> 24;
    let r = f32::from(((argb >> 16) & 0xff) as u8) / 255.0;
    let g = f32::from(((argb >> 8) & 0xff) as u8) / 255.0;
    let b = f32::from((argb & 0xff) as u8) / 255.0;

    let (l, oa, ob) = linear_to_oklab(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
    let c = oa.hypot(ob);
    if c < 1e-4 {
        return argb;
    }
    let h = ob.atan2(oa) + degrees.to_radians();
    let (na, nb) = (c * h.cos(), c * h.sin());
    let (nr, ng, nb2) = oklab_to_linear(l, na, nb);
    // Per-channel clamping is not gamut mapping: a rotated colour that lands
    // outside sRGB has each channel saturate independently, which shifts its
    // hue/chroma rather than scaling chroma down. Acceptable here because
    // every output is immediately snapped onto the 16-colour na16 palette
    // (`icon::quantize`), whose quantisation error dwarfs the clamp's.
    let enc = |x: f32| (linear_to_srgb(x).clamp(0.0, 1.0) * 255.0).round() as u32;
    (a << 24) | (enc(nr) << 16) | (enc(ng) << 8) | enc(nb2)
}
