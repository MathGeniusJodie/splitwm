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

pub const SPLIT_RATIO: f64 = 0.618;
pub const RESIZE_STEP: f64 = 0.05;
pub const SCROLL_STEP: i32 = 100;

// Round split-control button geometry.
pub const BTN_SIZE: i32 = 26;
pub const BTN_SPACING: i32 = 5;
pub const N_SPLIT_BTNS: i32 = 5;

pub fn min_split_w() -> i32 {
    N_SPLIT_BTNS * BTN_SIZE + (N_SPLIT_BTNS - 1) * BTN_SPACING
}

/// Effective tab-bar height: grows to match the gap so the bar never
/// disappears into it.
pub fn tb_h(gap: i32) -> i32 {
    TITLEBAR_HEIGHT.max(gap) + TAB_EXTRA_H
}

// --- colors (ARGB u32, matching rc.lua) ---
pub const COLOR_BG: u32 = 0xff000000; // splitwm_color_bg #000000ff
pub const COLOR_FG: u32 = 0xffffffff;
pub const COLOR_ACCENT: u32 = 0xffff6666;
pub const COLOR_BTN_BG: u32 = 0x80000000; // #00000080
pub const COLOR_FG_DISABLED: u32 = 0x55ffffff;
pub const COLOR_HANDLE: u32 = 0x55ffffff;
pub const COLOR_FG_HOVER: u32 = 0x20ffffff;
pub const COLOR_CLOSE: u32 = 0xffff6666;

/// Wallpaper / root background.
pub const WALLPAPER: u32 = COLOR_BG;
