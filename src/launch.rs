//! App launching support: resolving the taskbar quick-launch entries
//! (`theme::QUICK`) to commands, resolving `.desktop` entries to spawnable
//! commands (the dock autostart, via `freedesktop-desktop-entry`), and the
//! freedesktop icon-theme lookup behind the taskbar's icons (via
//! `freedesktop-icons`). Pure data; the X windows and rendering live in `wm`.

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

/// Resolve `<id>.desktop` from the standard application dirs into a
/// spawnable command: its `Exec` line with field codes expanded away
/// (`DesktopEntry::parse_exec`), each argument shell-quoted, prefixed with
/// a `cd` into its `Path=` working directory when one is set. Unlike the
/// quick-launch scan this ignores NoDisplay/Hidden — autostart doesn't care
/// about launcher visibility.
pub fn desktop_entry_cmd(id: &str) -> Option<String> {
    use freedesktop_desktop_entry as fde;
    let file = format!("{id}.desktop");
    let path = fde::default_paths()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())?;
    let entry = fde::DesktopEntry::from_path(path, None::<&[String]>).ok()?;
    let args = entry.parse_exec().ok()?;
    if args.is_empty() {
        return None;
    }
    let exec = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    Some(match entry.desktop_entry("Path") {
        Some(p) if !p.is_empty() => format!("cd {} && {exec}", shell_quote(p)),
        _ => exec,
    })
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
