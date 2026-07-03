//! Resolved colors and layout metrics, ported from splitwm/theme.lua + rc.lua.

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

/// Hand-picked darker counterpart for each na16 index, used to colour the
/// window-border/button outline stroke (`palette_color::PURPLE`) a shade
/// darker than the accent used for the fill (`palette_color::LAVENDER`) —
/// e.g. a `ROSE` accent gets a `CRIMSON` outline. Deliberately a fixed,
/// editable table rather than something computed from RGB distance, so the
/// pairing can be retuned by eye.
pub const DARKER_INDEX: [Index; 16] = [
    palette_color::GUNMETAL, // LAVENDER
    palette_color::PLUM,     // GUNMETAL
    palette_color::BLACK,    // PLUM
    palette_color::PLUM,     // BROWN
    palette_color::BROWN,    // PEACH
    palette_color::BROWN,    // CREAM
    palette_color::GREEN,    // LIME
    palette_color::PLUM,     // GREEN
    palette_color::BROWN,    // ORANGE
    palette_color::PLUM,     // CRIMSON
    palette_color::CRIMSON,  // ROSE
    palette_color::PLUM,     // PURPLE
    palette_color::BLUE,     // CYAN
    palette_color::PINE,     // BLUE
    palette_color::BLACK,    // PINE
    palette_color::BLACK,    // BLACK
];

pub const fn darker_index(index: Index) -> Index {
    DARKER_INDEX[(index as usize) % DARKER_INDEX.len()]
}

/// Hand-picked lighter counterpart for each na16 index, used to colour the
/// window-border/button highlight stroke (`palette_color::CREAM`) a shade
/// lighter than the accent used for the fill (`palette_color::LAVENDER`) —
/// e.g. a `ROSE` accent gets a `PEACH` highlight, `BLUE` gets `CYAN`. Same
/// deliberately hand-editable table as `DARKER_INDEX`, not computed.
pub const LIGHTER_INDEX: [Index; 16] = [
    palette_color::CREAM,    // LAVENDER
    palette_color::LAVENDER, // GUNMETAL
    palette_color::PURPLE,   // PLUM
    palette_color::PEACH,    // BROWN
    palette_color::CREAM,    // PEACH
    palette_color::CREAM,    // CREAM
    palette_color::CREAM,    // LIME
    palette_color::LIME,     // GREEN
    palette_color::CREAM,    // ORANGE
    palette_color::ROSE,     // CRIMSON
    palette_color::PEACH,    // ROSE
    palette_color::LAVENDER, // PURPLE
    palette_color::CREAM,    // CYAN
    palette_color::CYAN,     // BLUE
    palette_color::GREEN,    // PINE
    palette_color::PLUM,     // BLACK
];

pub const fn lighter_index(index: Index) -> Index {
    LIGHTER_INDEX[(index as usize) % LIGHTER_INDEX.len()]
}

// --- metrics (rc.lua overrides applied) ---
pub const GAP: i32 = 20; // beautiful.splitwm_gap

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

/// Titlebar height: the top inset of the bitmap window border.
pub const fn tb_h() -> i32 {
    BORDER_TOP
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
pub const fn leaf_color_index(id: u32) -> Index {
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

/// X11 keysyms used by `BINDINGS`.
pub mod ks {
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
    pub const E: u32 = 0x65;
    pub const SPACE: u32 = 0x20;
    pub const XF86_MON_BRIGHTNESS_UP: u32 = 0x1008_ff02;
    pub const XF86_MON_BRIGHTNESS_DOWN: u32 = 0x1008_ff03;
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
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
    /// Launch rofi in desktop-application (drun) mode.
    SpawnLauncher,
    Quit,
    /// Ask the focused window to close via `WM_DELETE_WINDOW`, falling back
    /// to disconnecting its client if it doesn't speak the protocol.
    CloseWindow,
    BrightnessUp,
    BrightnessDown,
}

/// Raw X modifier bits, so this module needs no x11rb dependency.
pub const MOD4: u16 = 0x40; // ModMask::M4
pub const SHIFT: u16 = 0x01; // ModMask::SHIFT

/// The key bindings `Wm::grab_keys` installs: (modifier mask, keysym, action).
/// Keys are named for the divider the user sees, actions for the branch
/// direction: Mod4+V draws a Vertical divider, i.e. an H-branch (side-by-side
/// children), and vice versa. Quit deliberately does *not* share a base key
/// with Close (it used to be Mod4+Shift+Q next to Mod4+Q): one sticky Shift
/// must not turn "close a split" into "end the session".
pub const BINDINGS: &[(u16, u32, Action)] = &[
    (MOD4, ks::RETURN, Action::SpawnTerminal),
    (MOD4, ks::SPACE, Action::SpawnLauncher),
    (MOD4, ks::V, Action::SplitH),
    (MOD4, ks::H, Action::SplitV),
    (MOD4, ks::Q, Action::Close),
    (MOD4, ks::TAB, Action::FocusNext),
    (MOD4 | SHIFT, ks::TAB, Action::FocusPrev),
    (MOD4, ks::RIGHT, Action::FocusNext),
    (MOD4, ks::LEFT, Action::FocusPrev),
    (MOD4, ks::BRACKETRIGHT, Action::NextTab),
    (MOD4, ks::BRACKETLEFT, Action::PrevTab),
    (MOD4 | SHIFT, ks::BRACKETRIGHT, Action::MoveTabNext),
    (MOD4 | SHIFT, ks::BRACKETLEFT, Action::MoveTabPrev),
    (MOD4, ks::L, Action::Grow),
    (MOD4 | SHIFT, ks::L, Action::Shrink),
    (MOD4, ks::EQUAL, Action::Grow),
    (MOD4, ks::MINUS, Action::Shrink),
    (MOD4 | SHIFT, ks::E, Action::Quit),
    (MOD4 | SHIFT, ks::C, Action::CloseWindow),
    (0, ks::XF86_MON_BRIGHTNESS_UP, Action::BrightnessUp),
    (0, ks::XF86_MON_BRIGHTNESS_DOWN, Action::BrightnessDown),
];

// --- launcher quick entries ---

/// A quick-launch shortcut shown at the top of the launcher menu's main
/// column: `env` overrides `default` when set.
pub struct Quick {
    pub label: &'static str,
    pub env: &'static str,
    pub default: &'static str,
}

pub const QUICK: &[Quick] = &[
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
