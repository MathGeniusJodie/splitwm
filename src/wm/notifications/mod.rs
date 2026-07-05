//! Everything under `_NET_WM_WINDOW_TYPE_NOTIFICATION`/`org.freedesktop.
//! Notifications`: other apps' notification windows (`foreign`) and the
//! speech-bubble popups splitwm draws for notifications it serves itself as
//! the session daemon (`popups`, backed by the D-Bus thread in
//! `crate::notify`). Both kinds live outside `clients`/the split tree/
//! taskbar and share one bottom-right stack (oldest at the bottom), so the
//! stacking/placement that spans both piles lives here rather than in
//! either submodule.

mod foreign;
mod popups;

pub use foreign::ForeignNote;
pub use popups::NotePopup;

use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode};

use super::types::{Wm, R};

/// Foreign notification windows and our own served-notification popups.
pub struct NoteState {
    /// Notification windows (`_NET_WM_WINDOW_TYPE_NOTIFICATION`), in mapping
    /// order. Like the dock window, they live outside `clients`/the split
    /// tree/`bar_order`: no chrome, no taskbar entry, no focus cycling.
    /// They stack above everything at the bottom-right of the screen
    /// (see `Wm::place_notifications`), at whatever size they requested —
    /// tracked here (updated on ConfigureRequest) so restacking the pile
    /// doesn't cost a `GetGeometry` round trip per window.
    pub foreign: Vec<ForeignNote>,
    /// Speech-bubble popups for notifications *we* serve as the session's
    /// `org.freedesktop.Notifications` daemon (see `crate::notify` and
    /// `Wm::on_note_ping`). Own override-redirect windows, drawn by the
    /// renderer, stacked bottom-right above the `foreign` pile.
    pub popups: Vec<NotePopup>,
    /// Incoming notification events from the daemon thread.
    pub rx: std::sync::mpsc::Receiver<crate::notify::NoteMsg>,
    /// `(id, close reason)` of popups the WM closed itself — user click
    /// (`CloseReason::Dismissed`) or popup-cap eviction
    /// (`CloseReason::Undefined`) — reported back to the daemon thread so
    /// it emits the matching `NotificationClosed` signal.
    pub dismiss: std::sync::mpsc::Sender<(u32, crate::notify::CloseReason)>,
}

impl Wm {
    /// Position every notification: stacked bottom-up starting just above
    /// the taskbar (oldest at the bottom; see `stack_note_pile` for the
    /// shared geometry). Our own served-notification popups continue the
    /// same pile upward (see `place_note_popups`), so both kinds share one
    /// stack instead of overlapping — which is also why a foreign-pile
    /// change must re-place the popups.
    pub(crate) fn place_notifications(&self) -> R<()> {
        let wa = self.wa();
        let bottom = wa.y + wa.h - Self::taskbar_h();
        self.stack_note_pile(self.notes.foreign.iter().map(|n| (n.win, n.w, n.h)), bottom)?;
        self.place_note_popups()
    }

    /// Restack every notification to the top, preserving their relative
    /// order — arrange()/focus() raise tiled clients, so notifications must
    /// be re-raised afterwards to stay on top of everything.
    pub(crate) fn raise_notifications(&self) -> R<()> {
        for n in &self.notes.foreign {
            self.conn.configure_window(
                n.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        for p in &self.notes.popups {
            self.conn.configure_window(
                p.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }
}
