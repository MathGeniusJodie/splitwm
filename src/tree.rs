//! Pure split-tree math.
//!
//! Splits form an n-ary tree (horizontal branches are n-ary, vertical too).
//! Each leaf shows at most one client window. Nodes live in an arena keyed
//! by `NodeId`; branches reference children by id so parent lookup and
//! mutation avoid Rust's aliasing headaches.

use std::collections::HashMap;

pub type Win = u32;

/// Arena key for a tree node. A newtype rather than a second bare `u32`
/// alias so a node id and a `Win` can never be swapped at a call site —
/// only this module mints them (`Tree::gen_id`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    H,
    V,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// This rect inset by `m` on every side.
    pub const fn shrunk(self, m: i32) -> Self {
        Self {
            x: self.x + m,
            y: self.y + m,
            w: self.w - 2 * m,
            h: self.h - 2 * m,
        }
    }
}

#[derive(Default, Clone)]
pub struct Leaf {
    /// The single window shown in this split, if any.
    pub client: Option<Win>,
    /// The window that was last displaced from this split to the taskbar
    /// (e.g. by a popup stealing the slot). If the current occupant's window
    /// is destroyed, this one is pulled back from the taskbar — so closing a
    /// popup restores what you were working on. Single slot, no history:
    /// consumed on restore, ignored if the window has left the taskbar.
    pub prev: Option<Win>,
    pub minimized: bool,
    /// Persistent accent palette index for this split (kept across
    /// splits/closes), used to palette-swap the bitmap window border.
    pub color: crate::Index,
}

impl Leaf {
    /// Show `c` in this leaf. Owns the "a leaf showing a window is never
    /// minimized" invariant: a minimized leaf's window is never mapped, so
    /// assigning a client without clearing the flag would leave the window
    /// unviewable. Every path that puts a window into a leaf goes through
    /// here.
    pub fn show(&mut self, c: Win) {
        self.client = Some(c);
        self.minimized = false;
    }
}

/// One child slot of a branch: the node it holds and its share of the
/// branch's span. A single struct rather than parallel `children`/`ratios`
/// vectors, so a slot's node and ratio can never desync in length or pairing.
#[derive(Clone, Copy)]
pub struct Child {
    pub node: NodeId,
    pub ratio: f64,
}

/// A split branch. `children` is private so its "at least two children"
/// invariant holds by construction: every constructor and mutation below
/// preserves it, so consumers (`Tree::first_leaf`'s `children()[0]`,
/// `State::close_focused`'s sibling lookup, `State::resize_focused`'s
/// neighbour index) need no degenerate-branch guards.
pub struct Branch {
    pub dir: Dir,
    children: Vec<Child>,
}

impl Branch {
    pub fn new(dir: Dir, a: Child, b: Child) -> Self {
        Self {
            dir,
            children: vec![a, b],
        }
    }

    pub fn children(&self) -> &[Child] {
        &self.children
    }

    /// Mutable slot access (ratio rewrites, node swaps); a slice keeps the
    /// length fixed, so arity can't be broken through it.
    pub fn children_mut(&mut self) -> &mut [Child] {
        &mut self.children
    }

    pub fn insert(&mut self, idx: usize, c: Child) {
        self.children.insert(idx, c);
    }

    /// Remove one child. Only valid on branches keeping two or more children
    /// afterwards — a binary branch collapses via its parent instead
    /// (`State::close_focused_binary`) — so a violation is a caller bug and
    /// fails loudly rather than leaving a degenerate branch in the tree.
    pub fn remove(&mut self, idx: usize) -> Child {
        assert!(
            self.children.len() > 2,
            "Branch::remove would leave a degenerate branch"
        );
        self.children.remove(idx)
    }

    /// Replace the child at `idx` with `replacement` (how
    /// `Tree::flatten_same_dir` splices a dissolved same-dir branch's
    /// children into its parent). `replacement` comes from a live branch, so
    /// it holds at least two entries and arity only grows.
    pub fn splice(&mut self, idx: usize, replacement: Vec<Child>) {
        assert!(
            !replacement.is_empty(),
            "Branch::splice with an empty replacement would shrink the branch"
        );
        self.children.splice(idx..=idx, replacement);
    }
}

pub enum Node {
    Leaf(Leaf),
    Branch(Branch),
}

pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    next_id: u32,
    pub root: NodeId,
}

impl Tree {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            NodeId(1),
            Node::Leaf(Leaf {
                color: crate::theme::leaf_color_index(1),
                ..Leaf::default()
            }),
        );
        Self {
            nodes,
            next_id: 2,
            root: NodeId(1),
        }
    }

    fn gen_id(&mut self) -> NodeId {
        let id = self.next_id;
        // Id uniqueness is the arena's core invariant: every live node is
        // addressed by its id, so a silent wraparound here would hand out an
        // id that already aliases a live node instead of failing loudly.
        self.next_id = id
            .checked_add(1)
            .unwrap_or_else(|| panic!("Tree::gen_id: NodeId space exhausted (next_id={id})"));
        NodeId(id)
    }

    pub fn make_leaf(&mut self) -> NodeId {
        let id = self.gen_id();
        self.nodes.insert(
            id,
            Node::Leaf(Leaf {
                color: self.unused_leaf_color(id),
                ..Leaf::default()
            }),
        );
        id
    }

    /// An accent index no existing leaf currently has, so two splits never
    /// look the same while a free colour remains. Falls back to the
    /// id-cycled colour (which may collide) once every leaf has a distinct
    /// entry in `theme::LEAF_PALETTE`.
    fn unused_leaf_color(&self, id: NodeId) -> crate::Index {
        let used: std::collections::HashSet<crate::Index> = self
            .collect_leaves()
            .into_iter()
            .filter_map(|l| self.leaf(l).map(|l| l.color))
            .collect();
        crate::theme::LEAF_PALETTE
            .into_iter()
            .find(|c| !used.contains(c))
            .unwrap_or_else(|| crate::theme::leaf_color_index(id.0))
    }

    pub fn make_branch(&mut self, dir: Dir, ratio: f64, a: NodeId, b: NodeId) -> NodeId {
        let id = self.gen_id();
        self.nodes.insert(
            id,
            Node::Branch(Branch::new(
                dir,
                Child { node: a, ratio },
                Child {
                    node: b,
                    ratio: 1.0 - ratio,
                },
            )),
        );
        id
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }
    pub fn leaf(&self, id: NodeId) -> Option<&Leaf> {
        match self.nodes.get(&id) {
            Some(Node::Leaf(l)) => Some(l),
            _ => None,
        }
    }
    pub fn leaf_mut(&mut self, id: NodeId) -> Option<&mut Leaf> {
        match self.nodes.get_mut(&id) {
            Some(Node::Leaf(l)) => Some(l),
            _ => None,
        }
    }
    pub fn branch(&self, id: NodeId) -> Option<&Branch> {
        match self.nodes.get(&id) {
            Some(Node::Branch(b)) => Some(b),
            _ => None,
        }
    }
    pub fn branch_mut(&mut self, id: NodeId) -> Option<&mut Branch> {
        match self.nodes.get_mut(&id) {
            Some(Node::Branch(b)) => Some(b),
            _ => None,
        }
    }
    pub fn is_leaf(&self, id: NodeId) -> bool {
        matches!(self.nodes.get(&id), Some(Node::Leaf(_)))
    }

    pub fn insert_node(&mut self, node: Node) -> NodeId {
        let id = self.gen_id();
        self.nodes.insert(id, node);
        id
    }
    pub fn remove_node(&mut self, id: NodeId) {
        self.nodes.remove(&id);
    }

    /// If `parent.children[idx]` is a branch in the same direction as
    /// `parent`, splice its children (ratios scaled by the slot's ratio)
    /// into `parent` in its place and drop the dissolved branch node.
    /// `State::split_node` maintains the no-nested-same-dir invariant on
    /// the way in; this is the counterpart for collapse paths
    /// (`State::close_focused`), which can splice a same-dir branch into a
    /// grandparent — leaving it nested would demote its gaps from
    /// root-level boundaries, silently losing their "+" insert buttons.
    pub fn flatten_same_dir(&mut self, parent: NodeId, idx: usize) {
        let (pdir, slot) = match self.branch(parent) {
            Some(b) if idx < b.children().len() => (b.dir, b.children()[idx]),
            _ => return,
        };
        let sub: Vec<Child> = match self.branch(slot.node) {
            Some(b) if b.dir == pdir => b
                .children()
                .iter()
                .map(|c| Child {
                    node: c.node,
                    ratio: c.ratio * slot.ratio,
                })
                .collect(),
            _ => return,
        };
        if let Some(b) = self.branch_mut(parent) {
            b.splice(idx, sub);
        }
        self.remove_node(slot.node);
    }

    /// Depth-first leaf ids in layout order.
    pub fn collect_leaves(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_from(self.root, &mut out);
        out
    }
    pub fn collect_from(&self, node: NodeId, out: &mut Vec<NodeId>) {
        match self.nodes.get(&node) {
            Some(Node::Leaf(_)) => out.push(node),
            Some(Node::Branch(b)) => {
                for c in b.children() {
                    self.collect_from(c.node, out);
                }
            }
            None => {}
        }
    }

    /// First leaf in subtree (left/top-most).
    pub fn first_leaf(&self, node: NodeId) -> NodeId {
        match self.nodes.get(&node) {
            Some(Node::Branch(b)) => self.first_leaf(b.children()[0].node),
            _ => node,
        }
    }

    pub fn find_leaf_for_client(&self, c: Win) -> Option<NodeId> {
        self.find_leaf_for_client_from(self.root, c)
    }

    fn find_leaf_for_client_from(&self, node: NodeId, c: Win) -> Option<NodeId> {
        match self.nodes.get(&node)? {
            Node::Leaf(l) => (l.client == Some(c)).then_some(node),
            Node::Branch(b) => b
                .children()
                .iter()
                .find_map(|child| self.find_leaf_for_client_from(child.node, c)),
        }
    }

    /// (parent id, index of `target` within parent.children), or None for root.
    ///
    /// Scans the whole arena, so it's O(n) per call. Fine for one-off
    /// mutations triggered by user actions (splits, closes, resizes), where
    /// `n` is tens of nodes and calls happen at most a few times per action.
    /// For per-frame loops that need a parent lookup for every leaf, use
    /// `parent_map` instead — it builds the full mapping in one arena walk,
    /// avoiding the O(n²) blowup of calling `find_parent` once per leaf.
    pub fn find_parent(&self, target: NodeId) -> Option<(NodeId, usize)> {
        for (&id, node) in &self.nodes {
            if let Node::Branch(b) = node {
                if let Some(idx) = b.children().iter().position(|c| c.node == target) {
                    return Some((id, idx));
                }
            }
        }
        None
    }

    /// (parent, index-within-parent) for every child in one arena walk, for
    /// callers that need many parent lookups per frame — `find_parent` scans
    /// the whole arena per call, which is O(n²) when done once per leaf.
    pub fn parent_map(&self) -> HashMap<NodeId, (NodeId, usize)> {
        let mut out = HashMap::new();
        for (&id, node) in &self.nodes {
            if let Node::Branch(b) = node {
                for (i, c) in b.children().iter().enumerate() {
                    out.insert(c.node, (id, i));
                }
            }
        }
        out
    }

    /// The layout's width in *column units*: how many minimum-width columns
    /// it needs side by side. A leaf is one column; an H-branch needs the sum
    /// of its children; a V-branch is only as wide as its widest child —
    /// stacking leaves vertically must not demand extra canvas width.
    pub fn h_units(&self) -> i32 {
        self.h_units_from(self.root)
    }

    fn h_units_from(&self, node: NodeId) -> i32 {
        match self.nodes.get(&node) {
            Some(Node::Branch(b)) => {
                let units = b.children().iter().map(|c| self.h_units_from(c.node));
                match b.dir {
                    Dir::H => units.sum(),
                    Dir::V => units.max().unwrap_or(1),
                }
            }
            Some(Node::Leaf(_)) => 1,
            None => 0,
        }
    }
}

// --- geometry ---

impl Tree {
    /// (is-minimized, ratio) for each child, the shared input `child_sizes`
    /// needs — factored out since both the leaf-geometry and boundary walks
    /// build the exact same thing per branch.
    fn child_meta(&self, children: &[Child]) -> Vec<(bool, f64)> {
        children
            .iter()
            .map(|c| (self.leaf(c.node).is_some_and(|l| l.minimized), c.ratio))
            .collect()
    }
}

fn child_sizes(children: &[(bool, f64)], usable: i32, min_sz: i32) -> Vec<i32> {
    // children: (is_minimized_leaf, ratio)
    let n = children.len();
    let mut min_total = 0;
    let mut ratio_sum = 0.0;
    let mut last_normal: Option<usize> = None;
    for (i, &(is_min, r)) in children.iter().enumerate() {
        if is_min {
            min_total += min_sz;
        } else {
            ratio_sum += r;
            last_normal = Some(i);
        }
    }
    if ratio_sum <= 0.0 {
        ratio_sum = 1.0;
    }
    let usable_normal = (usable - min_total).max(0);
    let normals_total = children.iter().filter(|&&(is_min, _)| !is_min).count() as i32;
    // The per-child 1px floor is best-effort: with fewer usable pixels than
    // normal children (viewport shrunk below the layout's demands), floors
    // of 1 would sum past `usable` no matter how the rest is clamped, so
    // they give way to 0 and the sum stays bounded instead of children
    // overlapping their siblings' slots.
    let floor = i32::from(usable_normal >= normals_total);
    let mut sizes = vec![0i32; n];
    let mut allocated = 0;
    let mut normals_seen = 0;
    for (i, &(is_min, r)) in children.iter().enumerate() {
        if is_min {
            sizes[i] = min_sz;
        } else if Some(i) != last_normal {
            normals_seen += 1;
            // Never allocate past what's left minus one floor for each
            // later normal child.
            let left = usable_normal - allocated - (normals_total - normals_seen) * floor;
            let sz = ((f64::from(usable_normal) * r / ratio_sum).floor() as i32)
                .max(floor)
                .min(left.max(floor));
            sizes[i] = sz;
            allocated += sz;
        }
    }
    if let Some(ln) = last_normal {
        sizes[ln] = (usable_normal - allocated).max(floor);
    }
    sizes
}

impl Tree {
    /// Compute the screen rect of every leaf. `geos` is keyed by leaf id.
    pub fn compute(&self, area: Rect, gap: i32) -> HashMap<NodeId, Rect> {
        let mut geos = HashMap::new();
        self.compute_inner(self.root, area.shrunk(gap), gap, &mut geos);
        geos
    }

    /// Pixel widths of the root's immediate horizontally-arranged columns,
    /// without recursing into subtrees — so it still works when a column
    /// is itself a further-split branch, whose own leaves wouldn't appear
    /// in `compute`'s per-leaf geometry. A single leaf, or a root that's
    /// itself a *vertical* branch (children stacked, each spanning the
    /// full width), count as one column occupying the whole row —
    /// `usable_w` is the already gap-trimmed width available to it (see
    /// `compute`'s `w - 2 * gap`).
    pub fn root_h_sizes(&self, usable_w: i32, gap: i32) -> Option<Vec<i32>> {
        match self.get(self.root)? {
            Node::Leaf(_) | Node::Branch(Branch { dir: Dir::V, .. }) => Some(vec![usable_w.max(0)]),
            Node::Branch(b) => {
                let n = i32::try_from(b.children().len()).unwrap_or(i32::MAX);
                let meta = self.child_meta(b.children());
                let usable = (usable_w - gap * (n - 1)).max(0);
                Some(child_sizes(&meta, usable, gap))
            }
        }
    }

    /// Visit each of `node`'s children with its laid-out slot — the shared
    /// geometry behind `compute_inner` and `boundaries_inner`, so leaf
    /// placement and boundary handles can never disagree. No-op when `node`
    /// isn't a branch. A minimized child collapses to `gap` in the split
    /// dimension (both directions): it's the same size already reserved as
    /// breathing room between normal children, so it stays visually
    /// consistent with the layout's spacing rather than needing a size of
    /// its own.
    fn walk_children(&self, node: NodeId, at: Rect, gap: i32, f: &mut impl FnMut(ChildSlot)) {
        let Some(b) = self.branch(node) else {
            return;
        };
        let dir = b.dir;
        let n = i32::try_from(b.children().len()).unwrap_or(i32::MAX);
        let meta = self.child_meta(b.children());
        let span = if dir == Dir::H { at.w } else { at.h };
        let usable = (span - gap * (n - 1)).max(0);
        let sizes = child_sizes(&meta, usable, gap);
        let mut pos = if dir == Dir::H { at.x } else { at.y };
        for (i, (child, &sz)) in b.children().iter().zip(&sizes).enumerate() {
            let rect = if dir == Dir::H {
                Rect {
                    x: pos,
                    w: sz,
                    ..at
                }
            } else {
                Rect {
                    y: pos,
                    h: sz,
                    ..at
                }
            };
            f(ChildSlot {
                idx: i,
                child: child.node,
                dir,
                rect,
                next_size: sizes.get(i + 1).copied(),
                resizable: i + 1 < meta.len() && !meta[i].0 && !meta[i + 1].0,
            });
            pos += sz + gap;
        }
    }

    fn compute_inner(&self, node: NodeId, at: Rect, gap: i32, geos: &mut HashMap<NodeId, Rect>) {
        if self.is_leaf(node) {
            geos.insert(node, at);
            return;
        }
        self.walk_children(node, at, gap, &mut |s| {
            self.compute_inner(s.child, s.rect, gap, geos);
        });
    }
}

/// One child's laid-out slot within its branch, as visited by
/// `Tree::walk_children`.
struct ChildSlot {
    /// Index within the parent's children.
    idx: usize,
    child: NodeId,
    /// The parent branch's direction.
    dir: Dir,
    rect: Rect,
    /// The next sibling's size along `dir`; `None` for the last child (no
    /// gap follows it).
    next_size: Option<i32>,
    /// Whether the gap after this child can be dragged: false when either
    /// neighbour is a minimized leaf, whose pixel size is pinned to `gap`
    /// regardless of ratio.
    resizable: bool,
}

#[cfg(test)]
mod child_sizes_tests {
    use super::child_sizes;

    #[test]
    fn sizes_fill_usable_exactly() {
        // Rounding must never leave the last child short or long.
        let kids = [(false, 0.3), (false, 0.3), (false, 0.4)];
        let sizes = child_sizes(&kids, 997, 20);
        assert_eq!(sizes.iter().sum::<i32>(), 997);
        assert!(sizes.iter().all(|&s| s >= 1));
    }

    #[test]
    fn minimized_children_get_min_size() {
        let kids = [(true, 0.5), (false, 0.25), (false, 0.25)];
        let sizes = child_sizes(&kids, 1000, 20);
        assert_eq!(sizes[0], 20);
        assert_eq!(sizes[1] + sizes[2], 980);
        // Equal ratios split the remainder evenly.
        assert_eq!(sizes[1], 490);
    }

    #[test]
    fn all_minimized_does_not_panic_or_go_negative() {
        let kids = [(true, 0.5), (true, 0.5)];
        let sizes = child_sizes(&kids, 100, 20);
        assert_eq!(sizes, vec![20, 20]);
    }

    #[test]
    fn skewed_ratios_never_sum_past_usable() {
        // A dominant first ratio floors the middle child to 1px; the last
        // child's remainder floor must not push the sum past `usable`.
        let kids = [(false, 0.9), (false, 0.05), (false, 0.05)];
        let sizes = child_sizes(&kids, 10, 2);
        assert!(sizes.iter().sum::<i32>() <= 10, "sizes {sizes:?}");
        assert!(sizes.iter().all(|&s| s >= 1));
    }

    #[test]
    fn more_children_than_pixels_never_overlaps() {
        // 3 normal children on 2 usable px: the per-child 1px floors must
        // give way (to 0) so the sum stays within `usable` — children
        // spilling past their branch's span would overlap the next sibling.
        let third = 1.0 / 3.0;
        let kids = [(false, third), (false, third), (false, third)];
        let sizes = child_sizes(&kids, 2, 2);
        assert!(sizes.iter().sum::<i32>() <= 2, "sizes {sizes:?}");
        assert!(sizes.iter().all(|&s| s >= 0), "sizes {sizes:?}");
    }

    #[test]
    fn zero_ratio_sum_falls_back() {
        // Degenerate ratios must not divide by zero.
        let kids = [(false, 0.0), (false, 0.0)];
        let sizes = child_sizes(&kids, 100, 20);
        assert_eq!(sizes.iter().sum::<i32>(), 100);
    }

    #[test]
    fn usable_smaller_than_min_total_clamps() {
        let kids = [(true, 0.5), (false, 0.5)];
        let sizes = child_sizes(&kids, 10, 20);
        assert_eq!(sizes[0], 20);
        assert_eq!(
            sizes[1], 0,
            "no usable space left: the 1px floor gives way rather than \
             pushing the sum further past `usable`"
        );
    }
}

/// A gap between two adjacent children of a branch: the place a drag handle
/// (and, at root level, a "+" button) sits. Coordinates are canvas-space.
/// `dir` is the parent branch's direction: `H` is a vertical gap dragged
/// along x, `V` a horizontal gap dragged along y.
#[derive(Clone, Copy)]
pub struct Boundary {
    pub parent: NodeId,
    pub idx: usize, // first (left/top) child index within parent.children
    pub dir: Dir,
    pub pos: i32,       // gap centre along the drag axis
    pub start: i32,     // first child's start along the drag axis
    pub first: i32,     // first child's size along the drag axis
    pub second: i32,    // second child's size along the drag axis
    pub cross: i32,     // strip start on the cross axis
    pub cross_len: i32, // strip extent on the cross axis
    pub root: bool,     // whether `parent` is the tree root (insert eligible)
    /// Whether dragging this gap can actually resize its neighbours: false
    /// when either adjacent child is a minimized leaf — its pixel size is
    /// pinned to `gap` regardless of ratio, so the drag's pixel-space
    /// fraction wouldn't correspond to the ratio-space one `resize_boundary`
    /// applies (the handle would not track the pointer).
    pub resizable: bool,
}

impl Tree {
    /// Gaps between adjacent children in every branch, both directions.
    pub fn boundaries(&self, area: Rect, gap: i32) -> Vec<Boundary> {
        let mut out = Vec::new();
        self.boundaries_inner(self.root, area.shrunk(gap), gap, &mut out);
        out
    }

    fn boundaries_inner(&self, node: NodeId, at: Rect, gap: i32, out: &mut Vec<Boundary>) {
        self.walk_children(node, at, gap, &mut |s| {
            if let Some(second) = s.next_size {
                let (drag_start, drag_size, cross, cross_len) = if s.dir == Dir::H {
                    (s.rect.x, s.rect.w, at.y, at.h)
                } else {
                    (s.rect.y, s.rect.h, at.x, at.w)
                };
                out.push(Boundary {
                    parent: node,
                    idx: s.idx,
                    dir: s.dir,
                    pos: drag_start + drag_size + gap / 2,
                    start: drag_start,
                    first: drag_size,
                    second,
                    cross,
                    cross_len,
                    root: node == self.root,
                    resizable: s.resizable,
                });
            }
            self.boundaries_inner(s.child, s.rect, gap, out);
        });
    }
}
