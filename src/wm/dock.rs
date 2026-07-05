//! The docked sidebar: a single identified window (matched by `WM_CLASS`,
//! title as a classless fallback) pinned past the right end of the
//! scrolling canvas, revealed by scrolling all the way right. Lives outside
//! `clients`/the split tree/taskbar entirely — no chrome, no taskbar entry,
//! not part of focus cycling — so normal tiled columns never lay out under
//! it.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonIndex, ConfigureWindowAux, ConnectionExt, EventMask, ModMask, StackMode,
};

use super::clients::WmState;
use super::types::{clamp_dim, Wm, WindowKind, R};
use crate::tree::Win;

/// The docked-sidebar identity config and the currently docked window.
pub struct DockState {
    /// The window pinned past the right end of the scrolling canvas, only
    /// revealed by scrolling all the way right (see `DockState::title`),
    /// if one is currently mapped. It lives outside `clients`/the split
    /// tree/`bar_order` entirely: no chrome, no taskbar entry, not part of
    /// focus cycling, and normal tiled columns never lay out under it.
    pub docked: Option<Dock>,
    /// Identity that marks the dock window — matched against either half of
    /// its `WM_CLASS` (`SPLITWM_DOCK_TITLE`, default `theme::DOCK_TITLE`);
    /// also the desktop id used to autostart it.
    pub title: String,
}

/// A docked window and the width captured from its own requested geometry
/// when it was first managed — the width only exists while something is
/// docked, so the pair travels as one value.
#[derive(Clone, Copy)]
pub struct Dock {
    pub win: Win,
    pub w: i32,
}

impl Dock {
    /// `theme::DOCK_OVERLAP` clamped to the dock's own width — an overlap
    /// wider than the dock would otherwise shove its right edge permanently
    /// away from the screen edge (fully tucked is the useful maximum).
    pub fn overlap(self) -> i32 {
        crate::theme::DOCK_OVERLAP.min(self.w)
    }
}

impl Wm {
    /// Whether `win` is the dock: either half of its `WM_CLASS`
    /// ("instance\0class\0") equals `DockState::title`, falling back to the
    /// window title only when it sets no `WM_CLASS` at all (the stock dock
    /// app doesn't). Class is preferred because a title is client-controlled
    /// free text that changes at runtime — matching on title alone would let
    /// any window titling itself "cozyui" (a browser tab, say) get yanked
    /// out of tiling and pinned as the dock; a window that *does* declare a
    /// class must match on that alone.
    pub(crate) fn matches_dock(&self, win: Win) -> bool {
        let parts = self.wm_class_parts(win);
        if !parts.is_empty() {
            return parts.iter().any(|part| part.as_slice() == self.dock.title.as_bytes());
        }
        self.client_title(win).as_ref() == self.dock.title
    }

    /// Pin `win` (`Wm::dock_title`) as a borderless window parked past
    /// the right end of the scrolling canvas, revealed by scrolling all the
    /// way right (see `place_dock`/`State::dock_extra`): it never enters
    /// `clients`/the split tree/taskbar, so it gets none of their chrome or
    /// focus cycling, and normal tiled columns never lay out under it. Its
    /// size is whatever it asked for at creation time, kept fixed for the
    /// rest of the session.
    pub(crate) fn manage_dock(&mut self, win: Win) -> R<()> {
        let width = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(240, |g| i32::from(g.width));
        self.dock.docked = Some(Dock {
            win,
            w: width.max(1),
        });
        self.register_kind(win, WindowKind::Dock);

        self.select_and_grab(win, EventMask::STRUCTURE_NOTIFY, true)?;
        // The dock is a mapped managed client too: give it the ICCCM
        // WM_STATE some toolkits misbehave without (see `set_wm_state`).
        self.set_wm_state(win, WmState::Normal)?;

        // arrange() calls place_dock() against the freshly computed canvas,
        // so no separate initial placement is needed here.
        self.arrange()?;
        self.conn.flush()?;
        Ok(())
    }

    /// A managed client (re)set its `WM_CLASS` or title: if it now matches
    /// the dock identity (and nothing is docked yet), pull it out of tiling
    /// and dock it — a toolkit that sets its identifying property only
    /// after mapping would otherwise leave the dock tiled as an ordinary
    /// window forever.
    pub(crate) fn on_dock_identity_change(&mut self, win: Win, changed_atom: u32) -> R<()> {
        if self.dock.docked.is_some() {
            return Ok(());
        }
        let Some(client) = self.clients.get(&win) else {
            return Ok(());
        };
        // Title changes are frequent (terminals retitle per prompt) and can
        // only affect dock identity for windows with no WM_CLASS at all
        // (the title is matches_dock's last-resort fallback). Don't pay the
        // property round trips for a title change on a classed window.
        let class_changed = changed_atom == u32::from(AtomEnum::WM_CLASS);
        if !class_changed && client.class.as_ref() != "?" {
            return Ok(());
        }
        if !self.matches_dock(win) {
            return Ok(());
        }
        self.clients.remove(&win);
        self.unregister_kind(win);
        self.bar_order.retain(|&w| w != win);
        self.forget_ignored_unmaps(win);
        self.clear_fullscreen_if(win);
        self.state.unpin_client(win);
        self.update_client_list()?;
        // Drop the click-to-focus grab `manage` installed before
        // `manage_dock` re-issues the identical passive grab. Re-grabbing
        // one's own combination is actually allowed (BadAccess is only for
        // *another* client's grab) — this just keeps the grab's bookkeeping
        // an explicit install/remove pair rather than leaning on that quirk.
        self.conn
            .ungrab_button(ButtonIndex::M1, win, ModMask::ANY)?;
        self.manage_dock(win)
    }

    /// The extra scroll room the docked sidebar needs (zero when nothing is
    /// docked): its width minus the strip already tucked under the canvas
    /// edge. One of `State::update_canvas`'s inputs.
    pub(crate) fn dock_extra(&self) -> i32 {
        self.dock.docked.map_or(0, |d| d.w - d.overlap())
    }

    /// The dock's pinned screen geometry `(x, y, w, h)`: parked at the right
    /// end of the tiling canvas, tucked `Dock::overlap` px under it (the
    /// canvas edge overlaps the dock, not the other way round: the dock
    /// stacks just above the underlay, below every tiled client), shifted
    /// by the current scroll like any other leaf. It's (mostly) off-screen
    /// at `scroll_x = 0` and only slides fully into view once the canvas is
    /// scrolled all the way right (`State::dock_extra` extends `max_scroll`
    /// to make that reachable). Full monitor height, not `la()`'s (which is
    /// trimmed for the bottom taskbar) — the dock spans the entire screen,
    /// overlapping the taskbar strip in its column. The single formula
    /// behind `place_dock` (configuring) and `tracked_geometry` (answering
    /// denied ConfigureRequests).
    pub(crate) fn dock_geometry(&self, d: Dock) -> (i32, i32, i32, i32) {
        let wa = self.la();
        let full = self.wa();
        let canvas_w = self.state.canvas_w(wa);
        let x = wa.x + canvas_w - d.overlap() - self.state.scroll_x();
        (x, full.y, d.w.max(1), full.h.max(1))
    }

    /// The dock went away: drop its record and re-tile now that the scroll
    /// headroom it needed is gone.
    pub(crate) fn forget_dock(&mut self, win: Win) -> R<()> {
        self.dock.docked = None;
        self.unregister_kind(win);
        self.clamp_scroll();
        self.arrange()
    }

    pub(crate) fn place_dock(&self) -> R<()> {
        let Some(d) = self.dock.docked else {
            return Ok(());
        };
        let (x, y, w, h) = self.dock_geometry(d);
        self.conn.configure_window(
            d.win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(clamp_dim(w))
                .height(clamp_dim(h))
                .border_width(0)
                .sibling(self.underlay)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(d.win)?;
        Ok(())
    }
}
