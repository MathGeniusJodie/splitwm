//! Keyboard-binding dispatch: mapping intercepted chords to layout
//! mutations, mirroring master's `wm/input/keyboard.rs` semantics.

use smithay::input::keyboard::ModifiersState;

use super::Comp;
use crate::theme::{self, Action};
use crate::tree::Dir;

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
            Action::SplitH => self.split(Dir::H),
            Action::SplitV => self.split(Dir::V),
            Action::Close => {
                if self.state.close_focused() {
                    self.arrange();
                }
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
            Action::StashNext | Action::StashPrev => {
                self.clear_focused_float();
                if self
                    .state
                    .cycle_stash(matches!(action, Action::StashNext))
                    .is_some()
                {
                    self.arrange();
                }
            }
            Action::MoveWindowNext | Action::MoveWindowPrev => {
                self.clear_focused_float();
                if self
                    .state
                    .move_window_to_direction(matches!(action, Action::MoveWindowNext))
                    .is_some()
                {
                    self.scroll_focus_into_view();
                    self.arrange();
                }
            }
            Action::Grow => {
                if self.state.resize_focused(theme::RESIZE_STEP) {
                    self.arrange();
                }
            }
            Action::Shrink => {
                if self.state.resize_focused(-theme::RESIZE_STEP) {
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
        }
    }

    /// Split the focused leaf if its frame is big enough for two children
    /// of `dir` (the same gate as master's `can_split_focused`).
    fn split(&mut self, dir: Dir) {
        let wa = self.layout_area();
        let leaf = self.state.focused_leaf_valid();
        if self.state.tree.leaf(leaf).is_some_and(|l| l.minimized) {
            return;
        }
        let fits = self
            .state
            .compute(wa)
            .get(&leaf)
            .is_some_and(|g| theme::split_fits(dir, g.w, g.h));
        if fits && self.state.split_focused(dir) {
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
        if let Some(toplevel) = self.managed.get(win).and_then(|w| w.toplevel()) {
            toplevel.send_close();
        }
    }

    /// Keep the focused leaf visible: master glides there (M5); for now the
    /// scroll lands instantly.
    fn scroll_focus_into_view(&mut self) {
        let wa = self.layout_area();
        self.state.ensure_in_view(wa);
        self.state.land_scroll();
    }

    /// Fire-and-forget spawn through the shell, like master's fallback
    /// path. The systemd-run transient-scope launcher ports in M8.
    pub fn spawn(&self, cmd: &str) {
        if let Err(err) = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .spawn()
        {
            tracing::warn!("spawn '{cmd}': {err}");
        }
    }
}
