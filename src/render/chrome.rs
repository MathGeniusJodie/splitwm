//! A leaf's own chrome: the bitmap window border, the titlebar (app icon +
//! title text), and the collapsed-to-a-strip rendering of a minimized leaf.
//! Buttons drawn over the titlebar live in `buttons`; the taskbar's icon
//! tiles live in `taskbar`.

use std::rc::Rc;

use pixel_graphics::{Framebuffer, Palette as PgPalette, Rect as PgRect, Sprite, Swap};

use crate::icon::Icon;
use crate::theme;
use crate::Index;

use super::{accent_swap, Renderer};

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
pub(super) struct NineSlice {
    pub(super) sprite: Sprite,
    pub(super) l: i32,
    pub(super) t: i32,
    pub(super) r: i32,
    pub(super) b: i32,
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

/// The `winmin.png` vertical 3-slice caps / `winmin_h.png` horizontal ones.
const MIN_CAP_H: usize = 18;
const MIN_CAP_W: usize = 18;

/// Gap between the window border and the titlebar's app icon/label, in px.
const TITLEBAR_ICON_PAD: i32 = 4;

impl Renderer {
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
}
