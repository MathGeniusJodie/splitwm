//! Keyboard-binding dispatch — mapping intercepted chords to layout
//! mutations, mirroring master's `wm/input/keyboard.rs` semantics — plus
//! the shared layout commands every input path (keyboard, titlebar
//! buttons, taskbar, drags) funnels through.

use smithay::input::keyboard::ModifiersState;

use super::Comp;
use crate::layout::{NodeId, Win};
use crate::state::Activation;
use crate::theme::{self, Action};

/// splitwm-mask bit for Ctrl. Never appears in `theme::BINDINGS`, but it
/// participates in the exact-match so `Mod4+Ctrl+X` doesn't trigger the
/// `Mod4+X` binding (X grabs matched exactly; so do we).
const CTRL: u16 = 0x04;

fn mask_of(mods: &ModifiersState) -> u16 {
    let mut mask = 0;
    if mods.shift {
        mask |= theme::SHIFT;
    }
    if mods.ctrl {
        mask |= CTRL;
    }
    if mods.alt {
        mask |= theme::MOD1;
    }
    if mods.logo {
        mask |= theme::MOD4;
    }
    mask
}

/// The action bound to `mods`+`sym`, if any. `sym` is the level-0 keysym:
/// bindings name the key as printed on the cap (`Mod4+Shift+]` matches
/// bracketright, not braceright), exactly like the X11 keycode grabs did.
pub fn binding_action(mods: &ModifiersState, sym: u32) -> Option<Action> {
    let mask = mask_of(mods);
    theme::BINDINGS
        .iter()
        .find(|(m, k, _)| *m == mask && *k == sym)
        .map(|&(_, _, action)| action)
}

impl Comp {
    pub fn do_action(&mut self, action: Action) {
        match action {
            Action::StackBelow => self.stack_below(),
            // The titlebar close button's semantics, on the focused split
            // (see `Comp::close_split`).
            Action::Close => {
                let leaf = self.state.focused_leaf_valid();
                self.close_split(leaf);
            }
            Action::FocusNext | Action::FocusPrev => {
                // Deliberate focus moves take the keyboard off any float.
                self.clear_focused_float();
                if self
                    .state
                    .focus_direction(matches!(action, Action::FocusNext))
                {
                    self.scroll_focus_into_view();
                }
                self.arrange();
            }
            Action::MoveSplitNext | Action::MoveSplitPrev => {
                self.clear_focused_float();
                let wa = self.layout_area();
                if self
                    .state
                    .move_focused_split(wa, matches!(action, Action::MoveSplitNext))
                {
                    self.scroll_focus_into_view();
                    self.arrange();
                }
            }
            Action::Grow | Action::Shrink => {
                let wa = self.layout_area();
                if self
                    .state
                    .resize_focused(wa, matches!(action, Action::Grow))
                {
                    self.arrange();
                }
            }
            Action::SpawnTerminal => {
                let term = std::env::var("TERMINAL").unwrap_or_else(|_| "alacritty".into());
                self.spawn(&term);
            }
            Action::SpawnLauncher => self.spawn(theme::LAUNCHER_CMD),
            Action::CloseWindow => self.close_focused_window(),
            Action::VolumeUp => self.spawn(theme::VOLUME_UP_CMD),
            Action::VolumeDown => self.spawn(theme::VOLUME_DOWN_CMD),
            Action::VolumeMuteToggle => self.spawn(theme::VOLUME_MUTE_CMD),
            Action::BrightnessUp => crate::backlight::step(theme::BRIGHTNESS_STEP_PERCENT),
            Action::BrightnessDown => crate::backlight::step(-theme::BRIGHTNESS_STEP_PERCENT),
        }
    }

    /// Stack an empty split below the focused one if its frame is tall
    /// enough for two rows (the same gate as the ⊞ button's).
    fn stack_below(&mut self) {
        let wa = self.layout_area();
        let leaf = self.state.focused_leaf_valid();
        let fits = self
            .state
            .compute(wa)
            .get(&leaf)
            .is_some_and(|g| theme::stack_fits(g.h));
        if fits && self.state.split_focused() {
            self.arrange();
        }
    }

    /// Politely ask the focused window to close — a focused float before
    /// the focused split's client, so Mod4+Shift+C closes the dialog the
    /// user is looking at. There is no force-kill fallback for Wayland
    /// clients (the connection is theirs); XWayland windows get the
    /// WM_DELETE/XKillClient treatment in M7.
    fn close_focused_window(&mut self) {
        let Some(win) = self.focused_float().or_else(|| self.state.focused_client()) else {
            return;
        };
        self.close_client(win);
    }

    /// Keep the focused leaf visible: master glides there (M5); for now the
    /// scroll lands instantly.
    fn scroll_focus_into_view(&mut self) {
        let wa = self.layout_area();
        self.state.ensure_in_view(wa);
        self.state.land_scroll();
    }

    /// Detached launch in its own transient scope (see `launch::spawn`).
    pub fn spawn(&self, cmd: &str) {
        crate::launch::spawn(cmd);
    }

    /// Close the split at `leaf` — the titlebar close button's and
    /// `Action::Close`'s shared semantics. Window and split live and die
    /// together: an occupied split's close politely closes the window, and
    /// the split collapses when it actually dies (`unpin_client` — so a
    /// "do you want to save?" refusal keeps the split). An empty
    /// placeholder is removed on the spot; the sole placeholder is the one
    /// split that can't go.
    pub fn close_split(&mut self, leaf: NodeId) {
        match self.state.layout.leaf(leaf).and_then(|l| l.client) {
            Some(win) => self.close_client(win),
            None => self.view.animate = self.state.remove_empty_leaf(leaf),
        }
        self.commit_layout();
    }

    /// Focus a managed tiled window's split and scroll it into view (via
    /// `commit_layout`'s `ensure_in_view`), un-minimizing it. `animate`
    /// requests a transition, but only when rects actually moved.
    pub fn bring_into_layout(&mut self, win: Win, animate: bool) {
        let changed = match self.state.activate_client(win) {
            Activation::Unminimized => true,
            Activation::Unchanged => false,
        };
        if animate {
            self.view.animate = changed;
        }
        self.commit_layout();
    }

    /// Shared epilogue for every layout-mutating action: invalidate drags
    /// whose tree snapshot went stale, keep the focused split in view
    /// (gliding unless an animation is about to run), re-arrange.
    pub fn commit_layout(&mut self) {
        self.interaction.drag = None;
        let wa = self.layout_area();
        self.state.clamp_scroll(wa, 0);
        self.state.ensure_in_view(wa);
        // An animation's placements are computed from scroll_x at arrange
        // time and held for the whole transition; a concurrent glide would
        // make them stale every frame, so land it. Otherwise leave the
        // target so step_scroll glides the viewport over.
        if self.view.animate {
            self.state.land_scroll();
        }
        self.arrange();
    }
}
