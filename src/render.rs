//! Software rendering of leaf decorations (tab bar, focus border, content
//! background) with tiny-skia. Produces a BGRX byte buffer ready for X
//! `PutImage` on a depth-24 `TrueColor` visual.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::unnecessary_cast,
    clippy::many_single_char_names,
    clippy::tuple_array_conversions
)]

use std::rc::Rc;

use fontdue::Font;
use pixel_graphics::{
    Framebuffer, Paint as PgPaint, Palette as PgPalette, Rgb as PgRgb, Sprite, Swap, TRANSPARENT,
};
use tiny_skia::{
    Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Rect as SkRect,
    Stroke, Transform,
};

use crate::theme::{self, palette_color};
use crate::Index;

/// Embedded-art PNG bytes, relative to the crate root (where the bitmap
/// assets live alongside `Cargo.toml`).
macro_rules! asset {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $name))
    };
}

pub struct Renderer {
    font: Font,
    /// The na16 palette, kept around (beyond building the variants below) so
    /// `accent_rgb` can look up a colour without a second hardcoded table.
    palette: PgPalette,
    /// Screen-sized scaled wallpaper; frame backgrounds copy their slice of it.
    wallpaper: Option<Pixmap>,
    /// Bitmap window border, pre-rendered once per accent palette index by
    /// `pixel_graphics` palette-swapping `winborder.png`'s titlebar/outline
    /// colours.
    border_variants: [NineSlice; 16],
    /// The `winmin.png` restore strip for a minimized *column* (squished
    /// narrow, so the strip runs vertically) and `winmin_h.png` for a
    /// minimized *row* (squished short, strip runs horizontally) — picked in
    /// `draw_leaf` by the minimized leaf's own aspect ratio. Palette-swapped
    /// per accent index like the border, so a minimized split keeps its
    /// colour.
    minimized: [MinimizedSlice; 16],
    minimized_h: [MinimizedSliceH; 16],
    /// Titlebar buttons, palette-swapped like the border so each leaf's
    /// buttons match its accent, indexed by `BtnIcon::index`. Disabled art
    /// tracks the accent too, except `LIME` (see
    /// `load_disabled_button_variants`); `Minimize`/`MinimizeH` are two
    /// separate slots (not enabled/disabled of the same button) — see
    /// `BtnIcon::MinimizeH`.
    buttons: [ButtonVariant; BtnIcon::COUNT],
}

/// One titlebar button's art, pre-rendered per accent palette index.
struct ButtonVariant {
    normal: [Icon; 16],
    disabled: [Icon; 16],
}

impl ButtonVariant {
    fn load(
        palette: &PgPalette,
        lut: &[[u8; 4]; 256],
        bytes: &[u8],
        disabled_bytes: &[u8],
    ) -> Self {
        Self {
            normal: load_button_variants(palette, lut, bytes),
            disabled: load_disabled_button_variants(palette, lut, disabled_bytes),
        }
    }
}

/// A decoded application icon (non-premultiplied ARGB pixels, row-major).
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub argb: Vec<u32>,
}

pub struct TabInfo {
    pub label: char,
    pub icon: Option<Rc<Icon>>,
    /// Icon hue-rotation (degrees) for same-app disambiguation (see
    /// `Wm::icon_hue`); `None` unless a sibling window is also open.
    pub icon_hue: Option<f32>,
}

pub struct LeafView {
    pub w: i32,
    pub h: i32, // frame height (content height + gap)
    pub tb_h: i32,
    pub bw: i32,
    /// Palette index this split's border and titlebar buttons are swapped to.
    pub accent_index: Index,
    /// The split's single window, if any.
    pub tab: Option<TabInfo>,
    /// Collapsed to a thin restore strip; renders as `winmin.png` only.
    pub minimized: bool,
}

fn argb(c: u32) -> Color {
    let a = ((c >> 24) & 0xff) as u8;
    let r = ((c >> 16) & 0xff) as u8;
    let g = ((c >> 8) & 0xff) as u8;
    let b = (c & 0xff) as u8;
    Color::from_rgba8(r, g, b, a)
}

fn pixmap_to_icon(pm: &Pixmap) -> Icon {
    let (w, h) = (pm.width(), pm.height());
    let data = pm.data();
    let mut argb = Vec::with_capacity((w * h) as usize);
    for px in data.chunks_exact(4) {
        let (r, g, b, a) = (
            u32::from(px[0]),
            u32::from(px[1]),
            u32::from(px[2]),
            u32::from(px[3]),
        );
        // Un-premultiply (tiny-skia stores premultiplied RGBA) into straight ARGB.
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else {
            (r * 255 / a, g * 255 / a, b * 255 / a)
        };
        argb.push((a << 24) | (r << 16) | (g << 8) | b);
    }
    Icon { w, h, argb }
}

/// Crop a `w`x`h` region out of `src` at (x, y) into a new owned `Pixmap`.
fn crop(src: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Pixmap {
    let mut out = Pixmap::new(w.max(1), h.max(1)).unwrap();
    let sw = src.width();
    let sdata = src.data();
    let odata = out.data_mut();
    for row in 0..h {
        let sy = y + row;
        if sy >= src.height() {
            break;
        }
        for col in 0..w {
            let sx = x + col;
            if sx >= sw {
                break;
            }
            let si = ((sy * sw + sx) * 4) as usize;
            let oi = ((row * w + col) * 4) as usize;
            odata[oi..oi + 4].copy_from_slice(&sdata[si..si + 4]);
        }
    }
    out
}

/// Blit `src` at screen offset (x, y), repeating it (tiling) to exactly fill
/// a `dst_w`x`dst_h` box, one image pixel per screen pixel — no scaling, no
/// interpolation, so pixel art stays crisp. When `dst_w`/`dst_h` equal
/// `src`'s size (e.g. a 9-slice corner), this draws the source exactly once.
fn blit_tile(pm: &mut PixmapMut, src: &Pixmap, x: f32, y: f32, dst_w: f32, dst_h: f32) {
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    if sw == 0 || sh == 0 {
        return;
    }
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let sdata = src.data();
    let data = pm.data_mut();
    let ox = x.round() as i32;
    let oy = y.round() as i32;
    let w = dst_w.round() as i32;
    let h = dst_h.round() as i32;
    for ty in 0..h {
        let py = oy + ty;
        if py < 0 || py >= ph {
            continue;
        }
        let sy = ty.rem_euclid(sh);
        for tx in 0..w {
            let px = ox + tx;
            if px < 0 || px >= pw {
                continue;
            }
            let sx = tx.rem_euclid(sw);
            let si = ((sy * sw + sx) * 4) as usize;
            let a = u32::from(sdata[si + 3]);
            if a == 0 {
                continue;
            }
            let idx = ((py * pw + px) * 4) as usize;
            // Both src and dst are premultiplied RGBA: plain source-over.
            for k in 0..3 {
                let sc = u32::from(sdata[si + k]);
                let dst = u32::from(data[idx + k]);
                data[idx + k] = (sc + dst * (255 - a) / 255).min(255) as u8;
            }
            data[idx + 3] = 255;
        }
    }
}

/// A bitmap 9-slice: 4 fixed corners plus 4 edges/a center that repeat to
/// fill an arbitrary target rect, drawn at native resolution (1:1 pixels).
struct NineSlice {
    tl: Pixmap,
    tr: Pixmap,
    bl: Pixmap,
    br: Pixmap,
    top: Pixmap,
    bottom: Pixmap,
    left: Pixmap,
    right: Pixmap,
    center: Pixmap,
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

impl NineSlice {
    /// The reference art bakes decorative close/minimize/etc. icons into the
    /// titlebar band (around native x=191..249); real buttons draw on top of
    /// those positions separately, so the top/bottom edges' stretchable strip
    /// is sampled from an icon-free column range instead of the full span
    /// between the corners (which would smear that art across the bar).
    const EDGE_SAMPLE_X0: u32 = 20;
    const EDGE_SAMPLE_X1: u32 = 180;

    fn new(src: &Pixmap, l: i32, t: i32, r: i32, b: i32) -> Self {
        let (w, h) = (src.width(), src.height());
        let (lu, tu, ru, bu) = (l as u32, t as u32, r as u32, b as u32);
        let edge_w = Self::EDGE_SAMPLE_X1 - Self::EDGE_SAMPLE_X0;
        Self {
            tl: crop(src, 0, 0, lu, tu),
            tr: crop(src, w - ru, 0, ru, tu),
            bl: crop(src, 0, h - bu, lu, bu),
            br: crop(src, w - ru, h - bu, ru, bu),
            top: crop(src, Self::EDGE_SAMPLE_X0, 0, edge_w, tu),
            bottom: crop(src, Self::EDGE_SAMPLE_X0, h - bu, edge_w, bu),
            left: crop(src, 0, tu, lu, h - tu - bu),
            right: crop(src, w - ru, tu, ru, h - tu - bu),
            center: crop(src, Self::EDGE_SAMPLE_X0, tu, edge_w, h - tu - bu),
            l,
            t,
            r,
            b,
        }
    }

    fn draw(&self, pm: &mut PixmapMut, ox: f32, oy: f32, w: f32, h: f32) {
        let (l, t, r, b) = (self.l as f32, self.t as f32, self.r as f32, self.b as f32);
        let mid_w = (w - l - r).max(1.0);
        let mid_h = (h - t - b).max(1.0);

        blit_tile(pm, &self.tl, ox, oy, l, t);
        blit_tile(pm, &self.tr, ox + w - r, oy, r, t);
        blit_tile(pm, &self.bl, ox, oy + h - b, l, b);
        blit_tile(pm, &self.br, ox + w - r, oy + h - b, r, b);

        blit_tile(pm, &self.top, ox + l, oy, mid_w, t);
        blit_tile(pm, &self.bottom, ox + l, oy + h - b, mid_w, b);
        blit_tile(pm, &self.left, ox, oy + t, l, mid_h);
        blit_tile(pm, &self.right, ox + w - r, oy + t, r, mid_h);
        blit_tile(pm, &self.center, ox + l, oy + t, mid_w, mid_h);
    }
}

/// The accent remap shared by the border and its titlebar buttons: the
/// titlebar/body fill (`LAVENDER`) becomes `index`, the outline (`PURPLE`)
/// becomes its hand-picked darker counterpart (`theme::DARKER_INDEX`), and
/// the highlight stroke (`CREAM`) becomes its hand-picked lighter
/// counterpart (`theme::LIGHTER_INDEX`).
fn accent_swap(index: Index) -> Swap {
    Swap::identity()
        .set(palette_color::LAVENDER, PgPaint::Solid(index))
        .set(
            palette_color::PURPLE,
            PgPaint::Solid(theme::darker_index(index)),
        )
        .set(
            palette_color::CREAM,
            PgPaint::Solid(theme::lighter_index(index)),
        )
}

/// Palette-swap `sprite` via `accent_swap`, for each of the 16 na16 indices —
/// exact palette colours only, no brightness scaling.
fn swap_accent_variants(
    sprite: &Sprite,
    palette: &PgPalette,
    lut: &[[u8; 4]; 256],
) -> [Pixmap; 16] {
    std::array::from_fn(|index| {
        let swap = accent_swap(index as Index);
        render_swapped_sprite(sprite, palette, &swap, lut)
    })
}

/// Render `winborder.png` once per na16 palette index (0..16), palette
/// swapping its titlebar/outline colours — the persistent per-leaf accent.
fn load_border_variants(palette: &PgPalette, lut: &[[u8; 4]; 256]) -> [NineSlice; 16] {
    let sprite =
        Sprite::load_native_bytes(asset!("winborder.png"), palette).expect("winborder.png");
    swap_accent_variants(&sprite, palette, lut).map(|pm| {
        NineSlice::new(
            &pm,
            theme::BORDER_LEFT,
            theme::BORDER_TOP,
            theme::BORDER_RIGHT,
            theme::BORDER_BOTTOM,
        )
    })
}

/// Render `winmin.png`/`winmin_h.png` once per na16 palette index, palette
/// swapping their fill/outline/highlight colours like the border — a
/// minimized leaf's restore strip keeps its split's persistent accent.
fn load_minimized_variants(palette: &PgPalette, lut: &[[u8; 4]; 256]) -> [MinimizedSlice; 16] {
    let sprite = Sprite::load_native_bytes(asset!("winmin.png"), palette).expect("winmin.png");
    swap_accent_variants(&sprite, palette, lut).map(|pm| MinimizedSlice::new(&pm))
}

fn load_minimized_h_variants(palette: &PgPalette, lut: &[[u8; 4]; 256]) -> [MinimizedSliceH; 16] {
    let sprite = Sprite::load_native_bytes(asset!("winmin_h.png"), palette).expect("winmin_h.png");
    swap_accent_variants(&sprite, palette, lut).map(|pm| MinimizedSliceH::new(&pm))
}

/// Load a titlebar button PNG, palette-swapped to each of the 16 accents via
/// `accent_swap`, plus one extra `Swap` rule from `extra` layered on top —
/// e.g. the disabled variant's `LIME` override.
fn load_button_variants_with(
    palette: &PgPalette,
    lut: &[[u8; 4]; 256],
    bytes: &[u8],
    extra: impl Fn(Swap) -> Swap,
) -> [Icon; 16] {
    let sprite = Sprite::load_native_bytes(bytes, palette).expect("embedded button PNG");
    std::array::from_fn(|index| {
        let swap = extra(accent_swap(index as Index));
        pixmap_to_icon(&render_swapped_sprite(&sprite, palette, &swap, lut))
    })
}

fn load_button_variants(palette: &PgPalette, lut: &[[u8; 4]; 256], bytes: &[u8]) -> [Icon; 16] {
    load_button_variants_with(palette, lut, bytes, |swap| swap)
}

/// As `load_button_variants`, but for the disabled close/minimize art: it
/// still tracks the leaf's accent so a disabled button doesn't look jarring
/// against a coloured border, but any `LIME` pixel is additionally always
/// remapped to `LAVENDER` — across every accent variant, not just the one
/// whose accent happens to be `LIME` — since lime reads as too vivid/live
/// for a disabled control.
fn load_disabled_button_variants(
    palette: &PgPalette,
    lut: &[[u8; 4]; 256],
    bytes: &[u8],
) -> [Icon; 16] {
    load_button_variants_with(palette, lut, bytes, |swap| {
        swap.set(palette_color::LIME, PgPaint::Solid(palette_color::LAVENDER))
    })
}

/// Index -> premultiplied RGBA output table; `TRANSPARENT` maps to a fully
/// transparent pixel (unlike `Palette::present_lut`, which assumes an
/// always-opaque root framebuffer).
fn rgba_lut(palette: &PgPalette) -> [[u8; 4]; 256] {
    let mut lut = [[0u8; 4]; 256];
    for (index, entry) in lut.iter_mut().enumerate() {
        if index as Index == TRANSPARENT {
            continue;
        }
        let c = palette.color(index as Index);
        *entry = [c.r, c.g, c.b, 255];
    }
    lut
}

/// Draw `sprite` through `swap` into a same-sized `Framebuffer`, then present
/// it into a tiny-skia `Pixmap` via `lut` so the rest of the pipeline
/// (`NineSlice`/`blit_tile`/`Icon`) can stay on tiny-skia.
fn render_swapped_sprite(
    sprite: &Sprite,
    palette: &PgPalette,
    swap: &Swap,
    lut: &[[u8; 4]; 256],
) -> Pixmap {
    let mut fb = Framebuffer::new(sprite.width, sprite.height, TRANSPARENT);
    fb.draw_sprite_swapped(sprite, 0, 0, palette, swap);
    let mut bytes = vec![0u8; sprite.width * sprite.height * 4];
    fb.present_into(&mut bytes, lut);
    Pixmap::from_vec(
        bytes,
        IntSize::from_wh(sprite.width as u32, sprite.height as u32).unwrap(),
    )
    .expect("swapped sprite framebuffer size")
}

/// A minimized leaf's `winmin.png` rendering: a vertical 3-slice (rounded
/// caps + a stretchy body), since the whole strip is a single restore button.
struct MinimizedSlice {
    top: Pixmap,
    bottom: Pixmap,
    body: Pixmap,
}

impl MinimizedSlice {
    const CAP_H: u32 = 30;

    fn new(src: &Pixmap) -> Self {
        let (w, h) = (src.width(), src.height());
        Self {
            top: crop(src, 0, 0, w, Self::CAP_H),
            bottom: crop(src, 0, h - Self::CAP_H, w, Self::CAP_H),
            body: crop(src, 0, Self::CAP_H, w, h - 2 * Self::CAP_H),
        }
    }

    fn draw(&self, pm: &mut PixmapMut, ox: f32, oy: f32, w: f32, h: f32) {
        let cap_h = Self::CAP_H as f32;
        let mid_h = 2.0f32.mul_add(-cap_h, h).max(1.0);
        // The strip isn't a tileable horizontal pattern (it's a single narrow
        // pill), so it's drawn at its exact native size, centred in whatever
        // width the leaf collapsed to, rather than stretched to fill it.
        let native_w = self.top.width() as f32;
        let cx = ox + (w - native_w) / 2.0;

        blit_tile(pm, &self.top, cx, oy, native_w, cap_h);
        blit_tile(pm, &self.bottom, cx, oy + h - cap_h, native_w, cap_h);
        blit_tile(pm, &self.body, cx, oy + cap_h, native_w, mid_h);
    }
}

/// A minimized *row*'s `winmin_h.png` rendering: the horizontal counterpart
/// of `MinimizedSlice` — a horizontal 3-slice (rounded caps left/right, a
/// stretchy body), for a leaf collapsed to a short, wide strip.
struct MinimizedSliceH {
    left: Pixmap,
    right: Pixmap,
    body: Pixmap,
}

impl MinimizedSliceH {
    const CAP_W: u32 = 10;

    fn new(src: &Pixmap) -> Self {
        let (w, h) = (src.width(), src.height());
        Self {
            left: crop(src, 0, 0, Self::CAP_W, h),
            right: crop(src, w - Self::CAP_W, 0, Self::CAP_W, h),
            body: crop(src, Self::CAP_W, 0, w - 2 * Self::CAP_W, h),
        }
    }

    fn draw(&self, pm: &mut PixmapMut, ox: f32, oy: f32, w: f32, h: f32) {
        let cap_w = Self::CAP_W as f32;
        let mid_w = 2.0f32.mul_add(-cap_w, w).max(1.0);
        // As with `MinimizedSlice`, the strip is a single pill drawn at its
        // exact native size, centred in whatever height the leaf collapsed
        // to, rather than stretched to fill it.
        let native_h = self.left.height() as f32;
        let cy = oy + (h - native_h) / 2.0;

        blit_tile(pm, &self.left, ox, cy, cap_w, native_h);
        blit_tile(pm, &self.right, ox + w - cap_w, cy, cap_w, native_h);
        blit_tile(pm, &self.body, ox + cap_w, cy, mid_w, native_h);
    }
}

impl Renderer {
    pub fn new() -> Self {
        let font = load_system_font();
        let palette = PgPalette::load_bytes(asset!("na16-1x.png")).expect("na16-1x.png");
        let lut = rgba_lut(&palette);
        Self {
            font,
            wallpaper: None,
            border_variants: load_border_variants(&palette, &lut),
            minimized: load_minimized_variants(&palette, &lut),
            minimized_h: load_minimized_h_variants(&palette, &lut),
            // Order must match `BtnIcon::index`.
            buttons: [
                ButtonVariant::load(
                    &palette,
                    &lut,
                    asset!("close.png"),
                    asset!("close_disabled.png"),
                ),
                ButtonVariant::load(
                    &palette,
                    &lut,
                    asset!("minimize.png"),
                    asset!("minimize_disabled.png"),
                ),
                ButtonVariant::load(
                    &palette,
                    &lut,
                    asset!("minimize_h.png"),
                    asset!("minimize_h_disabled.png"),
                ),
                ButtonVariant::load(
                    &palette,
                    &lut,
                    asset!("hsplit.png"),
                    asset!("hsplit_disabled.png"),
                ),
                ButtonVariant::load(
                    &palette,
                    &lut,
                    asset!("vsplit.png"),
                    asset!("vsplit_disabled.png"),
                ),
            ],
            palette,
        }
    }

    /// The ARGB colour for a na16 palette index, e.g. for the taskbar's
    /// accent highlight — reads the same loaded palette the border/button
    /// art was swapped through, rather than a second hardcoded RGB table.
    pub fn accent_rgb(&self, index: Index) -> u32 {
        let c = self.palette.color(index);
        0xff00_0000 | (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
    }

    /// Load+scale a PNG wallpaper to cover `w`x`h`. Returns whether it loaded.
    pub fn set_wallpaper(&mut self, path: &str, w: i32, h: i32) -> bool {
        self.wallpaper = load_wallpaper_pixmap(path, w, h);
        self.wallpaper.is_some()
    }

    /// A fresh screen-sized pixmap initialised with the wallpaper (or the
    /// solid background colour). All leaf chrome is composited onto this.
    pub fn screen_base(&self, w: u32, h: u32) -> Pixmap {
        let (w, h) = (w.max(1), h.max(1));
        // The wallpaper is pre-scaled to the screen size, so a clone (raw
        // memcpy) reproduces the base far cheaper than re-running the alpha
        // blend through draw_pixmap on every single frame.
        if let Some(wp) = &self.wallpaper {
            if wp.width() == w && wp.height() == h {
                return wp.clone();
            }
        }
        let mut pm = Pixmap::new(w, h).unwrap();
        if let Some(wp) = &self.wallpaper {
            pm.as_mut().draw_pixmap(
                0,
                0,
                wp.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        } else {
            pm.fill(argb(theme::WALLPAPER));
        }
        pm
    }

    /// Draw one leaf's chrome into the shared screen pixmap at screen offset
    /// (ox, oy): a minimized leaf is just the restore strip — `winmin.png`
    /// for a minimized column (narrow, tall) or `winmin_h.png` for a
    /// minimized row (short, wide), picked by the leaf's own aspect ratio;
    /// otherwise the bitmap window border plus a full-width titlebar holding
    /// the app icon/label.
    pub fn draw_leaf(&self, pm: &mut PixmapMut, ox: f32, oy: f32, v: &LeafView) {
        let accent = v.accent_index as usize % 16;
        if v.minimized {
            if v.w >= v.h {
                self.minimized_h[accent].draw(pm, ox, oy, v.w as f32, v.h as f32);
            } else {
                self.minimized[accent].draw(pm, ox, oy, v.w as f32, v.h as f32);
            }
            return;
        }
        self.border_variants[accent].draw(pm, ox, oy, v.w as f32, v.h as f32);

        self.draw_titlebar(pm, ox, oy, v);
    }

    fn draw_titlebar(&self, pm: &mut PixmapMut, ox: f32, oy: f32, v: &LeafView) {
        let Some(tab) = &v.tab else {
            return;
        };
        let tb_h = v.tb_h as f32;
        let bw = v.bw as f32;
        let isz = theme::BTN_SIZE as f32;
        let cx = ox + bw + isz / 2.0 + 4.0;
        let cy = oy + tb_h / 2.0;
        if let Some(img) = &tab.icon {
            self.draw_icon(pm, img, cx - isz / 2.0, cy - isz / 2.0, isz, tab.icon_hue);
        } else {
            self.draw_glyph(pm, tab.label, cx, cy + 2.0, isz * 0.7, theme::COLOR_FG);
        }
    }

    fn draw_glyph(&self, pm: &mut PixmapMut, ch: char, cx: f32, cy: f32, px: f32, color: u32) {
        let (metrics, bitmap) = self.font.rasterize(ch, px);
        if metrics.width == 0 || metrics.height == 0 {
            return;
        }
        let ox = (cx - metrics.width as f32 / 2.0).round() as i32;
        let oy = (cy - metrics.height as f32 / 2.0).round() as i32;
        let pw = pm.width() as i32;
        let ph = pm.height() as i32;
        let data = pm.data_mut();
        let [cr, cg, cb] = [
            ((color >> 16) & 0xff) as u32,
            ((color >> 8) & 0xff) as u32,
            (color & 0xff) as u32,
        ];
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let a = u32::from(bitmap[gy * metrics.width + gx]);
                if a == 0 {
                    continue;
                }
                let px_ = ox + gx as i32;
                let py_ = oy + gy as i32;
                if px_ < 0 || py_ < 0 || px_ >= pw || py_ >= ph {
                    continue;
                }
                let idx = ((py_ * pw + px_) * 4) as usize;
                // tiny-skia pixmap is premultiplied RGBA; blend glyph over.
                for (k, cc) in [cr, cg, cb].iter().enumerate() {
                    let dst = u32::from(data[idx + k]);
                    data[idx + k] = ((cc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }

    /// Draw one taskbar entry: a rounded background tile with the app icon
    /// (or letter-glyph fallback) centred in it.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_taskbar_item(
        &self,
        pm: &mut PixmapMut,
        r: TaskItem,
        icon: Option<&Icon>,
        label: char,
        color: u32,
        highlight: bool,
        icon_hue: Option<f32>,
    ) {
        let bgp = rounded_rect(r.x, r.y, r.w.max(1.0), r.h.max(1.0), 8.0);
        let mut bg = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        bg.set_color(argb(theme::COLOR_BTN_BG));
        pm.fill_path(&bgp, &bg, FillRule::Winding, Transform::identity(), None);

        let cx = r.x + r.w / 2.0;
        let cy = r.y + r.h / 2.0;
        let isz = r.h.min(r.w) - 6.0;
        if let Some(img) = icon {
            self.draw_icon(pm, img, cx - isz / 2.0, cy - isz / 2.0, isz, icon_hue);
        } else {
            self.draw_glyph(pm, label, cx, cy + 2.0, isz * 0.7, color);
        }

        // Windows currently shown in a split get an accent box around them.
        if highlight {
            let mut sp = Paint::<'_> {
                anti_alias: true,
                ..Default::default()
            };
            sp.set_color(argb(color | 0xff00_0000));
            pm.stroke_path(
                &bgp,
                &sp,
                &Stroke {
                    width: 2.0,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
    }

    /// Snap every non-transparent pixel in `icon` to the nearest na16
    /// palette colour (alpha is kept as-is), so app icons render as flat
    /// pixel art matching the rest of the UI's 16-colour chrome. Called
    /// once when an icon is fetched (`Wm::fetch_icon`), not per frame.
    pub fn quantize_icon(&self, icon: &Icon) -> Icon {
        let argb = icon.argb.iter().map(|&px| self.quantize_argb(px)).collect();
        Icon {
            w: icon.w,
            h: icon.h,
            argb,
        }
    }

    fn quantize_argb(&self, px: u32) -> u32 {
        let a = px >> 24;
        if a == 0 {
            return px;
        }
        let rgb = PgRgb {
            r: ((px >> 16) & 0xff) as u8,
            g: ((px >> 8) & 0xff) as u8,
            b: (px & 0xff) as u8,
        };
        let snapped = self.palette.color(self.palette.nearest_index(rgb));
        (a << 24)
            | (u32::from(snapped.r) << 16)
            | (u32::from(snapped.g) << 8)
            | u32::from(snapped.b)
    }

    /// Rotate `argb`'s hue by `deg` in OKLCH space, then re-snap the result
    /// onto the na16 palette so a rotated icon stays as flatly pixel-art as
    /// the un-rotated (already-quantized) source.
    fn rotate_and_requantize(&self, argb: u32, deg: f32) -> u32 {
        self.quantize_argb(crate::oklch::rotate_hue_argb(argb, deg))
    }

    /// Blit `img` scaled to a `size`x`size` box at (dx, dy), alpha-blending
    /// each source pixel over the (premultiplied RGBA) pixmap. When
    /// `hue_deg` is `Some`, each source pixel's hue is rotated that many
    /// degrees in OKLCH space and re-quantized before blending — the
    /// same-app icon disambiguation effect (see `Wm::icon_hue`), applied to
    /// the bitmap itself rather than an overlay.
    fn draw_icon(
        &self,
        pm: &mut PixmapMut,
        img: &Icon,
        dx: f32,
        dy: f32,
        size: f32,
        hue_deg: Option<f32>,
    ) {
        self.draw_icon_alpha(pm, img, dx, dy, size, 255, hue_deg);
    }

    /// As `draw_icon`, but each source pixel's alpha is additionally scaled
    /// by `alpha` (0-255) — used to dim disabled buttons.
    #[allow(clippy::too_many_arguments)]
    fn draw_icon_alpha(
        &self,
        pm: &mut PixmapMut,
        img: &Icon,
        dx: f32,
        dy: f32,
        size: f32,
        alpha: u32,
        hue_deg: Option<f32>,
    ) {
        if img.w == 0 || img.h == 0 || size < 1.0 {
            return;
        }
        let pw = pm.width() as i32;
        let ph = pm.height() as i32;
        let data = pm.data_mut();
        let isz = size as i32;
        let ox = dx.round() as i32;
        let oy = dy.round() as i32;
        for ty in 0..isz {
            let sy = (ty as u32 * img.h / isz as u32).min(img.h - 1);
            let py = oy + ty;
            if py < 0 || py >= ph {
                continue;
            }
            for tx in 0..isz {
                let sx = (tx as u32 * img.w / isz as u32).min(img.w - 1);
                let px = ox + tx;
                if px < 0 || px >= pw {
                    continue;
                }
                let s = img.argb[(sy * img.w + sx) as usize];
                let s = hue_deg.map_or(s, |deg| self.rotate_and_requantize(s, deg));
                let a = ((s >> 24) & 0xff) * alpha / 255;
                if a == 0 {
                    continue;
                }
                let (sr, sg, sb) = ((s >> 16) & 0xff, (s >> 8) & 0xff, s & 0xff);
                let idx = ((py * pw + px) * 4) as usize;
                // Source is straight ARGB; pixmap is premultiplied RGBA.
                for (k, sc) in [sr, sg, sb].iter().enumerate() {
                    let dst = u32::from(data[idx + k]);
                    data[idx + k] = ((sc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }
}

/// A taskbar tile rectangle (screen coords) for the renderer.
#[derive(Clone, Copy)]
pub struct TaskItem {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Draw a translucent rounded "+" insert button centred at (cx, cy).
pub fn draw_plus(pm: &mut PixmapMut, cx: f32, cy: f32, sz: f32) {
    let half = sz / 2.0;
    let bgp = rounded_rect(cx - half, cy - half, sz, sz, sz * 0.28);
    let mut bg = Paint::<'_> {
        anti_alias: true,
        ..Default::default()
    };
    bg.set_color(argb(theme::COLOR_BTN_BG));
    pm.fill_path(&bgp, &bg, FillRule::Winding, Transform::identity(), None);

    let arm = sz * 0.28;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - arm, cy);
    pb.line_to(cx + arm, cy);
    pb.move_to(cx, cy - arm);
    pb.line_to(cx, cy + arm);
    if let Some(path) = pb.finish() {
        let mut p = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color(argb(theme::COLOR_FG));
        let stroke = Stroke {
            width: 2.5,
            ..Default::default()
        };
        pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }
}

/// The split-control buttons drawn at the right of every leaf's titlebar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnIcon {
    Close,
    /// A leaf whose parent is an H-branch: minimizing collapses it to a
    /// narrow column, so the button previews that with `minimize.png`.
    Minimize,
    /// A leaf whose parent is a V-branch: minimizing collapses it to a
    /// short row, so the button previews that with `minimize_h.png`.
    MinimizeH,
    HSplit,
    VSplit,
}

impl BtnIcon {
    const COUNT: usize = 5;

    /// Slot into `Renderer.buttons`; must stay in sync with the array
    /// `Renderer::new` builds.
    const fn index(self) -> usize {
        match self {
            Self::Close => 0,
            Self::Minimize => 1,
            Self::MinimizeH => 2,
            Self::HSplit => 3,
            Self::VSplit => 4,
        }
    }
}

impl Renderer {
    /// Draw one bitmap split-control button centred at (cx, cy), palette-swapped
    /// to `accent_index` to match its leaf's border. Every button swaps in its
    /// dedicated `*_disabled.png` art when disabled (also accent-swapped, see
    /// `load_disabled_button_variants`).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_button(
        &self,
        pm: &mut PixmapMut,
        cx: f32,
        cy: f32,
        size: f32,
        icon: BtnIcon,
        disabled: bool,
        accent_index: Index,
    ) {
        let accent = accent_index as usize % 16;
        let variant = &self.buttons[icon.index()];
        let img = if disabled {
            &variant.disabled[accent]
        } else {
            &variant.normal[accent]
        };
        self.draw_icon(pm, img, cx - size / 2.0, cy - size / 2.0, size, None);
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.1 {
        let mut pb = PathBuilder::new();
        pb.push_rect(SkRect::from_xywh(x, y, w, h).unwrap());
        return pb.finish().unwrap();
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

// --- application launcher menu ---

pub const MENU_ROW_H: i32 = 26;
pub const MENU_BORDER: i32 = 8;
const MENU_PAD_X: f32 = 12.0;
const MENU_FONT_PX: f32 = 14.0;
const MENU_ARROW_W: f32 = 16.0;

impl Renderer {
    /// Advance width of a left-aligned string at pixel size `px`.
    fn text_width(&self, s: &str, px: f32) -> f32 {
        s.chars()
            .map(|c| self.font.metrics(c, px).advance_width)
            .sum()
    }

    /// Inner content width (excludes the black border) for a menu column.
    pub fn menu_content_w(&self, labels: &[String], any_arrow: bool) -> i32 {
        let mut w = 0.0f32;
        for l in labels {
            w = w.max(self.text_width(l, MENU_FONT_PX));
        }
        let arrow = if any_arrow { MENU_ARROW_W } else { 0.0 };
        (MENU_PAD_X.mul_add(2.0, w) + arrow).ceil() as i32
    }

    /// Render a menu column to its own pixmap. `content_w` is the inner width
    /// (shared across a menu + submenu so columns line up); `seps` marks
    /// divider rows, `hi` is the hovered row.
    pub fn draw_menu(
        &self,
        labels: &[String],
        arrows: &[bool],
        seps: &[bool],
        content_w: i32,
        hi: Option<usize>,
    ) -> Pixmap {
        let rows = labels.len() as i32;
        let w = (content_w + 2 * MENU_BORDER).max(1) as u32;
        let h = (rows * MENU_ROW_H + 2 * MENU_BORDER).max(1) as u32;
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(argb(0xff00_0000));
        let b = MENU_BORDER as f32;
        let cw = content_w as f32;
        let mut m = pm.as_mut();
        for (i, label) in labels.iter().enumerate() {
            let ry = (i as i32 as f32).mul_add(MENU_ROW_H as f32, b);
            if seps.get(i).copied().unwrap_or(false) {
                // Faint divider line centred in the row.
                let mut p = Paint::<'_> {
                    anti_alias: false,
                    ..Default::default()
                };
                p.set_color(argb(0x33ff_ffff));
                if let Some(rect) =
                    SkRect::from_xywh(b + 4.0, ry + MENU_ROW_H as f32 / 2.0, cw - 8.0, 1.0)
                {
                    let mut pb = PathBuilder::new();
                    pb.push_rect(rect);
                    m.fill_path(
                        &pb.finish().unwrap(),
                        &p,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
                continue;
            }
            if Some(i) == hi {
                let hp = rounded_rect(b + 2.0, ry + 1.0, cw - 4.0, MENU_ROW_H as f32 - 2.0, 4.0);
                let mut p = Paint::<'_> {
                    anti_alias: true,
                    ..Default::default()
                };
                p.set_color(argb(theme::COLOR_FG_HOVER));
                m.fill_path(&hp, &p, FillRule::Winding, Transform::identity(), None);
            }
            let baseline = MENU_FONT_PX.mul_add(0.35, ry + MENU_ROW_H as f32 / 2.0);
            self.draw_text(
                &mut m,
                label,
                b + MENU_PAD_X,
                baseline,
                MENU_FONT_PX,
                theme::COLOR_FG,
            );
            if arrows.get(i).copied().unwrap_or(false) {
                // Small right-pointing triangle (▸) drawn as a path.
                let ax = b + cw - MENU_ARROW_W + 4.0;
                let ay = ry + MENU_ROW_H as f32 / 2.0;
                let mut pb = PathBuilder::new();
                pb.move_to(ax, ay - 4.0);
                pb.line_to(ax + 6.0, ay);
                pb.line_to(ax, ay + 4.0);
                pb.close();
                let mut p = Paint::<'_> {
                    anti_alias: true,
                    ..Default::default()
                };
                p.set_color(argb(theme::COLOR_FG));
                m.fill_path(
                    &pb.finish().unwrap(),
                    &p,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        pm
    }

    /// Draw a left-aligned UTF-8 string with its baseline at `y`.
    fn draw_text(&self, pm: &mut PixmapMut, text: &str, x: f32, y: f32, px: f32, color: u32) {
        let mut pen = x;
        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(ch, px);
            if metrics.width > 0 && metrics.height > 0 {
                let gx = (pen + metrics.xmin as f32).round() as i32;
                let gy = (y - (metrics.ymin + metrics.height as i32) as f32).round() as i32;
                Self::blit_coverage(pm, &bitmap, metrics.width, metrics.height, gx, gy, color);
            }
            pen += metrics.advance_width;
        }
    }

    /// Alpha-blend an 8-bit coverage bitmap in `color` onto the pixmap.
    #[allow(clippy::too_many_arguments)]
    fn blit_coverage(
        pm: &mut PixmapMut,
        bitmap: &[u8],
        bw: usize,
        bh: usize,
        ox: i32,
        oy: i32,
        color: u32,
    ) {
        let pw = pm.width() as i32;
        let ph = pm.height() as i32;
        let data = pm.data_mut();
        let [cr, cg, cb] = [
            ((color >> 16) & 0xff) as u32,
            ((color >> 8) & 0xff) as u32,
            (color & 0xff) as u32,
        ];
        for gy in 0..bh {
            for gx in 0..bw {
                let a = u32::from(bitmap[gy * bw + gx]);
                if a == 0 {
                    continue;
                }
                let (px_, py_) = (ox + gx as i32, oy + gy as i32);
                if px_ < 0 || py_ < 0 || px_ >= pw || py_ >= ph {
                    continue;
                }
                let idx = ((py_ * pw + px_) * 4) as usize;
                for (k, cc) in [cr, cg, cb].iter().enumerate() {
                    let dst = u32::from(data[idx + k]);
                    data[idx + k] = ((cc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }
}

/// Public wrapper: convert a tiny-skia pixmap to `PutImage`-ready BGRX bytes.
/// Convert a tiny-skia pixmap to X11 BGRX bytes, reusing `out`'s allocation
/// (resized as needed) so the full-screen buffer isn't reallocated each frame.
pub fn pixmap_to_bgrx(pm: &Pixmap, out: &mut Vec<u8>) {
    let src = pm.data();
    out.resize(src.len(), 0);
    for i in (0..src.len()).step_by(4) {
        // tiny-skia: R,G,B,A (premultiplied; opaque here) -> B,G,R,X
        out[i] = src[i + 2];
        out[i + 1] = src[i + 1];
        out[i + 2] = src[i];
        out[i + 3] = 0;
    }
}

/// Load a PNG wallpaper and scale it to cover a `w`x`h` area. `None` if it
/// can't be read.
fn load_wallpaper_pixmap(path: &str, w: i32, h: i32) -> Option<Pixmap> {
    let src = Pixmap::load_png(path).ok()?;
    let (dw, dh) = (w.max(1) as u32, h.max(1) as u32);
    let mut dst = Pixmap::new(dw, dh)?;
    dst.fill(argb(theme::COLOR_BG));
    let scale = (dw as f32 / src.width() as f32).max(dh as f32 / src.height() as f32);
    let ox = (src.width() as f32).mul_add(-scale, dw as f32) / 2.0;
    let oy = (src.height() as f32).mul_add(-scale, dh as f32) / 2.0;
    let tf = Transform::from_scale(scale, scale).post_translate(ox, oy);
    dst.as_mut()
        .draw_pixmap(0, 0, src.as_ref(), &PixmapPaint::default(), tf, None);
    Some(dst)
}

fn load_system_font() -> Font {
    let candidates = [
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(f) = Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return f;
            }
        }
    }
    // Last resort: scan a couple of font dirs for any ttf.
    for dir in ["/usr/share/fonts", "/usr/local/share/fonts"] {
        if let Some(f) = scan_for_font(std::path::Path::new(dir), 0) {
            return f;
        }
    }
    panic!("no usable TTF font found on system");
}

fn scan_for_font(dir: &std::path::Path, depth: u32) -> Option<Font> {
    if depth > 4 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = scan_for_font(&p, depth + 1) {
                return Some(f);
            }
        } else if p.extension().is_some_and(|x| x == "ttf") {
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(f) = Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(f);
                }
            }
        }
    }
    None
}
