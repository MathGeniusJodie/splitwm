//! The split-control buttons (close/minimize/split) drawn over a leaf's
//! titlebar, on top of the chrome from `chrome`.

use pixel_graphics::{Framebuffer, Paint as PgPaint, PaletteIndex, Sprite};

use crate::theme::palette_color;
use crate::Index;

use super::{accent_swap, Renderer};

/// One titlebar button's art: the normal and dedicated disabled sprite.
pub(super) struct ButtonArt {
    pub(super) normal: Sprite,
    pub(super) disabled: Sprite,
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
    pub(super) const COUNT: usize = 5;

    /// Slot into `Renderer.buttons`; must stay in sync with the array
    /// `Renderer::new` builds.
    pub(super) const fn index(self) -> usize {
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
                accent_swap(accent_index).set(
                    palette_color::LIME,
                    PgPaint::Solid(PaletteIndex::new(palette_color::LAVENDER)),
                ),
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
