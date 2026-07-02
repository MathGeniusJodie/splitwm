//! Application launcher menu: scans freedesktop `.desktop` entries and groups
//! them into the same category structure the reference's `menu.lua` builds, so
//! clicking a leaf's "+" opens a cascading app menu (quick items + category
//! submenus). Pure data + layout; the X windows and rendering live in `wm`.

use std::collections::BTreeMap;

/// One launchable application.
#[derive(Clone)]
pub struct App {
    pub name: String,
    pub exec: String,
    /// `Icon=` value: a themed icon name or an absolute path.
    pub icon: Option<String>,
}

/// What activating a menu row does.
#[derive(Clone)]
pub enum Item {
    /// Spawn this command (appended into the target leaf).
    Launch(String),
    /// Open the submenu with this index.
    Submenu(usize),
    /// Inert divider row.
    Separator,
}

/// A single column of rows (the main menu or one category submenu).
pub struct Menu {
    pub labels: Vec<String>,
    pub items: Vec<Item>,
    /// Rows that open a submenu get a trailing ▸ arrow.
    pub arrows: Vec<bool>,
    /// Per-row icon name/path (from the desktop entry's `Icon=`), resolved
    /// and decoded lazily by `wm` when the row first becomes visible.
    pub icons: Vec<Option<String>>,
}

impl Menu {
    fn new() -> Self {
        Self {
            labels: Vec::new(),
            items: Vec::new(),
            arrows: Vec::new(),
            icons: Vec::new(),
        }
    }

    fn push(&mut self, label: String, item: Item, arrow: bool, icon: Option<String>) {
        self.labels.push(label);
        self.items.push(item);
        self.arrows.push(arrow);
        self.icons.push(icon);
    }
}

// Menu-frame geometry, shared by the renderer (drawing) and `wm` (window
// placement and row hit-testing).
pub const MENU_ROW_H: i32 = 26;
pub const MENU_BORDER: i32 = 8;

/// Outer (window) size of a menu frame holding `rows` rows of `content_w`.
pub const fn frame_size(rows: i32, content_w: i32) -> (i32, i32) {
    (
        content_w + 2 * MENU_BORDER,
        rows * MENU_ROW_H + 2 * MENU_BORDER,
    )
}

/// The whole menu tree: row 0.. of `main` may reference `subs` by index.
pub struct MenuTree {
    pub main: Menu,
    pub subs: Vec<Menu>,
}

/// freedesktop main-category → display name, in the reference's sorted order.
const CATEGORIES: &[(&str, &str)] = &[
    ("AudioVideo", "AudioVideo"),
    ("Development", "Development"),
    ("Education", "Education"),
    ("Game", "Game"),
    ("Graphics", "Graphics"),
    ("Network", "Network"),
    ("Office", "Office"),
    ("Settings", "Settings"),
    ("System", "System"),
    ("Utility", "Utility"),
];

fn first_main_category(cats: &str) -> String {
    for c in cats.split(';') {
        for (key, disp) in CATEGORIES {
            if c == *key {
                return (*disp).to_string();
            }
        }
    }
    "Other".to_string()
}

/// Strip desktop-entry field codes (`%f`, `%U`, …) from an Exec line;
/// `%%` is the spec's escape for a literal `%`.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            if chars.next() == Some('%') {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Parse one `.desktop` file's `[Desktop Entry]` group. Returns
/// `(name, exec, category, icon)` when it is a displayable Application.
fn parse_desktop(text: &str) -> Option<(String, String, String, Option<String>)> {
    let mut in_entry = false;
    let (mut name, mut exec, mut cats, mut icon) = (None, None, String::new(), None);
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
            "Name" if name.is_none() => name = Some(v.trim().to_string()),
            "Exec" if exec.is_none() => exec = Some(clean_exec(v.trim())),
            "Categories" => cats = v.trim().to_string(),
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
    let exec = exec?;
    if exec.is_empty() {
        return None;
    }
    Some((
        name?,
        exec,
        first_main_category(&cats),
        icon.filter(|i| !i.is_empty()),
    ))
}

/// Standard XDG data directories (per-user first, so it wins).
fn data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let data =
            std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
        dirs.push(std::path::PathBuf::from(data));
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
    data_dirs().into_iter().map(|d| d.join("applications")).collect()
}

/// Icon sizes worth loading for 16px menu rows, best first: native 16, then
/// clean or near-clean downscales.
const ICON_SIZES: &[&str] = &[
    "16x16", "32x32", "24x24", "22x22", "48x48", "64x64", "128x128", "256x256",
];

/// Resolve a desktop-entry `Icon=` value to a PNG file. Absolute paths are
/// used as-is; names are looked up in the hicolor theme's `apps` dirs and
/// `pixmaps` (a deliberately minimal cut of the freedesktop icon-theme
/// lookup — no theme inheritance, PNG only).
pub fn find_icon_file(icon: &str) -> Option<std::path::PathBuf> {
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

/// Scan every `.desktop` file once, grouped by display category and sorted.
fn scan() -> BTreeMap<String, Vec<App>> {
    let mut by_cat: BTreeMap<String, Vec<App>> = BTreeMap::new();
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
            // Earlier (more user-specific) dirs win per app id.
            if !seen.insert(stem.to_string()) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some((name, exec, cat, icon)) = parse_desktop(&text) {
                    by_cat.entry(cat).or_default().push(App { name, exec, icon });
                }
            }
        }
    }
    for apps in by_cat.values_mut() {
        apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    }
    by_cat
}

/// Resolve `<id>.desktop` from the standard application dirs into a
/// spawnable command: its cleaned `Exec` line, prefixed with a `cd` into its
/// `Path=` working directory when one is set. Unlike the launcher scan this
/// ignores NoDisplay/Hidden — autostart doesn't care about menu visibility.
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
        Some(p) if !p.is_empty() => format!("cd '{p}' && {exec}"),
        _ => exec,
    })
}

/// A quick-launch shortcut shown at the top of the main menu.
struct Quick {
    label: &'static str,
    env: &'static str,
    default: &'static str,
}

const QUICK: &[Quick] = &[
    Quick {
        label: "Terminal",
        env: "TERMINAL",
        default: "xterm",
    },
    Quick {
        label: "Browser",
        env: "BROWSER",
        default: "xdg-open https://",
    },
    Quick {
        label: "Files",
        env: "FILEMANAGER",
        default: "xdg-open .",
    },
    Quick {
        label: "Obsidian",
        env: "OBSIDIAN",
        default: "obsidian",
    },
    Quick {
        label: "Claude",
        env: "CLAUDE_DESKTOP",
        default: "claude-desktop",
    },
];

/// Icon name for a quick-launch command: the icon of the scanned app whose
/// Exec starts with the same program, else the program's own name (themed
/// icons are often named after the binary).
fn quick_icon(by_cat: &BTreeMap<String, Vec<App>>, cmd: &str) -> Option<String> {
    let prog = cmd.split_whitespace().next()?;
    let bin = prog.rsplit('/').next()?;
    for apps in by_cat.values() {
        for a in apps {
            let app_prog = a.exec.split_whitespace().next().unwrap_or("");
            if app_prog.rsplit('/').next() == Some(bin) && a.icon.is_some() {
                return a.icon.clone();
            }
        }
    }
    Some(bin.to_string())
}

/// Build the full menu tree (scans the system once).
pub fn build() -> MenuTree {
    let by_cat = scan();

    let mut main = Menu::new();
    let mut subs = Vec::new();
    let mut quick_rows = Vec::new();
    for q in QUICK {
        let cmd = std::env::var(q.env).unwrap_or_else(|_| q.default.to_string());
        let icon = quick_icon(&by_cat, &cmd);
        quick_rows.push((q.label.to_string(), cmd, icon));
    }
    for (cat, apps) in by_cat {
        if apps.is_empty() {
            continue;
        }
        let mut sub = Menu::new();
        for a in apps {
            sub.push(a.name, Item::Launch(a.exec), false, a.icon);
        }
        let idx = subs.len();
        subs.push(sub);
        main.push(cat, Item::Submenu(idx), true, None);
    }

    main.push(String::new(), Item::Separator, false, None);
    for (label, cmd, icon) in quick_rows {
        main.push(label, Item::Launch(cmd), false, icon);
    }

    MenuTree { main, subs }
}
