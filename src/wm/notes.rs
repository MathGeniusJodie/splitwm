//! Speech-bubble popups for the notifications splitwm itself serves as the
//! session's `org.freedesktop.Notifications` daemon (see `crate::notify`).
//! One override-redirect window per note, drawn by the renderer and shaped
//! to the bubble's outline, stacked bottom-right above the taskbar.

use pixel_graphics::{Framebuffer, TRANSPARENT};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ClipOrdering, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, Rectangle,
    StackMode, Window, WindowClass,
};

use super::types::{NotePopup, Wm, R};
use crate::notify::{Note, NoteMsg};
use crate::theme;

/// Cap on live popup windows; aliased directly to `notify::MAX_NOTES` (the
/// daemon thread's own outstanding-notification cap) so the two can't drift
/// — the daemon evicts its oldest tracked id at the same threshold, so this
/// is mostly a backstop for popups that reached us before an eviction
/// round-trips.
const MAX_NOTE_POPUPS: usize = crate::notify::MAX_NOTES;

impl Wm {
    /// The daemon thread's ClientMessage wakeup: drain the channel and
    /// bring the popup pile up to date.
    pub(crate) fn on_note_ping(&mut self) -> R<()> {
        let mut changed = false;
        loop {
            match self.notes.rx.try_recv() {
                Ok(NoteMsg::Show(note)) => {
                    self.show_note(note)?;
                    changed = true;
                }
                Ok(NoteMsg::Close(id)) => {
                    if let Some(i) = self.notes.popups.iter().position(|p| p.note.id == id) {
                        self.conn.destroy_window(self.notes.popups.remove(i).win)?;
                        changed = true;
                    }
                }
                Err(_) => break,
            }
        }
        if changed {
            self.place_note_popups()?;
            self.conn.flush()?;
        }
        Ok(())
    }

    /// Show a note, reusing its popup when the id already exists (a sender
    /// updating via `replaces_id`).
    fn show_note(&mut self, note: Note) -> R<()> {
        let fb = self.renderer.draw_note(&note.summary, &note.body);
        let (w, h) = (fb.width as i32, fb.height as i32);
        let win = match self.notes.popups.iter_mut().find(|p| p.note.id == note.id) {
            Some(p) => {
                p.note = note;
                (p.w, p.h) = (w, h);
                self.conn.configure_window(
                    p.win,
                    &ConfigureWindowAux::new().width(w as u32).height(h as u32),
                )?;
                p.win
            }
            None => {
                let win = self.conn.generate_id()?;
                self.conn.create_window(
                    self.depth,
                    win,
                    self.root,
                    0,
                    0,
                    w as u16,
                    h as u16,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    0, // CopyFromParent
                    &CreateWindowAux::new()
                        .override_redirect(1)
                        .cursor(self.cursors.hand)
                        .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
                )?;
                self.notes.popups.push(NotePopup { win, note, w, h });
                self.conn.map_window(win)?;
                // Cap live popups: the daemon caps outstanding notifications
                // too (see `notify::MAX_NOTES`), but a burst can still land
                // more `Show`s here before an eviction round-trips back —
                // drop our own oldest rather than let the pile grow. Report
                // the eviction back to the daemon like a click-dismissal:
                // otherwise it keeps the id outstanding forever (a
                // never-expiring note has no other way out) and the sender
                // still believes its notification is on screen.
                if self.notes.popups.len() > MAX_NOTE_POPUPS {
                    let evicted = self.notes.popups.remove(0);
                    let _ = self.notes.dismiss.send(evicted.note.id);
                    self.conn.destroy_window(evicted.win)?;
                }
                win
            }
        };
        self.shape_to_opaque(win, &fb)?;
        self.paint_note(win, &fb)?;
        Ok(())
    }

    /// Re-render and blit one popup (initial paint and Expose).
    pub(crate) fn paint_note_win(&mut self, win: Window) -> R<()> {
        let Some(p) = self.notes.popups.iter().find(|p| p.win == win) else {
            return Ok(());
        };
        let fb = self.renderer.draw_note(&p.note.summary, &p.note.body);
        self.paint_note(win, &fb)
    }

    fn paint_note(&mut self, win: Window, fb: &Framebuffer) -> R<()> {
        self.blit_fb(win, fb)
    }

    /// Shape a window to a framebuffer's opaque pixels: one 1-px-tall
    /// rectangle per opaque row span, so transparent areas (bubble corners,
    /// the tail's surroundings, float-frame border corners) are
    /// click-through and show whatever is beneath.
    pub(crate) fn shape_to_opaque(&self, win: Window, fb: &Framebuffer) -> R<()> {
        let mut rects = Vec::new();
        for y in 0..fb.height {
            let row = fb.row(y);
            let mut x = 0;
            while x < fb.width {
                if row[x] == TRANSPARENT {
                    x += 1;
                    continue;
                }
                let x0 = x;
                while x < fb.width && row[x] != TRANSPARENT {
                    x += 1;
                }
                rects.push(Rectangle {
                    x: x0 as i16,
                    y: y as i16,
                    width: (x - x0) as u16,
                    height: 1,
                });
            }
        }
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::YX_BANDED,
            win,
            0,
            0,
            &rects,
        )?;
        Ok(())
    }

    /// Stack the popups bottom-right, oldest nearest the corner, continuing
    /// upward from wherever the foreign-notification pile ends (see
    /// `place_notifications`) so the two kinds never overlap.
    pub(crate) fn place_note_popups(&self) -> R<()> {
        let wa = self.wa();
        let gap = theme::GAP;
        let foreign_h: i32 = self.notes.foreign.iter().map(|n| gap + n.h).sum();
        let mut bottom = wa.y + wa.h - Self::taskbar_h() - foreign_h;
        for p in &self.notes.popups {
            bottom -= gap + p.h;
            self.conn.configure_window(
                p.win,
                &ConfigureWindowAux::new()
                    .x(wa.x + wa.w - gap - p.w)
                    .y(bottom)
                    .stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    /// Click anywhere on a popup dismisses it; the daemon thread emits the
    /// `NotificationClosed(id, 2 /* by user */)` signal.
    pub(crate) fn dismiss_note(&mut self, win: Window) -> R<bool> {
        let Some(i) = self.notes.popups.iter().position(|p| p.win == win) else {
            return Ok(false);
        };
        let p = self.notes.popups.remove(i);
        let _ = self.notes.dismiss.send(p.note.id);
        self.conn.destroy_window(p.win)?;
        self.place_note_popups()?;
        self.conn.flush()?;
        Ok(true)
    }
}
