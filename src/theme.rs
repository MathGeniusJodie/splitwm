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
pub const TASKBAR_H: i32 = 56;
pub const TASKBAR_ICON: i32 = 36;
pub const TASKBAR_GAP: i32 = 10;
/// Side of the square close ("x") badge in a taskbar tile's bottom-right corner.
pub const TASKBAR_CLOSE: i32 = 13;

/// Default `WM_NAME` of the window parked entirely off-screen past the right
/// edge (see `Wm::manage_dock`), outside the split tree, immune to canvas
/// scrolling, and reserving no layout space. cozyui sets this exact title
/// and never sets `WM_CLASS`, so title is the only identity it exposes.
/// Overridable at runtime with the `SPLITWM_DOCK_TITLE` environment variable.
pub const DOCK_TITLE: &str = "cozyui";

pub const SPLIT_RATIO: f64 = 0.618;
pub const RESIZE_STEP: f64 = 0.05;
pub const SCROLL_STEP: i32 = 100;

// Split-control button geometry: native pixel size of the close/minimize/
// hsplit/vsplit PNGs, drawn at 1:1 scale (no stretching).
pub const BTN_SIZE: i32 = 19;
pub const BTN_SPACING: i32 = 4;
/// How many split-control buttons a titlebar holds (close/split/minimize);
/// must match `BtnKind`'s variants — `min_split_w` derives from it.
pub const N_SPLIT_BTNS: i32 = 3;
/// Vertical nudge (down = positive) applied to titlebar buttons, to fine-tune
/// their alignment within the bitmap titlebar.
pub const BTN_Y_OFFSET: i32 = 3;

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
