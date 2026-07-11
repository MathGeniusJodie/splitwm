//! A leaf's own chrome: the bitmap window border, the titlebar (app icon +
//! title text), and the collapsed-to-a-strip rendering of a minimized leaf.
//! Buttons drawn over the titlebar live in `buttons`; the taskbar's icon
//! tiles live in `taskbar`.

use std::rc::Rc;

use pixel_graphics::{Framebuffer, Palette as PgPalette, Sprite};

use crate::icon::Icon;
use crate::theme;
use crate::Index;

use super::Renderer;

pub struct TitleInfo {
    pub label: char,
    /// Icon to draw, already resolved by the caller — the hue-rotated
    /// variant when same-app disambiguation applies (see `Comp::icon_for`).
    pub icon: Option<Rc<Icon>>,
    /// `_NET_WM_NAME`/`WM_NAME`, drawn next to the icon/label when non-empty.
    pub title: Rc<str>,
}

/// A titlebar strip's draw inputs (`draw_titlebar_strip`); the frame around
/// it is the GPU-sliced border art, so nothing here describes the frame body.
pub struct LeafView {
    pub w: i32,
    pub tb_h: i32,
    pub bw: i32,
    /// Palette index this split's border and titlebar buttons are swapped to.
    pub accent_index: Index,
    /// The split's single window, if any.
    pub titlebar: Option<TitleInfo>,
    /// Whether split-control buttons are drawn over this titlebar afterward
    /// (always false for floats, which have no control buttons) — the title
    /// text stops short of them only when they'll actually be drawn.
    pub buttons: bool,
}

/// The window-border sprite with its 9-slice geometry: 4 fixed corners plus
/// edges/a center that tile to fill an arbitrary target rect at native
/// resolution — resolved by the GPU nine-slice shader via `border_art`'s
/// `SliceSpec`.
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
}

/// The `winmin.png` vertical 3-slice caps / `winmin_h.png` horizontal ones.
const MIN_CAP_H: usize = 18;
const MIN_CAP_W: usize = 18;

/// How a static frame sprite slices over an arbitrary destination rect, for
/// the GPU nine-slice shader (`render::indexed::NineSliceElement`): the fixed
/// margins plus the source column range the horizontal middle tiles from
/// (narrower than the span between the corners when decoration is baked
/// there — see `NineSlice::EDGE_SAMPLE_*`). The vertical middle always tiles
/// `t..height-b`. Every frame — leaf borders and float frames alike — slices
/// through this same GPU path now; there is no CPU nine-slice draw left.
pub struct SliceSpec {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
    pub edge0: i32,
    pub edge1: i32,
}

/// Rasterize `sprite` to an identity-indexed framebuffer (holes stay
/// `TRANSPARENT`), ready to upload once as the shader's static art.
fn sprite_fb(sprite: &Sprite, palette: &PgPalette) -> Framebuffer {
    let mut fb = Framebuffer::new(sprite.width, sprite.height, pixel_graphics::TRANSPARENT);
    fb.draw_sprite(sprite, 0, 0, palette);
    fb
}

/// Gap between the window border and the titlebar's app icon/label, in px.
const TITLEBAR_ICON_PAD: i32 = 4;

impl Renderer {
    /// The window-border sprite as an uploadable framebuffer with its slice
    /// spec, for the GPU nine-slice frames (uploaded once, shared by every
    /// tiled leaf and float).
    pub fn border_art(&self) -> (Framebuffer, SliceSpec) {
        let b = &self.border;
        // A redrawn asset too small for its own insets or edge-sample
        // columns would render quietly wrong through the shader's clamped
        // sampling; fail loudly in debug builds instead.
        debug_assert!(
            b.sprite.width >= (b.l + b.r) as usize
                && b.sprite.height >= (b.t + b.b) as usize
                && b.sprite.width >= NineSlice::EDGE_SAMPLE_X1,
            "border sprite ({}x{}) too small for insets l={} t={} r={} b={} \
             / edge-sample columns {}..{}",
            b.sprite.width,
            b.sprite.height,
            b.l,
            b.t,
            b.r,
            b.b,
            NineSlice::EDGE_SAMPLE_X0,
            NineSlice::EDGE_SAMPLE_X1
        );
        (
            sprite_fb(&b.sprite, self.palette.inner()),
            SliceSpec {
                l: b.l,
                t: b.t,
                r: b.r,
                b: b.b,
                edge0: NineSlice::EDGE_SAMPLE_X0 as i32,
                edge1: NineSlice::EDGE_SAMPLE_X1 as i32,
            },
        )
    }

    /// A minimized leaf's restore-strip sprite (`vertical` picks
    /// `winmin.png` vs `winmin_h.png`) with its 3-slice spec: caps fixed
    /// along the long axis, the pill body tiling between them, the cross
    /// axis at native size (the strip element is created at exactly that
    /// size, so its zero margins never stretch anything).
    pub fn minimized_art(&self, vertical: bool) -> (Framebuffer, SliceSpec) {
        let (sprite, spec) = if vertical {
            let s = &self.minimized;
            let cap = MIN_CAP_H as i32;
            (
                s,
                SliceSpec {
                    l: 0,
                    t: cap,
                    r: 0,
                    b: cap,
                    edge0: 0,
                    edge1: s.width as i32,
                },
            )
        } else {
            let s = &self.minimized_h;
            let cap = MIN_CAP_W as i32;
            (
                s,
                SliceSpec {
                    l: cap,
                    t: 0,
                    r: cap,
                    b: 0,
                    edge0: cap,
                    edge1: s.width as i32 - cap,
                },
            )
        };
        (sprite_fb(sprite, self.palette.inner()), spec)
    }

    /// The titlebar's *contents* — app icon/label and title text, no border
    /// art — drawn at (0, 0) into a `w`x`tb_h` strip buffer. The band fill
    /// behind them comes from the nine-slice frame's top margin; split
    /// buttons draw over via `draw_button`.
    pub fn draw_titlebar_strip(&self, fb: &mut Framebuffer, v: &LeafView) {
        self.draw_titlebar(fb, 0, 0, v);
    }

    fn draw_titlebar(&self, fb: &mut Framebuffer, ox: i32, oy: i32, v: &LeafView) {
        let Some(title) = &v.titlebar else {
            return;
        };
        let isz = theme::BTN_SIZE;
        // Left padding between the window border and the app icon/label,
        // so the icon doesn't sit flush against the border art.
        let cx = ox + v.bw + isz / 2 + TITLEBAR_ICON_PAD;
        let cy = oy + v.tb_h / 2;
        if let Some(img) = &title.icon {
            self.draw_icon(fb, img, cx - isz / 2, cy - isz / 2, isz);
        } else {
            self.draw_glyph(fb, title.label, cx, cy, self.fg);
        }
        self.draw_title(fb, ox, oy, v, title, cx + isz / 2);
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
        title: &TitleInfo,
        icon_right: i32,
    ) {
        if title.title.is_empty() {
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
        let clip_x = text_x.max(0) as isize;
        let clip_w = (right_limit - text_x) as usize;
        // Embossed look: a copy of the text offset one pixel vertically in an
        // accent shade, so the real text reads as if stamped into the bar.
        // Text contrast follows the accent's luma: dark accents get light
        // text with the dark shade above; light accents get dark text with
        // the light shade below.
        let (text_color, shadow_y, shadow_color) = if self.accent_is_light(v.accent_index) {
            (self.fg_dark, y + 1, theme::lighter_index(v.accent_index))
        } else {
            (self.fg, y - 1, theme::darker_index(v.accent_index))
        };
        if shadow_y >= 0 {
            font.draw_text_clipped(
                fb,
                &title.title,
                text_x as isize,
                shadow_y as isize,
                shadow_color,
                clip_x,
                clip_w,
            );
        }
        font.draw_text_clipped(
            fb,
            &title.title,
            text_x as isize,
            y as isize,
            text_color,
            clip_x,
            clip_w,
        );
    }
}
