//! The split-control buttons (close/minimize) drawn over a leaf's
//! titlebar, on top of the chrome from `chrome`.

use pixel_graphics::Framebuffer;

use crate::Index;

use super::{accent_swap, Renderer};

/// The split-control buttons drawn at the right of every leaf's titlebar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnIcon {
    Close,
    /// A leaf alone in its column: minimizing collapses it to a narrow
    /// column, so the button previews that with `minimize.png`.
    Minimize,
    /// A stacked leaf: minimizing collapses it to a short row, so the
    /// button previews that with `minimize_h.png`.
    MinimizeH,
}

impl BtnIcon {
    pub(super) const COUNT: usize = 3;

    /// Slot into `Renderer.buttons`; must stay in sync with the array
    /// `Renderer::new` builds.
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Close => 0,
            Self::Minimize => 1,
            Self::MinimizeH => 2,
        }
    }
}

impl Renderer {
    /// Draw one bitmap split-control button centred at (cx, cy),
    /// palette-swapped to `accent_index` to match its leaf's border, at the
    /// art's native 1:1 size.
    pub fn draw_button(
        &self,
        fb: &mut Framebuffer,
        cx: i32,
        cy: i32,
        icon: BtnIcon,
        accent_index: Index,
    ) {
        let sprite = &self.buttons[icon.index()];
        fb.draw_sprite_swapped(
            sprite,
            (cx - sprite.width as i32 / 2) as isize,
            (cy - sprite.height as i32 / 2) as isize,
            self.palette.inner(),
            &accent_swap(accent_index),
        );
    }
}
