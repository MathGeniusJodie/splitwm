//! Software rendering of leaf decorations (tab bar, focus border, content
//! background) as an indexed-colour `pixel_graphics::Framebuffer`. Presented
//! once per frame into a BGRX byte buffer ready for X `PutImage` on a
//! depth-24 `TrueColor` visual.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]

use std::rc::Rc;

use pixel_fonts::{BitmapFont, FUSION_PIXEL_12_SPEC};
use pixel_graphics::{
    decode_png_with_size, Framebuffer, Paint as PgPaint, Palette as PgPalette, PresentLut,
    Rect as PgRect, Rgb as PgRgb, Rgba, Sprite, Swap,
};

use crate::icon::Icon;
use crate::theme::{self, palette_color};
use crate::Index;

/// Embedded-art PNG bytes, relative to the crate root (where the bitmap
/// assets live alongside `Cargo.toml`).
macro_rules! asset {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $name))
    };
}

/// Dithered "translucent" chrome background: a checker of black and gunmetal
/// stands in for the old 50%-alpha black fills, keeping everything on the
/// 16-colour palette.
const CHROME_BG: PgPaint = PgPaint::Checker(palette_color::BLACK, palette_color::GUNMETAL);

pub struct Renderer {
    /// `None` when the pixel-fonts atlases can't be read; text drawing
    /// degrades to a no-op instead of refusing to start the WM.
    font: Option<BitmapFont>,
    /// The na16 palette all art/indices resolve through.
    palette: PgPalette,
    /// Index -> BGRX output table used by `present`.
    lut: Box<PresentLut>,
    /// Palette index used for foreground text/glyph strokes.
    fg: Index,
    /// Screen-sized scaled wallpaper (quantized to the palette); frame
    /// backgrounds copy it each frame.
    wallpaper: Option<Framebuffer>,
    /// Bitmap window border; palette-swapped to each leaf's accent at draw
    /// time via `accent_swap`.
    border: NineSlice,
    /// The `winmin.png` restore strip for a minimized *column* (squished
    /// narrow, so the strip runs vertically) and `winmin_h.png` for a
    /// minimized *row* (squished short, strip runs horizontally) — picked in
    /// `draw_leaf` by the minimized leaf's own aspect ratio. Accent-swapped
    /// at draw time like the border.
    minimized: Sprite,
    minimized_h: Sprite,
    /// Titlebar buttons, indexed by `BtnIcon::index`; accent-swapped at draw
    /// time. `Minimize`/`MinimizeH` are two separate slots (not
    /// enabled/disabled of the same button) — see `BtnIcon::MinimizeH`.
    buttons: [ButtonArt; BtnIcon::COUNT],
}

/// One titlebar button's art: the normal and dedicated disabled sprite.
struct ButtonArt {
    normal: Sprite,
    disabled: Sprite,
}

impl ButtonArt {
    fn load(palette: &PgPalette, bytes: &[u8], disabled_bytes: &[u8]) -> Self {
        Self {
            normal: Sprite::load_native_bytes(bytes, palette).expect("embedded button PNG"),
            disabled: Sprite::load_native_bytes(disabled_bytes, palette)
                .expect("embedded disabled button PNG"),
        }
    }
}

pub struct TabInfo {
    pub label: char,
    /// Icon to draw, already resolved by the caller — the hue-rotated
    /// variant when same-app disambiguation applies (see `Wm::icon_for`).
    pub icon: Option<Rc<Icon>>,
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

/// `fill_rect_paint` with signed, clipped coordinates.
fn fill_paint(fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, paint: PgPaint) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(fb.width as i32);
    let y1 = (y + h).min(fb.height as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    fb.fill_rect_paint(
        x0 as usize,
        y0 as usize,
        (x1 - x0) as usize,
        (y1 - y0) as usize,
        paint,
    );
}

fn fill(fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, index: Index) {
    fill_paint(fb, x, y, w, h, PgPaint::Solid(index));
}

/// Tile `src` (a region of `sprite`) to exactly fill a `w`x`h` box at
/// (ox, oy), one image pixel per screen pixel — no scaling, so pixel art
/// stays crisp. When the box equals `src`'s size (e.g. a 9-slice corner),
/// this draws the source exactly once.
#[allow(clippy::too_many_arguments)]
fn tile_swapped(
    fb: &mut Framebuffer,
    sprite: &Sprite,
    src: PgRect,
    ox: i32,
    oy: i32,
    w: i32,
    h: i32,
    palette: &PgPalette,
    swap: &Swap,
) {
    if w <= 0 || h <= 0 || src.w == 0 || src.h == 0 {
        return;
    }
    let cx0 = ox.max(0);
    let cy0 = oy.max(0);
    let cx1 = (ox + w).min(fb.width as i32);
    let cy1 = (oy + h).min(fb.height as i32);
    if cx0 >= cx1 || cy0 >= cy1 {
        return;
    }
    let clip = PgRect::new(
        cx0 as usize,
        cy0 as usize,
        (cx1 - cx0) as usize,
        (cy1 - cy0) as usize,
    );
    let mut y = oy;
    while y < oy + h {
        let mut x = ox;
        while x < ox + w {
            fb.draw_sprite_full(
                sprite,
                src,
                x as isize,
                y as isize,
                Some(clip),
                palette,
                Some(swap),
            );
            x += src.w as i32;
        }
        y += src.h as i32;
    }
}

/// A bitmap 9-slice over one source sprite: 4 fixed corners plus 4 edges/a
/// center that repeat to fill an arbitrary target rect at native resolution.
struct NineSlice {
    sprite: Sprite,
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
    const EDGE_SAMPLE_X0: usize = 20;
    const EDGE_SAMPLE_X1: usize = 180;

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        fb: &mut Framebuffer,
        palette: &PgPalette,
        swap: &Swap,
        ox: i32,
        oy: i32,
        w: i32,
        h: i32,
    ) {
        let (l, t, r, b) = (self.l, self.t, self.r, self.b);
        let (sw, sh) = (self.sprite.width, self.sprite.height);
        let (lu, tu, ru, bu) = (l as usize, t as usize, r as usize, b as usize);
        let edge_x = Self::EDGE_SAMPLE_X0;
        let edge_w = Self::EDGE_SAMPLE_X1 - Self::EDGE_SAMPLE_X0;
        let mid_h_src = sh - tu - bu;
        let mid_w = (w - l - r).max(1);
        let mid_h = (h - t - b).max(1);

        let mut part = |src: PgRect, x: i32, y: i32, dw: i32, dh: i32| {
            tile_swapped(fb, &self.sprite, src, x, y, dw, dh, palette, swap);
        };
        part(PgRect::new(0, 0, lu, tu), ox, oy, l, t);
        part(PgRect::new(sw - ru, 0, ru, tu), ox + w - r, oy, r, t);
        part(PgRect::new(0, sh - bu, lu, bu), ox, oy + h - b, l, b);
        part(
            PgRect::new(sw - ru, sh - bu, ru, bu),
            ox + w - r,
            oy + h - b,
            r,
            b,
        );
        part(PgRect::new(edge_x, 0, edge_w, tu), ox + l, oy, mid_w, t);
        part(
            PgRect::new(edge_x, sh - bu, edge_w, bu),
            ox + l,
            oy + h - b,
            mid_w,
            b,
        );
        part(PgRect::new(0, tu, lu, mid_h_src), ox, oy + t, l, mid_h);
        part(
            PgRect::new(sw - ru, tu, ru, mid_h_src),
            ox + w - r,
            oy + t,
            r,
            mid_h,
        );
        part(
            PgRect::new(edge_x, tu, edge_w, mid_h_src),
            ox + l,
            oy + t,
            mid_w,
            mid_h,
        );
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

/// The `winmin.png` vertical 3-slice caps / `winmin_h.png` horizontal ones.
const MIN_CAP_H: usize = 30;
const MIN_CAP_W: usize = 10;

impl Renderer {
    pub fn new() -> Self {
        let palette = PgPalette::load_bytes(asset!("na16-1x.png")).expect("na16-1x.png");
        let font = match BitmapFont::load(&FUSION_PIXEL_12_SPEC) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("splitwm: pixel font unavailable ({e}); text labels disabled");
                None
            }
        };
        let lut = palette.present_lut();
        let fg = palette.closest_to_white_index();
        Self {
            font,
            lut,
            fg,
            wallpaper: None,
            border: NineSlice {
                sprite: Sprite::load_native_bytes(asset!("winborder.png"), &palette)
                    .expect("winborder.png"),
                l: theme::BORDER_LEFT,
                t: theme::BORDER_TOP,
                r: theme::BORDER_RIGHT,
                b: theme::BORDER_BOTTOM,
            },
            minimized: Sprite::load_native_bytes(asset!("winmin.png"), &palette)
                .expect("winmin.png"),
            minimized_h: Sprite::load_native_bytes(asset!("winmin_h.png"), &palette)
                .expect("winmin_h.png"),
            // Order must match `BtnIcon::index`.
            buttons: [
                ButtonArt::load(&palette, asset!("close.png"), asset!("close_disabled.png")),
                ButtonArt::load(
                    &palette,
                    asset!("minimize.png"),
                    asset!("minimize_disabled.png"),
                ),
                ButtonArt::load(
                    &palette,
                    asset!("minimize_h.png"),
                    asset!("minimize_h_disabled.png"),
                ),
                ButtonArt::load(
                    &palette,
                    asset!("hsplit.png"),
                    asset!("hsplit_disabled.png"),
                ),
                ButtonArt::load(
                    &palette,
                    asset!("vsplit.png"),
                    asset!("vsplit_disabled.png"),
                ),
            ],
            palette,
        }
    }

    /// Present an indexed framebuffer as X11 BGRX bytes, reusing `out`'s
    /// allocation so the full-screen buffer isn't reallocated each frame.
    pub fn present(&self, fb: &Framebuffer, out: &mut Vec<u8>) {
        out.resize(fb.width * fb.height * Framebuffer::BYTES_PER_PIXEL, 0);
        fb.present_into(out, &self.lut);
    }

    /// Load+scale a PNG wallpaper to cover `w`x`h`, quantized onto the na16
    /// palette. Returns whether it loaded.
    pub fn set_wallpaper(&mut self, path: &str, w: i32, h: i32) -> bool {
        self.wallpaper = self.load_wallpaper(path, w, h);
        self.wallpaper.is_some()
    }

    fn load_wallpaper(&self, path: &str, w: i32, h: i32) -> Option<Framebuffer> {
        let (sw, sh, pixels) = Self::decode_image(path)?;
        let (dw, dh) = (w.max(1) as usize, h.max(1) as usize);
        // Scale-to-cover with nearest-neighbour sampling, then quantize to
        // the palette with serpentine Floyd-Steinberg error diffusion so the
        // 16-colour result reads as smooth gradients instead of hard bands.
        let scale = (dw as f32 / sw as f32).max(dh as f32 / sh as f32);
        let ox = (sw as f32).mul_add(-scale, dw as f32) / 2.0;
        let oy = (sh as f32).mul_add(-scale, dh as f32) / 2.0;
        let mut fb = Framebuffer::new(dw, dh, palette_color::BLACK);
        // Two rows of per-channel accumulated error: current and next.
        let mut err_cur = vec![[0.0f32; 3]; dw];
        let mut err_next = vec![[0.0f32; 3]; dw];
        for y in 0..dh {
            let sy = (((y as f32 - oy) / scale) as usize).min(sh - 1);
            let ltr = y % 2 == 0;
            let xs: Box<dyn Iterator<Item = usize>> =
                if ltr { Box::new(0..dw) } else { Box::new((0..dw).rev()) };
            for x in xs {
                let sx = (((x as f32 - ox) / scale) as usize).min(sw - 1);
                let c = pixels[sy * sw + sx];
                let want = [
                    (f32::from(c.r) + err_cur[x][0]).clamp(0.0, 255.0),
                    (f32::from(c.g) + err_cur[x][1]).clamp(0.0, 255.0),
                    (f32::from(c.b) + err_cur[x][2]).clamp(0.0, 255.0),
                ];
                let index = self.palette.nearest_index(PgRgb {
                    r: want[0] as u8,
                    g: want[1] as u8,
                    b: want[2] as u8,
                });
                fb.set_pixel(x, y, index);
                let got = self.palette.color(index);
                let err = [
                    want[0] - f32::from(got.r),
                    want[1] - f32::from(got.g),
                    want[2] - f32::from(got.b),
                ];
                // Floyd-Steinberg kernel, mirrored on right-to-left rows:
                //         *   7/16
                //  3/16  5/16  1/16
                let ahead = if ltr { x + 1 } else { x.wrapping_sub(1) };
                let behind = if ltr { x.wrapping_sub(1) } else { x + 1 };
                for ch in 0..3 {
                    if ahead < dw {
                        err_cur[ahead][ch] += err[ch] * (7.0 / 16.0);
                        err_next[ahead][ch] += err[ch] * (1.0 / 16.0);
                    }
                    if behind < dw {
                        err_next[behind][ch] += err[ch] * (3.0 / 16.0);
                    }
                    err_next[x][ch] += err[ch] * (5.0 / 16.0);
                }
            }
            std::mem::swap(&mut err_cur, &mut err_next);
            for e in &mut err_next {
                *e = [0.0; 3];
            }
        }
        Some(fb)
    }

    /// Decode a wallpaper image — PNG via pixel-graphics, or JPEG — into RGBA
    /// pixels. Format is sniffed from the file's magic bytes, not its
    /// extension.
    fn decode_image(path: &str) -> Option<(usize, usize, Vec<Rgba>)> {
        let bytes = std::fs::read(path).ok()?;
        if !bytes.starts_with(&[0xff, 0xd8]) {
            return decode_png_with_size(path).ok();
        }
        let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes));
        let data = dec.decode().ok()?;
        let (w, h) = dec.dimensions()?;
        // Grayscale JPEGs decode to 1 byte/pixel, colour to 3 (RGB).
        let comps = data.len().checked_div(w * h).filter(|&c| c == 1 || c == 3)?;
        let pixels = data
            .chunks_exact(comps)
            .map(|px| Rgba {
                r: px[0],
                g: px[comps / 2],
                b: px[comps - 1],
                a: 255,
            })
            .collect();
        Some((w, h, pixels))
    }

    /// A fresh screen-sized framebuffer initialised with the wallpaper (or
    /// the solid background colour). All leaf chrome is composited onto this.
    pub fn screen_base(&self, w: u32, h: u32) -> Framebuffer {
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        let mut fb = Framebuffer::new(w, h, palette_color::BLACK);
        if let Some(wp) = &self.wallpaper {
            fb.blit_from(wp, 0, 0);
        }
        fb
    }

    /// Draw one leaf's chrome into the shared screen framebuffer at screen
    /// offset (ox, oy): a minimized leaf is just the restore strip —
    /// `winmin.png` for a minimized column (narrow, tall) or `winmin_h.png`
    /// for a minimized row (short, wide), picked by the leaf's own aspect
    /// ratio; otherwise the bitmap window border plus a full-width titlebar
    /// holding the app icon/label.
    pub fn draw_leaf(&self, fb: &mut Framebuffer, ox: i32, oy: i32, v: &LeafView) {
        let swap = accent_swap(v.accent_index);
        if v.minimized {
            if v.w >= v.h {
                self.draw_minimized_h(fb, &swap, ox, oy, v.w, v.h);
            } else {
                self.draw_minimized(fb, &swap, ox, oy, v.w, v.h);
            }
            return;
        }
        self.border
            .draw(fb, &self.palette, &swap, ox, oy, v.w, v.h);
        self.draw_titlebar(fb, ox, oy, v);
    }

    /// A minimized column's `winmin.png` rendering: a vertical 3-slice
    /// (rounded caps + a stretchy body), the whole strip a single restore
    /// button. The strip isn't a tileable horizontal pattern (it's a single
    /// narrow pill), so it's drawn at its exact native width, centred in
    /// whatever width the leaf collapsed to.
    fn draw_minimized(&self, fb: &mut Framebuffer, swap: &Swap, ox: i32, oy: i32, w: i32, h: i32) {
        let s = &self.minimized;
        let (sw, sh) = (s.width, s.height);
        let cap = MIN_CAP_H as i32;
        let mid_h = (h - 2 * cap).max(1);
        let cx = ox + (w - sw as i32) / 2;
        let mut part = |src: PgRect, y: i32, dh: i32| {
            tile_swapped(fb, s, src, cx, y, sw as i32, dh, &self.palette, swap);
        };
        part(PgRect::new(0, 0, sw, MIN_CAP_H), oy, cap);
        part(PgRect::new(0, sh - MIN_CAP_H, sw, MIN_CAP_H), oy + h - cap, cap);
        part(PgRect::new(0, MIN_CAP_H, sw, sh - 2 * MIN_CAP_H), oy + cap, mid_h);
    }

    /// The horizontal counterpart of `draw_minimized`, from `winmin_h.png`,
    /// for a leaf collapsed to a short, wide strip; native height, centred.
    fn draw_minimized_h(
        &self,
        fb: &mut Framebuffer,
        swap: &Swap,
        ox: i32,
        oy: i32,
        w: i32,
        h: i32,
    ) {
        let s = &self.minimized_h;
        let (sw, sh) = (s.width, s.height);
        let cap = MIN_CAP_W as i32;
        let mid_w = (w - 2 * cap).max(1);
        let cy = oy + (h - sh as i32) / 2;
        let mut part = |src: PgRect, x: i32, dw: i32| {
            tile_swapped(fb, s, src, x, cy, dw, sh as i32, &self.palette, swap);
        };
        part(PgRect::new(0, 0, MIN_CAP_W, sh), ox, cap);
        part(PgRect::new(sw - MIN_CAP_W, 0, MIN_CAP_W, sh), ox + w - cap, cap);
        part(PgRect::new(MIN_CAP_W, 0, sw - 2 * MIN_CAP_W, sh), ox + cap, mid_w);
    }

    fn draw_titlebar(&self, fb: &mut Framebuffer, ox: i32, oy: i32, v: &LeafView) {
        let Some(tab) = &v.tab else {
            return;
        };
        let isz = theme::BTN_SIZE;
        let cx = ox + v.bw + isz / 2 + 4;
        let cy = oy + v.tb_h / 2;
        if let Some(img) = &tab.icon {
            self.draw_icon(fb, img, cx - isz / 2, cy - isz / 2, isz);
        } else {
            self.draw_glyph(fb, tab.label, cx, cy, self.fg);
        }
    }

    /// Draw a single character centred at (cx, cy) in palette colour `color`.
    fn draw_glyph(&self, fb: &mut Framebuffer, ch: char, cx: i32, cy: i32, color: Index) {
        let Some(font) = &self.font else {
            return;
        };
        let mut buf = [0u8; 4];
        let s = &*ch.encode_utf8(&mut buf);
        let w = font.text_width(s) as i32;
        let h = font.cell_h() as i32;
        let x = cx - w / 2;
        let y = cy - h / 2;
        if x < 0 || y < 0 {
            return;
        }
        font.draw_text(fb, s, x as usize, y as usize, color);
    }

    /// Draw one taskbar entry: a dithered background tile with the app icon
    /// (or letter-glyph fallback) centred in it. Windows currently shown in a
    /// split (`highlight`) get a 3px accent outline traced around the icon's
    /// own silhouette instead of a box.
    pub fn draw_taskbar_item(
        &self,
        fb: &mut Framebuffer,
        r: TaskItem,
        icon: Option<&Icon>,
        label: char,
        accent: Index,
        highlight: bool,
    ) {
        // Pixel-art rounded tile: full-height middle plus 2px-notched sides.
        fill_paint(fb, r.x + 2, r.y, r.w - 4, r.h, CHROME_BG);
        fill_paint(fb, r.x, r.y + 2, 2, r.h - 4, CHROME_BG);
        fill_paint(fb, r.x + r.w - 2, r.y + 2, 2, r.h - 4, CHROME_BG);

        let cx = r.x + r.w / 2;
        let cy = r.y + r.h / 2;
        let isz = r.h.min(r.w) - 6;
        if let Some(img) = icon {
            let (dx, dy) = (cx - isz / 2, cy - isz / 2);
            if highlight {
                self.draw_icon_outline(fb, img, dx, dy, isz, accent);
            }
            self.draw_icon(fb, img, dx, dy, isz);
        } else {
            if highlight {
                // Silhouette-outline the fallback glyph the same way.
                for oy in -3i32..=3 {
                    for ox in -3i32..=3 {
                        if ox == 0 && oy == 0 {
                            continue;
                        }
                        self.draw_glyph(fb, label, cx + ox, cy + oy, accent);
                    }
                }
            }
            self.draw_glyph(fb, label, cx, cy, self.fg);
        }
    }

    /// Trace a 3px outline in `accent` around `img`'s opaque silhouette: the
    /// scaled icon's coverage mask, stamped at every offset within Chebyshev
    /// distance 3, drawn before the icon itself so only the dilated ring
    /// stays visible.
    fn draw_icon_outline(
        &self,
        fb: &mut Framebuffer,
        img: &Icon,
        dx: i32,
        dy: i32,
        size: i32,
        accent: Index,
    ) {
        let Some(mask) = scaled_mask(img, size) else {
            return;
        };
        let sz = size as usize;
        for ty in 0..sz {
            for tx in 0..sz {
                if !mask[ty * sz + tx] {
                    continue;
                }
                for oy in -3i32..=3 {
                    for ox in -3i32..=3 {
                        let px = dx + tx as i32 + ox;
                        let py = dy + ty as i32 + oy;
                        if px >= 0 && py >= 0 {
                            fb.set_pixel(px as usize, py as usize, accent);
                        }
                    }
                }
            }
        }
    }

    /// The na16 palette all art/indices resolve through, for callers running
    /// the `icon` colour pipeline.
    pub fn palette(&self) -> &PgPalette {
        &self.palette
    }

    /// Blit `img` nearest-scaled to a `size`x`size` box at (dx, dy). Icons
    /// are pre-quantized to palette colours, so each drawn pixel resolves to
    /// a palette index (nearest match) and alpha thresholds at 50%.
    fn draw_icon(&self, fb: &mut Framebuffer, img: &Icon, dx: i32, dy: i32, size: i32) {
        if img.w == 0 || img.h == 0 || size < 1 {
            return;
        }
        for ty in 0..size {
            let sy = (ty as u32 * img.h / size as u32).min(img.h - 1);
            let py = dy + ty;
            if py < 0 {
                continue;
            }
            for tx in 0..size {
                let sx = (tx as u32 * img.w / size as u32).min(img.w - 1);
                let px = dx + tx;
                if px < 0 {
                    continue;
                }
                let s = img.argb[(sy * img.w + sx) as usize];
                if (s >> 24) & 0xff < 128 {
                    continue;
                }
                let index = self.palette.nearest_index(PgRgb {
                    r: ((s >> 16) & 0xff) as u8,
                    g: ((s >> 8) & 0xff) as u8,
                    b: (s & 0xff) as u8,
                });
                fb.set_pixel(px as usize, py as usize, index);
            }
        }
    }
}

/// The `size`x`size` nearest-scaled opacity mask of `img` (alpha >= 50%),
/// row-major; `None` for empty inputs.
fn scaled_mask(img: &Icon, size: i32) -> Option<Vec<bool>> {
    if img.w == 0 || img.h == 0 || size < 1 {
        return None;
    }
    let sz = size as usize;
    let mut mask = vec![false; sz * sz];
    for ty in 0..sz {
        let sy = (ty as u32 * img.h / size as u32).min(img.h - 1);
        for tx in 0..sz {
            let sx = (tx as u32 * img.w / size as u32).min(img.w - 1);
            let s = img.argb[(sy * img.w + sx) as usize];
            mask[ty * sz + tx] = (s >> 24) & 0xff >= 128;
        }
    }
    Some(mask)
}

/// A taskbar tile rectangle (screen coords) for the renderer.
#[derive(Clone, Copy)]
pub struct TaskItem {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Draw a dithered pixel-art "+" insert button centred at (cx, cy).
pub fn draw_plus(fb: &mut Framebuffer, cx: i32, cy: i32, sz: i32) {
    let half = sz / 2;
    let (x, y) = (cx - half, cy - half);
    // Notched-corner tile, same chrome dither as the taskbar.
    fill_paint(fb, x + 2, y, sz - 4, sz, CHROME_BG);
    fill_paint(fb, x, y + 2, 2, sz - 4, CHROME_BG);
    fill_paint(fb, x + sz - 2, y + 2, 2, sz - 4, CHROME_BG);

    // 2px-thick plus arms.
    let arm = (sz * 28 / 100).max(2);
    fill(fb, cx - arm, cy - 1, 2 * arm, 2, palette_color::CREAM);
    fill(fb, cx - 1, cy - arm, 2, 2 * arm, palette_color::CREAM);
}

/// Draw the small close ("x") badge in the bottom-right corner of a taskbar
/// tile: a dark square with a cross, always visible so the close affordance
/// needs no hover state.
pub fn draw_close_badge(fb: &mut Framebuffer, x: i32, y: i32, sz: i32) {
    fill_paint(fb, x + 1, y, sz - 2, sz, PgPaint::Solid(palette_color::BLACK));
    fill_paint(fb, x, y + 1, 1, sz - 2, PgPaint::Solid(palette_color::BLACK));
    fill_paint(fb, x + sz - 1, y + 1, 1, sz - 2, PgPaint::Solid(palette_color::BLACK));

    // 2px-thick diagonal cross.
    let inset = sz * 32 / 100;
    let span = sz - 2 * inset;
    for i in 0..span {
        for t in 0..2 {
            let px = x + inset + i;
            let ny = y + inset + i + t; // "\" stroke
            let sy = y + sz - 1 - inset - i + t; // "/" stroke
            if px >= 0 && ny >= 0 {
                fb.set_pixel(px as usize, ny as usize, palette_color::CREAM);
            }
            if px >= 0 && sy >= 0 {
                fb.set_pixel(px as usize, sy as usize, palette_color::CREAM);
            }
        }
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
    /// to `accent_index` to match its leaf's border, at the art's native 1:1
    /// size. Every button swaps in its dedicated `*_disabled.png` art when
    /// disabled — it still tracks the leaf's accent so a disabled button
    /// doesn't look jarring against a coloured border, but any `LIME` pixel is
    /// additionally remapped to `LAVENDER` (across every accent, not just the
    /// one whose accent happens to be `LIME`), since lime reads as too
    /// vivid/live for a disabled control.
    pub fn draw_button(
        &self,
        fb: &mut Framebuffer,
        cx: i32,
        cy: i32,
        icon: BtnIcon,
        disabled: bool,
        accent_index: Index,
    ) {
        let art = &self.buttons[icon.index()];
        let (sprite, swap) = if disabled {
            (
                &art.disabled,
                accent_swap(accent_index)
                    .set(palette_color::LIME, PgPaint::Solid(palette_color::LAVENDER)),
            )
        } else {
            (&art.normal, accent_swap(accent_index))
        };
        fb.draw_sprite_swapped(
            sprite,
            (cx - sprite.width as i32 / 2) as isize,
            (cy - sprite.height as i32 / 2) as isize,
            &self.palette,
            &swap,
        );
    }
}

// --- application launcher menu ---

use crate::menu::{frame_size, MENU_BORDER, MENU_ROW_H};

const MENU_PAD_X: i32 = 12;
const MENU_ARROW_W: i32 = 16;

impl Renderer {
    /// Pixel width of a left-aligned string in the UI font. Without a font,
    /// a rough estimate keeps menu geometry sane.
    fn text_width(&self, s: &str) -> i32 {
        match &self.font {
            Some(font) => font.text_width(s) as i32,
            None => 8 * s.chars().count() as i32,
        }
    }

    /// Inner content width (excludes the black border) for a menu column.
    pub fn menu_content_w(&self, labels: &[String], any_arrow: bool) -> i32 {
        let mut w = 0;
        for l in labels {
            w = w.max(self.text_width(l));
        }
        let arrow = if any_arrow { MENU_ARROW_W } else { 0 };
        w + 2 * MENU_PAD_X + arrow
    }

    /// Render a menu column to its own framebuffer. `content_w` is the inner
    /// width (shared across a menu + submenu so columns line up); `seps`
    /// marks divider rows, `hi` is the hovered row.
    pub fn draw_menu(
        &self,
        labels: &[String],
        arrows: &[bool],
        seps: &[bool],
        content_w: i32,
        hi: Option<usize>,
    ) -> Framebuffer {
        let rows = labels.len() as i32;
        let (fw, fh) = frame_size(rows, content_w);
        let (w, h) = (fw.max(1) as usize, fh.max(1) as usize);
        let mut fb = Framebuffer::new(w, h, palette_color::BLACK);
        let b = MENU_BORDER;
        let cw = content_w;
        for (i, label) in labels.iter().enumerate() {
            let ry = b + i as i32 * MENU_ROW_H;
            if seps.get(i).copied().unwrap_or(false) {
                // Faint divider line centred in the row.
                fill(
                    &mut fb,
                    b + 4,
                    ry + MENU_ROW_H / 2,
                    cw - 8,
                    1,
                    palette_color::GUNMETAL,
                );
                continue;
            }
            if Some(i) == hi {
                // Hover fill with 1px-notched corners.
                fill(
                    &mut fb,
                    b + 3,
                    ry + 1,
                    cw - 6,
                    MENU_ROW_H - 2,
                    palette_color::GUNMETAL,
                );
                fill(
                    &mut fb,
                    b + 2,
                    ry + 2,
                    cw - 4,
                    MENU_ROW_H - 4,
                    palette_color::GUNMETAL,
                );
            }
            if let Some(font) = &self.font {
                let ty = ry + (MENU_ROW_H - font.cell_h() as i32) / 2;
                font.draw_text(
                    &mut fb,
                    label,
                    (b + MENU_PAD_X) as usize,
                    ty.max(0) as usize,
                    self.fg,
                );
            }
            if arrows.get(i).copied().unwrap_or(false) {
                // Small right-pointing triangle (▸): stacked shrinking rows.
                let ax = b + cw - MENU_ARROW_W + 4;
                let ay = ry + MENU_ROW_H / 2;
                for col in 0..6 {
                    let ext = (6 - col) * 4 / 6; // half-height tapers 4 -> 0
                    fill(&mut fb, ax + col, ay - ext, 1, 2 * ext + 1, self.fg);
                }
            }
        }
        fb
    }
}
