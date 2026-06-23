//! X11 window-manager core: become WM, manage clients in per-leaf frame
//! windows, run keybindings, drive the splitwm layout + renderer.

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
use crate::tree::{client_geo, Dir, NodeId, Rect, Win};

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
    frame: Window,
    parent_leaf: NodeId, // leaf whose frame currently parents this client
    label: char,
    color: u32,
    icon: Option<Rc<Icon>>,
}

struct Wm {
    conn: RustConnection,
    root: Window,
    depth: u8,
    visual: u32,
    state: State,
    clients: HashMap<Win, Client>,
    frames: HashMap<NodeId, Window>, // leaf id -> frame window
    renderer: Renderer,
    gc: Gcontext,                     // shared graphics context for all `PutImage` blits
    keymap: HashMap<u32, u8>,         // keysym -> keycode
    bindings: Vec<(u16, u8, Action)>, // (modmask, keycode, action)
    running: bool,
    max_req_bytes: usize,
    atom_net_wm_icon: u32,
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

    let mut wm = Wm {
        depth: screen.root_depth,
        visual: screen.root_visual,
        gc,
        keymap: HashMap::new(),
        bindings: Vec::new(),
        renderer: Renderer::new(),
        state: State::new(),
        clients: HashMap::new(),
        frames: HashMap::new(),
        running: true,
        max_req_bytes,
        atom_net_wm_icon,
        conn,
        root,
    };

    wm.build_keymap()?;
    wm.grab_keys()?;
    wm.set_wallpaper()?;
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
    /// Paint a scaled PNG wallpaper (env `SPLITWM_WALLPAPER`) as the root
    /// background so the gaps between leaves show it. No-op if unset/unreadable.
    fn set_wallpaper(&mut self) -> R<()> {
        let Ok(path) = std::env::var("SPLITWM_WALLPAPER") else {
            return Ok(());
        };
        let wa = self.wa();
        let Some(buf) = crate::render::load_wallpaper(&path, wa.w, wa.h) else {
            return Ok(());
        };
        let (w, h) = (wa.w as u16, wa.h as u16);
        let pm = self.conn.generate_id()?;
        self.conn.create_pixmap(self.depth, pm, self.root, w, h)?;
        self.put_image(pm, w, h, &buf)?;
        self.conn.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().background_pixmap(pm),
        )?;
        self.conn.clear_area(false, self.root, 0, 0, 0, 0)?;
        self.conn.free_pixmap(pm)?;
        Ok(())
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

        // Create the frame for the focused leaf if needed; pin client there.
        self.state.pin_client(win);
        let leaf = self
            .state
            .leaf_of_client(win)
            .unwrap_or_else(|| self.state.focused_leaf_valid());

        let frame = self.ensure_frame(leaf)?;

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
        self.conn.reparent_window(win, frame, 0, 0)?;
        self.conn.map_window(win)?;

        self.clients.insert(
            win,
            Client {
                frame,
                parent_leaf: leaf,
                label,
                color,
                icon,
            },
        );
        self.state.activate_client(win);
        self.arrange()?;
        self.focus(Some(win))?;
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
            .get_property(false, win, self.atom_net_wm_icon, AtomEnum::CARDINAL, 0, u32::MAX)
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
    fn resample_color(&mut self, win: Win) {
        let Ok(geo) = self.conn.get_geometry(win).and_then(|c| Ok(c.reply())) else {
            return;
        };
        let Ok(geo) = geo else { return };
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
        self.state.ensure_in_view(wa);
        self.state.scroll_x = self.state.scroll_target;
        self.arrange()?;
        let f = self.state.focused_client();
        self.focus(f)?;
        Ok(())
    }

    fn on_button(&mut self, e: ButtonPressEvent) -> R<()> {
        let wa = self.wa();
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

    fn on_expose(&self, e: ExposeEvent) -> R<()> {
        if e.count != 0 {
            return Ok(());
        }
        // Repaint whichever frame got exposed.
        let leaf = self
            .frames
            .iter()
            .find(|(_, &w)| w == e.window)
            .map(|(&l, _)| l);
        if let Some(leaf) = leaf {
            self.paint_frame(leaf)?;
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

    // --- frames & arrange ---

    /// Reparent a client window into `parent` and record which leaf now owns
    /// it. `leaf` is `NodeId::MAX` when parking a client back on root.
    fn reparent_client(&mut self, w: Win, parent: Window, leaf: NodeId) -> R<()> {
        self.conn.reparent_window(w, parent, 0, 0)?;
        if let Some(c) = self.clients.get_mut(&w) {
            c.parent_leaf = leaf;
        }
        Ok(())
    }

    fn ensure_frame(&mut self, leaf: NodeId) -> R<Window> {
        if let Some(&f) = self.frames.get(&leaf) {
            return Ok(f);
        }
        let f = self.conn.generate_id()?;
        let aux = CreateWindowAux::new()
            .background_pixel(theme::WALLPAPER & 0x00ff_ffff)
            .event_mask(EventMask::EXPOSURE | EventMask::SUBSTRUCTURE_NOTIFY);
        self.conn.create_window(
            self.depth,
            f,
            self.root,
            0,
            0,
            100,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &aux,
        )?;
        self.frames.insert(leaf, f);
        Ok(f)
    }

    fn arrange(&mut self) -> R<()> {
        // Refresh the focused client's sampled accent from its content.
        if let Some(f) = self.state.focused_client() {
            self.resample_color(f);
        }
        let wa = self.wa();
        let gap = theme::GAP;
        let tb_h = theme::tb_h(gap);
        let bw = theme::FOCUS_BORDER_WIDTH;

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

        // Remove frames for leaves that no longer exist.
        let live: std::collections::HashSet<NodeId> = leaves.iter().copied().collect();
        let dead: Vec<NodeId> = self
            .frames
            .keys()
            .copied()
            .filter(|l| !live.contains(l))
            .collect();
        for leaf in dead {
            if let Some(f) = self.frames.remove(&leaf) {
                // Reparent any surviving clients back to root before destroy.
                let kids: Vec<Win> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| c.frame == f)
                    .map(|(&w, _)| w)
                    .collect();
                for w in kids {
                    self.reparent_client(w, self.root, NodeId::MAX)?;
                }
                self.conn.destroy_window(f)?;
            }
        }

        for &leaf in &leaves {
            let geo = match geos.get(&leaf) {
                Some(g) => *g,
                None => continue,
            };
            let frame = self.ensure_frame(leaf)?;

            // Frame screen rect: visual top is gap above geo.y.
            let fx = geo.x - scroll_x;
            let fy = geo.y - gap;
            let fw = geo.w.max(1);
            let fh = (geo.h + gap).max(1);

            let off_screen = fx + fw <= wa.x || fx >= wa.x + wa.w;

            // Always reparent this leaf's clients into its frame (so they
            // travel/hide with it), then map/unmap based on visibility.
            let (active_client, tabs): (Option<Win>, Vec<Win>) = {
                let l = self.state.tree.leaf(leaf).unwrap();
                (l.tabs.get(l.active).copied(), l.tabs.clone())
            };
            let cg = client_geo(geo, bw, gap, tb_h, scroll_x);
            for w in &tabs {
                let need_reparent = self.clients.get(w).is_some_and(|c| c.parent_leaf != leaf);
                if need_reparent {
                    self.reparent_client(*w, frame, leaf)?;
                }
                if Some(*w) == active_client && !off_screen {
                    self.conn.configure_window(
                        *w,
                        &ConfigureWindowAux::new()
                            .x(bw)
                            .y(tb_h)
                            .width(u32::try_from(cg.w).unwrap_or(0))
                            .height(u32::try_from(cg.h).unwrap_or(0))
                            .border_width(0),
                    )?;
                    self.conn.map_window(*w)?;
                } else {
                    self.conn.unmap_window(*w)?;
                }
            }

            if off_screen {
                self.conn.unmap_window(frame)?;
                continue;
            }
            self.conn.configure_window(
                frame,
                &ConfigureWindowAux::new()
                    .x(fx)
                    .y(fy)
                    .width(u32::try_from(fw).unwrap_or(0))
                    .height(u32::try_from(fh).unwrap_or(0)),
            )?;
            self.conn.map_window(frame)?;
            self.paint_frame_geo(leaf, frame, fw, fh, focused == leaf)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn paint_frame(&self, leaf: NodeId) -> R<()> {
        let Some(&frame) = self.frames.get(&leaf) else {
            return Ok(());
        };
        let g = self.conn.get_geometry(frame)?.reply()?;
        let focused = self.state.focused_leaf_valid() == leaf;
        self.paint_frame_geo(
            leaf,
            frame,
            i32::from(g.width),
            i32::from(g.height),
            focused,
        )
    }

    fn paint_frame_geo(
        &self,
        leaf: NodeId,
        frame: Window,
        fw: i32,
        fh: i32,
        focused: bool,
    ) -> R<()> {
        let gap = theme::GAP;
        let tb_h = theme::tb_h(gap);
        let bw = theme::FOCUS_BORDER_WIDTH;

        let accent = {
            let l = self.state.tree.leaf(leaf);
            l.and_then(|l| l.tabs.get(l.active))
                .and_then(|w| self.clients.get(w))
                .map_or(theme::COLOR_ACCENT, |c| c.color)
        };
        let tabs: Vec<TabInfo> = {
            let Some(l) = self.state.tree.leaf(leaf) else {
                return Ok(());
            };
            l.tabs
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    let c = self.clients.get(w);
                    TabInfo {
                        label: c.map_or('?', |c| c.label),
                        color: c.map_or(theme::COLOR_FG, |c| c.color),
                        active: i == l.active,
                        icon: c.and_then(|c| c.icon.clone()),
                    }
                })
                .collect()
        };

        let view = LeafView {
            w: fw,
            h: fh,
            tb_h,
            bw,
            focused,
            accent,
            tabs,
        };
        let buf = self.renderer.render(&view);
        self.put_image(
            frame,
            u16::try_from(fw).unwrap_or(u16::MAX),
            u16::try_from(fh).unwrap_or(u16::MAX),
            &buf,
        )?;
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
