//! Resolved colors, layout metrics, and keyboard configuration — everything
//! a user might retune lives here rather than in `wm`.

use crate::Index;

/// Indices into the na16 palette (`na16-1x.png`), used to palette-swap
/// bitmap art rendered through `pixel_graphics`.
pub mod palette_color {
    use crate::Index;

    pub const LAVENDER: Index = 0;
    pub const GUNMETAL: Index = 1;
    pub const PLUM: Index = 2;
    pub const BROWN: Index = 3;
    pub const PEACH: Index = 4;
    pub const CREAM: Index = 5;
    pub const LIME: Index = 6;
    pub const GREEN: Index = 7;
    pub const ORANGE: Index = 8;
    pub const CRIMSON: Index = 9;
    pub const ROSE: Index = 10;
    pub const PURPLE: Index = 11;
    pub const CYAN: Index = 12;
    pub const BLUE: Index = 13;
    pub const PINE: Index = 14;
    pub const BLACK: Index = 15;
}

/// Hand-picked darker/lighter counterpart for each na16 colour: the darker
/// shade colours the window-border/button outline stroke and the lighter one
/// its highlight stroke, relative to the accent used for the fill — e.g. a
/// `ROSE` accent gets a `CRIMSON` outline and a `PEACH` highlight.
/// Deliberately a fixed, hand-editable table rather than something computed
/// from RGB distance, so each pairing can be retuned by eye. Rows are
/// `(accent, darker, lighter)` and are looked up by matching the accent, so
/// row order doesn't matter (a pair of parallel arrays indexed by palette
/// position would desync silently if either were reordered).
const SHADES: [(Index, Index, Index); 16] = [
    (
        palette_color::LAVENDER,
        palette_color::GUNMETAL,
        palette_color::CREAM,
    ),
    (
        palette_color::GUNMETAL,
        palette_color::PLUM,
        palette_color::LAVENDER,
    ),
    (
        palette_color::PLUM,
        palette_color::BLACK,
        palette_color::PURPLE,
    ),
    (
        palette_color::BROWN,
        palette_color::PLUM,
        palette_color::PEACH,
    ),
    (
        palette_color::PEACH,
        palette_color::BROWN,
        palette_color::CREAM,
    ),
    (
        palette_color::CREAM,
        palette_color::BROWN,
        palette_color::CREAM,
    ),
    (
        palette_color::LIME,
        palette_color::GREEN,
        palette_color::CREAM,
    ),
    (
        palette_color::GREEN,
        palette_color::PLUM,
        palette_color::LIME,
    ),
    (
        palette_color::ORANGE,
        palette_color::BROWN,
        palette_color::CREAM,
    ),
    (
        palette_color::CRIMSON,
        palette_color::PLUM,
        palette_color::ROSE,
    ),
    (
        palette_color::ROSE,
        palette_color::CRIMSON,
        palette_color::PEACH,
    ),
    (
        palette_color::PURPLE,
        palette_color::PLUM,
        palette_color::LAVENDER,
    ),
    (
        palette_color::CYAN,
        palette_color::BLUE,
        palette_color::CREAM,
    ),
    (
        palette_color::BLUE,
        palette_color::PINE,
        palette_color::CYAN,
    ),
    (
        palette_color::PINE,
        palette_color::BLACK,
        palette_color::GREEN,
    ),
    (
        palette_color::BLACK,
        palette_color::BLACK,
        palette_color::PLUM,
    ),
];

/// The `SHADES` row for `index`, found by matching the accent column. An
/// index with no row (only possible if `SHADES` loses an entry) degrades to
/// a black outline / cream highlight rather than panicking.
const fn shade(index: Index) -> (Index, Index, Index) {
    let mut i = 0;
    while i < SHADES.len() {
        if SHADES[i].0 == index {
            return SHADES[i];
        }
        i += 1;
    }
    (index, palette_color::BLACK, palette_color::CREAM)
}

pub const fn darker_index(index: Index) -> Index {
    shade(index).1
}

pub const fn lighter_index(index: Index) -> Index {
    shade(index).2
}

// --- metrics ---

/// Margin between splits (and around the canvas); also the width of the
/// gap drag handles.
pub const GAP: i32 = 20;

// Bitmap window-border 9-slice insets, at winborder.png's native resolution
// (drawn 1:1, one image pixel per screen pixel).
pub const BORDER_LEFT: i32 = 6;
pub const BORDER_TOP: i32 = 27;
pub const BORDER_RIGHT: i32 = 6;
pub const BORDER_BOTTOM: i32 = 7;

// Bottom taskbar holding windows not shown in any split.

pub const TASKBAR_ICON: i32 = 42;
pub const TASKBAR_H: i32 = TASKBAR_ICON + GAP * 2;
pub const TASKBAR_GAP: i32 = 10;
/// Side of the square close ("x") badge in a taskbar tile's bottom-right corner.
pub const TASKBAR_CLOSE: i32 = 17;

/// Default `WM_NAME` of the window parked entirely off-screen past the right
/// edge (see `Wm::manage_dock`), outside the split tree, immune to canvas
/// scrolling, and reserving no layout space. cozyui sets this exact title
/// and never sets `WM_CLASS`, so title is the only identity it exposes.
/// Overridable at runtime with the `SPLITWM_DOCK_TITLE` environment variable.
pub const DOCK_TITLE: &str = "cozyui";

/// How far the docked sidebar is tucked under the right end of the split
/// canvas, in px: `Wm::place_dock` shifts the dock left by this much from
/// the canvas edge, and the canvas (stacked above it) overlaps it by the
/// same amount. The first `GAP` px only close the canvas's trailing margin;
/// beyond that the last column's windows themselves cover the dock's edge.
/// 0 restores the flush side-by-side layout.
pub const DOCK_OVERLAP: i32 = 310;

pub const SPLIT_RATIO: f64 = 0.618;
pub const RESIZE_STEP: f64 = 0.05;
/// Smallest fraction of a branch a child can be resized down to, shared by
/// keyboard resizing and boundary drags so both stop at the same point.
pub const MIN_SPLIT_FRAC: f64 = 0.05;
pub const SCROLL_STEP: i32 = 100;

// Split-control button geometry: native pixel size of the close/minimize/
// hsplit/vsplit PNGs, drawn at 1:1 scale (no stretching).
pub const BTN_SIZE: i32 = 19;
pub const BTN_SPACING: i32 = 4;
/// How many split-control buttons a titlebar holds (close/split/minimize);
/// must match `BtnKind`'s variants — `min_split_w` derives from it.
pub const N_SPLIT_BTNS: i32 = 3;
/// Vertical nudge (down = positive) applied to titlebar buttons, to fine-tune
/// their alignment within the bitmap titlebar. Folds in a 2px correction
/// against the naive titlebar-midpoint calculation (`tb_h / 2`), which sits
/// a couple of pixels low of the button row's true visual centre.
pub const BTN_Y_OFFSET: i32 = 1;

pub const fn min_split_w() -> i32 {
    N_SPLIT_BTNS * BTN_SIZE + (N_SPLIT_BTNS - 1) * BTN_SPACING
}

/// Right edge of the split-control button strip in a titlebar spanning
/// `[x, x+w)` with border width `bw` — shared by `compute_btn_regions`
/// (button placement) and `draw_title` (title-text clipping) so the two
/// can't drift out of sync.
pub const fn btn_strip_right(x: i32, w: i32, bw: i32) -> i32 {
    x + w - bw - 4
}

/// Left edge of the full 3-button strip, i.e. where title text must stop.
/// Only meaningful when `w >= min_split_w()`; below that threshold the strip
/// collapses to a single centred button with no dedicated free strip (see
/// `compute_btn_regions`).
pub const fn btn_strip_left(x: i32, w: i32, bw: i32) -> i32 {
    btn_strip_right(x, w, bw) - min_split_w()
}

/// Titlebar height: the top inset of the bitmap window border.
pub const fn tb_h() -> i32 {
    BORDER_TOP
}

/// Whether a `w`x`h` frame is big enough to split into a `dir` branch: it
/// must fit two children of the direction's minimum size plus the gap
/// between them. The single threshold behind both the titlebar Split
/// button's enabled state (`Wm::leaf_meta`) and the keyboard split gate
/// (`Wm::can_split_focused`), so the two can't drift apart.
pub fn split_fits(dir: crate::tree::Dir, w: i32, h: i32) -> bool {
    match dir {
        crate::tree::Dir::H => w >= 2 * min_split_w() + GAP,
        crate::tree::Dir::V => h >= 2 * tb_h() + GAP,
    }
}

// Palette indices cycled through to give each split its own persistent
// accent, used both to tint the bitmap window border (palette-swapping
// LAVENDER) and to colour the taskbar highlight. Excludes indices reserved
// for the border art itself (LAVENDER/PLUM/CREAM/PURPLE) and the near-black
// GUNMETAL/BLACK, which read as "no accent" rather than a colour choice.
pub const LEAF_PALETTE: [Index; 8] = [
    palette_color::BLUE,
    palette_color::ROSE,
    palette_color::GREEN,
    palette_color::ORANGE,
    palette_color::CYAN,
    palette_color::CRIMSON,
    palette_color::LIME,
    palette_color::PEACH,
];

/// Fallback accent for a leaf id that no longer resolves in the tree.
pub const FALLBACK_ACCENT_INDEX: Index = palette_color::CRIMSON;

/// A stable accent palette index for a leaf, picked from `LEAF_PALETTE` by id.
pub const fn cycled_leaf_color(id: u32) -> Index {
    LEAF_PALETTE[(id as usize) % LEAF_PALETTE.len()]
}

// --- icon color-rotation (same-app window disambiguation) ---
//
// Separate from `LEAF_PALETTE`/split accents above: this hue-rotates a
// window's own app-icon bitmap (in OKLCH space, via `oklch::rotate_hue_argb`)
// rather than palette-swapping na16-indexed chrome, so it isn't limited to
// the fixed 16-colour set. `ICON_HUE_STEPS` evenly spaced slots around the
// hue circle — kept small (60° apart) so each rotation is unmistakable at a
// glance rather than a subtle shift.
pub const ICON_HUE_STEPS: usize = 6;

/// The `slot`th persistent icon hue-rotation (degrees), assigned once per
/// window (see `Wm::assign_icon_slot`) and kept for the window's lifetime.
pub const fn icon_hue_rotation(slot: usize) -> f32 {
    (slot % ICON_HUE_STEPS) as f32 * (360.0 / ICON_HUE_STEPS as f32)
}

// --- keyboard configuration ---
//
// Everything a user might retune lives here rather than in `wm`: the keysym
// constants, the action set, and the binding table `Wm::grab_keys` installs.

/// Keysyms used by `BINDINGS` (raw `u32` values from the `xkeysym` crate's
/// generated tables — X11 keysym values are xkb keysym values, so the same
/// constants serve the Wayland keyboard).
pub use xkeysym::key as ks;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    SplitH,
    SplitV,
    Close,
    FocusNext,
    FocusPrev,
    StashNext,
    StashPrev,
    MoveWindowNext,
    MoveWindowPrev,
    Grow,
    Shrink,
    SpawnTerminal,
    /// Launch the app launcher (see `LAUNCHER_CMD`).
    SpawnLauncher,
    /// Ask the focused window to close via `WM_DELETE_WINDOW`, falling back
    /// to disconnecting its client if it doesn't speak the protocol.
    CloseWindow,
    VolumeUp,
    VolumeDown,
    VolumeMuteToggle,
}

/// splitwm's own modifier bitmask (matched against the xkb modifier state
/// by the keyboard dispatcher); the values are private to this table.
pub const MOD4: u16 = 0x40; // Super/Logo
pub const MOD1: u16 = 0x08; // Alt
pub const SHIFT: u16 = 0x01;

/// Command `Action::SpawnLauncher` spawns: rofi in combi mode (drun + run +
/// window). WAYLAND_DISPLAY is scrubbed so a wayland-capable rofi doesn't
/// pick its wayland backend (which needs layer-shell, out of scope in v1)
/// and instead runs under XWayland as the override-redirect float the
/// launcher design counts on.
pub const LAUNCHER_CMD: &str = "env -u WAYLAND_DISPLAY rofi -show combi";
/// Command `Action::VolumeUp` spawns to raise the default sink's volume.
pub const VOLUME_UP_CMD: &str = "wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 5%+";
/// Command `Action::VolumeDown` spawns to lower the default sink's volume.
pub const VOLUME_DOWN_CMD: &str = "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-";
/// Command `Action::VolumeMuteToggle` spawns to toggle the default sink's mute.
pub const VOLUME_MUTE_CMD: &str = "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle";

/// The key bindings the keyboard dispatcher intercepts before clients see
/// anything: (modifier mask, keysym, action). Keys are named for the divider
/// the user sees, actions for the branch direction: Mod4+V draws a Vertical
/// divider, i.e. an H-branch (side-by-side children), and vice versa. There
/// is deliberately no quit binding: the compositor ends its session only by
/// being killed (SIGTERM), never by a stray chord.
pub const BINDINGS: &[(u16, u32, Action)] = &[
    (MOD4, ks::Return, Action::SpawnTerminal),
    (MOD4, ks::space, Action::SpawnLauncher),
    (MOD4, ks::v, Action::SplitH),
    (MOD4, ks::h, Action::SplitV),
    (MOD4, ks::q, Action::Close),
    (MOD4, ks::Tab, Action::FocusNext),
    (MOD4 | SHIFT, ks::Tab, Action::FocusPrev),
    (MOD4, ks::Right, Action::FocusNext),
    (MOD4, ks::Left, Action::FocusPrev),
    (MOD4, ks::bracketright, Action::StashNext),
    (MOD4, ks::bracketleft, Action::StashPrev),
    (MOD4 | SHIFT, ks::bracketright, Action::MoveWindowNext),
    (MOD4 | SHIFT, ks::bracketleft, Action::MoveWindowPrev),
    (MOD4, ks::l, Action::Grow),
    (MOD4 | SHIFT, ks::l, Action::Shrink),
    (MOD4, ks::equal, Action::Grow),
    (MOD4, ks::minus, Action::Shrink),
    (MOD4 | SHIFT, ks::c, Action::CloseWindow),
    (MOD1, ks::F4, Action::CloseWindow),
    // Media keys carry no modifier: the keysym itself is the whole chord.
    (0, ks::XF86_AudioRaiseVolume, Action::VolumeUp),
    (0, ks::XF86_AudioLowerVolume, Action::VolumeDown),
    (0, ks::XF86_AudioMute, Action::VolumeMuteToggle),
];

// --- taskbar quick-launch entries ---

/// When a quick-launch icon is present in the taskbar, keyed on whether a
/// managed window's `WM_CLASS` matches (case-insensitively).
#[derive(Clone, Copy)]
pub enum ShowWhen {
    Always,
    /// Hidden while a matching window is open — for single-instance apps
    /// whose window tile already covers reaching them.
    UnlessRunning(&'static str),
}

/// A quick-launch shortcut shown as an icon in the taskbar: `env` overrides
/// `default` when set. `icon` is a freedesktop icon-theme name — the generic
/// role icon (terminal, browser, …), deliberately not the icon of whichever
/// app the command resolves to.
pub struct Quick {
    pub label: &'static str,
    pub env: &'static str,
    pub default: &'static str,
    pub icon: &'static str,
    pub show: ShowWhen,
}

pub const QUICK: &[Quick] = &[
    Quick {
        label: "Terminal",
        env: "TERMINAL",
        default: "alacritty",
        icon: "utilities-terminal",
        show: ShowWhen::Always,
    },
    Quick {
        label: "Browser",
        env: "BROWSER",
        default: "xdg-open https://",
        icon: "web-browser",
        show: ShowWhen::Always,
    },
    Quick {
        label: "Files",
        env: "FILEMANAGER",
        default: "xdg-open .",
        icon: "system-file-manager",
        show: ShowWhen::Always,
    },
    Quick {
        label: "Obsidian",
        env: "OBSIDIAN",
        default: "obsidian",
        icon: "obsidian",
        show: ShowWhen::UnlessRunning("obsidian"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every na16 palette index must have exactly one `SHADES` row, so no
    /// accent silently falls through to `shade`'s fallback.
    #[test]
    fn shades_cover_every_palette_index_once() {
        for i in 0..16u8 {
            assert_eq!(
                SHADES.iter().filter(|s| s.0 == i).count(),
                1,
                "palette index {i} must have exactly one SHADES row"
            );
        }
    }
}
