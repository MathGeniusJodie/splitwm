//! Per-tag layout state plus every mutation of the split tree / tab stacks.
//! Combines splitwm core.lua + ops.lua + scroll bookkeeping for one tag.

use crate::theme;
use crate::tree::{Boundary, Dir, Leaf, Node, NodeId, Rect, Tree, Win};

pub struct State {
    pub tree: Tree,
    pub focused_leaf: NodeId,
    pub scroll_x: i32,
    pub scroll_target: i32,
    /// Canvas width; None falls back to the workarea width.
    pub canvas_w: Option<i32>,
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
        }
        self.focused_leaf = dst;
    }

    /// Remove a client entirely (window gone): clear it from its split/taskbar.
    pub fn unpin_client(&mut self, c: Win) {
        self.detach(c);
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
    pub fn cycle_taskbar(&mut self, offset: i32) -> Option<Win> {
        if self.taskbar.is_empty() {
            return None;
        }
        let lid = self.focused_leaf_valid();
        let next = if offset >= 0 {
            self.taskbar.remove(0)
        } else {
            self.taskbar.pop()?
        };
        self.assign_to_leaf(next, lid);
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

    /// Swap the contents of two leaves (the "swap split" button). Each leaf
    /// keeps its own persistent colour; only the window/minimized state moves.
    pub fn swap_leaves(&mut self, a: NodeId, b: NodeId) {
        if a == b {
            return;
        }
        let read = |id| self.tree.leaf(id).map(|l| (l.client, l.minimized));
        let (Some(da), Some(db)) = (read(a), read(b)) else {
            return;
        };
        if let Some(l) = self.tree.leaf_mut(a) {
            (l.client, l.minimized) = db;
        }
        if let Some(l) = self.tree.leaf_mut(b) {
            (l.client, l.minimized) = da;
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
        let (client, minimized, color) = {
            let l = self.tree.leaf(leaf).unwrap();
            (l.client, l.minimized, l.color)
        };
        let child_a = self.tree.insert_node(Node::Leaf(Leaf {
            client,
            minimized,
            color,
        }));
        let child_b = self.tree.make_leaf();
        // Insert a branch in place of `leaf`, with child_a carrying the window.
        self.split_node(leaf, dir, child_a, child_b);
        // The old leaf node is now detached; drop it.
        self.tree.remove_node(leaf);
        self.focused_leaf = child_a;
    }

    /// Close the focused leaf. Its window moves into the adjacent sibling if
    /// that split is empty, otherwise down to the taskbar.
    pub fn close_focused(&mut self) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false; // root leaf: nothing to close
        };

        // Relocate this leaf's window: into the adjacent sibling's first leaf
        // if it is empty, otherwise onto the taskbar.
        let client = self.tree.leaf(leaf).and_then(|l| l.client);
        let dest_child = {
            let children = match self.tree.get(parent) {
                Some(Node::Branch { children, .. }) => children.clone(),
                _ => return false,
            };
            let dest_idx = if idx > 0 { idx - 1 } else { 1 };
            self.tree.first_leaf(children[dest_idx])
        };
        if let Some(c) = client {
            let dest_free = self
                .tree
                .leaf(dest_child)
                .is_some_and(|l| l.client.is_none());
            if dest_free {
                if let Some(d) = self.tree.leaf_mut(dest_child) {
                    d.client = Some(c);
                }
            } else {
                self.push_taskbar(c);
            }
        }

        let focused_id = self.focused_leaf;
        let nchildren = match self.tree.get(parent) {
            Some(Node::Branch { children, .. }) => children.len(),
            _ => return false,
        };

        if nchildren > 2 {
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
            let keep = children
                .iter()
                .find(|&&c| self.tree.contains(c, focused_id));
            self.focused_leaf = if let Some(&_) = keep {
                focused_id
            } else {
                let fb = idx.min(children.len() - 1);
                self.tree.first_leaf(children[fb])
            };
        } else {
            // binary: collapse parent, sibling takes its place.
            let sibling = match self.tree.get(parent) {
                Some(Node::Branch { children, .. }) => children[usize::from(idx == 0)],
                _ => return false,
            };
            self.tree.remove_node(leaf);
            if parent == self.tree.root {
                self.tree.root = sibling;
            } else if let Some((grand, pidx)) = self.tree.find_parent(parent) {
                if let Some(Node::Branch { children, .. }) = self.tree.get_mut(grand) {
                    children[pidx] = sibling;
                }
            }
            self.tree.remove_node(parent);
            self.focused_leaf = if self.tree.contains(sibling, focused_id) {
                focused_id
            } else {
                self.tree.first_leaf(sibling)
            };
        }
        true
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
            let other = if idx + 1 < n { idx + 1 } else { idx - 1 };
            let min_r = 0.01;
            let cur = ratios[idx];
            let cur_other = ratios[other];
            let new_cur = (cur + delta).max(min_r);
            ratios[idx] = new_cur;
            ratios[other] = (cur_other - (new_cur - cur)).max(min_r);
            true
        } else {
            false
        }
    }

    // --- scroll ---

    pub fn max_scroll(&self, wa: Rect) -> i32 {
        (self.canvas_w.unwrap_or(wa.w) - wa.w).max(0)
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
        self.tree
            .compute(wa.x, wa.y, canvas_w, wa.h, gap, theme::tb_h(gap))
    }

    /// Vertical gaps between columns, for drag handles / insert buttons.
    pub fn boundaries(&self, wa: Rect) -> Vec<Boundary> {
        let gap = theme::GAP;
        let canvas_w = self.canvas_w.unwrap_or(wa.w);
        self.tree
            .h_boundaries(wa.x, wa.y, canvas_w, wa.h, gap, theme::tb_h(gap))
    }

    /// Set the split ratio at a boundary so the left child occupies fraction
    /// `frac` of the two neighbours' combined width (their sum is preserved).
    pub fn resize_boundary(&mut self, parent: NodeId, idx: usize, frac: f64) {
        if let Some(Node::Branch { ratios, .. }) = self.tree.get_mut(parent) {
            if idx + 1 < ratios.len() {
                let combined = ratios[idx] + ratios[idx + 1];
                let f = frac.clamp(0.05, 0.95);
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
}
