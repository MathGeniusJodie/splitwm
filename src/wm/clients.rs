//! Tiled-client window lifecycle for `Wm`: adopting/managing/unmanaging
//! windows, focus, spawning, and the small ICCCM/EWMH surface (WM_STATE,
//! WM_DELETE_WINDOW, _NET_CLIENT_LIST, _NET_ACTIVE_WINDOW). Floats live in
//! `floats`, the docked sidebar in `dock`, notifications in `notifications`,
//! and the icon cache in `icons` — this module dispatches to all four from
//! `manage`/`forget_client` but otherwise only owns tiled clients.

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonIndex, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt, EventMask, GrabMode, InputFocus, MapState, ModMask, PropMode, StackMode,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::types::{Client, FocusModel, Wm, WindowKind, R};

use crate::launch::shell_quote;
use crate::tree::Win;

/// ICCCM `WM_STATE` values. An enum rather than bare `u32` constants so a
/// state write can only name one of the three states the protocol defines;
/// the wire value is produced at the property-write edge (`set_wm_state`).
#[derive(Clone, Copy)]
pub(crate) enum WmState {
    Withdrawn = 0,
    Normal = 1,
    Iconic = 3,
}

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
            if win == self.underlay {
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
            // A window destroyed between query_tree and managing it (easy to
            // race during a --replace handover) must not abort the whole
            // startup — log and adopt the rest, matching the main loop's
            // per-event error containment.
            if let Err(e) = self.manage(win, true) {
                eprintln!("splitwm: failed to adopt existing window {win:#x}: {e}");
            }
        }
        // `manage` defers per-window layout work during adoption
        // (`already_mapped`): one arrange/focus/client-list pass here covers
        // the whole batch instead of one full recomposite per window.
        self.update_client_list()?;
        self.arrange()?;
        let f = self.state.focused_client();
        self.focus(f)?;
        let adopted: Vec<Win> = self.clients.keys().copied().collect();
        for win in adopted {
            self.sync_wm_state(win)?;
        }
        Ok(())
    }

    /// Record the ICCCM `WM_STATE` matching whether we currently have the
    /// window mapped (Normal) or hidden (Iconic).
    fn sync_wm_state(&self, win: Win) -> R<()> {
        let mapped = self.clients.get(&win).is_some_and(|c| c.mapped);
        self.set_wm_state(
            win,
            if mapped {
                WmState::Normal
            } else {
                WmState::Iconic
            },
        )
    }

    /// Shared adoption prologue: select the events we need from `win`, strip
    /// its core border (chrome is ours), and optionally install the
    /// click-to-focus passive button-1 grab.
    pub(crate) fn select_and_grab(&self, win: Win, mask: EventMask, grab: bool) -> R<()> {
        self.conn
            .change_window_attributes(win, &ChangeWindowAttributesAux::new().event_mask(mask))?;
        if grab {
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
        }
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;
        Ok(())
    }

    /// Start managing `win`. `already_mapped` distinguishes adopted
    /// already-viewable windows (whose next unmap by us must be recorded in
    /// `ignore_unmaps`, and whose arrange/focus work is batched by
    /// `manage_existing_windows`) from fresh `MapRequest`s that are not yet
    /// mapped.
    pub(crate) fn manage(&mut self, win: Win, already_mapped: bool) -> R<()> {
        if self.is_notification(win) {
            return self.manage_notification(win);
        }
        if self.wants_float(win) {
            return self.manage_float(win);
        }
        if self.matches_dock(win) {
            if self.dock.docked.is_none() {
                return self.manage_dock(win);
            }
            eprintln!(
                "splitwm: second '{}' dock window {win:#x}; tiling it normally",
                self.dock.title
            );
        }

        // Class -> label; app icon from _NET_WM_ICON, falling back (off
        // event loop) to the icon theme — see `resolve_icon`.
        let class = self.client_identity(win);
        let label = Self::label_from_class(&class);
        let icon = self.resolve_icon(win, &class);
        let icon_slot = self.assign_icon_slot(&class);
        let title = self.client_title(win);

        // Place the client into the focused split (displacing any occupant).
        self.state.pin_client(win);

        self.select_and_grab(
            win,
            EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
            true,
        )?;

        self.clients.insert(
            win,
            Client {
                label,
                title,
                icon,
                icon_rotated: None,
                class: class.clone(),
                icon_slot,
                mapped: already_mapped,
                // Clamped to the CARD16 wire range: hints are client-
                // controlled, and an absurd minimum would otherwise make
                // every arrange configure a size the server rejects with
                // BadValue, freezing the window at stale geometry.
                min_size: self
                    .size_hints(win)
                    .and_then(|h| h.min_size)
                    .map_or((1, 1), |(w, h)| {
                        (
                            w.clamp(1, i32::from(u16::MAX)),
                            h.clamp(1, i32::from(u16::MAX)),
                        )
                    }),
                focus: self.focus_model(win),
                icon_fetched: std::time::Instant::now(),
                icon_stale: false,
            },
        );
        self.register_kind(win, WindowKind::Tiled);
        self.refresh_icon_rotations(&class);
        if !self.bar_order.contains(&win) {
            self.bar_order.push(win);
        }
        self.state.activate_client(win);
        // During startup adoption (`already_mapped`) the arrange/focus/
        // client-list/WM_STATE work is batched by `manage_existing_windows`
        // after the loop — once for all adopted windows.
        if !already_mapped {
            self.update_client_list()?;
            // The full layout epilogue, not a bare arrange: the focused leaf
            // may be scrolled out of view, and only `commit_layout`'s
            // ensure_in_view/land_scroll brings it (and the new window) back
            // into the viewport where place_clients maps it. A new window
            // takes the keyboard, so any focused dialog yields it first.
            self.clear_focused_float();
            self.commit_layout()?;
            // The arrange has mapped it (or left it hidden); record the
            // ICCCM state.
            self.sync_wm_state(win)?;
        }
        // EWMH allows requesting fullscreen by setting the property before
        // mapping; the ClientMessage path only covers later requests.
        if self.wants_fullscreen(win) {
            self.set_fullscreen(win, true)?;
        }
        Ok(())
    }

    /// Every atom in the ATOM-list property `prop` on `win`; empty on any
    /// failure. The shared read behind protocol/window-type/state checks.
    fn prop_atoms(&self, win: Win, prop: u32) -> Vec<u32> {
        self.conn
            .get_property(false, win, prop, AtomEnum::ATOM, 0, 32)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| Some(r.value32()?.collect()))
            .unwrap_or_default()
    }

    /// Whether `win` lists `atom` in `WM_PROTOCOLS`.
    fn supports_protocol(&self, win: Win, atom: u32) -> bool {
        self.prop_atoms(win, self.atoms.wm_protocols)
            .contains(&atom)
    }

    /// Whether `win`'s `_NET_WM_STATE` *property* (set before mapping, per
    /// EWMH) already asks for fullscreen — the ClientMessage path only
    /// covers requests made after the window is managed.
    pub(crate) fn wants_fullscreen(&self, win: Win) -> bool {
        self.prop_atoms(win, self.atoms.net_wm_state)
            .contains(&self.atoms.net_wm_state_fullscreen)
    }

    pub(crate) fn size_hints(&self, win: Win) -> Option<x11rb::properties::WmSizeHints> {
        x11rb::properties::WmSizeHints::get_normal_hints(&self.conn, win)
            .ok()?
            .reply()
            .ok()?
    }

    /// ICCCM focus model from `WM_HINTS.input` (defaults to true when unset,
    /// per ICCCM) and `WM_TAKE_FOCUS` membership in `WM_PROTOCOLS`.
    pub(crate) fn focus_model(&self, win: Win) -> FocusModel {
        let input = x11rb::properties::WmHints::get(&self.conn, win)
            .ok()
            .and_then(|c| c.reply().ok().flatten())
            .and_then(|h| h.input)
            .unwrap_or(true);
        let take_focus = self.supports_protocol(win, self.atoms.wm_take_focus);
        FocusModel { input, take_focus }
    }

    pub(crate) fn is_window_type(&self, win: Win, wanted: u32) -> bool {
        self.prop_atoms(win, self.atoms.net_wm_window_type)
            .contains(&wanted)
    }

    /// Stop managing `win` (destroyed or withdrawn): drop all bookkeeping,
    /// re-tile, and keep focus inside the leaf the window lived in.
    pub(crate) fn forget_client(&mut self, win: Win) -> R<()> {
        let known = self.clients.remove(&win).is_some();
        self.unregister_kind(win);
        // A window can occupy a leaf/taskbar slot without an entry in
        // `clients` if `manage` errored out partway (it pins into the tree
        // before its X requests); clean the layout up regardless, or the
        // split shows a phantom occupant forever.
        let in_layout = self.state.tree.find_leaf_for_client(win).is_some()
            || self.state.taskbar().contains(&win);
        if !known && !in_layout {
            return Ok(());
        }
        self.forget_client_tracking(win)?;
        // Unpinning may have dropped a column; don't leave the viewport
        // scrolled past the narrower canvas.
        self.clamp_scroll();
        self.arrange()?;
        let next = self.state.focused_client();
        self.focus(next)?;
        Ok(())
    }

    /// The bookkeeping `forget_client` and `on_dock_identity_change` both
    /// need to drop `win` from every tracking structure outside `clients`
    /// itself (bar order, fullscreen state, ignored-unmap suppression,
    /// pins, `_NET_CLIENT_LIST`). Callers that immediately reclassify `win`
    /// into a different `WindowKind` run this instead of `forget_client` so
    /// they can finish with their own placement rather than a dead
    /// scroll/arrange/focus pass for a window that's about to be re-managed.
    pub(crate) fn forget_client_tracking(&mut self, win: Win) -> R<()> {
        self.clear_fullscreen_if(win);
        self.bar_order.retain(|&w| w != win);
        self.forget_ignored_unmaps(win);
        self.state.unpin_client(win);
        self.update_client_list()
    }

    /// Ask `win` to close via `WM_DELETE_WINDOW` when it participates in
    /// the protocol (giving it a chance to prompt/save); fall back to
    /// disconnecting its client only when it doesn't.
    pub(crate) fn close_client(&self, win: Win) -> R<()> {
        if self.supports_protocol(win, self.atoms.wm_delete_window) {
            // A real timestamp, not CURRENT_TIME: some clients use it to
            // arbitrate focus races on their "save changes?" prompt. A close
            // is always user-initiated, so the last input event's time is
            // fresh (and 0 degrades to CURRENT_TIME before any input).
            let msg = ClientMessageEvent::new(
                32,
                win,
                self.atoms.wm_protocols,
                [self.atoms.wm_delete_window, self.last_event_time, 0, 0, 0],
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
            self.set_wm_state(win, WmState::Normal)?;
        }
        if let Some(d) = self.dock.docked {
            self.conn.map_window(d.win)?;
        }
        for f in &self.floats {
            self.conn.map_window(f.win)?;
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
    pub(crate) fn set_wm_state(&self, win: Win, state: WmState) -> R<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.wm_state,
            self.atoms.wm_state,
            &[state as u32, 0],
        )?;
        Ok(())
    }

    /// Mark a window Withdrawn (ICCCM) on its way out of management. The
    /// write races the window's own destruction — an UnmapNotify can come
    /// from the client quitting outright, with the DestroyNotify already
    /// behind it on the wire — so the request is checked and a BadWindow
    /// deliberately swallowed: it only means there is no window left to
    /// mark. Every other error still surfaces.
    pub(crate) fn withdraw_wm_state(&self, win: Win) -> R<()> {
        let cookie = self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.wm_state,
            self.atoms.wm_state,
            &[WmState::Withdrawn as u32, 0],
        )?;
        match cookie.check() {
            Err(x11rb::errors::ReplyError::X11Error(e))
                if e.error_kind == x11rb::protocol::ErrorKind::Window =>
            {
                Ok(())
            }
            r => Ok(r?),
        }
    }

    /// A managed client's or float's `_NET_WM_NAME`/`WM_NAME` changed:
    /// refresh the cached title the titlebar draws and repaint just enough
    /// to show it — a full `arrange()` for a tiled client (whose titlebar
    /// lives in the shared composite) or a targeted `paint_float_frame` for
    /// a float's own chrome window. No-ops if the text is unchanged, since
    /// terminals retitle on every prompt.
    pub(crate) fn on_title_change(&mut self, win: Win) -> R<()> {
        let title = self.client_title(win);
        match self.kind_of(win) {
            Some(WindowKind::Tiled) => {
                let Some(client) = self.clients.get_mut(&win) else {
                    return Ok(());
                };
                if client.title == title {
                    return Ok(());
                }
                client.title = title;
                self.arrange()
            }
            Some(WindowKind::Float) => {
                let Some(f) = self.floats.iter_mut().find(|f| f.win == win) else {
                    return Ok(());
                };
                if f.title == title {
                    return Ok(());
                }
                f.title = title;
                let frame = f.frame;
                self.paint_float_frame(frame)
            }
            Some(WindowKind::Dock | WindowKind::Notification) | None => Ok(()),
        }
    }

    /// Refresh `_NET_CLIENT_LIST` on the root: managed tiled windows in
    /// `bar_order` (mapping order) plus the floats and the dock — they're
    /// managed client windows too, and panels/pagers should see them.
    pub(crate) fn update_client_list(&self) -> R<()> {
        let mut list = self.bar_order.clone();
        list.extend(self.floats.iter().map(|f| f.win));
        list.extend(self.dock.docked.map(|d| d.win));
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_client_list,
            AtomEnum::WINDOW,
            &list,
        )?;
        Ok(())
    }

    // --- EWMH fullscreen ---

    /// Apply an `_NET_WM_STATE_FULLSCREEN` change for a managed window —
    /// tiled client or float: track it, mirror the state onto the window's
    /// `_NET_WM_STATE` property, and re-arrange (which positions/stacks the
    /// fullscreen window). Only one window is fullscreen at a time; a
    /// second request replaces the first (its property is cleared so it
    /// doesn't believe it's still fullscreen).
    pub(crate) fn set_fullscreen(&mut self, win: Win, on: bool) -> R<()> {
        let is_client = match self.kind_of(win) {
            Some(WindowKind::Tiled) => true,
            Some(WindowKind::Float) => false,
            _ => return Ok(()),
        };
        if on {
            if let Some(prev) = self.set_fullscreen_win(win) {
                if prev != win {
                    self.set_net_wm_state_fullscreen(prev, false)?;
                    // If the replaced window was a fullscreen float, put its
                    // frame and geometry back (no-op for tiled clients).
                    self.restore_float_geometry(prev)?;
                }
            }
            self.set_net_wm_state_fullscreen(win, true)?;
        } else {
            if !self.clear_fullscreen_if(win) {
                return Ok(());
            }
            self.set_net_wm_state_fullscreen(win, false)?;
            if !is_client {
                self.restore_float_geometry(win)?;
            }
        }
        if is_client {
            // `bring_into_layout` rather than a bare arrange+focus: leaving
            // fullscreen in a split that was since scrolled out of view must
            // scroll it back in before focusing, or the focus would target a
            // window `place_clients` keeps unmapped.
            self.bring_into_layout(win)?;
        } else {
            // Float: `arrange`'s fullscreen block applies/keeps the
            // full-workarea geometry while it's active.
            self.arrange()?;
            if on {
                // Hide the chrome frame; the client alone covers the screen.
                if let Some(f) = self.floats.iter().find(|f| f.win == win) {
                    self.conn.unmap_window(f.frame)?;
                }
            }
            self.focus_float(win)?;
        }
        Ok(())
    }

    /// Re-show a float's chrome frame and restore its remembered geometry
    /// after leaving fullscreen. No-op for windows that aren't floats.
    fn restore_float_geometry(&mut self, win: Win) -> R<()> {
        let Some(f) = self.floats.iter().find(|f| f.win == win) else {
            return Ok(());
        };
        let (frame, x, y, w, h) = (f.frame, f.x, f.y, f.w, f.h);
        self.conn.map_window(frame)?;
        self.configure_float_frame(win, frame, x, y, w, h)?;
        self.paint_float_frame(frame)?;
        self.restack_float(win)?;
        Ok(())
    }

    /// Mirror our fullscreen bookkeeping onto the client's `_NET_WM_STATE`
    /// property. Only the fullscreen atom is ours to add or remove: states
    /// the client set itself (MODAL, SKIP_TASKBAR, ABOVE, …) must survive
    /// the rewrite, or pagers reading the property see them vanish.
    fn set_net_wm_state_fullscreen(&self, win: Win, on: bool) -> R<()> {
        let fs = self.atoms.net_wm_state_fullscreen;
        // A failed read (window racing to destruction) degrades to an empty
        // list; the write below then fails or is moot anyway.
        let mut states: Vec<u32> = self
            .conn
            .get_property(false, win, self.atoms.net_wm_state, AtomEnum::ATOM, 0, 1024)?
            .reply()
            .ok()
            .and_then(|r| r.value32().map(Iterator::collect))
            .unwrap_or_default();
        states.retain(|&s| s != fs);
        if on {
            states.push(fs);
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.net_wm_state,
            AtomEnum::ATOM,
            &states,
        )?;
        Ok(())
    }

    // --- identity ---

    /// The window's title — `_NET_WM_NAME` (UTF-8) with a latin-1 `WM_NAME`
    /// fallback. Read at manage time for the titlebar (`Client::title` /
    /// `FloatWin::title`) and kept live by `on_title_change`; also consulted
    /// as the dock identity of last resort for windows that never set
    /// `WM_CLASS` (see `matches_dock`).
    pub(crate) fn client_title(&self, win: Win) -> Rc<str> {
        let read = |atom: u32, ty: u32| -> Option<Vec<u8>> {
            let v = self
                .conn
                .get_property(false, win, atom, ty, 0, 256)
                .ok()?
                .reply()
                .ok()?
                .value;
            (!v.is_empty()).then_some(v)
        };
        let name = read(self.atoms.net_wm_name, self.atoms.utf8_string)
            .or_else(|| read(u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)))
            .unwrap_or_default();
        Rc::from(String::from_utf8_lossy(&name).as_ref())
    }

    // --- focus & spawning ---

    /// A guaranteed-fresh server timestamp: append zero bytes to a property
    /// on our never-mapped selection owner (which has `PROPERTY_CHANGE`
    /// selected) and read the time off the resulting PropertyNotify — the
    /// standard ICCCM trick. Events drained while waiting are stashed in
    /// `pending_events` for the main loop, preserving their order.
    fn fresh_timestamp(&mut self) -> R<u32> {
        self.conn.change_property8(
            PropMode::APPEND,
            self.sel_owner,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            &[],
        )?;
        self.conn.flush()?;
        // Deadline-bounded, not an unbounded wait: if the notify is lost or
        // the server wedges with the socket still open, blocking forever
        // here bricks the whole WM. On timeout, degrade to CURRENT_TIME —
        // a possibly-ignored focus request beats a hang. Connection errors
        // still propagate.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match super::wait_event_deadline(&self.conn, Some(deadline))? {
                Some(x11rb::protocol::Event::PropertyNotify(e)) if e.window == self.sel_owner => {
                    self.last_event_time = e.time;
                    self.last_event_instant = std::time::Instant::now();
                    return Ok(e.time);
                }
                Some(ev) => self.pending_events.push(ev),
                None => return Ok(CURRENT_TIME),
            }
        }
    }

    /// Deliver input focus per the ICCCM model: `SetInputFocus` only when
    /// `WM_HINTS.input` allows it, plus a `WM_TAKE_FOCUS` handshake for
    /// clients that asked for one — with a real timestamp, not
    /// `CURRENT_TIME`, so a slow client can't steal focus back across a race.
    /// A `SetInputFocus` older than the server's last focus change is
    /// silently ignored, so a harvested timestamp that has gone stale
    /// (nothing we receive carries times while the user types into a
    /// client) is replaced with a freshly fetched one.
    pub(crate) fn give_focus(&mut self, win: Win, model: FocusModel) -> R<()> {
        let time = self.focus_timestamp()?;
        if model.input {
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, win, time)?;
        }
        if model.take_focus {
            let msg = ClientMessageEvent::new(
                32,
                win,
                self.atoms.wm_protocols,
                [self.atoms.wm_take_focus, time, 0, 0, 0],
            );
            self.conn.send_event(false, win, EventMask::NO_EVENT, msg)?;
        }
        Ok(())
    }

    /// The timestamp `SetInputFocus`/`WM_TAKE_FOCUS` should carry: the last
    /// harvested event time while it's fresh, a freshly fetched server time
    /// once it has gone stale (a stale timestamp is silently ignored if
    /// focus moved more recently).
    fn focus_timestamp(&mut self) -> R<u32> {
        let stale = self.last_event_time == 0
            || self.last_event_instant.elapsed() > std::time::Duration::from_secs(2);
        // `?`, not unwrap_or: fresh_timestamp already degrades to
        // CURRENT_TIME on timeout, so an Err from it is a real (likely
        // connection) failure that must not be silently eaten here.
        if stale {
            self.fresh_timestamp()
        } else {
            Ok(self.last_event_time)
        }
    }

    pub(crate) fn focus(&mut self, win: Option<Win>) -> R<()> {
        match win {
            Some(w) if self.clients.contains_key(&w) => {
                self.clear_focused_float();
                let model = self.clients[&w].focus;
                self.give_focus(w, model)?;
                // Raising the focused client puts it above everything;
                // re-apply the shared stacking policy above it.
                self.conn
                    .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
                self.apply_stacking()?;
                self.set_net_active_window(w)?;
            }
            _ => {
                self.clear_focused_float();
                let time = self.focus_timestamp()?;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, time)?;
                self.set_net_active_window(x11rb::NONE)?;
            }
        }
        Ok(())
    }

    /// The stacking order above tiled clients, bottom to top: floats, the
    /// fullscreen window, then notifications. `arrange` and `focus` both
    /// raise windows to the top and re-apply this same sequence afterwards
    /// — inserting a new layer means adding it here, once.
    pub(crate) fn apply_stacking(&self) -> R<()> {
        self.raise_floats()?;
        self.raise_fullscreen()?;
        self.raise_notifications()
    }

    /// Advertise `win` (or `x11rb::NONE`) as `_NET_ACTIVE_WINDOW` on the
    /// root, keeping pagers in step with every focus path.
    pub(crate) fn set_net_active_window(&self, win: Win) -> R<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_active_window,
            AtomEnum::WINDOW,
            &[win],
        )?;
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
    ///
    /// When systemd-run is available the command is placed in its own
    /// transient scope under app.slice, like a desktop-environment launcher
    /// would; Chromium/Electron apps otherwise try to move themselves out of
    /// the shared session scope and log a spurious UnitExists error.
    #[allow(clippy::unused_self)]
    pub(crate) fn spawn(&self, cmd: &str) {
        // Both paths hand `cmd` to `/bin/sh -c` as one quoted word, so a
        // command line containing `;`/`&&` behaves identically whether or
        // not systemd-run is available (a bare `{cmd} &` fallback would
        // background only the last statement of a compound command).
        let line = if Self::have_systemd_run() {
            format!(
                "systemd-run --user --scope --slice=app.slice --collect --quiet -- /bin/sh -c {} &",
                shell_quote(cmd)
            )
        } else {
            format!("/bin/sh -c {} &", shell_quote(cmd))
        };
        match std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(line)
            .spawn()
        {
            Ok(mut sh) => {
                // Reap the short-lived `sh` off-thread: it exits as soon as
                // it has forked, but even that wait doesn't belong on the
                // event loop.
                super::spawn_masked(move || {
                    let _ = sh.wait();
                });
            }
            Err(e) => eprintln!("splitwm: failed to spawn '{cmd}': {e}"),
        }
    }

    /// Whether `systemd-run` exists and a user manager is reachable.
    /// Checked once and cached; false on non-systemd setups or bare X
    /// sessions. The probe is a synchronous D-Bus round trip, so `run`
    /// warms it at startup rather than letting the first launch pay for it
    /// inside the event loop.
    pub(crate) fn have_systemd_run() -> bool {
        use std::sync::OnceLock;
        static HAVE: OnceLock<bool> = OnceLock::new();
        *HAVE.get_or_init(|| {
            std::process::Command::new("systemd-run")
                .args(["--user", "--scope", "--collect", "--quiet", "--", "true"])
                .status()
                .is_ok_and(|s| s.success())
        })
    }
}

