//! Resolved colors and layout metrics, ported from splitwm/theme.lua + rc.lua.
//! Some palette entries are reserved for features still being ported.
#![allow(dead_code)]

// --- metrics (rc.lua overrides applied) ---
pub const GAP: i32 = 40; // beautiful.splitwm_gap
pub const FOCUS_BORDER_WIDTH: i32 = 3;
pub const BORDER_RADIUS: f32 = 2.0;
pub const EMPTY_RADIUS: f32 = 14.0;

pub const TITLEBAR_HEIGHT: i32 = 34;
pub const TAB_EXTRA_H: i32 = 2;

// Split widths below which the focused client gets font-shrink shortcuts.
pub const SMUSH_THRESHOLD: i32 = 900;
pub const TINY_SMUSH_THRESHOLD: i32 = 650;

// Bottom taskbar holding windows not shown in any split.
pub const TASKBAR_H: i32 = 56;
pub const TASKBAR_ICON: i32 = 36;
pub const TASKBAR_GAP: i32 = 10;

pub const SPLIT_RATIO: f64 = 0.618;
pub const RESIZE_STEP: f64 = 0.05;
pub const SCROLL_STEP: i32 = 100;

// Round split-control button geometry.
pub const BTN_SIZE: i32 = 26;
pub const BTN_SPACING: i32 = 5;
pub const N_SPLIT_BTNS: i32 = 5;

pub const fn min_split_w() -> i32 {
    N_SPLIT_BTNS * BTN_SIZE + (N_SPLIT_BTNS - 1) * BTN_SPACING
}

/// Effective tab-bar height: grows to match the gap so the bar never
/// disappears into it.
pub fn tb_h(gap: i32) -> i32 {
    TITLEBAR_HEIGHT.max(gap) + TAB_EXTRA_H
}

// Hue-rotated palette giving each split its own persistent accent colour.
pub const LEAF_PALETTE: [u32; 8] = [
    0xff66_aaff,
    0xffff_6688,
    0xff66_dd99,
    0xffff_cc66,
    0xffcc_88ff,
    0xff66_dddd,
    0xffff_9966,
    0xffaa_dd66,
];

/// A stable accent colour for a leaf, picked from `LEAF_PALETTE` by id.
pub const fn leaf_color(id: u32) -> u32 {
    LEAF_PALETTE[(id as usize) % LEAF_PALETTE.len()]
}

// --- colors (ARGB u32, matching rc.lua) ---
pub const COLOR_BG: u32 = 0xff00_0000; // splitwm_color_bg #000000ff
pub const COLOR_FG: u32 = 0xffff_ffff;
pub const COLOR_ACCENT: u32 = 0xffff_6666;
pub const COLOR_BTN_BG: u32 = 0x8000_0000; // #00000080
pub const COLOR_FG_DISABLED: u32 = 0x55ff_ffff;
pub const COLOR_HANDLE: u32 = 0x55ff_ffff;
pub const COLOR_FG_HOVER: u32 = 0x20ff_ffff;
pub const COLOR_CLOSE: u32 = 0xffff_6666;

/// Wallpaper / root background.
pub const WALLPAPER: u32 = COLOR_BG;
