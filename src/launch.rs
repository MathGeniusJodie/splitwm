//! App launching support: resolving the taskbar quick-launch entries
//! (`theme::QUICK`) to commands, detached spawning with the session's
//! display env injected per child, and the freedesktop icon-theme lookup
//! behind the taskbar's icons (via `freedesktop-icons`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One quick-launch taskbar entry, resolved from `theme::QUICK`.
pub struct QuickLaunch {
    pub label: &'static str,
    /// The command to spawn (the entry's env override or its default).
    pub cmd: String,
    /// Freedesktop icon-theme name for `find_icon_file`.
    pub icon: &'static str,
    pub show: crate::theme::ShowWhen,
}

/// Single-quote `s` for use as one `sh` word.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The session env handed to every spawned child. Kept out of the process
/// environment on purpose: `std::env::set_var` after threads exist (the
/// icon fetchers, the notification daemon) races their `getenv`s, so
/// `spawn` injects the pair per child instead.
///
/// `WAYLAND_DISPLAY` is our socket, set once at startup. `DISPLAY` is our
/// XWayland server, recorded when it reports Ready; until then children get
/// *no* `DISPLAY` — an X11-pinned child must reach our XWayland or nothing,
/// never the host X server a nested run inherited.
static WAYLAND_DISPLAY: OnceLock<std::ffi::OsString> = OnceLock::new();
static X11_DISPLAY: OnceLock<String> = OnceLock::new();

/// Record the compositor's Wayland socket name for spawned children.
pub fn set_wayland_display(socket: std::ffi::OsString) {
    let _ = WAYLAND_DISPLAY.set(socket);
}

/// Record the compositor's X11 display (`:N`) once XWayland is ready.
pub fn set_x11_display(display: String) {
    let _ = X11_DISPLAY.set(display);
}

/// Resolve an icon name to an image file. Absolute paths are used as-is;
/// names are looked up through the configured icon theme and its
/// inheritance chain, then hicolor and `pixmaps`. Any format the lookup
/// returns (PNG, SVG, XPM) is fine: `icon::load_image` converts non-PNG
/// files through `ImageMagick`.
pub fn find_icon_file(icon: &str) -> Option<std::path::PathBuf> {
    // Repeated lookups re-resolve the same icon names, so cache results
    // keyed by the raw `icon` string. Hits are trusted forever (nothing
    // uninstalls a theme mid-session), but *misses* expire: an icon theme
    // installed while we run should start resolving without a WM restart.
    const NEG_TTL: std::time::Duration = std::time::Duration::from_secs(60);
    type Entry = (Option<std::path::PathBuf>, std::time::Instant);
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((hit, at)) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(icon)
    {
        if hit.is_some() || at.elapsed() < NEG_TTL {
            return hit.clone();
        }
    }
    let found = find_icon_file_uncached(icon);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::render::insert_capped(
        &mut cache,
        1024,
        icon.to_string(),
        (found.clone(), std::time::Instant::now()),
    );
    found
}

fn find_icon_file_uncached(icon: &str) -> Option<std::path::PathBuf> {
    if icon.starts_with('/') {
        let p = std::path::PathBuf::from(icon);
        return p.is_file().then_some(p);
    }
    // 48px: the smallest clean downscale to the ~36px taskbar tile.
    let lookup = freedesktop_icons::lookup(icon).with_size(48);
    match configured_icon_theme() {
        Some(theme) => lookup.with_theme(&theme).find(),
        None => lookup.find(),
    }
}

/// The user's icon theme name from GTK's `gtk-3.0/settings.ini` — the WM
/// links no GTK, but that ini is where the theme is conventionally
/// configured per user. Resolved once — switching icon themes takes a WM
/// restart.
fn configured_icon_theme() -> Option<String> {
    static THEME: OnceLock<Option<String>> = OnceLock::new();
    THEME
        .get_or_init(|| {
            let config = std::env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
            let text = std::fs::read_to_string(format!("{config}/gtk-3.0/settings.ini")).ok()?;
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == "gtk-icon-theme-name" {
                        return Some(v.trim().trim_matches('"').to_string());
                    }
                }
            }
            None
        })
        .clone()
}

/// Resolve every `theme::QUICK` entry: its env override or default command,
/// carrying the entry's icon name and visibility rule through.
pub fn quick_launches() -> Vec<QuickLaunch> {
    crate::theme::QUICK
        .iter()
        .map(|q| QuickLaunch {
            label: q.label,
            cmd: std::env::var(q.env).unwrap_or_else(|_| q.default.to_string()),
            icon: q.icon,
            show: q.show,
        })
        .collect()
}

/// Publish `key=value` to the systemd user manager and the D-Bus
/// activation environment, so services started by either (portals,
/// voxtype, anything dbus-activated) can reach the session's displays —
/// they inherit the manager's environment, not ours, and per-child
/// injection (`spawn`) can't reach them. Only the login session may call
/// this (see `Backend::is_session`): a nested or headless run publishing
/// its socket would hijack the real session's.
pub fn publish_session_env(key: &str, value: &std::ffi::OsStr) {
    let mut pair = std::ffi::OsString::from(format!("{key}="));
    pair.push(value);
    run_off_thread(move || {
        for argv in [
            &["systemctl", "--user", "set-environment"][..],
            &["dbus-update-activation-environment"][..],
        ] {
            run_logged(argv, &pair);
        }
    });
}

/// Publish the Wayland socket session-wide, then activate
/// `splitwm-session.target` (which `BindsTo=graphical-session.target`, the
/// systemd-blessed signal that a graphical session is up) so units
/// installed into the graphical session start. One sequential chain: the
/// environment must land in the manager before the target's units spawn,
/// or they'd start display-blind. Missing unit or no systemd is fine: the
/// failure only costs those units their autostart.
pub fn announce_session(socket: std::ffi::OsString) {
    let mut pair = std::ffi::OsString::from("WAYLAND_DISPLAY=");
    pair.push(&socket);
    run_off_thread(move || {
        for argv in [
            &["systemctl", "--user", "set-environment"][..],
            &["dbus-update-activation-environment"][..],
        ] {
            run_logged(argv, &pair);
        }
        run_logged(
            &["systemctl", "--user", "start", "--no-block"],
            std::ffi::OsStr::new("splitwm-session.target"),
        );
    });
}

/// Run `argv` + `arg` to completion, logging (never failing) on a missing
/// binary or nonzero exit — non-systemd setups lack these commands.
fn run_logged(argv: &[&str], arg: &std::ffi::OsStr) {
    match std::process::Command::new(argv[0])
        .args(&argv[1..])
        .arg(arg)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!("{argv:?} {arg:?} exited with {status}"),
        Err(e) => tracing::warn!("failed to spawn {argv:?}: {e}"),
    }
}

/// The waits above don't belong on the event loop (a wedged user manager
/// would stall compositing); one short-lived thread per announcement is
/// cheap and they're rare (session start, XWayland ready).
fn run_off_thread(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

/// Detached spawn of `cmd`. `sh` reaps its own fork thanks to the trailing
/// `&`; the outer `sh` is reaped off-thread so the compositor never leaves
/// a zombie per launch and never waits on the event loop.
///
/// When systemd-run is available the command is placed in its own
/// transient scope under app.slice, like a desktop-environment launcher
/// would; Chromium/Electron apps otherwise try to move themselves out of
/// the shared session scope and log a spurious `UnitExists` error.
pub fn spawn(cmd: &str) {
    // Both paths hand `cmd` to `/bin/sh -c` as one quoted word, so a
    // command line containing `;`/`&&` behaves identically whether or
    // not systemd-run is available (a bare `{cmd} &` fallback would
    // background only the last statement of a compound command).
    let line = if have_systemd_run() {
        format!(
            "systemd-run --user --scope --slice=app.slice --collect --quiet -- /bin/sh -c {} &",
            shell_quote(cmd)
        )
    } else {
        format!("/bin/sh -c {} &", shell_quote(cmd))
    };
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(line);
    if let Some(socket) = WAYLAND_DISPLAY.get() {
        command.env("WAYLAND_DISPLAY", socket);
    }
    match X11_DISPLAY.get() {
        Some(display) => command.env("DISPLAY", display),
        None => command.env_remove("DISPLAY"),
    };
    match command.spawn() {
        Ok(mut sh) => {
            // Reap the short-lived `sh` off-thread: it exits as soon as
            // it has forked, but even that wait doesn't belong on the
            // event loop.
            std::thread::spawn(move || {
                let _ = sh.wait();
            });
        }
        Err(e) => tracing::warn!("failed to spawn '{cmd}': {e}"),
    }
}

/// Whether `systemd-run` exists and a user manager is reachable. Checked
/// once and cached; false on non-systemd setups. The probe is a
/// synchronous D-Bus round trip, so `main` warms it at startup rather than
/// letting the first launch pay for it inside the event loop — which is
/// exactly why it must be deadline-bounded: a wedged user manager (a hung
/// D-Bus socket answers nothing, ever) would otherwise hang the
/// compositor before it manages a single window. A timed-out probe counts
/// as "no systemd-run"; launches then skip the transient scope, the same
/// degradation as any non-systemd session.
pub fn have_systemd_run() -> bool {
    use std::sync::OnceLock;
    static HAVE: OnceLock<bool> = OnceLock::new();
    *HAVE.get_or_init(|| {
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        let Ok(mut child) = std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--collect", "--quiet", "--", "true"])
            .spawn()
        else {
            // No systemd-run binary at all.
            return false;
        };
        // std has no wait-with-timeout, so poll `try_wait` against a
        // deadline. 10ms granularity is plenty: a healthy probe answers
        // in single-digit milliseconds, and only startup ever blocks on
        // this.
        let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // Deadline passed (manager wedged) or the child is
                // unwaitable: kill and reap what we can, then report no
                // systemd — a launch degraded to plain `sh` beats a
                // compositor that never starts.
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    })
}
