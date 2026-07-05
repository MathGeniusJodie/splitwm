//! Input handling, split by modality: `keyboard` (bindings, autorepeat),
//! `pointer` (clicks, hit-testing, drags, hover cursor), and `scroll`
//! (trackpad/wheel horizontal-scroll panning). `events` dispatches raw X11
//! events into these, plus the window-lifecycle protocol handling that
//! isn't really "input" (map/unmap/configure requests).

mod keyboard;
mod pointer;
mod scroll;

pub use keyboard::KeyRepeatState;
pub use pointer::{ActiveDrag, Cursors, DragState};
pub use scroll::HScroll;
