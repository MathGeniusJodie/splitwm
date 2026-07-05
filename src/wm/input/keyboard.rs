//! Keyboard bindings and autorepeat handling: resolving a keypress to an
//! `Action`, swallowing autorepeat for layout-mutating actions, and running
//! each bound action against layout state.

use x11rb::protocol::xproto::{KeyPressEvent, KeyReleaseEvent, ModMask};

use super::super::types::{Action, FrameRect, Wm, R};
use super::super::widgets::BtnKind;
use crate::theme;
use crate::tree::Dir;

/// A layout-mutating keycode's (split/close/mute-toggle) autorepeat state:
/// `Held` since its last genuine `KeyPress`, or `ReleasedAt(time)` since its
/// last `KeyRelease` at server `time` — see `Wm::key_is_repeating`. Folding
/// both into one enum (rather than a keycode sitting in one "held" list and
/// a separate "last release" list) makes "held and recently-released at
/// once" for the same keycode unrepresentable instead of merely unintended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRepeatState {
    Held,
    ReleasedAt(u32),
}

impl Wm {
    pub(crate) fn lookup_action(&self, modmask: u16, keycode: u8) -> Option<Action> {
        // Keep only the 8 modifier bits, then strip Lock/NumLock before
        // matching: `KeyPress.state` is a KeyButMask, so a held mouse
        // button sets bits 8+ and would otherwise make every binding miss
        // mid-drag.
        let clean = modmask & 0x00ff & !(u16::from(ModMask::LOCK) | u16::from(ModMask::M2));
        self.bindings
            .iter()
            .find(|(m, kc, _)| *m == clean && *kc == keycode)
            .map(|(_, _, a)| *a)
    }

    /// Minimum spacing enforced between spawned `VolumeUp`/`VolumeDown`
    /// commands while the key autorepeats. X autorepeat lands ~20 `KeyPress`
    /// events/sec; forking `sh -> systemd-run -> wpctl` for every single one
    /// is a fork storm for no perceptible benefit. 66ms (~15 spawns/sec cap,
    /// i.e. every 3rd-ish repeat tick let through) still tracks a held key
    /// closely enough to resize "by feel" while cutting the fork rate by
    /// more than half.
    const VOLUME_SPAWN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(66);

    /// Spawn a volume-adjust command, rate-limited to
    /// `VOLUME_SPAWN_INTERVAL` (see its doc) so a held Volume key doesn't
    /// fork a process tree per autorepeat tick. `up` selects which
    /// direction's own timestamp gates this call, so a tap of one direction
    /// never throttles a genuine tap of the other.
    fn spawn_volume_throttled(&mut self, cmd: &str, up: bool) {
        let now = std::time::Instant::now();
        let last = &mut self.last_volume_spawn[usize::from(up)];
        let throttled = last.is_some_and(|t| now.duration_since(t) < Self::VOLUME_SPAWN_INTERVAL);
        if !throttled {
            *last = Some(now);
            self.spawn(cmd);
        }
    }

    pub(crate) fn on_key(&mut self, e: KeyPressEvent) -> R<()> {
        let Some(action) = self.lookup_action(e.state.into(), e.detail) else {
            return Ok(());
        };
        // Swallow keyboard autorepeat for the layout-mutating actions:
        // holding Mod4+V must not carve ~20 splits a second, each queueing
        // its own animation. Resize/focus actions deliberately keep
        // repeating (holding Grow is how you resize by feel). See
        // `Wm::key_is_repeating` for how a repeat is told apart from a
        // genuine fast double-tap.
        if matches!(action, Action::SplitH | Action::SplitV | Action::Close)
            && self.key_is_repeating(e.detail, e.time)
        {
            return Ok(());
        }
        // Deliberate focus movement returns the keyboard to the tree: it
        // must also clear a focused dialog's keyboard-target bookkeeping,
        // or `commit_layout` would hand focus straight back to it.
        if matches!(
            action,
            Action::FocusNext
                | Action::FocusPrev
                | Action::NextTab
                | Action::PrevTab
                | Action::MoveTabNext
                | Action::MoveTabPrev
        ) {
            self.clear_focused_float();
        }
        // Layout-changing actions get an animated transition.
        self.animate = matches!(
            action,
            Action::SplitH
                | Action::SplitV
                | Action::Close
                | Action::Grow
                | Action::Shrink
                | Action::MoveTabNext
                | Action::MoveTabPrev
        );
        // On split the existing content moves to a fresh leaf id; carry its
        // current frame rect over so it slides from its old spot, not a sliver.
        let pre_split = matches!(action, Action::SplitH | Action::SplitV)
            .then(|| {
                self.prev_frame_rect
                    .get(&self.state.focused_leaf_valid())
                    .copied()
            })
            .flatten();
        // A refused mutation (root-leaf close, resize at its clamp, no
        // adjacent split) cancels the queued animation: there is nothing to
        // slide, and a no-op transition still costs 280 ms of frame-paced
        // full-screen recomposites.
        match action {
            // Volume keys auto-repeat while held, and nothing in the layout
            // changes: skip the commit epilogue rather than recomposite ~20
            // times a second for a held key.
            // Up/down are "resize by feel": holding the key should keep
            // adjusting, so repeats aren't swallowed like Split/Close — just
            // rate-limited, or holding it down would still fork a process
            // tree per repeat tick.
            Action::VolumeUp => {
                self.spawn_volume_throttled(
                    "wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 5%+",
                    true,
                );
                return Ok(());
            }
            Action::VolumeDown => {
                self.spawn_volume_throttled("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-", false);
                return Ok(());
            }
            Action::VolumeMuteToggle => {
                // Unlike up/down, toggling mute isn't a "by feel" action —
                // there's no useful meaning to re-toggling it 20 times a
                // second while the key is held — so this reuses the
                // Split/Close swallow-all-repeats behavior instead of a rate
                // limit.
                if !self.key_is_repeating(e.detail, e.time) {
                    self.spawn("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle");
                }
                return Ok(());
            }
            Action::SpawnTerminal => self.spawn_terminal(),
            Action::SpawnLauncher => self.spawn("rofi -show combi"),
            Action::SplitH => self.try_split(Dir::H),
            Action::SplitV => self.try_split(Dir::V),
            Action::Close => self.animate &= self.state.close_focused(),
            Action::FocusNext => {
                self.state.focus_direction(true);
            }
            Action::FocusPrev => {
                self.state.focus_direction(false);
            }
            Action::NextTab => {
                self.state.cycle_taskbar(true);
            }
            Action::PrevTab => {
                self.state.cycle_taskbar(false);
            }
            Action::MoveTabNext => {
                self.animate &= self.state.move_window_to_direction(true).is_some();
            }
            Action::MoveTabPrev => {
                self.animate &= self.state.move_window_to_direction(false).is_some();
            }
            Action::Grow => self.animate &= self.state.resize_focused(theme::RESIZE_STEP),
            Action::Shrink => self.animate &= self.state.resize_focused(-theme::RESIZE_STEP),
            Action::CloseWindow => {
                // The fullscreen window covers everything, so it's the one
                // the user means regardless of where tree focus sits; then a
                // focused float (dialog), then the focused split's client.
                if let Some(c) = self
                    .fullscreen()
                    .or_else(|| self.focused_float())
                    .or_else(|| self.state.focused_client())
                {
                    self.close_client(c)?;
                }
                // Nothing in the layout changed, so skip the commit
                // epilogue: its re-arrange + re-focus would target the
                // closing window while the client is already acting on the
                // delete request, and a fast-exiting client turns that into
                // a spray of BadWindow errors. (The taskbar close button
                // returns without committing for the same reason.)
                return Ok(());
            }
        }
        if let Some(rect) = pre_split {
            self.prev_frame_rect
                .insert(self.state.focused_leaf_valid(), rect);
        }
        self.commit_layout()
    }

    /// Moves the physical key that just came back up from `Held` to
    /// `ReleasedAt(e.time)` in `layout_key_state`, so the next `KeyPress` for
    /// that keycode reads as a genuine new press rather than autorepeat —
    /// unless it turns out to be classic (non-detectable) autorepeat, whose
    /// repeat `KeyPress` follows immediately at this same server time, see
    /// `Wm::key_is_repeating`. Other keys' entries (e.g. a split key held
    /// through an interleaved mute tap) are untouched.
    pub(crate) fn on_key_release(&mut self, e: &KeyReleaseEvent) {
        match self
            .layout_key_state
            .iter_mut()
            .find(|(kc, _)| *kc == e.detail)
        {
            Some((_, state)) => *state = KeyRepeatState::ReleasedAt(e.time),
            None => self
                .layout_key_state
                .push((e.detail, KeyRepeatState::ReleasedAt(e.time))),
        }
    }

    /// Whether a `KeyPress` for `detail` at server time `time` is a
    /// continuation of an already-held layout-mutating key, per
    /// `key_is_repeating` — see there for the two autorepeat mechanisms this
    /// tells apart from a genuine fresh press. Either way, `detail` is left
    /// `Held` afterward: callers don't separately record the fresh-press
    /// case themselves.
    fn key_is_repeating(&mut self, detail: u8, time: u32) -> bool {
        key_is_repeating(&mut self.layout_key_state, detail, time)
    }

    /// Split the focused leaf in `dir` if it's eligible; otherwise cancel
    /// the animation queued for the action. Gated the same way as the
    /// titlebar Split button (which checks `leaf_meta.can_split` and skips
    /// minimized leaves): splitting a minimized leaf would clone the
    /// minimized flag into `child_a`, a state the button logic considers
    /// invalid, and produce split frames already too small for the
    /// direction, whose windows then overhang and paint over neighbours.
    fn try_split(&mut self, dir: Dir) {
        if self.can_split_focused(dir) {
            self.state.split_focused(dir);
        } else {
            self.animate = false;
        }
    }

    /// Whether the focused leaf can be split in `dir` (the same
    /// `theme::split_fits` threshold the titlebar Split button uses):
    /// never a minimized leaf, and the frame must fit two children of the
    /// direction's minimum size plus the gap between them.
    fn can_split_focused(&self, dir: Dir) -> bool {
        let leaf = self.state.focused_leaf_valid();
        if self.state.tree.leaf(leaf).is_some_and(|l| l.minimized) {
            return false;
        }
        // An off-screen leaf (scrolled out of view) has no cached frame
        // rect; its canvas-space geometry has the same size, so size checks
        // work from either. A leaf in neither is unknown — deny, since
        // splitting an unmeasured leaf is how too-small splits happen.
        let (w, h) = match self.prev_frame_rect.get(&leaf) {
            Some(f) => (f.w, f.h),
            None => match self.state.compute(self.la()).get(&leaf) {
                Some(g) => (g.w, g.h),
                None => return false,
            },
        };
        theme::split_fits(dir, w, h)
    }

    /// Act on a split-control button click. `secondary` is a right-click,
    /// which on the split button picks the opposite split direction.
    pub(crate) fn click_split_button(
        &mut self,
        leaf: crate::tree::NodeId,
        kind: BtnKind,
        secondary: bool,
    ) -> R<()> {
        let wa = self.la();
        let frame = self
            .prev_frame_rect
            .get(&leaf)
            .copied()
            .unwrap_or(FrameRect {
                x: 0,
                y: 0,
                w: wa.w,
                h: wa.h,
            });
        let meta = self.leaf_meta(leaf, frame);
        match kind {
            BtnKind::Split => {
                if !meta.can_split {
                    return Ok(());
                }
                let base = if meta.wider { Dir::H } else { Dir::V };
                let dir = if secondary {
                    match base {
                        Dir::V => Dir::H,
                        Dir::H => Dir::V,
                    }
                } else {
                    base
                };
                self.state.focus_leaf(leaf);
                let pre = self.prev_frame_rect.get(&leaf).copied();
                self.state.split_focused(dir);
                // Carry the pre-split frame so content slides from its old spot.
                if let Some(rect) = pre {
                    self.prev_frame_rect
                        .insert(self.state.focused_leaf_valid(), rect);
                }
                self.animate = true;
            }
            BtnKind::Close => {
                if meta.parent_dir.is_none() {
                    return Ok(());
                }
                self.state.focus_leaf(leaf);
                self.animate = self.state.close_focused();
            }
            BtnKind::Minimize => {
                if meta.parent_dir.is_none() {
                    return Ok(());
                }
                self.animate = self.state.toggle_minimize(leaf);
            }
        }
        self.commit_layout()
    }
}

/// Whether a `KeyPress` for `detail` at server time `time` is a continuation
/// of an already-held layout-mutating key (autorepeat), not a fresh press —
/// and updates `state` to `Held` either way, since a fresh press starts
/// holding the key just as much as a recognised repeat continues it. Two
/// distinct mechanisms feed the repeat check, matching the two ways X
/// delivers autorepeat: XKB detectable autorepeat sends consecutive
/// `KeyPress`es with no intervening `KeyRelease` at all, so the keycode's
/// entry is still `Held`; classic autorepeat sends a `KeyRelease`
/// immediately before each repeat `KeyPress`, both stamped with the same
/// server timestamp, which `Wm::on_key_release` records as
/// `ReleasedAt(time)` — a genuine key-up followed by a fresh press never
/// shares a timestamp, since some wall-clock time elapses even for the
/// fastest double-tap. Keyed by keycode (not a single shared slot) so a
/// second layout-mutating key's release in between (e.g. a split key held
/// through an interleaved mute tap) can't clobber this one's record. Pulled
/// out of `Wm::key_is_repeating` since the logic is pure `Vec` bookkeeping
/// with no need for `self`.
fn key_is_repeating(state: &mut Vec<(u8, KeyRepeatState)>, detail: u8, time: u32) -> bool {
    match state.iter_mut().find(|(kc, _)| *kc == detail) {
        Some((_, s)) => {
            let repeating = match *s {
                KeyRepeatState::Held => true,
                KeyRepeatState::ReleasedAt(t) => t == time,
            };
            *s = KeyRepeatState::Held;
            repeating
        }
        None => {
            state.push((detail, KeyRepeatState::Held));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::key_is_repeating;
    use super::KeyRepeatState::{Held, ReleasedAt};

    /// A genuine first press of a key that's never been seen isn't a repeat,
    /// and is left `Held` for the next call.
    #[test]
    fn fresh_press_is_not_repeating() {
        let mut state = vec![];
        assert!(!key_is_repeating(&mut state, 38, 100));
        assert_eq!(state, vec![(38, Held)]);
    }

    /// XKB detectable autorepeat: the keycode's entry is still `Held` (no
    /// `KeyRelease` arrived between repeats) — recognised without an entry's
    /// timestamp ever coming into it.
    #[test]
    fn held_keycode_is_repeating_under_detectable_autorepeat() {
        let mut state = vec![(38, Held)];
        assert!(key_is_repeating(&mut state, 38, 100));
        assert_eq!(state, vec![(38, Held)]);
    }

    /// Classic (non-detectable) autorepeat: a `KeyRelease` moves the entry to
    /// `ReleasedAt` immediately before each repeat's `KeyPress`, but the pair
    /// share the same server timestamp — recognised as a repeat, and the
    /// entry is restored to `Held` so the *next* repeat tick is caught too.
    #[test]
    fn same_timestamp_release_then_press_is_repeating() {
        let mut state = vec![(38, ReleasedAt(100))];
        assert!(key_is_repeating(&mut state, 38, 100));
        assert_eq!(state, vec![(38, Held)]);
    }

    /// A genuine fast double-tap: the release and the next press always
    /// carry distinct server timestamps (some wall-clock time elapses even
    /// for the fastest tap), so it must not be mistaken for a repeat — but
    /// the fresh press still leaves the entry `Held`.
    #[test]
    fn distinct_timestamp_release_then_press_is_not_repeating() {
        let mut state = vec![(38, ReleasedAt(100))];
        assert!(!key_is_repeating(&mut state, 38, 105));
        assert_eq!(state, vec![(38, Held)]);
    }

    /// A release/press pair for a *different* keycode at the same timestamp
    /// (e.g. two keys physically released and pressed in the same tick)
    /// must not cross-match; each keycode has its own independent entry.
    #[test]
    fn same_timestamp_different_keycode_is_not_repeating() {
        let mut state = vec![(38, ReleasedAt(100))];
        assert!(!key_is_repeating(&mut state, 39, 100));
        assert_eq!(state, vec![(38, ReleasedAt(100)), (39, Held)]);
    }

    /// Two layout-mutating keys held at once (e.g. a split key and mute)
    /// under classic autorepeat: an interleaved release for the *other* key
    /// at the same timestamp must not clobber this key's own pending match —
    /// each keycode's entry is independent.
    #[test]
    fn interleaved_release_of_a_different_key_does_not_clobber_this_ones_match() {
        let mut state = vec![(38, ReleasedAt(100)), (39, ReleasedAt(100))];
        assert!(key_is_repeating(&mut state, 38, 100));
        assert_eq!(state, vec![(38, Held), (39, ReleasedAt(100))]);
        assert!(key_is_repeating(&mut state, 39, 100));
        assert_eq!(state, vec![(38, Held), (39, Held)]);
    }
}
