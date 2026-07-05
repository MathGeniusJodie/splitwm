//! Other applications' notification windows
//! (`_NET_WM_WINDOW_TYPE_NOTIFICATION`): adopted like any managed window,
//! but pinned outside the split tree/taskbar and stacked at the
//! bottom-right (see `super::place_notifications`).

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, EventMask};

use super::super::clients::WmState;
use super::super::types::{Wm, R};
use crate::tree::Win;

/// A foreign notification window (`_NET_WM_WINDOW_TYPE_NOTIFICATION`) and
/// its last-known size, so the bottom-right pile can be restacked without
/// re-querying geometry.
#[derive(Clone, Copy)]
pub struct ForeignNote {
    pub win: Win,
    pub w: i32,
    pub h: i32,
}

impl Wm {
    /// Whether `win` declares `_NET_WM_WINDOW_TYPE_NOTIFICATION` (the type
    /// property is a preference-ordered list; any entry counts).
    pub(crate) fn is_notification(&self, win: Win) -> bool {
        self.is_window_type(win, self.atoms.net_wm_window_type_notification)
    }

    /// Pin `win` as a notification: never in the split tree or taskbar,
    /// stacked above everything at the bottom-right of the screen (above the
    /// taskbar strip), at whatever size it requested. Newer notifications
    /// stack upward above older ones.
    pub(crate) fn manage_notification(&mut self, win: Win) -> R<()> {
        self.select_and_grab(win, EventMask::STRUCTURE_NOTIFY, false)?;
        if !self.notes.foreign.iter().any(|n| n.win == win) {
            // One geometry query at manage time; size updates thereafter
            // come from the window's own ConfigureRequests.
            let (w, h) = self
                .conn
                .get_geometry(win)
                .ok()
                .and_then(|c| c.reply().ok())
                .map_or((1, 1), |g| (i32::from(g.width), i32::from(g.height)));
            self.notes.foreign.push(ForeignNote { win, w, h });
        }
        self.place_notifications()?;
        self.conn.map_window(win)?;
        // Notifications are mapped managed windows too: record the ICCCM
        // WM_STATE (see `set_wm_state`).
        self.set_wm_state(win, WmState::Normal)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Stop tracking a closed notification and re-stack the survivors.
    pub(crate) fn forget_notification(&mut self, win: Win) -> R<()> {
        self.notes.foreign.retain(|n| n.win != win);
        self.place_notifications()?;
        Ok(())
    }
}
