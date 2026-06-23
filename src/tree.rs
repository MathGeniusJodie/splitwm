//! Pure split-tree math, ported from splitwm/tree.lua.
//!
//! Splits form an n-ary tree (horizontal branches are n-ary, vertical too).
//! Each leaf owns a tab stack of client windows. Nodes live in an arena keyed
//! by `NodeId`; branches reference children by id so parent lookup and
//! mutation avoid Rust's aliasing headaches.

use std::collections::HashMap;

pub type Win = u32;
pub type NodeId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    H,
    V,
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Default)]
pub struct Leaf {
    pub tabs: Vec<Win>,
    /// 0-based index into `tabs`; meaningless when `tabs` is empty.
    pub active: usize,
    pub minimized: bool,
}

pub enum Node {
    Leaf(Leaf),
    Branch {
        dir: Dir,
        children: Vec<NodeId>,
        ratios: Vec<f64>,
    },
}

pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    next_id: NodeId,
    pub root: NodeId,
}

impl Tree {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(1, Node::Leaf(Leaf::default()));
        Self {
            nodes,
            next_id: 2,
            root: 1,
        }
    }

    const fn gen_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn make_leaf(&mut self) -> NodeId {
        let id = self.gen_id();
        self.nodes.insert(id, Node::Leaf(Leaf::default()));
        id
    }

    pub fn make_branch(&mut self, dir: Dir, ratio: f64, a: NodeId, b: NodeId) -> NodeId {
        let id = self.gen_id();
        self.nodes.insert(
            id,
            Node::Branch {
                dir,
                children: vec![a, b],
                ratios: vec![ratio, 1.0 - ratio],
            },
        );
        id
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id)
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

    /// Depth-first leaf ids in layout order.
    pub fn collect_leaves(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_from(self.root, &mut out);
        out
    }
    pub fn collect_from(&self, node: NodeId, out: &mut Vec<NodeId>) {
        match self.nodes.get(&node) {
            Some(Node::Leaf(_)) => out.push(node),
            Some(Node::Branch { children, .. }) => {
                for &c in children {
                    self.collect_from(c, out);
                }
            }
            None => {}
        }
    }

    /// First leaf in subtree (left/top-most).
    pub fn first_leaf(&self, node: NodeId) -> NodeId {
        match self.nodes.get(&node) {
            Some(Node::Branch { children, .. }) => self.first_leaf(children[0]),
            _ => node,
        }
    }

    pub fn find_leaf_for_client(&self, c: Win) -> Option<NodeId> {
        self.find_leaf_for_client_from(self.root, c)
    }

    fn find_leaf_for_client_from(&self, node: NodeId, c: Win) -> Option<NodeId> {
        match self.nodes.get(&node)? {
            Node::Leaf(l) => l.tabs.contains(&c).then_some(node),
            Node::Branch { children, .. } => children
                .iter()
                .find_map(|&child| self.find_leaf_for_client_from(child, c)),
        }
    }

    pub fn contains(&self, subtree: NodeId, target: NodeId) -> bool {
        if subtree == target {
            return true;
        }
        match self.nodes.get(&subtree) {
            Some(Node::Branch { children, .. }) => {
                children.iter().any(|&c| self.contains(c, target))
            }
            _ => false,
        }
    }

    /// (parent id, index of `target` within parent.children), or None for root.
    pub fn find_parent(&self, target: NodeId) -> Option<(NodeId, usize)> {
        for (&id, node) in &self.nodes {
            if let Node::Branch { children, .. } = node {
                if let Some(idx) = children.iter().position(|&c| c == target) {
                    return Some((id, idx));
                }
            }
        }
        None
    }
}

// --- geometry ---

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
    let mut sizes = vec![0i32; n];
    let mut allocated = 0;
    for (i, &(is_min, r)) in children.iter().enumerate() {
        if is_min {
            sizes[i] = min_sz;
        } else if Some(i) != last_normal {
            let sz = ((f64::from(usable_normal) * r / ratio_sum).floor() as i32).max(1);
            sizes[i] = sz;
            allocated += sz;
        }
    }
    if let Some(ln) = last_normal {
        sizes[ln] = (usable_normal - allocated).max(1);
    }
    sizes
}

impl Tree {
    /// Compute the screen rect of every leaf. `geos` is keyed by leaf id.
    pub fn compute(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        gap: i32,
        tb_h: i32,
    ) -> HashMap<NodeId, Rect> {
        let mut geos = HashMap::new();
        self.compute_inner(
            self.root,
            x + gap,
            y + gap,
            w - 2 * gap,
            h - 2 * gap,
            gap,
            tb_h,
            &mut geos,
        );
        geos
    }

    #[allow(clippy::many_single_char_names, clippy::too_many_arguments)] // recursive geometry walk
    fn compute_inner(
        &self,
        node: NodeId,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        gap: i32,
        tb_h: i32,
        geos: &mut HashMap<NodeId, Rect>,
    ) {
        match self.nodes.get(&node) {
            Some(Node::Leaf(_)) => {
                geos.insert(node, Rect { x, y, w, h });
            }
            Some(Node::Branch {
                dir,
                children,
                ratios,
            }) => {
                let inner = gap;
                let n = i32::try_from(children.len()).unwrap_or(i32::MAX);
                let meta: Vec<(bool, f64)> = children
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| {
                        let is_min = self.leaf(c).is_some_and(|l| l.minimized);
                        (is_min, ratios[i])
                    })
                    .collect();
                if *dir == Dir::H {
                    let usable = (w - inner * (n - 1)).max(0);
                    let sizes = child_sizes(&meta, usable, gap);
                    let mut cx = x;
                    for (i, &c) in children.iter().enumerate() {
                        let cw = sizes[i];
                        self.compute_inner(c, cx, y, cw, h, gap, tb_h, geos);
                        cx += cw + inner;
                    }
                } else {
                    let usable = (h - inner * (n - 1)).max(0);
                    let min_sz = (tb_h - inner).max(0);
                    let sizes = child_sizes(&meta, usable, min_sz);
                    let mut cy = y;
                    for (i, &c) in children.iter().enumerate() {
                        let ch = sizes[i];
                        self.compute_inner(c, x, cy, w, ch, gap, tb_h, geos);
                        cy += ch + inner;
                    }
                }
            }
            None => {}
        }
    }
}

/// Client content rect inside a leaf (below the tab bar, inside the border),
/// translated by horizontal scroll. Mirrors `theme.client_geo` in Lua.
pub fn client_geo(geo: Rect, bw: i32, gap: i32, tb_h: i32, scroll_x: i32) -> Rect {
    Rect {
        x: geo.x + bw - scroll_x,
        y: geo.y - gap + tb_h,
        w: (geo.w - bw * 2).max(1),
        h: (geo.h + gap - bw - tb_h).max(1),
    }
}
