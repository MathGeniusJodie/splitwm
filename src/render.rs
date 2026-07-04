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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pixel_fonts::{BitmapFont, FUSION_PIXEL_12_SPEC};
use pixel_graphics::{
    Framebuffer, Paint as PgPaint, Palette as PgPalette, PresentLut, Rect as PgRect, Rgb as PgRgb,
    Rgba, Sprite, Swap,
};

use crate::icon::Icon;
use crate::oklch::OklabPalette;
use crate::theme::{self, palette_color};
use crate::Index;

/// Dithered "translucent" chrome background: a checker of black and gunmetal
/// stands in for a 50%-alpha black fill, keeping everything on the 16-colour
/// palette.
const CHROME_BG: PgPaint = PgPaint::Checker(palette_color::BLACK, palette_color::GUNMETAL);

pub struct Renderer {
    /// `None` when the pixel-fonts atlases can't be read; text drawing
    /// degrades to a no-op instead of refusing to start the WM.
    font: Option<BitmapFont>,
    /// The na16 palette all art/indices resolve through, paired with its
    /// precomputed OKLab coordinates for perceptual nearest-colour snapping.
    palette: OklabPalette,
    /// Index -> BGRX output table used by `present`.
    lut: Box<PresentLut>,
    /// Palette index used for foreground text/glyph strokes.
    fg: Index,
    /// Screen-sized scaled wallpaper (quantized to the palette), tagged
    /// with the (path, w, h) it was loaded for; frame backgrounds copy it
    /// each frame.
    wallpaper: Option<Wallpaper>,
    /// The full-screen compositing framebuffer, recycled between frames via
    /// `take_screen_base`/`retire_frame`: allocating ~8 MB per composited
    /// frame (60/s during animations) would be pure churn.
    frame: Option<Framebuffer>,
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
    /// `draw_icon`'s per-pixel `nearest_index` lookups are wasted work every
    /// frame — icons are already quantized to exact palette colours, so the
    /// resulting index buffer never changes for a given icon+size. Keyed by
    /// (`Icon::id`, size) — ids are process-unique, so a dropped icon's
    /// entry can never be served for a new one (a pointer key could, via
    /// allocator address reuse). Entries for dropped icons are dead weight,
    /// so the whole map is cleared once it exceeds `ICON_CACHE_CAP`.
    /// `Rc` payloads so a cache hit is a refcount bump, not a buffer copy.
    icon_idx_cache: IconCache<u8>,
}

/// A loaded wallpaper together with the (path, w, h) it was produced from,
/// so `set_wallpaper` can recognise a repeat request (e.g. a same-size root
/// ConfigureNotify) and skip the decode+dither pass.
struct Wallpaper {
    src: (String, i32, i32),
    fb: Framebuffer,
}

/// A per-(icon id, size) render cache; `Rc` payloads make a hit a refcount
/// bump rather than a buffer copy.
type IconCache<T> = RefCell<HashMap<(u64, i32), Rc<[T]>>>;

/// One titlebar button's art: the normal and dedicated disabled sprite.
struct ButtonArt {
    normal: Sprite,
    disabled: Sprite,
}

pub struct TabInfo {
    pub label: char,
    /// Icon to draw, already resolved by the caller — the hue-rotated
    /// variant when same-app disambiguation applies (see `Wm::icon_for`).
    pub icon: Option<Rc<Icon>>,
    /// `_NET_WM_NAME`/`WM_NAME`, drawn next to the icon/label when non-empty.
    pub title: Rc<str>,
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
    /// Whether split-control buttons are drawn over this titlebar afterward
    /// (`Wm::compose`'s `widgets` flag; always false for floats, which have
    /// no control buttons, and false for tiled splits during an animation
    /// frame, which composes with `widgets: false`) — the title text stops
    /// short of them only when they'll actually be drawn.
    pub buttons: bool,
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
    // Start at the first tile that reaches the clip rect: a leaf mostly
    // scrolled off-screen would otherwise walk (and fully clip) every
    // off-screen tile.
    let (sw_i, sh_i) = (src.w as i32, src.h as i32);
    let x0 = ox + ((cx0 - ox) / sw_i).max(0) * sw_i;
    let mut y = oy + ((cy0 - oy) / sh_i).max(0) * sh_i;
    while y < oy + h {
        let mut x = x0;
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
    /// The border art bakes decorative close/minimize/etc. icons into the
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
        // The sprite must cover its own configured insets or `sw - lu - ru`
        // (etc.) below underflows mid-frame; a mismatched border asset
        // should fail loudly in debug builds rather than panic on that
        // subtraction.
        debug_assert!(
            sw >= l as usize + r as usize && sh >= t as usize + b as usize,
            "NineSlice sprite ({sw}x{sh}) too small for insets l={l} t={t} r={r} b={b}"
        );
        // The edge-sample columns assume the art spans them; a narrower
        // redrawn asset would otherwise just render quietly truncated tiles
        // (`draw_sprite_full` clamps the source rect) with no loud failure.
        debug_assert!(
            sw >= Self::EDGE_SAMPLE_X1,
            "NineSlice sprite ({sw}px wide) doesn't cover edge-sample columns \
             {}..{}",
            Self::EDGE_SAMPLE_X0,
            Self::EDGE_SAMPLE_X1
        );
        if sw < l as usize + r as usize || sh < t as usize + b as usize {
            // Release-build fallback: nothing sane to draw, skip rather than
            // underflow.
            return;
        }
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
const MIN_CAP_H: usize = 18;
const MIN_CAP_W: usize = 18;

/// Gap between the window border and the titlebar's app icon/label, in px.
const TITLEBAR_ICON_PAD: i32 = 4;

impl Renderer {
    pub fn new() -> Self {
        let palette = OklabPalette::new(crate::assets::palette());
        let font = match BitmapFont::load(&FUSION_PIXEL_12_SPEC) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("splitwm: pixel font unavailable ({e}); text labels disabled");
                None
            }
        };
        let lut = palette.inner().present_lut();
        let fg = palette.inner().closest_to_white_index();
        Self {
            font,
            lut,
            fg,
            wallpaper: None,
            frame: None,
            border: NineSlice {
                sprite: crate::assets::winborder(),
                l: theme::BORDER_LEFT,
                t: theme::BORDER_TOP,
                r: theme::BORDER_RIGHT,
                b: theme::BORDER_BOTTOM,
            },
            minimized: crate::assets::winmin(),
            minimized_h: crate::assets::winmin_h(),
            // Order must match `BtnIcon::index`.
            buttons: [
                ButtonArt {
                    normal: crate::assets::close(),
                    disabled: crate::assets::close_disabled(),
                },
                ButtonArt {
                    normal: crate::assets::minimize(),
                    disabled: crate::assets::minimize_disabled(),
                },
                ButtonArt {
                    normal: crate::assets::minimize_h(),
                    disabled: crate::assets::minimize_h_disabled(),
                },
                ButtonArt {
                    normal: crate::assets::hsplit(),
                    disabled: crate::assets::hsplit_disabled(),
                },
                ButtonArt {
                    normal: crate::assets::vsplit(),
                    disabled: crate::assets::vsplit_disabled(),
                },
            ],
            palette,
            icon_idx_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Present an indexed framebuffer as X11 BGRX bytes into a slice of exactly
    /// `w * h * BYTES_PER_PIXEL` bytes — e.g. the MIT-SHM mapping, so the
    /// full-screen frame is written once, directly where the server reads it.
    pub fn present_into_slice(&self, fb: &Framebuffer, out: &mut [u8]) {
        // Fail loudly at the boundary rather than handing a short slice to
        // present_into — `out` is typically the MIT-SHM mapping, and a
        // framebuffer/segment resize race must not become a deep panic (or
        // worse) inside pixel-graphics.
        assert_eq!(
            out.len(),
            fb.width * fb.height * Framebuffer::BYTES_PER_PIXEL
        );
        fb.present_into(out, &self.lut);
    }

    /// Load+scale a PNG wallpaper to cover `w`x`h`, quantized onto the na16
    /// palette. Returns whether it loaded. No-op when the same wallpaper is
    /// already loaded at this size (e.g. a same-size root ConfigureNotify).
    pub fn set_wallpaper(&mut self, path: &str, w: i32, h: i32) -> bool {
        let src = (path.to_string(), w, h);
        if self.wallpaper.as_ref().is_some_and(|wp| wp.src == src) {
            return true;
        }
        self.wallpaper = self
            .load_wallpaper(path, w, h)
            .map(|fb| Wallpaper { src, fb });
        self.wallpaper.is_some()
    }

    fn load_wallpaper(&self, path: &str, w: i32, h: i32) -> Option<Framebuffer> {
        let (dw, dh) = (w.max(1) as usize, h.max(1) as usize);
        // The quantized result is cached on disk: the decode+dither pass
        // costs noticeable startup time on a full-screen image, and its
        // output only changes when the source file, target size or palette
        // does — exactly what the cache header records. Each distinct header
        // gets its own cache file (named from a hash of the header), so
        // multiple (path, size, palette) combinations coexist on disk
        // instead of thrashing a single shared slot.
        let header = self.wallpaper_cache_header(path, dw, dh);
        let cache = header.as_deref().and_then(wallpaper_cache_path);
        if let (Some(header), Some(cache)) = (header.as_deref(), cache.as_deref()) {
            if let Some(indices) = load_cached_wallpaper(cache, header, dw, dh) {
                return Some(fb_from_indices(dw, dh, &indices));
            }
        }
        let indices = self.dither_wallpaper(path, dw, dh)?;
        if let (Some(header), Some(cache)) = (header, cache) {
            // Best-effort: a failed write just means the next startup
            // re-dithers.
            let _ = store_cached_wallpaper(&cache, &header, &indices);
        }
        Some(fb_from_indices(dw, dh, &indices))
    }

    /// The validation header a cached quantization of `path` at `dw`x`dh`
    /// with the current palette must carry. `None` when the source file
    /// can't be stat'ed (it won't decode either).
    fn wallpaper_cache_header(&self, path: &str, dw: usize, dh: usize) -> Option<Vec<u8>> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let mut h = b"splitwm-wallpaper-v1\n".to_vec();
        h.extend((dw as u64).to_le_bytes());
        h.extend((dh as u64).to_le_bytes());
        h.extend(meta.len().to_le_bytes());
        h.extend(mtime.as_secs().to_le_bytes());
        h.extend(mtime.subsec_nanos().to_le_bytes());
        h.extend((path.len() as u64).to_le_bytes());
        h.extend(path.as_bytes());
        h.push(self.palette.inner().len() as u8);
        for i in 0..self.palette.inner().len() {
            let c = self.palette.inner().color(i as Index);
            h.extend([c.r, c.g, c.b]);
        }
        Some(h)
    }

    /// Decode `path` and scale-to-cover `dw`x`dh`, quantized onto the
    /// palette — the expensive pass behind the disk cache. Returns the
    /// row-major palette indices (the form the cache stores).
    fn dither_wallpaper(&self, path: &str, dw: usize, dh: usize) -> Option<Vec<Index>> {
        let (sw, sh, pixels) = Self::decode_image(path)?;
        // Belt-and-braces: a malformed/truncated wallpaper file must never
        // reach the sampling loop below, which indexes `pixels` unchecked.
        if sw == 0 || sh == 0 || pixels.len() < sw * sh {
            return None;
        }
        // Scale-to-cover with nearest-neighbour sampling, then quantize to
        // the palette with serpentine Floyd-Steinberg error diffusion so the
        // 16-colour result reads as smooth gradients instead of hard bands.
        let scale = (dw as f32 / sw as f32).max(dh as f32 / sh as f32);
        let ox = (sw as f32).mul_add(-scale, dw as f32) / 2.0;
        let oy = (sh as f32).mul_add(-scale, dh as f32) / 2.0;
        let mut indices = vec![palette_color::BLACK; dw * dh];
        // Two rows of per-channel accumulated error: current and next.
        let mut err_cur = vec![[0.0f32; 3]; dw];
        let mut err_next = vec![[0.0f32; 3]; dw];
        for y in 0..dh {
            let sy = (((y as f32 - oy) / scale) as usize).min(sh - 1);
            let ltr = y % 2 == 0;
            // Index flip instead of a boxed reversed iterator: this loop
            // body runs once per output pixel.
            for xi in 0..dw {
                let x = if ltr { xi } else { dw - 1 - xi };
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
                indices[y * dw + x] = index;
                let got = self.palette.inner().color(index);
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
            err_next.fill([0.0; 3]);
        }
        Some(indices)
    }

    /// Decode a wallpaper image (any format ImageMagick reads) into RGBA
    /// pixels via `icon::magick_decode_rgba`.
    fn decode_image(path: &str) -> Option<(usize, usize, Vec<Rgba>)> {
        // Widest wallpaper dimension worth keeping: caps the pixel buffer
        // and the O(w*h) dither pass below (decode itself happens in the
        // magick process, under its own resource limits).
        const MAX_DIM: usize = 16_384;
        crate::icon::magick_decode_rgba(path, MAX_DIM)
    }

    /// A screen-sized framebuffer initialised with the wallpaper (or the
    /// solid background colour). All leaf chrome is composited onto this.
    /// Recycles the previous frame's buffer (hand it back via
    /// `retire_frame`) so per-frame compositing allocates nothing.
    pub fn take_screen_base(&mut self, w: u32, h: u32) -> Framebuffer {
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        let mut fb = match self.frame.take() {
            Some(f) if f.width == w && f.height == h => f,
            _ => Framebuffer::new(w, h, palette_color::BLACK),
        };
        // A same-size wallpaper repaints every pixel on its own; only clear
        // first when it can't (absent, or mid-resize before its rescale).
        let covered = self
            .wallpaper
            .as_ref()
            .is_some_and(|wp| wp.fb.width >= w && wp.fb.height >= h);
        if !covered {
            fb.fill_rect_paint(0, 0, w, h, PgPaint::Solid(palette_color::BLACK));
        }
        if let Some(wp) = &self.wallpaper {
            // `copy_from`, not `blit_from`: the quantized wallpaper only
            // ever holds real palette indices (never `TRANSPARENT`), and
            // this is a full-screen blit on every composited frame —
            // `blit_from`'s per-pixel transparency test would be ~8M
            // pointless branches per 4K frame.
            fb.copy_from(&wp.fb, 0, 0);
        }
        fb
    }

    /// Return the compositing framebuffer for reuse by the next
    /// `take_screen_base`.
    pub fn retire_frame(&mut self, fb: Framebuffer) {
        self.frame = Some(fb);
    }

    /// 2px focus outline traced just inside the focused split's frame rect,
    /// in the palette's closest-to-white colour.
    pub fn draw_focus_outline(&self, fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32) {
        const T: i32 = 2;
        fill(fb, x, y, w, T, self.fg);
        fill(fb, x, y + h - T, w, T, self.fg);
        fill(fb, x, y + T, T, h - 2 * T, self.fg);
        fill(fb, x + w - T, y + T, T, h - 2 * T, self.fg);
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
            self.draw_minimized_axis(fb, &swap, ox, oy, v.w, v.h, v.w < v.h);
            return;
        }
        self.border
            .draw(fb, self.palette.inner(), &swap, ox, oy, v.w, v.h);
        self.draw_titlebar(fb, ox, oy, v);
    }

    /// A minimized leaf's restore-strip rendering: a 3-slice (rounded caps +
    /// a stretchy body) along the strip's long axis, the whole strip a
    /// single restore button. `vertical` selects the axis and, with it, the
    /// sprite: `winmin.png`/`MIN_CAP_H` for a minimized column (narrow,
    /// tall, caps stacked top/bottom) or `winmin_h.png`/`MIN_CAP_W` for a
    /// minimized row (short, wide, caps side by side). The strip isn't a
    /// tileable pattern along its short axis (it's a single pill), so it's
    /// drawn at its exact native cross-axis size, centred in whatever space
    /// the leaf collapsed to.
    #[allow(clippy::too_many_arguments)]
    fn draw_minimized_axis(
        &self,
        fb: &mut Framebuffer,
        swap: &Swap,
        ox: i32,
        oy: i32,
        w: i32,
        h: i32,
        vertical: bool,
    ) {
        let (s, cap) = if vertical {
            (&self.minimized, MIN_CAP_H)
        } else {
            (&self.minimized_h, MIN_CAP_W)
        };
        let (sw, sh) = (s.width, s.height);
        let cap_i = cap as i32;
        // (src rect, dest x, dest y, dest w, dest h) for the leading cap,
        // trailing cap, and stretchy middle, laid out along the chosen axis.
        let parts: [(PgRect, i32, i32, i32, i32); 3] = if vertical {
            let mid_h = (h - 2 * cap_i).max(1);
            let cx = ox + (w - sw as i32) / 2;
            [
                (PgRect::new(0, 0, sw, cap), cx, oy, sw as i32, cap_i),
                (
                    PgRect::new(0, sh - cap, sw, cap),
                    cx,
                    oy + h - cap_i,
                    sw as i32,
                    cap_i,
                ),
                (
                    PgRect::new(0, cap, sw, sh - 2 * cap),
                    cx,
                    oy + cap_i,
                    sw as i32,
                    mid_h,
                ),
            ]
        } else {
            let mid_w = (w - 2 * cap_i).max(1);
            let cy = oy + (h - sh as i32) / 2;
            [
                (PgRect::new(0, 0, cap, sh), ox, cy, cap_i, sh as i32),
                (
                    PgRect::new(sw - cap, 0, cap, sh),
                    ox + w - cap_i,
                    cy,
                    cap_i,
                    sh as i32,
                ),
                (
                    PgRect::new(cap, 0, sw - 2 * cap, sh),
                    ox + cap_i,
                    cy,
                    mid_w,
                    sh as i32,
                ),
            ]
        };
        for (src, x, y, dw, dh) in parts {
            tile_swapped(fb, s, src, x, y, dw, dh, self.palette.inner(), swap);
        }
    }

    fn draw_titlebar(&self, fb: &mut Framebuffer, ox: i32, oy: i32, v: &LeafView) {
        let Some(tab) = &v.tab else {
            return;
        };
        let isz = theme::BTN_SIZE;
        // Left padding between the window border and the app icon/label,
        // so the icon doesn't sit flush against the border art.
        let cx = ox + v.bw + isz / 2 + TITLEBAR_ICON_PAD;
        let cy = oy + v.tb_h / 2;
        if let Some(img) = &tab.icon {
            self.draw_icon(fb, img, cx - isz / 2, cy - isz / 2, isz);
        } else {
            self.draw_glyph(fb, tab.label, cx, cy, self.fg);
        }
        self.draw_title(fb, ox, oy, v, tab, cx + isz / 2);
    }

    /// Draw the window title after the icon/label, clipped so it never runs
    /// under the split-control buttons (drawn separately, on top, for tiled
    /// leaves — see `v.buttons`) or past the leaf's own right edge.
    fn draw_title(
        &self,
        fb: &mut Framebuffer,
        ox: i32,
        oy: i32,
        v: &LeafView,
        tab: &TabInfo,
        icon_right: i32,
    ) {
        if tab.title.is_empty() {
            return;
        }
        let Some(font) = &self.font else {
            return;
        };
        let text_x = icon_right + TITLEBAR_ICON_PAD;
        if v.buttons && v.w < theme::min_split_w() {
            // Too narrow for the 3-button strip: a single Minimize button is
            // centred instead, with no dedicated free strip for text (see
            // `compute_btn_regions`) — skip the title.
            return;
        }
        let right_limit = if v.buttons {
            theme::btn_strip_left(ox, v.w, v.bw)
        } else {
            ox + v.w - v.bw
        };
        if right_limit <= text_x {
            return;
        }
        let y = oy + (v.tb_h - font.cell_h() as i32) / 2;
        if y < 0 {
            return;
        }
        let clip_x = text_x.max(0) as usize;
        let clip_w = (right_limit - text_x) as usize;
        // Embossed look: a copy of the text one pixel up in the split's dark
        // accent shade, so the real text reads as if stamped into the bar.
        if y > 0 {
            font.draw_text_clipped(
                fb,
                &tab.title,
                text_x as isize,
                (y - 1) as usize,
                theme::darker_index(v.accent_index),
                clip_x,
                clip_w,
            );
        }
        font.draw_text_clipped(
            fb,
            &tab.title,
            text_x as isize,
            y as usize,
            self.fg,
            clip_x,
            clip_w,
        );
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
        // The font API's y origin is unsigned; a glyph poking past the top
        // edge is dropped (callers never place labels there). Negative x is
        // real (taskbar tiles fanning off the left edge) and clips instead.
        if y < 0 {
            return;
        }
        font.draw_text_clipped(fb, s, x as isize, y as usize, color, 0, fb.width);
    }

    /// Draw one taskbar entry: the app icon (or letter-glyph fallback) with
    /// a drop shadow, centred in its slot directly on the bar background.
    /// Windows currently shown in a split (`highlight`) get an
    /// accent-coloured rounded-rect box traced around the icon.
    pub fn draw_taskbar_item(
        &self,
        fb: &mut Framebuffer,
        r: crate::tree::Rect,
        icon: Option<&Icon>,
        label: char,
        accent: Index,
        highlight: bool,
    ) {
        let cx = r.x + r.w / 2;
        let cy = r.y + r.h / 2;
        let isz = r.h.min(r.w) - 6;
        let (dx, dy) = (cx - isz / 2, cy - isz / 2);
        if let Some(img) = icon {
            self.draw_icon_shadow(fb, img, dx, dy, isz);
        } else {
            self.draw_glyph(
                fb,
                label,
                cx + SHADOW_OFFSET,
                cy + SHADOW_OFFSET,
                SHADOW_COLOR,
            );
        }
        if highlight {
            let bx = dx - ICON_BOX_PAD;
            let by = dy - ICON_BOX_PAD;
            let bsz = isz + 2 * ICON_BOX_PAD;
            draw_rounded_box(fb, bx, by, bsz, bsz, accent);
        }
        if let Some(img) = icon {
            self.draw_icon(fb, img, dx, dy, isz);
        } else {
            self.draw_glyph(fb, label, cx, cy, self.fg);
        }
    }

    /// Draw a solid drop shadow behind `img`: its own opaque silhouette,
    /// offset by `SHADOW_OFFSET` and flattened to `SHADOW_COLOR`, reusing
    /// the cached per-pixel index buffer (`cached_icon_indices`) so this
    /// costs one extra store per opaque icon pixel instead of a second scale
    /// pass.
    fn draw_icon_shadow(&self, fb: &mut Framebuffer, img: &Icon, dx: i32, dy: i32, size: i32) {
        if img.w == 0 || img.h == 0 || size < 1 {
            return;
        }
        let sz = size as usize;
        let idx = self.cached_icon_indices(img, size);
        for ty in 0..size {
            let py = dy + SHADOW_OFFSET + ty;
            if py < 0 {
                continue;
            }
            for tx in 0..size {
                let px = dx + SHADOW_OFFSET + tx;
                if px < 0 {
                    continue;
                }
                if idx[ty as usize * sz + tx as usize] == TRANSPARENT_INDEX {
                    continue;
                }
                fb.set_pixel(px as usize, py as usize, SHADOW_COLOR);
            }
        }
    }

    /// The na16 palette all art/indices resolve through, for callers running
    /// the `icon` colour pipeline.
    pub fn palette(&self) -> &OklabPalette {
        &self.palette
    }

    /// Blit `img` nearest-scaled to a `size`x`size` box at (dx, dy). Icons
    /// are pre-quantized to palette colours, so each drawn pixel resolves to
    /// a palette index (nearest match) and alpha thresholds at 50%.
    fn draw_icon(&self, fb: &mut Framebuffer, img: &Icon, dx: i32, dy: i32, size: i32) {
        if img.w == 0 || img.h == 0 || size < 1 {
            return;
        }
        let sz = size as usize;
        let idx = self.cached_icon_indices(img, size);
        for ty in 0..size {
            let py = dy + ty;
            if py < 0 {
                continue;
            }
            for tx in 0..size {
                let px = dx + tx;
                if px < 0 {
                    continue;
                }
                let i = idx[ty as usize * sz + tx as usize];
                if i == TRANSPARENT_INDEX {
                    continue;
                }
                fb.set_pixel(px as usize, py as usize, i);
            }
        }
    }

    /// The `size`x`size` nearest-scaled palette-index buffer for `img`
    /// (`TRANSPARENT_INDEX` where alpha < 50%), computed once per
    /// icon+size and reused every frame after. Aspect-preserving: the
    /// icon's larger dimension maps to `size` and the other scales
    /// proportionally, centred — a non-square `_NET_WM_ICON` block renders
    /// letterboxed on transparent padding instead of stretched.
    fn cached_icon_indices(&self, img: &Icon, size: i32) -> Rc<[u8]> {
        // Callers (`draw_icon`) pre-check dims; the `img.h - 1` /
        // `img.w - 1` below would wrap to u32::MAX on a zero-sized icon,
        // and the cast lints that would flag it are allowed module-wide.
        debug_assert!(
            img.w > 0 && img.h > 0 && size >= 1,
            "cached_icon_indices needs non-empty icon and positive size"
        );
        let key = (img.id(), size);
        if let Some(v) = self.icon_idx_cache.borrow().get(&key) {
            return Rc::clone(v);
        }
        let sz = size as usize;
        let (iw, ih) = (img.w as usize, img.h as usize);
        let (dw, dh) = if iw >= ih {
            (sz, (ih * sz / iw).max(1))
        } else {
            ((iw * sz / ih).max(1), sz)
        };
        let (ox, oy) = ((sz - dw) / 2, (sz - dh) / 2);
        let mut idx = vec![TRANSPARENT_INDEX; sz * sz];
        for ty in 0..dh {
            let sy = (ty * ih / dh).min(ih - 1);
            for tx in 0..dw {
                let sx = (tx * iw / dw).min(iw - 1);
                let s = img.argb[sy * iw + sx];
                if (s >> 24) & 0xff < 128 {
                    continue;
                }
                idx[(oy + ty) * sz + ox + tx] = self.palette.nearest_index(PgRgb {
                    r: ((s >> 16) & 0xff) as u8,
                    g: ((s >> 8) & 0xff) as u8,
                    b: (s & 0xff) as u8,
                });
            }
        }
        let idx: Rc<[u8]> = idx.into();
        insert_capped(
            &mut self.icon_idx_cache.borrow_mut(),
            ICON_CACHE_CAP,
            key,
            Rc::clone(&idx),
        );
        idx
    }
}

/// Palette index is a valid `Index` for every real colour, so a distinct
/// out-of-band value marks "no pixel here" in the icon index cache.
const TRANSPARENT_INDEX: Index = Index::MAX;

/// Where the quantized wallpaper cache for a given `header` lives:
/// `$XDG_CACHE_HOME/splitwm/wallpaper-<hash>` (default `~/.cache`), where
/// `<hash>` is derived from the header's identity (source path, target size,
/// palette). Distinct headers land in distinct files, so switching between
/// wallpapers — or resizing a monitor — never evicts another entry's cache.
/// `None` when neither `XDG_CACHE_HOME` nor `HOME` is set.
fn wallpaper_cache_path(header: &[u8]) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    header.hash(&mut hasher);
    Some(
        base.join("splitwm")
            .join(format!("wallpaper-{:016x}", hasher.finish())),
    )
}

/// Read a cached quantized wallpaper: `header` followed by exactly `dw*dh`
/// palette indices. Any mismatch (stale source, different size/palette,
/// truncation) is a miss. Index *values* aren't validated — presenting runs
/// every index through a full 256-entry LUT, so corrupt bytes can only
/// render as wrong colours, never break memory safety.
fn load_cached_wallpaper(
    cache: &std::path::Path,
    header: &[u8],
    dw: usize,
    dh: usize,
) -> Option<Vec<Index>> {
    let mut bytes = std::fs::read(cache).ok()?;
    if !bytes.starts_with(header) || bytes.len() - header.len() != dw * dh {
        return None;
    }
    // Reuse the read buffer instead of copying the multi-megabyte body.
    Some(bytes.split_off(header.len()))
}

fn store_cached_wallpaper(
    cache: &std::path::Path,
    header: &[u8],
    indices: &[Index],
) -> std::io::Result<()> {
    if let Some(dir) = cache.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(header.len() + indices.len());
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(indices);
    std::fs::write(cache, bytes)
}

/// A `dw`x`dh` framebuffer holding `indices` (row-major, `dw*dh` entries).
fn fb_from_indices(dw: usize, dh: usize, indices: &[Index]) -> Framebuffer {
    let mut fb = Framebuffer::new(dw, dh, palette_color::BLACK);
    for y in 0..dh {
        for x in 0..dw {
            fb.set_pixel(x, y, indices[y * dw + x]);
        }
    }
    fb
}

/// Entry cap on the icon render caches. Entries for dropped icons are never
/// individually evicted (nothing tracks icon lifetimes here), so the maps
/// are wholesale-cleared at this size — icon churn (e.g. repeated
/// `_NET_WM_ICON` updates) then costs an occasional re-render instead of
/// unbounded growth. Live icons repopulate on the next frame.
const ICON_CACHE_CAP: usize = 256;

/// Insert into a cache map, wholesale-clearing it first once it reaches
/// `cap`: the shared "bounded in practice, but nothing should grow without
/// a lid" policy of every cache here and in `launch` (entries are cheap to
/// recompute, so occasional total eviction beats per-entry bookkeeping).
/// The clear discards *live* entries along with dead ones, so a working set
/// hovering at `cap` re-renders everything on the frame after each clear —
/// accepted because real working sets sit far below the caps.
pub(crate) fn insert_capped<K: std::hash::Hash + Eq, V>(
    map: &mut HashMap<K, V>,
    cap: usize,
    key: K,
    value: V,
) {
    if map.len() >= cap {
        map.clear();
    }
    map.insert(key, value);
}

/// Pixel offset (down and right) of a taskbar icon's drop shadow from the
/// icon itself.
const SHADOW_OFFSET: i32 = 2;

/// Flat colour a taskbar icon's drop shadow silhouette is drawn in.
const SHADOW_COLOR: Index = palette_color::BLACK;

/// Gap between a highlighted taskbar icon's own footprint and the
/// rounded-rect box traced around it.
const ICON_BOX_PAD: i32 = 3;

/// Thickness in pixels of the highlighted taskbar icon's rounded-rect box
/// stroke.
const ICON_BOX_THICKNESS: i32 = 2;

/// Stroke an accent-coloured rounded-rect box: a `w`x`h` outline with
/// 2px-notched corners, matching the pixel-art rounding used elsewhere in
/// the chrome. Used to mark a taskbar icon as currently shown in a split.
fn draw_rounded_box(fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, color: Index) {
    let t = ICON_BOX_THICKNESS;
    let paint = PgPaint::Solid(color);
    fill_paint(fb, x + 2, y, w - 4, t, paint); // top
    fill_paint(fb, x + 2, y + h - t, w - 4, t, paint); // bottom
    fill_paint(fb, x, y + 2, t, h - 4, paint); // left
    fill_paint(fb, x + w - t, y + 2, t, h - 4, paint); // right
}

/// Half-length of each "+" arm as a percentage of the icon's overall size
/// (clamped to a 2px minimum so the arms stay visible at the smallest icon
/// sizes); picked by eye to look proportionate against `draw_plus`'s notched
/// tile.
const PLUS_ARM_PCT: i32 = 28;

/// Draw a dithered pixel-art "+" insert button centred at (cx, cy).
pub fn draw_plus(fb: &mut Framebuffer, cx: i32, cy: i32, sz: i32) {
    let half = sz / 2;
    let (x, y) = (cx - half, cy - half);
    // Notched-corner tile, same chrome dither as the taskbar.
    fill_paint(fb, x + 2, y, sz - 4, sz, CHROME_BG);
    fill_paint(fb, x, y + 2, 2, sz - 4, CHROME_BG);
    fill_paint(fb, x + sz - 2, y + 2, 2, sz - 4, CHROME_BG);

    // 2px-thick plus arms.
    let arm = (sz * PLUS_ARM_PCT / 100).max(2);
    fill(fb, cx - arm, cy - 1, 2 * arm, 2, palette_color::CREAM);
    fill(fb, cx - 1, cy - arm, 2, 2 * arm, palette_color::CREAM);
}

/// Draw the vertical pill separating the taskbar's window tiles from its
/// quick-launch icons: a cream rounded bar, corners notched pixel-art style
/// like the tiles around it.
pub fn draw_taskbar_sep(fb: &mut Framebuffer, r: crate::tree::Rect) {
    fill(fb, r.x + 1, r.y, r.w - 2, r.h, palette_color::CREAM);
    fill(fb, r.x, r.y + 2, r.w, r.h - 4, palette_color::CREAM);
}

/// Inset of the diagonal cross's endpoints from the badge's corners, as a
/// percentage of the badge's overall size; picked by eye so the "x" strokes
/// clear the 1px border drawn around the badge.
const CLOSE_BADGE_INSET_PCT: i32 = 32;

/// Draw the small close ("x") badge in the bottom-right corner of a taskbar
/// tile: a dark square with a cross, always visible so the close affordance
/// needs no hover state.
pub fn draw_close_badge(fb: &mut Framebuffer, x: i32, y: i32, sz: i32) {
    fill_paint(
        fb,
        x + 1,
        y,
        sz - 2,
        sz,
        PgPaint::Solid(palette_color::BLACK),
    );
    fill_paint(
        fb,
        x,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(palette_color::BLACK),
    );
    fill_paint(
        fb,
        x + sz - 1,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(palette_color::BLACK),
    );

    // 2px-thick diagonal cross.
    let inset = sz * CLOSE_BADGE_INSET_PCT / 100;
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
            self.palette.inner(),
            &swap,
        );
    }
}

// --- notification speech bubbles ---

/// 9-slice caps of `bubble.png` (matching cozyui's), measured on the
/// *unmirrored* art; `draw_note` mirrors it so the tail points bottom-right,
/// toward the dock.
const BUBBLE_CAP_X: usize = 21;
const BUBBLE_CAP_TOP: usize = 17;
const BUBBLE_CAP_BOTTOM: usize = 17;
/// Text padding inside the bubble; the bottom leaves room for the tail band.
const NOTE_PAD_LEFT: usize = 14;
const NOTE_PAD_RIGHT: usize = 16;
const NOTE_PAD_TOP: usize = 10;
const NOTE_PAD_BOTTOM: usize = 16;
const NOTE_TEXT_MAX_W: usize = 280;
const NOTE_MAX_LINES: usize = 8;

impl Renderer {
    /// Pixel width of a left-aligned string in the UI font. Without a font,
    /// a rough estimate keeps bubble geometry sane.
    fn text_width(&self, s: &str) -> i32 {
        match &self.font {
            Some(font) => font.text_width(s) as i32,
            None => 8 * s.chars().count() as i32,
        }
    }

    /// Render one notification as a speech bubble: summary (bold) then body,
    /// wrapped, on the 9-slice-stretched bubble sprite. The framebuffer is
    /// exactly the popup window's size; pixels outside the bubble stay
    /// `TRANSPARENT` for the caller to shape away.
    pub fn draw_note(&self, summary: &str, body: &str) -> Framebuffer {
        let bubble = crate::assets::bubble();
        let line_h = self.font.as_ref().map_or(14, |f| f.cell_h() + 2);

        // (line, bold) — summary lines bold, body lines regular.
        let mut lines: Vec<(String, bool)> = Vec::new();
        if let Some(font) = &self.font {
            let layout = pixel_fonts::TextLayout::new(
                font,
                0,
                0,
                NOTE_TEXT_MAX_W,
                pixel_fonts::LinePlacement::Uniform { line_h },
            );
            for (text, bold) in [(summary, true), (body, false)] {
                for para in text.split('\n').filter(|p| !p.is_empty()) {
                    if lines.len() >= NOTE_MAX_LINES {
                        break;
                    }
                    // Wrapping is O(input) and the body is unauthenticated
                    // bus input of unbounded length: feed the wrapper only
                    // what can still be shown. Every glyph is at least 1px
                    // wide, so a line NOTE_TEXT_MAX_W px wide holds at most
                    // NOTE_TEXT_MAX_W chars.
                    let budget = (NOTE_MAX_LINES - lines.len()) * NOTE_TEXT_MAX_W;
                    let capped = crate::notify::cap_chars(para, budget);
                    lines.extend(layout.wrap(capped).into_iter().map(|l| (l, bold)));
                }
            }
        }
        lines.truncate(NOTE_MAX_LINES);

        let text_w = lines
            .iter()
            .map(|(l, _)| self.text_width(l) as usize)
            .max()
            .unwrap_or(0);
        // min/max, not clamp(): clamp panics if min > max, and nothing ties
        // the baked bubble art's width to the text-cap constants — if the
        // art ever grows past the cap, its width wins as the floor instead
        // of panicking on every incoming notification.
        let w = (text_w + NOTE_PAD_LEFT + NOTE_PAD_RIGHT)
            .min(NOTE_TEXT_MAX_W + NOTE_PAD_LEFT + NOTE_PAD_RIGHT)
            .max(bubble.width);
        let h = (lines.len().max(1) * line_h + NOTE_PAD_TOP + NOTE_PAD_BOTTOM)
            .max(BUBBLE_CAP_TOP + BUBBLE_CAP_BOTTOM + 1);

        let mut fb = Framebuffer::new(w, h, pixel_graphics::TRANSPARENT);
        for dy in 0..h {
            let sy = pixel_graphics::stretch_source_coord(
                dy,
                h,
                bubble.height,
                BUBBLE_CAP_TOP,
                BUBBLE_CAP_BOTTOM,
            );
            for dx in 0..w {
                // Horizontal mirror: sample the flipped column so the tail
                // (bottom-left in the art) lands bottom-right on screen.
                let sx = bubble.width
                    - 1
                    - pixel_graphics::stretch_source_coord(
                        dx,
                        w,
                        bubble.width,
                        BUBBLE_CAP_X,
                        BUBBLE_CAP_X,
                    );
                let idx = bubble.at(sx, sy);
                if idx != pixel_graphics::TRANSPARENT {
                    fb.set_pixel(dx, dy, idx);
                }
            }
        }

        if let Some(font) = &self.font {
            let mut y = NOTE_PAD_TOP;
            for (line, bold) in &lines {
                font.draw_text(&mut fb, line, NOTE_PAD_LEFT, y, palette_color::BLACK);
                if *bold {
                    // Faux bold: restrike one pixel right.
                    font.draw_text(&mut fb, line, NOTE_PAD_LEFT + 1, y, palette_color::BLACK);
                }
                y += line_h;
            }
        }
        fb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_cache_round_trips_and_rejects_stale_headers() {
        let dir = std::env::temp_dir().join(format!("splitwm-wp-test-{}", std::process::id()));
        let cache = dir.join("wallpaper");
        let indices: Vec<Index> = vec![0, 1, 2, 3, 4, 5];
        let header = b"splitwm-wallpaper-v1\ntest-header".to_vec();

        store_cached_wallpaper(&cache, &header, &indices).unwrap();
        assert_eq!(
            load_cached_wallpaper(&cache, &header, 3, 2).as_deref(),
            Some(indices.as_slice())
        );

        // Any header mismatch (stale mtime/size/palette/...) is a miss, as
        // is a body of the wrong length for the requested dimensions.
        assert!(load_cached_wallpaper(&cache, b"splitwm-wallpaper-v1\nother", 3, 2).is_none());
        assert!(load_cached_wallpaper(&cache, &header, 4, 2).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wallpaper_cache_path_varies_with_header() {
        // Two different (path, size, palette) identities must land in two
        // different cache files, so switching wallpapers (or resizing a
        // monitor) never evicts the other's cached entry.
        let a = wallpaper_cache_path(b"splitwm-wallpaper-v1\nfoo.png-100x100").unwrap();
        let b = wallpaper_cache_path(b"splitwm-wallpaper-v1\nbar.png-200x200").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());

        // Same header, same path — deterministic, so a second lookup of the
        // same wallpaper actually hits its own cache file.
        let a2 = wallpaper_cache_path(b"splitwm-wallpaper-v1\nfoo.png-100x100").unwrap();
        assert_eq!(a, a2);
    }
}
