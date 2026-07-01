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
/// `(name, exec, category)` when it is a displayable Application.
fn parse_desktop(text: &str) -> Option<(String, String, String)> {
    let mut in_entry = false;
    let (mut name, mut exec, mut cats) = (None, None, String::new());
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
    Some((name?, exec, first_main_category(&cats)))
}

/// Standard application directories (XDG data dirs + per-user).
fn app_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let data =
            std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
        dirs.push(std::path::PathBuf::from(data).join("applications"));
    }
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in system.split(':') {
        if !d.is_empty() {
            dirs.push(std::path::PathBuf::from(d).join("applications"));
        }
    }
    dirs
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
                if let Some((name, exec, cat)) = parse_desktop(&text) {
                    by_cat.entry(cat).or_default().push(App { name, exec });
                }
            }
        }
    }
    for apps in by_cat.values_mut() {
        apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    }
    by_cat
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
];

/// Build the full menu tree (scans the system once).
pub fn build() -> MenuTree {
    let by_cat = scan();

    let mut main = Menu {
        labels: Vec::new(),
        items: Vec::new(),
        arrows: Vec::new(),
    };
    let push = |m: &mut Menu, label: String, item: Item, arrow: bool| {
        m.labels.push(label);
        m.items.push(item);
        m.arrows.push(arrow);
    };

    let mut subs = Vec::new();
    for (cat, apps) in by_cat {
        if apps.is_empty() {
            continue;
        }
        let mut sub = Menu {
            labels: Vec::new(),
            items: Vec::new(),
            arrows: Vec::new(),
        };
        for a in apps {
            sub.labels.push(a.name);
            sub.items.push(Item::Launch(a.exec));
            sub.arrows.push(false);
        }
        let idx = subs.len();
        subs.push(sub);
        push(&mut main, cat, Item::Submenu(idx), true);
    }

    push(&mut main, String::new(), Item::Separator, false);
    for q in QUICK {
        let cmd = std::env::var(q.env).unwrap_or_else(|_| q.default.to_string());
        push(&mut main, q.label.to_string(), Item::Launch(cmd), false);
    }

    MenuTree { main, subs }
}
