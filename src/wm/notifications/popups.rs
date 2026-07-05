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

use super::super::types::{Wm, R};
use crate::notify::{Note, NoteMsg};
use crate::theme;
use crate::tree::Rect;

/// Cap on live popup windows; aliased directly to `notify::MAX_NOTES` (the
/// daemon thread's own outstanding-notification cap) so the two can't drift
/// — the daemon evicts its oldest tracked id at the same threshold, so this
/// is mostly a backstop for popups that reached us before an eviction
/// round-trips.
const MAX_NOTE_POPUPS: usize = crate::notify::MAX_NOTES;

/// One on-screen speech-bubble notification popup and the note it shows.
pub struct NotePopup {
    pub win: Window,
    pub note: crate::notify::Note,
    pub w: i32,
    pub h: i32,
}

impl Wm {
    /// The daemon thread's ClientMessage wakeup: drain the channel and
    /// bring the popup pile up to date.
    pub(crate) fn on_note_ping(&mut self) -> R<()> {
        // Contain per-item errors: the channel has no other wakeup, so an
        // early `?` here would strand every message still queued behind the
        // failed one until an unrelated ping. Connection errors resurface at
        // the flush below.
        let mut changed = false;
        loop {
            match self.notes.rx.try_recv() {
                Ok(NoteMsg::Show(note)) => {
                    let id = note.id;
                    match self.show_note(note) {
                        Ok(()) => changed = true,
                        Err(e) => eprintln!("splitwm: failed to show notification {id}: {e}"),
                    }
                }
                Ok(NoteMsg::Close(id)) => {
                    if let Some(i) = self.notes.popups.iter().position(|p| p.note.id == id) {
                        let win = self.notes.popups.remove(i).win;
                        changed = true;
                        if let Err(e) = self.conn.destroy_window(win) {
                            eprintln!("splitwm: failed to close notification {id}: {e}");
                        }
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
                // still believes its notification is on screen. Reported
                // with the "undefined" reason (matching the daemon's own
                // evictions), not as a user dismissal — it wasn't one.
                if self.notes.popups.len() > MAX_NOTE_POPUPS {
                    let evicted = self.notes.popups.remove(0);
                    let _ = self
                        .notes
                        .dismiss
                        .send((evicted.note.id, crate::notify::CloseReason::Undefined));
                    self.conn.destroy_window(evicted.win)?;
                }
                win
            }
        };
        self.shape_to_opaque(win, &fb)?;
        self.blit_fb(win, &fb)?;
        Ok(())
    }

    /// Re-render and blit one popup (initial paint and Expose).
    pub(crate) fn paint_note_win(&mut self, win: Window) -> R<()> {
        let Some(p) = self.notes.popups.iter().find(|p| p.win == win) else {
            return Ok(());
        };
        let fb = self.renderer.draw_note(&p.note.summary, &p.note.body);
        self.blit_fb(win, &fb)
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

    /// Stack a pile of `(window, w, h)` bottom-up from `bottom`,
    /// right-aligned to the screen edge (split-gap margin), each raised to
    /// the top of the stacking order. Returns the pile's new top edge.
    pub(crate) fn stack_note_pile(
        &self,
        items: impl Iterator<Item = (Window, i32, i32)>,
        bottom: i32,
    ) -> R<i32> {
        let items: Vec<(Window, i32, i32)> = items.collect();
        let (positions, new_bottom) = pile_positions(
            items.iter().map(|&(_, w, h)| (w, h)),
            bottom,
            self.wa(),
            theme::GAP,
        );
        for (&(win, ..), (x, y)) in items.iter().zip(positions) {
            self.conn.configure_window(
                win,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(new_bottom)
    }

    /// Stack the popups bottom-right, oldest nearest the corner, continuing
    /// upward from wherever the foreign-notification pile ends (see
    /// `place_notifications`) so the two kinds never overlap.
    pub(crate) fn place_note_popups(&self) -> R<()> {
        let wa = self.wa();
        let gap = theme::GAP;
        let foreign_h: i32 = self.notes.foreign.iter().map(|n| gap + n.h).sum();
        let bottom = wa.y + wa.h - Self::taskbar_h() - foreign_h;
        self.stack_note_pile(self.notes.popups.iter().map(|p| (p.win, p.w, p.h)), bottom)?;
        Ok(())
    }

    /// Click anywhere on a popup dismisses it; the daemon thread emits the
    /// `NotificationClosed(id, 2 /* by user */)` signal.
    pub(crate) fn dismiss_note(&mut self, win: Window) -> R<bool> {
        let Some(i) = self.notes.popups.iter().position(|p| p.win == win) else {
            return Ok(false);
        };
        let p = self.notes.popups.remove(i);
        let _ = self
            .notes
            .dismiss
            .send((p.note.id, crate::notify::CloseReason::Dismissed));
        self.conn.destroy_window(p.win)?;
        self.place_note_popups()?;
        self.conn.flush()?;
        Ok(true)
    }
}

/// Bottom-up, right-aligned (screen-edge minus `gap`) stacked positions for
/// a pile of `(w, h)` boxes, continuing upward from `bottom` in order.
/// Clamped to the top of the workarea: a deep enough pile would otherwise
/// place the overflow at negative y — visible nowhere and (click-to-dismiss
/// being the only dismissal) undismissable; overflowing boxes overlap at
/// the top edge instead. Returns each box's `(x, y)` alongside the pile's
/// new top edge, kept free of `Wm`/X11 so the stacking math is directly
/// testable.
fn pile_positions(
    sizes: impl Iterator<Item = (i32, i32)>,
    mut bottom: i32,
    wa: Rect,
    gap: i32,
) -> (Vec<(i32, i32)>, i32) {
    let mut positions = Vec::new();
    for (w, h) in sizes {
        bottom = (bottom - gap - h).max(wa.y);
        positions.push((wa.x + wa.w - gap - w, bottom));
    }
    (positions, bottom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 800,
    };

    #[test]
    fn stacks_bottom_up_right_aligned_with_gaps() {
        let (positions, new_bottom) =
            pile_positions([(100, 20), (140, 30)].into_iter(), 800, WA, 8);
        assert_eq!(
            positions,
            vec![
                (1280 - 8 - 100, 800 - 8 - 20),
                (1280 - 8 - 140, 800 - 8 - 20 - 8 - 30),
            ]
        );
        assert_eq!(new_bottom, 800 - 8 - 20 - 8 - 30);
    }

    #[test]
    fn a_deep_pile_clamps_at_the_workarea_top_instead_of_going_negative() {
        let sizes = std::iter::repeat_n((100, 50), 100);
        let (positions, new_bottom) = pile_positions(sizes, 800, WA, 8);
        assert_eq!(new_bottom, WA.y, "pile stops climbing at the workarea top");
        assert!(
            positions.iter().all(|&(_, y)| y >= WA.y),
            "no box is placed above the workarea, however deep the pile"
        );
    }

    #[test]
    fn empty_pile_is_a_no_op() {
        let (positions, new_bottom) = pile_positions(std::iter::empty(), 500, WA, 8);
        assert!(positions.is_empty());
        assert_eq!(new_bottom, 500);
    }
}
