//! The bottom taskbar's own drawing: icon tiles (with drop shadow and
//! shown-in-a-split highlight), the separator pill before the quick-launch
//! icons, the close badge on each tile, and the hover compass around a
//! quick-launch icon.

use pixel_graphics::{Framebuffer, Paint as PgPaint, PaletteIndex};

use crate::icon::Icon;
use crate::layout::Side;
use crate::theme::palette_color;
use crate::Index;

use super::{fill, fill_paint, Renderer};

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

impl Renderer {
    /// Draw one taskbar entry: the app icon (or letter-glyph fallback) with
    /// a drop shadow, centred in its slot directly on the bar background.
    /// Windows currently shown in a split (`highlight`) get an
    /// accent-coloured rounded-rect box traced around the icon.
    pub fn draw_taskbar_item(
        &self,
        fb: &mut Framebuffer,
        r: crate::layout::Rect,
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
        self.for_each_icon_pixel(
            img,
            dx + SHADOW_OFFSET,
            dy + SHADOW_OFFSET,
            size,
            |px, py, _| fb.set_pixel(px as isize, py as isize, SHADOW_COLOR),
        );
    }
}

/// Stroke an accent-coloured rounded-rect box: a `w`x`h` outline with
/// 2px-notched corners, matching the pixel-art rounding used elsewhere in
/// the chrome. Used to mark a taskbar icon as currently shown in a split.
fn draw_rounded_box(fb: &mut Framebuffer, x: i32, y: i32, w: i32, h: i32, color: Index) {
    let t = ICON_BOX_THICKNESS;
    let paint = PgPaint::Solid(PaletteIndex::new(color));
    fill_paint(fb, x + 2, y, w - 4, t, paint); // top
    fill_paint(fb, x + 2, y + h - t, w - 4, t, paint); // bottom
    fill_paint(fb, x, y + 2, t, h - 4, paint); // left
    fill_paint(fb, x + w - t, y + 2, t, h - 4, paint); // right
}

/// Draw the vertical pill separating the taskbar's window tiles from its
/// quick-launch icons: a cream rounded bar, corners notched pixel-art style
/// like the tiles around it.
pub fn draw_taskbar_sep(fb: &mut Framebuffer, r: crate::layout::Rect) {
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
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
    );
    fill_paint(
        fb,
        x,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
    );
    fill_paint(
        fb,
        x + sz - 1,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
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
                fb.set_pixel(px as isize, ny as isize, palette_color::CREAM);
            }
            if px >= 0 && sy >= 0 {
                fb.set_pixel(px as isize, sy as isize, palette_color::CREAM);
            }
        }
    }
}

/// Width of the gap the diagonals cut between two neighbouring compass
/// wedges, in pixels. The wedges carry no outline, so this gap is what
/// separates them.
const COMPASS_GAP: i32 = 4;

/// Draw the hover compass around a quick-launch icon: a square, a ring
/// wider than the icon on every side, cut by its diagonals into the four
/// wedges that place the launched window left of, right of, above or below
/// the focused split. The wedge under the pointer (`hover`) is solid white;
/// the rest are the same white on a checker, so they read as half-lit next
/// to it and let what they cover show between their pixels. The icon is
/// drawn over the square's middle, which the wedges run under — it is part
/// of them, not a hole in them.
pub fn draw_compass(fb: &mut Framebuffer, r: crate::layout::Rect, hover: Side) {
    // Doubled coordinates keep the centre of an even-sided square exact.
    let (cx2, cy2) = (2 * r.x + r.w, 2 * r.y + r.h);
    for y in r.y.max(0)..r.y + r.h {
        for x in r.x.max(0)..r.x + r.w {
            // Notched corners, the pixel-art rounding the rest of the
            // chrome uses.
            let notch = (x < r.x + 2 || x >= r.x + r.w - 2) && (y < r.y + 2 || y >= r.y + r.h - 2);
            if notch {
                continue;
            }
            let (dx, dy) = (2 * x + 1 - cx2, 2 * y + 1 - cy2);
            // The diagonals themselves stay clear: equidistant from both
            // axes is the gap between two wedges.
            if (dx.abs() - dy.abs()).abs() < 2 * COMPASS_GAP {
                continue;
            }
            if crate::widgets::compass_side(dx, dy) != hover && (x + y) % 2 != 0 {
                continue;
            }
            fb.set_pixel(x as isize, y as isize, palette_color::CREAM);
        }
    }
}
