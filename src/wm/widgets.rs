//! Widget computation (hit-regions, tabs, buttons, taskbar) for the underlay.

use super::types::{BtnKind, FrameRect, Placement, TaskTile, Wm};
use crate::theme;
use crate::tree::{Dir, Rect};

impl Wm {
    pub(crate) fn compute_widgets(&mut self, wa: Rect, placed: &[Placement]) {
        self.widgets.clear();

        self.compute_leaf_widgets(placed);
        self.compute_boundary_widgets(wa);
    }

    /// Lay out the bottom bar's tiles: one per managed window (in stable
    /// `bar_order`) across the full screen width. Each tile's accent colour and
    /// on-screen flag are resolved here, once per arrange, so the per-frame
    /// compositor needs no tree walks.
    pub(crate) fn compute_taskbar(&mut self) {
        let wa = self.wa();
        let gap = theme::TASKBAR_GAP;
        let isz = theme::TASKBAR_ICON;
        let cbs = theme::TASKBAR_CLOSE;
        // Centre the tile + close-badge group vertically in the bar; the
        // badge overlaps the tile's bottom edge slightly.
        let overlap = 4;
        let pad = (theme::TASKBAR_H - (isz + cbs - overlap)) / 2;
        let y = wa.y + wa.h - theme::TASKBAR_H + pad;
        // Quick-launch icons are pinned right-aligned at the bar's right
        // edge, in `theme::QUICK` order; window tiles fill from the left in
        // the space that remains, with a separator pill between the groups.
        let bar_right = wa.x + wa.w - theme::GAP;
        let nq = i32::try_from(self.quick.len()).unwrap_or(0);
        let quick_w = (nq * (isz + gap) - gap).max(0);
        let mut qx = bar_right - quick_w;
        self.widgets.quick_regions.clear();
        for i in 0..self.quick.len() {
            self.widgets.quick_regions.push((
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
        // Tiles fill from the left, stopping short of the separator + quick
        // group. Left/right edge margins match the split gap. When the bar
        // can't hold every window at full pitch, the stride compresses
        // (tiles overlap like fanned cards, rightmost on top — draw order
        // and the reversed hit-tests in `on_button` agree on that) instead
        // of silently dropping tiles: a window without a tile would be
        // unreachable by mouse entirely.
        let sep_w = 6;
        let right = if nq > 0 {
            bar_right - quick_w - gap - sep_w - gap
        } else {
            bar_right
        };
        let left = wa.x + theme::GAP;
        let full_stride = isz + gap;
        let n = i32::try_from(self.bar_order.len()).unwrap_or(i32::MAX);
        let stride = if n > 1 {
            let avail = right - left - isz;
            (avail / (n - 1)).clamp(10, full_stride)
        } else {
            full_stride
        };
        self.widgets.taskbar_sep = (nq > 0 && !self.bar_order.is_empty()).then(|| FrameRect {
            x: right + gap,
            y,
            w: sep_w,
            h: isz,
        });
        // One tree walk for every tile's leaf lookup — `find_leaf_for_client`
        // per tile is O(tiles × tree) on a per-arrange path.
        let mut client_leaf = std::collections::HashMap::new();
        for l in self.state.tree.collect_leaves() {
            if let Some(c) = self.state.tree.leaf(l).and_then(|lf| lf.client) {
                client_leaf.insert(c, l);
            }
        }
        let mut x = left;
        let mut tiles = Vec::with_capacity(self.bar_order.len());
        for &win in &self.bar_order {
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
                accent: leaf.map_or(theme::palette_color::CREAM, |l| self.leaf_color_index(l)),
                in_split: leaf.is_some(),
            });
            x += stride;
        }
        self.widgets.taskbar_regions = tiles;
    }

    /// Per-leaf titlebar hit-rects, trailing "+" new-tab buttons, and split-control buttons.
    pub(crate) fn compute_leaf_widgets(&mut self, placed: &[Placement]) {
        let tb_h = theme::tb_h();
        let bw = theme::BORDER_LEFT;
        for p in placed {
            let leaf = self.state.tree.leaf(p.leaf);
            let has_client = leaf.is_some_and(|l| l.client.is_some());
            let minimized = leaf.is_some_and(|l| l.minimized);
            if has_client && !minimized {
                self.widgets.tab_regions.push((
                    FrameRect {
                        x: p.target.x + bw,
                        y: p.target.y,
                        w: (p.target.w - 2 * bw).max(0),
                        h: tb_h,
                    },
                    p.leaf,
                ));
            }
            self.compute_btn_regions(p, tb_h, bw, minimized);
        }
    }

    /// Split-control buttons on the right of a leaf's titlebar; a minimized
    /// leaf instead gets one full-frame region (the whole bitmap is the
    /// restore button, drawn by `draw_leaf`).
    pub(crate) fn compute_btn_regions(
        &mut self,
        p: &Placement,
        tb_h: i32,
        bw: i32,
        minimized: bool,
    ) {
        if minimized {
            self.widgets
                .btn_regions
                .push((p.target, p.leaf, BtnKind::Minimize));
            return;
        }
        let bsz = theme::BTN_SIZE;
        let bsp = theme::BTN_SPACING;
        let bcy = p.target.y + tb_h / 2 + theme::BTN_Y_OFFSET;
        if p.target.w >= theme::min_split_w() {
            let right = p.target.x + p.target.w - bw - 4;
            for (i, kind) in [BtnKind::Close, BtnKind::Split, BtnKind::Minimize]
                .into_iter()
                .enumerate()
            {
                let bcx = right - bsz / 2 - i32::try_from(i).unwrap_or(0) * (bsz + bsp);
                self.widgets.btn_regions.push((
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
            self.widgets.btn_regions.push((
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
    pub(crate) fn compute_boundary_widgets(&mut self, wa: Rect) {
        let gap = theme::GAP;
        let hw = (gap - Self::HANDLE_INSET).max(4);
        let scroll_x = self.state.scroll_x();
        let canvas_w = self.state.canvas_w(wa);
        for b in self.state.boundaries(wa) {
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
            self.widgets.handle_regions.push((rect, b));
            if b.root && b.dir == Dir::H {
                let py = b.cross + (b.cross_len - Self::PLUS_SZ) / 2;
                self.widgets
                    .plus_regions
                    .push((Self::plus_rect(b.pos - scroll_x, py), b.idx + 1));
            }
        }
        self.compute_edge_plus_buttons(wa, scroll_x, canvas_w, gap);
        self.compute_edge_handle_widgets(wa);
    }

    /// Drag handles at the outer left/right canvas margins, letting the
    /// leftmost/rightmost column grow or shrink into its own margin — the
    /// edge-of-canvas analogue of the internal boundary handles above.
    /// Only present once there are at least two root-level columns (see
    /// `State::edge_span`); nothing to grab otherwise.
    pub(crate) fn compute_edge_handle_widgets(&mut self, wa: Rect) {
        let gap = theme::GAP;
        let scroll_x = self.state.scroll_x();
        let span_h = (wa.h - 2 * gap).max(1);
        for left in [true, false] {
            let Some((start_x, w)) = self.state.edge_span(wa, left) else {
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
            self.widgets.edge_handle_regions.push((
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
    pub(crate) fn compute_edge_plus_buttons(
        &mut self,
        wa: Rect,
        scroll_x: i32,
        canvas_w: i32,
        gap: i32,
    ) {
        let span_h = (wa.h - 2 * gap).max(Self::PLUS_SZ);
        let edge_cy = wa.y + gap + (span_h - Self::PLUS_SZ) / 2;
        for (canvas_x, at) in [
            (wa.x + gap / 2, 0usize),
            (wa.x + canvas_w - gap / 2, usize::MAX),
        ] {
            let vis_x = canvas_x - scroll_x;
            if vis_x < wa.x || vis_x > wa.x + wa.w {
                continue;
            }
            self.widgets
                .plus_regions
                .push((Self::plus_rect(vis_x, edge_cy), at));
        }
    }
}
