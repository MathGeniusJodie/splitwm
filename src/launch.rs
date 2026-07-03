//! App launching support: resolving the taskbar quick-launch entries
//! (`theme::QUICK`) to commands, resolving `.desktop` entries to spawnable
//! commands (the dock autostart), and the freedesktop icon-theme file
//! lookup behind the taskbar's icons. Pure data; the X windows and
//! rendering live in `wm`.

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

/// Resolve an icon name to a PNG file. Absolute paths are used as-is; names
/// are looked up through the configured icon theme and its inheritance
/// chain (see `theme_search_dirs`), then `pixmaps` (a deliberately minimal
/// cut of the freedesktop icon-theme lookup — PNG only, no SVG).
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
    let data = data_dirs();
    // Theme-major order: every base dir of a theme is preferred over any
    // dir of the theme it inherits from, per the freedesktop lookup.
    for rel in theme_search_dirs() {
        for d in &data {
            let p = d.join("icons").join(rel).join(&file);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for d in &data {
        let p = d.join("pixmaps").join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// One lookup directory inside an icon theme, from its `index.theme`.
struct ThemeDir {
    path: String,
    size: i32,
    scale: i32,
}

/// Rank a theme directory for lookup order: unscaled dirs before `@2x`
/// ones, then by size — the smallest size >= 48 first (a clean downscale to
/// the ~36px taskbar tile), then the largest smaller size (the mildest
/// upscale).
fn dir_rank(d: &ThemeDir) -> (bool, i32) {
    let size_rank = if d.size >= 48 { d.size - 48 } else { 10_000 - d.size };
    (d.scale != 1, size_rank)
}

/// Parse an `index.theme`: its ranked lookup directories and the themes it
/// inherits from.
fn parse_index_theme(text: &str) -> (Vec<ThemeDir>, Vec<String>) {
    let mut group = "";
    let mut directories: Vec<&str> = Vec::new();
    let mut inherits: Vec<String> = Vec::new();
    // Per-directory groups carry the dir's nominal Size and Scale.
    let mut props: HashMap<&str, (i32, i32)> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            group = g;
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if group == "Icon Theme" {
            let items = || v.split(',').map(str::trim).filter(|s| !s.is_empty());
            match k {
                "Directories" => directories = items().collect(),
                "Inherits" => inherits = items().map(String::from).collect(),
                _ => {}
            }
        } else {
            let e = props.entry(group).or_insert((0, 1));
            match (k, v.parse::<i32>()) {
                ("Size", Ok(n)) => e.0 = n,
                ("Scale", Ok(n)) => e.1 = n,
                _ => {}
            }
        }
    }
    let mut dirs: Vec<ThemeDir> = directories
        .into_iter()
        .filter_map(|d| {
            // A directory without a Size can't be ranked; skip it.
            let &(size, scale) = props.get(d)?;
            (size > 0).then(|| ThemeDir {
                path: d.to_string(),
                size,
                scale,
            })
        })
        .collect();
    dirs.sort_by_key(dir_rank);
    (dirs, inherits)
}

/// The user's icon theme name from GTK's `gtk-3.0/settings.ini` — the WM
/// links no GTK, but that ini is where the theme is conventionally
/// configured per user.
fn configured_icon_theme() -> Option<String> {
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
}

/// The flattened icon search directories (`<theme>/<subdir>`, relative to a
/// data dir's `icons/`) for the configured icon theme and everything it
/// inherits, ending at hicolor. Resolved once — switching icon themes takes
/// a WM restart.
fn theme_search_dirs() -> &'static [std::path::PathBuf] {
    static DIRS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let data = data_dirs();
        let mut queue = vec![configured_icon_theme().unwrap_or_else(|| "hicolor".into())];
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            while let Some(theme) = queue.get(i).cloned() {
                i += 1;
                if !seen.insert(theme.clone()) {
                    continue;
                }
                let Some(index) = data
                    .iter()
                    .find_map(|d| std::fs::read_to_string(d.join("icons").join(&theme).join("index.theme")).ok())
                else {
                    continue;
                };
                let (dirs, inherits) = parse_index_theme(&index);
                out.extend(dirs.into_iter().map(|d| std::path::Path::new(&theme).join(d.path)));
                queue.extend(inherits);
            }
            // hicolor is the spec's implicit final fallback; visit it even
            // when no Inherits chain reached it.
            if seen.contains("hicolor") {
                break;
            }
            queue.push("hicolor".into());
        }
        // No readable index.theme anywhere (bare setups): fall back to
        // hicolor's conventional layout blind.
        if out.is_empty() {
            for size in [48, 64, 32, 128, 256, 24, 22, 16] {
                out.push(format!("hicolor/{size}x{size}/apps").into());
            }
        }
        out
    })
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

#[cfg(test)]
mod tests {
    use super::{clean_exec, dir_rank, parse_index_theme};

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
    fn index_theme_dirs_are_parsed_and_ranked() {
        let text = "[Icon Theme]\nName=T\nDirectories=apps/16,apps/48@2x,places/64,apps/48,nosize\nInherits=Parent, hicolor\n\n\
            [apps/16]\nSize=16\nContext=Applications\n\
            [apps/48@2x]\nSize=48\nScale=2\n\
            [places/64]\nSize=64\n\
            [apps/48]\nSize=48\n";
        let (dirs, inherits) = parse_index_theme(text);
        let order: Vec<&str> = dirs.iter().map(|d| d.path.as_str()).collect();
        // Smallest >= 48 first, @2x behind every unscaled dir, no-Size
        // dirs dropped.
        assert_eq!(order, ["apps/48", "places/64", "apps/16", "apps/48@2x"]);
        assert_eq!(inherits, ["Parent", "hicolor"]);
    }

    #[test]
    fn dir_rank_prefers_mild_downscale_over_any_upscale() {
        let d = |size, scale| super::ThemeDir {
            path: String::new(),
            size,
            scale,
        };
        assert!(dir_rank(&d(48, 1)) < dir_rank(&d(64, 1)));
        assert!(dir_rank(&d(256, 1)) < dir_rank(&d(32, 1)));
        assert!(dir_rank(&d(32, 1)) < dir_rank(&d(16, 1)));
        assert!(dir_rank(&d(16, 1)) < dir_rank(&d(48, 2)));
    }
}
