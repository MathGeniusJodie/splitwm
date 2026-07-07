//! Asserting integration tests over the real Wayland socket: the itest.sh
//! analog. Each test boots the actual compositor binary on the headless
//! backend (`SPLITWM_HEADLESS=1`), connects as an ordinary client, and
//! asserts what a client can observe — configure sizes, xdg activated
//! state, keyboard focus, and close semantics. WM chords are injected over
//! the debug channel (`SPLITWM_DEBUG_CHANNEL=1`, stdin), which acks each
//! command on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
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
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent-splitwm-test")
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
    /// zwlr_layer_surface configures seen (each acked on arrival).
    layer_configures: u32,
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
            wl_keyboard::Event::Enter { surface, .. } => app.focused = Some(surface.id()),
            wl_keyboard::Event::Leave { .. } => app.focused = None,
            _ => {}
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

/// A booted compositor plus one connected test client.
struct Session {
    wm: Wm,
    queue: EventQueue<App>,
    qh: QueueHandle<App>,
    app: App,
    globals: wayland_client::globals::GlobalList,
    compositor: wl_compositor::WlCompositor,
    wm_base: xdg_wm_base::XdgWmBase,
    pool: wl_shm_pool::WlShmPool,
    /// Backs `pool`; the fd must outlive it.
    #[allow(dead_code)]
    pool_file: std::fs::File,
}

/// Buffer geometry for test windows: content is irrelevant (all zeroes),
/// the commit just has to carry *a* buffer so the compositor maps and
/// classifies the window.
const BUF: i32 = 64;

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
        let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=5, ()).expect("bind wl_seat");

        let pool_file = tempfile::tempfile().expect("shm backing file");
        let pool_len = BUF * BUF * 4;
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
            if pred(&self.app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Create a toplevel and map it (initial commit, configure, buffer
    /// commit) — the standard xdg-shell dance every real client performs.
    fn open_window(&mut self) -> usize {
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
            activated: false,
            configures: 0,
            closed: false,
        });
        self.wait_until("initial configure", |app| app.wins[win].configures > 0);
        let buffer = self
            .pool
            .create_buffer(0, BUF, BUF, BUF * 4, wl_shm::Format::Argb8888, &self.qh, ());
        self.app.wins[win].surface.attach(Some(&buffer), 0, 0);
        self.app.wins[win].surface.commit();
        win
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
    assert!(full.1 > 500, "tiled height should be most of the output, got {full:?}");
    s.wait_until("keyboard enters w1", |app| app.focus_is(a));

    // --- displacement: a second client takes the slot; the first is
    // stashed to the taskbar and loses activation, not its connection ---
    let b = s.open_window();
    s.wait_until("w2 activated at the same slot size", |app| {
        app.wins[b].activated && app.wins[b].size == full
    });
    s.wait_until("displaced w1 deactivated", |app| !app.wins[a].activated);
    s.wait_until("keyboard moves to w2", |app| app.focus_is(b));

    // --- split: the occupant keeps the golden-ratio major share
    // (SPLIT_RATIO 0.618), the new empty split gets the rest ---
    s.wm.key("super+v");
    s.wait_until("w2 reconfigured to the 0.618 share", |app| {
        let w = app.wins[b].size.0;
        w > 0 && w < full.0 * 7 / 10
    });
    let major_w = s.app.wins[b].size.0;

    // --- taskbar cycle: focus the new empty split, cycle the stashed w1
    // into it; w1 comes back activated at the split's minor share ---
    s.wm.key("super+Right");
    s.wait_until("empty split focused: both windows deactivated", |app| {
        !app.wins[a].activated && !app.wins[b].activated
    });
    s.wm.key("super+bracketright");
    s.wait_until("w1 restored into the split and focused", |app| {
        app.wins[a].activated && app.focus_is(a)
    });
    let minor_w = s.app.wins[a].size.0;
    assert!(
        minor_w > 0 && minor_w < major_w,
        "restored w1 should get the minor share: {minor_w} vs {major_w}"
    );
    // Shares in golden proportion, within chrome-inset slack.
    let ratio = f64::from(major_w) / f64::from(major_w + minor_w);
    assert!(
        (ratio - 0.618).abs() < 0.03,
        "split shares should sit at SPLIT_RATIO, got {ratio:.3}"
    );

    // --- polite close: the chord asks the focused client, and only asks ---
    assert!(!s.app.wins[a].closed);
    s.wm.key("super+shift+c");
    s.wait_until("w1 received xdg_toplevel.close", |app| app.wins[a].closed);
    assert!(!s.app.wins[b].closed, "only the focused window is asked to close");
    // Honour it. The emptied split keeps layout focus (as on master), so
    // nothing holds the keyboard until a deliberate move points it at w2.
    s.app.wins[a].toplevel.destroy();
    s.app.wins[a].surface.destroy();
    s.wait_until("keyboard focus parks on the emptied split", |app| {
        app.focused.is_none() && !app.wins[b].activated
    });
    s.wm.key("super+Left");
    s.wait_until("focus moves back to w2", |app| {
        app.wins[b].activated && app.focus_is(b)
    });
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

    // Sample mid-strip. `.rgba` makes `shot` write raw R,G,B,A rows, so
    // the assertion reads bytes instead of parsing encoder output.
    let (px, py) = (OUTPUT_W - PANEL_W / 2, OUTPUT_H / 2);
    let path = std::env::temp_dir().join(format!("splitwm-test-bottom-{}.rgba", std::process::id()));
    let path = path.to_str().expect("utf-8 temp path").to_string();
    s.wm.cmd(&format!("shot {path}"));
    let frame = std::fs::read(&path).expect("read screenshot");
    std::fs::remove_file(&path).ok();
    let at = ((py * OUTPUT_W + px) * 4) as usize;
    assert_eq!(
        &frame[at..at + 4],
        &[255, 0, 0, 255],
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
    s.wait_until("keyboard returns to the tiled window", |app| app.focus_is(a));
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
