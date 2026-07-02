//! Client window lifecycle for `Wm`: adopting/managing/unmanaging windows,
//! the docked sidebar, app icons, focus, spawning, and the small ICCCM/EWMH
//! surface (WM_STATE, WM_DELETE_WINDOW, _NET_CLIENT_LIST, _NET_ACTIVE_WINDOW).

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonIndex, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt, EventMask, GrabMode, InputFocus, MapState, ModMask, PropMode, StackMode,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::types::{Client, Wm, R};
use crate::icon::{self, Icon};
use crate::theme;
use crate::tree::Win;

/// ICCCM WM_STATE values.
pub(crate) const WM_STATE_WITHDRAWN: u32 = 0;
pub(crate) const WM_STATE_NORMAL: u32 = 1;
pub(crate) const WM_STATE_ICONIC: u32 = 3;

impl Wm {
    /// Adopt windows already mapped on the root when splitwm starts, so
    /// taking over from a previous window manager doesn't lose whatever was
    /// on screen. A well-behaved WM adds every client it reparents to its
    /// SaveSet, so once it exits (releasing `SUBSTRUCTURE_REDIRECT`, which
    /// is what let us become the WM in the first place) the X server
    /// auto-reparents surviving client windows back onto root, still
    /// mapped — exactly what a normal `MapRequest` handles, just batched
    /// here once at startup instead of arriving one at a time.
    pub(crate) fn manage_existing_windows(&mut self) -> R<()> {
        let children = self.conn.query_tree(self.root)?.reply()?.children;
        for win in children {
            if win == self.underlay || win == self.menu.main_win || win == self.menu.sub_win {
                continue;
            }
            let Ok(attrs) = self.conn.get_window_attributes(win)?.reply() else {
                continue;
            };
            if attrs.override_redirect
                || attrs.map_state != MapState::VIEWABLE
                || attrs.class != WindowClass::INPUT_OUTPUT
            {
                continue;
            }
            self.manage(win, true)?;
        }
        Ok(())
    }

    /// Start managing `win`. `already_mapped` distinguishes adopted
    /// already-viewable windows (whose next unmap by us must be counted in
    /// `ignore_unmaps`) from fresh `MapRequest`s that are not yet mapped.
    pub(crate) fn manage(&mut self, win: Win, already_mapped: bool) -> R<()> {
        if self.is_notification(win) {
            return self.manage_notification(win);
        }
        let title = self.client_title(win);
        if title.as_ref() == self.dock_title {
            if self.docked.is_none() {
                return self.manage_dock(win);
            }
            eprintln!(
                "splitwm: second '{}' dock window {win:#x}; tiling it normally",
                self.dock_title
            );
        }

        // Class -> label; app icon from _NET_WM_ICON, falling back to the
        // icon theme (some apps, e.g. Electron ones, set the property late
        // or not at all — see `on_icon_change` for the late case).
        let class = self.client_identity(win);
        let label = class.chars().next().map_or('?', |c| c.to_ascii_uppercase());
        let icon = self.fetch_icon(win).or_else(|| self.theme_icon(&class));
        let icon_slot = self.assign_icon_slot(&class);

        // Place the client into the focused split (displacing any occupant).
        self.state.pin_client(win);

        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )?;
        // Click-to-focus passive grab.
        self.conn.grab_button(
            true,
            win,
            EventMask::BUTTON_PRESS,
            GrabMode::SYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            ButtonIndex::M1,
            ModMask::ANY,
        )?;
        // No border; the chrome (border + tab bar) is drawn on the underlay.
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;

        self.clients.insert(
            win,
            Client {
                label,
                icon,
                icon_rotated: None,
                class: class.clone(),
                icon_slot,
                mapped: already_mapped,
            },
        );
        self.refresh_icon_rotations(&class);
        if !self.bar_order.contains(&win) {
            self.bar_order.push(win);
        }
        self.update_client_list()?;
        self.state.activate_client(win);
        self.arrange()?;
        self.focus(Some(win))?;
        // arrange() has mapped it (or left it hidden); record the ICCCM state.
        let mapped = self.clients.get(&win).is_some_and(|c| c.mapped);
        self.set_wm_state(
            win,
            if mapped {
                WM_STATE_NORMAL
            } else {
                WM_STATE_ICONIC
            },
        )?;
        Ok(())
    }

    /// Pin `win` (`Wm::dock_title`) as a borderless window parked past
    /// the right end of the scrolling canvas, revealed by scrolling all the
    /// way right (see `place_dock`/`State::dock_extra`): it never enters
    /// `clients`/the split tree/taskbar, so it gets none of their chrome or
    /// focus cycling, and normal tiled columns never lay out under it. Its
    /// size is whatever it asked for at creation time, kept fixed for the
    /// rest of the session.
    fn manage_dock(&mut self, win: Win) -> R<()> {
        let width = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(240, |g| i32::from(g.width));
        self.docked = Some(win);
        self.docked_w = width.max(1);

        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        self.conn.grab_button(
            true,
            win,
            EventMask::BUTTON_PRESS,
            GrabMode::SYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            ButtonIndex::M1,
            ModMask::ANY,
        )?;
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;

        // arrange() calls place_dock() with the freshly computed workarea
        // and canvas width, so no separate initial placement is needed here.
        self.arrange()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Whether `win` declares `_NET_WM_WINDOW_TYPE_NOTIFICATION` (the type
    /// property is a preference-ordered list; any entry counts).
    fn is_notification(&self, win: Win) -> bool {
        self.conn
            .get_property(
                false,
                win,
                self.atoms.net_wm_window_type,
                AtomEnum::ATOM,
                0,
                32,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| {
                Some(
                    r.value32()?
                        .any(|a| a == self.atoms.net_wm_window_type_notification),
                )
            })
            .unwrap_or(false)
    }

    /// Pin `win` as a notification: never in the split tree or taskbar,
    /// stacked above everything at the bottom-right of the screen (above the
    /// taskbar strip), at whatever size it requested. Newer notifications
    /// stack upward above older ones.
    fn manage_notification(&mut self, win: Win) -> R<()> {
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;
        if !self.notifications.contains(&win) {
            self.notifications.push(win);
        }
        self.place_notifications()?;
        self.conn.map_window(win)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Position every notification: right-aligned to the screen edge (split
    /// gap margin), stacked bottom-up starting just above the taskbar, each
    /// raised to the top of the stacking order (oldest at the bottom).
    pub(crate) fn place_notifications(&self) -> R<()> {
        let wa = self.wa();
        let gap = theme::GAP;
        let mut bottom = wa.y + wa.h - Self::taskbar_h();
        for &win in &self.notifications {
            let Some(g) = self
                .conn
                .get_geometry(win)
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                continue;
            };
            let (w, h) = (i32::from(g.width), i32::from(g.height));
            bottom -= gap + h;
            self.conn.configure_window(
                win,
                &ConfigureWindowAux::new()
                    .x(wa.x + wa.w - gap - w)
                    .y(bottom)
                    .stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    /// Restack every notification to the top, preserving their relative
    /// order — arrange()/focus() raise tiled clients, so notifications must
    /// be re-raised afterwards to stay on top of everything.
    pub(crate) fn raise_notifications(&self) -> R<()> {
        for &win in &self.notifications {
            self.conn
                .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        }
        Ok(())
    }

    /// Stop tracking a closed notification and re-stack the survivors.
    pub(crate) fn forget_notification(&mut self, win: Win) -> R<()> {
        self.notifications.retain(|&w| w != win);
        self.place_notifications()?;
        Ok(())
    }

    /// Stop managing `win` (destroyed or withdrawn): drop all bookkeeping,
    /// re-tile, and keep focus inside the leaf the window lived in.
    pub(crate) fn forget_client(&mut self, win: Win) -> R<()> {
        if self.clients.remove(&win).is_none() {
            return Ok(());
        }
        self.bar_order.retain(|&w| w != win);
        self.ignore_unmaps.remove(&win);
        self.state.unpin_client(win);
        self.update_client_list()?;
        self.arrange()?;
        let next = self.state.focused_client();
        self.focus(next)?;
        Ok(())
    }

    /// Ask `win` to close via `WM_DELETE_WINDOW` when it participates in
    /// the protocol (giving it a chance to prompt/save); fall back to
    /// disconnecting its client only when it doesn't.
    pub(crate) fn close_client(&self, win: Win) -> R<()> {
        let supports_delete = self
            .conn
            .get_property(false, win, self.atoms.wm_protocols, AtomEnum::ATOM, 0, 32)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| Some(r.value32()?.any(|a| a == self.atoms.wm_delete_window)))
            .unwrap_or(false);
        if supports_delete {
            let msg = ClientMessageEvent::new(
                32,
                win,
                self.atoms.wm_protocols,
                [self.atoms.wm_delete_window, CURRENT_TIME, 0, 0, 0],
            );
            self.conn.send_event(false, win, EventMask::NO_EVENT, msg)?;
        } else {
            self.conn.kill_client(win)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Remap every managed client on the way out (quit or WM handover):
    /// layout hiding uses plain unmaps, and a departing WM that leaves
    /// windows unmapped strands them — the next WM only adopts viewable
    /// windows.
    pub(crate) fn restore_clients(&mut self) -> R<()> {
        let wins: Vec<Win> = self.clients.keys().copied().collect();
        for win in wins {
            self.conn.map_window(win)?;
            self.set_wm_state(win, WM_STATE_NORMAL)?;
        }
        if let Some(dock) = self.docked {
            self.conn.map_window(dock)?;
        }
        // A plain flush is not enough right before process exit: the server
        // can notice the connection hang up before draining what we just
        // wrote and drop it. A round trip proves everything was processed.
        self.conn.sync()?;
        Ok(())
    }

    // --- ICCCM / EWMH bookkeeping ---

    /// Set the ICCCM `WM_STATE` property (Normal/Iconic/Withdrawn) — some
    /// toolkits (notably Java's) misbehave without it.
    pub(crate) fn set_wm_state(&self, win: Win, state: u32) -> R<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.wm_state,
            self.atoms.wm_state,
            &[state, 0],
        )?;
        Ok(())
    }

    /// Refresh `_NET_CLIENT_LIST` on the root (managed windows in
    /// `bar_order`, i.e. mapping order), for panels/pagers.
    pub(crate) fn update_client_list(&self) -> R<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_client_list,
            AtomEnum::WINDOW,
            &self.bar_order,
        )?;
        Ok(())
    }

    // --- identity & icons ---

    /// WM_CLASS's class string (second of the "instance\0class\0" pair),
    /// used both as the taskbar label's source letter and to group windows
    /// of the same app for icon color-rotation (`assign_icon_slot`).
    fn client_identity(&self, win: Win) -> Rc<str> {
        let class = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        let parts: Vec<&[u8]> = class.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
        let name = parts
            .get(1)
            .or_else(|| parts.first())
            .copied()
            .unwrap_or(b"?");
        Rc::from(String::from_utf8_lossy(name).as_ref())
    }

    /// `WM_NAME`, used to identify windows that never set `WM_CLASS` (e.g.
    /// the `Wm::dock_title` sidebar).
    fn client_title(&self, win: Win) -> Rc<str> {
        let name = self
            .conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        Rc::from(String::from_utf8_lossy(&name).as_ref())
    }

    /// First hue-rotation slot (see `theme::icon_hue_rotation`) not already
    /// held by another open window of `class`, so windows of one app stay
    /// distinguishable while a free slot remains. Freeing is implicit: once
    /// a window is unmanaged it drops out of `self.clients`, so its slot no
    /// longer counts as used.
    fn assign_icon_slot(&self, class: &str) -> Option<usize> {
        let used: std::collections::HashSet<usize> = self
            .clients
            .values()
            .filter(|c| c.class.as_ref() == class)
            .filter_map(|c| c.icon_slot)
            .collect();
        (0..theme::ICON_HUE_STEPS).find(|s| !used.contains(s))
    }

    /// Render (once) the hue-rotated icon variant for every window of
    /// `class`, as soon as the class has two windows open. Slot 0 is a 0°
    /// rotation and keeps using the base icon; already-rendered variants
    /// are kept (the slot is persistent for the window's lifetime).
    fn refresh_icon_rotations(&mut self, class: &Rc<str>) {
        let wins: Vec<Win> = self
            .clients
            .iter()
            .filter(|(_, c)| c.class == *class)
            .map(|(&w, _)| w)
            .collect();
        if wins.len() < 2 {
            return;
        }
        for win in wins {
            let (slot, icon) = match self.clients.get(&win) {
                Some(c) if c.icon_rotated.is_none() => (c.icon_slot, c.icon.clone()),
                _ => continue,
            };
            let (Some(slot), Some(icon)) = (slot, icon) else {
                continue;
            };
            if slot == 0 {
                continue;
            }
            let rotated = Rc::new(icon::rotate(
                self.renderer.palette(),
                &icon,
                theme::icon_hue_rotation(slot),
            ));
            if let Some(c) = self.clients.get_mut(&win) {
                c.icon_rotated = Some(rotated);
            }
        }
    }

    /// The icon to draw for `win`: the pre-rendered hue-rotated variant
    /// while another window of the same app class is open (same-app
    /// disambiguation), the plain icon otherwise.
    pub(crate) fn icon_for(&self, win: Win) -> Option<Rc<Icon>> {
        let client = self.clients.get(&win)?;
        let siblings = self
            .clients
            .values()
            .filter(|c| c.class == client.class)
            .count();
        if siblings >= 2 {
            if let Some(rotated) = &client.icon_rotated {
                return Some(rotated.clone());
            }
        }
        client.icon.clone()
    }

    /// Read `_NET_WM_ICON` and pick the icon whose size is closest to (but
    /// preferably >=) the tab height. The property is a list of
    /// `width, height, w*h ARGB pixels` blocks packed as 32-bit CARDINALs.
    fn fetch_icon(&self, win: Win) -> Option<Rc<Icon>> {
        let reply = self
            .conn
            .get_property(
                false,
                win,
                self.atoms.net_wm_icon,
                AtomEnum::CARDINAL,
                0,
                u32::MAX,
            )
            .ok()?
            .reply()
            .ok()?;
        let vals: Vec<u32> = reply.value32()?.collect();
        let want = theme::tb_h() as u32;
        let mut i = 0;
        let mut best: Option<(u32, u32, usize)> = None; // (w, h, pixel_start)
        while i + 2 <= vals.len() {
            let (w, h) = (vals[i], vals[i + 1]);
            let start = i + 2;
            let count = (w as usize).checked_mul(h as usize)?;
            if w == 0 || h == 0 || start + count > vals.len() {
                break;
            }
            let better = match best {
                None => true,
                Some((bw, _, _)) => {
                    // Prefer the smallest size that still covers `want`,
                    // otherwise the largest available.
                    (w >= want && (bw < want || w < bw)) || (bw < want && w > bw)
                }
            };
            if better {
                best = Some((w, h, start));
            }
            i = start + count;
        }
        let (w, h, start) = best?;
        let argb = vals[start..start + (w * h) as usize].to_vec();
        let icon = Icon { w, h, argb };
        // Quantize to the na16 chrome palette so app icons render as flat
        // pixel art matching the rest of the UI, and so the (rotate + snap)
        // hue-rotation for same-app disambiguation stays crisp.
        Some(Rc::new(icon::quantize(self.renderer.palette(), &icon)))
    }

    /// Resolve `class` against the icon theme (the same lookup the launcher
    /// menu uses), for clients that don't provide `_NET_WM_ICON`.
    fn theme_icon(&self, class: &str) -> Option<Rc<Icon>> {
        let path = crate::menu::find_icon_file(class)
            .or_else(|| crate::menu::find_icon_file(&class.to_lowercase()))?;
        let img = icon::load_png(&path)?;
        Some(Rc::new(icon::quantize(self.renderer.palette(), &img)))
    }

    /// `_NET_WM_ICON` changed on a managed window: refetch and redraw. Apps
    /// that set the property only after mapping (Electron, notably) would
    /// otherwise keep whatever `manage` resolved at map time.
    pub(crate) fn on_icon_change(&mut self, win: Win) -> R<()> {
        let Some(class) = self.clients.get(&win).map(|c| c.class.clone()) else {
            return Ok(());
        };
        let Some(icon) = self.fetch_icon(win) else {
            return Ok(());
        };
        let client = self.clients.get_mut(&win).expect("checked above");
        client.icon = Some(icon);
        client.icon_rotated = None;
        self.refresh_icon_rotations(&class);
        self.arrange()
    }

    // --- focus & spawning ---

    pub(crate) fn focus(&self, win: Option<Win>) -> R<()> {
        match win {
            Some(w) if self.clients.contains_key(&w) => {
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, w, CURRENT_TIME)?;
                self.conn
                    .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
                self.raise_notifications()?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    self.atoms.net_active_window,
                    AtomEnum::WINDOW,
                    &[w],
                )?;
            }
            _ => {
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    self.atoms.net_active_window,
                    AtomEnum::WINDOW,
                    &[x11rb::NONE],
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn spawn_terminal(&self) {
        let term = std::env::var("TERMINAL").unwrap_or_else(|_| "xterm".into());
        self.spawn(&term);
    }

    /// Spawn a shell command, detached. `sh -c "cmd &"` reparents the real
    /// command to init immediately; waiting on the short-lived `sh` itself
    /// (it exits as soon as it has forked) is what actually prevents a
    /// zombie per launch — dropping the `Child` would leave it unreaped.
    #[allow(clippy::unused_self)]
    pub(crate) fn spawn(&self, cmd: &str) {
        match std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{cmd} &"))
            .spawn()
        {
            Ok(mut sh) => {
                let _ = sh.wait();
            }
            Err(e) => eprintln!("splitwm: failed to spawn '{cmd}': {e}"),
        }
    }
}
