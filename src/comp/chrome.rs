//! Compositing the software-rendered chrome under the client windows.
//!
//! The ported `render::Renderer` draws wallpaper + leaf frames into one
//! palette-indexed full-output `Framebuffer` (the X11 version's underlay);
//! here it is presented into a `MemoryRenderBuffer` (smithay owns the
//! texture upload and damage) and rendered as the bottom element of every
//! frame. Nothing allocates per frame: the indexed framebuffer recycles
//! via take_screen_base/retire_frame, and the memory buffer is rewritten
//! only when the chrome is actually dirty.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::desktop::space::SpaceRenderElements;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::render_elements;
use smithay::utils::{Buffer as BufferCoords, Rectangle, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::Comp;
use crate::render::{LeafView, TitleInfo};
use crate::theme;
use crate::tree::{NodeId, Rect, Win};

render_elements! {
    /// Everything one output frame is made of, front-to-back: client
    /// surfaces (with their popups) above, the chrome underlay below.
    pub OutputElement<=GlesRenderer>;
    Window=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Chrome=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// One on-screen leaf's placement, captured by `arrange` for the chrome
/// composer and (from M5) the pointer's hit regions. Present for every
/// visible leaf — empty and minimized ones still draw chrome.
pub struct Placement {
    pub leaf: NodeId,
    /// Screen-space frame rect (scroll already applied).
    pub frame: Rect,
    pub client: Option<Win>,
    pub focused: bool,
}

impl Comp {
    /// Redraw the underlay for the current placements. Runs only when
    /// something chrome-visible changed (`chrome_dirty`), not per frame.
    pub fn compose_chrome(&mut self) {
        let size = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| self.backend.window_size());
        let (w, h) = (size.w.max(1), size.h.max(1));

        let mut fb = self.chrome.take_screen_base(w as u32, h as u32);
        for p in &self.placed {
            let view = self.leaf_view(p);
            self.chrome.draw_leaf(&mut fb, p.frame.x, p.frame.y, &view);
            if p.focused {
                self.chrome
                    .draw_focus_outline(&mut fb, p.frame.x, p.frame.y, p.frame.w, p.frame.h);
            }
        }

        if self.chrome_size != (w, h) {
            self.chrome_buf =
                MemoryRenderBuffer::new(Fourcc::Argb8888, (w, h), 1, Transform::Normal, None);
            self.chrome_size = (w, h);
        }
        let full: Rectangle<i32, BufferCoords> = Rectangle::from_size((w, h).into());
        let chrome = &self.chrome;
        self.chrome_buf
            .render()
            .draw(|buf| {
                chrome.present_into_slice(&fb, buf);
                Ok::<_, std::convert::Infallible>(vec![full])
            })
            .expect("present chrome into memory buffer");
        self.chrome.retire_frame(fb);
    }

    /// What a leaf's chrome shows: accent, title, minimized state. Icons
    /// arrive in M8 (`TitleInfo.icon` stays `None` until then).
    fn leaf_view(&self, p: &Placement) -> LeafView {
        let titlebar = p.client.and_then(|c| self.managed.get(c)).map(|window| {
            let title: std::rc::Rc<str> = window
                .toplevel()
                .map(|t| {
                    with_states(t.wl_surface(), |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .and_then(|d| d.lock().ok().and_then(|d| d.title.clone()))
                            .unwrap_or_default()
                    })
                })
                .unwrap_or_default()
                .into();
            TitleInfo {
                label: title.chars().next().map_or('?', |c| c.to_ascii_uppercase()),
                icon: None,
                title,
            }
        });
        LeafView {
            w: p.frame.w,
            h: p.frame.h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: self
                .state
                .tree
                .leaf(p.leaf)
                .map_or(theme::FALLBACK_ACCENT_INDEX, |l| l.color),
            titlebar,
            minimized: self.state.tree.leaf(p.leaf).is_some_and(|l| l.minimized),
            buttons: false,
        }
    }
}
