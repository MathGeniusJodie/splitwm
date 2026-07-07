//! Served-notification popups: speech bubbles stacked bottom-right above
//! the taskbar, oldest nearest the corner. Any click dismisses one (the
//! daemon then emits `NotificationClosed(id, 2)`); Show/Close arrive from
//! the daemon over a calloop channel.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Logical, Point, Transform};

use super::Comp;
use crate::notify::{CloseReason, NoteMsg};
use crate::theme;
use crate::widgets::FrameRect;

pub struct NotePopup {
    pub id: u32,
    pub buf: MemoryRenderBuffer,
    pub w: i32,
    pub h: i32,
}

impl Comp {
    pub fn on_note_msg(&mut self, msg: NoteMsg) {
        match msg {
            NoteMsg::Show(note) => {
                let fb = self
                    .chrome
                    .draw_note(&note.summary, &note.body, note.urgency >= 2);
                let (w, h) = (fb.width as i32, fb.height as i32);
                let buf =
                    MemoryRenderBuffer::new(Fourcc::Argb8888, (w, h), 1, Transform::Normal, None);
                {
                    // The bubble's rounded corners are TRANSPARENT-indexed;
                    // present them as alpha 0 (the X11 version SHAPE'd the
                    // window instead) so clients show through.
                    let mut lut = self.chrome.palette().inner().present_lut();
                    lut[usize::from(pixel_graphics::TRANSPARENT)] = [0, 0, 0, 0];
                    let full: smithay::utils::Rectangle<i32, smithay::utils::Buffer> =
                        smithay::utils::Rectangle::from_size((w, h).into());
                    let mut b = buf.clone();
                    b.render()
                        .draw(|out| {
                            fb.present_into(out, &lut);
                            Ok::<_, std::convert::Infallible>(vec![full])
                        })
                        .expect("present note bubble");
                }
                let popup = NotePopup {
                    id: note.id,
                    buf,
                    w,
                    h,
                };
                // A replaces_id re-show keeps its stack slot; a new note
                // joins as newest (top of the pile).
                match self.note_popups.iter_mut().find(|p| p.id == note.id) {
                    Some(slot) => *slot = popup,
                    None => self.note_popups.push(popup),
                }
            }
            NoteMsg::Close(id) => self.note_popups.retain(|p| p.id != id),
        }
    }

    /// Screen rects of the popups, stacked bottom-right above the taskbar,
    /// oldest nearest the corner, growing upward.
    pub fn note_rects(&self) -> Vec<(u32, FrameRect)> {
        let size = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| self.backend.window_size());
        let gap = theme::GAP;
        let mut bottom = size.h - theme::TASKBAR_H;
        let mut rects = Vec::with_capacity(self.note_popups.len());
        for p in &self.note_popups {
            bottom -= gap + p.h;
            rects.push((
                p.id,
                FrameRect {
                    x: size.w - gap - p.w,
                    y: bottom + gap,
                    w: p.w,
                    h: p.h,
                },
            ));
        }
        rects
    }

    /// Any click on a bubble dismisses it; the daemon emits the
    /// `NotificationClosed(id, 2 /* by user */)` signal.
    pub fn dismiss_note_at(&mut self, pos: Point<f64, Logical>) -> bool {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        let hit = self
            .note_rects()
            .into_iter()
            .find(|(_, r)| mx >= r.x && mx < r.x + r.w && my >= r.y && my < r.y + r.h);
        let Some((id, _)) = hit else {
            return false;
        };
        self.note_popups.retain(|p| p.id != id);
        let _ = self.note_dismiss_tx.send((id, CloseReason::Dismissed));
        true
    }
}
