//! Layout state plus every mutation of the split tree / tab stacks —
//! there is exactly one layout (no workspaces/tags).
//! Combines splitwm core.lua + ops.lua + scroll bookkeeping.

use crate::theme;
use crate::tree::{Boundary, Dir, Node, NodeId, Rect, Tree, Win};

pub struct State {
    pub tree: Tree,
    pub focused_leaf: NodeId,
    pub scroll_x: i32,
    pub scroll_target: i32,
    /// Canvas width; None falls back to the workarea width.
    pub canvas_w: Option<i32>,
    /// Extra scrollable width past `canvas_w` reserved for the docked
    /// sidebar (see `Wm::manage_dock`), so scrolling all the way right
    /// reveals it even though it sits outside the split tree and doesn't
    /// affect `compute`'s leaf geometry. Zero when nothing is docked.
    pub dock_extra: i32,
    /// Cumulative manual adjustment to `canvas_w` from dragging an
    /// edge-of-canvas resize handle (see `resize_edge`), layered on top of
    /// `Wm::arrange`'s own leaf-count-driven heuristic every frame so a
    /// manual resize isn't immediately overwritten by it.
    pub canvas_w_extra: i32,
    /// Windows not currently shown in any split, in the bottom taskbar.
    pub taskbar: Vec<Win>,
}

impl State {
    pub fn new() -> Self {
        let tree = Tree::new();
        let root = tree.root;
        Self {
            tree,
            focused_leaf: root,
            scroll_x: 0,
            scroll_target: 0,
            canvas_w: None,
            dock_extra: 0,
            canvas_w_extra: 0,
            taskbar: Vec::new(),
        }
    }

    pub fn focused_leaf_valid(&self) -> NodeId {
        if self.tree.is_leaf(self.focused_leaf) {
            self.focused_leaf
        } else {
            self.tree.first_leaf(self.tree.root)
        }
    }

    // --- window placement helpers ---

    /// Drop a client into the bottom taskbar (deduplicated).
    fn push_taskbar(&mut self, c: Win) {
        if !self.taskbar.contains(&c) {
            self.taskbar.push(c);
        }
    }

    /// Detach `c` from wherever it lives (a split or the taskbar).
    fn detach(&mut self, c: Win) {
        if let Some(lid) = self.tree.find_leaf_for_client(c) {
            if let Some(l) = self.tree.leaf_mut(lid) {
                l.client = None;
            }
        }
        self.taskbar.retain(|&w| w != c);
    }

    /// Place a new client into the focused leaf, bumping any current occupant
    /// down to the taskbar.
    pub fn pin_client(&mut self, c: Win) {
        if self.tree.find_leaf_for_client(c).is_some() || self.taskbar.contains(&c) {
            return;
        }
        self.assign_to_leaf(c, self.focused_leaf_valid());
    }

    /// Put `c` into leaf `dst`, displacing the existing occupant to the taskbar.
    /// `c` is first detached from its previous home.
    pub fn assign_to_leaf(&mut self, c: Win, dst: NodeId) {
        if !self.tree.is_leaf(dst) {
            return;
        }
        self.detach(c);
        let displaced = self.tree.leaf(dst).and_then(|l| l.client);
        if let Some(prev) = displaced {
            if prev != c {
                self.push_taskbar(prev);
            }
        }
        if let Some(l) = self.tree.leaf_mut(dst) {
            l.client = Some(c);
            if displaced.is_some_and(|p| p != c) {
                l.prev = displaced;
            }
        }
        self.focused_leaf = dst;
    }

    /// Remove a client entirely (window gone): clear it from its split/taskbar.
    /// If the leaf it occupied remembers a displaced window (`Leaf::prev`)
    /// that's still in the taskbar, that window is put back into the split —
    /// closing a focus-stealing popup restores what it displaced.
    pub fn unpin_client(&mut self, c: Win) {
        let lid = self.tree.find_leaf_for_client(c);
        self.detach(c);
        if let Some(lid) = lid {
            let prev = self.tree.leaf(lid).and_then(|l| l.prev);
            if let Some(p) = prev {
                if self.taskbar.contains(&p) {
                    self.taskbar.retain(|&w| w != p);
                    if let Some(l) = self.tree.leaf_mut(lid) {
                        l.client = Some(p);
                    }
                }
            }
            if let Some(l) = self.tree.leaf_mut(lid) {
                l.prev = None;
            }
        }
        // The destroyed window can't come back anywhere.
        for leaf in self.tree.collect_leaves() {
            if let Some(l) = self.tree.leaf_mut(leaf) {
                if l.prev == Some(c) {
                    l.prev = None;
                }
            }
        }
    }

    /// Focus whatever split currently shows `c`.
    pub fn activate_client(&mut self, c: Win) -> bool {
        if let Some(lid) = self.tree.find_leaf_for_client(c) {
            self.focused_leaf = lid;
            return true;
        }
        false
    }

    /// Currently shown client of the focused leaf.
    pub fn focused_client(&self) -> Option<Win> {
        self.tree.leaf(self.focused_leaf_valid())?.client
    }

    /// Swap the focused split's window with the next/prev taskbar entry,
    /// cycling which off-screen window is shown.
    pub fn cycle_taskbar(&mut self, forward: bool) -> Option<Win> {
        if self.taskbar.is_empty() {
            return None;
        }
        let lid = self.focused_leaf_valid();
        let displaced = self.tree.leaf(lid).and_then(|l| l.client);
        let next = if forward {
            self.taskbar.remove(0)
        } else {
            self.taskbar.pop()?
        };
        self.assign_to_leaf(next, lid);
        // `assign_to_leaf` pushes the displaced occupant to the *back* —
        // exactly where backward cycling pops from, which would make prev
        // flip-flop between two windows instead of walking the list in
        // reverse. Move it to the front so forward and backward are true
        // inverse rotations of the same queue.
        if !forward {
            if let Some(d) = displaced {
                self.taskbar.retain(|&w| w != d);
                self.taskbar.insert(0, d);
            }
        }
        Some(next)
    }

    // --- focus / move between splits ---

    fn adjacent_leaf(&self, from: NodeId, next: bool) -> Option<NodeId> {
        let leaves = self.tree.collect_leaves();
        if leaves.len() < 2 {
            return None;
        }
        let cur = leaves.iter().position(|&l| l == from)?;
        let n = leaves.len();
        let i = if next {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        Some(leaves[i])
    }

    pub fn focus_direction(&mut self, next: bool) -> bool {
        if let Some(l) = self.adjacent_leaf(self.focused_leaf_valid(), next) {
            self.focused_leaf = l;
            true
        } else {
            false
        }
    }

    /// Move the focused window to the adjacent split (displacing its occupant
    /// to the taskbar). Returns the moved client.
    pub fn move_tab_to_direction(&mut self, next: bool) -> Option<Win> {
        let src = self.focused_leaf_valid();
        let dst = self.adjacent_leaf(src, next)?;
        let c = self.tree.leaf(src)?.client?;
        self.assign_to_leaf(c, dst);
        Some(c)
    }

    /// Toggle a leaf's minimized flag (the layout collapses it to min size).
    pub fn toggle_minimize(&mut self, leaf: NodeId) {
        if let Some(l) = self.tree.leaf_mut(leaf) {
            l.minimized = !l.minimized;
        }
    }

    // --- splitting ---

    fn split_node(&mut self, leaf: NodeId, dir: Dir, child_a: NodeId, child_b: NodeId) {
        if let Some((parent, idx)) = self.tree.find_parent(leaf) {
            let same_dir =
                matches!(self.tree.get(parent), Some(Node::Branch { dir: d, .. }) if *d == dir);
            if same_dir {
                if let Some(Node::Branch {
                    children, ratios, ..
                }) = self.tree.get_mut(parent)
                {
                    let old_r = ratios[idx];
                    ratios[idx] = old_r * theme::SPLIT_RATIO;
                    ratios.insert(idx + 1, old_r * (1.0 - theme::SPLIT_RATIO));
                    children[idx] = child_a;
                    children.insert(idx + 1, child_b);
                }
                return;
            }
            let branch = self
                .tree
                .make_branch(dir, theme::SPLIT_RATIO, child_a, child_b);
            if let Some(Node::Branch { children, .. }) = self.tree.get_mut(parent) {
                children[idx] = branch;
            }
        } else {
            // leaf is root
            let branch = self
                .tree
                .make_branch(dir, theme::SPLIT_RATIO, child_a, child_b);
            self.tree.root = branch;
        }
    }

    /// Split the focused leaf; the existing window stays in `child_a` (now
    /// focused) and `child_b` starts empty.
    pub fn split_focused(&mut self, dir: Dir) {
        let leaf = self.focused_leaf_valid();
        // child_a keeps the original split's window *and* its accent colour, so
        // colour stays with the content; child_b gets a fresh colour.
        // `leaf` should always resolve (it came from `focused_leaf_valid`),
        // but a dangling id degrades to an empty leaf rather than panicking.
        let child_a = self.tree.insert_node(Node::Leaf(
            self.tree.leaf(leaf).cloned().unwrap_or_else(|| {
                debug_assert!(
                    false,
                    "split_focused: focused_leaf_valid() returned a dangling id {leaf:?}"
                );
                eprintln!(
                    "split_focused: focused leaf {leaf:?} not found in tree; \
                     falling back to an empty leaf"
                );
                Default::default()
            }),
        ));
        let child_b = self.tree.make_leaf();
        // Insert a branch in place of `leaf`, with child_a carrying the window.
        self.split_node(leaf, dir, child_a, child_b);
        // The old leaf node is now detached; drop it.
        self.tree.remove_node(leaf);
        self.focused_leaf = child_a;
    }

    /// Relocate `leaf`'s window: into the adjacent sibling's first leaf if it
    /// is empty, otherwise onto the taskbar. `idx` is `leaf`'s index among
    /// `parent`'s children.
    fn relocate_closed_window(&mut self, parent: NodeId, idx: usize, leaf: NodeId) -> bool {
        // Relocate this leaf's window: into the adjacent sibling's first leaf
        // if it is empty, otherwise onto the taskbar.
        let client = self.tree.leaf(leaf).and_then(|l| l.client);
        let dest_child = {
            let children = match self.tree.get(parent) {
                Some(Node::Branch { children, .. }) => children.clone(),
                _ => return false,
            };
            if children.len() < 2 {
                // Degenerate single-child branch: no sibling to fall back
                // to (and `children[1]` below would be out of bounds).
                return false;
            }
            let dest_idx = if idx > 0 { idx - 1 } else { 1 };
            self.tree.first_leaf(children[dest_idx])
        };
        if let Some(c) = client {
            let dest_free = self
                .tree
                .leaf(dest_child)
                .is_some_and(|l| l.client.is_none());
            if dest_free {
                // Carry the closed leaf's displaced-window memory
                // (`Leaf::prev`) along with its window, so popup-restore
                // still works after the split it happened in is closed.
                let prev = self.tree.leaf(leaf).and_then(|l| l.prev);
                if let Some(d) = self.tree.leaf_mut(dest_child) {
                    d.client = Some(c);
                    if d.prev.is_none() {
                        d.prev = prev;
                    }
                }
            } else {
                self.push_taskbar(c);
            }
        }
        true
    }

    /// n-ary close path: remove `leaf` (at index `idx` of `parent`'s
    /// children), redistributing its ratio among the survivors, and refocus
    /// the nearest surviving neighbour.
    fn close_focused_nary(&mut self, parent: NodeId, idx: usize, leaf: NodeId) {
        // n-ary: remove this child, redistribute its ratio.
        if let Some(Node::Branch {
            children, ratios, ..
        }) = self.tree.get_mut(parent)
        {
            let removed = ratios[idx];
            children.remove(idx);
            ratios.remove(idx);
            let remaining: f64 = ratios.iter().sum();
            if remaining > 0.0 {
                for r in ratios.iter_mut() {
                    *r += removed * *r / remaining;
                }
            }
        }
        self.tree.remove_node(leaf);
        let children = match self.tree.get(parent) {
            Some(Node::Branch { children, .. }) => children.clone(),
            _ => unreachable!(),
        };
        let fb = idx.min(children.len() - 1);
        self.focused_leaf = self.tree.first_leaf(children[fb]);
    }

    /// Binary close path: collapse `parent`, the surviving sibling (at index
    /// `idx`'s opposite) takes its place, and same-direction branches are
    /// flattened back into the grandparent.
    fn close_focused_binary(&mut self, parent: NodeId, idx: usize, leaf: NodeId) -> bool {
        // binary: collapse parent, sibling takes its place.
        let sibling = match self.tree.get(parent) {
            Some(Node::Branch { children, .. }) => children[usize::from(idx == 0)],
            _ => return false,
        };
        self.tree.remove_node(leaf);
        // Resolve focus before any flattening: the sibling *node* may
        // be dissolved into the grandparent below, but its leaves
        // survive.
        let new_focus = self.tree.first_leaf(sibling);
        if parent == self.tree.root {
            self.tree.root = sibling;
            self.tree.remove_node(parent);
        } else if let Some((grand, pidx)) = self.tree.find_parent(parent) {
            if let Some(Node::Branch { children, .. }) = self.tree.get_mut(grand) {
                children[pidx] = sibling;
            }
            self.tree.remove_node(parent);
            // The spliced-in sibling can be a branch in the
            // grandparent's own direction; dissolve it so same-dir
            // splits stay one flat n-ary branch.
            self.tree.flatten_same_dir(grand, pidx);
        } else {
            self.tree.remove_node(parent);
        }
        self.focused_leaf = new_focus;
        true
    }

    /// Close the focused leaf. Its window moves into the adjacent sibling if
    /// that split is empty, otherwise down to the taskbar.
    pub fn close_focused(&mut self) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false; // root leaf: nothing to close
        };

        if !self.relocate_closed_window(parent, idx, leaf) {
            return false;
        }

        let nchildren = match self.tree.get(parent) {
            Some(Node::Branch { children, .. }) => children.len(),
            _ => return false,
        };

        // Focus always moves to the nearest surviving neighbour: the closed
        // leaf *was* the focused one (node ids are never reused, so it can't
        // still be found anywhere in the tree after removal).
        if nchildren > 2 {
            self.close_focused_nary(parent, idx, leaf);
            true
        } else {
            self.close_focused_binary(parent, idx, leaf)
        }
    }

    // --- resize ---

    pub fn resize_focused(&mut self, delta: f64) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false;
        };
        if let Some(Node::Branch {
            children, ratios, ..
        }) = self.tree.get_mut(parent)
        {
            let n = children.len();
            if n < 2 {
                // No sibling to trade width with (also guards the `idx - 1`
                // underflow below if a degenerate single-child branch ever
                // exists).
                return false;
            }
            let other = if idx + 1 < n { idx + 1 } else { idx - 1 };
            let min_r = theme::MIN_SPLIT_FRAC;
            let cur = ratios[idx];
            let cur_other = ratios[other];
            // Cap the transfer at what each side can actually give, so the
            // pair's sum is exactly conserved — clamping both ends
            // independently let the total ratio mass drift upward once the
            // neighbour bottomed out, silently shrinking every *other*
            // sibling via renormalisation.
            let (lo, hi) = ((min_r - cur).min(0.0), (cur_other - min_r).max(0.0));
            let delta = delta.clamp(lo, hi);
            if delta == 0.0 {
                return false;
            }
            ratios[idx] = cur + delta;
            ratios[other] = cur_other - delta;
            true
        } else {
            false
        }
    }

    // --- scroll ---

    pub fn max_scroll(&self, wa: Rect) -> i32 {
        (self.canvas_w.unwrap_or(wa.w) + self.dock_extra - wa.w).max(0)
    }

    pub fn scroll_to(&mut self, wa: Rect, target: i32) {
        self.scroll_target = target.clamp(0, self.max_scroll(wa));
    }

    pub fn scroll_delta(&mut self, wa: Rect, delta: i32) {
        let t = self.scroll_target + delta;
        self.scroll_to(wa, t);
    }

    /// Geometry of every leaf in canvas coordinates.
    pub fn compute(&self, wa: Rect) -> std::collections::HashMap<NodeId, Rect> {
        let gap = theme::GAP;
        let canvas_w = self.canvas_w.unwrap_or(wa.w);
        self.tree.compute(Rect { w: canvas_w, ..wa }, gap)
    }

    /// Gaps between adjacent splits, for drag handles / insert buttons.
    pub fn boundaries(&self, wa: Rect) -> Vec<Boundary> {
        let gap = theme::GAP;
        let canvas_w = self.canvas_w.unwrap_or(wa.w);
        self.tree.boundaries(Rect { w: canvas_w, ..wa }, gap)
    }

    /// Canvas-space x-span `(start_x, width)` of the leftmost/rightmost
    /// root-level column — used to seed and drive an edge-of-canvas resize
    /// drag (see `resize_edge`). A single leaf, or a root that's itself a
    /// vertical branch, count as one column spanning the whole row, so
    /// `left`/`right` both describe the same span in that case (see
    /// `Tree::root_h_sizes`). `None` only if the tree is somehow empty.
    pub fn edge_span(&self, wa: Rect, left: bool) -> Option<(i32, i32)> {
        let gap = theme::GAP;
        let canvas_w = self.canvas_w.unwrap_or(wa.w);
        let sizes = self.tree.root_h_sizes(canvas_w - 2 * gap, gap)?;
        let start_x = wa.x + gap;
        if left {
            Some((start_x, sizes[0]))
        } else {
            let n = sizes.len();
            let before: i32 = sizes[..n - 1].iter().sum();
            let gaps_before = gap * i32::try_from(n - 1).unwrap_or(0);
            Some((start_x + before + gaps_before, sizes[n - 1]))
        }
    }

    /// Resize the leftmost or rightmost root-level column to `target_w`
    /// pixels: the column absorbs the whole delta, every sibling keeps its
    /// exact current pixel width, and `canvas_w` grows/shrinks by that same
    /// delta (via `canvas_w_extra`, layered on top of `Wm::arrange`'s
    /// heuristic each frame) — the canvas itself tracks the resize, the
    /// same way it grows when a new column is inserted. `theme::GAP` (the
    /// margin) never changes. A single leaf (or a vertical-branch root) has
    /// no sibling ratios to redistribute — it's the whole row already, so
    /// only `canvas_w_extra` moves.
    ///
    /// For the left edge specifically, the column's *start* is what's
    /// meant to track the mouse (growing toward the screen edge), but
    /// `Tree::compute` always lays children out left-to-right from a fixed
    /// origin — so growing column 0 there necessarily shifts every later
    /// column's canvas-space x right by `delta`. The caller (`Wm::on_motion`)
    /// nudges `scroll_x`/`scroll_target` by the same `delta` so those
    /// columns stay put on screen and only the dragged edge visibly moves.
    /// Which root children are minimized leaves: their pixel width is
    /// pinned to `gap` regardless of ratio (see `child_sizes`), so their
    /// stored ratios must survive the rewrite in `redistribute_column_widths`
    /// untouched — deriving a ratio from the pinned width would crush the
    /// share they restore to.
    /// Built strictly parallel to `widths`: per root H-child, or a single
    /// `false` for the one-column cases (lone leaf, V-branch root, where
    /// `root_h_sizes` returns one full-width span). Matching on any
    /// branch here used to index a V-root's *stacked children* with a
    /// column index — correct only by coincidence of the len-1 guards
    /// around its callers.
    fn root_column_minimized(&self, root: NodeId, widths_len: usize) -> Vec<bool> {
        match self.tree.get(root) {
            Some(Node::Branch {
                dir: Dir::H,
                children,
                ..
            }) => children
                .iter()
                .map(|&c| self.tree.leaf(c).is_some_and(|l| l.minimized))
                .collect(),
            _ => vec![false; widths_len],
        }
    }

    /// Multi-column case of `resize_edge`: rewrite `root`'s ratios so that
    /// column `idx` becomes `new_w` pixels wide, every other *normal*
    /// (non-minimized) column keeps its relative share, and minimized
    /// columns' ratios are left untouched. Returns `false` if the rewrite
    /// isn't possible (e.g. every other column is minimized).
    fn redistribute_column_widths(
        &mut self,
        root: NodeId,
        idx: usize,
        new_w: i32,
        mut widths: Vec<i32>,
        minimized: &[bool],
    ) -> bool {
        widths[idx] = new_w;
        let total: i32 = widths
            .iter()
            .zip(minimized)
            .filter_map(|(&w, &m)| (!m).then_some(w))
            .sum();
        if total <= 0 {
            return false;
        }
        if let Some(Node::Branch {
            dir: Dir::H,
            ratios,
            ..
        }) = self.tree.get_mut(root)
        {
            // Only normal children's ratios matter to the layout
            // (`child_sizes` normalises over them alone), so rewriting
            // just those reproduces the pixel widths exactly.
            for ((r, &w), &m) in ratios.iter_mut().zip(widths.iter()).zip(minimized) {
                if !m {
                    *r = f64::from(w) / f64::from(total);
                }
            }
        }
        true
    }

    pub fn resize_edge(&mut self, wa: Rect, left: bool, target_w: i32) -> i32 {
        let root = self.tree.root;
        let gap = theme::GAP;
        let canvas_w = self.canvas_w.unwrap_or(wa.w);
        let Some(widths) = self.tree.root_h_sizes(canvas_w - 2 * gap, gap) else {
            return 0;
        };
        let idx = if left { 0 } else { widths.len() - 1 };
        let minimized = self.root_column_minimized(root, widths.len());
        debug_assert_eq!(widths.len(), minimized.len());
        if widths.len() > 1 && minimized[idx] {
            // The end column itself being minimized makes the whole drag
            // meaningless (old_w is the pinned gap, not a real width).
            return 0;
        }
        let old_w = widths[idx];
        let min_w = theme::min_split_w();
        let new_w = target_w.max(min_w);
        let delta = new_w - old_w;
        if delta == 0 {
            return 0;
        }
        if widths.len() > 1
            && !self.redistribute_column_widths(root, idx, new_w, widths, &minimized)
        {
            return 0;
        }
        self.canvas_w_extra += delta;
        delta
    }

    /// Set the split ratio at a boundary so the left child occupies fraction
    /// `frac` of the two neighbours' combined width (their sum is preserved).
    pub fn resize_boundary(&mut self, parent: NodeId, idx: usize, frac: f64) {
        if let Some(Node::Branch { ratios, .. }) = self.tree.get_mut(parent) {
            if idx + 1 < ratios.len() {
                let combined = ratios[idx] + ratios[idx + 1];
                let f = frac.clamp(theme::MIN_SPLIT_FRAC, 1.0 - theme::MIN_SPLIT_FRAC);
                ratios[idx] = combined * f;
                ratios[idx + 1] = combined * (1.0 - f);
            }
        }
    }

    /// Insert a new empty leaf column at root-children index `at`, making the
    /// root an H-branch if it isn't one. The new leaf becomes focused.
    #[allow(clippy::cast_precision_loss)]
    pub fn insert_at_root(&mut self, at: usize) -> NodeId {
        let new = self.tree.make_leaf();
        let root = self.tree.root;
        let is_h = matches!(self.tree.get(root), Some(Node::Branch { dir: Dir::H, .. }));
        if is_h {
            if let Some(Node::Branch {
                children, ratios, ..
            }) = self.tree.get_mut(root)
            {
                let avg = ratios.iter().sum::<f64>() / ratios.len() as f64;
                let i = at.min(children.len());
                children.insert(i, new);
                ratios.insert(i, avg);
                let s: f64 = ratios.iter().sum();
                for r in ratios.iter_mut() {
                    *r /= s;
                }
            }
        } else {
            let branch = self.tree.make_branch(Dir::H, theme::SPLIT_RATIO, root, new);
            if at == 0 {
                if let Some(Node::Branch { children, .. }) = self.tree.get_mut(branch) {
                    children.swap(0, 1);
                }
            }
            self.tree.root = branch;
        }
        self.focused_leaf = new;
        new
    }

    /// Scroll so the focused split sits inside the viewport (one gap margin).
    pub fn ensure_in_view(&mut self, wa: Rect) {
        let geos = self.compute(wa);
        let geo = match geos.get(&self.focused_leaf_valid()) {
            Some(g) => *g,
            None => return,
        };
        let gap = theme::GAP;
        let sx = self.scroll_x;
        let mut target = sx;
        if geo.x - sx < wa.x + gap {
            target = geo.x - wa.x - gap;
        } else if geo.x + geo.w - sx > wa.x + wa.w - gap {
            target = geo.x + geo.w - wa.x - wa.w + gap;
        }
        if target != sx {
            self.scroll_to(wa, target);
        }
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

    #[test]
    fn insert_at_root_grows_columns() {
        let mut s = State::new();
        s.split_focused(Dir::H); // root H-branch, 2 columns
        assert_eq!(s.tree.collect_leaves().len(), 2);
        s.insert_at_root(1); // insert between
        assert_eq!(s.tree.collect_leaves().len(), 3);
        // The inserted leaf is focused and empty.
        assert!(s.focused_client().is_none());
        // Ratios renormalise to sum 1.
        if let Some(Node::Branch { ratios, .. }) = s.tree.get(s.tree.root) {
            let sum: f64 = ratios.iter().sum();
            assert!((sum - 1.0).abs() < 1e-9, "ratios sum {sum}");
        } else {
            panic!("root not a branch");
        }
    }

    #[test]
    fn insert_at_root_wraps_single_leaf() {
        let mut s = State::new();
        s.insert_at_root(0); // root is a lone leaf -> wrap into H-branch
        assert_eq!(s.tree.collect_leaves().len(), 2);
        assert!(matches!(
            s.tree.get(s.tree.root),
            Some(Node::Branch { dir: Dir::H, .. })
        ));
    }

    #[test]
    fn resize_boundary_preserves_neighbour_sum() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        let root = s.tree.root;
        let before = match s.tree.get(root) {
            Some(Node::Branch { ratios, .. }) => ratios[0] + ratios[1],
            _ => panic!(),
        };
        s.resize_boundary(root, 0, 0.25);
        match s.tree.get(root) {
            Some(Node::Branch { ratios, .. }) => {
                assert!((ratios[0] + ratios[1] - before).abs() < 1e-9);
                assert!((ratios[0] / (ratios[0] + ratios[1]) - 0.25).abs() < 1e-9);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn boundaries_match_column_count() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.canvas_w = Some(WA.w);
        // One gap between two columns.
        assert_eq!(s.boundaries(WA).len(), 1);
        s.insert_at_root(1);
        s.canvas_w = Some(WA.w);
        assert_eq!(s.boundaries(WA).len(), 2);
    }

    #[test]
    fn splits_get_unique_colors_up_to_palette_size() {
        let mut s = State::new();
        for _ in 0..7 {
            s.split_focused(Dir::H);
        }
        let leaves = s.tree.collect_leaves();
        assert_eq!(leaves.len(), 8);
        let colors: Vec<_> = leaves
            .iter()
            .map(|&l| s.tree.leaf(l).unwrap().color)
            .collect();
        let unique: std::collections::HashSet<_> = colors.iter().collect();
        assert_eq!(
            unique.len(),
            8,
            "expected all 8 leaves to have distinct colors"
        );

        // A 9th split must reuse a color (only 8 available) but shouldn't panic.
        s.split_focused(Dir::H);
        assert_eq!(s.tree.collect_leaves().len(), 9);
    }

    #[test]
    fn closing_and_resplitting_still_avoids_collisions() {
        let mut s = State::new();
        for _ in 0..3 {
            s.split_focused(Dir::H);
        }
        // Close one, then split again — the freed color should be reusable
        // without colliding with the remaining leaves.
        s.close_focused();
        s.split_focused(Dir::H);
        let leaves = s.tree.collect_leaves();
        let colors: Vec<_> = leaves
            .iter()
            .map(|&l| s.tree.leaf(l).unwrap().color)
            .collect();
        let unique: std::collections::HashSet<_> = colors.iter().collect();
        assert_eq!(unique.len(), colors.len());
    }

    /// `dock_extra` must open up exactly enough extra scroll room to slide
    /// the docked sidebar fully into view, mirroring `Wm::place_dock`'s
    /// `x = wa.x + canvas_w - overlap - scroll_x` formula (overlap =
    /// `DOCK_OVERLAP` clamped to the dock's width, see `Wm::dock_overlap`),
    /// even when there's only one column and the canvas alone has no scroll
    /// room of its own.
    #[test]
    fn dock_extra_reveals_sidebar_at_max_scroll() {
        let mut s = State::new();
        s.canvas_w = Some(WA.w); // single leaf: canvas == viewport, no scroll room on its own
        let docked_w = 300;
        let overlap = theme::DOCK_OVERLAP.min(docked_w);
        s.dock_extra = docked_w - overlap;

        assert_eq!(s.max_scroll(WA), docked_w - overlap);

        let canvas_w = s.canvas_w.unwrap();
        let dock_x_at = |scroll_x: i32| WA.x + canvas_w - overlap - scroll_x;

        // Before scrolling, only the tucked-under overlap strip reaches
        // on-screen (it sits below the canvas in stacking).
        assert_eq!(dock_x_at(0), WA.x + WA.w - overlap);

        // Scrolling to the clamped max brings it flush to the right edge.
        s.scroll_to(WA, i32::MAX);
        assert_eq!(s.scroll_target, docked_w - overlap);
        assert_eq!(dock_x_at(s.scroll_target), WA.x + WA.w - docked_w);
    }

    /// A single leaf has no sibling to trade width with, but it should
    /// still be resizable — both edges describe the same full-width span,
    /// and shrinking it should work exactly like a two-column edge resize
    /// minus the ratio bookkeeping (see `resize_edge_shrinks_lone_leaf`).
    #[test]
    fn edge_span_is_full_width_for_single_leaf() {
        let s = State::new();
        let canvas_w = s.canvas_w.unwrap_or(WA.w);
        let want = (WA.x + crate::theme::GAP, canvas_w - 2 * crate::theme::GAP);
        assert_eq!(s.edge_span(WA, true), Some(want));
        assert_eq!(s.edge_span(WA, false), Some(want));
    }

    /// Shrinking the lone leaf from the right edge should narrow it by
    /// exactly the requested delta and shrink `canvas_w` by the same
    /// amount, with no ratios to touch (there's no `Node::Branch` at all).
    #[test]
    fn resize_edge_shrinks_lone_leaf() {
        let mut s = State::new();
        s.canvas_w = Some(WA.w);
        let (_, w_before) = s.edge_span(WA, false).unwrap();

        let shrink_by = 50;
        let applied = s.resize_edge(WA, false, w_before - shrink_by);
        assert_eq!(applied, -shrink_by);
        assert_eq!(s.canvas_w_extra, -shrink_by);

        s.canvas_w = Some(WA.w + s.canvas_w_extra);
        let (_, w_after) = s.edge_span(WA, false).unwrap();
        assert_eq!(w_after, w_before - shrink_by);
    }

    /// Growing the left column via `resize_edge` should widen it by exactly
    /// the requested delta, leave the other column's pixel width untouched,
    /// grow `canvas_w` by that same delta (so the scrollable canvas tracks
    /// the resize, the way it does for every other canvas-widening
    /// operation), and report the applied delta so the caller can
    /// compensate scroll.
    #[test]
    fn resize_edge_grows_left_column_and_canvas() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.canvas_w = Some(WA.w);
        let canvas_w_before = s.canvas_w.unwrap();

        let before = s.compute(WA);
        let leaves = s.tree.collect_leaves();
        let (left_id, right_id) = (leaves[0], leaves[1]);
        let left_w_before = before[&left_id].w;
        let right_w_before = before[&right_id].w;

        let (start_x, w) = s.edge_span(WA, true).expect("two columns");
        assert_eq!(w, left_w_before);
        assert_eq!(start_x, WA.x + crate::theme::GAP);

        let grow_by = 40;
        let applied = s.resize_edge(WA, true, left_w_before + grow_by);
        assert_eq!(applied, grow_by);
        assert_eq!(s.canvas_w_extra, grow_by);

        // `canvas_w` itself isn't updated by `resize_edge` (that's
        // `Wm::arrange`'s job, layering `canvas_w_extra` on top of its
        // heuristic each frame); simulate that here to check geometry.
        s.canvas_w = Some(canvas_w_before + s.canvas_w_extra);
        let after = s.compute(WA);
        assert_eq!(after[&left_id].w, left_w_before + grow_by);
        assert_eq!(after[&right_id].w, right_w_before);
    }

    /// Backward taskbar cycling must walk the whole list in reverse, not
    /// flip-flop between the shown window and the last taskbar entry
    /// (regression: `assign_to_leaf` pushes the displaced window to the
    /// back, which is exactly where the next backward pop looked).
    #[test]
    fn cycle_taskbar_prev_visits_every_window() {
        let mut s = State::new();
        for w in [10, 1, 2, 3] {
            s.pin_client(w);
        }
        // Leaf shows 3; taskbar is [10, 1, 2].
        assert_eq!(s.focused_client(), Some(3));

        let shown: Vec<_> = (0..4).map(|_| s.cycle_taskbar(false).unwrap()).collect();
        assert_eq!(shown, vec![2, 1, 10, 3], "prev must rotate, not toggle");
    }

    /// One step forward then one step back must restore both the shown
    /// window and the taskbar order.
    #[test]
    fn cycle_taskbar_prev_inverts_next() {
        let mut s = State::new();
        for w in [10, 1, 2, 3] {
            s.pin_client(w);
        }
        let before = s.taskbar.clone();
        s.cycle_taskbar(true);
        s.cycle_taskbar(false);
        assert_eq!(s.focused_client(), Some(3));
        assert_eq!(s.taskbar, before);
    }

    /// A popup that displaces the working window and is then destroyed must
    /// give the split back to the displaced window (pulled from the taskbar).
    #[test]
    fn closing_popup_restores_displaced_window() {
        let mut s = State::new();
        s.pin_client(1); // working window
        s.pin_client(99); // popup steals the split; 1 -> taskbar
        assert_eq!(s.focused_client(), Some(99));
        assert!(s.taskbar.contains(&1));

        s.unpin_client(99); // popup window destroyed
        assert_eq!(s.focused_client(), Some(1), "displaced window comes back");
        assert!(!s.taskbar.contains(&1));
    }

    /// If the displaced window has since left the taskbar (shown elsewhere or
    /// itself closed), the split just stays empty — no stale restore.
    #[test]
    fn no_restore_when_displaced_window_is_gone() {
        let mut s = State::new();
        s.pin_client(1);
        s.pin_client(99);
        s.unpin_client(1); // the remembered window itself is destroyed
        s.unpin_client(99);
        assert_eq!(s.focused_client(), None);
        assert!(s.taskbar.is_empty());
    }

    /// Restoration is single-shot: after a restore the leaf holds no further
    /// history, so closing the restored window doesn't resurrect anything.
    #[test]
    fn restore_is_single_shot() {
        let mut s = State::new();
        s.pin_client(1);
        s.pin_client(2); // prev = 1
        s.pin_client(3); // prev = 2, taskbar [1, 2]
        s.unpin_client(3); // restores 2
        assert_eq!(s.focused_client(), Some(2));
        s.unpin_client(2); // prev was consumed; 1 stays in the taskbar
        assert_eq!(s.focused_client(), None);
        assert_eq!(s.taskbar, vec![1]);
    }

    /// A degenerate single-child branch must not panic `close_focused`
    /// (`children[1]` when there is no sibling).
    #[test]
    fn close_focused_survives_single_child_branch() {
        let mut s = State::new();
        let leaf = s.tree.first_leaf(s.tree.root);
        let branch = s.tree.insert_node(Node::Branch {
            dir: Dir::H,
            children: vec![leaf],
            ratios: vec![1.0],
        });
        s.tree.root = branch;
        s.focused_leaf = leaf;
        assert!(!s.close_focused());
    }

    /// Repeated growing must stop once the neighbour bottoms out at
    /// `MIN_SPLIT_FRAC`, conserving the pair's ratio sum exactly — clamping
    /// both sides independently used to let total ratio mass drift upward,
    /// silently shrinking every other sibling via renormalisation.
    #[test]
    fn resize_focused_conserves_ratio_sum() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.insert_at_root(2); // three columns
        s.focused_leaf = s.tree.collect_leaves()[0];
        let sum_before: f64 = match s.tree.get(s.tree.root) {
            Some(Node::Branch { ratios, .. }) => ratios.iter().sum(),
            _ => panic!(),
        };
        for _ in 0..100 {
            s.resize_focused(theme::RESIZE_STEP);
        }
        let ratios = match s.tree.get(s.tree.root) {
            Some(Node::Branch { ratios, .. }) => ratios.clone(),
            _ => panic!(),
        };
        let sum_after: f64 = ratios.iter().sum();
        assert!((sum_after - sum_before).abs() < 1e-9, "sum drifted");
        assert!(ratios.iter().all(|&r| r >= theme::MIN_SPLIT_FRAC - 1e-9));
        // Once the neighbour is at its floor, further growth is refused.
        assert!(!s.resize_focused(theme::RESIZE_STEP));
    }

    /// A degenerate single-child branch must not panic `resize_focused`
    /// (`idx - 1` underflow when there is no sibling).
    #[test]
    fn resize_focused_survives_single_child_branch() {
        let mut s = State::new();
        let leaf = s.tree.first_leaf(s.tree.root);
        let branch = s.tree.insert_node(Node::Branch {
            dir: Dir::H,
            children: vec![leaf],
            ratios: vec![1.0],
        });
        s.tree.root = branch;
        s.focused_leaf = leaf;
        assert!(!s.resize_focused(0.1));
    }

    /// Collapsing a binary branch must dissolve a same-direction sibling
    /// branch into the grandparent (`Tree::flatten_same_dir`): nested
    /// same-dir branches demote their gaps from root-level boundaries,
    /// losing the "+" insert buttons between visually root-level columns.
    #[test]
    fn collapse_flattens_same_dir_branches() {
        let mut s = State::new();
        s.split_focused(Dir::H); // root H[a, b]
        s.split_focused(Dir::V); // a -> V[a1, a2]
        s.split_focused(Dir::H); // a1 -> H[x1, x2]
        let leaves = s.tree.collect_leaves(); // x1, x2, a2, b
        assert_eq!(leaves.len(), 4);
        s.focused_leaf = leaves[2]; // a2
        assert!(s.close_focused());
        // The V-branch collapses; H[x1, x2] splices into the root H-branch
        // and must flatten into it: one H-branch, three leaf children.
        match s.tree.get(s.tree.root) {
            Some(Node::Branch {
                dir: Dir::H,
                children,
                ratios,
            }) => {
                assert_eq!(children.len(), 3, "nested same-dir branch survived");
                assert!((ratios.iter().sum::<f64>() - 1.0).abs() < 1e-9);
                for &c in children {
                    assert!(s.tree.is_leaf(c));
                }
            }
            _ => panic!("root not an H-branch"),
        }
        // Focus landed on a surviving leaf, and every root-level gap is
        // insert-eligible again.
        assert!(s.tree.is_leaf(s.focused_leaf));
        s.canvas_w = Some(WA.w);
        assert!(s.boundaries(WA).iter().all(|b| b.root));
    }

    /// Closing a split whose window moves into an empty sibling must carry
    /// the displaced-window memory (`Leaf::prev`) with it, so popup-restore
    /// survives the collapse.
    #[test]
    fn close_focused_carries_prev_to_sibling() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H); // window 1 stays in the focused child
        s.pin_client(2); // displaces 1 -> taskbar, prev = 1
        assert!(s.close_focused()); // 2 moves into the empty sibling
        assert_eq!(s.focused_client(), Some(2));
        s.unpin_client(2); // popup dies: 1 must come back
        assert_eq!(s.focused_client(), Some(1), "prev restore survives collapse");
    }

    /// Closing a middle column moves focus to a surviving neighbour.
    #[test]
    fn close_focused_moves_focus_to_neighbour() {
        let mut s = State::new();
        s.split_focused(Dir::H); // two columns, focus left
        s.insert_at_root(1); // middle column, focused
        assert_eq!(s.tree.collect_leaves().len(), 3);
        assert!(s.close_focused());
        let leaves = s.tree.collect_leaves();
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&s.focused_leaf_valid()));
    }

    /// Canvas width demand is measured in columns: vertical stacking must
    /// not increase `h_units`, horizontal splitting must.
    #[test]
    fn h_units_counts_columns_not_leaves() {
        let mut s = State::new();
        assert_eq!(s.tree.h_units(), 1);
        for _ in 0..3 {
            s.split_focused(Dir::V);
        }
        assert_eq!(s.tree.collect_leaves().len(), 4);
        assert_eq!(s.tree.h_units(), 1, "a vertical stack is one column");
        s.split_focused(Dir::H);
        assert_eq!(s.tree.h_units(), 2);
    }

    /// `parent_map` must agree with `find_parent` for every node.
    #[test]
    fn parent_map_matches_find_parent() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.split_focused(Dir::V);
        s.insert_at_root(0);
        let map = s.tree.parent_map();
        for leaf in s.tree.collect_leaves() {
            assert_eq!(map.get(&leaf).copied(), s.tree.find_parent(leaf));
        }
    }

    /// An edge resize must not rewrite a minimized column's ratio from its
    /// pinned pixel width (regression: un-minimizing after an edge drag
    /// restored the column as a sliver of its former share), and dragging
    /// an end column that is itself minimized is refused outright.
    #[test]
    fn resize_edge_leaves_minimized_ratio_alone() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.insert_at_root(2); // three columns
        s.canvas_w = Some(WA.w);
        let leaves = s.tree.collect_leaves();
        s.toggle_minimize(leaves[1]); // middle column pinned to `gap` px
        let ratio_before = match s.tree.get(s.tree.root) {
            Some(Node::Branch { ratios, .. }) => ratios[1],
            _ => panic!(),
        };
        let (_, w) = s.edge_span(WA, false).unwrap();
        assert_eq!(s.resize_edge(WA, false, w - 40), -40);
        let ratios = match s.tree.get(s.tree.root) {
            Some(Node::Branch { ratios, .. }) => ratios.clone(),
            _ => panic!(),
        };
        assert!(
            (ratios[1] - ratio_before).abs() < 1e-9,
            "minimized column's ratio was rewritten from its pinned width"
        );
        // Minimize the right end column: dragging that edge is refused.
        s.toggle_minimize(leaves[1]);
        s.toggle_minimize(leaves[2]);
        assert_eq!(s.resize_edge(WA, false, 500), 0);
    }

    /// Same as above, mirrored for the right edge (shrinking instead).
    #[test]
    fn resize_edge_shrinks_right_column_and_canvas() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.canvas_w = Some(WA.w);
        let canvas_w_before = s.canvas_w.unwrap();

        let before = s.compute(WA);
        let leaves = s.tree.collect_leaves();
        let (left_id, right_id) = (leaves[0], leaves[1]);
        let left_w_before = before[&left_id].w;
        let right_w_before = before[&right_id].w;

        let shrink_by = 30;
        let applied = s.resize_edge(WA, false, right_w_before - shrink_by);
        assert_eq!(applied, -shrink_by);
        assert_eq!(s.canvas_w_extra, -shrink_by);

        s.canvas_w = Some(canvas_w_before + s.canvas_w_extra);
        let after = s.compute(WA);
        assert_eq!(after[&right_id].w, right_w_before - shrink_by);
        assert_eq!(after[&left_id].w, left_w_before);
    }
}
