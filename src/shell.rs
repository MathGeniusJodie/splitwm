//! The managed-window store: the bridge between the pure layout core
//! (which tracks opaque `Win` ids) and smithay's `Window` objects.
//!
//! `Win`s are allocated here and only here, on manage; an id in the layout
//! always resolves while the window is managed, and insertion order is the
//! taskbar order (matching master's `managed` store).

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::tree::{Rect, Win};
use crate::theme;

#[derive(Default)]
pub struct Managed {
    /// Monotonic id source; `Win`s are never reused within a session, so a
    /// stale id from a closed window can never alias a live one.
    next: Win,
    /// Insertion-ordered (taskbar order).
    entries: Vec<(Win, Window)>,
}

impl Managed {
    pub fn insert(&mut self, window: Window) -> Win {
        self.next += 1;
        self.entries.push((self.next, window));
        self.next
    }

    pub fn remove(&mut self, win: Win) -> Option<Window> {
        let idx = self.entries.iter().position(|(w, _)| *w == win)?;
        Some(self.entries.remove(idx).1)
    }

    pub fn get(&self, win: Win) -> Option<&Window> {
        self.entries
            .iter()
            .find_map(|(w, window)| (*w == win).then_some(window))
    }

    /// The `Win` whose toplevel's root surface is `surface`.
    pub fn win_for_surface(&self, surface: &WlSurface) -> Option<Win> {
        self.entries.iter().find_map(|(w, window)| {
            window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == surface)
                .then_some(*w)
        })
    }

    pub fn win_for_window(&self, window: &Window) -> Option<Win> {
        self.entries
            .iter()
            .find_map(|(w, wd)| (wd == window).then_some(*w))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (Win, &Window)> {
        self.entries.iter().map(|(w, window)| (*w, window))
    }
}

/// Client-area rect inside a leaf's chrome frame: below the titlebar,
/// inside the side/bottom borders. `min` lets a client's size floor
/// overhang the frame rather than be clipped (matching master).
pub fn client_rect_in_frame(r: Rect, (min_w, min_h): (i32, i32)) -> (i32, i32, i32, i32) {
    let (bw, tb) = (theme::BORDER_LEFT, theme::tb_h());
    (
        r.x + bw,
        r.y + tb,
        (r.w - 2 * bw).max(min_w).max(1),
        (r.h - tb - theme::BORDER_BOTTOM).max(min_h).max(1),
    )
}
