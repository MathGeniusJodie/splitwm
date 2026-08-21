//! Widget-region computation (hit-regions, titlebars, buttons, taskbar) for
//! the chrome underlay. Every function below reads layout state and writes
//! into a `Widgets`; none touch compositor state, so they're free functions
//! and directly unit-testable. Ported from master's `wm/widgets.rs`; the
//! only Wayland adaptation is that clients are represented by `(Win, class)`
//! pairs instead of the X11 `Client` struct.

use std::rc::Rc;

use crate::layout::{Boundary, GapAt, Layout, NodeId, Rect, Side, Win};
use crate::state::State;
use crate::theme;

/// Screen-space rect; the same shape as canvas-space `layout::Rect`,
/// aliased so signatures can still say which space they mean.
pub type FrameRect = Rect;

/// One on-screen leaf's placement for an arrange: its screen-space frame
/// (scroll applied), its window, and whether it holds layout focus.
/// Present for every visible leaf — a minimized one draws chrome too.
#[derive(Clone, Copy)]
pub struct Placement {
    pub leaf: NodeId,
    pub target: FrameRect,
    pub client: Win,
    pub focused: bool,
}

/// The pure placement pass of an arrange: every leaf's frame rect at the
/// current scroll (on-screen or not — a leaf scrolled out of view keeps a
/// sane animation start / hit rect for its return), and a `Placement` for
/// each on-screen leaf. Every visible leaf gets a placement (chrome draws
/// minimized frames too); which ones actually map a window is the
/// compositor's business (`Comp::apply_placements`).
pub fn compute_placements(
    state: &State,
    wa: Rect,
) -> (Vec<Placement>, std::collections::HashMap<NodeId, FrameRect>) {
    let geos = state.compute(wa);
    let scroll_x = state.scroll_x();
    let focused = state.focused_leaf_valid();
    let mut placed = Vec::new();
    let mut frame_rects = std::collections::HashMap::new();
    for leaf in state.layout.collect_leaves() {
        let Some(geo) = geos.get(&leaf).copied() else {
            continue;
        };
        let frame = FrameRect {
            x: geo.x - scroll_x,
            y: geo.y,
            w: geo.w.max(1),
            h: geo.h.max(1),
        };
        frame_rects.insert(leaf, frame);
        if frame.x + frame.w <= wa.x || frame.x >= wa.x + wa.w {
            continue;
        }
        let Some(client) = state.layout.leaf(leaf).map(|l| l.client) else {
            continue;
        };
        placed.push(Placement {
            leaf,
            target: frame,
            client,
            focused: focused == Some(leaf),
        });
    }
    (placed, frame_rects)
}

/// Width of the hover compass's ring around a quick-launch icon: how far
/// each of its four wedges extends past the icon's edge. The ring reaches
/// over the neighbouring icons, so only the hovered icon's compass is ever
/// drawn or hit-tested.
pub const COMPASS_RING: i32 = 12;

/// The hover compass square drawn around the quick-launch icon at `icon`:
/// the icon's rect grown by `COMPASS_RING` on every side.
pub const fn compass_rect(icon: FrameRect) -> FrameRect {
    FrameRect {
        x: icon.x - COMPASS_RING,
        y: icon.y - COMPASS_RING,
        w: icon.w + 2 * COMPASS_RING,
        h: icon.h + 2 * COMPASS_RING,
    }
}

/// Which wedge of the compass the point (`mx`, `my`) falls in: the square
/// is quartered by its diagonals, so the wedge is whichever axis the point
/// sits furthest along from the centre. Every point of the compass names a
/// wedge, the icon in the middle included — a quick-launch icon showing
/// its compass has no plain-launch spot left. `around` may be the icon
/// rect or the compass rect: they share a centre, so both answer alike,
/// and the drawing pass and the hit-test can't disagree.
pub fn compass_zone(around: FrameRect, mx: i32, my: i32) -> Side {
    // Doubled so the centre of an even-sided rect is exact.
    let dx = 2 * mx + 1 - (2 * around.x + around.w);
    let dy = 2 * my + 1 - (2 * around.y + around.h);
    if dx.abs() > dy.abs() {
        if dx < 0 {
            Side::Left
        } else {
            Side::Right
        }
    } else if dy < 0 {
        Side::Up
    } else {
        Side::Down
    }
}

/// Whether `r` covers the point (`mx`, `my`).
pub const fn rect_contains(r: FrameRect, mx: i32, my: i32) -> bool {
    mx >= r.x && mx < r.x + r.w && my >= r.y && my < r.y + r.h
}

/// Every hit-testable widget rect computed for the current layout: gap drag
/// handles, titlebar titles, split-control buttons, taskbar tiles, the
/// quick-launch icons, and the canvas-edge resize handles. Grouped so the
/// whole set is rebuilt (and cleared) as one unit — the caches must always
/// describe the same arrange.
#[derive(Default)]
pub struct Widgets {
    pub handle_regions: Vec<(FrameRect, Boundary)>,
    /// Quick-launch icons in the bottom taskbar (after the window tiles);
    /// entries hidden by their `ShowWhen` rule get no region.
    pub quick_regions: Vec<QuickTile>,
    /// The pill separating window tiles from the quick-launch icons; only
    /// present when both groups are (an unpaired separator is just clutter).
    pub taskbar_sep: Option<FrameRect>,
    pub title_regions: Vec<(FrameRect, NodeId)>,
    pub taskbar_regions: Vec<TaskTile>,
    pub btn_regions: Vec<(FrameRect, NodeId, BtnKind)>,
    /// Hit-regions for the outer canvas-edge resize handles (see
    /// `compute_edge_handle_widgets`); the bool is `true` for the left
    /// edge, `false` for the right.
    pub edge_handle_regions: Vec<(FrameRect, bool)>,
}

impl Widgets {
    /// Drop every region (and stale rect) from the previous layout.
    pub fn clear(&mut self) {
        self.handle_regions.clear();
        self.quick_regions.clear();
        self.taskbar_sep = None;
        self.title_regions.clear();
        self.btn_regions.clear();
        self.taskbar_regions.clear();
        self.edge_handle_regions.clear();
    }
}

/// One quick-launch icon's place in the bar: where the icon draws, the
/// larger rect that raises its hover compass, and which `QuickSlot` it
/// launches. The hover rects of neighbouring icons meet and fill the bar's
/// height, so no part of the quick-launch run is neutral — the pointer
/// always has a compass up somewhere over it.
#[derive(Clone, Copy)]
pub struct QuickTile {
    pub icon: FrameRect,
    pub hover: FrameRect,
    pub slot: usize,
}

/// One taskbar quick-launch entry: the command it spawns and its icon,
/// resolved once at startup.
pub struct QuickSlot {
    /// Spawned when the icon is clicked.
    pub cmd: String,
    /// Decoded, palette-quantized icon; `None` falls back to the label glyph.
    pub icon: Option<Rc<crate::icon::Icon>>,
    /// First letter of the entry's label, the no-icon fallback glyph.
    pub label: char,
    /// Visibility rule, re-evaluated against the managed clients each
    /// arrange (see `compute_taskbar`).
    pub show: theme::ShowWhen,
}

/// The split-control buttons on the right of every leaf's titlebar (count
/// mirrored by `theme::N_SPLIT_BTNS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnKind {
    Minimize,
    Close,
}

/// How many buttons a titlebar's fixed array holds — `BtnKind`'s variants,
/// as `theme::N_SPLIT_BTNS` counts them for the strip's width.
pub const N_BTNS: usize = theme::N_SPLIT_BTNS as usize;

/// A bottom-bar tile with its window and accent resolved once at compute
/// time, so per-frame compositing needs no tree walks. Tiles mirror the
/// splits one-to-one, in the same left-to-right (depth-first) order.
#[derive(Clone, Copy)]
pub struct TaskTile {
    pub rect: FrameRect,
    /// The close ("x") badge in the tile's bottom-right corner; hit-tested
    /// before `rect` so it wins the click.
    pub close: FrameRect,
    pub win: Win,
    /// The split showing this window — every taskbar'd window has one.
    /// Both the accent below and drag-drop targeting resolve through it.
    pub leaf: NodeId,
    pub accent: crate::Index,
}

/// Each split's persistent accent palette index, stored on the leaf so it
/// survives splits and closes; palette-swaps the bitmap window border and
/// colours the bottom-bar highlight.
pub fn leaf_color_index(layout: &Layout, leaf: NodeId) -> crate::Index {
    layout
        .leaf(leaf)
        .map_or(theme::FALLBACK_ACCENT_INDEX, |l| l.color)
}

/// The taskbar/titlebar fallback glyph for a client's class (Wayland:
/// app_id): its first character, uppercased, or `?` when empty.
pub fn label_from_class(class: &str) -> char {
    class.chars().next().map_or('?', |c| c.to_ascii_uppercase())
}

/// Lay out the bottom bar's tiles: one per shown window, in `bar_order` —
/// the splits' own depth-first order, so the bar always reads left-to-right
/// like the canvas — across the full screen width. Each tile's accent
/// colour is resolved here, once per arrange, so the per-frame compositor
/// needs no tree walks. `clients` pairs each managed window with its class
/// string (for the quick-launch `ShowWhen` rules).
pub fn compute_taskbar(
    widgets: &mut Widgets,
    layout: &Layout,
    clients: &[(Win, String)],
    quick: &[QuickSlot],
    bar_order: &[(Win, NodeId)],
    wa: Rect,
) {
    let gap = theme::TASKBAR_GAP;
    let isz = theme::TASKBAR_ICON;
    let cbs = theme::TASKBAR_CLOSE;
    // Centre the tile + close-badge group vertically in the bar; the
    // badge overlaps the tile's bottom edge slightly.
    let overlap = 4;
    let pad = (theme::TASKBAR_H - (isz + cbs - overlap)) / 2;
    let y = wa.y + wa.h - theme::TASKBAR_H + pad;
    // Which quick-launch entries are visible right now: each entry's
    // `ShowWhen` rule is keyed on whether a managed window's class
    // matches it.
    let running = |class: &str| clients.iter().any(|(_, c)| c.eq_ignore_ascii_case(class));
    let visible: Vec<usize> = (0..quick.len())
        .filter(|&i| match quick[i].show {
            theme::ShowWhen::Always => true,
            theme::ShowWhen::UnlessRunning(class) => !running(class),
        })
        .collect();
    // Window tiles fill from the left; the quick-launch icons (in
    // `theme::QUICK` order) follow the last tile, walled off by the
    // separator pill. Left/right edge margins match the split gap.
    // Tiles may claim everything up to where the quick group would be
    // pushed against the bar's right edge: when the bar can't hold
    // every window at full pitch, the stride compresses (tiles overlap
    // like fanned cards, rightmost on top — draw order and the
    // reversed hit-tests agree on that) instead of silently dropping
    // tiles: a window without a tile would be unreachable by mouse
    // entirely.
    let bar_right = wa.x + wa.w - theme::GAP;
    let nq = i32::try_from(visible.len()).unwrap_or(0);
    let quick_w = (nq * (isz + gap) - gap).max(0);
    let sep_w = 4;
    let right = if nq > 0 {
        bar_right - quick_w - gap - sep_w - gap
    } else {
        bar_right
    };
    let left = wa.x + theme::GAP;
    let full_stride = isz + gap;
    let n = i32::try_from(bar_order.len()).unwrap_or(i32::MAX);
    let stride = if n > 1 {
        let avail = right - left - isz;
        (avail / (n - 1)).clamp(10, full_stride)
    } else {
        full_stride
    };
    let mut x = left;
    let mut tiles = Vec::with_capacity(bar_order.len());
    for &(win, leaf) in bar_order {
        // Even at minimum stride a pathological window count can run
        // past the edge; pin the excess at the right rather than lose it.
        let tx = x.min(right - isz);
        tiles.push(TaskTile {
            rect: FrameRect {
                x: tx,
                y,
                w: isz,
                h: isz,
            },
            // Close badge below the tile (overlapping its bottom edge),
            // right-aligned; hit-tested before the tile so clicking it
            // closes instead of focusing.
            close: FrameRect {
                x: tx + isz - cbs,
                y: y + isz - overlap,
                w: cbs,
                h: cbs,
            },
            win,
            leaf,
            accent: leaf_color_index(layout, leaf),
        });
        x += stride;
    }
    // Quick icons trail the last tile (or sit at the bar's left edge
    // when there are no windows, with no pill to separate).
    let tail = tiles.last().map(|t: &TaskTile| t.rect.x + isz);
    widgets.taskbar_sep = tail.filter(|_| nq > 0).map(|t| FrameRect {
        x: t + gap,
        y,
        w: sep_w,
        h: isz,
    });
    let mut qx = match tail {
        Some(t) => t + gap + sep_w + gap,
        None => left,
    };
    for i in visible {
        let icon = FrameRect {
            x: qx,
            y,
            w: isz,
            h: isz,
        };
        widgets.quick_regions.push(QuickTile {
            icon,
            // Half the pitch's gap on each side (so neighbours meet
            // exactly) and the bar's full height.
            hover: FrameRect {
                x: icon.x - gap / 2,
                y: y - pad,
                w: isz + gap,
                h: theme::TASKBAR_H,
            },
            slot: i,
        });
        qx += isz + gap;
    }
    widgets.taskbar_regions = tiles;
}

/// Per-leaf titlebar hit-rects and split-control buttons.
pub fn compute_leaf_widgets(widgets: &mut Widgets, layout: &Layout, placed: &[Placement]) {
    let tb_h = theme::tb_h();
    let bw = theme::BORDER_LEFT;
    for p in placed {
        let minimized = layout.leaf(p.leaf).is_some_and(|l| l.minimized);
        if !minimized {
            widgets.title_regions.push((
                FrameRect {
                    x: p.target.x + bw,
                    y: p.target.y,
                    w: (p.target.w - 2 * bw).max(0),
                    h: tb_h,
                },
                p.leaf,
            ));
        }
        compute_btn_regions(widgets, p, minimized);
    }
}

/// The split-control button rects for an unminimized leaf occupying
/// `frame`, right-aligned in its titlebar (a too-narrow leaf keeps only a
/// centred minimize button). The single source of button geometry: both the
/// hit-regions (`compute_btn_regions`) and the baked chrome
/// (`Comp::leaf_buttons`, which re-derives them at each interpolated size
/// mid-animation) read it, so a click always lands where a button drew.
pub fn leaf_btn_rects(frame: FrameRect) -> impl Iterator<Item = (BtnKind, FrameRect)> {
    let bsz = theme::BTN_SIZE;
    let bsp = theme::BTN_SPACING;
    let bcy = frame.y + theme::tb_h() / 2 + theme::BTN_Y_OFFSET;
    let at = |bcx: i32, kind: BtnKind| {
        (
            kind,
            FrameRect {
                x: bcx - bsz / 2,
                y: bcy - bsz / 2,
                w: bsz,
                h: bsz,
            },
        )
    };
    // At most `N_SPLIT_BTNS` buttons, in a fixed array: this runs per leaf
    // per frame (the baked-chrome fingerprint), so it must not allocate.
    let mut btns = [None; N_BTNS];
    if frame.w >= theme::min_split_w() {
        let right = theme::btn_strip_right(frame.x, frame.w, theme::BORDER_LEFT);
        for (i, kind) in [BtnKind::Close, BtnKind::Minimize].into_iter().enumerate() {
            let bcx = right - bsz / 2 - i32::try_from(i).unwrap_or(0) * (bsz + bsp);
            btns[i] = Some(at(bcx, kind));
        }
    } else {
        btns[0] = Some(at(frame.x + frame.w / 2, BtnKind::Minimize));
    }
    btns.into_iter().flatten()
}

/// Split-control buttons on the right of a leaf's titlebar; a minimized
/// leaf instead gets one full-frame region (the whole bitmap is the
/// restore button, drawn by `draw_leaf`).
fn compute_btn_regions(widgets: &mut Widgets, p: &Placement, minimized: bool) {
    if minimized {
        widgets
            .btn_regions
            .push((p.target, p.leaf, BtnKind::Minimize));
        return;
    }
    for (kind, rect) in leaf_btn_rects(p.target) {
        widgets.btn_regions.push((rect, p.leaf, kind));
    }
}

/// Gap resize handles: one per gap between two columns or two stacked
/// rows, plus the outer canvas-edge handles.
pub fn compute_boundary_widgets(widgets: &mut Widgets, state: &State, wa: Rect) {
    let gap = theme::GAP;
    let scroll_x = state.scroll_x();
    for b in state.boundaries(wa) {
        let rect = match b.at {
            // Vertical gap between columns: the whole gap, full height
            // (it scrolls with the canvas). Its two halves belong to the
            // columns either side — see `Comp::hit_test`.
            GapAt::Col(_) => {
                let vis_x = b.pos - scroll_x;
                if vis_x + gap / 2 <= wa.x || vis_x - gap / 2 >= wa.x + wa.w {
                    continue;
                }
                FrameRect {
                    x: vis_x - gap / 2,
                    y: b.cross,
                    w: gap,
                    h: b.cross_len.max(1),
                }
            }
            // Horizontal gap between stacked rows: a full-width strip
            // dragged along y, spanning the whole gap *and* the bottom
            // border of the split above it. Border and gap read as one
            // divider between two stacked windows, so the whole of it
            // drags — unlike a column gap, whose flanking side borders
            // are their own (column-resizing) drag.
            GapAt::Row { .. } => {
                let vis_x = b.cross - scroll_x;
                if vis_x + b.cross_len <= wa.x || vis_x >= wa.x + wa.w {
                    continue;
                }
                FrameRect {
                    x: vis_x,
                    y: b.pos - gap / 2 - theme::BORDER_BOTTOM,
                    w: b.cross_len.max(1),
                    h: gap + theme::BORDER_BOTTOM,
                }
            }
        };
        widgets.handle_regions.push((rect, b));
    }
    compute_edge_handle_widgets(widgets, state, wa);
}

/// Drag handles at the outer left/right canvas margins, letting the
/// leftmost/rightmost column grow or shrink into its own margin — the
/// edge-of-canvas analogue of the internal boundary handles above. Present
/// even with a single root-level leaf (see `State::edge_span`, whose left
/// and right span then coincide): resizing "the only column" still moves
/// its edge against the wallpaper margin.
fn compute_edge_handle_widgets(widgets: &mut Widgets, state: &State, wa: Rect) {
    let gap = theme::GAP;
    let scroll_x = state.scroll_x();
    let span_h = (wa.h - 2 * gap).max(1);
    for left in [true, false] {
        let Some((start_x, w)) = state.edge_span(wa, left) else {
            continue;
        };
        // The whole gap-wide margin strip *outside* the column is the
        // hit region — not a narrow pill centred on the column's edge:
        // half of such a pill sits under the client window (which
        // swallows clicks), leaving only a few workable pixels next to
        // the split.
        let col_edge = (if left { start_x } else { start_x + w }) - scroll_x;
        let x = if left { col_edge - gap } else { col_edge };
        if x + gap <= wa.x || x >= wa.x + wa.w {
            continue;
        }
        widgets.edge_handle_regions.push((
            FrameRect {
                x,
                y: wa.y + gap,
                w: gap,
                h: span_h,
            },
            left,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 800,
    };

    fn placement(state: &State, wa: Rect) -> Vec<Placement> {
        let leaves = state.layout.collect_leaves();
        let geos = state.compute(wa);
        let focused = state.focused_leaf_valid();
        leaves
            .iter()
            .filter_map(|&leaf| {
                let geo = geos.get(&leaf).copied()?;
                Some(Placement {
                    leaf,
                    target: FrameRect {
                        x: geo.x,
                        y: geo.y,
                        w: geo.w.max(1),
                        h: geo.h.max(1),
                    },
                    client: state.layout.leaf(leaf)?.client,
                    focused: focused == Some(leaf),
                })
            })
            .collect()
    }

    /// A strip holding `n` windows, each in its own column.
    fn windows(n: Win) -> State {
        let mut s = State::new();
        for w in 1..=n {
            s.place_new_window(WA, w, None);
        }
        s
    }

    /// Neighbouring quick-launch icons' hover rects meet exactly and each
    /// spans the bar's full height, so the pointer crossing the run of
    /// icons is never over a spot that raises no compass.
    #[test]
    fn quick_launch_hover_rects_tile_the_bar_without_gaps() {
        let quick: Vec<QuickSlot> = (0..3)
            .map(|_| QuickSlot {
                cmd: "term".into(),
                icon: None,
                label: 'T',
                show: theme::ShowWhen::Always,
            })
            .collect();
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &Layout::new(), &[], &quick, &[], WA);
        let tiles = &widgets.quick_regions;
        assert_eq!(tiles.len(), 3);
        let bar_top = WA.y + WA.h - theme::TASKBAR_H;
        for t in tiles {
            assert_eq!((t.hover.y, t.hover.h), (bar_top, theme::TASKBAR_H));
            assert!(
                t.hover.x <= t.icon.x && t.hover.x + t.hover.w >= t.icon.x + t.icon.w,
                "the hover rect covers its icon"
            );
        }
        for pair in tiles.windows(2) {
            assert_eq!(
                pair[0].hover.x + pair[0].hover.w,
                pair[1].hover.x,
                "no neutral gap between two icons"
            );
        }
    }

    /// The compass's wedges are the square's four quarters cut by its
    /// diagonals: whichever axis the point sits furthest along from the
    /// centre names the wedge, over the icon as much as around it.
    #[test]
    fn compass_zones_quarter_the_square_around_the_icon() {
        let icon = FrameRect {
            x: 100,
            y: 100,
            w: 42,
            h: 42,
        };
        let r = compass_rect(icon);
        assert_eq!(compass_zone(icon, r.x + 1, 121), Side::Left);
        assert_eq!(compass_zone(icon, r.x + r.w - 2, 121), Side::Right);
        assert_eq!(compass_zone(icon, 121, r.y + 1), Side::Up);
        assert_eq!(compass_zone(icon, 121, r.y + r.h - 2), Side::Down);
        // Over the icon too: it is no click target of its own any more.
        assert_eq!(compass_zone(icon, 121, icon.y + 2), Side::Up);
        assert_eq!(compass_zone(icon, icon.x + 2, 121), Side::Left);
        // The compass rect shares the icon's centre, so it answers alike.
        assert_eq!(compass_zone(r, 121, r.y + 1), Side::Up);
        // Just off the icon's corner, the diagonal decides by a pixel.
        assert_eq!(compass_zone(icon, icon.x - 1, icon.y - 2), Side::Up);
        assert_eq!(compass_zone(icon, icon.x - 2, icon.y - 1), Side::Left);
    }

    /// A single leaf still spans the whole row (see `State::edge_span`), so
    /// its left/right margins are still both draggable — edge handles are
    /// not gated on having 2+ root-level columns.
    #[test]
    fn edge_handles_present_even_with_a_single_root_leaf() {
        let s = windows(1);
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.edge_handle_regions.len(), 2, "left and right edge");
        let lefts: Vec<bool> = widgets
            .edge_handle_regions
            .iter()
            .map(|&(_, l)| l)
            .collect();
        assert!(lefts.contains(&true) && lefts.contains(&false));
    }

    /// Three narrow columns that all fit inside the viewport, so no
    /// handle is culled as off-screen.
    fn three_visible_columns() -> State {
        let mut s = windows(3);
        for col in 0..3 {
            s.layout
                .set_col_width(col, crate::layout::ColWidth::Px(300));
        }
        s
    }

    /// Regardless of how many columns exist, there are always exactly two
    /// edge handles (left margin, right margin) — not one per column.
    #[test]
    fn edge_handles_stay_at_exactly_two_with_more_columns() {
        let s = three_visible_columns();
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.edge_handle_regions.len(), 2);
    }

    /// Every gap between two splits gets exactly one drag handle —
    /// between columns and within a stack alike.
    #[test]
    fn one_handle_per_gap() {
        let mut s = three_visible_columns(); // 3 columns -> 2 gaps
        s.aim_next_window(Side::Down); // a stack -> 1 more gap
        s.place_new_window(WA, 4, None);
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.handle_regions.len(), 3);
    }

    /// The gap between two columns is one band edge to edge: it starts
    /// where the left frame ends and ends where the right frame starts,
    /// over the strip's full height — so with the frames' own border
    /// bands nothing between two side-by-side windows is neutral.
    #[test]
    fn column_handle_covers_the_whole_gap() {
        let s = three_visible_columns();
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        let leaves = s.layout.collect_leaves();
        let geos = s.compute(WA);
        let (left, right) = (geos[&leaves[0]], geos[&leaves[1]]);
        let (rect, _) = widgets
            .handle_regions
            .iter()
            .find(|(_, b)| matches!(b.at, GapAt::Col(0)))
            .expect("the first column gap");
        assert_eq!(rect.x, left.x + left.w, "starts at the left frame's edge");
        assert_eq!(rect.x + rect.w, right.x, "ends at the right frame's edge");
        assert_eq!(
            (rect.y, rect.h),
            (WA.y + theme::GAP, WA.h - 2 * theme::GAP),
            "the strip's full height"
        );
    }

    /// The divider between two stacked windows is one drag target end to
    /// end: the bottom border of the split above plus the whole gap, up
    /// to (not into) the lower split's titlebar.
    #[test]
    fn row_handle_covers_the_gap_and_the_border_above_it() {
        let mut s = windows(1);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        let rows = s.layout.collect_leaves();
        let geos = s.compute(WA);
        let (above, below) = (geos[&rows[0]], geos[&rows[1]]);
        let (rect, _) = widgets
            .handle_regions
            .iter()
            .find(|(_, b)| matches!(b.at, GapAt::Row { .. }))
            .expect("the stack's one gap");
        assert_eq!(
            rect.y,
            above.y + above.h - theme::BORDER_BOTTOM,
            "starts at the top of the border above"
        );
        assert_eq!(
            rect.y + rect.h,
            below.y,
            "ends where the lower titlebar starts"
        );
    }

    #[test]
    fn taskbar_stride_never_overlaps_within_available_width() {
        let s = windows(1);
        let layout = &s.layout;
        let leaf = layout.first_leaf().expect("one window");
        let clients: Vec<(Win, String)> = Vec::new();
        // A pathological number of windows: the stride must compress
        // (clamped at a floor of 10px) rather than run tiles off-screen or
        // silently drop any of them.
        let bar_order: Vec<(Win, NodeId)> = (0..200).map(|w| (w, leaf)).collect();
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, layout, &clients, &[], &bar_order, WA);
        assert_eq!(
            widgets.taskbar_regions.len(),
            200,
            "every window gets a tile"
        );
        for t in &widgets.taskbar_regions {
            assert!(t.rect.x >= WA.x, "tile must not start left of the bar");
            assert!(
                t.rect.x + t.rect.w <= WA.x + WA.w,
                "tile must not run off the right edge"
            );
        }
    }

    #[test]
    fn quick_launch_hidden_when_its_class_is_running() {
        let layout = Layout::new();
        let clients: Vec<(Win, String)> = vec![(1 as Win, "Firefox".to_string())];
        let quick = [QuickSlot {
            cmd: "firefox".into(),
            icon: None,
            label: 'F',
            show: theme::ShowWhen::UnlessRunning("firefox"),
        }];
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &layout, &clients, &quick, &[], WA);
        assert!(
            widgets.quick_regions.is_empty(),
            "quick-launch entry must hide once its class is already running"
        );
    }

    #[test]
    fn quick_launch_shown_when_its_class_is_not_running() {
        let layout = Layout::new();
        let clients: Vec<(Win, String)> = Vec::new();
        let quick = [QuickSlot {
            cmd: "firefox".into(),
            icon: None,
            label: 'F',
            show: theme::ShowWhen::UnlessRunning("firefox"),
        }];
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &layout, &clients, &quick, &[], WA);
        assert_eq!(widgets.quick_regions.len(), 1);
    }

    /// A titlebar is a drag handle: every shown split gets a title region
    /// so a split-move drag can start on it. Only minimizing removes it.
    #[test]
    fn titlebars_are_grabbable() {
        let s = windows(1);
        let leaf = s.layout.first_leaf().expect("one window");
        let placed = placement(&s, WA);
        let mut widgets = Widgets::default();
        compute_leaf_widgets(&mut widgets, &s.layout, &placed);
        assert!(widgets.title_regions.iter().any(|&(_, l)| l == leaf));
    }

    #[test]
    fn minimized_leaf_gets_one_full_frame_restore_button() {
        let mut s = windows(2);
        let minimized_leaf = s.layout.first_leaf().expect("two windows");
        assert!(s.toggle_minimize(minimized_leaf));
        let placed = placement(&s, WA);
        let mut widgets = Widgets::default();
        compute_leaf_widgets(&mut widgets, &s.layout, &placed);
        let target = placed
            .iter()
            .find(|p| p.leaf == minimized_leaf)
            .unwrap()
            .target;
        let btns: Vec<_> = widgets
            .btn_regions
            .iter()
            .filter(|(_, l, _)| *l == minimized_leaf)
            .collect();
        assert_eq!(btns.len(), 1, "one region, not the usual pair");
        assert_eq!(btns[0].2, BtnKind::Minimize);
        assert_eq!(btns[0].0, target, "whole frame is the restore button");
    }
}
