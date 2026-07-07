//! Window classification and the float/dock lifecycles. A toplevel is
//! classified on its first buffer commit (Wayland clients set app_id /
//! parent / size hints after role creation, so classifying at
//! `new_toplevel` time would misfile nearly everything).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::desktop::Window;
use smithay::utils::Transform;

use super::Comp;
use crate::render::{LeafView, TitleInfo};
use crate::shell::{DockData, FloatData, Kind};
use crate::theme;
use crate::tree::Win;
use crate::widgets::label_from_class;

impl Comp {
    /// First commit with a buffer: decide what this window is and manage
    /// it accordingly.
    pub fn classify_and_manage(&mut self, window: Window) {
        if self.matches_dock(&window) && self.managed.dock().is_none() {
            self.manage_dock(window);
        } else if crate::shell::toplevel_parent(&window).is_some()
            || crate::shell::toplevel_fixed_size(&window)
        {
            self.manage_float(window);
        } else {
            let surface = window.toplevel().map(|t| t.wl_surface().clone());
            let win = self.managed.insert(window, Kind::Tiled);
            self.state.pin_client(win);
            if let Some(surface) = surface {
                if let Some(idx) = self.pending_fullscreen.iter().position(|s| *s == surface) {
                    self.pending_fullscreen.swap_remove(idx);
                    self.fullscreen = Some(win);
                }
            }
            self.arrange();
        }
    }

    /// Whether `window` is the dock: its app_id equals the dock identity
    /// (`SPLITWM_DOCK_TITLE`, default `theme::DOCK_TITLE`), falling back to
    /// the title only when it sets no app_id at all — a title is
    /// client-controlled free text, and matching it for classed windows
    /// would let any browser tab titled "cozyui" get yanked out of tiling.
    pub fn matches_dock(&self, window: &Window) -> bool {
        let identity =
            std::env::var("SPLITWM_DOCK_TITLE").unwrap_or_else(|_| theme::DOCK_TITLE.to_string());
        let app_id = crate::shell::toplevel_app_id(window);
        if !app_id.is_empty() {
            return app_id.eq_ignore_ascii_case(&identity);
        }
        crate::shell::toplevel_title(window).as_ref() == identity
    }

    /// Float `window`: show it at its requested size, centered over its
    /// parent's split frame when that parent is a tiled client currently
    /// on screen, otherwise centered in the workarea. Its chrome frame
    /// (split border + titlebar, no control buttons) renders just below
    /// it; dragging the frame moves the pair. It takes focus immediately
    /// (a dialog exists to be answered).
    pub fn manage_float(&mut self, window: Window) {
        let parent =
            crate::shell::toplevel_parent(&window).and_then(|s| self.managed.win_for_surface(&s));
        let size = window.geometry().size;
        let (w, h) = (size.w.max(1), size.h.max(1));

        let wa = self.layout_area();
        // Center over the parent's frame when we know it, else the workarea.
        let around = parent
            .and_then(|p| self.state.tree.find_leaf_for_client(p))
            .and_then(|l| self.prev_frame_rect.get(&l).copied())
            .unwrap_or(wa);
        let x = (around.x + (around.w - w) / 2).clamp(wa.x, (wa.x + wa.w - w).max(wa.x));
        let y = (around.y + (around.h - h) / 2).clamp(wa.y, (wa.y + wa.h - h).max(wa.y));

        // The dialog inherits its transient parent's split accent so the
        // chrome visibly ties them together.
        let accent = parent
            .and_then(|p| self.state.tree.find_leaf_for_client(p))
            .map_or(theme::FALLBACK_ACCENT_INDEX, |l| {
                crate::widgets::leaf_color_index(&self.state.tree, l)
            });

        let data = FloatData {
            parent,
            x,
            y,
            w,
            h,
            accent,
            frame_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            frame_dirty: true,
        };
        let win = self.managed.insert(window, Kind::Float(data));
        self.float_stack.insert(0, win);
        self.focus_float(win);
    }

    /// Pin `window` as the borderless sidebar parked past the right end of
    /// the scrolling canvas, revealed by scrolling all the way right. It
    /// never enters the split tree/taskbar — no chrome, no focus cycling —
    /// and normal tiled columns never lay out under it. Its size is
    /// whatever it asked for at first commit, kept fixed for the session.
    pub fn manage_dock(&mut self, window: Window) {
        let w = window.geometry().size.w.max(1);
        self.managed.insert(window, Kind::Dock(DockData { w }));
        self.arrange();
    }

    /// The extra scroll room the docked sidebar needs (zero when nothing
    /// is docked): its width minus the strip already tucked under the
    /// canvas edge.
    pub fn dock_extra(&self) -> i32 {
        self.managed.dock().map_or(0, |(_, _, d)| d.w - d.overlap())
    }

    /// The dock's pinned screen geometry: parked at the right end of the
    /// tiling canvas, tucked `overlap` px under it, shifted by the current
    /// scroll like any other leaf. Full monitor height (it overlaps the
    /// taskbar strip in its column).
    pub fn dock_geometry(&self, d: DockData) -> crate::tree::Rect {
        let wa = self.layout_area();
        let size = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| self.backend.window_size());
        let canvas_w = self.state.canvas_w(wa);
        crate::tree::Rect {
            x: wa.x + canvas_w - d.overlap() - self.state.scroll_x(),
            y: 0,
            w: d.w.max(1),
            h: size.h.max(1),
        }
    }

    /// Give a float the keyboard and raise it to the top of the float
    /// stack.
    pub fn focus_float(&mut self, win: Win) {
        if self.managed.float(win).is_none() {
            return;
        }
        self.float_stack.retain(|&w| w != win);
        self.float_stack.insert(0, win);
        self.focused_float = Some(win);
        self.refocus();
    }

    /// Reads of the focused float re-validate against the store, so a
    /// dangling record is never handed out.
    pub fn focused_float(&self) -> Option<Win> {
        self.focused_float
            .filter(|&w| self.managed.float(w).is_some())
    }

    /// The window holding the keyboard outside the split tree: a focused
    /// float, or the dock after a click on it.
    pub fn keyboard_override(&self) -> Option<Win> {
        self.focused_float.filter(|&w| {
            matches!(
                self.managed.kind_of(w),
                Some(crate::shell::Kind::Float(_) | crate::shell::Kind::Dock(_))
            )
        })
    }

    /// Hand the keyboard to a non-tiled window (dock click).
    pub fn focus_override(&mut self, win: Win) {
        self.focused_float = Some(win);
        self.refocus();
    }

    pub fn clear_focused_float(&mut self) {
        self.focused_float = None;
    }

    /// A float went away: drop its records and hand focus back to its
    /// parent's split (when it had one) or the focused split.
    pub fn forget_float(&mut self, win: Win) {
        let parent = self.managed.remove(win).and_then(|m| match m.kind {
            Kind::Float(f) => f.parent,
            _ => None,
        });
        self.float_stack.retain(|&w| w != win);
        if self.focused_float == Some(win) {
            self.focused_float = None;
            if let Some(leaf) = parent.and_then(|p| self.state.tree.find_leaf_for_client(p)) {
                self.state.focus_leaf(leaf);
            }
        }
        self.arrange();
    }

    /// Move a float (drag): pure element repositioning, no repaint.
    pub fn move_float(&mut self, win: Win, x: i32, y: i32) {
        if let Some((_, f)) = self.managed.float_mut(win) {
            f.x = x;
            f.y = y;
        }
    }

    /// Repaint a float's chrome frame into its own buffer (rare: size,
    /// title, or accent changed — never per frame or per drag step).
    pub fn paint_float_frame(&mut self, win: Win) {
        let Some((window, f)) = self.managed.float(win) else {
            return;
        };
        let title = crate::shell::toplevel_title(window);
        let label = label_from_class(&crate::shell::toplevel_app_id(window));
        let rect = f.frame_rect();
        let accent = f.accent;
        let view = LeafView {
            w: rect.w,
            h: rect.h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: accent,
            titlebar: Some(TitleInfo {
                label,
                icon: None,
                title,
            }),
            minimized: false,
            buttons: false,
        };
        // A scratch frame-sized fb; float paints are rare enough that
        // recycling machinery would outweigh the allocation.
        let mut fb = pixel_graphics::Framebuffer::new(
            rect.w.max(1) as usize,
            rect.h.max(1) as usize,
            pixel_graphics::TRANSPARENT,
        );
        self.chrome.draw_leaf(&mut fb, 0, 0, &view);
        let buf = MemoryRenderBuffer::new(
            Fourcc::Argb8888,
            (rect.w.max(1), rect.h.max(1)),
            1,
            Transform::Normal,
            None,
        );
        {
            let mut ctx_buf = buf.clone();
            let full: smithay::utils::Rectangle<i32, smithay::utils::Buffer> =
                smithay::utils::Rectangle::from_size((rect.w.max(1), rect.h.max(1)).into());
            let chrome = &self.chrome;
            ctx_buf
                .render()
                .draw(|out| {
                    chrome.present_into_slice(&fb, out);
                    Ok::<_, std::convert::Infallible>(vec![full])
                })
                .expect("present float frame");
        }
        if let Some((_, f)) = self.managed.float_mut(win) {
            f.frame_buf = buf;
            f.frame_dirty = false;
        }
    }
}
