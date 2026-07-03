//! App launching support: resolving the taskbar quick-launch entries
//! (`theme::QUICK`) to commands and icons, resolving `.desktop` entries to
//! spawnable commands (the dock autostart), and the minimal freedesktop
//! icon-file lookup behind both. Pure data; the X windows and rendering
//! live in `wm`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One quick-launch taskbar entry, resolved from `theme::QUICK`.
pub struct QuickLaunch {
    pub label: &'static str,
    /// The command to spawn (the entry's env override or its default).
    pub cmd: String,
    /// Icon name/path for `find_icon_file`, when one could be inferred.
    pub icon: Option<String>,
}

/// A scanned `.desktop` application: just the facts icon inference needs.
struct App {
    exec: String,
    icon: String,
}

/// Strip desktop-entry field codes (`%f`, `%U`, …) from an Exec line;
/// `%%` is the spec's escape for a literal `%`. Only the spec's field-code
/// letters are treated as codes — any other `%x` pair passes through
/// literally rather than being eaten (an Exec like `convert 50%x50%` must
/// not lose characters).
fn clean_exec(exec: &str) -> String {
    const FIELD_CODES: &str = "fFuUdDnNickvm";
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(c2) if FIELD_CODES.contains(c2) => {}
            Some(c2) => {
                out.push('%');
                out.push(c2);
            }
            None => out.push('%'),
        }
    }
    out.trim().to_string()
}

/// Parse one `.desktop` file's `[Desktop Entry]` group into `(exec, icon)`
/// when it is a displayable Application with both set.
fn parse_desktop(text: &str) -> Option<(String, String)> {
    let mut in_entry = false;
    let (mut exec, mut icon): (Option<String>, Option<String>) = (None, None);
    let (mut no_display, mut hidden, mut is_app) = (false, false, true);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Exec" if exec.is_none() => exec = Some(clean_exec(v.trim())),
            "Icon" if icon.is_none() => icon = Some(v.trim().to_string()),
            "NoDisplay" => no_display = v.trim().eq_ignore_ascii_case("true"),
            "Hidden" => hidden = v.trim().eq_ignore_ascii_case("true"),
            "Type" => is_app = v.trim() == "Application",
            _ => {}
        }
    }
    if no_display || hidden || !is_app {
        return None;
    }
    let exec = exec.filter(|e| !e.is_empty())?;
    let icon = icon.filter(|i| !i.is_empty())?;
    Some((exec, icon))
}

/// Standard XDG data directories (per-user first, so it wins).
fn data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    // A set-but-empty XDG_DATA_HOME counts as unset per the XDG spec; fall
    // back to the spec default, $HOME/.local/share.
    let user = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| format!("{h}/.local/share"))
        });
    if let Some(d) = user {
        dirs.push(std::path::PathBuf::from(d));
    }
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in system.split(':') {
        if !d.is_empty() {
            dirs.push(std::path::PathBuf::from(d));
        }
    }
    dirs
}

/// Standard application directories (XDG data dirs + per-user).
fn app_dirs() -> Vec<std::path::PathBuf> {
    data_dirs()
        .into_iter()
        .map(|d| d.join("applications"))
        .collect()
}

/// Icon sizes worth loading for taskbar-tile icons, best (largest useful)
/// first: the tiles draw the icon at ~36px, so a clean downscale from 48
/// beats a blocky 16px upscale.
const ICON_SIZES: &[&str] = &[
    "48x48", "64x64", "32x32", "128x128", "256x256", "24x24", "22x22", "16x16",
];

/// Resolve an `Icon=` value to a PNG file. Absolute paths are used as-is;
/// names are looked up in the hicolor theme's `apps` dirs and `pixmaps` (a
/// deliberately minimal cut of the freedesktop icon-theme lookup — no theme
/// inheritance, PNG only).
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
        return (p.extension().is_some_and(|x| x == "png") && p.is_file()).then_some(p);
    }
    let file = format!("{icon}.png");
    for d in data_dirs() {
        for size in ICON_SIZES {
            let p = d.join("icons/hicolor").join(size).join("apps").join(&file);
            if p.is_file() {
                return Some(p);
            }
        }
        let p = d.join("pixmaps").join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Scan every `.desktop` file once into a flat app list (earlier, more
/// user-specific dirs win per app id) — the corpus `quick_icon` infers
/// icons from.
fn scan() -> Vec<App> {
    let mut apps = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in app_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !seen.insert(stem.to_string()) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some((exec, icon)) = parse_desktop(&text) {
                    apps.push(App { exec, icon });
                }
            }
        }
    }
    apps
}

/// Resolve `<id>.desktop` from the standard application dirs into a
/// spawnable command: its cleaned `Exec` line, prefixed with a `cd` into its
/// `Path=` working directory when one is set. Unlike the quick-launch scan
/// this ignores NoDisplay/Hidden — autostart doesn't care about launcher
/// visibility.
pub fn desktop_entry_cmd(id: &str) -> Option<String> {
    let file = app_dirs()
        .into_iter()
        .map(|d| d.join(format!("{id}.desktop")))
        .find_map(|p| std::fs::read_to_string(p).ok())?;
    let (mut in_entry, mut exec, mut path) = (false, None, None);
    for line in file.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Exec" if exec.is_none() => exec = Some(clean_exec(v.trim())),
            "Path" if path.is_none() => path = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let exec = exec.filter(|e| !e.is_empty())?;
    Some(match path {
        Some(p) if !p.is_empty() => {
            // Single-quote the Path= value for the shell: close the quote,
            // emit an escaped literal quote, reopen it.
            let escaped = p.replace('\'', "'\\''");
            format!("cd '{escaped}' && {exec}")
        }
        _ => exec,
    })
}

/// Icon name for a quick-launch command: the icon of the scanned app whose
/// Exec starts with the same program, else the program's own name (themed
/// icons are often named after the binary).
fn quick_icon(apps: &[App], cmd: &str) -> Option<String> {
    let prog = cmd.split_whitespace().next()?;
    let bin = prog.rsplit('/').next()?;
    for a in apps {
        let app_prog = a.exec.split_whitespace().next().unwrap_or("");
        if app_prog.rsplit('/').next() == Some(bin) {
            return Some(a.icon.clone());
        }
    }
    Some(bin.to_string())
}

/// Resolve every `theme::QUICK` entry (env override or default command, plus
/// an inferred icon). Scans the system's `.desktop` files once.
pub fn quick_launches() -> Vec<QuickLaunch> {
    let apps = scan();
    crate::theme::QUICK
        .iter()
        .map(|q| {
            let cmd = std::env::var(q.env).unwrap_or_else(|_| q.default.to_string());
            let icon = quick_icon(&apps, &cmd);
            QuickLaunch {
                label: q.label,
                cmd,
                icon,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{clean_exec, parse_desktop, quick_icon, App};

    #[test]
    fn clean_exec_strips_field_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("app --flag %F --other"), "app --flag  --other");
        assert_eq!(clean_exec("echo 100%% done"), "echo 100% done");
    }

    #[test]
    fn clean_exec_keeps_non_field_code_percents() {
        // Only the spec's field-code letters are codes; other %x pairs (and
        // a trailing %) must pass through, not lose characters.
        assert_eq!(clean_exec("convert 50%x50% a.png"), "convert 50%x50% a.png");
        assert_eq!(clean_exec("echo 100%"), "echo 100%");
    }

    #[test]
    fn parses_a_normal_application() {
        let text = "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo %U\nIcon=foo\nCategories=Network;WebBrowser;\n";
        let (exec, icon) = parse_desktop(text).unwrap();
        assert_eq!(exec, "foo");
        assert_eq!(icon, "foo");
    }

    #[test]
    fn hidden_nodisplay_and_non_apps_are_skipped() {
        for extra in ["NoDisplay=true", "Hidden=true", "Type=Link"] {
            let text =
                format!("[Desktop Entry]\nType=Application\nName=X\nExec=x\nIcon=x\n{extra}\n");
            assert!(parse_desktop(&text).is_none(), "{extra} should filter");
        }
    }

    #[test]
    fn keys_outside_desktop_entry_group_are_ignored() {
        let text = "[Desktop Action new]\nExec=evil\n[Desktop Entry]\nType=Application\nName=A\nExec=good\nIcon=a\n";
        let (exec, _) = parse_desktop(text).unwrap();
        assert_eq!(exec, "good");
    }

    #[test]
    fn quick_icon_matches_by_binary_and_falls_back_to_it() {
        let apps = vec![App {
            exec: "/usr/bin/firefox --new-window".into(),
            icon: "firefox-icon".into(),
        }];
        assert_eq!(
            quick_icon(&apps, "firefox https://x").as_deref(),
            Some("firefox-icon")
        );
        assert_eq!(
            quick_icon(&apps, "alacritty -e sh").as_deref(),
            Some("alacritty")
        );
    }
}
