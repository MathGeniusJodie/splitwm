//! Event dispatch and input handling for the WM core: keyboard, pointer,
//! scroll coalescing, and the client map/unmap/destroy protocol events.

use x11rb::connection::Connection;
use x11rb::protocol::xinput;
use x11rb::protocol::xproto::{
    Allow, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux, ConfigWindow,
    ConfigureNotifyEvent, ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt, EventMask,
    ExposeEvent, InputFocus, KeyPressEvent, MapRequestEvent, Mapping, MappingNotifyEvent, ModMask,
    MotionNotifyEvent, UnmapNotifyEvent,
};
use x11rb::protocol::Event;

use super::clients::WmState;
use super::types::{
    clamp_dim, rect_contains, Action, BtnKind, Drag, EdgeDrag, FloatDrag, FrameRect, Wm, MOD4, R,
};
use crate::theme;
use crate::tree::{Boundary, Dir, NodeId, Rect, Win};

/// Everything clickable on the underlay, resolved by one priority-ordered
/// hit-test (`Wm::hit_test`) shared by `on_button` (dispatch) and
/// `hover_cursor` (cursor feedback) — a single ordering both consume, so
/// click handling and hover feedback can never drift apart.
#[derive(Clone, Copy)]
enum Hit {
    /// A split-control titlebar button (close/split/minimize).
    Btn(NodeId, BtnKind),
    /// The corner "x" badge on a taskbar tile.
    TaskbarClose(Win),
    /// A taskbar tile body.
    TaskbarTile(Win),
    /// A quick-launch icon in the taskbar (`Wm::quick` index).
    QuickLaunch(usize),
    /// A leaf's titlebar tab.
    Tab(NodeId),
    /// A boundary/edge "+" insert button (root-children insert index).
    Plus(usize),
    /// A gap drag handle.
    Handle(Boundary),
    /// An outer canvas-edge resize handle (`true` = left edge).
    Edge(bool),
    /// An empty split's body (no client window catches the click there).
    LeafBody(NodeId),
    Miss,
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

    // --- trackpad / horizontal-scroll-wheel smooth canvas panning ---

    /// Sum of this raw motion event's horizontal-scroll valuator deltas
    /// (in wheel-click fractions), across every device that reported one.
    pub(crate) fn hscroll_delta(&self, e: &xinput::RawMotionEvent) -> f64 {
        if self.debug_scroll {
            eprintln!(
                "splitwm: raw motion from sourceid={} mask={:?} known_hscroll_devs={:?}",
                e.sourceid,
                e.valuator_mask,
                self.hscroll.iter().map(|h| h.dev).collect::<Vec<_>>()
            );
        }
        self.hscroll
            .iter()
            .filter(|h| h.dev == e.sourceid)
            .filter_map(|h| {
                super::valuator_value(&e.valuator_mask, &e.axisvalues, h.valuator)
                    .map(|v| v / h.incr)
            })
            .sum()
    }

    /// Apply an accumulated horizontal-scroll delta (wheel-click fractions)
    /// to the canvas, gated on where the pointer currently is: freely over
    /// the underlay/gaps and over the docked sidebar (it has no scrollable
    /// content of its own to fight for the swipe), only with Mod4 held over
    /// an ordinary client window (so a swipe doesn't fight an app's own
    /// horizontal scrolling).
    fn apply_hscroll(&mut self, delta: f64) -> R<()> {
        if !self.hscroll_allowed()? {
            return Ok(());
        }
        let wa = self.la();
        // Carry the sub-pixel remainder between batches: a slow continuous
        // swipe can deliver less than a pixel per batch, and truncating each
        // batch independently would discard the entire gesture.
        let px_f = delta.mul_add(f64::from(theme::SCROLL_STEP), self.hscroll_frac);
        let px = px_f as i32;
        self.hscroll_frac = px_f - f64::from(px);
        if px == 0 {
            return Ok(());
        }
        self.state.scroll_delta(wa, px);
        self.state.land_scroll();
        if self.debug_scroll {
            let t0 = std::time::Instant::now();
            self.arrange()?;
            eprintln!("splitwm: arrange() for scroll took {:?}", t0.elapsed());
            return Ok(());
        }
        self.arrange()
    }

    /// Whether scrolling is currently allowed (see `apply_hscroll`). A swipe
    /// can call this dozens of times a second; re-querying the pointer that
    /// often would itself be a source of per-event round-trip latency, so
    /// the answer is cached for a short window — long enough to absorb a
    /// whole burst, short enough that moving the pointer under/off a window
    /// mid-swipe is still honoured almost immediately.
    pub(crate) fn hscroll_allowed(&mut self) -> R<bool> {
        if let Some((last, allowed)) = self.hscroll_gate {
            if last.elapsed() < std::time::Duration::from_millis(30) {
                return Ok(allowed);
            }
        }
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let allowed =
            if p.child == x11rb::NONE
                || p.child == self.underlay
                || self.dock.docked.is_some_and(|d| d.win == p.child)
            {
                true
            } else {
                u16::from(p.mask) & MOD4 != 0
            };
        if self.debug_scroll {
            eprintln!(
                "splitwm: hscroll_allowed child={} underlay={} mask={:?} -> {}",
                p.child, self.underlay, p.mask, allowed
            );
        }
        self.hscroll_gate = Some((std::time::Instant::now(), allowed));
        Ok(allowed)
    }

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
                _ => {}
            }
            // Flush pending motion/scroll before any other event so ordering
            // (e.g. a button release ending a drag) is preserved.
            for m in pending_motion.drain(..) {
                contain(self.on_motion(&m), "event")?;
            }
            if pending_hscroll != 0.0 {
                if self.debug_scroll {
                    eprintln!("splitwm: mid-batch flush forced by {ev:?}");
                }
                contain(
                    self.apply_hscroll(std::mem::take(&mut pending_hscroll)),
                    "event",
                )?;
            }
            contain(self.handle_event(&ev), "event")?;
        }
        for m in pending_motion {
            contain(self.on_motion(&m), "event")?;
        }
        if pending_hscroll != 0.0 {
            contain(self.apply_hscroll(pending_hscroll), "event")?;
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
            Event::ButtonPress(e) => self.on_button(*e)?,
            Event::ButtonRelease(e) => self.on_button_release(e)?,
            Event::MotionNotify(e) => self.on_motion(e)?,
            Event::Expose(e) => self.on_expose(*e)?,
            // A client (re)set its icon after we managed it (Electron apps
            // set _NET_WM_ICON only after mapping).
            Event::PropertyNotify(e) if e.atom == self.atoms.net_wm_icon => {
                self.on_icon_change(e.window)?;
            }
            // A managed client (re)set its WM_CLASS or title. The dock is
            // identified by class with a title fallback for classless
            // windows (see `Wm::matches_dock`), and some toolkits set these
            // only after mapping — a late-identified dock would otherwise
            // tile as a normal window forever.
            Event::PropertyNotify(e)
                if e.atom == u32::from(x11rb::protocol::xproto::AtomEnum::WM_CLASS)
                    || e.atom == self.atoms.net_wm_name
                    || e.atom == u32::from(x11rb::protocol::xproto::AtomEnum::WM_NAME) =>
            {
                self.on_dock_identity_change(e.window, e.atom)?;
            }
            // Keyboard layout / modifier mapping changed: rebind everything.
            Event::MappingNotify(e) => self.on_mapping(e)?,
            // Device hotplug: rebuild the horizontal-scroll device map.
            Event::XinputHierarchy(_) => self.build_hscroll_map()?,
            // The notification-daemon thread pinged us: drain its channel.
            Event::ClientMessage(e) if e.type_ == self.atoms.splitwm_note => {
                self.on_note_ping()?;
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
                        _ => self.fullscreen != Some(e.window),
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
        // are positioned by arrange(). ICCCM 4.1.5: a WM that doesn't grant
        // a ConfigureRequest must still answer with a synthetic
        // ConfigureNotify carrying the actual geometry — clients that
        // resize themselves and block waiting for the echo (xterm's
        // `resize`) hang without it.
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
            // ConfigureNotify for one, and ICCCM 4.1.5 requires the
            // synthetic echo so a client blocking on it doesn't hang.
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
        if self.fullscreen == Some(win) {
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
        self.arrange()
    }

    /// Keyboard layout or modifier mapping changed at runtime: drop every
    /// key grab (they're bound to now-stale keycodes), rebuild the
    /// keysym→keycode map, and re-grab.
    fn on_mapping(&mut self, e: &MappingNotifyEvent) -> R<()> {
        if e.request == Mapping::POINTER {
            return Ok(());
        }
        self.conn.ungrab_key(0u8, self.root, ModMask::ANY)?; // 0 = AnyKey
        self.keymap.clear();
        self.bindings.clear();
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
        self.drags.split = None;
        self.drags.edge = None;
        let wa = self.la();
        self.state.ensure_in_view(wa);
        self.state.land_scroll();
        self.arrange()?;
        // A focused dialog keeps the keyboard across pure layout changes
        // (split/resize/insert): re-assert its focus instead of handing it
        // to the focused split's client, so Mod4+Shift+C still closes the
        // dialog the user is looking at. Deliberate focus-moving actions
        // (`on_key`, activation requests) clear `focused_float` first.
        if let Some(fw) = self.focused_float {
            if self.floats.iter().any(|f| f.win == fw) {
                return self.focus_float(fw);
            }
            self.focused_float = None;
        }
        let f = self.state.focused_client();
        self.focus(f)
    }

    /// Bring a managed tiled window into view and focus it: into its split
    /// if it has one, otherwise into the focused split. The shared policy
    /// behind `_NET_ACTIVE_WINDOW` activation and ICCCM deiconify
    /// MapRequests. It takes focus, so a focused dialog yields the keyboard.
    pub(crate) fn bring_into_layout(&mut self, win: u32) -> R<()> {
        self.focused_float = None;
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
        if let Some(seqs) = self.ignore_unmaps.get_mut(&win) {
            // A self-inflicted UnmapNotify carries the sequence number of
            // the UnmapWindow request that caused it, so it is matched by
            // sequence rather than merely counted: an unmap the WM issues
            // for a window the client has just withdrawn generates no event
            // (already unmapped), and a bare counter would swallow the
            // client's own withdraw notification in its place. Records with
            // sequences at or behind this event (modular u16 comparison)
            // are pruned — their events either just matched or will never
            // arrive. Residual race: sequence numbers on the wire are u16,
            // so a record could alias a withdraw issued exactly 65536
            // requests later; pruning keeps records too short-lived for
            // that in practice.
            let matched = seqs.contains(&e.sequence);
            seqs.retain(|&s| s.wrapping_sub(e.sequence) < 0x8000 && s != e.sequence);
            if seqs.is_empty() {
                self.ignore_unmaps.remove(&win);
            }
            if matched {
                return Ok(());
            }
        }
        if self.dock.docked.is_some_and(|d| d.win == win) {
            self.dock.docked = None;
            return self.arrange();
        }
        if self.notes.foreign.iter().any(|n| n.win == win) {
            return self.forget_notification(win);
        }
        if self.floats.iter().any(|f| f.win == win) {
            self.set_wm_state(win, WmState::Withdrawn)?;
            return self.forget_float(win);
        }
        if self.clients.contains_key(&win) {
            self.set_wm_state(win, WmState::Withdrawn)?;
            self.forget_client(win)?;
        }
        Ok(())
    }

    fn on_destroy(&mut self, win: u32) -> R<()> {
        if self.dock.docked.is_some_and(|d| d.win == win) {
            self.dock.docked = None;
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

    fn on_key(&mut self, e: KeyPressEvent) -> R<()> {
        let Some(action) = self.lookup_action(e.state.into(), e.detail) else {
            return Ok(());
        };
        // Swallow keyboard auto-repeat for the layout-mutating actions:
        // holding Mod4+V must not carve ~20 splits a second, each queueing
        // its own animation. Resize/focus actions deliberately keep
        // repeating (holding Grow is how you resize by feel).
        if matches!(action, Action::SplitH | Action::SplitV | Action::Close) {
            let now = std::time::Instant::now();
            if self.last_layout_key.is_some_and(|(kc, at)| {
                kc == e.detail && now.duration_since(at) < std::time::Duration::from_millis(200)
            }) {
                self.last_layout_key = Some((e.detail, now));
                return Ok(());
            }
            self.last_layout_key = Some((e.detail, now));
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
            self.focused_float = None;
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
            Action::SpawnTerminal => self.spawn_terminal(),
            Action::SpawnLauncher => self.spawn("rofi -show drun"),
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
                // A focused float (dialog) is the keyboard target before the
                // focused split's client.
                if let Some(c) = self.focused_float.or_else(|| self.state.focused_client()) {
                    self.close_client(c)?;
                }
            }
        }
        if let Some(rect) = pre_split {
            self.prev_frame_rect
                .insert(self.state.focused_leaf_valid(), rect);
        }
        self.commit_layout()
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
    fn click_split_button(&mut self, leaf: NodeId, kind: BtnKind, secondary: bool) -> R<()> {
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

    #[allow(clippy::too_many_lines)]
    fn on_button(&mut self, e: ButtonPressEvent) -> R<()> {
        // Any click on a notification bubble dismisses it.
        if self.dismiss_note(e.event)? {
            return Ok(());
        }
        let wa = self.la();
        // Button 1 on a float's frame: focus the float and start moving it.
        if e.detail == 1 {
            if let Some(f) = self.floats.iter().find(|f| f.frame == e.event) {
                let (win, fx, fy) = (f.win, f.x, f.y);
                self.drags.float = Some(FloatDrag {
                    win,
                    dx: i32::from(e.root_x) - fx,
                    dy: i32::from(e.root_y) - fy,
                });
                self.focus_float(win)?;
                self.raise_notifications()?;
                return Ok(());
            }
        }
        // Clicks on the underlay: one shared, priority-ordered hit-test
        // (`hit_test`) resolves the target; `hover_cursor` consumes the same
        // ordering, so click dispatch and cursor feedback stay in lockstep.
        if e.event == self.underlay && (e.detail == 1 || e.detail == 3) {
            // Hit regions are computed from the *final* layout, but a
            // layout animation may still be drawing chrome mid-slide (the
            // event loop only cuts it after this batch). Snap it now so the
            // click lands on what the user sees.
            if self.anim.is_some() {
                self.step_animation(true)?;
            }
            let (mx, my) = (i32::from(e.event_x), i32::from(e.event_y));
            let hit = self.hit_test(mx, my);
            // Split-control buttons take left and right click (right picks
            // the opposite split direction); everything else is left only.
            if let Hit::Btn(leaf, kind) = hit {
                return self.click_split_button(leaf, kind, e.detail == 3);
            }
            if e.detail != 1 {
                return Ok(());
            }
            match hit {
                Hit::Btn(..) => {} // handled above
                // The corner "x" badge on a bottom-bar tile: politely close
                // that window.
                Hit::TaskbarClose(win) => return self.close_client(win),
                // A bottom-bar icon: bring that window into view and focus
                // it — via `bring_into_layout`, whose `commit_layout` also
                // scrolls a split that sits outside the viewport back in
                // (activating a window `place_clients` keeps unmapped would
                // otherwise focus an unviewable window).
                Hit::TaskbarTile(win) => {
                    self.animate = true;
                    return self.bring_into_layout(win);
                }
                // A quick-launch icon: spawn its command. The new window
                // lands wherever a normal map lands (the focused split or
                // the taskbar).
                Hit::QuickLaunch(i) => {
                    if let Some(cmd) = self.quick.get(i).map(|q| q.cmd.clone()) {
                        self.spawn(&cmd);
                    }
                    return Ok(());
                }
                // Click a title (tab) or an empty split's body to focus it.
                Hit::Tab(leaf) | Hit::LeafBody(leaf) => {
                    self.state.focus_leaf(leaf);
                    self.arrange()?;
                    self.focus(self.state.focused_client())?;
                }
                Hit::Plus(at) => {
                    self.state.insert_at_root(at);
                    self.animate = true;
                    return self.commit_layout();
                }
                Hit::Handle(b) => {
                    // A gap next to a minimized leaf can't be dragged (its
                    // pixel size is pinned); ignore the press.
                    if b.resizable {
                        self.drags.split = Some(Drag {
                            parent: b.parent,
                            idx: b.idx,
                            vertical: b.dir == Dir::V,
                            start: b.start,
                            combined: b.first + b.second,
                            gap: theme::GAP,
                        });
                    }
                }
                // Outer canvas-edge resize handles: the screen-space x of
                // whichever end of the leftmost/rightmost column isn't being
                // dragged stays fixed for the whole gesture (see `EdgeDrag`).
                Hit::Edge(left) => {
                    if let Some((start_x, w)) = self.state.edge_span(wa, left) {
                        let canvas_anchor = if left { start_x + w } else { start_x };
                        let anchor_x = canvas_anchor - self.state.scroll_x();
                        self.drags.edge = Some(EdgeDrag { left, anchor_x });
                    }
                }
                Hit::Miss => {}
            }
            return Ok(());
        }
        // Click-to-focus on a client window.
        if e.detail == 1 {
            // Replay *before* any of the focus/arrange work below: the press
            // froze the pointer in a synchronous grab, and every call below
            // can fail (the clicked window may have died in the race window)
            // — an early `?` return that skipped the replay would leave the
            // pointer frozen until the server timed the grab out. Use the
            // grab event's own timestamp, not CURRENT_TIME — under latency
            // CURRENT_TIME can release a *later* grab than the one this
            // press froze.
            self.conn.allow_events(Allow::REPLAY_POINTER, e.time)?;
            if self.clients.contains_key(&e.event) {
                self.state.activate_client(e.event);
                self.arrange()?;
                self.focus(Some(e.event))?;
            } else if self.floats.iter().any(|f| f.win == e.event) {
                self.focus_float(e.event)?;
                self.raise_notifications()?;
            } else if self.dock.docked.is_some_and(|d| d.win == e.event) {
                // Outside the tree/`clients`, so `focus()` (which only knows
                // tiled windows) can't take it; set input focus directly.
                // The press's own timestamp, not CURRENT_TIME — same race
                // `give_focus` guards against.
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, e.event, e.time)?;
                // Keep `_NET_ACTIVE_WINDOW` in step with the keyboard like
                // every other focus path — pagers otherwise show the
                // previous window as active while the user types into the
                // dock.
                self.set_net_active_window(e.event)?;
            }
        }
        Ok(())
    }

    fn on_motion(&mut self, e: &MotionNotifyEvent) -> R<()> {
        if let Some(fd) = self.drags.float {
            self.move_float(
                fd.win,
                i32::from(e.root_x) - fd.dx,
                i32::from(e.root_y) - fd.dy,
            )?;
            self.conn.flush()?;
            return Ok(());
        }
        if let Some(ed) = self.drags.edge {
            let wa = self.la();
            let mouse_x = i32::from(e.root_x);
            // Screen-space width: `anchor_x` is the fixed far edge, so the
            // gap to the mouse *is* the target width, no scroll conversion
            // needed — width is scroll-invariant, only position isn't.
            let target_w = if ed.left {
                ed.anchor_x - mouse_x
            } else {
                mouse_x - ed.anchor_x
            };
            let applied = self.state.resize_edge(wa, ed.left, target_w);
            // Growing the left column shifts every later column's
            // canvas-space x right by `applied` (`Tree::compute` always
            // lays out left-to-right from a fixed origin); scroll by the
            // same amount so they stay put on screen and only the dragged
            // edge visibly moves.
            if ed.left && applied != 0 {
                self.state.shift_scroll(applied);
            }
            self.arrange()?;
            return Ok(());
        }
        let Some(d) = self.drags.split else {
            // Not dragging: hover feedback only.
            if e.event == self.underlay {
                let cur = self.hover_cursor(i32::from(e.event_x), i32::from(e.event_y));
                self.set_underlay_cursor(cur)?;
            }
            return Ok(());
        };
        if d.combined <= 0 {
            return Ok(());
        }
        // Only x scrolls; a vertical (row-boundary) drag reads y directly.
        let canvas_pos = if d.vertical {
            i32::from(e.root_y)
        } else {
            i32::from(e.root_x) + self.state.scroll_x()
        };
        let new_first = canvas_pos - d.start - d.gap / 2;
        let frac = f64::from(new_first) / f64::from(d.combined);
        self.state.resize_boundary(d.parent, d.idx, frac);
        self.arrange()?;
        Ok(())
    }

    /// Priority-ordered hit-test of everything clickable on the underlay,
    /// shared by `on_button` (dispatch) and `hover_cursor` (feedback).
    fn hit_test(&self, mx: i32, my: i32) -> Hit {
        if let Some((leaf, kind)) = self
            .widgets
            .btn_regions
            .iter()
            .find(|(r, _, _)| rect_contains(*r, mx, my))
            .map(|(_, l, k)| (*l, *k))
        {
            return Hit::Btn(leaf, kind);
        }
        // Compressed taskbar tiles overlap like fanned cards, rightmost on
        // top; reverse iteration matches draw order so the topmost tile
        // wins. The corner "x" badge is checked before the tile bodies so
        // the badge wins the click.
        if let Some(win) = self
            .widgets
            .taskbar_regions
            .iter()
            .rev()
            .find(|t| rect_contains(t.close, mx, my))
            .map(|t| t.win)
        {
            return Hit::TaskbarClose(win);
        }
        if let Some(win) = self
            .widgets
            .taskbar_regions
            .iter()
            .rev()
            .find(|t| rect_contains(t.rect, mx, my))
            .map(|t| t.win)
        {
            return Hit::TaskbarTile(win);
        }
        if let Some(i) = self
            .widgets
            .quick_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, i)| *i)
        {
            return Hit::QuickLaunch(i);
        }
        if let Some(leaf) = self
            .widgets
            .tab_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, l)| *l)
        {
            return Hit::Tab(leaf);
        }
        // "+" buttons sit centred inside their drag handle's (or the edge
        // handle's) larger hit region — check the narrower "+" rects first
        // so they aren't shadowed by the handles.
        if let Some(at) = self
            .widgets
            .plus_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, at)| *at)
        {
            return Hit::Plus(at);
        }
        if let Some(b) = self
            .widgets
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, b)| *b)
        {
            return Hit::Handle(b);
        }
        if let Some(&(_, left)) = self
            .widgets
            .edge_handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            return Hit::Edge(left);
        }
        if let Some(leaf) = self
            .prev_frame_rect
            .iter()
            .find(|(l, r)| self.state.tree.is_leaf(**l) && rect_contains(**r, mx, my))
            .map(|(l, _)| *l)
        {
            return Hit::LeafBody(leaf);
        }
        Hit::Miss
    }

    /// Pick the pointer cursor for a hover position on the underlay:
    /// resize arrows over gap/edge drag handles, the hand over clickable
    /// buttons, the "disabled" cursor over a disabled titlebar button, and
    /// the plain arrow otherwise. Consumes the same `hit_test` ordering as
    /// `on_button`, so the advertised cursor always matches the click.
    fn hover_cursor(&self, mx: i32, my: i32) -> u32 {
        let c = self.cursors;
        match self.hit_test(mx, my) {
            Hit::Btn(leaf, kind) => {
                // Mirror `compose`'s enabled/disabled choice for the button
                // art (a minimized leaf's whole-frame region is always a
                // live restore button).
                if let Some(&frame) = self.prev_frame_rect.get(&leaf) {
                    let meta = self.leaf_meta(leaf, frame);
                    let disabled = !meta.minimized
                        && match kind {
                            BtnKind::Close | BtnKind::Minimize => meta.parent_dir.is_none(),
                            BtnKind::Split => !meta.can_split,
                        };
                    if disabled {
                        return c.disabled;
                    }
                }
                c.hand
            }
            Hit::TaskbarClose(_)
            | Hit::TaskbarTile(_)
            | Hit::QuickLaunch(_)
            | Hit::Tab(_)
            | Hit::Plus(_) => c.hand,
            Hit::Handle(b) => {
                // A gap next to a minimized leaf can't be dragged (its size
                // is pinned); don't advertise a resize that won't happen.
                if !b.resizable {
                    c.arrow
                } else if b.dir == Dir::V {
                    c.v_resize
                } else {
                    c.h_resize
                }
            }
            Hit::Edge(_) => c.h_resize,
            Hit::LeafBody(_) | Hit::Miss => c.arrow,
        }
    }

    /// Set the underlay's cursor, skipping the request when unchanged.
    fn set_underlay_cursor(&mut self, cursor: u32) -> R<()> {
        if self.cursors.current != cursor {
            self.cursors.current = cursor;
            self.conn.change_window_attributes(
                self.underlay,
                &ChangeWindowAttributesAux::new().cursor(cursor),
            )?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn on_button_release(&mut self, e: &ButtonReleaseEvent) -> R<()> {
        // Drags are button-1 gestures; a stray right/middle release mid-drag
        // must not end them.
        if e.detail != 1 {
            return Ok(());
        }
        self.drags.float = None;
        let dragged = self.drags.split.take().is_some();
        let edge_dragged = self.drags.edge.take().is_some();
        if dragged || edge_dragged {
            self.arrange()?;
        }
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

