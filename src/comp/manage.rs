//! Window classification and the float/dock lifecycles. A toplevel is
//! classified on its first buffer commit (Wayland clients set app_id /
//! parent / size hints after role creation, so classifying at
//! `new_toplevel` time would misfile nearly everything).

use smithay::desktop::Window;

use super::Comp;
use crate::layout::Win;
use crate::render::{LeafView, TitleInfo};
use crate::shell::{DockData, FloatData, Kind};
use crate::theme;
use crate::widgets::label_from_class;

impl Comp {
    /// First commit with a buffer (Wayland) or map request (X11): decide
    /// what this window is and manage it accordingly. `fullscreen` is the
    /// pre-map fullscreen request carried over from the pending record
    /// (master's pre-map `_NET_WM_STATE` behavior), honored only for a
    /// tiled window.
    pub fn classify_and_manage(&mut self, window: Window, fullscreen: bool) {
        if self.matches_dock(&window) && self.managed.dock().is_none() {
            self.manage_dock(window);
        } else if self.wants_float(&window) {
            self.manage_float(window);
        } else {
            let class = crate::shell::toplevel_app_id(&window);
            // Both backends state their preferred size before this point —
            // the xdg initial configure names no size, so the first buffer
            // is the client's own choice; an X11 window carries its map
            // geometry — so the column opens at that width plus the frame
            // borders rather than a default.
            let want_w = match window.geometry().size.w {
                w if w > 0 => Some(w + 2 * theme::BORDER_LEFT),
                _ => None,
            };
            let win = self.managed.insert(window, Kind::Tiled);
            let slot = self.assign_icon_slot(&class);
            if let Some(entry) = self.managed.entry_mut(win) {
                entry.icon_slot = slot;
            }
            self.spawn_icon_fetch(win, class);
            // Into the focused split if it's an empty placeholder, else a
            // fresh column right of the focused one, as the gap `+`
            // button would (see `State::place_new_window`). Animate the
            // new column sliding in and scroll it into view.
            let wa = self.layout_area();
            self.state.place_new_window(wa, win, want_w);
            if fullscreen {
                self.windows.fullscreen = Some(win);
            }
            self.view.animate = true;
            self.commit_layout();
        }
    }

    /// Whether `window` is the dock: its app_id equals the dock identity
    /// (`SPLITWM_DOCK_TITLE`, default `theme::DOCK_TITLE`), falling back to
    /// the title only when it sets no app_id at all — a title is
    /// client-controlled free text, and matching it for classed windows
    /// would let any browser tab titled "cozyui" get yanked out of tiling.
    pub fn matches_dock(&self, window: &Window) -> bool {
        let identity = theme::dock_identity();
        let app_id = crate::shell::toplevel_app_id(window);
        if !app_id.is_empty() {
            return app_id.eq_ignore_ascii_case(&identity);
        }
        crate::shell::toplevel_title(window).as_ref() == identity
    }

    /// Whether `window` should float instead of tiling: a transient (xdg
    /// parent / `WM_TRANSIENT_FOR`), a declared X11 dialog, or a
    /// fixed-size window (min == max — it can't be resized, so stretching
    /// it into a split only produces gravel).
    fn wants_float(&self, window: &Window) -> bool {
        if self.parent_win_of(window).is_some() {
            return true;
        }
        if let Some(x11) = window.x11_surface() {
            if x11.is_transient_for().is_some()
                || x11.window_type() == Some(smithay::xwayland::xwm::WmWindowType::Dialog)
            {
                return true;
            }
            return x11.size_hints().is_some_and(|h| {
                matches!((h.min_size, h.max_size),
                    (Some((minw, minh)), Some((maxw, maxh)))
                        if minw == maxw && minh == maxh && minw > 0 && minh > 0)
            });
        }
        crate::shell::toplevel_fixed_size(window)
    }

    /// The managed `Win` this window declares as its parent, either
    /// backend.
    fn parent_win_of(&self, window: &Window) -> Option<Win> {
        if let Some(surface) = crate::shell::toplevel_parent(window) {
            return self.managed.win_for_surface(&surface);
        }
        let parent_id = window.x11_surface()?.is_transient_for()?;
        self.managed.entries_windows().find_map(|(w, wd)| {
            wd.x11_surface()
                .is_some_and(|s| s.window_id() == parent_id)
                .then_some(w)
        })
    }

    /// A managed tiled window is gone — both protocol destroy paths
    /// (Wayland and XWayland) land here. The dying window usually takes
    /// its split with it (a badge-closed one leaves a placeholder, see
    /// `unpin_client`); animate the layout settling — stacked neighbours
    /// reclaiming the height, or the later columns sliding over a removed
    /// one. arrange (via `commit_layout`) refocuses, so focus never rests
    /// on a dead client.
    pub fn unmanage_tiled(&mut self, win: Win) {
        if let Some(m) = self.managed.remove(win) {
            self.space.unmap_elem(&m.window);
        }
        self.view.animate = self.state.unpin_client(win);
        self.commit_layout();
    }

    /// Politely close a managed window, whichever backend it speaks.
    pub fn close_client(&self, win: Win) {
        if let Some(window) = self.managed.get(win) {
            crate::shell::close_window(window);
        }
    }

    /// Float `window`: show it at its requested size, centered over its
    /// parent's split frame when that parent is a tiled client currently
    /// on screen, otherwise centered in the workarea. Its chrome frame
    /// (split border + titlebar, no control buttons) renders just below
    /// it; dragging the frame moves the pair. It takes focus immediately
    /// (a dialog exists to be answered).
    pub fn manage_float(&mut self, window: Window) {
        let parent = self.parent_win_of(&window);
        let size = window.geometry().size;
        let (w, h) = (size.w.max(1), size.h.max(1));

        let wa = self.layout_area();
        // Center over the parent's frame when we know it, else the workarea.
        let around = parent
            .and_then(|p| self.state.layout.find_leaf_for_client(p))
            .and_then(|l| self.view.prev_frame_rect.get(&l).copied())
            .unwrap_or(wa);
        let x = (around.x + (around.w - w) / 2).clamp(wa.x, (wa.x + wa.w - w).max(wa.x));
        let y = (around.y + (around.h - h) / 2).clamp(wa.y, (wa.y + wa.h - h).max(wa.y));

        // The dialog inherits its transient parent's split accent so the
        // chrome visibly ties them together.
        let accent = parent
            .and_then(|p| self.state.layout.find_leaf_for_client(p))
            .map_or(theme::FALLBACK_ACCENT_INDEX, |l| {
                crate::widgets::leaf_color_index(&self.state.layout, l)
            });

        let data = FloatData {
            parent,
            x,
            y,
            w,
            h,
            accent,
            frame: crate::shell::FrameTex::Stale(None),
            frame_id: smithay::backend::renderer::element::Id::new(),
        };
        let class = crate::shell::toplevel_app_id(&window);
        let win = self.managed.insert(window, Kind::Float(data));
        self.windows.float_stack.insert(0, win);
        self.spawn_icon_fetch(win, class);
        self.focus_float(win);
    }

    /// Pin `window` as the borderless sidebar parked past the right end of
    /// the scrolling canvas, revealed by scrolling all the way right. It
    /// never enters the layout/taskbar — no chrome, no focus cycling —
    /// and normal tiled columns never lay out under it. Its size is
    /// whatever it asked for at first commit, kept fixed for the session.
    pub fn manage_dock(&mut self, window: Window) {
        let w = window.geometry().size.w.max(1);
        self.managed.insert(window, Kind::Dock(DockData { w }));
        self.arrange();
    }

    /// The extra scroll room the docked sidebar needs (zero when nothing
    /// is docked): its width minus the strip already tucked under the
    /// canvas edge. The dock is either a managed window (XWayland cozyui)
    /// or a native layer surface (see `layer_dock_extra`), whichever is
    /// present.
    pub fn dock_extra(&self) -> i32 {
        if let Some((_, _, d)) = self.managed.dock() {
            return d.w - d.overlap();
        }
        self.layer_dock_extra()
    }

    /// The dock's pinned screen geometry: parked at the right end of the
    /// tiling canvas, tucked `overlap` px under it, shifted by the current
    /// scroll like any other leaf. Full monitor height (it overlaps the
    /// taskbar strip in its column).
    pub fn dock_geometry(&self, d: DockData) -> crate::layout::Rect {
        let wa = self.layout_area();
        let size = self.output_size();
        let canvas_w = self.state.canvas_w(wa);
        crate::layout::Rect {
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
        self.windows.float_stack.retain(|&w| w != win);
        self.windows.float_stack.insert(0, win);
        self.windows.focused_float = Some(win);
        self.refocus();
    }

    /// Reads of the focused float re-validate against the store, so a
    /// dangling record is never handed out.
    pub fn focused_float(&self) -> Option<Win> {
        self.windows
            .focused_float
            .filter(|&w| self.managed.float(w).is_some())
    }

    /// The fullscreen tiled client, if it is still managed. Like
    /// `focused_float`, reads re-validate against the store rather than
    /// relying on every destroy path clearing the record — a `Win` is
    /// never reused, so a dead one can't alias a later window.
    pub fn fullscreen(&self) -> Option<Win> {
        self.windows
            .fullscreen
            .filter(|&w| self.managed.get(w).is_some())
    }

    /// The window holding the keyboard outside the layout: a focused
    /// float, or the dock after a click on it.
    pub fn keyboard_override(&self) -> Option<Win> {
        self.windows.focused_float.filter(|&w| {
            matches!(
                self.managed.kind_of(w),
                Some(crate::shell::Kind::Float(_) | crate::shell::Kind::Dock(_))
            )
        })
    }

    /// Hand the keyboard to a non-tiled window (dock click).
    pub fn focus_override(&mut self, win: Win) {
        self.windows.focused_float = Some(win);
        self.refocus();
    }

    pub fn clear_focused_float(&mut self) {
        self.windows.focused_float = None;
    }

    /// A float went away: drop its records and hand focus back to its
    /// parent's split (when it had one) or the focused split.
    pub fn forget_float(&mut self, win: Win) {
        let parent = self.managed.remove(win).and_then(|m| match m.kind {
            Kind::Float(f) => f.parent,
            _ => None,
        });
        self.windows.float_stack.retain(|&w| w != win);
        if self.windows.focused_float == Some(win) {
            self.windows.focused_float = None;
            if let Some(leaf) = parent.and_then(|p| self.state.layout.find_leaf_for_client(p)) {
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

    /// Repaint a float's titlebar strip into its own buffer (rare: size or
    /// title changed — never per frame or per drag step). The border around
    /// it is the shared art sliced on the GPU and never repaints.
    pub fn paint_float_frame(&mut self, win: Win) {
        let Some((window, f)) = self.managed.float(win) else {
            return;
        };
        let title = crate::shell::toplevel_title(window);
        let label = label_from_class(&crate::shell::toplevel_app_id(window));
        let icon = self.managed.entry(win).and_then(|m| m.icon.clone());
        let rect = f.frame_rect();
        let view = LeafView {
            w: rect.w,
            h: rect.h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: f.accent,
            titlebar: Some(TitleInfo { label, icon, title }),
            minimized: false,
            buttons: false,
        };
        // A scratch strip-sized fb; float paints are rare enough that
        // recycling machinery would outweigh the allocation. Transparent so
        // the frame's titlebar band shows through around the icon and text.
        let mut fb = pixel_graphics::Framebuffer::new(
            rect.w.max(1) as usize,
            theme::tb_h().max(1) as usize,
            pixel_graphics::TRANSPARENT,
        );
        self.view.chrome.draw_titlebar_strip(&mut fb, &view);
        if let Some((_, f)) = self.managed.float_mut(win) {
            let mut tex = f.frame.take();
            self.view
                .indexed
                .upload(self.backend.renderer(), &mut tex, &fb, false);
            f.frame = match tex {
                Some(tex) => crate::shell::FrameTex::Fresh(tex),
                None => crate::shell::FrameTex::Stale(None),
            };
        }
    }
}
