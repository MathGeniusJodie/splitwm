//! Event dispatch and the client/float/dock/notification window-lifecycle
//! protocol events (map/unmap/destroy/configure). Keyboard, pointer, and
//! scroll input live in `input`; this module routes raw X11 events to them
//! and to the lifecycle handling each window category owns.

use x11rb::protocol::xproto::{
    ConfigWindow, ConfigureNotifyEvent, ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt,
    EventMask, ExposeEvent, MapRequestEvent, Mapping, MappingNotifyEvent, ModMask,
    MotionNotifyEvent, UnmapNotifyEvent,
};
use x11rb::protocol::Event;

use super::input::ActiveDrag;
use super::types::{clamp_dim, Wm, R};
use crate::tree::{Rect, Win};

impl Wm {
    // --- event dispatch ---

    #[allow(clippy::needless_pass_by_value)]
    /// Process a drained batch of events, coalescing consecutive `MotionNotify`
    /// events down to the most recent one. A drag emits motion events far
    /// faster than a full-screen software recomposite can keep up with; without
    /// coalescing each one would queue its own `arrange()`, so renders pile up
    /// and the dragged boundary keeps sliding after the pointer has stopped.
    /// We only ever render the latest pointer position per batch.
    pub(crate) fn handle_batch(&mut self, batch: Vec<Event>) -> R<()> {
        // Coalesced per *window*, not one global slot: a batch can span a
        // window crossing (e.g. a float frame -> underlay), and keeping only
        // the very last motion would drop the final hover update on the
        // window left behind. One pointer means this stays at most 2-3
        // entries.
        let mut pending_motion: Vec<MotionNotifyEvent> = Vec::new();
        let coalesce = |pending: &mut Vec<MotionNotifyEvent>, e: MotionNotifyEvent| match pending
            .iter_mut()
            .find(|p| p.event == e.event)
        {
            Some(slot) => *slot = e,
            None => pending.push(e),
        };
        // Raw scroll deltas from one batch are summed and applied as a single
        // scroll + arrange, for the same reason motion is coalesced: a swipe
        // reports far faster than we can recomposite the whole screen.
        let mut pending_hscroll = 0.0f64;
        for ev in batch {
            match ev {
                Event::MotionNotify(e) => {
                    coalesce(&mut pending_motion, e);
                    continue;
                }
                Event::XinputRawMotion(ref e) => {
                    pending_hscroll += self.hscroll_delta(e);
                    continue;
                }
                // Legacy wheel-click compatibility events (buttons 4-7):
                // libinput synthesizes one of these alongside every smooth
                // XI2 scroll report, for X clients that only understand the
                // discrete-click protocol. We scroll from the raw axis
                // instead, so these carry no information for us — leaving
                // them unhandled here would force a flush of the accumulated
                // scroll delta per event, defeating the coalescing above: a
                // burst of N scroll reports would mean N clicks means N full
                // recomposites.
                //
                // Exception: on a server with no smooth-scroll devices at all
                // (`hscroll` empty — XI2 missing or too old, e.g. some Xvfb/
                // Xephyr setups) the legacy clicks are the *only* scroll
                // input there is, so horizontal ticks (6 = left, 7 = right)
                // feed the pan directly, one wheel-click per event.
                //
                // Vertical ticks (4 = up, 5 = down) are consumed and
                // deliberately dropped in both cases: the WM has no
                // vertical-scroll behaviour of its own, and clients still
                // receive their own copies (these only reach us over the
                // underlay/root, never from inside a client window).
                Event::ButtonPress(e) if (4..=7).contains(&e.detail) => {
                    if self.hscroll.is_empty() {
                        match e.detail {
                            6 => pending_hscroll -= 1.0,
                            7 => pending_hscroll += 1.0,
                            _ => {}
                        }
                    }
                    continue;
                }
                Event::ButtonRelease(e) if (4..=7).contains(&e.detail) => continue,
                // High-rate client chatter that neither reads nor mutates
                // drag/pointer/layout state, handled in place *without*
                // flushing the coalesced motion. A dock app answers every
                // per-frame reconfigure (an edge drag moves it with the
                // canvas) with a ConfigureRequest / property update /
                // expose, and those land interleaved with the motion
                // stream — flushing on each would slice the batch back
                // into per-motion pieces, one full recomposite each, and
                // the drag falls seconds behind the pointer (the exact
                // backlog coalescing exists to prevent). The root's own
                // ConfigureNotify (a screen resize) is not exempt: it
                // changes the workarea every motion handler computes
                // against, so it still flushes below.
                Event::PropertyNotify(_) | Event::Expose(_) | Event::ConfigureRequest(_) => {
                    contain(self.handle_event(&ev), event_kind(&ev))?;
                    continue;
                }
                Event::ConfigureNotify(e) if e.window != self.root => {
                    contain(self.handle_event(&ev), event_kind(&ev))?;
                    continue;
                }
                _ => {}
            }
            // Flush pending motion/scroll before any other event so ordering
            // (e.g. a button release ending a drag) is preserved.
            for m in pending_motion.drain(..) {
                contain(self.on_motion(&m), "MotionNotify")?;
            }
            if pending_hscroll != 0.0 {
                if self.debug_scroll {
                    eprintln!("splitwm: mid-batch flush forced by {ev:?}");
                }
                contain(
                    self.apply_hscroll(std::mem::take(&mut pending_hscroll)),
                    "XinputRawMotion",
                )?;
            }
            contain(self.handle_event(&ev), event_kind(&ev))?;
        }
        for m in pending_motion {
            contain(self.on_motion(&m), "MotionNotify")?;
        }
        if pending_hscroll != 0.0 {
            contain(self.apply_hscroll(pending_hscroll), "XinputRawMotion")?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Event) -> R<()> {
        // NOTE: errors from here are contained per-event by `contain` in
        // `handle_batch`; only fatal (connection) errors abort the batch.
        // Track the last real server timestamp for ICCCM focus handoffs
        // (`Wm::give_focus` wants a real time, not CURRENT_TIME).
        // PropertyNotify counts too: clients update properties while the
        // user types into them, which no input event of ours would see —
        // and `last_event_instant` records the harvest so `give_focus` can
        // tell when even this has gone stale.
        let time = match ev {
            Event::KeyPress(e) => Some(e.time),
            Event::ButtonPress(e) => Some(e.time),
            Event::ButtonRelease(e) => Some(e.time),
            Event::MotionNotify(e) => Some(e.time),
            Event::PropertyNotify(e) => Some(e.time),
            _ => None,
        };
        if let Some(t) = time {
            self.last_event_time = t;
            self.last_event_instant = std::time::Instant::now();
        }
        match ev {
            Event::MapRequest(e) => self.on_map_request(*e)?,
            Event::UnmapNotify(e) => self.on_unmap(e)?,
            Event::DestroyNotify(e) => self.on_destroy(e.window)?,
            Event::ConfigureRequest(e) => self.on_configure_request(e)?,
            // The root's own ConfigureNotify is a screen resize (RandR).
            Event::ConfigureNotify(e) if e.window == self.root => self.on_root_resize(e)?,
            Event::KeyPress(e) => self.on_key(*e)?,
            Event::KeyRelease(e) => self.on_key_release(e),
            Event::ButtonPress(e) => self.on_button(*e)?,
            Event::ButtonRelease(e) => self.on_button_release(e)?,
            Event::MotionNotify(e) => self.on_motion(e)?,
            Event::Expose(e) => self.on_expose(*e)?,
            // A client (re)set its icon after we managed it (Electron apps
            // set _NET_WM_ICON only after mapping).
            Event::PropertyNotify(e) if e.atom == self.atoms.net_wm_icon => {
                self.on_icon_change(e.window)?;
            }
            // A managed client (re)set its WM_CLASS. The dock is identified
            // by class with a title fallback for classless windows (see
            // `Wm::matches_dock`), and some toolkits set this only after
            // mapping — a late-identified dock would otherwise tile as a
            // normal window forever.
            Event::PropertyNotify(e)
                if e.atom == u32::from(x11rb::protocol::xproto::AtomEnum::WM_CLASS) =>
            {
                self.on_dock_identity_change(e.window, e.atom)?;
            }
            // A managed client (re)set its title: refresh the cached title
            // and, for a still-unidentified dock, retry the classless
            // title-fallback match (see `Wm::matches_dock`).
            Event::PropertyNotify(e)
                if e.atom == self.atoms.net_wm_name
                    || e.atom == u32::from(x11rb::protocol::xproto::AtomEnum::WM_NAME) =>
            {
                self.on_dock_identity_change(e.window, e.atom)?;
                self.on_title_change(e.window)?;
            }
            // Keyboard layout / modifier mapping changed: rebind everything.
            Event::MappingNotify(e) => self.on_mapping(e)?,
            // Device hotplug: rebuild the horizontal-scroll device map.
            Event::XinputHierarchy(_) => self.build_hscroll_map()?,
            // The notification-daemon thread pinged us: drain its channel.
            Event::ClientMessage(e) if e.type_ == self.atoms.splitwm_note => {
                self.on_note_ping()?;
            }
            // A background theme-icon fetch finished: drain its channel.
            Event::ClientMessage(e) if e.type_ == self.atoms.splitwm_icon => {
                self.on_icon_ping()?;
            }
            // EWMH fullscreen request (data32: [action, prop1, prop2, ..]).
            // The spec mandates format 32; a malformed 8/16-format message
            // must not have its bytes reinterpreted as data32 words.
            Event::ClientMessage(e) if e.type_ == self.atoms.net_wm_state && e.format == 32 => {
                let d = e.data.as_data32();
                let fs = self.atoms.net_wm_state_fullscreen;
                if d[1] == fs || d[2] == fs {
                    // 0 = remove, 1 = add, 2 = toggle.
                    let on = match d[0] {
                        0 => false,
                        1 => true,
                        _ => self.fullscreen() != Some(e.window),
                    };
                    self.set_fullscreen(e.window, on)?;
                }
            }
            // A pager/panel asks to activate a window.
            Event::ClientMessage(e) if e.type_ == self.atoms.net_active_window => {
                self.on_activate_request(e.window)?;
            }
            // Another WM took over the manager selection (e.g. its own
            // `--replace`); quit gracefully so it can grab the redirect.
            Event::SelectionClear(e) if e.owner == self.sel_owner => self.running = false,
            // Errors from unchecked requests land here; at least say so
            // instead of silently eating them.
            Event::Error(e) => eprintln!("splitwm: X error: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    fn on_configure_request(&mut self, e: &ConfigureRequestEvent) -> R<()> {
        // Honour requests for windows we don't (yet) manage; managed clients
        // are positioned by arrange() and just get the synthetic echo
        // ICCCM 4.1.5 requires for a denied request (see
        // `send_synthetic_configure`).
        if self.clients.contains_key(&e.window) {
            self.send_synthetic_configure(e.window)?;
            return Ok(());
        }
        // A float resizing itself keeps its new size but our position (the
        // frame is the move handle); resize + repaint the frame to match.
        if let Some(i) = self.floats.iter().position(|f| f.win == e.window) {
            let f = &mut self.floats[i];
            if u16::from(e.value_mask) & u16::from(ConfigWindow::WIDTH) != 0 {
                f.w = i32::from(e.width).max(1);
            }
            if u16::from(e.value_mask) & u16::from(ConfigWindow::HEIGHT) != 0 {
                f.h = i32::from(e.height).max(1);
            }
            let (frame, w, h, x, y) = (f.frame, f.w, f.h, f.x, f.y);
            self.configure_float_frame(e.window, frame, x, y, w, h)?;
            self.paint_float_frame(frame)?;
            // The position (and any denied field) stayed ours, so the
            // request may have been a complete no-op — X emits no
            // ConfigureNotify for one, so it still needs the echo below.
            return self.send_synthetic_configure(e.window);
        }
        // The dock's geometry is ours (set once in manage_dock and reasserted
        // by every place_dock): granting its request would let it drift for
        // one frame and then snap back. Deny + echo, like tiled clients.
        if self.dock.docked.is_some_and(|d| d.win == e.window) {
            return self.send_synthetic_configure(e.window);
        }
        let aux = ConfigureWindowAux::from_configure_request(e);
        self.conn.configure_window(e.window, &aux)?;
        // A notification resizing itself keeps its new size, but position
        // stays ours: record the size and re-stack the bottom-right pile.
        if let Some(n) = self.notes.foreign.iter_mut().find(|n| n.win == e.window) {
            if u16::from(e.value_mask) & u16::from(ConfigWindow::WIDTH) != 0 {
                n.w = i32::from(e.width).max(1);
            }
            if u16::from(e.value_mask) & u16::from(ConfigWindow::HEIGHT) != 0 {
                n.h = i32::from(e.height).max(1);
            }
            self.place_notifications()?;
        }
        Ok(())
    }

    /// Echo a managed window's current geometry back as a synthetic
    /// ConfigureNotify (ICCCM 4.1.5, for denied ConfigureRequests).
    /// Answered from our own tracked geometry when we have it — some
    /// toolkits fight a tiler with a stream of ConfigureRequests, and a
    /// `GetGeometry` round trip per denial stalls the event loop — falling
    /// back to `GetGeometry` only for windows whose geometry we don't track
    /// (hidden/taskbar'd clients). The window is a root child, so both
    /// sources are root-relative, as the synthetic event requires.
    fn send_synthetic_configure(&self, win: u32) -> R<()> {
        let (x, y, w, h) = match self.tracked_geometry(win) {
            Some(g) => g,
            None => {
                let g = self.conn.get_geometry(win)?.reply()?;
                (
                    i32::from(g.x),
                    i32::from(g.y),
                    i32::from(g.width),
                    i32::from(g.height),
                )
            }
        };
        let ev = ConfigureNotifyEvent {
            response_type: x11rb::protocol::xproto::CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: win,
            window: win,
            above_sibling: x11rb::NONE,
            x: i16::try_from(x).unwrap_or(0),
            y: i16::try_from(y).unwrap_or(0),
            width: u16::try_from(w.max(1)).unwrap_or(1),
            height: u16::try_from(h.max(1)).unwrap_or(1),
            border_width: 0,
            override_redirect: false,
        };
        self.conn
            .send_event(false, win, EventMask::STRUCTURE_NOTIFY, ev)?;
        Ok(())
    }

    /// The screen geometry `(x, y, w, h)` we last configured `win` to, when
    /// derivable without asking the server: the fullscreen client's
    /// workarea, a visible tiled client's slot (the same formula
    /// `place_clients` applies, min-size clamp included), or the dock's
    /// pinned column. `None` for anything else (hidden clients, unknowns).
    fn tracked_geometry(&self, win: Win) -> Option<(i32, i32, i32, i32)> {
        if let Some(d) = self.dock.docked.filter(|d| d.win == win) {
            return Some(self.dock_geometry(d));
        }
        if self.raw_fullscreen() == Some(win) {
            let full = self.wa();
            return Some((full.x, full.y, full.w.max(1), full.h.max(1)));
        }
        // Floats: `f.x`/`f.y` are the client window's root coordinates (the
        // frame is a sibling underneath, not a reparent).
        if let Some(f) = self.floats.iter().find(|f| f.win == win) {
            return Some((f.x, f.y, f.w.max(1), f.h.max(1)));
        }
        let leaf = self.state.tree.find_leaf_for_client(win)?;
        // A hidden window (minimized, or its split scrolled off-screen) was
        // never configured to its slot; answer from the server instead.
        let client = self.clients.get(&win);
        if self.state.tree.leaf(leaf).is_some_and(|l| l.minimized)
            || !client.is_some_and(|c| c.mapped)
        {
            return None;
        }
        let r = self.prev_frame_rect.get(&leaf)?;
        let min_size = client.map_or((1, 1), |c| c.min_size);
        Some(super::types::client_rect_in_frame(*r, min_size))
    }

    /// The screen changed size (RandR): refresh the cached workarea, resize
    /// the underlay to keep covering it, rescale the wallpaper, and re-tile.
    fn on_root_resize(&mut self, e: &ConfigureNotifyEvent) -> R<()> {
        let (w, h) = (i32::from(e.width), i32::from(e.height));
        if w == self.workarea.w && h == self.workarea.h {
            return Ok(());
        }
        self.workarea = Rect { x: 0, y: 0, w, h };
        self.conn.configure_window(
            self.underlay,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(clamp_dim(w.max(1)))
                .height(clamp_dim(h.max(1))),
        )?;
        self.set_wallpaper();
        self.update_net_workarea()?;
        self.clamp_scroll();
        self.arrange()
    }

    /// Keyboard layout or modifier mapping changed at runtime: drop every
    /// key grab (they're bound to now-stale keycodes), rebuild the
    /// keysym→keycode map, and re-grab. Also drops any in-progress
    /// autorepeat bookkeeping: a keycode recorded in `layout_key_state`
    /// referred to whatever action the old mapping bound it to, and the
    /// regrab below has a window where a genuine `KeyRelease` could be
    /// missed (no grab installed to deliver it) — either way, a stale
    /// `Held` entry would otherwise swallow every future press of that
    /// keycode until the process restarts, with no self-correction.
    fn on_mapping(&mut self, e: &MappingNotifyEvent) -> R<()> {
        if e.request == Mapping::POINTER {
            return Ok(());
        }
        self.conn.ungrab_key(0u8, self.root, ModMask::ANY)?; // 0 = AnyKey
        self.keymap.clear();
        self.bindings.clear();
        self.layout_key_state.clear();
        self.build_keymap()?;
        self.grab_keys()?;
        Ok(())
    }

    /// Shared epilogue for every layout-mutating action: keep the focused
    /// split in view, land the scroll, re-arrange, and focus the focused
    /// split's client.
    pub(crate) fn commit_layout(&mut self) -> R<()> {
        // A layout mutation invalidates any in-progress gap/edge drag: the
        // drag's `parent`/`idx` snapshot may now name a removed node or a
        // shifted child slot, and further motion would silently resize the
        // wrong boundary. (Float drags don't reference the tree; they keep
        // going.)
        match self.drags.active {
            Some(ActiveDrag::Float(_)) | None => {}
            Some(ActiveDrag::Split(_) | ActiveDrag::Edge(_)) => self.drags.active = None,
        }
        // Re-clamp before ensure_in_view: the mutation may have shrunk the
        // scroll range (closed column), and ensure_in_view must judge
        // visibility from an in-range scroll.
        self.clamp_scroll();
        let wa = self.la();
        self.state.ensure_in_view(wa);
        self.state.land_scroll();
        self.arrange()?;
        // A focused dialog keeps the keyboard across pure layout changes
        // (split/resize/insert): re-assert its focus instead of handing it
        // to the focused split's client, so Mod4+Shift+C still closes the
        // dialog the user is looking at. Deliberate focus-moving actions
        // (`on_key`, activation requests) clear the focused-float record
        // first (see `Wm::clear_focused_float`).
        if let Some(fw) = self.focused_float() {
            return self.focus_float(fw);
        }
        let f = self.state.focused_client();
        self.focus(f)
    }

    /// Bring a managed tiled window into view and focus it: into its split
    /// if it has one, otherwise into the focused split. The shared policy
    /// behind `_NET_ACTIVE_WINDOW` activation and ICCCM deiconify
    /// MapRequests. It takes focus, so a focused dialog yields the keyboard.
    pub(crate) fn bring_into_layout(&mut self, win: u32) -> R<()> {
        self.clear_focused_float();
        if !self.state.activate_client(win) {
            let leaf = self.state.focused_leaf_valid();
            self.state.assign_to_leaf(win, leaf);
        }
        self.commit_layout()
    }

    /// `_NET_ACTIVE_WINDOW` ClientMessage: bring the window into view and
    /// focus it (see `bring_into_layout`); floats just take focus.
    fn on_activate_request(&mut self, win: u32) -> R<()> {
        if self.clients.contains_key(&win) {
            return self.bring_into_layout(win);
        }
        if self.floats.iter().any(|f| f.win == win) {
            self.focus_float(win)?;
            self.raise_notifications()?;
        }
        Ok(())
    }

    fn on_map_request(&mut self, e: MapRequestEvent) -> R<()> {
        // A float re-requesting a map (some toolkits unmap/remap on hide)
        // must not be managed again: just show and re-focus it.
        if self.floats.iter().any(|f| f.win == e.window) {
            self.conn.map_window(e.window)?;
            self.focus_float(e.window)?;
            self.raise_notifications()?;
            return Ok(());
        }
        if self.clients.contains_key(&e.window) {
            // A map request for a window we manage but have hidden is the
            // ICCCM deiconify request (Iconic -> Normal): bring it into a
            // split rather than blindly mapping it over the layout.
            return self.bring_into_layout(e.window);
        }
        // The dock re-requesting a map must not fall through to manage():
        // matches_dock would see this same window already docked, misread
        // it as a second dock, and tile it while place_dock still manages
        // it — a permanently leaked, geometry-fighting duplicate.
        if self.dock.docked.is_some_and(|d| d.win == e.window) {
            self.conn.map_window(e.window)?;
            return Ok(());
        }
        self.manage(e.window, false)?;
        Ok(())
    }

    /// A window was unmapped. Layout hiding accounts for its own unmaps in
    /// `ignore_unmaps`; anything beyond that is the client withdrawing
    /// itself (ICCCM), so it must be unmanaged here — otherwise the next
    /// arrange would forcibly re-map a withdrawn window.
    fn on_unmap(&mut self, e: &UnmapNotifyEvent) -> R<()> {
        // Each unmap of a client is delivered twice: once via the root's
        // SubstructureNotify and once via the client's own StructureNotify
        // mask. Act only on the root copy (which is also where ICCCM
        // synthetic withdraw notifications arrive) so nothing double-fires.
        if e.event != self.root {
            return Ok(());
        }
        let win = e.window;
        // A self-inflicted UnmapNotify carries the sequence number of the
        // UnmapWindow request that caused it, so it is matched by sequence
        // rather than merely counted: an unmap the WM issues for a window
        // the client has just withdrawn generates no event (already
        // unmapped), and a bare counter would swallow the client's own
        // withdraw notification in its place. See `Wm::take_ignored_unmap`
        // for the exact matching/pruning semantics.
        if self.take_ignored_unmap(win, e.sequence) {
            return Ok(());
        }
        if self.dock.docked.is_some_and(|d| d.win == win) {
            self.dock.docked = None;
            // The dock's scroll headroom is gone; don't leave the viewport
            // parked in it.
            self.clamp_scroll();
            return self.arrange();
        }
        if self.notes.foreign.iter().any(|n| n.win == win) {
            return self.forget_notification(win);
        }
        if self.floats.iter().any(|f| f.win == win) {
            self.withdraw_wm_state(win)?;
            return self.forget_float(win);
        }
        if self.clients.contains_key(&win) {
            self.withdraw_wm_state(win)?;
            self.forget_client(win)?;
        }
        Ok(())
    }

    fn on_destroy(&mut self, win: u32) -> R<()> {
        if self.dock.docked.is_some_and(|d| d.win == win) {
            self.dock.docked = None;
            self.clamp_scroll();
            return self.arrange();
        }
        if self.notes.foreign.iter().any(|n| n.win == win) {
            return self.forget_notification(win);
        }
        if self.floats.iter().any(|f| f.win == win) {
            return self.forget_float(win);
        }
        self.forget_client(win)?;
        Ok(())
    }

    fn on_expose(&mut self, e: ExposeEvent) -> R<()> {
        // The underlay needs no handling here: its composited image is its
        // `background_pixmap`, so the server repaints exposed areas itself.
        if self.floats.iter().any(|f| f.frame == e.window) {
            self.paint_float_frame(e.window)?;
        } else {
            self.paint_note_win(e.window)?;
        }
        Ok(())
    }
}

/// Contain a non-fatal error (logged, tagged with `what`) so surrounding
/// work still runs: aborting an event batch can drop a queued ButtonRelease
/// and leave a drag armed with no button held (hover motion then keeps
/// resizing a boundary). Fatal (connection) errors still propagate — the
/// loop must exit on those.
pub(super) fn contain(r: R<()>, what: &str) -> R<()> {
    match r {
        Err(e) if !e.is_fatal() => {
            eprintln!("splitwm: error handling {what}: {e}");
            Ok(())
        }
        other => other,
    }
}

/// The `Event` variant's name, for tagging a `contain`'d error with which
/// handler actually failed instead of the uninformative literal "event".
fn event_kind(ev: &Event) -> &'static str {
    match ev {
        Event::ButtonPress(_) => "ButtonPress",
        Event::ButtonRelease(_) => "ButtonRelease",
        Event::ClientMessage(_) => "ClientMessage",
        Event::ConfigureNotify(_) => "ConfigureNotify",
        Event::ConfigureRequest(_) => "ConfigureRequest",
        Event::DestroyNotify(_) => "DestroyNotify",
        Event::EnterNotify(_) => "EnterNotify",
        Event::Error(_) => "Error",
        Event::Expose(_) => "Expose",
        Event::KeyPress(_) => "KeyPress",
        Event::KeyRelease(_) => "KeyRelease",
        Event::LeaveNotify(_) => "LeaveNotify",
        Event::MapNotify(_) => "MapNotify",
        Event::MapRequest(_) => "MapRequest",
        Event::MappingNotify(_) => "MappingNotify",
        Event::MotionNotify(_) => "MotionNotify",
        Event::PropertyNotify(_) => "PropertyNotify",
        Event::SelectionClear(_) => "SelectionClear",
        Event::UnmapNotify(_) => "UnmapNotify",
        Event::XinputHierarchy(_) => "XinputHierarchy",
        Event::XinputRawMotion(_) => "XinputRawMotion",
        _ => "event",
    }
}
