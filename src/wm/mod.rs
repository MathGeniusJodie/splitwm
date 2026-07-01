//! X11 window-manager core: become WM, manage clients in per-leaf frame
//! windows, run keybindings, drive the splitwm layout + renderer.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

mod menu;
mod types;
mod widgets;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Allow, AtomEnum, ButtonIndex, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux,
    ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, ExposeEvent, GrabMode, ImageFormat, InputFocus, KeyPressEvent, MapRequestEvent,
    ModMask, StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::CURRENT_TIME;

use crate::render::{Icon, LeafView, Renderer, TabInfo, TaskItem};
use crate::state::State;
use crate::theme;
use crate::tree::{Dir, Node, NodeId, Rect, Win};

pub use types::*;

#[allow(clippy::too_many_lines)]
pub fn run() -> R<()> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;

    // Become the window manager.
    let mask =
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::BUTTON_PRESS;
    let change = ChangeWindowAttributesAux::new().event_mask(mask);
    conn.change_window_attributes(root, &change)?
        .check()
        .map_err(|_| "another window manager is already running")?;

    // Black root background + a normal left-pointer cursor. Without setting a
    // root cursor the pointer is invisible over the root and the underlay
    // (which inherits the root's cursor), so give it the standard arrow from
    // the "cursor" font (glyph 68 = XC_left_ptr, 69 = its mask).
    let cursor_font = conn.generate_id()?;
    conn.open_font(cursor_font, b"cursor")?;
    let cursor = conn.generate_id()?;
    conn.create_glyph_cursor(
        cursor,
        cursor_font,
        cursor_font,
        68,
        69,
        0,
        0,
        0,
        0xffff,
        0xffff,
        0xffff,
    )?;
    conn.close_font(cursor_font)?;
    let cw = ChangeWindowAttributesAux::new()
        .background_pixel(screen.black_pixel)
        .cursor(cursor);
    conn.change_window_attributes(root, &cw)?;
    conn.clear_area(false, root, 0, 0, 0, 0)?;

    // Conservative cap for chunking PutImage (X core caps near 256 KiB).
    let max_req_bytes = 200_000usize;

    // One graphics context, reused for every frame blit (all frames share the
    // root's depth/visual, so a single GC created on root works for all).
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new())?;

    let atom_net_wm_icon = conn
        .intern_atom(false, b"_NET_WM_ICON")?
        .reply()
        .map(|r| r.atom)
        .unwrap_or(0);

    // Single full-screen underlay window: wallpaper + all leaf chrome + drag
    // handles + "+" buttons are composited onto it, below every client.
    let geo = conn.get_geometry(root)?.reply()?;
    let underlay = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        underlay,
        root,
        0,
        0,
        geo.width,
        geo.height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::BUTTON1_MOTION,
            ),
    )?;
    conn.map_window(underlay)?;
    // Keep the underlay pinned at the bottom of the stack.
    conn.configure_window(
        underlay,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    )?;
    // Deliver button1 (and the drag's motion/release) even over the underlay.
    conn.grab_button(
        true,
        underlay,
        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON1_MOTION,
        GrabMode::ASYNC,
        GrabMode::ASYNC,
        x11rb::NONE,
        x11rb::NONE,
        ButtonIndex::M1,
        ModMask::ANY,
    )?;

    // Override-redirect popup windows for the app launcher menu (main column +
    // one submenu). Created hidden; mapped/moved on demand. POINTER_MOTION
    // drives hover, BUTTON_PRESS selection, EXPOSURE repaints.
    let menu_mask = EventMask::EXPOSURE
        | EventMask::BUTTON_PRESS
        | EventMask::POINTER_MOTION
        | EventMask::LEAVE_WINDOW;
    let (menu_main, menu_sub) = (conn.generate_id()?, conn.generate_id()?);
    for mw in [menu_main, menu_sub] {
        conn.create_window(
            screen.root_depth,
            mw,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(screen.black_pixel)
                .event_mask(menu_mask),
        )?;
    }
    let menu = MenuUi {
        tree: crate::menu::build(),
        main_win: menu_main,
        sub_win: menu_sub,
        open: false,
        main: FrameRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        },
        main_cw: 0,
        main_hi: None,
        open_cat: None,
        sub_cw: 0,
        sub_hi: None,
        target_leaf: NodeId::default(),
    };

    let mut wm = Wm {
        depth: screen.root_depth,
        gc,
        keymap: HashMap::new(),
        bindings: Vec::new(),
        renderer: Renderer::new(),
        state: State::new(),
        clients: HashMap::new(),
        bar_order: Vec::new(),
        underlay,
        running: true,
        max_req_bytes,
        atom_net_wm_icon,
        animate: false,
        prev_frame_rect: HashMap::new(),
        handle_regions: Vec::new(),
        plus_regions: Vec::new(),
        taskbar_plus: FrameRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        tab_regions: Vec::new(),
        taskbar_regions: Vec::new(),
        btn_regions: Vec::new(),
        menu,
        drag: None,
        bgrx: RefCell::new(Vec::new()),
        conn,
        root,
    };

    wm.build_keymap()?;
    wm.grab_keys()?;
    wm.set_wallpaper();
    // Mod4 + wheel scrolls the canvas horizontally.
    for btn in [ButtonIndex::M4, ButtonIndex::M5] {
        wm.conn.grab_button(
            true,
            root,
            EventMask::BUTTON_PRESS,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            btn,
            ModMask::M4,
        )?;
    }

    wm.conn.flush()?;
    wm.arrange()?;

    while wm.running {
        wm.conn.flush()?;
        // Collect a whole batch: block for one event, then drain everything the
        // server already has queued. Motion events that arrive faster than we
        // can render are coalesced (see handle_batch) so renders never pile up.
        let mut batch = vec![wm.conn.wait_for_event()?];
        while let Some(ev) = wm.conn.poll_for_event()? {
            batch.push(ev);
        }
        wm.handle_batch(batch)?;
    }
    Ok(())
}

impl Wm {
    /// Load the wallpaper (env `SPLITWM_WALLPAPER`) into the renderer; it is
    /// composited onto the underlay each arrange. No-op if unset/unreadable.
    fn set_wallpaper(&mut self) {
        if let Ok(path) = std::env::var("SPLITWM_WALLPAPER") {
            let wa = self.wa();
            self.renderer.set_wallpaper(&path, wa.w, wa.h);
        }
    }

    fn wa(&self) -> Rect {
        workarea(&self.conn, self.root).unwrap_or(Rect {
            x: 0,
            y: 0,
            w: 1280,
            h: 800,
        })
    }

    /// Height reserved at the bottom for the window bar. Always present so the
    /// launcher "+" at its right edge is reachable even with no windows open.
    const fn taskbar_h() -> i32 {
        theme::TASKBAR_H
    }

    /// The split-layout area: the workarea minus the bottom taskbar strip.
    fn la(&self) -> Rect {
        let wa = self.wa();
        Rect {
            h: (wa.h - Self::taskbar_h()).max(1),
            ..wa
        }
    }

    // --- keyboard ---

    fn build_keymap(&mut self) -> R<()> {
        let setup = self.conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max - min + 1;
        let mapping = self.conn.get_keyboard_mapping(min, count)?.reply()?;
        let per = mapping.keysyms_per_keycode as usize;
        for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
            let keycode = min + i as u8;
            for &sym in chunk {
                if sym != 0 {
                    self.keymap.entry(sym).or_insert(keycode);
                }
            }
        }
        Ok(())
    }

    fn grab_keys(&mut self) -> R<()> {
        let shift = u16::from(ModMask::SHIFT);
        let defs: &[(u16, u32, Action)] = &[
            (MOD4, ks::RETURN, Action::SpawnTerminal),
            (MOD4, ks::V, Action::SplitH),
            (MOD4, ks::H, Action::SplitV),
            (MOD4, ks::Q, Action::Close),
            (MOD4, ks::TAB, Action::FocusNext),
            (MOD4 | shift, ks::TAB, Action::FocusPrev),
            (MOD4, ks::RIGHT, Action::FocusNext),
            (MOD4, ks::LEFT, Action::FocusPrev),
            (MOD4, ks::BRACKETRIGHT, Action::NextTab),
            (MOD4, ks::BRACKETLEFT, Action::PrevTab),
            (MOD4 | shift, ks::BRACKETRIGHT, Action::MoveTabNext),
            (MOD4 | shift, ks::BRACKETLEFT, Action::MoveTabPrev),
            (MOD4, ks::L, Action::Grow),
            (MOD4 | shift, ks::L, Action::Shrink),
            (MOD4, ks::EQUAL, Action::Grow),
            (MOD4, ks::MINUS, Action::Shrink),
            (MOD4 | shift, ks::Q, Action::Quit),
            (MOD4 | shift, ks::C, Action::KillClient),
        ];
        // Also grab with Lock (CapsLock) and Mod2 (NumLock) variants.
        let extra = [
            0u16,
            u16::from(ModMask::LOCK),
            u16::from(ModMask::M2),
            u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
        ];
        for &(modmask, sym, action) in defs {
            if let Some(&kc) = self.keymap.get(&sym) {
                self.bindings.push((modmask, kc, action));
                for e in extra {
                    let m = ModMask::from(modmask | e);
                    self.conn
                        .grab_key(true, self.root, m, kc, GrabMode::ASYNC, GrabMode::ASYNC)?;
                }
            }
        }
        Ok(())
    }

    fn lookup_action(&self, modmask: u16, keycode: u8) -> Option<Action> {
        // Strip Lock/Mod2 before matching.
        let clean = modmask & !(u16::from(ModMask::LOCK) | u16::from(ModMask::M2));
        self.bindings
            .iter()
            .find(|(m, kc, _)| *m == clean && *kc == keycode)
            .map(|(_, _, a)| *a)
    }

    // --- event dispatch ---

    #[allow(clippy::needless_pass_by_value)]
    /// Process a drained batch of events, coalescing consecutive `MotionNotify`
    /// events down to the most recent one. A drag emits motion events far
    /// faster than a full-screen software recomposite can keep up with; without
    /// coalescing each one would queue its own `arrange()`, so renders pile up
    /// and the dragged boundary keeps sliding after the pointer has stopped.
    /// We only ever render the latest pointer position per batch.
    fn handle_batch(&mut self, batch: Vec<Event>) -> R<()> {
        let mut pending_motion: Option<x11rb::protocol::xproto::MotionNotifyEvent> = None;
        for ev in batch {
            if let Event::MotionNotify(e) = ev {
                pending_motion = Some(e);
                continue;
            }
            // Flush the pending motion before any non-motion event so ordering
            // (e.g. a button release ending a drag) is preserved.
            if let Some(m) = pending_motion.take() {
                self.on_motion(&m)?;
            }
            self.handle_event(&ev)?;
        }
        if let Some(m) = pending_motion.take() {
            self.on_motion(&m)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Event) -> R<()> {
        match ev {
            Event::MapRequest(e) => self.on_map_request(*e)?,
            Event::UnmapNotify(e) => self.on_unmap(e.window),
            Event::DestroyNotify(e) => self.on_destroy(e.window)?,
            Event::ConfigureRequest(e) => self.on_configure_request(e)?,
            Event::KeyPress(e) => self.on_key(*e)?,
            Event::ButtonPress(e) => self.on_button(*e)?,
            Event::ButtonRelease(e) => self.on_button_release(e)?,
            Event::MotionNotify(e) => self.on_motion(e)?,
            Event::Expose(e) => self.on_expose(*e)?,
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

    fn on_map_request(&mut self, e: MapRequestEvent) -> R<()> {
        if self.clients.contains_key(&e.window) {
            self.conn.map_window(e.window)?;
            return Ok(());
        }
        self.manage(e.window)?;
        Ok(())
    }

    fn manage(&mut self, win: Win) -> R<()> {
        // Class -> label; app icon from _NET_WM_ICON.
        let label = self.client_identity(win);
        let icon = self.fetch_icon(win);

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

        self.clients.insert(win, Client { label, icon });
        if !self.bar_order.contains(&win) {
            self.bar_order.push(win);
        }
        self.state.activate_client(win);
        self.arrange()?;
        self.focus(Some(win))?;
        Ok(())
    }

    fn client_identity(&self, win: Win) -> char {
        let class = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        // WM_CLASS is "instance\0class\0"; take the class (second string).
        let parts: Vec<&[u8]> = class.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
        let name = parts
            .get(1)
            .or_else(|| parts.first())
            .copied()
            .unwrap_or(b"?");
        name.first()
            .map_or('?', |&b| (b as char).to_ascii_uppercase())
    }

    /// Read `_NET_WM_ICON` and pick the icon whose size is closest to (but
    /// preferably >=) the tab height. The property is a list of
    /// `width, height, w*h ARGB pixels` blocks packed as 32-bit CARDINALs.
    fn fetch_icon(&self, win: Win) -> Option<Rc<Icon>> {
        if self.atom_net_wm_icon == 0 {
            return None;
        }
        let reply = self
            .conn
            .get_property(
                false,
                win,
                self.atom_net_wm_icon,
                AtomEnum::CARDINAL,
                0,
                u32::MAX,
            )
            .ok()?
            .reply()
            .ok()?;
        let vals: Vec<u32> = reply.value32()?.collect();
        let want = theme::tb_h(theme::GAP) as u32;
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
        Some(Rc::new(Icon { w, h, argb }))
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    const fn on_unmap(&self, _win: Win) {
        // We unmap off-screen windows and reparent clients ourselves, both of
        // which generate UnmapNotify. Distinguishing those from a real client
        // withdraw is fiddly, so we unmanage on DestroyNotify only.
    }

    fn on_destroy(&mut self, win: Win) -> R<()> {
        self.forget_client(win)?;
        Ok(())
    }

    fn forget_client(&mut self, win: Win) -> R<()> {
        if self.clients.remove(&win).is_none() {
            return Ok(());
        }
        self.bar_order.retain(|&w| w != win);
        self.state.unpin_client(win);
        // Keep focus inside the leaf the window lived in.
        self.arrange()?;
        let next = self.state.focused_client();
        self.focus(next)?;
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
            Action::KillClient => {
                if let Some(c) = self.state.focused_client() {
                    self.conn.kill_client(c)?;
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
            if let Some(b) = self
                .handle_regions
                .iter()
                .find(|(r, _)| rect_contains(*r, mx, my))
                .map(|(_, b)| *b)
            {
                self.drag = Some(Drag {
                    parent: b.parent,
                    idx: b.idx,
                    left_x: b.left_x,
                    combined: b.left_w + b.right_w,
                    gap: theme::GAP,
                });
                return Ok(());
            }
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
        if e.detail == 4 || e.detail == 5 {
            let dir = if e.detail == 4 { -1 } else { 1 };
            self.state.scroll_delta(wa, dir * theme::SCROLL_STEP);
            self.state.scroll_x = self.state.scroll_target;
            self.arrange()?;
            return Ok(());
        }
        // Click-to-focus on a client window.
        if e.detail == 1 {
            if self.clients.contains_key(&e.event) {
                self.state.activate_client(e.event);
                self.arrange()?;
                self.focus(Some(e.event))?;
            }
            // Replay so the click reaches the app.
            self.conn
                .allow_events(Allow::REPLAY_POINTER, CURRENT_TIME)?;
        }
        Ok(())
    }

    fn on_motion(&mut self, e: &x11rb::protocol::xproto::MotionNotifyEvent) -> R<()> {
        if self.menu.open && (e.event == self.menu.main_win || e.event == self.menu.sub_win) {
            return self.on_menu_motion(e.event, i32::from(e.event_y));
        }
        let Some(d) = self.drag else {
            return Ok(());
        };
        if d.combined <= 0 {
            return Ok(());
        }
        let canvas_mx = i32::from(e.root_x) + self.state.scroll_x;
        let new_left_w = canvas_mx - d.left_x - d.gap / 2;
        let frac = f64::from(new_left_w) / f64::from(d.combined);
        self.state.resize_boundary(d.parent, d.idx, frac);
        self.arrange()?;
        Ok(())
    }

    fn on_button_release(&mut self, _e: &ButtonReleaseEvent) -> R<()> {
        if self.drag.take().is_some() {
            self.arrange()?;
        }
        Ok(())
    }

    fn on_expose(&mut self, e: ExposeEvent) -> R<()> {
        // Recompose the underlay once the exposure run completes.
        if e.count == 0 && e.window == self.underlay {
            self.arrange()?;
        } else if self.menu.open && e.window == self.menu.main_win {
            self.paint_menu_main()?;
        } else if self.menu.open && e.window == self.menu.sub_win {
            self.paint_menu_sub()?;
        }
        Ok(())
    }

    #[allow(clippy::unused_self)]
    fn spawn_terminal(&self) {
        let term = std::env::var("TERMINAL").unwrap_or_else(|_| "xterm".into());
        // Detach so children don't become zombies / die with us.
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{term} &"))
            .spawn();
    }

    /// Spawn an arbitrary shell command, detached.
    #[allow(clippy::unused_self)]
    pub(crate) fn spawn(&self, cmd: &str) {
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{cmd} &"))
            .spawn();
    }

    // --- focus ---

    fn focus(&self, win: Option<Win>) -> R<()> {
        match win {
            Some(w) if self.clients.contains_key(&w) => {
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, w, CURRENT_TIME)?;
                self.conn
                    .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
            }
            _ => {
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
        }
        Ok(())
    }

    // --- arrange: compose the underlay + place clients ---

    fn arrange(&mut self) -> R<()> {
        let wa = self.la();
        let gap = theme::GAP;

        // Grow the canvas if the tree is wider than the viewport: root-level
        // horizontal branches accumulate width as they gain children. We give
        // each leaf a comfortable minimum so splits don't get crushed.
        let leaves = self.state.tree.collect_leaves();
        let min_leaf_w = (theme::min_split_w() + 2 * gap).max(wa.w / 3);
        let needed = i32::try_from(leaves.len()).unwrap_or(i32::MAX) * min_leaf_w;
        self.state.canvas_w = Some(needed.max(wa.w));

        let geos = self.state.compute(wa);
        let scroll_x = self.state.scroll_x;
        let focused = self.state.focused_leaf_valid();

        // Screen-space chrome rect for every on-screen leaf.
        let mut placed: Vec<Placement> = Vec::new();
        for &leaf in &leaves {
            let Some(geo) = geos.get(&leaf).copied() else {
                continue;
            };
            let target = FrameRect {
                x: geo.x - scroll_x,
                y: geo.y,
                w: geo.w.max(1),
                h: geo.h.max(1),
            };
            if target.x + target.w <= wa.x || target.x >= wa.x + wa.w {
                continue;
            }
            let active_client = self.state.tree.leaf(leaf).and_then(|l| l.client);
            placed.push(Placement {
                leaf,
                target,
                active_client,
                focused: focused == leaf,
            });
        }

        // Drag-handle / "+" / tab hit-regions for the current layout.
        self.compute_widgets(wa, &placed);
        self.compute_taskbar();

        if std::mem::take(&mut self.animate) {
            self.run_layout_animation(wa, &placed)?;
        }
        self.compose(wa, &placed, true)?;
        self.place_clients(&placed)?;
        // place_clients raised every client to the top; keep an open launcher
        // menu above them (an arrange can be triggered while it's open).
        self.raise_menu()?;

        // Cache final rects as the start point for the next transition.
        self.prev_frame_rect = placed.iter().map(|p| (p.leaf, p.target)).collect();
        self.conn.flush()?;
        Ok(())
    }

    /// Composite the wallpaper, every placed leaf's chrome, and (optionally)
    /// the drag handles / "+" buttons onto the single underlay window.
    fn compose(&self, _layout: Rect, placed: &[Placement], widgets: bool) -> R<()> {
        use crate::render::BtnIcon as I;
        // The underlay (and base pixmap) always cover the full screen, even
        // though the split layout only uses the area above the taskbar.
        let wa = self.wa();
        let (w, h) = (wa.w.max(1) as u32, wa.h.max(1) as u32);
        let mut pm = self.renderer.screen_base(w, h);
        {
            let mut m = pm.as_mut();
            for p in placed {
                let view = self.leaf_view(p.leaf, p.target.w, p.target.h);
                self.renderer
                    .draw_leaf(&mut m, p.target.x as f32, p.target.y as f32, &view);
            }
            if widgets {
                for (r, _) in &self.plus_regions {
                    let cx = (r.x + r.w / 2) as f32;
                    let cy = (r.y + r.h / 2) as f32;
                    crate::render::draw_plus(&mut m, cx, cy, r.w as f32);
                }
                // Split-control buttons. Look each leaf's final frame up so the
                // icon/enabled state matches the post-arrange geometry.
                // `btn_regions` has up to 3 entries per leaf (close/split/
                // minimize); `leaf_meta` does a linear parent scan, so it's
                // computed once per leaf here rather than once per region.
                let metas: HashMap<NodeId, LeafMeta> = placed
                    .iter()
                    .map(|p| (p.leaf, self.leaf_meta(p.leaf, p.target)))
                    .collect();
                for (r, leaf, kind) in &self.btn_regions {
                    let Some(&meta) = metas.get(leaf) else {
                        continue;
                    };
                    // A minimized leaf's region is the whole frame (a single
                    // restore button); `draw_leaf`'s winmin.png already shows
                    // it, so no button glyph is drawn on top.
                    if meta.minimized {
                        continue;
                    }
                    let (icon, disabled) = match kind {
                        // A V-branch parent means this leaf collapses to a
                        // row (short/wide) when minimized, so its button
                        // previews that with the horizontal glyph.
                        BtnKind::Minimize => (
                            if meta.parent_dir == Some(Dir::V) {
                                I::MinimizeH
                            } else {
                                I::Minimize
                            },
                            meta.parent_dir.is_none(),
                        ),
                        BtnKind::Split => (
                            if meta.wider { I::VSplit } else { I::HSplit },
                            !meta.can_split,
                        ),
                        BtnKind::Close => (I::Close, meta.parent_dir.is_none()),
                    };
                    let cx = (r.x + r.w / 2) as f32;
                    let cy = (r.y + r.h / 2) as f32;
                    self.renderer.draw_button(
                        &mut m,
                        cx,
                        cy,
                        theme::BTN_SIZE as f32,
                        icon,
                        disabled,
                        self.leaf_color_index(*leaf),
                    );
                }
            }
            // Bottom bar: one tile per managed window; split-visible windows
            // get an accent highlight box.
            for t in &self.taskbar_regions {
                let client = self.clients.get(&t.win);
                self.renderer.draw_taskbar_item(
                    &mut m,
                    TaskItem {
                        x: t.rect.x as f32,
                        y: t.rect.y as f32,
                        w: t.rect.w as f32,
                        h: t.rect.h as f32,
                    },
                    client.and_then(|c| c.icon.as_deref()),
                    client.map_or('?', |c| c.label),
                    t.accent,
                    t.on_screen,
                );
            }
            // Launcher "+" at the right end of the bar.
            let pr = self.taskbar_plus;
            crate::render::draw_plus(
                &mut m,
                (pr.x + pr.w / 2) as f32,
                (pr.y + pr.h / 2) as f32,
                pr.w as f32,
            );
        }
        let mut buf = self.bgrx.borrow_mut();
        crate::render::pixmap_to_bgrx(&pm, &mut buf);
        self.put_image(self.underlay, w as u16, h as u16, &buf)?;
        Ok(())
    }

    /// Each split's persistent accent, stored on the leaf so it survives splits
    /// and closes; colours the tab bar and the bottom-bar highlight.
    pub(crate) fn leaf_color(&self, leaf: NodeId) -> u32 {
        self.renderer.accent_rgb(self.leaf_color_index(leaf))
    }

    /// The raw palette index behind `leaf_color`, used to palette-swap the
    /// bitmap window border to this leaf's accent.
    pub(crate) fn leaf_color_index(&self, leaf: NodeId) -> crate::Index {
        self.state
            .tree
            .leaf(leaf)
            .map_or(theme::FALLBACK_ACCENT_INDEX, |l| l.color)
    }

    fn leaf_view(&self, leaf: NodeId, w: i32, h: i32) -> LeafView {
        let win = self.state.tree.leaf(leaf).and_then(|l| l.client);
        let client = win.and_then(|w| self.clients.get(&w));
        let accent_index = self.leaf_color_index(leaf);
        let tab = client.map(|c| TabInfo {
            label: c.label,
            icon: c.icon.clone(),
        });
        LeafView {
            w,
            h,
            tb_h: theme::tb_h(theme::GAP),
            bw: theme::BORDER_LEFT,
            accent_index,
            tab,
            minimized: self.state.tree.leaf(leaf).is_some_and(|l| l.minimized),
        }
    }

    /// Position each split's window below its title bar; unmap the rest.
    fn place_clients(&self, placed: &[Placement]) -> R<()> {
        let gap = theme::GAP;
        let tb_h = theme::tb_h(gap);
        let bw = theme::BORDER_LEFT;
        let mut visible: std::collections::HashSet<Win> = std::collections::HashSet::new();
        for p in placed {
            let minimized = self.state.tree.leaf(p.leaf).is_some_and(|l| l.minimized);
            if let Some(c) = p.active_client {
                if minimized {
                    continue;
                }
                let r = p.target;
                let cw = (r.w - 2 * bw).max(1);
                let ch = (r.h - tb_h - bw).max(1);
                self.conn.configure_window(
                    c,
                    &ConfigureWindowAux::new()
                        .x(r.x + bw)
                        .y(r.y + tb_h)
                        .width(u32::try_from(cw).unwrap_or(1))
                        .height(u32::try_from(ch).unwrap_or(1))
                        .border_width(0)
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.map_window(c)?;
                visible.insert(c);
            }
        }
        for &w in self.clients.keys() {
            if !visible.contains(&w) {
                self.conn.unmap_window(w)?;
            }
        }
        Ok(())
    }

    // --- gap drag handles & "+" insert buttons (composited on the underlay) ---

    const PLUS_SZ: i32 = 22;
    /// Total px trimmed off the gap to get the drag-handle pill width.
    const HANDLE_INSET: i32 = 10;

    /// A `PLUS_SZ`-square hit/draw rect centred horizontally on `vis_x`.
    const fn plus_rect(vis_x: i32, y: i32) -> FrameRect {
        FrameRect {
            x: vis_x - Self::PLUS_SZ / 2,
            y,
            w: Self::PLUS_SZ,
            h: Self::PLUS_SZ,
        }
    }

    /// Recompute the screen-space hit-regions for gap drag handles and "+"
    /// insert buttons. They are drawn by `compose`, not separate windows.
    /// Parent direction / split-eligibility metadata used to choose each
    /// split-control button's icon and enabled state.
    pub(crate) fn leaf_meta(&self, leaf: NodeId, frame: FrameRect) -> LeafMeta {
        let parent = self.state.tree.find_parent(leaf);
        let parent_dir = parent.and_then(|(p, _)| match self.state.tree.get(p) {
            Some(Node::Branch { dir, .. }) => Some(*dir),
            _ => None,
        });
        let gap = theme::GAP;
        let wider = frame.w >= frame.h;
        let can_v = frame.w >= 2 * theme::min_split_w() + gap;
        let can_h = frame.h >= 2 * theme::tb_h(gap) + gap;
        LeafMeta {
            parent_dir,
            wider,
            can_split: if wider { can_v } else { can_h },
            minimized: self.state.tree.leaf(leaf).is_some_and(|l| l.minimized),
        }
    }

    /// Animate the placed leaves from their previous rect (or a collapsed
    /// sliver, for freshly-created leaves) to their target with an
    /// ease-out-back curve, re-compositing the underlay each frame.
    ///
    /// Driven by wall-clock time, not a fixed frame count: each frame does a
    /// full-screen software recomposite + blit (not cheap), so we step by how
    /// much real time has elapsed and always finish in `DURATION`, ending
    /// exactly on the target. A slow renderer simply shows fewer frames.
    fn run_layout_animation(&self, wa: Rect, placed: &[Placement]) -> R<()> {
        use std::time::{Duration, Instant};
        const DURATION: Duration = Duration::from_millis(280);
        let starts: Vec<FrameRect> = placed
            .iter()
            .map(|p| {
                self.prev_frame_rect
                    .get(&p.leaf)
                    .copied()
                    .unwrap_or(FrameRect {
                        x: p.target.x,
                        y: p.target.y,
                        w: 1,
                        h: p.target.h,
                    })
            })
            .collect();
        let start = Instant::now();
        loop {
            let t = (start.elapsed().as_secs_f32() / DURATION.as_secs_f32()).min(1.0);
            let e = ease_out_back(t);
            let interp: Vec<Placement> = placed
                .iter()
                .zip(&starts)
                .map(|(p, s)| Placement {
                    leaf: p.leaf,
                    target: lerp_rect(*s, p.target, e),
                    active_client: p.active_client,
                    focused: p.focused,
                })
                .collect();
            self.compose(wa, &interp, false)?;
            self.place_clients(&interp)?;
            self.conn.flush()?;
            if t >= 1.0 {
                break;
            }
        }
        Ok(())
    }

    fn put_image(&self, drawable: Window, w: u16, h: u16, data: &[u8]) -> R<()> {
        let gc = self.gc;
        let stride = w as usize * 4;
        // Chunk by rows to stay under the maximum request length.
        let overhead = 64;
        let max_rows = (((self.max_req_bytes.saturating_sub(overhead)) / stride).max(1)) as u16;
        let mut y = 0u16;
        while y < h {
            let rows = max_rows.min(h - y);
            let start = y as usize * stride;
            let end = start + rows as usize * stride;
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                gc,
                w,
                rows,
                0,
                i16::try_from(y).unwrap_or(i16::MAX),
                0,
                self.depth,
                &data[start..end],
            )?;
            y += rows;
        }
        Ok(())
    }
}
