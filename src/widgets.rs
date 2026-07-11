//! Widget-region computation (hit-regions, titlebars, buttons, taskbar) for
//! the chrome underlay. Every function below reads layout state and writes
//! into a `Widgets`; none touch compositor state, so they're free functions
//! and directly unit-testable. Ported from master's `wm/widgets.rs`; the
//! only Wayland adaptation is that clients are represented by `(Win, class)`
//! pairs instead of the X11 `Client` struct.

use std::rc::Rc;

use crate::layout::{Boundary, Dir, GapAt, Insert, Layout, NodeId, Rect, Win};
use crate::state::State;
use crate::theme;

/// Screen-space rect; the same shape as canvas-space `layout::Rect`,
/// aliased so signatures can still say which space they mean.
pub type FrameRect = Rect;

/// One on-screen leaf's placement for an arrange: its screen-space frame
/// (scroll applied), its shown window, and whether it holds layout focus.
/// Present for every visible leaf — empty and minimized ones draw chrome.
#[derive(Clone, Copy)]
pub struct Placement {
    pub leaf: NodeId,
    pub target: FrameRect,
    pub active_client: Option<Win>,
    pub focused: bool,
}

/// The pure placement pass of an arrange: every leaf's frame rect at the
/// current scroll (on-screen or not — a leaf scrolled out of view keeps a
/// sane animation start / hit rect for its return), and a `Placement` for
/// each on-screen leaf. Every visible leaf gets a placement (chrome draws
/// empty and minimized frames too); which ones actually map a window is the
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
        placed.push(Placement {
            leaf,
            target: frame,
            active_client: state.layout.leaf(leaf).and_then(|l| l.client),
            focused: focused == leaf,
        });
    }
    (placed, frame_rects)
}

/// Side of a "+" insert / drag square, sized to sit inside a gap.
pub const PLUS_SZ: i32 = theme::GAP - 4;
/// How much narrower than the gap a boundary drag handle is drawn/hit.
pub const HANDLE_INSET: i32 = 10;

/// A `PLUS_SZ`-square hit/draw rect centred on (`cx`, `cy`).
pub const fn plus_rect(cx: i32, cy: i32) -> FrameRect {
    FrameRect {
        x: cx - PLUS_SZ / 2,
        y: cy - PLUS_SZ / 2,
        w: PLUS_SZ,
        h: PLUS_SZ,
    }
}

/// Every hit-testable widget rect computed for the current layout: gap drag
/// handles, "+" insert buttons, titlebar titles, split-control buttons,
/// taskbar tiles, the quick-launch icons, and the canvas-edge resize
/// handles. Grouped so the whole set is rebuilt (and cleared) as one unit —
/// the caches must always describe the same arrange.
#[derive(Default)]
pub struct Widgets {
    pub handle_regions: Vec<(FrameRect, Boundary)>,
    pub plus_regions: Vec<(FrameRect, Insert)>,
    /// Quick-launch icons in the bottom taskbar (after the window tiles),
    /// paired with their quick-slot index; entries hidden by their
    /// `ShowWhen` rule get no region.
    pub quick_regions: Vec<(FrameRect, usize)>,
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
        self.plus_regions.clear();
        self.quick_regions.clear();
        self.taskbar_sep = None;
        self.title_regions.clear();
        self.btn_regions.clear();
        self.taskbar_regions.clear();
        self.edge_handle_regions.clear();
    }
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

/// The three split-control buttons on the right of every leaf's titlebar
/// (count mirrored by `theme::N_SPLIT_BTNS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnKind {
    Minimize,
    Split,
    Close,
}

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

/// Per-leaf metadata driving the split-control buttons' icons/enabled state.
#[derive(Clone, Copy)]
pub struct LeafMeta {
    /// Whether the split shares its column with other rows: it minimizes
    /// to a horizontal strip, and its ⊞ always stacks below.
    pub stacked: bool,
    /// The strip's one guaranteed split: it can't be closed (when empty)
    /// or minimized.
    pub sole: bool,
    /// What the split's ⊞ button does. `None` disables the button (a
    /// too-short lone split can neither stack nor claim a preference);
    /// `Dir::H` opens a new column right of this one, `Dir::V` stacks an
    /// empty split below. A stacked split always stacks; a lone one goes
    /// by shape — wide opens a column, tall stacks. Right-click flips
    /// where the flipped action is possible.
    pub split_dir: Option<Dir>,
    pub minimized: bool,
    /// Whether the leaf shows a window. Close always works on an occupied
    /// split (it closes the window; the split follows on its death); only
    /// an empty sole placeholder has nothing to close.
    pub occupied: bool,
}

/// Position / split-eligibility metadata used to choose each
/// split-control button's icon and enabled state.
pub fn leaf_meta(layout: &Layout, leaf: NodeId, frame: FrameRect) -> LeafMeta {
    let stacked = layout.stacked(leaf);
    let can_stack = theme::stack_fits(frame.h);
    let split_dir = if stacked || frame.w < frame.h {
        // A stacked split only ever stacks further; a tall lone one
        // prefers stacking too. Opening a column needs no room, so it is
        // never the *disabled* fallback — only a too-short stack target
        // disables the button.
        can_stack.then_some(Dir::V)
    } else {
        Some(Dir::H)
    };
    let leaf = layout.leaf(leaf);
    LeafMeta {
        stacked,
        sole: layout.sole_split(),
        split_dir,
        minimized: leaf.is_some_and(|l| l.minimized),
        occupied: leaf.is_some_and(|l| l.client.is_some()),
    }
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
        widgets.quick_regions.push((
            FrameRect {
                x: qx,
                y,
                w: isz,
                h: isz,
            },
            i,
        ));
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
        // Placeholders get a title region too: an empty split's titlebar
        // is grabbable for a move drag even though it paints no title.
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
    // At most three buttons, in a fixed array: this runs per leaf per frame
    // (the baked-chrome fingerprint), so it must not allocate.
    let mut btns = [None; 3];
    if frame.w >= theme::min_split_w() {
        let right = theme::btn_strip_right(frame.x, frame.w, theme::BORDER_LEFT);
        for (i, kind) in [BtnKind::Close, BtnKind::Split, BtnKind::Minimize]
            .into_iter()
            .enumerate()
        {
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

/// Gap resize handles and "+" insert buttons. Every gap *and margin*
/// carries a "+" through one shared path (`Layout::insert_slots`): the
/// outer margins and column gaps insert a new column, a column's
/// top/bottom margins and stack gaps insert a row into that stack.
pub fn compute_boundary_widgets(widgets: &mut Widgets, state: &State, wa: Rect) {
    let gap = theme::GAP;
    let hw = (gap - HANDLE_INSET).max(4);
    let scroll_x = state.scroll_x();
    for b in state.boundaries(wa) {
        let rect = match b.at {
            // Vertical gap between columns: a full-height pill dragged
            // along x (scrolls with the canvas).
            GapAt::Col(_) => {
                let vis_x = b.pos - scroll_x;
                if vis_x + hw / 2 <= wa.x || vis_x - hw / 2 >= wa.x + wa.w {
                    continue;
                }
                FrameRect {
                    x: vis_x - hw / 2,
                    y: b.cross,
                    w: hw,
                    h: b.cross_len.max(1),
                }
            }
            // Horizontal gap between stacked rows: a full-width strip
            // dragged along y.
            GapAt::Row { .. } => {
                let vis_x = b.cross - scroll_x;
                if vis_x + b.cross_len <= wa.x || vis_x >= wa.x + wa.w {
                    continue;
                }
                FrameRect {
                    x: vis_x,
                    y: b.pos - hw / 2,
                    w: b.cross_len.max(1),
                    h: hw,
                }
            }
        };
        widgets.handle_regions.push((rect, b));
    }
    for (cx, cy, at) in state.layout.insert_slots(wa, gap) {
        let vis_x = cx - scroll_x;
        if vis_x + PLUS_SZ / 2 <= wa.x || vis_x - PLUS_SZ / 2 >= wa.x + wa.w {
            continue;
        }
        widgets.plus_regions.push((plus_rect(vis_x, cy), at));
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
                    active_client: state.layout.leaf(leaf).and_then(|l| l.client),
                    focused: focused == leaf,
                })
            })
            .collect()
    }

    /// A single leaf still spans the whole row (see `State::edge_span`), so
    /// its left/right margins are still both draggable — edge handles are
    /// not gated on having 2+ root-level columns.
    #[test]
    fn edge_handles_present_even_with_a_single_root_leaf() {
        let s = State::new();
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
        let mut s = State::new();
        s.insert_at(WA, Insert::Col(1));
        s.insert_at(WA, Insert::Col(1));
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

    /// Every gap gets one drag handle, and every gap *and margin* gets a
    /// "+" button — column and row positions alike.
    #[test]
    fn one_handle_per_gap_and_one_plus_per_insert_position() {
        let mut s = three_visible_columns(); // 3 columns -> 2 gaps
        s.split_focused(); // a stack -> 1 more gap
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.handle_regions.len(), 3);
        let row_plus = widgets
            .plus_regions
            .iter()
            .filter(|(_, at)| matches!(at, Insert::Row { .. }))
            .count();
        // Column slots: 2 margins + 2 gaps. Row slots: top + bottom margin
        // per column, plus the stacked column's one inter-row gap.
        assert_eq!((widgets.plus_regions.len(), row_plus), (11, 7));
    }

    #[test]
    fn taskbar_stride_never_overlaps_within_available_width() {
        let layout = Layout::new();
        let leaf = layout.first_leaf();
        let clients: Vec<(Win, String)> = Vec::new();
        // A pathological number of windows: the stride must compress
        // (clamped at a floor of 10px) rather than run tiles off-screen or
        // silently drop any of them.
        let bar_order: Vec<(Win, NodeId)> = (0..200).map(|w| (w, leaf)).collect();
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &layout, &clients, &[], &bar_order, WA);
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

    /// An empty placeholder's titlebar is a drag handle like any other:
    /// it gets a title region even with no client, so a split-move drag
    /// can start on it. Only minimizing removes the region.
    #[test]
    fn placeholder_titlebar_is_grabbable() {
        let s = State::new(); // one empty placeholder leaf
        let leaf = s.layout.first_leaf();
        assert_eq!(s.layout.leaf(leaf).unwrap().client, None);
        let placed = placement(&s, WA);
        let mut widgets = Widgets::default();
        compute_leaf_widgets(&mut widgets, &s.layout, &placed);
        assert!(widgets.title_regions.iter().any(|&(_, l)| l == leaf));
    }

    #[test]
    fn minimized_leaf_gets_one_full_frame_restore_button() {
        let mut s = State::new();
        s.insert_at(WA, Insert::Col(1)); // a sole split can't be minimized
        let minimized_leaf = s.layout.first_leaf();
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
        assert_eq!(btns.len(), 1, "one region, not the usual three");
        assert_eq!(btns[0].2, BtnKind::Minimize);
        assert_eq!(btns[0].0, target, "whole frame is the restore button");
    }
}
