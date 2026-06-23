//! Per-tag layout state plus every mutation of the split tree / tab stacks.
//! Combines splitwm core.lua + ops.lua + scroll bookkeeping for one tag.

use crate::theme;
use crate::tree::{Dir, Leaf, Node, NodeId, Rect, Tree, Win};

pub struct State {
    pub tree: Tree,
    pub focused_leaf: NodeId,
    pub scroll_x: i32,
    pub scroll_target: i32,
    /// Canvas width; None falls back to the workarea width.
    pub canvas_w: Option<i32>,
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
        }
    }

    pub fn focused_leaf_valid(&self) -> NodeId {
        if self.tree.is_leaf(self.focused_leaf) {
            self.focused_leaf
        } else {
            self.tree.first_leaf(self.tree.root)
        }
    }

    // --- tab helpers ---

    pub fn leaf_of_client(&self, c: Win) -> Option<NodeId> {
        self.tree.find_leaf_for_client(c)
    }

    /// Pin a client into the focused leaf just after the active tab.
    pub fn pin_client(&mut self, c: Win) {
        let lid = self.focused_leaf_valid();
        if self.tree.find_leaf_for_client(c).is_some() {
            return;
        }
        if let Some(l) = self.tree.leaf_mut(lid) {
            let pos = if l.tabs.is_empty() {
                0
            } else {
                (l.active + 1).min(l.tabs.len())
            };
            l.tabs.insert(pos, c);
            l.active = pos;
        }
    }

    pub fn unpin_client(&mut self, c: Win) {
        for lid in self.tree.collect_leaves() {
            if let Some(l) = self.tree.leaf_mut(lid) {
                remove_from_leaf(l, c);
            }
        }
    }

    /// Set the active tab / focused leaf to whatever holds `c`.
    pub fn activate_client(&mut self, c: Win) -> bool {
        if let Some(lid) = self.tree.find_leaf_for_client(c) {
            if let Some(l) = self.tree.leaf_mut(lid) {
                if let Some(i) = l.tabs.iter().position(|&t| t == c) {
                    l.active = i;
                    self.focused_leaf = lid;
                    return true;
                }
            }
        }
        false
    }

    /// Currently active client of the focused leaf.
    pub fn focused_client(&self) -> Option<Win> {
        let l = self.tree.leaf(self.focused_leaf_valid())?;
        l.tabs.get(l.active).copied()
    }

    pub fn cycle_tab(&mut self, offset: i32) -> Option<Win> {
        let lid = self.focused_leaf_valid();
        let l = self.tree.leaf_mut(lid)?;
        if l.tabs.is_empty() {
            return None;
        }
        let n = i32::try_from(l.tabs.len()).unwrap_or(i32::MAX);
        let pos = (i32::try_from(l.active).unwrap_or(0) + offset).rem_euclid(n);
        l.active = usize::try_from(pos).unwrap_or(0);
        l.tabs.get(l.active).copied()
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

    /// Move the focused tab to the adjacent split. Returns the moved client.
    pub fn move_tab_to_direction(&mut self, next: bool) -> Option<Win> {
        let src = self.focused_leaf_valid();
        let dst = self.adjacent_leaf(src, next)?;
        let c = {
            let l = self.tree.leaf(src)?;
            *l.tabs.get(l.active)?
        };
        self.unpin_client(c);
        if let Some(d) = self.tree.leaf_mut(dst) {
            d.tabs.push(c);
            d.active = d.tabs.len() - 1;
        }
        self.focused_leaf = dst;
        Some(c)
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

    /// Split the focused leaf; existing tabs stay in `child_a` (now focused).
    pub fn split_focused(&mut self, dir: Dir) {
        let leaf = self.focused_leaf_valid();
        // Move tabs into a fresh leaf node (child_a), leave child_b empty.
        let (tabs, active, minimized) = {
            let l = self.tree.leaf(leaf).unwrap();
            (l.tabs.clone(), l.active, l.minimized)
        };
        let child_a = self.tree.insert_node(Node::Leaf(Leaf {
            tabs,
            active,
            minimized,
        }));
        let child_b = self.tree.make_leaf();
        // The original `leaf` node id is reused as a branch via split_node:
        // we replace references to it. Easiest: turn `leaf` into child_a's
        // role by giving child_a the tabs and inserting a branch in place.
        self.split_node(leaf, dir, child_a, child_b);
        // The old leaf node is now detached; drop it.
        self.tree.remove_node(leaf);
        self.focused_leaf = child_a;
    }

    /// Close the focused leaf, merging its tabs into the adjacent sibling.
    pub fn close_focused(&mut self) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false; // root leaf: nothing to close
        };

        // Merge this leaf's tabs into the adjacent sibling's first leaf.
        let tabs: Vec<Win> = self
            .tree
            .leaf(leaf)
            .map(|l| l.tabs.clone())
            .unwrap_or_default();
        let dest_child = {
            let (children,) = match self.tree.get(parent) {
                Some(Node::Branch { children, .. }) => (children.clone(),),
                _ => return false,
            };
            let dest_idx = if idx > 0 { idx - 1 } else { 1 };
            self.tree.first_leaf(children[dest_idx])
        };
        if let Some(d) = self.tree.leaf_mut(dest_child) {
            let was_empty = d.tabs.is_empty();
            d.tabs.extend(tabs);
            if was_empty && !d.tabs.is_empty() {
                d.active = 0;
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

fn remove_from_leaf(l: &mut Leaf, c: Win) {
    if let Some(i) = l.tabs.iter().position(|&t| t == c) {
        l.tabs.remove(i);
        if l.tabs.is_empty() {
            l.active = 0;
        } else if i < l.active || l.active >= l.tabs.len() {
            l.active = l.active.saturating_sub(1).min(l.tabs.len() - 1);
        }
    }
}
