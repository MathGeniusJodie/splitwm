//! Widget-region computation (hit-regions, titlebars, buttons, taskbar) for
//! the chrome underlay. Every function below reads layout state and writes
//! into a `Widgets`; none touch compositor state, so they're free functions
//! and directly unit-testable. Ported from master's `wm/widgets.rs`; the
//! only Wayland adaptation is that clients are represented by `(Win, class)`
//! pairs instead of the X11 `Client` struct.

use std::collections::HashMap;
use std::rc::Rc;

use crate::state::{InsertAt, State};
use crate::theme;
use crate::tree::{Boundary, Dir, NodeId, Rect, Tree, Win};

/// Screen-space rect; the same shape as canvas-space `tree::Rect`, aliased
/// so signatures can still say which space they mean.
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

/// Side of a "+" insert / drag square, sized to sit inside a gap.
pub const PLUS_SZ: i32 = theme::GAP - 4;
/// How much narrower than the gap a boundary drag handle is drawn/hit.
pub const HANDLE_INSET: i32 = 10;

/// A `PLUS_SZ`-square hit/draw rect centred horizontally on `vis_x`.
pub const fn plus_rect(vis_x: i32, y: i32) -> FrameRect {
    FrameRect {
        x: vis_x - PLUS_SZ / 2,
        y,
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
    pub plus_regions: Vec<(FrameRect, InsertAt)>,
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
    /// Spawned when the icon is clicked (M5 wires the click).
    #[allow(dead_code)]
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

/// A bottom-bar tile with its window and accent/visibility resolved once at
/// compute time, so per-frame compositing needs no tree walks.
#[derive(Clone, Copy)]
pub struct TaskTile {
    pub rect: FrameRect,
    /// The close ("x") badge in the tile's bottom-right corner; hit-tested
    /// before `rect` so it wins the click.
    pub close: FrameRect,
    pub win: Win,
    pub accent: crate::Index,
    /// Whether the window occupies a split (drives the accent highlight).
    /// Deliberately not "on screen": a split scrolled out of the viewport
    /// still counts.
    pub in_split: bool,
}

/// Per-leaf metadata driving the split-control buttons' icons/enabled state.
#[derive(Clone, Copy)]
pub struct LeafMeta {
    pub parent_dir: Option<Dir>,
    pub wider: bool,
    pub can_split: bool,
    pub minimized: bool,
}

/// Parent direction / split-eligibility metadata used to choose each
/// split-control button's icon and enabled state.
pub fn leaf_meta(
    tree: &Tree,
    parent: Option<(NodeId, usize)>,
    leaf: NodeId,
    frame: FrameRect,
) -> LeafMeta {
    let parent_dir = parent.and_then(|(p, _)| tree.branch(p).map(|b| b.dir));
    let wider = frame.w >= frame.h;
    let split_dir = if wider { Dir::H } else { Dir::V };
    LeafMeta {
        parent_dir,
        wider,
        can_split: theme::split_fits(split_dir, frame.w, frame.h),
        minimized: tree.leaf(leaf).is_some_and(|l| l.minimized),
    }
}

/// Each split's persistent accent palette index, stored on the leaf so it
/// survives splits and closes; palette-swaps the bitmap window border and
/// colours the bottom-bar highlight.
pub fn leaf_color_index(tree: &Tree, leaf: NodeId) -> crate::Index {
    tree.leaf(leaf)
        .map_or(theme::FALLBACK_ACCENT_INDEX, |l| l.color)
}

/// The taskbar/titlebar fallback glyph for a client's class (Wayland:
/// app_id): its first character, uppercased, or `?` when empty.
pub fn label_from_class(class: &str) -> char {
    class.chars().next().map_or('?', |c| c.to_ascii_uppercase())
}

/// Lay out the bottom bar's tiles: one per managed window (in stable
/// `bar_order`) across the full screen width. Each tile's accent colour and
/// in-split flag are resolved here, once per arrange, so the per-frame
/// compositor needs no tree walks. `clients` pairs each managed window with
/// its class string (for the quick-launch `ShowWhen` rules).
pub fn compute_taskbar(
    widgets: &mut Widgets,
    tree: &Tree,
    clients: &[(Win, &str)],
    quick: &[QuickSlot],
    bar_order: &[Win],
    wa: Rect,
    leaves: &[NodeId],
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
    let running = |class: &str| {
        clients
            .iter()
            .any(|(_, c)| c.eq_ignore_ascii_case(class))
    };
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
    // One pass over the caller's already-collected leaves for every tile's
    // leaf lookup, rather than a second `collect_leaves` tree walk here —
    // `find_leaf_for_client` per tile would be O(tiles × tree) on a
    // per-arrange path.
    let mut client_leaf = HashMap::new();
    for &l in leaves {
        if let Some(c) = tree.leaf(l).and_then(|lf| lf.client) {
            client_leaf.insert(c, l);
        }
    }
    let mut x = left;
    let mut tiles = Vec::with_capacity(bar_order.len());
    for &win in bar_order {
        // Even at minimum stride a pathological window count can run
        // past the edge; pin the excess at the right rather than lose it.
        let tx = x.min(right - isz);
        let leaf = client_leaf.get(&win).copied();
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
            accent: leaf.map_or(theme::palette_color::CREAM, |l| leaf_color_index(tree, l)),
            in_split: leaf.is_some(),
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
pub fn compute_leaf_widgets(widgets: &mut Widgets, tree: &Tree, placed: &[Placement]) {
    let tb_h = theme::tb_h();
    let bw = theme::BORDER_LEFT;
    for p in placed {
        let leaf = tree.leaf(p.leaf);
        let has_client = leaf.is_some_and(|l| l.client.is_some());
        let minimized = leaf.is_some_and(|l| l.minimized);
        if has_client && !minimized {
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
        compute_btn_regions(widgets, p, tb_h, bw, minimized);
    }
}

/// Split-control buttons on the right of a leaf's titlebar; a minimized
/// leaf instead gets one full-frame region (the whole bitmap is the
/// restore button, drawn by `draw_leaf`).
fn compute_btn_regions(widgets: &mut Widgets, p: &Placement, tb_h: i32, bw: i32, minimized: bool) {
    if minimized {
        widgets
            .btn_regions
            .push((p.target, p.leaf, BtnKind::Minimize));
        return;
    }
    let bsz = theme::BTN_SIZE;
    let bsp = theme::BTN_SPACING;
    let bcy = p.target.y + tb_h / 2 + theme::BTN_Y_OFFSET;
    if p.target.w >= theme::min_split_w() {
        let right = theme::btn_strip_right(p.target.x, p.target.w, bw);
        for (i, kind) in [BtnKind::Close, BtnKind::Split, BtnKind::Minimize]
            .into_iter()
            .enumerate()
        {
            let bcx = right - bsz / 2 - i32::try_from(i).unwrap_or(0) * (bsz + bsp);
            widgets.btn_regions.push((
                FrameRect {
                    x: bcx - bsz / 2,
                    y: bcy - bsz / 2,
                    w: bsz,
                    h: bsz,
                },
                p.leaf,
                kind,
            ));
        }
    } else {
        let bcx = p.target.x + p.target.w / 2;
        widgets.btn_regions.push((
            FrameRect {
                x: bcx - bsz / 2,
                y: bcy - bsz / 2,
                w: bsz,
                h: bsz,
            },
            p.leaf,
            BtnKind::Minimize,
        ));
    }
}

/// Gap resize handles, boundary "+" buttons, and edge insert buttons.
pub fn compute_boundary_widgets(widgets: &mut Widgets, state: &State, wa: Rect) {
    let gap = theme::GAP;
    let hw = (gap - HANDLE_INSET).max(4);
    let scroll_x = state.scroll_x();
    let canvas_w = state.canvas_w(wa);
    for b in state.boundaries(wa) {
        let rect = if b.dir == Dir::H {
            // Vertical gap between columns: a full-height pill dragged
            // along x (scrolls with the canvas).
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
        } else {
            // Horizontal gap between stacked rows: a full-width strip
            // dragged along y.
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
        };
        widgets.handle_regions.push((rect, b));
        if b.root && b.dir == Dir::H {
            let py = b.cross + (b.cross_len - PLUS_SZ) / 2;
            widgets
                .plus_regions
                .push((plus_rect(b.pos - scroll_x, py), InsertAt::Index(b.idx + 1)));
        }
    }
    compute_edge_plus_buttons(widgets, wa, scroll_x, canvas_w, gap);
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

/// Edge "+" buttons at the far left / far right of the canvas.
fn compute_edge_plus_buttons(
    widgets: &mut Widgets,
    wa: Rect,
    scroll_x: i32,
    canvas_w: i32,
    gap: i32,
) {
    let span_h = (wa.h - 2 * gap).max(PLUS_SZ);
    let edge_cy = wa.y + gap + (span_h - PLUS_SZ) / 2;
    for (canvas_x, at) in [
        (wa.x + gap / 2, InsertAt::Index(0)),
        (wa.x + canvas_w - gap / 2, InsertAt::End),
    ] {
        let vis_x = canvas_x - scroll_x;
        if vis_x < wa.x || vis_x > wa.x + wa.w {
            continue;
        }
        widgets.plus_regions.push((plus_rect(vis_x, edge_cy), at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Dir;

    const WA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 800,
    };

    fn placement(state: &State, wa: Rect) -> Vec<Placement> {
        let leaves = state.tree.collect_leaves();
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
                    active_client: state.tree.leaf(leaf).and_then(|l| l.client),
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

    /// Regardless of how many root-level columns exist, there are always
    /// exactly two edge handles (left margin, right margin) — not one per
    /// column.
    #[test]
    fn edge_handles_stay_at_exactly_two_with_more_columns() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.insert_at_root(InsertAt::Index(1));
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.edge_handle_regions.len(), 2);
    }

    #[test]
    fn one_boundary_handle_per_gap_between_columns() {
        let mut s = State::new();
        s.split_focused(Dir::H); // 2 columns -> 1 gap
        s.insert_at_root(InsertAt::Index(1)); // 3 columns -> 2 gaps
        let mut widgets = Widgets::default();
        compute_boundary_widgets(&mut widgets, &s, WA);
        assert_eq!(widgets.handle_regions.len(), 2);
    }

    #[test]
    fn taskbar_stride_never_overlaps_within_available_width() {
        let tree = crate::tree::Tree::new();
        let clients: Vec<(Win, &str)> = Vec::new();
        // A pathological number of windows: the stride must compress
        // (clamped at a floor of 10px) rather than run tiles off-screen or
        // silently drop any of them.
        let bar_order: Vec<Win> = (0..200).collect();
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &tree, &clients, &[], &bar_order, WA, &[]);
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
        let tree = crate::tree::Tree::new();
        let clients: Vec<(Win, &str)> = vec![(1 as Win, "Firefox")];
        let quick = [QuickSlot {
            cmd: "firefox".into(),
            icon: None,
            label: 'F',
            show: theme::ShowWhen::UnlessRunning("firefox"),
        }];
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &tree, &clients, &quick, &[], WA, &[]);
        assert!(
            widgets.quick_regions.is_empty(),
            "quick-launch entry must hide once its class is already running"
        );
    }

    #[test]
    fn quick_launch_shown_when_its_class_is_not_running() {
        let tree = crate::tree::Tree::new();
        let clients: Vec<(Win, &str)> = Vec::new();
        let quick = [QuickSlot {
            cmd: "firefox".into(),
            icon: None,
            label: 'F',
            show: theme::ShowWhen::UnlessRunning("firefox"),
        }];
        let mut widgets = Widgets::default();
        compute_taskbar(&mut widgets, &tree, &clients, &quick, &[], WA, &[]);
        assert_eq!(widgets.quick_regions.len(), 1);
    }

    #[test]
    fn minimized_leaf_gets_one_full_frame_restore_button() {
        let mut s = State::new();
        s.split_focused(Dir::H); // a lone root leaf can't be minimized
        let minimized_leaf = s.tree.first_leaf(s.tree.root);
        assert!(s.toggle_minimize(minimized_leaf));
        let placed = placement(&s, WA);
        let mut widgets = Widgets::default();
        compute_leaf_widgets(&mut widgets, &s.tree, &placed);
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
