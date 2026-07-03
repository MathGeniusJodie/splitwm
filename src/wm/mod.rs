//! X11 window-manager core: claim the manager selection, become the WM, set
//! up input/EWMH plumbing, and run the event loop. Client lifecycle lives in
//! `clients`, event handling in `events`, layout/compositing in `arrange`.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

mod arrange;
mod clients;
mod events;
mod menu;
mod notes;
mod types;
mod widgets;

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _, ScrollType, XIEventMask};
use x11rb::protocol::render::{self, ConnectionExt as _, PictType, Pictformat};
use x11rb::protocol::xproto::{
    AtomEnum, ButtonIndex, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, GrabMode, ImageFormat, ImageOrder,
    ModMask, PropMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// Pseudo-device IDs accepted by `XISelectEvents`/`XIQueryDevice` meaning
/// "every device" and "every master (logical) pointer/keyboard pair".
const XI_ALL_DEVICES: u16 = 0;
const XI_ALL_MASTER_DEVICES: u16 = 1;

use crate::render::Renderer;
use crate::state::State;
use crate::theme;
use crate::tree::Rect;

pub use types::*;

fn fp3232_to_f64(v: xinput::Fp3232) -> f64 {
    f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0
}

/// Pick out the raw-event valuator numbered `number` from `axisvalues`,
/// which holds one entry per set bit in `mask` (in bit order, low to high,
/// spanning as many `u32` words as needed) — the wire format XInput2 raw
/// events use to report only the axes that moved.
fn valuator_value(mask: &[u32], axisvalues: &[xinput::Fp3232], number: u16) -> Option<f64> {
    let number = usize::from(number);
    let (word, bit) = (number / 32, number % 32);
    if mask.get(word).is_none_or(|w| w & (1 << bit) == 0) {
        return None;
    }
    let idx = mask
        .iter()
        .enumerate()
        .flat_map(|(w, &m)| (0..32).filter(move |b| w * 32 + b < number && m & (1 << b) != 0))
        .count();
    axisvalues.get(idx).copied().map(fp3232_to_f64)
}

/// Claim the ICCCM manager selection (`WM_S<n>`) for this screen before
/// grabbing `SUBSTRUCTURE_REDIRECT`, which only one client may hold at a
/// time. Plain startup (no existing owner) just registers ours. With
/// `--replace` and an existing owner, this waits for the outgoing WM to
/// notice it lost the selection and destroy its manager window — which is
/// also when it releases the redirect — before returning, so the
/// `SUBSTRUCTURE_REDIRECT` grab in `run` can succeed right after.
fn claim_manager_selection(
    conn: &x11rb::rust_connection::RustConnection,
    screen_num: usize,
    root: Window,
    screen: &x11rb::protocol::xproto::Screen,
    replace: bool,
) -> R<Window> {
    let wm_sn_atom = conn
        .intern_atom(false, format!("WM_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    let manager_atom = conn.intern_atom(false, b"MANAGER")?.reply()?.atom;

    // A tiny, never-mapped window that exists only to own the selection for
    // the rest of the process's life (kept alive, never destroyed).
    let sel_owner = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        sel_owner,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    // ICCCM wants a real timestamp, not CurrentTime, for SetSelectionOwner:
    // change a property on our own window and read the server's timestamp
    // back off the resulting PropertyNotify.
    conn.change_property(
        PropMode::REPLACE,
        sel_owner,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        8,
        7,
        b"splitwm",
    )?;
    conn.flush()?;
    let timestamp = loop {
        if let Event::PropertyNotify(e) = conn.wait_for_event()? {
            if e.window == sel_owner {
                break e.time;
            }
        }
    };

    let previous_owner = conn.get_selection_owner(wm_sn_atom)?.reply()?.owner;
    if previous_owner != x11rb::NONE {
        if !replace {
            return Err(
                "another window manager is already running (pass --replace to take over)".into(),
            );
        }
        // Watch for the outgoing WM's manager window going away, which is
        // how we know it actually relinquished control.
        conn.change_window_attributes(
            previous_owner,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
    }

    conn.set_selection_owner(sel_owner, wm_sn_atom, timestamp)?;
    if conn.get_selection_owner(wm_sn_atom)?.reply()?.owner != sel_owner {
        return Err("failed to acquire the WM_Sn manager selection".into());
    }

    if previous_owner != x11rb::NONE {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match conn.poll_for_event()? {
                Some(Event::DestroyNotify(e)) if e.window == previous_owner => break,
                _ if std::time::Instant::now() >= deadline => {
                    // Best-effort: the SUBSTRUCTURE_REDIRECT grab right
                    // after this call is the real gate, so proceed even if
                    // the old WM never confirmed.
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    // Announce the handover to anyone watching root for MANAGER messages
    // (panels/pagers use this to notice a WM switch).
    let manager_msg = ClientMessageEvent::new(
        32,
        root,
        manager_atom,
        [timestamp, wm_sn_atom, sel_owner, 0, 0],
    );
    conn.send_event(false, root, EventMask::STRUCTURE_NOTIFY, manager_msg)?;
    conn.flush()?;
    Ok(sel_owner)
}

#[allow(clippy::too_many_lines)]
pub fn run(replace: bool) -> R<()> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;

    let sel_owner = claim_manager_selection(&conn, screen_num, root, &screen, replace)?;

    // Become the window manager. STRUCTURE_NOTIFY is included so the root's
    // own ConfigureNotify reports screen (RandR) resizes.
    let mask = EventMask::SUBSTRUCTURE_REDIRECT
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::BUTTON_PRESS;
    let change = ChangeWindowAttributesAux::new().event_mask(mask);
    conn.change_window_attributes(root, &change)?
        .check()
        .map_err(|_| "another window manager is already running")?;

    let atoms = Atoms::intern(&conn)?;
    // Minimal EWMH presence: announce what we support, and point
    // _NET_SUPPORTING_WM_CHECK at the (never-mapped) selection-owner window
    // so pagers/panels recognise a live EWMH WM.
    conn.change_property32(
        PropMode::REPLACE,
        root,
        atoms.net_supported,
        AtomEnum::ATOM,
        &[
            atoms.net_supported,
            atoms.net_client_list,
            atoms.net_active_window,
            atoms.net_supporting_wm_check,
            atoms.net_wm_name,
            atoms.net_wm_icon,
            atoms.net_wm_window_type,
            atoms.net_wm_window_type_notification,
        ],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        root,
        atoms.net_supporting_wm_check,
        AtomEnum::WINDOW,
        &[sel_owner],
    )?;
    // Present-but-empty until the first client is managed.
    conn.change_property32(
        PropMode::REPLACE,
        root,
        atoms.net_client_list,
        AtomEnum::WINDOW,
        &[],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        sel_owner,
        atoms.net_supporting_wm_check,
        AtomEnum::WINDOW,
        &[sel_owner],
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        sel_owner,
        atoms.net_wm_name,
        atoms.utf8_string,
        b"splitwm",
    )?;

    // Black root background + a normal left-pointer cursor. Without setting a
    // root cursor the pointer is invisible over the root and the underlay
    // (which inherits the root's cursor). The arrow/hand/disabled cursors are
    // the hand-drawn `cursor_*` sprites, built as ARGB cursors via RENDER;
    // the core "cursor" font supplies the resize arrows (no drawn art) and
    // the fallbacks when the server lacks RENDER cursors (glyph 68 =
    // XC_left_ptr, 108 = XC_sb_h_double_arrow, 116 = XC_sb_v_double_arrow,
    // 60 = XC_hand2, 0 = XC_X_cursor; a glyph's mask is always the next
    // glyph).
    let cursor_font = conn.generate_id()?;
    conn.open_font(cursor_font, b"cursor")?;
    let make_cursor = |glyph: u16| -> R<u32> {
        let c = conn.generate_id()?;
        conn.create_glyph_cursor(
            c,
            cursor_font,
            cursor_font,
            glyph,
            glyph + 1,
            0,
            0,
            0,
            0xffff,
            0xffff,
            0xffff,
        )?;
        Ok(c)
    };
    let mut cursors = Cursors {
        arrow: make_cursor(68)?,
        h_resize: make_cursor(108)?,
        v_resize: make_cursor(116)?,
        disabled: make_cursor(0)?,
        hand: make_cursor(60)?,
        current: 0,
    };
    conn.close_font(cursor_font)?;
    if let Some(argb32) = render_argb32_format(&conn)? {
        let palette = crate::assets::palette();
        // Hotspots: arrow tip, fingertip, circle center.
        cursors.arrow =
            sprite_cursor(&conn, root, argb32, &crate::assets::cursor_pointer(), &palette, (4, 0))?;
        cursors.hand =
            sprite_cursor(&conn, root, argb32, &crate::assets::cursor_hand(), &palette, (11, 0))?;
        cursors.disabled = sprite_cursor(
            &conn,
            root,
            argb32,
            &crate::assets::cursor_disabled(),
            &palette,
            (12, 12),
        )?;
    }
    let cursor = cursors.arrow;
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

    // Single full-screen underlay window: wallpaper + all leaf chrome + drag
    // handles + "+" buttons are composited onto it, below every client.
    let geo = conn.get_geometry(root)?.reply()?;
    let workarea = Rect {
        x: 0,
        y: 0,
        w: i32::from(geo.width),
        h: i32::from(geo.height),
    };
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
                    | EventMask::BUTTON1_MOTION
                    // Hover motion drives the resize/disabled cursor feedback
                    // over drag handles and titlebar buttons.
                    | EventMask::POINTER_MOTION,
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
                // Every menu row is clickable, so the hand covers the menu.
                .cursor(cursors.hand)
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
        target_leaf: crate::tree::NodeId::default(),
        icon_cache: HashMap::new(),
        main_icons: Vec::new(),
        sub_icons: Vec::new(),
    };

    let debug_scroll = std::env::var_os("SPLITWM_DEBUG_SCROLL").is_some();

    // Become the session's notification daemon: the thread owns the D-Bus
    // connection and wakes our event loop with a `splitwm_note` ClientMessage
    // whenever the channel has something for us.
    let (note_tx, note_rx) = std::sync::mpsc::channel();
    let note_dismiss = crate::notify::spawn(note_tx);

    let mut wm = Wm {
        depth: screen.root_depth,
        gc,
        keymap: HashMap::new(),
        bindings: Vec::new(),
        renderer: Renderer::new(),
        state: State::new(),
        clients: HashMap::new(),
        bar_order: Vec::new(),
        docked: None,
        docked_w: 0,
        dock_title: std::env::var("SPLITWM_DOCK_TITLE")
            .unwrap_or_else(|_| theme::DOCK_TITLE.to_string()),
        notifications: Vec::new(),
        note_popups: Vec::new(),
        note_rx,
        note_dismiss,
        underlay,
        underlay_pix: 0,
        underlay_pix_size: (0, 0),
        sel_owner,
        running: true,
        max_req_bytes,
        atoms,
        workarea,
        wallpaper_path: std::env::var("SPLITWM_WALLPAPER").ok(),
        debug_scroll,
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
        edge_handle_regions: Vec::new(),
        edge_drag: None,
        cursors,
        bgrx: Vec::new(),
        hscroll: Vec::new(),
        hscroll_gate: (
            std::time::Instant::now() - std::time::Duration::from_secs(1),
            true,
        ),
        ignore_unmaps: HashMap::new(),
        conn,
        root,
    };

    wm.build_keymap()?;
    wm.grab_keys()?;
    wm.set_wallpaper();
    // Two-finger trackpad swipes (and any other horizontal-scroll-capable
    // device) report a smooth XInput2 scroll valuator; listen for its raw
    // motion globally so panning tracks the swipe instead of jumping in
    // fixed wheel-click steps. Selecting on root doesn't steal the events
    // from whichever client the pointer is over.
    let xi_version = wm.conn.xinput_xi_query_version(2, 1)?.reply()?;
    wm.build_hscroll_map()?;
    wm.conn
        .xinput_xi_select_events(
            root,
            &[
                xinput::EventMask {
                    deviceid: XI_ALL_MASTER_DEVICES,
                    mask: vec![XIEventMask::RAW_MOTION],
                },
                xinput::EventMask {
                    deviceid: XI_ALL_DEVICES,
                    mask: vec![XIEventMask::HIERARCHY],
                },
            ],
        )?
        .check()?;
    if wm.debug_scroll {
        eprintln!(
            "splitwm: XInput2 {}.{}, hscroll devices: {}",
            xi_version.major_version,
            xi_version.minor_version,
            wm.hscroll.len()
        );
    }

    // Take over from a previous WM (if any) without dropping whatever it had
    // on screen: adopt already-mapped windows before the first arrange.
    wm.manage_existing_windows()?;

    // Autostart the docked sidebar from its freedesktop entry (the desktop
    // id must match the dock title, e.g. cozyui.desktop), unless a previous
    // WM already handed a running one over.
    if wm.docked.is_none() {
        match crate::menu::desktop_entry_cmd(&wm.dock_title) {
            Some(cmd) => wm.spawn(&cmd),
            None => eprintln!(
                "splitwm: no {}.desktop entry found; not autostarting the dock",
                wm.dock_title
            ),
        }
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
        // A trackpad reports horizontal-scroll `XI_RawMotion` at up to a few
        // hundred Hz, often faster than the socket delivers them to us in one
        // go — without this, each report can land in its own batch and force
        // its own full-screen recomposite, and rendering falls behind the
        // swipe (a visible backlog that keeps "catching up" after the finger
        // stops). Give a fast burst a few ms to land in the same batch so one
        // recomposite picks up many reports' worth of delta at once.
        //
        // Gate on an actual scroll *delta*, not the mere presence of raw
        // motion: RAW_MOTION fires for every plain pointer movement too, and
        // sleeping on those would add 8 ms of input latency (and constant
        // wakeups) whenever the mouse moves.
        let scroll_delta: f64 = batch
            .iter()
            .filter_map(|e| match e {
                Event::XinputRawMotion(e) => Some(wm.hscroll_delta(e)),
                _ => None,
            })
            .sum();
        if scroll_delta != 0.0 {
            std::thread::sleep(std::time::Duration::from_millis(8));
            while let Some(ev) = wm.conn.poll_for_event()? {
                batch.push(ev);
            }
        }
        let debug_scroll = scroll_delta != 0.0 && wm.debug_scroll;
        let batch_len = batch.len();
        let t0 = std::time::Instant::now();
        wm.handle_batch(batch)?;
        if debug_scroll {
            eprintln!(
                "splitwm: batch of {batch_len} events (scroll delta {scroll_delta:.3}) took {:?}",
                t0.elapsed()
            );
        }
    }

    // Layout hiding uses plain unmaps; remap everything on the way out so
    // taskbar'd windows aren't stranded invisible for the next WM (which
    // only adopts viewable windows).
    wm.restore_clients()?;
    Ok(())
}

impl Wm {
    /// Load the wallpaper (`SPLITWM_WALLPAPER`) into the renderer, scaled to
    /// the current workarea; it is composited onto the underlay each
    /// arrange. Re-run on root resize. No-op if unset/unreadable.
    pub(crate) fn set_wallpaper(&mut self) {
        if let Some(path) = self.wallpaper_path.clone() {
            let wa = self.wa();
            self.renderer.set_wallpaper(&path, wa.w, wa.h);
        }
    }

    /// The full-screen workarea, cached (refreshed by root ConfigureNotify) —
    /// `arrange` needs it several times per frame, so it must not cost a
    /// `GetGeometry` round trip.
    pub(crate) const fn wa(&self) -> Rect {
        self.workarea
    }

    /// Height reserved at the bottom for the window bar. Always present so the
    /// launcher "+" at its right edge is reachable even with no windows open.
    pub(crate) const fn taskbar_h() -> i32 {
        theme::TASKBAR_H
    }

    /// The split-layout area: the workarea minus the bottom taskbar strip.
    /// The docked sidebar (see `manage_dock`) is parked off-screen, so it
    /// reserves no space here and the tiling canvas stays full width.
    pub(crate) fn la(&self) -> Rect {
        let wa = self.wa();
        Rect {
            h: (wa.h - Self::taskbar_h()).max(1),
            ..wa
        }
    }

    // --- keyboard ---

    pub(crate) fn build_keymap(&mut self) -> R<()> {
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

    pub(crate) fn grab_keys(&mut self) -> R<()> {
        let shift = u16::from(ModMask::SHIFT);
        let defs: &[(u16, u32, Action)] = &[
            (MOD4, ks::RETURN, Action::SpawnTerminal),
            (MOD4, ks::SPACE, Action::SpawnLauncher),
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
            (MOD4 | shift, ks::C, Action::CloseWindow),
            (0, ks::XF86_MON_BRIGHTNESS_UP, Action::BrightnessUp),
            (0, ks::XF86_MON_BRIGHTNESS_DOWN, Action::BrightnessDown),
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

    // --- trackpad / horizontal-scroll device discovery ---

    /// Rescan every input device for a horizontal scroll valuator. Run once
    /// at startup and again on every `XI_HierarchyChanged` (device
    /// plug/unplug), mirroring scrollpipe.c's `build_map`.
    pub(crate) fn build_hscroll_map(&mut self) -> R<()> {
        let reply = self.conn.xinput_xi_query_device(XI_ALL_DEVICES)?.reply()?;
        self.hscroll.clear();
        for info in &reply.infos {
            for class in &info.classes {
                let xinput::DeviceClassData::Scroll(s) = &class.data else {
                    continue;
                };
                if s.scroll_type != ScrollType::HORIZONTAL {
                    continue;
                }
                let incr = fp3232_to_f64(s.increment);
                self.hscroll.push(HScroll {
                    dev: class.sourceid,
                    valuator: s.number,
                    incr: if incr == 0.0 { 120.0 } else { incr },
                });
            }
        }
        if self.debug_scroll {
            eprintln!(
                "splitwm: hscroll map rebuilt, {} device(s): {:?}",
                self.hscroll.len(),
                self.hscroll
                    .iter()
                    .map(|h| (h.dev, h.valuator, h.incr))
                    .collect::<Vec<_>>()
            );
        }
        Ok(())
    }
}

/// The server's ARGB32 pict format, or `None` when RENDER cursors (>= 0.5)
/// aren't available and the core-font glyph cursors should stay.
fn render_argb32_format(conn: &x11rb::rust_connection::RustConnection) -> R<Option<Pictformat>> {
    use x11rb::connection::RequestConnection;
    if conn
        .extension_information(render::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    let version = conn.render_query_version(0, 8)?.reply()?;
    if version.major_version == 0 && version.minor_version < 5 {
        return Ok(None);
    }
    let formats = conn.render_query_pict_formats()?.reply()?;
    Ok(formats
        .formats
        .iter()
        .find(|f| {
            f.depth == 32
                && f.type_ == PictType::DIRECT
                && f.direct.alpha_mask == 0xFF
                && f.direct.alpha_shift == 24
                && f.direct.red_shift == 16
                && f.direct.green_shift == 8
                && f.direct.blue_shift == 0
        })
        .map(|f| f.id))
}

/// Build an ARGB hardware cursor from a baked palette-indexed sprite.
fn sprite_cursor(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
    format: Pictformat,
    sprite: &pixel_graphics::Sprite,
    palette: &pixel_graphics::Palette,
    (hot_x, hot_y): (u16, u16),
) -> R<u32> {
    let msb_first = conn.setup().image_byte_order == ImageOrder::MSB_FIRST;
    let mut data = Vec::with_capacity(sprite.width * sprite.height * 4);
    for y in 0..sprite.height {
        for x in 0..sprite.width {
            let index = sprite.at(x, y);
            // RENDER wants premultiplied alpha; with only fully opaque or
            // fully transparent pixels the colors pass through unchanged.
            let pixel = if index == pixel_graphics::TRANSPARENT {
                [0, 0, 0, 0]
            } else {
                let c = palette.color(index);
                if msb_first {
                    [0xFF, c.r, c.g, c.b]
                } else {
                    [c.b, c.g, c.r, 0xFF]
                }
            };
            data.extend_from_slice(&pixel);
        }
    }

    let pixmap = conn.generate_id()?;
    conn.create_pixmap(32, pixmap, root, sprite.width as u16, sprite.height as u16)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new())?;
    conn.put_image(
        ImageFormat::Z_PIXMAP,
        pixmap,
        gc,
        sprite.width as u16,
        sprite.height as u16,
        0,
        0,
        0,
        32,
        &data,
    )?;
    let picture = conn.generate_id()?;
    conn.render_create_picture(picture, pixmap, format, &render::CreatePictureAux::new())?;
    let cursor = conn.generate_id()?;
    conn.render_create_cursor(cursor, picture, hot_x, hot_y)?;
    conn.render_free_picture(picture)?;
    conn.free_gc(gc)?;
    conn.free_pixmap(pixmap)?;
    Ok(cursor)
}
