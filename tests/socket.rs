//! Asserting integration tests over the real Wayland socket: the itest.sh
//! analog. Each test boots the actual compositor binary on the headless
//! backend (`SPLITWM_HEADLESS=1`), connects as an ordinary client, and
//! asserts what a client can observe — configure sizes, xdg activated
//! state, keyboard focus, and close semantics. WM chords are injected over
//! the debug channel (`SPLITWM_DEBUG_CHANNEL=1`, stdin), which acks each
//! command on stdout.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
    wl_data_source, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// The headless backend's fixed output, and the taskbar strip under the
/// layout area — a generous bound; the exact chrome insets stay the
/// compositor's business.
const OUTPUT_W: i32 = 1280;

/// The compositor subprocess plus its debug channel.
struct Wm {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    socket_name: String,
}

impl Wm {
    fn spawn() -> Wm {
        let mut child = Command::new(env!("CARGO_BIN_EXE_splitwm"))
            .env("SPLITWM_HEADLESS", "1")
            .env("SPLITWM_DEBUG_CHANNEL", "1")
            // Never contend with the live session's notification daemon.
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent-splitwm-test",
            )
            .env_remove("SPLITWM_WALLPAPER")
            .env_remove("SPLITWM_DOCK_TITLE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn headless compositor");
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));

        // The compositor announces its socket on stdout once it listens.
        let mut socket_name = String::new();
        let mut line = String::new();
        while socket_name.is_empty() {
            line.clear();
            let n = stdout.read_line(&mut line).expect("read compositor stdout");
            assert!(n > 0, "compositor exited before announcing its socket");
            if let Some(name) = line.trim().strip_prefix("WAYLAND_DISPLAY=") {
                socket_name = name.to_string();
            }
        }
        Wm {
            child,
            stdin,
            stdout,
            socket_name,
        }
    }

    /// Send a debug-channel command and wait for its ack, so the action
    /// has run (not merely been sent) on return. Skips announcement lines
    /// (`DISPLAY=…` arrives whenever XWayland gets ready).
    fn cmd(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write debug channel");
        loop {
            let mut ack = String::new();
            let n = self.stdout.read_line(&mut ack).expect("read ack");
            assert!(n > 0, "compositor exited awaiting ack for: {line}");
            if ack.starts_with("ok ") || ack.starts_with("err ") {
                assert_eq!(ack.trim(), format!("ok {line}"));
                return;
            }
        }
    }

    /// Block until the compositor announces its XWayland DISPLAY — X11
    /// clients launched earlier would race the server's startup.
    fn await_xwayland(&mut self) {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read stdout");
            assert!(n > 0, "compositor exited before XWayland became ready");
            if line.starts_with("DISPLAY=") {
                return;
            }
        }
    }

    fn key(&mut self, chord: &str) {
        self.cmd(&format!("key {chord}"));
    }

    /// Send a query command and return its ack payload (the text after
    /// `ok <line> `).
    fn query(&mut self, line: &str) -> String {
        writeln!(self.stdin, "{line}").expect("write debug channel");
        loop {
            let mut ack = String::new();
            let n = self.stdout.read_line(&mut ack).expect("read ack");
            assert!(n > 0, "compositor exited awaiting ack for: {line}");
            let ack = ack.trim();
            if let Some(rest) = ack.strip_prefix(&format!("ok {line} ")) {
                return rest.to_string();
            }
            assert!(!ack.starts_with("err "), "query failed: {ack}");
        }
    }
}

impl Drop for Wm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One test window's client-side view.
struct Win {
    surface: wl_surface::WlSurface,
    #[allow(dead_code)] // held so the role object outlives the test
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    /// Latest xdg_toplevel.configure, applied on xdg_surface.configure.
    pending: (i32, i32, bool),
    size: (i32, i32),
    /// Size of the buffer currently attached; when a configure names a
    /// different size, `commit_configured_sizes` attaches a matching
    /// buffer like a real client would — a stale oversized buffer keeps
    /// covering (and stealing clicks from) chrome and panels.
    attached: (i32, i32),
    activated: bool,
    configures: u32,
    closed: bool,
}

#[derive(Default)]
struct App {
    wins: Vec<Win>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The wl_surface holding keyboard focus, by protocol id.
    focused: Option<wayland_client::backend::ObjectId>,
    /// The serial of the last keyboard enter — what set_selection wants.
    enter_serial: u32,
    /// zwlr_layer_surface configures seen (each acked on arrival).
    layer_configures: u32,
    /// The current clipboard offer and the mime types seen per offer.
    selection: Option<wl_data_offer::WlDataOffer>,
    offer_mimes: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    /// What our own wl_data_source answers Send events with.
    paste_payload: String,
}

impl App {
    fn focus_is(&self, win: usize) -> bool {
        self.focused.as_ref() == Some(&self.wins[win].surface.id())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut App,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut App,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for App {
    fn event(
        app: &mut App,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        &win: &usize,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            let w = &mut app.wins[win];
            (w.size.0, w.size.1, w.activated) = w.pending;
            w.configures += 1;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for App {
    fn event(
        app: &mut App,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        &win: &usize,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let activated = states
                    .chunks_exact(4)
                    .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
                    .any(|s| s == xdg_toplevel::State::Activated as u32);
                app.wins[win].pending = (width, height, activated);
            }
            xdg_toplevel::Event::Close => app.wins[win].closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        app: &mut App,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && app.keyboard.is_none() {
                app.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut App,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            wl_keyboard::Event::Enter {
                surface, serial, ..
            } => {
                app.focused = Some(surface.id());
                app.enter_serial = serial;
            }
            wl_keyboard::Event::Leave { .. } => app.focused = None,
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        app: &mut App,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                app.offer_mimes.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => app.selection = id,
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for App {
    fn event(
        app: &mut App,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            app.offer_mimes
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for App {
    fn event(
        app: &mut App,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wl_data_source::Event::Send { fd, .. } = event {
            let mut file = std::fs::File::from(fd);
            file.write_all(app.paste_payload.as_bytes())
                .expect("write selection payload");
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut App,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            layer.ack_configure(serial);
            app.layer_configures += 1;
        }
    }
}

wayland_client::delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(App: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(App: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(App: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);

/// A booted compositor plus one connected test client.
struct Session {
    wm: Wm,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    app: App,
    globals: wayland_client::globals::GlobalList,
    compositor: wl_compositor::WlCompositor,
    wm_base: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
    pool: wl_shm_pool::WlShmPool,
    /// Backs `pool`; the fd must outlive it.
    #[allow(dead_code)]
    pool_file: std::fs::File,
}

/// Buffer geometry for layer-surface test buffers: content is irrelevant
/// (all zeroes), the commit just has to carry *a* buffer.
const BUF: i32 = 64;

/// Default buffer size for test toplevels. The first commit's size is the
/// window's stated preference (the compositor sizes its column by it), so
/// windows that should behave like the old full-slot bootstrap ask for at
/// least the whole viewport strip.
const WIN_BUF: (i32, i32) = (OUTPUT_W, 800);

impl Session {
    fn boot() -> Session {
        let wm = Wm::spawn();
        let runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
        let stream = UnixStream::connect(format!("{runtime}/{}", wm.socket_name))
            .expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let (globals, queue) = registry_queue_init::<App>(&conn).expect("registry init");
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        // Binding the seat starts capability events; the keyboard arrives
        // in the dispatch loop.
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=5, ()).expect("bind wl_seat");

        let pool_file = tempfile::tempfile().expect("shm backing file");
        let pool_len = WIN_BUF.0 * WIN_BUF.1 * 4;
        pool_file.set_len(pool_len as u64).expect("size shm file");
        let pool = shm.create_pool(pool_file.as_fd(), pool_len, &qh, ());

        Session {
            wm,
            queue,
            qh,
            app: App::default(),
            globals,
            compositor,
            wm_base,
            seat,
            pool,
            pool_file,
        }
    }

    /// Pump events until `pred` holds, failing the test after 10 s. The
    /// roundtrips always return promptly (the compositor answers sync), so
    /// a dead expectation panics instead of hanging.
    fn wait_until(&mut self, what: &str, pred: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.queue.roundtrip(&mut self.app).expect("roundtrip");
            self.commit_configured_sizes();
            if pred(&self.app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Honor configures like a real client: any window whose configured
    /// size differs from its attached buffer gets a matching buffer
    /// committed. Skips destroyed windows (`closed` is always seen before
    /// the tests destroy one).
    fn commit_configured_sizes(&mut self) {
        for win in &mut self.app.wins {
            let (w, h) = win.size;
            if w > 0 && h > 0 && (w, h) != win.attached && !win.closed {
                let buffer =
                    self.pool
                        .create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
                win.surface.attach(Some(&buffer), 0, 0);
                win.surface.commit();
                win.attached = (w, h);
            }
        }
    }

    /// Create a toplevel and map it (initial commit, configure, buffer
    /// commit) — the standard xdg-shell dance every real client performs.
    /// The default `WIN_BUF` first buffer asks for the whole viewport
    /// strip, so the window fills the slot like the old bootstrap.
    fn open_window(&mut self) -> usize {
        self.open_window_sized(WIN_BUF.0, WIN_BUF.1)
    }

    /// `open_window` with an explicit first-buffer size — the window's
    /// stated preferred size, which the compositor opens its column at.
    fn open_window_sized(&mut self, w: i32, h: i32) -> usize {
        let win = self.app.wins.len();
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.wm_base.get_xdg_surface(&surface, &self.qh, win);
        let toplevel = xdg.get_toplevel(&self.qh, win);
        toplevel.set_app_id("splitwm-test".to_string());
        toplevel.set_title(format!("test-{win}"));
        surface.commit();
        self.app.wins.push(Win {
            surface,
            xdg,
            toplevel,
            pending: (0, 0, false),
            size: (0, 0),
            attached: (w, h),
            activated: false,
            configures: 0,
            closed: false,
        });
        self.wait_until("initial configure", |app| app.wins[win].configures > 0);
        let buffer =
            self.pool
                .create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        self.app.wins[win].surface.attach(Some(&buffer), 0, 0);
        self.app.wins[win].surface.commit();
        win
    }

    /// Screenshot the output and sample one pixel as R,G,B,A (`shot`
    /// writes raw rows for a `.rgba` path and acks after the file is
    /// complete).
    fn pixel(&mut self, x: i32, y: i32) -> [u8; 4] {
        // Tests share the process and run concurrently: the path must be
        // unique per session, not just per process.
        let path = std::env::temp_dir().join(format!(
            "splitwm-test-px-{}-{}.rgba",
            std::process::id(),
            self.wm.child.id()
        ));
        let path = path.to_str().expect("utf-8 temp path").to_string();
        self.wm.cmd(&format!("shot {path}"));
        let frame = std::fs::read(&path).expect("read screenshot");
        std::fs::remove_file(&path).ok();
        let at = ((y * OUTPUT_W + x) * 4) as usize;
        frame[at..at + 4].try_into().expect("pixel in bounds")
    }
}

#[test]
fn socket_lifecycle() {
    let mut s = Session::boot();

    // --- manage: the first client tiles into the full slot and holds both
    // activation and keyboard focus ---
    let a = s.open_window();
    s.wait_until("w1 activated with a tiled size", |app| {
        app.wins[a].activated && app.wins[a].size.0 > 0
    });
    let full = s.app.wins[a].size;
    assert!(
        full.0 > OUTPUT_W * 3 / 4 && full.0 <= OUTPUT_W,
        "first window should fill the slot width, got {full:?}"
    );
    assert!(
        full.1 > 500,
        "tiled height should be most of the output, got {full:?}"
    );
    s.wait_until("keyboard enters w1", |app| app.focus_is(a));

    // --- open-right: a second client gets its own fresh column to the
    // right of the first, at its own preferred width; the sitting tenant
    // keeps its full width untouched (columns never resize each other —
    // the strip grows and scrolls instead) ---
    let b = s.open_window_sized(400, 650);
    s.wait_until("w2 mapped in its own column and focused", |app| {
        app.wins[b].activated && app.wins[b].size.0 > 0 && app.wins[b].size.0 < full.0
    });
    s.wait_until("w1 keeps its full width", |app| {
        !app.wins[a].activated && app.wins[a].size == full
    });
    s.wait_until("keyboard moves to w2", |app| app.focus_is(b));
    let minor_w = s.app.wins[b].size.0;
    assert_eq!(
        minor_w, 400,
        "new column opens at the window's preferred width"
    );

    // --- bracket cycling is focus cycling (everything is visible) ---
    s.wm.key("super+bracketleft");
    s.wait_until("focus cycles back to w1", |app| {
        app.wins[a].activated && !app.wins[b].activated && app.focus_is(a)
    });

    // --- polite close: the chord asks the focused client, and only asks ---
    assert!(!s.app.wins[a].closed);
    s.wm.key("super+q");
    s.wait_until("w1 received xdg_toplevel.close", |app| app.wins[a].closed);
    assert!(
        !s.app.wins[b].closed,
        "only the focused window is asked to close"
    );
    // Honour it. The window's death takes its column with it, but
    // side-by-side splits don't merge: the survivor keeps its own width
    // (the canvas shrinks instead), and focus lands on it by itself.
    let survivor = s.app.wins[b].size;
    s.app.wins[a].toplevel.destroy();
    s.app.wins[a].surface.destroy();
    s.wait_until("survivor keeps its width and takes the keyboard", |app| {
        app.wins[b].activated && app.focus_is(b)
    });
    assert_eq!(
        s.app.wins[b].size, survivor,
        "closing a neighbouring column must not resize the survivor"
    );
}

/// Drag-and-drop split reordering, observed through the `layout` query
/// (depth-first leaf order — also the taskbar's tile order). Geometry
/// mirrors `theme.rs`: GAP 20, TASKBAR_ICON 42, TASKBAR_GAP 10,
/// TASKBAR_H 82, badge 17px overlapping the tile bottom by 4.
#[test]
fn drag_reorders_splits_and_badge_close_leaves_placeholder() {
    const OUTPUT_H: i32 = 800;
    // Tile k spans x [20 + 52k, 62 + 52k); the icon row's centre sits at
    // pad 13 + half the 42px icon below the bar's top edge.
    let tile_x = |k: i32| 20 + 52 * k;
    let tile_cy = OUTPUT_H - 82 + 13 + 21;

    let mut s = Session::boot();
    let mut wins = Vec::new();
    for i in 0..3 {
        // The first window fills the slot; the others ask for the same
        // width the default column used to open at (426 including the
        // 12px of frame borders), keeping the coordinate math below
        // identical to the pre-size-hint layout.
        let w = if i == 0 {
            s.open_window()
        } else {
            s.open_window_sized(414, 650)
        };
        s.wait_until("window mapped and focused", |app| app.wins[w].activated);
        wins.push(w);
        assert_eq!(i, w);
    }
    // New windows open right of the focused split: creation order is
    // layout order.
    assert_eq!(s.wm.query("layout"), "test-0 test-1 test-2");

    // Tile onto a tile's left half: test-2's tile dropped on the left
    // half of test-0's puts its split leftmost.
    s.wm.cmd(&format!("press {} {tile_cy}", tile_x(2) + 21));
    s.wm.cmd(&format!("motion 100 {tile_cy}"));
    s.wm.cmd(&format!("release {} {tile_cy}", tile_x(0) + 10));
    assert_eq!(s.wm.query("layout"), "test-2 test-0 test-1");

    // Titlebar onto a tile's right half: the leftmost split's titlebar
    // (frame at x 20, y 20, 27px tall) dropped after test-1's tile.
    s.wm.cmd("press 90 33");
    s.wm.cmd("motion 400 400");
    s.wm.cmd(&format!("release {} {tile_cy}", tile_x(2) + 35));
    assert_eq!(s.wm.query("layout"), "test-0 test-1 test-2");

    // Tile onto a split frame's right half: test-0's tile dropped near
    // the right edge of the rightmost split lands after it.
    s.wm.cmd(&format!("press {} {tile_cy}", tile_x(0) + 21));
    s.wm.cmd("motion 600 400");
    s.wm.cmd("release 1250 400");
    assert_eq!(s.wm.query("layout"), "test-1 test-2 test-0");

    // The tile badge closes only the window: its split stays behind as an
    // empty placeholder, and the next new window fills it.
    let badge = (tile_x(0) + 25 + 8, OUTPUT_H - 82 + 13 + 42 - 4 + 8);
    s.wm.cmd(&format!("click {} {}", badge.0, badge.1));
    s.wait_until("badge asks the window to close", |app| app.wins[1].closed);
    s.app.wins[1].toplevel.destroy();
    s.app.wins[1].surface.destroy();
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.wm.query("layout") != "- test-2 test-0" {
        // Keep the connection flushed so the destroy actually reaches the
        // compositor (the layout query rides a separate channel).
        s.queue.roundtrip(&mut s.app).expect("roundtrip");
        assert!(
            Instant::now() < deadline,
            "badge close should leave a placeholder, got: {}",
            s.wm.query("layout")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // An *unfocused* placeholder attracts nothing: the new window opens
    // its own column beside the focused one instead.
    let w3 = s.open_window();
    s.wait_until("new window mapped", |app| app.wins[w3].activated);
    assert_eq!(s.wm.query("layout"), "- test-2 test-0 test-3");
    // Focused (one step of wrap-around cycling from the new rightmost
    // window), the placeholder is exactly where the next window lands.
    s.wm.key("super+bracketright");
    let w4 = s.open_window();
    s.wait_until("new window mapped", |app| app.wins[w4].activated);
    assert_eq!(s.wm.query("layout"), "test-4 test-2 test-0 test-3");
}

#[test]
fn override_redirect_keyboard_focus() {
    // The launcher path: rofi runs under XWayland as an override-redirect
    // window, holds the keyboard while mapped, and — after a click stole
    // the focus — gets it back by being clicked. Observed from the tiled
    // client's side as keyboard leave/enter/leave.
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("client holds the keyboard", |app| app.focus_is(a));

    // WAYLAND_DISPLAY is scrubbed to pin rofi onto XWayland — the o-r
    // path under test (the launcher itself runs native layer-shell now).
    // The private pidfile keeps this instance from blocking, or being
    // blocked by, a launcher on the live session.
    s.wm.await_xwayland();
    s.wm.cmd(&format!(
        "spawn env -u WAYLAND_DISPLAY rofi -show drun -pid /tmp/splitwm-test-rofi-{}.pid",
        std::process::id()
    ));
    s.wait_until("rofi maps and takes the keyboard", |app| {
        app.focused.is_none()
    });

    // Rofi centers on the 1280x800 output; a corner click misses it and
    // click-to-focus returns the keyboard to the tiled client.
    s.wm.cmd("click 40 40");
    s.wait_until("click on the client takes the keyboard back", |app| {
        app.focus_is(a)
    });

    // Clicking rofi must re-grant it the keyboard (its X-side grab is
    // dead while XWayland doesn't hold our focus).
    s.wm.cmd("click 640 400");
    s.wait_until("click on rofi returns it the keyboard", |app| {
        app.focused.is_none()
    });
}

#[test]
fn x11_window_takes_keyboard_on_map() {
    // A managed X11 window is arranged at map-request time, before
    // XWayland associates its wl_surface — there is nothing to hand the
    // keyboard to yet. Once the surface arrives the keyboard must land on
    // it by itself: launching an X11 app should not need a click before
    // typing works. Observed compositor-side via the `focus` query (the
    // test's Wayland connection can't see another client's focus).
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("client holds the keyboard", |app| app.focus_is(a));

    // WAYLAND_DISPLAY is scrubbed to pin alacritty onto XWayland.
    s.wm.await_xwayland();
    s.wm.cmd("spawn env -u WAYLAND_DISPLAY alacritty --class splitwm-test-x11");
    s.wait_until("keyboard leaves the wayland client", |app| {
        app.focused.is_none()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.wm.query("focus") != "splitwm-test-x11" {
        assert!(
            Instant::now() < deadline,
            "keyboard never landed on the mapped X11 window"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn chrome_hover_cursor_shapes() {
    // Hover feedback over the chrome, observed through the debug
    // channel's `cursor` query: the arrow where nothing is clickable, the
    // resize arrow over the canvas-edge drag handle.
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("window mapped", |app| app.wins[a].activated);

    // The top-left corner sits above the edge-handle strip (which starts
    // a gap down) and outside every leaf frame: nothing to hit.
    s.wm.cmd("motion 5 5");
    assert_eq!(s.wm.query("cursor"), "default");

    // The left canvas-edge drag handle spans the outer margin strip
    // (x within the gap, y past the first gap) — a horizontal resize.
    s.wm.cmd("motion 10 300");
    assert_eq!(s.wm.query("cursor"), "ew-resize");
}

#[test]
fn layer_shell_zone_and_keyboard() {
    // wlr-layer-shell: an Overlay surface with an exclusive zone shrinks
    // the tiling area, and exclusive keyboard interactivity holds the
    // keyboard while mapped (the native-rofi contract). Destroying it
    // restores both.
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("window tiled", |app| {
        app.wins[a].activated && app.wins[a].size.0 > 0
    });
    s.wait_until("keyboard on the window", |app| app.focus_is(a));
    let full = s.app.wins[a].size;

    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = s
        .globals
        .bind(&s.qh, 1..=4, ())
        .expect("bind zwlr_layer_shell_v1");
    let surface = s.compositor.create_surface(&s.qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "splitwm-test-panel".into(),
        &s.qh,
        (),
    );
    layer.set_size(300, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom,
    );
    layer.set_exclusive_zone(300);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    // Pre-buffer commit: the compositor answers with a configure.
    surface.commit();
    s.wait_until("layer surface configured", |app| app.layer_configures > 0);

    // Mapping it takes the exclusive zone out of the tiling area and the
    // keyboard away from the window.
    let buffer = s
        .pool
        .create_buffer(0, BUF, BUF, BUF * 4, wl_shm::Format::Argb8888, &s.qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    s.wait_until("window shrinks by the exclusive zone", |app| {
        app.wins[a].size.0 > 0 && app.wins[a].size.0 <= full.0 - 250
    });
    let layer_id = surface.id();
    s.wait_until("keyboard moves to the layer surface", |app| {
        app.focused.as_ref() == Some(&layer_id)
    });

    // Teardown: zone and keyboard return to the layout.
    layer.destroy();
    surface.destroy();
    s.wait_until("window regains the full slot", |app| {
        app.wins[a].size == full
    });
    s.wait_until("keyboard returns to the window", |app| app.focus_is(a));
}

#[test]
fn bottom_layer_panel_visible_and_clickable() {
    // The cozyui contract: a Bottom-layer panel anchored to the right
    // edge with an exclusive zone shrinks the tiling area, composites
    // above the opaque chrome underlay (wallpaper included), and takes
    // the keyboard on click (OnDemand interactivity).
    const PANEL_W: i32 = 300;
    const OUTPUT_H: i32 = 800;
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("window tiled", |app| {
        app.wins[a].activated && app.wins[a].size.0 > 0
    });
    s.wait_until("keyboard on the window", |app| app.focus_is(a));
    let full = s.app.wins[a].size;

    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = s
        .globals
        .bind(&s.qh, 1..=4, ())
        .expect("bind zwlr_layer_shell_v1");
    let surface = s.compositor.create_surface(&s.qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Bottom,
        "splitwm-test-sidebar".into(),
        &s.qh,
        (),
    );
    layer.set_size(PANEL_W as u32, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_exclusive_zone(PANEL_W);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
    surface.commit();
    s.wait_until("layer surface configured", |app| app.layer_configures > 0);

    // A solid red full-strip buffer, so the screenshot can prove the
    // panel's pixels — not the wallpaper — fill the reserved strip.
    let shm: wl_shm::WlShm = s.globals.bind(&s.qh, 1..=1, ()).expect("bind wl_shm");
    let file = tempfile::tempfile().expect("shm backing file");
    let len = PANEL_W * OUTPUT_H * 4;
    // Argb8888 little-endian: B, G, R, A.
    let red: Vec<u8> = [0u8, 0, 255, 255].repeat((PANEL_W * OUTPUT_H) as usize);
    (&file).write_all(&red).expect("fill shm with red");
    let pool = shm.create_pool(file.as_fd(), len, &s.qh, ());
    let buffer = pool.create_buffer(
        0,
        PANEL_W,
        OUTPUT_H,
        PANEL_W * 4,
        wl_shm::Format::Argb8888,
        &s.qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    s.wait_until("window shrinks by the exclusive zone", |app| {
        app.wins[a].size.0 > 0 && app.wins[a].size.0 <= full.0 - PANEL_W + 50
    });

    // Sample mid-strip: the panel's pixels, not the wallpaper, fill it.
    let (px, py) = (OUTPUT_W - PANEL_W / 2, OUTPUT_H / 2);
    assert_eq!(
        s.pixel(px, py),
        [255, 0, 0, 255],
        "the panel strip should show the panel's pixels at ({px}, {py})"
    );

    // OnDemand keyboard: a click inside the strip hands the panel the
    // keyboard; a click back on the tiled client reclaims it.
    let layer_id = surface.id();
    s.wm.cmd(&format!("click {px} {py}"));
    s.wait_until("click hands the panel the keyboard", |app| {
        app.focused.as_ref() == Some(&layer_id)
    });
    s.wm.cmd("click 40 40");
    s.wait_until("keyboard returns to the tiled window", |app| {
        app.focus_is(a)
    });
}

#[test]
fn dock_layer_panel_rides_the_canvas_scroll() {
    // The dock-named Bottom panel (cozyui's native sidebar) gets the
    // XWayland dock's scroll semantics instead of a static pin: the tiled
    // layout keeps the full width (the exclusive zone becomes scroll room
    // past the canvas end), only the overlap strip past the zone shows at
    // scroll 0, scrolling right reveals the rest, clicks land at the
    // scrolled position, and scrolling back tucks it away again.
    const PANEL_W: i32 = 300;
    // The revealable share; the remaining 100 px stay tucked under the
    // canvas edge even when scrolled fully right.
    const ZONE: i32 = 200;
    const OUTPUT_H: i32 = 800;
    const RED: [u8; 4] = [255, 0, 0, 255];
    // Inside the parked overlap strip (right edge, scroll 0)…
    const PARKED_X: i32 = OUTPUT_W - 50;
    // …and inside the zone that only a full right-scroll reveals — right
    // of the canvas end at full scroll (OUTPUT_W - ZONE), where the tiled
    // window can no longer cover it: the panel renders above the chrome
    // but below tiled windows, and clicks route the same way.
    const REVEALED_X: i32 = OUTPUT_W - ZONE + 40;
    const SAMPLE_Y: i32 = OUTPUT_H / 2;

    fn wait_pixel(s: &mut Session, what: &str, x: i32, y: i32, want_red: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Flush the client too: a just-queued commit only reaches the
            // compositor on a roundtrip.
            s.queue.roundtrip(&mut s.app).expect("roundtrip");
            if (s.pixel(x, y) == RED) == want_red {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("window tiled", |app| {
        app.wins[a].activated && app.wins[a].size.0 > 0
    });
    let full = s.app.wins[a].size;

    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = s
        .globals
        .bind(&s.qh, 1..=4, ())
        .expect("bind zwlr_layer_shell_v1");
    let surface = s.compositor.create_surface(&s.qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Bottom,
        // The dock identity: this namespace is what opts the panel in.
        "cozyui".into(),
        &s.qh,
        (),
    );
    layer.set_size(PANEL_W as u32, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_exclusive_zone(ZONE);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
    surface.commit();
    s.wait_until("layer surface configured", |app| app.layer_configures > 0);

    // A solid red full-strip buffer, so screenshots track where the panel
    // actually composites.
    let shm: wl_shm::WlShm = s.globals.bind(&s.qh, 1..=1, ()).expect("bind wl_shm");
    let file = tempfile::tempfile().expect("shm backing file");
    let len = PANEL_W * OUTPUT_H * 4;
    // Argb8888 little-endian: B, G, R, A.
    let red: Vec<u8> = [0u8, 0, 255, 255].repeat((PANEL_W * OUTPUT_H) as usize);
    (&file).write_all(&red).expect("fill shm with red");
    let pool = shm.create_pool(file.as_fd(), len, &s.qh, ());
    let buffer = pool.create_buffer(
        0,
        PANEL_W,
        OUTPUT_H,
        PANEL_W * 4,
        wl_shm::Format::Argb8888,
        &s.qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();

    // Parked at scroll 0: the overlap strip pokes in at the right edge,
    // the zone's share stays past it, offscreen.
    wait_pixel(
        &mut s,
        "overlap strip parked at the right edge",
        PARKED_X,
        SAMPLE_Y,
        true,
    );
    assert_ne!(
        s.pixel(REVEALED_X, SAMPLE_Y),
        RED,
        "the zone's share should still be past the canvas end at scroll 0"
    );

    // Scrolling right (clamped to the dock's scroll room) glides the
    // panel into view.
    s.wm.cmd("scroll 50");
    wait_pixel(
        &mut s,
        "right scroll reveals the panel",
        REVEALED_X,
        SAMPLE_Y,
        true,
    );

    // Let the scroll glide land before clicking: mid-glide the tiled
    // window's right edge still covers REVEALED_X and would swallow the
    // click. The panel's left edge only reaches OUTPUT_W - PANEL_W at
    // full scroll, so a red pixel just past that means the glide is (all
    // but) done and the window is clear of REVEALED_X.
    wait_pixel(
        &mut s,
        "scroll glide lands",
        OUTPUT_W - PANEL_W + 5,
        SAMPLE_Y,
        true,
    );

    // Input follows the scrolled position: OnDemand click-to-focus works
    // where the panel now shows.
    let layer_id = surface.id();
    s.wm.cmd(&format!("click {REVEALED_X} {SAMPLE_Y}"));
    s.wait_until(
        "click at the scrolled position hands the panel the keyboard",
        |app| app.focused.as_ref() == Some(&layer_id),
    );

    // Scrolling back tucks it away again, and the layout never gave up
    // any width for it: the zone was scroll room, not a strut.
    s.wm.cmd("scroll -50");
    wait_pixel(
        &mut s,
        "left scroll tucks the panel away",
        REVEALED_X,
        SAMPLE_Y,
        false,
    );
    s.wait_until("tiled window kept the full slot throughout", |app| {
        app.wins[a].size == full
    });
}

#[test]
fn clipboard_bridges_x11_and_wayland() {
    // The selection bridge: an X client's CLIPBOARD arrives at the focused
    // Wayland client as a wl_data_offer, and a Wayland client's selection
    // is readable from X — but only while an X window holds the keyboard
    // focus (X selection requests carry no identity to authorize by).
    const UTF8: &str = "text/plain;charset=utf-8";
    let mut s = Session::boot();
    let a = s.open_window();
    s.wait_until("keyboard on the window", |app| app.focus_is(a));

    let ddm: wl_data_device_manager::WlDataDeviceManager = s
        .globals
        .bind(&s.qh, 1..=3, ())
        .expect("bind wl_data_device_manager");
    let device = ddm.get_data_device(&s.seat, &s.qh, ());

    // --- X → Wayland: xclip takes the X CLIPBOARD (and lingers as its
    // owner); the offer reaches the focused client with no focus change ---
    s.wm.await_xwayland();
    s.wm.cmd("spawn printf from-x | xclip -selection clipboard -t UTF8_STRING");
    s.wait_until("X selection offered to the client", |app| {
        app.selection.as_ref().is_some_and(|o| {
            app.offer_mimes
                .get(&o.id())
                .is_some_and(|mimes| mimes.iter().any(|m| m == UTF8))
        })
    });
    let offer = s.app.selection.clone().expect("selection offer");
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    offer.receive(UTF8.into(), writer.as_fd());
    drop(writer);
    s.queue.roundtrip(&mut s.app).expect("roundtrip");
    let mut pasted = String::new();
    reader.read_to_string(&mut pasted).expect("read the offer");
    assert_eq!(pasted, "from-x", "X clipboard should paste into Wayland");

    // --- Wayland → X: our data source takes the selection (replacing the
    // X owner); with rofi focused, an X client may read it back ---
    let source = ddm.create_data_source(&s.qh, ());
    source.offer(UTF8.into());
    s.app.paste_payload = "from-wayland".into();
    device.set_selection(Some(&source), s.app.enter_serial);
    s.queue.roundtrip(&mut s.app).expect("roundtrip");

    s.wm.cmd(&format!(
        "spawn env -u WAYLAND_DISPLAY rofi -show drun -pid /tmp/splitwm-test-rofi-clip-{}.pid",
        std::process::id()
    ));
    s.wait_until("rofi maps and takes the keyboard", |app| {
        app.focused.is_none()
    });

    let out = std::env::temp_dir().join(format!("splitwm-test-clip-{}.txt", std::process::id()));
    let out = out.to_str().expect("utf-8 temp path").to_string();
    s.wm.cmd(&format!(
        "spawn xclip -selection clipboard -t UTF8_STRING -o > {out}"
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Keep pumping: the Send event arrives here and must be answered.
        s.queue.roundtrip(&mut s.app).expect("roundtrip");
        if std::fs::read_to_string(&out).is_ok_and(|c| c == "from-wayland") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for X to read the Wayland selection"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_file(&out).ok();
}

#[test]
fn sigterm_ends_the_session() {
    // No quit binding, faithful to master: SIGTERM is the only way out.
    let mut s = Session::boot();
    let w = s.open_window();
    s.wait_until("window mapped", |app| app.wins[w].activated);

    Command::new("kill")
        .args(["-TERM", &s.wm.child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if s.wm.child.try_wait().expect("try_wait").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "compositor ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
}
