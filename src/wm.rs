//! X11 window-manager core: become WM, manage clients in per-leaf frame
//! windows, run keybindings, drive the splitwm layout + renderer.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Allow, AtomEnum, ButtonIndex, ButtonPressEvent, ChangeWindowAttributesAux,
    ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, ExposeEvent, Gcontext, GrabMode, ImageFormat, InputFocus, KeyPressEvent,
    MapRequestEvent, ModMask, StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use crate::render::{Icon, LeafView, Renderer, TabInfo};
use crate::state::State;
use crate::theme;
use crate::tree::{Boundary, Dir, NodeId, Rect, Win};

type R<T> = Result<T, Box<dyn Error>>;

// --- X11 keysyms we bind ---
mod ks {
    pub const RETURN: u32 = 0xff0d;
    pub const TAB: u32 = 0xff09;
    pub const LEFT: u32 = 0xff51;
    pub const RIGHT: u32 = 0xff53;
    pub const BRACKETLEFT: u32 = 0x5b;
    pub const BRACKETRIGHT: u32 = 0x5d;
    pub const MINUS: u32 = 0x2d;
    pub const EQUAL: u32 = 0x3d;
    pub const ZERO: u32 = 0x30;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const V: u32 = 0x76;
    pub const H: u32 = 0x68;
    pub const Q: u32 = 0x71;
    pub const L: u32 = 0x6c;
    pub const C: u32 = 0x63;
}

#[derive(Clone, Copy, Debug)]
enum Action {
    SplitH,
    SplitV,
    Close,
    FocusNext,
    FocusPrev,
    NextTab,
    PrevTab,
    MoveTabNext,
    MoveTabPrev,
    Grow,
    Shrink,
    SpawnTerminal,
    Quit,
    KillClient,
}

struct Client {
    label: char,
    color: u32,
    icon: Option<Rc<Icon>>,
}

struct Wm {
    conn: RustConnection,
    root: Window,
    depth: u8,
    state: State,
    clients: HashMap<Win, Client>,
    underlay: Window, // single full-screen window holding all chrome
    renderer: Renderer,
    gc: Gcontext,                     // shared graphics context for all `PutImage` blits
    keymap: HashMap<u32, u8>,         // keysym -> keycode
    bindings: Vec<(u16, u8, Action)>, // (modmask, keycode, action)
    running: bool,
    max_req_bytes: usize,
    atom_net_wm_icon: u32,
    animate: bool,                               // play a transition on next arrange
    prev_frame_rect: HashMap<NodeId, FrameRect>, // last applied leaf chrome screen rects
    handle_regions: Vec<(FrameRect, Boundary)>,  // gap resize hit-rects (screen coords)
    plus_regions: Vec<(FrameRect, usize)>,       // "+" hit-rects -> root insertion index
    drag: Option<Drag>,                          // active gap resize
    smush_applied: HashMap<Win, (u8, i32)>,      // client -> (mode, width bucket)
}

/// An in-progress gap resize started by dragging a handle.
#[derive(Clone, Copy)]
struct Drag {
    parent: NodeId,
    idx: usize,
    left_x: i32,   // canvas-space left edge of the left child
    combined: i32, // left_w + right_w in px (held constant)
    gap: i32,
}

#[derive(Clone, Copy)]
struct FrameRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// A leaf placed during an arrange, retained so the animator can move it.
#[derive(Clone, Copy)]
struct Placement {
    leaf: NodeId,
    target: FrameRect,
    active_client: Option<Win>,
    focused: bool,
}

/// ease-out-back (slight overshoot then settle), matching animation.lua.
fn ease_out_back(t: f32) -> f32 {
    let c = 1.1_f32;
    let t = t - 1.0;
    let inner = (c + 1.0).mul_add(t, c);
    (t * t).mul_add(inner, 1.0)
}

const fn rect_contains(r: FrameRect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

fn lerp_rect(a: FrameRect, b: FrameRect, p: f32) -> FrameRect {
    let l = |s: i32, e: i32| s + ((e - s) as f32 * p) as i32;
    FrameRect {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        w: l(a.w, b.w).max(1),
        h: l(a.h, b.h).max(1),
    }
}

const MOD4: u16 = 0x40; // ModMask::M4

fn workarea(conn: &RustConnection, root: Window) -> R<Rect> {
    let geo = conn.get_geometry(root)?.reply()?;
    Ok(Rect {
        x: 0,
        y: 0,
        w: i32::from(geo.width),
        h: i32::from(geo.height),
    })
}

// Hue-rotated palette so distinct clients get distinct accent colours.
const PALETTE: [u32; 8] = [
    0xff66_aaff,
    0xffff_6688,
    0xff66_dd99,
    0xffff_cc66,
    0xffcc_88ff,
    0xff66_dddd,
    0xffff_9966,
    0xffaa_dd66,
];

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

    // Black root background.
    let cw = ChangeWindowAttributesAux::new().background_pixel(screen.black_pixel);
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

    let mut wm = Wm {
        depth: screen.root_depth,
        gc,
        keymap: HashMap::new(),
        bindings: Vec::new(),
        renderer: Renderer::new(),
        state: State::new(),
        clients: HashMap::new(),
        underlay,
        running: true,
        max_req_bytes,
        atom_net_wm_icon,
        animate: false,
        prev_frame_rect: HashMap::new(),
        handle_regions: Vec::new(),
        plus_regions: Vec::new(),
        drag: None,
        smush_applied: HashMap::new(),
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
        let ev = wm.conn.wait_for_event()?;
        wm.handle_event(ev)?;
        // Drain any queued events before re-arranging.
        while let Some(ev) = wm.conn.poll_for_event()? {
            wm.handle_event(ev)?;
        }
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
    fn handle_event(&mut self, ev: Event) -> R<()> {
        match ev {
            Event::MapRequest(e) => self.on_map_request(e)?,
            Event::UnmapNotify(e) => self.on_unmap(e.window),
            Event::DestroyNotify(e) => self.on_destroy(e.window)?,
            Event::ConfigureRequest(e) => self.on_configure_request(&e)?,
            Event::KeyPress(e) => self.on_key(e)?,
            Event::ButtonPress(e) => self.on_button(e)?,
            Event::ButtonRelease(_) => self.on_button_release()?,
            Event::MotionNotify(e) => self.on_motion(&e)?,
            Event::Expose(e) => self.on_expose(e)?,
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
        // Class -> label + colour; app icon from _NET_WM_ICON.
        let (label, color) = self.client_identity(win);
        let icon = self.fetch_icon(win);

        // Pin the client into the focused leaf's tab stack.
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

        self.clients.insert(win, Client { label, color, icon });
        self.state.activate_client(win);
        self.arrange()?;
        self.focus(Some(win))?;
        self.smush_focused()?;
        Ok(())
    }

    fn client_identity(&self, win: Win) -> (char, u32) {
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
        let label = name
            .first()
            .map_or('?', |&b| (b as char).to_ascii_uppercase());
        let mut hash: u32 = 5381;
        for &b in name {
            hash = hash.wrapping_mul(33).wrapping_add(u32::from(b));
        }
        let color = PALETTE[(hash as usize) % PALETTE.len()];
        (label, color)
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

    /// Sample the focused client's top-center content and, if it reads as a
    /// real colour, adopt it as the client's accent (matches the original's
    /// "active tab blends into the app" behaviour). Best-effort: errors and
    /// near-black/uniform reads leave the palette colour in place.
    #[allow(clippy::many_single_char_names)]
    fn resample_color(&mut self, win: Win) {
        let Ok(cookie) = self.conn.get_geometry(win) else {
            return;
        };
        let Ok(geo) = cookie.reply() else { return };
        let w = geo.width;
        if w == 0 || geo.height == 0 {
            return;
        }
        let strip_h = 4u16.min(geo.height);
        let Ok(img) = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w, strip_h, !0)
        else {
            return;
        };
        let Ok(img) = img.reply() else { return };
        let data = &img.data;
        if data.len() < 4 {
            return;
        }
        let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
        for px in data.chunks_exact(4) {
            // Z_PIXMAP on a depth-24 TrueColor visual: B, G, R, X.
            sb += u64::from(px[0]);
            sg += u64::from(px[1]);
            sr += u64::from(px[2]);
            n += 1;
        }
        if n == 0 {
            return;
        }
        let (r, g, b) = ((sr / n) as u32, (sg / n) as u32, (sb / n) as u32);
        // Reject near-black (unrendered/obscured content).
        if r + g + b < 24 {
            return;
        }
        let color = 0xff00_0000 | (r << 16) | (g << 8) | b;
        if let Some(c) = self.clients.get_mut(&win) {
            c.color = color;
        }
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    const fn on_unmap(&self, _win: Win) {
        // We unmap inactive tabs and reparent clients ourselves, both of which
        // generate UnmapNotify. Distinguishing those from a real client
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
        self.smush_applied.remove(&win);
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
        let wa = self.wa();
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
                self.state.cycle_tab(1);
            }
            Action::PrevTab => {
                self.state.cycle_tab(-1);
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
        self.smush_focused()?;
        Ok(())
    }

    fn on_button(&mut self, e: ButtonPressEvent) -> R<()> {
        let wa = self.wa();
        // Clicks on the underlay (gaps): hit-test drag handles and "+" buttons.
        if e.detail == 1 && e.event == self.underlay {
            let (mx, my) = (i32::from(e.event_x), i32::from(e.event_y));
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

    fn on_button_release(&mut self) -> R<()> {
        if self.drag.take().is_some() {
            self.arrange()?;
            self.smush_focused()?;
        }
        Ok(())
    }

    fn on_expose(&mut self, e: ExposeEvent) -> R<()> {
        // Recompose the underlay once the exposure run completes.
        if e.count == 0 && e.window == self.underlay {
            self.arrange()?;
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

    // --- smush (auto font shrink in narrow splits) ---

    /// If the focused split is narrow, synthesize Ctrl+0 then Ctrl+- (once or
    /// twice) into the focused client to shrink its font, matching splitwm's
    /// smush. Only the focused, settled client is touched; results are cached
    /// per client + width bucket so small resizes don't re-trigger.
    fn smush_focused(&mut self) -> R<()> {
        let Some(c) = self.state.focused_client() else {
            return Ok(());
        };
        let wa = self.wa();
        let geos = self.state.compute(wa);
        let Some(g) = geos.get(&self.state.focused_leaf_valid()) else {
            return Ok(());
        };
        // Bucket widths so sub-bucket resizes don't re-trigger the shortcuts.
        const SMUSH_BUCKET: i32 = 25;
        let width = g.w;
        let bucket = width / SMUSH_BUCKET;
        let (mode, bucket) = if width >= theme::SMUSH_THRESHOLD {
            (0u8, 0)
        } else if width < theme::TINY_SMUSH_THRESHOLD {
            (2u8, bucket)
        } else {
            (1u8, bucket)
        };
        match self.smush_applied.get(&c) {
            Some(&(0, _)) if mode == 0 => return Ok(()), // already at default zoom
            Some(&(m, b)) if m == mode && b == bucket => return Ok(()),
            _ => {}
        }
        self.smush_applied.insert(c, (mode, bucket));

        let ctrl = self.keymap.get(&ks::CONTROL_L).copied();
        let zero = self.keymap.get(&ks::ZERO).copied();
        let minus = self.keymap.get(&ks::MINUS).copied();
        let (Some(ctrl), Some(zero), Some(minus)) = (ctrl, zero, minus) else {
            return Ok(());
        };
        self.send_combo(ctrl, zero)?; // reset zoom
        for _ in 0..mode {
            self.send_combo(ctrl, minus)?; // shrink
        }
        Ok(())
    }

    /// Synthesize a Ctrl+<key> chord to the focused window via XTEST.
    fn send_combo(&self, ctrl: u8, key: u8) -> R<()> {
        use x11rb::protocol::xtest::ConnectionExt as _;
        const PRESS: u8 = 2;
        const RELEASE: u8 = 3;
        for (ty, kc) in [(PRESS, ctrl), (PRESS, key), (RELEASE, key), (RELEASE, ctrl)] {
            self.conn
                .xtest_fake_input(ty, kc, CURRENT_TIME, self.root, 0, 0, 0)?;
        }
        Ok(())
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
        // Refresh the focused client's sampled accent from its content.
        if let Some(f) = self.state.focused_client() {
            self.resample_color(f);
        }
        let wa = self.wa();
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
                y: geo.y - gap,
                w: geo.w.max(1),
                h: (geo.h + gap).max(1),
            };
            if target.x + target.w <= wa.x || target.x >= wa.x + wa.w {
                continue;
            }
            let active_client = self
                .state
                .tree
                .leaf(leaf)
                .and_then(|l| l.tabs.get(l.active).copied());
            placed.push(Placement {
                leaf,
                target,
                active_client,
                focused: focused == leaf,
            });
        }

        // Drag-handle / "+" hit-regions for the current layout.
        self.compute_widgets(wa);

        if std::mem::take(&mut self.animate) {
            self.run_layout_animation(wa, &placed)?;
        }
        self.compose(wa, &placed, true)?;
        self.place_clients(&placed)?;

        // Cache final rects as the start point for the next transition.
        self.prev_frame_rect = placed.iter().map(|p| (p.leaf, p.target)).collect();
        self.conn.flush()?;
        Ok(())
    }

    /// Composite the wallpaper, every placed leaf's chrome, and (optionally)
    /// the drag handles / "+" buttons onto the single underlay window.
    fn compose(&self, wa: Rect, placed: &[Placement], widgets: bool) -> R<()> {
        let (w, h) = (wa.w.max(1) as u32, wa.h.max(1) as u32);
        let mut pm = self.renderer.screen_base(w, h);
        {
            let mut m = pm.as_mut();
            for p in placed {
                let view = self.leaf_view(p.leaf, p.target.w, p.target.h, p.focused);
                self.renderer
                    .draw_leaf(&mut m, p.target.x as f32, p.target.y as f32, &view);
            }
            if widgets {
                for (r, _) in &self.handle_regions {
                    crate::render::draw_handle(
                        &mut m, r.x as f32, r.y as f32, r.w as f32, r.h as f32, false,
                    );
                }
                for (r, _) in &self.plus_regions {
                    let cx = (r.x + r.w / 2) as f32;
                    let cy = (r.y + r.h / 2) as f32;
                    crate::render::draw_plus(&mut m, cx, cy, r.w as f32);
                }
            }
        }
        let buf = crate::render::pixmap_to_bgrx(&pm);
        self.put_image(self.underlay, w as u16, h as u16, &buf)?;
        Ok(())
    }

    /// Build the render view for a leaf at a given (possibly animated) size.
    fn leaf_view(&self, leaf: NodeId, w: i32, h: i32, focused: bool) -> LeafView {
        let l = self.state.tree.leaf(leaf);
        let accent = l
            .and_then(|l| l.tabs.get(l.active))
            .and_then(|c| self.clients.get(c))
            .map_or(theme::COLOR_ACCENT, |c| c.color);
        let tabs: Vec<TabInfo> = l
            .map(|l| {
                l.tabs
                    .iter()
                    .enumerate()
                    .map(|(i, win)| {
                        let c = self.clients.get(win);
                        TabInfo {
                            label: c.map_or('?', |c| c.label),
                            color: c.map_or(theme::COLOR_FG, |c| c.color),
                            active: i == l.active,
                            icon: c.and_then(|c| c.icon.clone()),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        LeafView {
            w,
            h,
            tb_h: theme::tb_h(theme::GAP),
            bw: theme::FOCUS_BORDER_WIDTH,
            focused,
            accent,
            tabs,
        }
    }

    /// Position each leaf's active client below its tab bar; unmap the rest.
    fn place_clients(&self, placed: &[Placement]) -> R<()> {
        let gap = theme::GAP;
        let tb_h = theme::tb_h(gap);
        let bw = theme::FOCUS_BORDER_WIDTH;
        let mut visible: std::collections::HashSet<Win> = std::collections::HashSet::new();
        for p in placed {
            if let Some(c) = p.active_client {
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
    fn compute_widgets(&mut self, wa: Rect) {
        self.handle_regions.clear();
        self.plus_regions.clear();
        let gap = theme::GAP;
        let hw = (gap - Self::HANDLE_INSET).max(4);
        let scroll_x = self.state.scroll_x;
        let canvas_w = self.state.canvas_w.unwrap_or(wa.w);
        for b in self.state.boundaries(wa) {
            let vis_x = b.x - scroll_x;
            if vis_x + hw / 2 <= wa.x || vis_x - hw / 2 >= wa.x + wa.w {
                continue;
            }
            self.handle_regions.push((
                FrameRect {
                    x: vis_x - hw / 2,
                    y: b.y,
                    w: hw,
                    h: b.h.max(1),
                },
                b,
            ));
            if b.root {
                let py = b.y + (b.h - Self::PLUS_SZ) / 2;
                self.plus_regions.push((Self::plus_rect(vis_x, py), b.idx + 1));
            }
        }
        // Edge "+" buttons (insert at the far left / far right of the canvas).
        let span_h = (wa.h - 2 * gap).max(Self::PLUS_SZ);
        let edge_cy = wa.y + gap + (span_h - Self::PLUS_SZ) / 2;
        for (canvas_x, at) in [
            (wa.x + gap / 2, 0usize),
            (wa.x + canvas_w - gap / 2, usize::MAX),
        ] {
            let vis_x = canvas_x - scroll_x;
            if vis_x < wa.x || vis_x > wa.x + wa.w {
                continue;
            }
            self.plus_regions.push((Self::plus_rect(vis_x, edge_cy), at));
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
