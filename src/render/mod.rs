//! Software rendering of the chrome pieces (wallpaper, leaf decorations,
//! taskbar, insert buttons) as indexed-colour `pixel_graphics::Framebuffer`s.
//! The index bytes upload straight to the GPU as `R8` textures and the
//! palette lookup happens in a fragment shader (`comp::indexed`); this module
//! only draws the indices. Each piece is drawn into its own small buffer at
//! its own origin (leaf frames at (0,0) into a leaf-sized buffer, the taskbar
//! into a strip-sized buffer, each "+" into a square) — see `comp::chrome`,
//! which owns the per-piece texture caches and composites them by position.
//! Each concern owns its own module: wallpaper loading/caching in
//! `wallpaper`, a leaf's border/titlebar chrome in `chrome`, its
//! split-control buttons in `buttons`, icon blitting/caching (shared by the
//! titlebar and the taskbar) in `icon_cache`, the taskbar's own
//! tiles/badges/insert-button in `taskbar`, and served-notification speech
//! bubbles in `notify_popup`. The `Renderer` struct itself and the handful
//! of drawing primitives genuinely shared across all of the above live here.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]

mod buttons;
mod chrome;
mod icon_cache;
mod notify_popup;
mod taskbar;
mod wallpaper;

pub use buttons::BtnIcon;
pub use chrome::{LeafView, TitleInfo};
#[allow(unused_imports)] // consumed when the taskbar composes (M3-b)
pub use taskbar::{draw_close_badge, draw_plus, draw_taskbar_sep};

use std::cell::RefCell;
use std::collections::HashMap;

use pixel_fonts::{BitmapFont, FUSION_PIXEL_12_SPEC};
use pixel_graphics::{Framebuffer, Paint as PgPaint, PaletteIndex, Swap};

use crate::oklch::OklabPalette;
use crate::theme::{self, palette_color};
use crate::Index;

use buttons::ButtonArt;
use chrome::NineSlice;
use icon_cache::IconCache;
use wallpaper::Wallpaper;

pub struct Renderer {
    /// `None` when the pixel-fonts atlases can't be read; text drawing
    /// degrades to a no-op instead of refusing to start the WM.
    font: Option<BitmapFont>,
    /// The na16 palette all art/indices resolve through, paired with its
    /// precomputed `OKLab` coordinates for perceptual nearest-colour snapping.
    palette: OklabPalette,
    /// Palette index used for foreground text/glyph strokes.
    fg: Index,
    /// Screen-sized scaled wallpaper (quantized to the palette), tagged
    /// with the (path, w, h) it was loaded for; `wallpaper_base` copies it
    /// into the full-output wallpaper piece on load/resize.
    wallpaper: Option<Wallpaper>,
    /// Bitmap window border; palette-swapped to each leaf's accent at draw
    /// time via `accent_swap`.
    border: NineSlice,
    /// The `winmin.png` restore strip for a minimized *column* (squished
    /// narrow, so the strip runs vertically) and `winmin_h.png` for a
    /// minimized *row* (squished short, strip runs horizontally) — picked in
    /// `draw_leaf` by the minimized leaf's own aspect ratio. Accent-swapped
    /// at draw time like the border.
    minimized: pixel_graphics::Sprite,
    minimized_h: pixel_graphics::Sprite,
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
        x0 as isize,
        y0 as isize,
        (x1 - x0) as usize,
        (y1 - y0) as usize,
        paint,
    );
}

fn fill(fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, index: Index) {
    fill_paint(fb, x, y, w, h, PgPaint::Solid(PaletteIndex::new(index)));
}

/// The accent remap shared by the border and its titlebar buttons: the
/// titlebar/body fill (`LAVENDER`) becomes `index`, the outline (`PURPLE`)
/// becomes its hand-picked darker counterpart (`theme::DARKER_INDEX`), and
/// the highlight stroke (`CREAM`) becomes its hand-picked lighter
/// counterpart (`theme::LIGHTER_INDEX`).
fn accent_swap(index: Index) -> Swap {
    Swap::identity()
        .set(
            palette_color::LAVENDER,
            PgPaint::Solid(PaletteIndex::new(index)),
        )
        .set(
            palette_color::PURPLE,
            PgPaint::Solid(PaletteIndex::new(theme::darker_index(index))),
        )
        .set(
            palette_color::CREAM,
            PgPaint::Solid(PaletteIndex::new(theme::lighter_index(index))),
        )
}

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
        let fg = palette.inner().closest_to_white_index();
        Self {
            font,
            fg,
            wallpaper: None,
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

    /// The full-output wallpaper piece: a `w`x`h` framebuffer holding the
    /// scaled wallpaper (or the solid background colour where the image
    /// doesn't reach), fully opaque. Rebuilt only on load / resize, so a
    /// fresh allocation is fine here.
    pub fn wallpaper_base(&self, w: u32, h: u32) -> Framebuffer {
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        let mut fb = Framebuffer::new(w, h, palette_color::BLACK);
        if let Some(wp) = &self.wallpaper {
            // `copy_from`, not `blit_from`: the quantized wallpaper only ever
            // holds real palette indices (never `TRANSPARENT`), so no
            // per-pixel transparency test is needed; the black initialiser
            // backs any margin an undersized image leaves.
            fb.copy_from(&wp.fb, 0, 0);
        }
        fb
    }

    /// The focus outline's colour: the palette's closest-to-white entry as
    /// premultiplied `[r, g, b, a]` (opaque, so the channels pass through).
    /// The 2px outline draws as its own GPU solid element stacked over the
    /// underlay, not painted into it, so a focus change moves the outline
    /// without recompositing the chrome.
    pub fn focus_color(&self) -> [f32; 4] {
        let c = self.palette.inner().color(self.fg);
        [
            f32::from(c.r) / 255.0,
            f32::from(c.g) / 255.0,
            f32::from(c.b) / 255.0,
            1.0,
        ]
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
        // A glyph poking past the top edge is dropped (callers never place
        // labels there). Negative x is real (taskbar tiles fanning off the
        // left edge) and clips instead.
        if y < 0 {
            return;
        }
        font.draw_text_clipped(fb, s, x as isize, y as isize, color, 0, fb.width);
    }

    /// The na16 palette all art/indices resolve through, for callers running
    /// the `icon` colour pipeline.
    pub fn palette(&self) -> &OklabPalette {
        &self.palette
    }
}

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
