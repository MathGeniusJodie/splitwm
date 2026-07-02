//! Event dispatch and input handling for the WM core: keyboard, pointer,
//! scroll coalescing, and the client map/unmap/destroy protocol events.

use x11rb::protocol::xinput;
use x11rb::protocol::xproto::{
    Allow, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux, ConfigureNotifyEvent,
    ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt, ExposeEvent, InputFocus,
    KeyPressEvent, MapRequestEvent,
    Mapping, MappingNotifyEvent, ModMask, MotionNotifyEvent, UnmapNotifyEvent,
};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::CURRENT_TIME;

use super::clients::WM_STATE_WITHDRAWN;
use super::types::{rect_contains, Action, BtnKind, Drag, EdgeDrag, FrameRect, Wm, MOD4, R};
use crate::theme;
use crate::tree::{Dir, NodeId, Rect};

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
        let px = (delta * f64::from(theme::SCROLL_STEP)) as i32;
        if px == 0 {
            return Ok(());
        }
        self.state.scroll_delta(wa, px);
        self.state.scroll_x = self.state.scroll_target;
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
    fn hscroll_allowed(&mut self) -> R<bool> {
        let (last, allowed) = self.hscroll_gate;
        if last.elapsed() < std::time::Duration::from_millis(30) {
            return Ok(allowed);
        }
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let allowed =
            if p.child == x11rb::NONE || p.child == self.underlay || Some(p.child) == self.docked {
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
        self.hscroll_gate = (std::time::Instant::now(), allowed);
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
        let mut pending_motion: Option<MotionNotifyEvent> = None;
        // Raw scroll deltas from one batch are summed and applied as a single
        // scroll + arrange, for the same reason motion is coalesced: a swipe
        // reports far faster than we can recomposite the whole screen.
        let mut pending_hscroll = 0.0f64;
        for ev in batch {
            match ev {
                Event::MotionNotify(e) => {
                    pending_motion = Some(e);
                    continue;
                }
                Event::XinputRawMotion(ref e) => {
                    pending_hscroll += self.hscroll_delta(e);
                    continue;
                }
                // Legacy wheel-click compatibility events (buttons 4-7):
                // libinput synthesizes one of these alongside every smooth
                // XI2 scroll report, for X clients that only understand the
                // old discrete-click protocol. We scroll from the raw axis
                // instead, so these carry no information for us — but until
                // ignored here, each one forced a flush of the accumulated
                // scroll delta, defeating the coalescing above (a burst of
                // N scroll reports meant N clicks meant N full recomposites,
                // which is what "piling up" actually was).
                Event::ButtonPress(e) if (4..=7).contains(&e.detail) => continue,
                Event::ButtonRelease(e) if (4..=7).contains(&e.detail) => continue,
                _ => {}
            }
            // Flush pending motion/scroll before any other event so ordering
            // (e.g. a button release ending a drag) is preserved.
            if let Some(m) = pending_motion.take() {
                self.on_motion(&m)?;
            }
            if pending_hscroll != 0.0 {
                if self.debug_scroll {
                    eprintln!("splitwm: mid-batch flush forced by {ev:?}");
                }
                self.apply_hscroll(std::mem::take(&mut pending_hscroll))?;
            }
            self.handle_event(&ev)?;
        }
        if let Some(m) = pending_motion.take() {
            self.on_motion(&m)?;
        }
        if pending_hscroll != 0.0 {
            self.apply_hscroll(pending_hscroll)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Event) -> R<()> {
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
            // Keyboard layout / modifier mapping changed: rebind everything.
            Event::MappingNotify(e) => self.on_mapping(e)?,
            // Device hotplug: rebuild the horizontal-scroll device map.
            Event::XinputHierarchy(_) => self.build_hscroll_map()?,
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

    fn on_configure_request(&self, e: &ConfigureRequestEvent) -> R<()> {
        // Honour requests for windows we don't (yet) manage; managed clients
        // are positioned by arrange().
        if self.clients.contains_key(&e.window) {
            return Ok(());
        }
        let aux = ConfigureWindowAux::from_configure_request(e);
        self.conn.configure_window(e.window, &aux)?;
        Ok(())
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
                .width(u32::try_from(w.max(1)).unwrap_or(1))
                .height(u32::try_from(h.max(1)).unwrap_or(1)),
        )?;
        self.set_wallpaper();
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

    fn on_map_request(&mut self, e: MapRequestEvent) -> R<()> {
        if self.clients.contains_key(&e.window) {
            // A map request for a window we manage but have hidden is the
            // ICCCM deiconify request (Iconic -> Normal): bring it into a
            // split rather than blindly mapping it over the layout.
            if !self.state.activate_client(e.window) {
                let leaf = self.state.focused_leaf_valid();
                self.state.assign_to_leaf(e.window, leaf);
            }
            let wa = self.la();
            self.state.ensure_in_view(wa);
            self.state.scroll_x = self.state.scroll_target;
            self.arrange()?;
            self.focus(Some(e.window))?;
            return Ok(());
        }
        self.manage(e.window, false)?;
        Ok(())
    }

    /// A window was unmapped. Layout hiding accounts for its own unmaps in
    /// `ignore_unmaps`; anything beyond that is the client withdrawing
    /// itself (ICCCM), which unmanages it — the old no-op here meant a
    /// withdrawn window got forcibly re-mapped by the next arrange.
    fn on_unmap(&mut self, e: &UnmapNotifyEvent) -> R<()> {
        // Each unmap of a client is delivered twice: once via the root's
        // SubstructureNotify and once via the client's own StructureNotify
        // mask. Act only on the root copy (which is also where ICCCM
        // synthetic withdraw notifications arrive) so nothing double-fires.
        if e.event != self.root {
            return Ok(());
        }
        let win = e.window;
        if let Some(n) = self.ignore_unmaps.get_mut(&win) {
            *n -= 1;
            if *n == 0 {
                self.ignore_unmaps.remove(&win);
            }
            return Ok(());
        }
        if self.docked == Some(win) {
            self.docked = None;
            self.docked_w = 0;
            return self.arrange();
        }
        if self.clients.contains_key(&win) {
            self.set_wm_state(win, WM_STATE_WITHDRAWN)?;
            self.forget_client(win)?;
        }
        Ok(())
    }

    fn on_destroy(&mut self, win: u32) -> R<()> {
        if self.docked == Some(win) {
            self.docked = None;
            self.docked_w = 0;
            return self.arrange();
        }
        self.forget_client(win)?;
        Ok(())
    }

    fn on_key(&mut self, e: KeyPressEvent) -> R<()> {
        let Some(action) = self.lookup_action(e.state.into(), e.detail) else {
            return Ok(());
        };
        let wa = self.la();
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
        match action {
            Action::SpawnTerminal => self.spawn_terminal(),
            Action::SplitH => self.state.split_focused(Dir::H),
            Action::SplitV => self.state.split_focused(Dir::V),
            Action::Close => {
                self.state.close_focused();
            }
            Action::FocusNext => {
                self.state.focus_direction(true);
            }
            Action::FocusPrev => {
                self.state.focus_direction(false);
            }
            Action::NextTab => {
                self.state.cycle_taskbar(1);
            }
            Action::PrevTab => {
                self.state.cycle_taskbar(-1);
            }
            Action::MoveTabNext => {
                self.state.move_tab_to_direction(true);
            }
            Action::MoveTabPrev => {
                self.state.move_tab_to_direction(false);
            }
            Action::Grow => {
                self.state.resize_focused(theme::RESIZE_STEP);
            }
            Action::Shrink => {
                self.state.resize_focused(-theme::RESIZE_STEP);
            }
            Action::CloseWindow => {
                if let Some(c) = self.state.focused_client() {
                    self.close_client(c)?;
                }
            }
            Action::Quit => {
                self.running = false;
                return Ok(());
            }
        }
        if let Some(rect) = pre_split {
            self.prev_frame_rect
                .insert(self.state.focused_leaf_valid(), rect);
        }
        self.state.ensure_in_view(wa);
        self.state.scroll_x = self.state.scroll_target;
        self.arrange()?;
        let f = self.state.focused_client();
        self.focus(f)?;
        Ok(())
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
                self.state.focused_leaf = leaf;
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
                self.state.focused_leaf = leaf;
                self.state.close_focused();
                self.animate = true;
            }
            BtnKind::Minimize => {
                if meta.parent_dir.is_none() {
                    return Ok(());
                }
                self.state.toggle_minimize(leaf);
                self.animate = true;
            }
        }
        self.state.ensure_in_view(wa);
        self.state.scroll_x = self.state.scroll_target;
        self.arrange()?;
        let f = self.state.focused_client();
        self.focus(f)?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn on_button(&mut self, e: ButtonPressEvent) -> R<()> {
        let wa = self.la();
        // Clicks inside the launcher menu select an item; clicks elsewhere
        // dismiss it before falling through to normal handling.
        if self.menu.open {
            if e.event == self.menu.main_win || e.event == self.menu.sub_win {
                if e.detail == 1 {
                    self.on_menu_click(e.event, i32::from(e.event_y))?;
                }
                return Ok(());
            }
            self.close_menu()?;
        }
        // Split-control buttons (left = primary, right = opposite split dir).
        if (e.detail == 1 || e.detail == 3) && e.event == self.underlay {
            let (mx, my) = (i32::from(e.event_x), i32::from(e.event_y));
            if let Some((leaf, kind)) = self
                .btn_regions
                .iter()
                .find(|(r, _, _)| rect_contains(*r, mx, my))
                .map(|(_, l, k)| (*l, *k))
            {
                return self.click_split_button(leaf, kind, e.detail == 3);
            }
        }
        // Clicks on the underlay (gaps): hit-test drag handles and "+" buttons.
        if e.detail == 1 && e.event == self.underlay {
            let (mx, my) = (i32::from(e.event_x), i32::from(e.event_y));
            // The corner "x" badge on a bottom-bar tile: politely close that
            // window (checked before the tile itself so the badge wins).
            if let Some(win) = self
                .taskbar_regions
                .iter()
                .find(|t| rect_contains(t.close, mx, my))
                .map(|t| t.win)
            {
                return self.close_client(win);
            }
            // A bottom-bar icon: focus its split if already on-screen,
            // otherwise bring that window into the focused split.
            if let Some(win) = self
                .taskbar_regions
                .iter()
                .find(|t| rect_contains(t.rect, mx, my))
                .map(|t| t.win)
            {
                if self.state.activate_client(win) {
                    self.arrange()?;
                    self.focus(Some(win))?;
                    return Ok(());
                }
                let leaf = self.state.focused_leaf_valid();
                self.state.assign_to_leaf(win, leaf);
                self.animate = true;
                self.arrange()?;
                let f = self.state.focused_client();
                self.focus(f)?;
                return Ok(());
            }
            // Taskbar "+" opens the app launcher into the focused split.
            if rect_contains(self.taskbar_plus, mx, my) {
                let leaf = self.state.focused_leaf_valid();
                let pr = self.taskbar_plus;
                return self.open_menu(leaf, pr.x + pr.w, pr.y);
            }
            // Click the title (tab) to focus it.
            if let Some(leaf) = self
                .tab_regions
                .iter()
                .find(|(r, _)| rect_contains(*r, mx, my))
                .map(|(_, l)| *l)
            {
                let client = self.state.tree.leaf(leaf).and_then(|l| l.client);
                if let Some(c) = client {
                    self.state.focused_leaf = leaf;
                    self.arrange()?;
                    self.focus(Some(c))?;
                }
                return Ok(());
            }
            // The boundary "+" button sits centred inside its drag handle's
            // hit region (the handle spans the full boundary height so it's
            // easy to grab; "+" is a small target in its middle) — check the
            // narrower "+" rect first so it isn't shadowed by the handle.
            if let Some(at) = self
                .plus_regions
                .iter()
                .find(|(r, _)| rect_contains(*r, mx, my))
                .map(|(_, at)| *at)
            {
                self.state.insert_at_root(at);
                self.animate = true;
                self.state.ensure_in_view(wa);
                self.state.scroll_x = self.state.scroll_target;
                self.arrange()?;
                let f = self.state.focused_client();
                self.focus(f)?;
                return Ok(());
            }
            if let Some(b) = self
                .handle_regions
                .iter()
                .find(|(r, _)| rect_contains(*r, mx, my))
                .map(|(_, b)| *b)
            {
                self.drag = Some(Drag {
                    parent: b.parent,
                    idx: b.idx,
                    vertical: b.dir == Dir::V,
                    start: b.start,
                    combined: b.first + b.second,
                    gap: theme::GAP,
                });
                return Ok(());
            }
            // Outer canvas-edge resize handles: the screen-space x of
            // whichever end of the leftmost/rightmost column isn't being
            // dragged stays fixed for the whole gesture (see `EdgeDrag`).
            if let Some(&(_, left)) = self
                .edge_handle_regions
                .iter()
                .find(|(r, _)| rect_contains(*r, mx, my))
            {
                if let Some((start_x, w)) = self.state.edge_span(wa, left) {
                    let canvas_anchor = if left { start_x + w } else { start_x };
                    let anchor_x = canvas_anchor - self.state.scroll_x;
                    self.edge_drag = Some(EdgeDrag { left, anchor_x });
                }
                return Ok(());
            }
            // Clicking an empty split's body (no client window catches it):
            // focus that leaf.
            if let Some(leaf) = self
                .prev_frame_rect
                .iter()
                .find(|(l, r)| self.state.tree.is_leaf(**l) && rect_contains(**r, mx, my))
                .map(|(l, _)| *l)
            {
                self.state.focused_leaf = leaf;
                self.arrange()?;
                self.focus(self.state.focused_client())?;
            }
            return Ok(());
        }
        // Click-to-focus on a client window.
        if e.detail == 1 {
            if self.clients.contains_key(&e.event) {
                self.state.activate_client(e.event);
                self.arrange()?;
                self.focus(Some(e.event))?;
            } else if self.docked == Some(e.event) {
                // Outside the tree/`clients`, so `focus()` (which only knows
                // tiled windows) can't take it; set input focus directly.
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, e.event, CURRENT_TIME)?;
            }
            // Replay so the click reaches the app.
            self.conn
                .allow_events(Allow::REPLAY_POINTER, CURRENT_TIME)?;
        }
        Ok(())
    }

    fn on_motion(&mut self, e: &MotionNotifyEvent) -> R<()> {
        if self.menu.open && (e.event == self.menu.main_win || e.event == self.menu.sub_win) {
            return self.on_menu_motion(e.event, i32::from(e.event_y));
        }
        if let Some(ed) = self.edge_drag {
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
                self.state.scroll_x += applied;
                self.state.scroll_target += applied;
            }
            self.arrange()?;
            return Ok(());
        }
        let Some(d) = self.drag else {
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
            i32::from(e.root_x) + self.state.scroll_x
        };
        let new_first = canvas_pos - d.start - d.gap / 2;
        let frac = f64::from(new_first) / f64::from(d.combined);
        self.state.resize_boundary(d.parent, d.idx, frac);
        self.arrange()?;
        Ok(())
    }

    /// Pick the pointer cursor for a hover position on the underlay:
    /// resize arrows over gap/edge drag handles, the "disabled" cursor over
    /// a disabled titlebar button, the plain arrow otherwise.
    fn hover_cursor(&self, mx: i32, my: i32) -> u32 {
        let c = self.cursors;
        if let Some((leaf, kind)) = self
            .btn_regions
            .iter()
            .find(|(r, _, _)| rect_contains(*r, mx, my))
            .map(|(_, l, k)| (*l, *k))
        {
            // Mirror `compose`'s enabled/disabled choice for the button art
            // (a minimized leaf's whole-frame region is always a live
            // restore button).
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
            return c.arrow;
        }
        if let Some((_, b)) = self
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            // The boundary "+" button sits inside the handle's hit region;
            // keep the arrow over it, matching the click hit-test order.
            if b.root && self.plus_regions.iter().any(|(r, _)| rect_contains(*r, mx, my)) {
                return c.arrow;
            }
            return if b.dir == Dir::V {
                c.v_resize
            } else {
                c.h_resize
            };
        }
        if self
            .edge_handle_regions
            .iter()
            .any(|(r, _)| rect_contains(*r, mx, my))
        {
            return c.h_resize;
        }
        c.arrow
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

    fn on_button_release(&mut self, _e: &ButtonReleaseEvent) -> R<()> {
        let dragged = self.drag.take().is_some();
        let edge_dragged = self.edge_drag.take().is_some();
        if dragged || edge_dragged {
            self.arrange()?;
        }
        Ok(())
    }

    fn on_expose(&mut self, e: ExposeEvent) -> R<()> {
        // The underlay needs no handling here: its composited image is its
        // `background_pixmap`, so the server repaints exposed areas itself.
        if self.menu.open && e.window == self.menu.main_win {
            self.paint_menu_main()?;
        } else if self.menu.open && e.window == self.menu.sub_win {
            self.paint_menu_sub()?;
        }
        Ok(())
    }
}
