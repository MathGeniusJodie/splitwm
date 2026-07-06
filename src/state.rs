//! Layout state plus every mutation of the split tree / tab stacks and the
//! scroll bookkeeping — there is exactly one layout (no workspaces/tags).

use crate::theme;
use crate::tree::{Boundary, Child, Dir, Node, NodeId, Rect, Tree, Win};

/// Where a "+" insert button adds a new root-level column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InsertAt {
    /// Before the root child at this index.
    Index(usize),
    /// After the last root child (the right-edge button).
    End,
}

/// Outcome of `activate_client`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activation {
    /// `c` isn't tracked in any leaf (sitting in the stash);
    /// the caller must place it itself.
    NotFound,
    /// `c` occupied a minimized leaf that is now shown — rects changed.
    Unminimized,
    /// `c` already occupied a shown leaf — nothing a redraw would show moved.
    Unchanged,
}

pub struct State {
    pub tree: Tree,
    /// Private so every write outside `#[cfg(test)]` goes through
    /// `focus_leaf` (which accepts only live leaf ids) and every read
    /// through `focused_leaf_valid` (the focused leaf can still be
    /// *removed* by a later mutation, so reads re-validate) — a dangling
    /// focus is never handed out. Tests assign the field directly to set up
    /// states.
    focused_leaf: NodeId,
    /// Current and target scroll offsets. Private so every mutation goes
    /// through the clamping/landing methods below — `update_canvas`
    /// re-clamps both whenever the scroll range changes.
    scroll_x: i32,
    scroll_target: i32,
    /// Canvas width, derived by `update_canvas` (0 until the first call;
    /// read through `canvas_w()`, which falls back to the viewport width).
    canvas_w: i32,
    /// Extra scrollable width past `canvas_w` reserved for the docked
    /// sidebar (see `Wm::manage_dock`), so scrolling all the way right
    /// reveals it even though it sits outside the split tree and doesn't
    /// affect `compute`'s leaf geometry. Zero when nothing is docked.
    /// Private so the only write is `update_canvas`, which re-clamps the
    /// scroll offsets against the widened range in the same breath.
    dock_extra: i32,
    /// Cumulative manual adjustment to `canvas_w` from dragging an
    /// edge-of-canvas resize handle, layered on top of `update_canvas`'s
    /// column-count-driven heuristic every recompute so a manual resize
    /// isn't immediately overwritten by it. Private so the only write is
    /// `resize_edge`, whose per-column `min_split_w` clamp keeps it sane.
    canvas_w_extra: i32,
    /// Windows not shown in any split; they appear un-highlighted in the
    /// bottom taskbar and are what Mod4+[ / Mod4+] cycle through. Private so
    /// every insert goes through `push_stash`, which owns the
    /// no-duplicates invariant; read through `stash()`.
    stash: Vec<Win>,
}

/// How a collapsing branch hangs in the tree: it is the root, or child
/// `usize` of a grandparent branch. Resolved by `close_focused` *before*
/// relocating any window, so an orphan subtree (a parent that is neither)
/// is refused up front — there is deliberately no variant for one.
enum Attach {
    Root,
    Child(NodeId, usize),
}

impl State {
    /// `parent`'s attachment to the rest of the tree, or `None` if `parent`
    /// is reachable from neither the root nor any branch (an orphan
    /// subtree). `Attach` deliberately has no variant for that case.
    fn attach_of(&self, parent: NodeId) -> Option<Attach> {
        if parent == self.tree.root {
            Some(Attach::Root)
        } else {
            self.tree
                .find_parent(parent)
                .map(|(grand, pidx)| Attach::Child(grand, pidx))
        }
    }

    pub fn new() -> Self {
        let tree = Tree::new();
        let root = tree.root;
        Self {
            tree,
            focused_leaf: root,
            scroll_x: 0,
            scroll_target: 0,
            canvas_w: 0,
            dock_extra: 0,
            canvas_w_extra: 0,
            stash: Vec::new(),
        }
    }

    pub fn focused_leaf_valid(&self) -> NodeId {
        if self.tree.is_leaf(self.focused_leaf) {
            self.focused_leaf
        } else {
            self.tree.first_leaf(self.tree.root)
        }
    }

    /// Point focus at `leaf`. Anything that isn't a live leaf is ignored:
    /// callers can hold ids captured before an intervening mutation, and
    /// focus must never come to rest on a node `compute` doesn't lay out.
    pub fn focus_leaf(&mut self, leaf: NodeId) {
        if self.tree.is_leaf(leaf) {
            self.focused_leaf = leaf;
        }
    }

    // --- window placement helpers ---

    /// Windows not currently shown in any split, in stash (queue) order.
    pub fn stash(&self) -> &[Win] {
        &self.stash
    }

    /// Drop a client into the stash (deduplicated).
    fn push_stash(&mut self, c: Win) {
        if !self.stash.contains(&c) {
            self.stash.push(c);
        }
    }

    /// Pull `w` out of the stash; whether it was there.
    fn take_from_stash(&mut self, w: Win) -> bool {
        let len = self.stash.len();
        self.stash.retain(|&x| x != w);
        self.stash.len() < len
    }

    /// Detach `c` from wherever it lives (a split or the stash).
    fn detach(&mut self, c: Win) {
        if let Some(lid) = self.tree.find_leaf_for_client(c) {
            if let Some(l) = self.tree.leaf_mut(lid) {
                l.client = None;
            }
        }
        self.take_from_stash(c);
    }

    /// Place a new client into the focused leaf, bumping any current occupant
    /// down to the stash.
    pub fn pin_client(&mut self, c: Win) {
        if self.tree.find_leaf_for_client(c).is_some() || self.stash.contains(&c) {
            return;
        }
        self.assign_to_leaf(c, self.focused_leaf_valid());
    }

    /// Put `c` into leaf `dst`, displacing the existing occupant to the stash.
    /// `c` is first detached from its previous home. The destination is
    /// un-minimized: showing a window in a leaf means the user wants to see
    /// it, and a minimized leaf's window is never mapped.
    pub fn assign_to_leaf(&mut self, c: Win, dst: NodeId) {
        if !self.tree.is_leaf(dst) {
            return;
        }
        self.detach(c);
        // `detach` just cleared `c` from wherever it lived (including `dst`),
        // so whatever still occupies `dst` cannot be `c` itself.
        let displaced = self.tree.leaf(dst).and_then(|l| l.client);
        debug_assert_ne!(displaced, Some(c), "detach left {c:#x} in dst");
        if let Some(prev) = displaced {
            self.push_stash(prev);
        }
        if let Some(l) = self.tree.leaf_mut(dst) {
            l.show(c);
            if displaced.is_some() {
                l.prev = displaced;
            }
        }
        self.focus_leaf(dst);
    }

    /// Remove a client entirely (window gone): clear it from its split/stash.
    /// If the leaf it occupied remembers a displaced window (`Leaf::prev`)
    /// that's still in the stash, that window is put back into the split —
    /// closing a focus-stealing popup restores what it displaced.
    pub fn unpin_client(&mut self, c: Win) {
        let lid = self.tree.find_leaf_for_client(c);
        self.detach(c);
        if let Some(lid) = lid {
            let prev = self.tree.leaf(lid).and_then(|l| l.prev);
            if let Some(p) = prev {
                if self.take_from_stash(p) {
                    if let Some(l) = self.tree.leaf_mut(lid) {
                        l.show(p);
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

    /// Focus whatever split currently shows `c`, un-minimizing it —
    /// activation means the user (or a pager) wants the window visible, and
    /// a minimized leaf's window is never mapped, so focusing one without
    /// restoring it would target an unviewable window. Reports whether that
    /// changed anything a redraw would show, so callers can skip animating a
    /// transition that moves no rects (a plain refocus of an already-visible
    /// window).
    pub fn activate_client(&mut self, c: Win) -> Activation {
        let Some(lid) = self.tree.find_leaf_for_client(c) else {
            return Activation::NotFound;
        };
        let was_minimized = self.tree.leaf(lid).is_some_and(|l| l.minimized);
        if let Some(l) = self.tree.leaf_mut(lid) {
            l.minimized = false;
        }
        self.focus_leaf(lid);
        if was_minimized {
            Activation::Unminimized
        } else {
            Activation::Unchanged
        }
    }

    /// Currently *shown* client of the focused leaf. A minimized leaf shows
    /// nothing — its window is unmapped, and handing it out as a focus
    /// target would mean `SetInputFocus` on an unviewable window (a
    /// BadMatch) and a `_NET_ACTIVE_WINDOW` naming an invisible one.
    pub fn focused_client(&self) -> Option<Win> {
        let l = self.tree.leaf(self.focused_leaf_valid())?;
        if l.minimized {
            return None;
        }
        l.client
    }

    /// Swap the focused split's window with the next/prev stash entry,
    /// cycling which off-screen window is shown.
    pub fn cycle_stash(&mut self, forward: bool) -> Option<Win> {
        if self.stash.is_empty() {
            return None;
        }
        let lid = self.focused_leaf_valid();
        let displaced = self.tree.leaf(lid).and_then(|l| l.client);
        let next = if forward {
            self.stash.remove(0)
        } else {
            self.stash.pop()?
        };
        self.assign_to_leaf(next, lid);
        // `assign_to_leaf` pushes the displaced occupant to the *back* —
        // exactly where backward cycling pops from, which would make prev
        // flip-flop between two windows instead of walking the list in
        // reverse. Move it to the front so forward and backward are true
        // inverse rotations of the same queue.
        if !forward {
            if let Some(d) = displaced {
                self.take_from_stash(d);
                self.stash.insert(0, d);
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
            self.focus_leaf(l);
            true
        } else {
            false
        }
    }

    /// Move the focused window to the adjacent split (displacing its occupant
    /// to the stash). Returns the moved client.
    pub fn move_window_to_direction(&mut self, next: bool) -> Option<Win> {
        let src = self.focused_leaf_valid();
        let dst = self.adjacent_leaf(src, next)?;
        let c = self.tree.leaf(src)?.client?;
        self.assign_to_leaf(c, dst);
        Some(c)
    }

    /// Toggle a leaf's minimized flag (the layout collapses it to min size).
    /// Refused for the root leaf: it has no siblings to yield space to, and
    /// its whole-frame restore button is disabled (`parent_dir.is_none()`),
    /// so a minimized root would be a full-screen strip with no way back.
    /// Returns whether the flag changed.
    pub fn toggle_minimize(&mut self, leaf: NodeId) -> bool {
        if leaf == self.tree.root {
            return false;
        }
        match self.tree.leaf_mut(leaf) {
            Some(l) => {
                l.minimized = !l.minimized;
                true
            }
            None => false,
        }
    }

    // --- splitting ---

    fn split_node(&mut self, leaf: NodeId, dir: Dir, child_a: NodeId, child_b: NodeId) {
        if let Some((parent, idx)) = self.tree.find_parent(leaf) {
            let same_dir = self.tree.branch(parent).is_some_and(|b| b.dir == dir);
            if same_dir {
                if let Some(b) = self.tree.branch_mut(parent) {
                    let old_r = b.children()[idx].ratio;
                    b.children_mut()[idx] = Child {
                        node: child_a,
                        ratio: old_r * theme::SPLIT_RATIO,
                    };
                    b.insert(
                        idx + 1,
                        Child {
                            node: child_b,
                            ratio: old_r * (1.0 - theme::SPLIT_RATIO),
                        },
                    );
                }
                return;
            }
            let branch = self
                .tree
                .make_branch(dir, theme::SPLIT_RATIO, child_a, child_b);
            if let Some(b) = self.tree.branch_mut(parent) {
                b.children_mut()[idx].node = branch;
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
    /// focused) and `child_b` starts empty. Refused for a minimized leaf: a
    /// minimized `child_a` cloned from it would be a split state the rest of
    /// the system (titlebar Split button, keyboard split gate) treats as
    /// impossible. Returns whether the split happened, so callers that queue
    /// an animation for the action can cancel it on refusal.
    pub fn split_focused(&mut self, dir: Dir) -> bool {
        let leaf = self.focused_leaf_valid();
        // `focused_leaf_valid` only hands out live leaves, so this always
        // resolves; a dangling id makes the split a no-op rather than a panic.
        let Some(leaf_data) = self.tree.leaf(leaf).cloned() else {
            return false;
        };
        if leaf_data.minimized {
            return false;
        }
        // child_a keeps the original split's window *and* its accent colour, so
        // colour stays with the content; child_b gets a fresh colour.
        let child_a = self.tree.insert_node(Node::Leaf(leaf_data));
        let child_b = self.tree.make_leaf();
        // Insert a branch in place of `leaf`, with child_a carrying the window.
        self.split_node(leaf, dir, child_a, child_b);
        // `leaf` is now detached from the tree; drop it.
        self.tree.remove_node(leaf);
        self.focus_leaf(child_a);
        true
    }

    /// Relocate `leaf`'s window: into the adjacent sibling's first leaf if it
    /// is empty, otherwise onto the stash. `idx` is `leaf`'s index among
    /// `parent`'s children.
    fn relocate_closed_window(&mut self, parent: NodeId, idx: usize, leaf: NodeId) -> bool {
        // Relocate this leaf's window: into the adjacent sibling's first leaf
        // if it is empty, otherwise onto the stash.
        let client = self.tree.leaf(leaf).and_then(|l| l.client);
        let dest_child = {
            let Some(b) = self.tree.branch(parent) else {
                return false;
            };
            // A branch always has a sibling to fall back to (`Branch` holds
            // at least two children by construction).
            let dest_idx = if idx > 0 { idx - 1 } else { 1 };
            self.tree.first_leaf(b.children()[dest_idx].node)
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
                    d.show(c);
                    if d.prev.is_none() {
                        d.prev = prev;
                    }
                }
            } else {
                self.push_stash(c);
            }
        }
        true
    }

    /// n-ary close path: remove `leaf` (at index `idx` of `parent`'s
    /// children), redistributing its ratio among the survivors, and refocus
    /// the nearest surviving neighbour.
    fn close_focused_nary(&mut self, parent: NodeId, idx: usize, leaf: NodeId) {
        // n-ary: remove this child, redistribute its ratio. Resolving the
        // branch up front keeps a bad `parent` a no-op — focus must never
        // land on the removed leaf.
        let Some(b) = self.tree.branch_mut(parent) else {
            return;
        };
        let removed = b.remove(idx).ratio;
        let remaining: f64 = b.children().iter().map(|c| c.ratio).sum();
        if remaining > 0.0 {
            for c in b.children_mut() {
                c.ratio += removed * c.ratio / remaining;
            }
        }
        let fb = idx.min(b.children().len() - 1);
        let new_focus = b.children()[fb].node;
        self.tree.remove_node(leaf);
        let new_focus = self.tree.first_leaf(new_focus);
        self.focus_leaf(new_focus);
    }

    /// Binary close path: collapse `parent`, the surviving sibling (at index
    /// `idx`'s opposite) takes its place, and same-direction branches are
    /// flattened back into the grandparent. `attach` is `parent`'s own
    /// attachment, resolved by `close_focused` before any relocation.
    fn close_focused_binary(&mut self, parent: NodeId, idx: usize, attach: Attach, leaf: NodeId) {
        // binary: collapse parent, sibling takes its place.
        let sibling = match self.tree.branch(parent) {
            Some(b) => b.children()[usize::from(idx == 0)].node,
            None => return,
        };
        self.tree.remove_node(leaf);
        // Resolve focus before any flattening: the sibling *node* may
        // be dissolved into the grandparent below, but its leaves
        // survive.
        let new_focus = self.tree.first_leaf(sibling);
        self.tree.remove_node(parent);
        match attach {
            Attach::Root => {
                self.tree.root = sibling;
                // The root leaf is never minimized: its whole-frame restore
                // button is disabled (a root has nothing to collapse
                // relative to), so a minimized leaf promoted to root would
                // be a full-screen restore strip with no way back and its
                // window unmapped. The promotion restores it instead.
                if let Some(l) = self.tree.leaf_mut(sibling) {
                    l.minimized = false;
                }
            }
            Attach::Child(grand, pidx) => {
                if let Some(b) = self.tree.branch_mut(grand) {
                    b.children_mut()[pidx].node = sibling;
                }
                // The spliced-in sibling can be a branch in the
                // grandparent's own direction; dissolve it so same-dir
                // splits stay one flat n-ary branch.
                self.tree.flatten_same_dir(grand, pidx);
            }
        }
        self.focus_leaf(new_focus);
    }

    /// Close the focused leaf. Its window moves into the adjacent sibling if
    /// that split is empty, otherwise down to the stash.
    pub fn close_focused(&mut self) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false; // root leaf: nothing to close
        };

        let Some(nchildren) = self.tree.branch(parent).map(|b| b.children().len()) else {
            return false;
        };

        // Focus always moves to the nearest surviving neighbour: the closed
        // leaf *was* the focused one (node ids are never reused, so it can't
        // still be found anywhere in the tree after removal).
        //
        // Both paths below refuse to close inside an orphan subtree (a
        // `parent` reachable from neither the root nor any branch) before
        // mutating anything — see the binary path's comment for why.
        if nchildren > 2 {
            if self.attach_of(parent).is_none() {
                return false;
            }
            if !self.relocate_closed_window(parent, idx, leaf) {
                return false;
            }
            self.close_focused_nary(parent, idx, leaf);
            return true;
        }

        // Binary: resolve `parent`'s attachment before relocating — a false
        // return means the close did not happen, so no window may have been
        // evicted from its leaf yet. A parent that is neither the root nor
        // anyone's child is an orphan subtree; collapsing inside it would
        // leave `focused_leaf` pointing at a leaf unreachable from the root
        // (still a leaf by `is_leaf`, so `focused_leaf_valid` would keep
        // returning it while `compute` never lays it out). Refuse instead —
        // `Attach` deliberately has no variant for it.
        let Some(attach) = self.attach_of(parent) else {
            return false;
        };

        if !self.relocate_closed_window(parent, idx, leaf) {
            return false;
        }
        self.close_focused_binary(parent, idx, attach, leaf);
        true
    }

    // --- resize ---

    pub fn resize_focused(&mut self, delta: f64) -> bool {
        let leaf = self.focused_leaf_valid();
        let Some((parent, idx)) = self.tree.find_parent(leaf) else {
            return false;
        };
        if let Some(b) = self.tree.branch_mut(parent) {
            // `Branch` holds at least two children by construction, so a
            // sibling to trade width with always exists.
            let n = b.children().len();
            let other = if idx + 1 < n { idx + 1 } else { idx - 1 };
            let min_r = theme::MIN_SPLIT_FRAC;
            let cs = b.children_mut();
            let cur = cs[idx].ratio;
            let cur_other = cs[other].ratio;
            // Cap the transfer at what each side can actually give, so the
            // pair's sum is exactly conserved — clamping both ends
            // independently would let the total ratio mass drift upward once
            // the neighbour bottoms out, silently shrinking every *other*
            // sibling via renormalisation.
            let (lo, hi) = ((min_r - cur).min(0.0), (cur_other - min_r).max(0.0));
            let delta = delta.clamp(lo, hi);
            if delta == 0.0 {
                return false;
            }
            cs[idx].ratio = cur + delta;
            cs[other].ratio = cur_other - delta;
            true
        } else {
            false
        }
    }

    // --- canvas ---

    /// The scrollable canvas width. Before the first `update_canvas` this
    /// falls back to the viewport width, so pure-`State` callers (tests,
    /// pre-first-arrange paths) see sane geometry.
    pub fn canvas_w(&self, wa: Rect) -> i32 {
        if self.canvas_w > 0 {
            self.canvas_w
        } else {
            wa.w
        }
    }

    /// Recompute the canvas width for the current tree and viewport; called
    /// once per arrange. Width demand is measured in *columns*
    /// (`Tree::h_units`), not leaves — a vertical stack of any depth still
    /// occupies one column, so it must not open up phantom scroll space.
    /// Each column gets a comfortable minimum so splits don't get crushed.
    /// A manual edge-of-canvas resize layers on via `canvas_w_extra` and may
    /// legitimately take the canvas narrower than the viewport (leaving
    /// margin on the far side); `resize_edge`'s per-column `min_split_w`
    /// clamp is what keeps that sane. `dock_extra` is the extra scroll room
    /// the docked sidebar needs (zero when nothing is docked).
    ///
    /// Scroll positions are deliberately *not* re-clamped here: an
    /// edge-of-canvas drag parks them outside `[0, max_scroll]` to hold a
    /// wallpaper margin at the dragged edge (see `shift_scroll`), and this
    /// runs on every arrange, so clamping here would yank that margin shut
    /// on the next hover repaint. Mutations that change the scroll range
    /// out from under the user (structural layout changes, viewport
    /// resizes, dock removal) call `clamp_scroll` explicitly instead.
    pub fn update_canvas(&mut self, wa: Rect, dock_extra: i32) {
        let gap = theme::GAP;
        let columns = self.tree.h_units().max(1);
        let min_col_w = (theme::min_split_w() + 2 * gap).max(wa.w / 3);
        let needed = columns.saturating_mul(min_col_w);
        self.canvas_w = needed.max(wa.w) + self.canvas_w_extra;
        self.dock_extra = dock_extra;
    }

    /// Pull both scroll positions back into `[0, max_scroll]`, recomputing
    /// the canvas first so the range reflects the tree as it is *now* (the
    /// caller typically just mutated it; clamping against the stale width
    /// would be a no-op). This is the companion to `update_canvas` not
    /// clamping: structural layout changes, viewport resizes and dock
    /// removal shrink the scroll range and must not strand the viewport
    /// past the content, while edge-drag margins (scroll out of range on
    /// purpose) survive everything that doesn't call this.
    pub fn clamp_scroll(&mut self, wa: Rect, dock_extra: i32) {
        self.update_canvas(wa, dock_extra);
        let max_scroll = self.max_scroll(wa);
        self.scroll_target = self.scroll_target.clamp(0, max_scroll);
        self.scroll_x = self.scroll_x.clamp(0, max_scroll);
    }

    // --- scroll ---

    pub fn scroll_x(&self) -> i32 {
        self.scroll_x
    }

    /// Land the scroll: snap the current offset to the target.
    pub fn land_scroll(&mut self) {
        self.scroll_x = self.scroll_target;
    }

    /// Shift both offsets by `delta` without clamping — used by the
    /// left-edge resize drag to keep on-screen columns stationary while the
    /// canvas width changes underneath (`Tree::compute` lays out from a
    /// fixed origin, so resizing column 0 moves every other column in
    /// canvas space). A left-edge shrink legitimately takes the scroll
    /// negative; see `max_scroll` for what out-of-range scroll means.
    pub fn shift_scroll(&mut self, delta: i32) {
        self.scroll_x += delta;
        self.scroll_target += delta;
    }

    /// Upper end of the *scrollable* range. The current scroll can sit
    /// outside `[0, max_scroll]`, and that state is meaningful: negative
    /// scroll is a wallpaper margin left of the canvas (left-edge shrink,
    /// via `shift_scroll`), scroll past `max_scroll` is margin right of it
    /// (a right-edge shrink narrows the canvas under an unmoved scroll).
    /// Such a margin holds until a scroll gesture (`scroll_to` clamps) or
    /// a range-shrinking mutation (`clamp_scroll`) repositions the
    /// viewport.
    pub fn max_scroll(&self, wa: Rect) -> i32 {
        (self.canvas_w(wa) + self.dock_extra - wa.w).max(0)
    }

    pub fn scroll_to(&mut self, wa: Rect, target: i32) {
        self.scroll_target = target.clamp(0, self.max_scroll(wa));
    }

    pub fn scroll_delta(&mut self, wa: Rect, delta: i32) {
        let t = self.scroll_target + delta;
        self.scroll_to(wa, t);
    }

    /// The viewport rect widened to the scrollable canvas — the area every
    /// layout query is answered against.
    fn canvas_rect(&self, wa: Rect) -> Rect {
        Rect {
            w: self.canvas_w(wa),
            ..wa
        }
    }

    /// Geometry of every leaf in canvas coordinates.
    pub fn compute(&self, wa: Rect) -> std::collections::HashMap<NodeId, Rect> {
        self.tree.compute(self.canvas_rect(wa), theme::GAP)
    }

    /// Gaps between adjacent splits, for drag handles / insert buttons.
    pub fn boundaries(&self, wa: Rect) -> Vec<Boundary> {
        self.tree.boundaries(self.canvas_rect(wa), theme::GAP)
    }

    /// Canvas-space x-span `(start_x, width)` of the leftmost/rightmost
    /// root-level column — used to seed and drive an edge-of-canvas resize
    /// drag (see `resize_edge`). A single leaf, or a root that's itself a
    /// vertical branch, count as one column spanning the whole row, so
    /// `left`/`right` both describe the same span in that case (see
    /// `Tree::root_h_sizes`). `None` only if the tree is somehow empty.
    pub fn edge_span(&self, wa: Rect, left: bool) -> Option<(i32, i32)> {
        let gap = theme::GAP;
        let canvas_w = self.canvas_w(wa);
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
        if let Some(b) = self.tree.branch_mut(root) {
            if b.dir == Dir::H {
                // Only normal children's ratios matter to the layout
                // (`child_sizes` normalises over them alone), so rewriting
                // just those reproduces the pixel widths exactly.
                for ((c, &w), &m) in b
                    .children_mut()
                    .iter_mut()
                    .zip(widths.iter())
                    .zip(minimized)
                {
                    if !m {
                        c.ratio = f64::from(w) / f64::from(total);
                    }
                }
            }
        }
        true
    }

    /// Resize the leftmost or rightmost root-level column to `target_w`
    /// pixels: the column absorbs the whole delta, every sibling keeps its
    /// exact current pixel width, and `canvas_w` grows/shrinks by that same
    /// delta (via `canvas_w_extra`, layered on top of `update_canvas`'s
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
    pub fn resize_edge(&mut self, wa: Rect, left: bool, target_w: i32) -> i32 {
        let root = self.tree.root;
        let gap = theme::GAP;
        let canvas_w = self.canvas_w(wa);
        let Some(widths) = self.tree.root_h_sizes(canvas_w - 2 * gap, gap) else {
            return 0;
        };
        let idx = if left { 0 } else { widths.len() - 1 };
        let old_w = widths[idx];
        let new_w = target_w.max(theme::min_split_w());
        let delta = new_w - old_w;
        if delta == 0 {
            return 0;
        }
        if widths.len() > 1 {
            // A multi-column root is necessarily an H-branch (`root_h_sizes`
            // returns a single full-width span for a lone leaf or V-branch
            // root), so per-column minimized flags index `widths` one-to-one.
            // A minimized column's pixel width is pinned to `gap` regardless
            // of ratio (see `child_sizes`), so its stored ratio must survive
            // the rewrite in `redistribute_column_widths` untouched —
            // deriving a ratio from the pinned width would crush the share
            // it restores to.
            let minimized: Vec<bool> = match self.tree.branch(root) {
                Some(b) if b.dir == Dir::H => b
                    .children()
                    .iter()
                    .map(|c| self.tree.leaf(c.node).is_some_and(|l| l.minimized))
                    .collect(),
                _ => return 0,
            };
            if minimized[idx] {
                // The end column itself being minimized makes the whole drag
                // meaningless (old_w is the pinned gap, not a real width).
                return 0;
            }
            if !self.redistribute_column_widths(root, idx, new_w, widths, &minimized) {
                return 0;
            }
        }
        self.canvas_w_extra += delta;
        delta
    }

    /// Set the split ratio at a boundary so the left child occupies fraction
    /// `frac` of the two neighbours' combined width (their sum is preserved).
    pub fn resize_boundary(&mut self, parent: NodeId, idx: usize, frac: f64) {
        if let Some(b) = self.tree.branch_mut(parent) {
            let cs = b.children_mut();
            if idx + 1 < cs.len() {
                let combined = cs[idx].ratio + cs[idx + 1].ratio;
                let f = frac.clamp(theme::MIN_SPLIT_FRAC, 1.0 - theme::MIN_SPLIT_FRAC);
                cs[idx].ratio = combined * f;
                cs[idx + 1].ratio = combined * (1.0 - f);
            }
        }
    }

    /// Insert a new empty leaf column at root-children index `at`, making the
    /// root an H-branch if it isn't one. The new leaf becomes focused.
    #[allow(clippy::cast_precision_loss)]
    pub fn insert_at_root(&mut self, at: InsertAt) -> NodeId {
        let new = self.tree.make_leaf();
        let root = self.tree.root;
        let is_h = self.tree.branch(root).is_some_and(|b| b.dir == Dir::H);
        if is_h {
            if let Some(b) = self.tree.branch_mut(root) {
                let n = b.children().len();
                let avg = b.children().iter().map(|c| c.ratio).sum::<f64>() / n as f64;
                let i = match at {
                    InsertAt::Index(i) => i.min(n),
                    InsertAt::End => n,
                };
                b.insert(
                    i,
                    Child {
                        node: new,
                        ratio: avg,
                    },
                );
                let s: f64 = b.children().iter().map(|c| c.ratio).sum();
                for c in b.children_mut() {
                    c.ratio /= s;
                }
            }
        } else {
            // The existing content keeps the larger `SPLIT_RATIO` share on
            // whichever side it lands; building the branch in its final
            // order (rather than swapping children afterwards) keeps the
            // ratios paired with the right children by construction.
            let branch = if at == InsertAt::Index(0) {
                self.tree
                    .make_branch(Dir::H, 1.0 - theme::SPLIT_RATIO, new, root)
            } else {
                self.tree.make_branch(Dir::H, theme::SPLIT_RATIO, root, new)
            };
            self.tree.root = branch;
        }
        self.focus_leaf(new);
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

    /// The root branch's per-child ratios; panics when the root is a leaf.
    fn root_ratios(s: &State) -> Vec<f64> {
        s.tree
            .branch(s.tree.root)
            .expect("root not a branch")
            .children()
            .iter()
            .map(|c| c.ratio)
            .collect()
    }

    #[test]
    fn insert_at_root_grows_columns() {
        let mut s = State::new();
        s.split_focused(Dir::H); // root H-branch, 2 columns
        assert_eq!(s.tree.collect_leaves().len(), 2);
        s.insert_at_root(InsertAt::Index(1)); // insert between
        assert_eq!(s.tree.collect_leaves().len(), 3);
        // The inserted leaf is focused and empty.
        assert!(s.focused_client().is_none());
        // Ratios renormalise to sum 1.
        let sum: f64 = root_ratios(&s).iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "ratios sum {sum}");
    }

    #[test]
    fn insert_at_root_wraps_single_leaf() {
        let mut s = State::new();
        s.insert_at_root(InsertAt::Index(0)); // root is a lone leaf -> wrap into H-branch
        assert_eq!(s.tree.collect_leaves().len(), 2);
        assert!(s.tree.branch(s.tree.root).is_some_and(|b| b.dir == Dir::H));
    }

    /// Wrapping a lone leaf must keep the *existing* content on the larger
    /// `SPLIT_RATIO` share regardless of which side the new column lands:
    /// swapping the children without also swapping the ratios would hand
    /// the empty column the bigger share on a left-edge insert.
    #[test]
    fn insert_at_root_keeps_existing_content_share_on_both_sides() {
        for (at, existing_idx) in [(InsertAt::Index(0), 1usize), (InsertAt::End, 0usize)] {
            let mut s = State::new();
            let existing = s.tree.first_leaf(s.tree.root);
            s.insert_at_root(at);
            let children = s
                .tree
                .branch(s.tree.root)
                .expect("root not a branch")
                .children()
                .to_vec();
            assert_eq!(children[existing_idx].node, existing, "at={at:?}");
            assert!(
                (children[existing_idx].ratio - theme::SPLIT_RATIO).abs() < 1e-9,
                "at={at:?}: existing content got ratio {}",
                children[existing_idx].ratio
            );
        }
    }

    #[test]
    fn resize_boundary_preserves_neighbour_sum() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        let root = s.tree.root;
        let before: f64 = root_ratios(&s).iter().sum();
        s.resize_boundary(root, 0, 0.25);
        let ratios = root_ratios(&s);
        assert!((ratios[0] + ratios[1] - before).abs() < 1e-9);
        assert!((ratios[0] / (ratios[0] + ratios[1]) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn boundaries_match_column_count() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.update_canvas(WA, 0);
        // One gap between two columns.
        assert_eq!(s.boundaries(WA).len(), 1);
        s.insert_at_root(InsertAt::Index(1));
        s.update_canvas(WA, 0);
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
    /// `DOCK_OVERLAP` clamped to the dock's width, see `Dock::overlap`),
    /// even when there's only one column and the canvas alone has no scroll
    /// room of its own.
    #[test]
    fn dock_extra_reveals_sidebar_at_max_scroll() {
        let mut s = State::new();
        let docked_w = 300;
        let overlap = theme::DOCK_OVERLAP.min(docked_w);
        // Single leaf: canvas == viewport, no scroll room of its own.
        s.update_canvas(WA, docked_w - overlap);

        assert_eq!(s.max_scroll(WA), docked_w - overlap);

        let canvas_w = s.canvas_w(WA);
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
        let canvas_w = s.canvas_w(WA);
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
        s.update_canvas(WA, 0);
        let (_, w_before) = s.edge_span(WA, false).unwrap();

        let shrink_by = 50;
        let applied = s.resize_edge(WA, false, w_before - shrink_by);
        assert_eq!(applied, -shrink_by);
        assert_eq!(s.canvas_w_extra, -shrink_by);

        s.update_canvas(WA, 0);
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
        s.update_canvas(WA, 0);
        let canvas_w_before = s.canvas_w(WA);

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

        // `resize_edge` records the delta in `canvas_w_extra`; the next
        // `update_canvas` (run once per arrange) layers it onto the width.
        s.update_canvas(WA, 0);
        assert_eq!(s.canvas_w(WA), canvas_w_before + grow_by);
        let after = s.compute(WA);
        assert_eq!(after[&left_id].w, left_w_before + grow_by);
        assert_eq!(after[&right_id].w, right_w_before);
    }

    /// Backward stash cycling must walk the whole list in reverse, not
    /// flip-flop between the shown window and the last stash entry:
    /// `assign_to_leaf` pushes the displaced window to the back, which is
    /// exactly where the next backward pop looks, so the displaced window
    /// must be moved to the front.
    #[test]
    fn cycle_stash_prev_visits_every_window() {
        let mut s = State::new();
        for w in [10, 1, 2, 3] {
            s.pin_client(w);
        }
        // Leaf shows 3; stash is [10, 1, 2].
        assert_eq!(s.focused_client(), Some(3));

        let shown: Vec<_> = (0..4).map(|_| s.cycle_stash(false).unwrap()).collect();
        assert_eq!(shown, vec![2, 1, 10, 3], "prev must rotate, not toggle");
    }

    /// One step forward then one step back must restore both the shown
    /// window and the stash order.
    #[test]
    fn cycle_stash_prev_inverts_next() {
        let mut s = State::new();
        for w in [10, 1, 2, 3] {
            s.pin_client(w);
        }
        let before = s.stash.clone();
        s.cycle_stash(true);
        s.cycle_stash(false);
        assert_eq!(s.focused_client(), Some(3));
        assert_eq!(s.stash, before);
    }

    /// A popup that displaces the working window and is then destroyed must
    /// give the split back to the displaced window (pulled from the stash).
    #[test]
    fn closing_popup_restores_displaced_window() {
        let mut s = State::new();
        s.pin_client(1); // working window
        s.pin_client(99); // popup steals the split; 1 -> stash
        assert_eq!(s.focused_client(), Some(99));
        assert!(s.stash.contains(&1));

        s.unpin_client(99); // popup window destroyed
        assert_eq!(s.focused_client(), Some(1), "displaced window comes back");
        assert!(!s.stash.contains(&1));
    }

    /// If the displaced window has since left the stash (shown elsewhere or
    /// itself closed), the split just stays empty — no stale restore.
    #[test]
    fn no_restore_when_displaced_window_is_gone() {
        let mut s = State::new();
        s.pin_client(1);
        s.pin_client(99);
        s.unpin_client(1); // the remembered window itself is destroyed
        s.unpin_client(99);
        assert_eq!(s.focused_client(), None);
        assert!(s.stash.is_empty());
    }

    /// Restoration is single-shot: after a restore the leaf holds no further
    /// history, so closing the restored window doesn't resurrect anything.
    #[test]
    fn restore_is_single_shot() {
        let mut s = State::new();
        s.pin_client(1);
        s.pin_client(2); // prev = 1
        s.pin_client(3); // prev = 2, stash [1, 2]
        s.unpin_client(3); // restores 2
        assert_eq!(s.focused_client(), Some(2));
        s.unpin_client(2); // prev was consumed; 1 stays in the stash
        assert_eq!(s.focused_client(), None);
        assert_eq!(s.stash, vec![1]);
    }

    /// Activating a client whose leaf is minimized must restore the leaf:
    /// its window is unmapped while minimized, so focusing without
    /// restoring would target an unviewable window (`SetInputFocus` on one
    /// is a BadMatch). Assignment into a minimized leaf restores it too.
    #[test]
    fn activation_unminimizes_the_leaf() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H);
        let leaf = s.tree.find_leaf_for_client(1).unwrap();
        s.toggle_minimize(leaf);
        assert_eq!(s.activate_client(1), Activation::Unminimized);
        assert!(!s.tree.leaf(leaf).unwrap().minimized);

        s.toggle_minimize(leaf);
        s.assign_to_leaf(2, leaf);
        assert!(!s.tree.leaf(leaf).unwrap().minimized);
    }

    /// `activate_client`'s report distinguishes a real layout change (a
    /// minimized leaf reappearing) from a no-op refocus of a window that's
    /// already shown, and from a window not tracked in the tree at all —
    /// callers use this to skip animating a transition that moves no rects.
    #[test]
    fn activation_reports_whether_anything_changed() {
        let mut s = State::new();
        s.pin_client(1);
        assert_eq!(s.activate_client(1), Activation::Unchanged);
        assert_eq!(s.activate_client(99), Activation::NotFound);
    }

    /// Restoring the displaced window into a leaf that was minimized in the
    /// meantime must clear the minimized flag: a leaf showing a window is
    /// never minimized (its window would otherwise stay unmapped forever).
    #[test]
    fn popup_restore_unminimizes_the_leaf() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H);
        s.pin_client(99); // displaces 1 -> stash, prev = 1
        let leaf = s.tree.find_leaf_for_client(99).unwrap();
        s.toggle_minimize(leaf);
        s.unpin_client(99); // popup dies while its leaf is minimized
        assert_eq!(s.tree.leaf(leaf).unwrap().client, Some(1));
        assert!(!s.tree.leaf(leaf).unwrap().minimized);
    }

    /// Closing a window whose empty adjacent sibling is a minimized leaf
    /// relocates the window into it *and* restores it — the same "a leaf
    /// showing a window is never minimized" invariant as assignment and
    /// activation; the relocated window would otherwise be unmapped and in
    /// no stash, visible nowhere.
    #[test]
    fn close_into_minimized_sibling_unminimizes() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H);
        let win_leaf = s.tree.find_leaf_for_client(1).unwrap();
        let sibling = s
            .tree
            .collect_leaves()
            .into_iter()
            .find(|&l| l != win_leaf)
            .unwrap();
        s.toggle_minimize(sibling);
        s.focused_leaf = win_leaf;
        assert!(s.close_focused());
        let dst = s.tree.find_leaf_for_client(1).unwrap();
        assert!(!s.tree.leaf(dst).unwrap().minimized);
        assert_eq!(s.focused_client(), Some(1));
        assert!(s.stash.is_empty());
    }

    /// Closing when the adjacent sibling already shows a window pushes the
    /// closed leaf's window to the stash instead of displacing the
    /// sibling's occupant.
    #[test]
    fn close_with_occupied_sibling_pushes_to_stash() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H);
        let a = s.tree.find_leaf_for_client(1).unwrap();
        let b = s
            .tree
            .collect_leaves()
            .into_iter()
            .find(|&l| l != a)
            .unwrap();
        s.focused_leaf = b;
        s.pin_client(2);
        s.focused_leaf = a;
        assert!(s.close_focused());
        assert_eq!(s.stash, vec![1]);
        assert_eq!(s.focused_client(), Some(2));
    }

    /// Closing inside an orphan subtree (a branch reachable from neither the
    /// root nor any other branch) must be refused: collapsing it would leave
    /// `focused_leaf` pointing at a leaf `compute` never lays out.
    #[test]
    fn close_focused_refuses_orphan_subtree() {
        let mut s = State::new();
        let a = s.tree.make_leaf();
        let b = s.tree.make_leaf();
        let orphan = s.tree.make_branch(Dir::H, 0.5, a, b);
        assert_ne!(orphan, s.tree.root);
        s.focused_leaf = a;
        assert!(!s.close_focused());
        // Both orphan leaves survive; focus resolution stays inside leaves.
        assert!(s.tree.is_leaf(a) && s.tree.is_leaf(b));
    }

    /// Same refusal as `close_focused_refuses_orphan_subtree`, but for the
    /// n-ary path (`nchildren > 2`), which must enforce the same
    /// reachability check as the binary path even though it never collapses
    /// `parent` itself.
    #[test]
    fn close_focused_refuses_orphan_nary_subtree() {
        let mut s = State::new();
        let a = s.tree.make_leaf();
        let b = s.tree.make_leaf();
        let c = s.tree.make_leaf();
        let orphan = s.tree.make_branch(Dir::H, 0.5, a, b);
        assert_ne!(orphan, s.tree.root);
        if let Some(branch) = s.tree.branch_mut(orphan) {
            branch.insert(
                2,
                Child {
                    node: c,
                    ratio: 0.25,
                },
            );
        }
        s.focused_leaf = a;
        assert!(!s.close_focused());
        // All three orphan leaves survive; focus resolution stays inside leaves.
        assert!(s.tree.is_leaf(a) && s.tree.is_leaf(b) && s.tree.is_leaf(c));
    }

    /// A minimized sibling promoted to root by a binary collapse must be
    /// restored: the root leaf is never minimized — its whole-frame restore
    /// button is disabled, so the promotion would otherwise leave a
    /// full-screen restore strip with no way back and its window unmapped.
    #[test]
    fn collapse_unminimizes_a_leaf_promoted_to_root() {
        let mut s = State::new();
        s.pin_client(1);
        s.split_focused(Dir::H); // H[A(win 1), B]
        let leaves = s.tree.collect_leaves();
        let (a, b) = (leaves[0], leaves[1]);
        s.assign_to_leaf(1, b); // A empty, B holds the window
        s.toggle_minimize(b);
        s.focus_leaf(a);
        assert!(s.close_focused()); // nothing to relocate out of A
        assert_eq!(s.tree.root, b, "sibling takes the root");
        assert!(
            !s.tree.leaf(b).unwrap().minimized,
            "promotion must restore the leaf"
        );
        assert_eq!(s.focused_client(), Some(1));
    }

    /// The root leaf can't be minimized: nothing exists to restore it.
    #[test]
    fn toggle_minimize_refuses_the_root_leaf() {
        let mut s = State::new();
        let root = s.tree.root;
        assert!(!s.toggle_minimize(root));
        assert!(!s.tree.leaf(root).unwrap().minimized);
    }

    /// A minimized leaf must never be split: `child_a` would inherit
    /// `minimized: true`, a split-then-minimized state the titlebar Split
    /// button and keyboard split gate both treat as impossible to produce.
    /// `State::split_focused` is the single place that refuses this, so the
    /// tree is unchanged and nothing ends up both minimized and freshly split.
    #[test]
    fn split_focused_refuses_a_minimized_leaf() {
        let mut s = State::new();
        s.split_focused(Dir::H); // two leaves, one non-root and minimizable
        let leaves = s.tree.collect_leaves();
        let target = leaves[1];
        s.toggle_minimize(target);
        s.focused_leaf = target;
        let leaf_count_before = s.tree.collect_leaves().len();
        assert!(!s.split_focused(Dir::V));
        let leaves_after = s.tree.collect_leaves();
        assert_eq!(leaves_after.len(), leaf_count_before);
        assert!(s.tree.leaf(target).unwrap().minimized);
    }

    /// Repeated growing must stop once the neighbour bottoms out at
    /// `MIN_SPLIT_FRAC`, conserving the pair's ratio sum exactly — clamping
    /// both sides independently would let total ratio mass drift upward,
    /// silently shrinking every other sibling via renormalisation.
    #[test]
    fn resize_focused_conserves_ratio_sum() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.insert_at_root(InsertAt::Index(2)); // three columns
        s.focused_leaf = s.tree.collect_leaves()[0];
        let sum_before: f64 = root_ratios(&s).iter().sum();
        for _ in 0..100 {
            s.resize_focused(theme::RESIZE_STEP);
        }
        let ratios = root_ratios(&s);
        let sum_after: f64 = ratios.iter().sum();
        assert!((sum_after - sum_before).abs() < 1e-9, "sum drifted");
        assert!(ratios.iter().all(|&r| r >= theme::MIN_SPLIT_FRAC - 1e-9));
        // Once the neighbour is at its floor, further growth is refused.
        assert!(!s.resize_focused(theme::RESIZE_STEP));
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
        {
            let b = s.tree.branch(s.tree.root).expect("root not a branch");
            assert_eq!(b.dir, Dir::H, "root not an H-branch");
            assert_eq!(b.children().len(), 3, "nested same-dir branch survived");
            let ratio_sum: f64 = b.children().iter().map(|c| c.ratio).sum();
            assert!((ratio_sum - 1.0).abs() < 1e-9);
            for c in b.children() {
                assert!(s.tree.is_leaf(c.node));
            }
        }
        // Focus landed on a surviving leaf, and every root-level gap is
        // insert-eligible again.
        assert!(s.tree.is_leaf(s.focused_leaf));
        s.update_canvas(WA, 0);
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
        s.pin_client(2); // displaces 1 -> stash, prev = 1
        assert!(s.close_focused()); // 2 moves into the empty sibling
        assert_eq!(s.focused_client(), Some(2));
        s.unpin_client(2); // popup dies: 1 must come back
        assert_eq!(
            s.focused_client(),
            Some(1),
            "prev restore survives collapse"
        );
    }

    /// Closing a middle column moves focus to a surviving neighbour.
    #[test]
    fn close_focused_moves_focus_to_neighbour() {
        let mut s = State::new();
        s.split_focused(Dir::H); // two columns, focus left
        s.insert_at_root(InsertAt::Index(1)); // middle column, focused
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
        s.insert_at_root(InsertAt::Index(0));
        let map = s.tree.parent_map();
        for leaf in s.tree.collect_leaves() {
            assert_eq!(map.get(&leaf).copied(), s.tree.find_parent(leaf));
        }
    }

    /// An edge resize must not rewrite a minimized column's ratio from its
    /// pinned pixel width, or un-minimizing after an edge drag would restore
    /// the column as a sliver of its former share; dragging an end column
    /// that is itself minimized is refused outright.
    #[test]
    fn resize_edge_leaves_minimized_ratio_alone() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.insert_at_root(InsertAt::Index(2)); // three columns
        s.update_canvas(WA, 0);
        let leaves = s.tree.collect_leaves();
        s.toggle_minimize(leaves[1]); // middle column pinned to `gap` px
        let ratio_before = root_ratios(&s)[1];
        let (_, w) = s.edge_span(WA, false).unwrap();
        assert_eq!(s.resize_edge(WA, false, w - 40), -40);
        let ratios = root_ratios(&s);
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
        s.update_canvas(WA, 0);
        let canvas_w_before = s.canvas_w(WA);

        let before = s.compute(WA);
        let leaves = s.tree.collect_leaves();
        let (left_id, right_id) = (leaves[0], leaves[1]);
        let left_w_before = before[&left_id].w;
        let right_w_before = before[&right_id].w;

        let shrink_by = 30;
        let applied = s.resize_edge(WA, false, right_w_before - shrink_by);
        assert_eq!(applied, -shrink_by);
        assert_eq!(s.canvas_w_extra, -shrink_by);

        s.update_canvas(WA, 0);
        assert_eq!(s.canvas_w(WA), canvas_w_before - shrink_by);
        let after = s.compute(WA);
        assert_eq!(after[&right_id].w, right_w_before - shrink_by);
        assert_eq!(after[&left_id].w, left_w_before);
    }

    /// A left-edge shrink compensates with `shift_scroll`, taking the
    /// scroll negative — the wallpaper margin left of the canvas. That
    /// margin must survive `update_canvas` (run on every arrange, i.e.
    /// every hover repaint) and close only at `clamp_scroll`.
    #[test]
    fn left_edge_margin_survives_update_canvas() {
        let mut s = State::new();
        s.split_focused(Dir::H);
        s.update_canvas(WA, 0);

        let (_, w) = s.edge_span(WA, true).expect("two columns");
        let shrink_by = 50;
        let applied = s.resize_edge(WA, true, w - shrink_by);
        assert_eq!(applied, -shrink_by);
        s.shift_scroll(applied);

        s.update_canvas(WA, 0);
        assert_eq!(s.scroll_x(), -shrink_by);

        s.clamp_scroll(WA, 0);
        assert_eq!(s.scroll_x(), 0);
    }

    /// A right-edge shrink narrows the canvas under an unmoved scroll;
    /// when the canvas was scrolled to its end this leaves the scroll past
    /// `max_scroll` — the on-screen margin at the dragged edge. Same
    /// lifetime as the left margin: arranges keep it, `clamp_scroll` ends
    /// it.
    #[test]
    fn right_edge_margin_survives_update_canvas() {
        let mut s = State::new();
        // Enough columns to out-span the viewport and open scroll room.
        s.split_focused(Dir::H);
        s.split_focused(Dir::H);
        s.split_focused(Dir::H);
        s.update_canvas(WA, 0);
        let max_before = s.max_scroll(WA);
        assert!(max_before > 0, "canvas should out-span the viewport");
        s.scroll_to(WA, i32::MAX);
        s.land_scroll();
        assert_eq!(s.scroll_x(), max_before);

        let (_, w) = s.edge_span(WA, false).expect("four columns");
        let shrink_by = 60;
        let applied = s.resize_edge(WA, false, w - shrink_by);
        assert_eq!(applied, -shrink_by);

        s.update_canvas(WA, 0);
        assert_eq!(s.max_scroll(WA), max_before - shrink_by);
        assert_eq!(s.scroll_x(), s.max_scroll(WA) + shrink_by);

        s.clamp_scroll(WA, 0);
        assert_eq!(s.scroll_x(), s.max_scroll(WA));
    }
}
