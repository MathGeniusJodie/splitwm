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
use super::Comp;
use crate::render::{BtnIcon, LeafView, TitleInfo};
use crate::theme;
use crate::tree::Dir;
use crate::widgets::{leaf_meta, BtnKind, Placement};

render_elements! {
    /// Everything one output frame is made of, front-to-back: client
    /// surfaces (with their popups) above, the chrome underlay below.
    pub OutputElement<=GlesRenderer>;
    Window=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Chrome=MemoryRenderBufferRenderElement<GlesRenderer>,
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
            self.chrome
                .draw_leaf(&mut fb, p.target.x, p.target.y, &view);
            if p.focused {
                self.chrome.draw_focus_outline(
                    &mut fb,
                    p.target.x,
                    p.target.y,
                    p.target.w,
                    p.target.h,
                );
            }
        }

        // "+" insert buttons in the gaps and at the canvas edges.
        for (r, _) in &self.widgets.plus_regions {
            crate::render::draw_plus(&mut fb, r.x + r.w / 2, r.y + r.h / 2, r.w);
        }

        // Split-control buttons over each titlebar. A minimized leaf's
        // region is the whole frame (a single restore button); draw_leaf's
        // winmin art already shows it, so no glyph is drawn on top.
        for (r, leaf, kind) in &self.widgets.btn_regions {
            let Some(p) = self.placed.iter().find(|p| p.leaf == *leaf) else {
                continue;
            };
            let meta = leaf_meta(
                &self.state.tree,
                self.parents.get(leaf).copied(),
                *leaf,
                p.target,
            );
            if meta.minimized {
                continue;
            }
            let (icon, disabled) = match kind {
                // A V-branch parent means this leaf collapses to a row
                // (short/wide) when minimized, so its button previews that
                // with the horizontal glyph.
                BtnKind::Minimize => (
                    if meta.parent_dir == Some(Dir::V) {
                        BtnIcon::MinimizeH
                    } else {
                        BtnIcon::Minimize
                    },
                    meta.parent_dir.is_none(),
                ),
                BtnKind::Split => (
                    if meta.wider {
                        BtnIcon::VSplit
                    } else {
                        BtnIcon::HSplit
                    },
                    !meta.can_split,
                ),
                BtnKind::Close => (BtnIcon::Close, meta.parent_dir.is_none()),
            };
            self.chrome.draw_button(
                &mut fb,
                r.x + r.w / 2,
                r.y + r.h / 2,
                icon,
                disabled,
                crate::widgets::leaf_color_index(&self.state.tree, *leaf),
            );
        }

        // Bottom bar: one tile per managed window; split-visible windows
        // get an accent highlight box, and every tile carries a corner
        // close badge. Icons arrive in M8; the class-initial glyph stands
        // in meanwhile.
        for t in &self.widgets.taskbar_regions {
            let label = self
                .managed
                .get(t.win)
                .map_or('?', |w| {
                    crate::widgets::label_from_class(&crate::shell::toplevel_app_id(w))
                });
            self.chrome
                .draw_taskbar_item(&mut fb, t.rect, None, label, t.accent, t.in_split);
            crate::render::draw_close_badge(&mut fb, t.close.x, t.close.y, t.close.w);
        }
        if let Some(sep) = self.widgets.taskbar_sep {
            crate::render::draw_taskbar_sep(&mut fb, sep);
        }
        for &(r, i) in &self.widgets.quick_regions {
            let Some(q) = self.quick.get(i) else {
                continue;
            };
            self.chrome.draw_taskbar_item(
                &mut fb,
                r,
                q.icon.as_deref(),
                q.label,
                theme::palette_color::CREAM,
                false,
            );
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
        let titlebar = p
            .active_client
            .and_then(|c| self.managed.get(c))
            .map(|window| TitleInfo {
                label: crate::widgets::label_from_class(&crate::shell::toplevel_app_id(window)),
                icon: None,
                title: crate::shell::toplevel_title(window),
            });
        LeafView {
            w: p.target.w,
            h: p.target.h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: crate::widgets::leaf_color_index(&self.state.tree, p.leaf),
            titlebar,
            minimized: self.state.tree.leaf(p.leaf).is_some_and(|l| l.minimized),
            buttons: true,
        }
    }
}
