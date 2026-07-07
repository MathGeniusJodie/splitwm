//! Pointer semantics on the chrome: the priority-ordered hit-test, click
//! dispatch, gap/edge drags, canvas panning, and the shared layout-commit
//! epilogue. Ported from master's `wm/input/pointer.rs` + `scroll.rs`;
//! floats join in M6. Unlike X11, the surface under the pointer and the
//! modifier state are already ours — no server round-trips, no caching.

use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point};

use super::Comp;
use crate::state::{Activation, InsertAt};
use crate::theme;
use crate::tree::{Boundary, Dir, NodeId, Win};
use crate::widgets::{leaf_meta, BtnKind, FrameRect};

/// An in-progress drag, keyed off button-1 press on a handle/edge/float
/// frame.
#[derive(Clone, Copy)]
pub enum ActiveDrag {
    Split(SplitDrag),
    Edge(EdgeDrag),
    Float(FloatDrag),
}

/// Moving a float by its chrome frame: the pointer's offset into the
/// client rect is pinned for the whole gesture.
#[derive(Clone, Copy)]
pub struct FloatDrag {
    pub win: Win,
    pub dx: i32,
    pub dy: i32,
}

/// Dragging the boundary between two children of `parent`: fraction =
/// (pointer - start) / combined extent of the two children.
#[derive(Clone, Copy)]
pub struct SplitDrag {
    pub parent: NodeId,
    pub idx: usize,
    pub vertical: bool,
    pub start: i32,
    pub combined: i32,
    pub gap: i32,
}

/// Dragging an outer canvas edge: the far edge of the leftmost/rightmost
/// column stays fixed at `anchor_x` (screen space) for the whole gesture.
#[derive(Clone, Copy)]
pub struct EdgeDrag {
    pub left: bool,
    pub anchor_x: i32,
}

/// What a click on the chrome resolved to, in priority order.
enum Hit {
    Btn(NodeId, BtnKind),
    TaskbarClose(Win),
    TaskbarTile(Win),
    QuickLaunch(usize),
    Title(NodeId),
    Plus(InsertAt),
    Handle(Boundary),
    Edge(bool),
    LeafBody(NodeId),
    Miss,
}

const fn rect_contains(r: FrameRect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

impl Comp {
    /// A button-1/3 press that landed on the chrome (no client surface
    /// under the pointer). Returns `true` when the press was consumed.
    pub fn on_chrome_button(&mut self, pos: Point<f64, Logical>, secondary: bool) -> bool {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        // Button 1 on a float's frame border (a press inside the client
        // area never reaches here — the surface catches it): focus the
        // float and start moving it.
        if !secondary {
            let hit = self.float_stack.iter().copied().find(|&fw| {
                self.managed
                    .float(fw)
                    .is_some_and(|(_, f)| rect_contains(f.frame_rect(), mx, my))
            });
            if let Some(fw) = hit {
                let (dx, dy) = self
                    .managed
                    .float(fw)
                    .map(|(_, f)| (mx - f.x, my - f.y))
                    .expect("found above");
                self.drag = Some(ActiveDrag::Float(FloatDrag { win: fw, dx, dy }));
                self.focus_float(fw);
                return true;
            }
        }
        // Hit regions describe the final layout, but an animation may still
        // be drawing chrome mid-slide; snap it so the click lands on what
        // the user sees.
        if self.anim.is_some() {
            self.finish_animation();
        }
        match self.hit_test(mx, my) {
            // The split button takes left and right click (right picks the
            // opposite direction); everything else is left only.
            Hit::Btn(leaf, kind @ BtnKind::Split) => {
                self.click_split_button(leaf, kind, secondary);
            }
            _ if secondary => {}
            Hit::Btn(leaf, kind) => self.click_split_button(leaf, kind, false),
            Hit::TaskbarClose(win) => self.close_client(win),
            Hit::TaskbarTile(win) => self.bring_into_layout(win, true),
            Hit::QuickLaunch(i) => {
                if let Some(cmd) = self.quick.get(i).map(|q| q.cmd.clone()) {
                    self.spawn(&cmd);
                }
            }
            Hit::Title(leaf) | Hit::LeafBody(leaf) => {
                self.state.focus_leaf(leaf);
                self.arrange();
            }
            Hit::Plus(at) => {
                self.state.insert_at_root(at);
                self.animate = true;
                self.commit_layout();
            }
            Hit::Handle(b) => {
                // A gap next to a minimized leaf can't be dragged (its
                // pixel size is pinned); ignore the press.
                if b.resizable {
                    // Land any in-flight glide: the drag reads scroll_x
                    // fresh per motion, and a glide underneath would drift
                    // the anchor math out from under the pointer.
                    self.state.land_scroll();
                    self.drag = Some(ActiveDrag::Split(SplitDrag {
                        parent: b.parent,
                        idx: b.idx,
                        vertical: b.dir == Dir::V,
                        start: b.start,
                        combined: b.first + b.second,
                        gap: theme::GAP,
                    }));
                }
            }
            Hit::Edge(left) => {
                self.state.land_scroll();
                let wa = self.layout_area();
                if let Some((start_x, w)) = self.state.edge_span(wa, left) {
                    let canvas_anchor = if left { start_x + w } else { start_x };
                    let anchor_x = canvas_anchor - self.state.scroll_x();
                    self.drag = Some(ActiveDrag::Edge(EdgeDrag { left, anchor_x }));
                }
            }
            Hit::Miss => return false,
        }
        true
    }

    /// Feed pointer motion into an active drag. Returns `true` while a
    /// drag is consuming motion (the client under the pointer must not
    /// also see it).
    pub fn on_drag_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        match self.drag {
            Some(ActiveDrag::Float(fd)) => {
                self.move_float(fd.win, pos.x as i32 - fd.dx, pos.y as i32 - fd.dy);
                true
            }
            Some(ActiveDrag::Edge(ed)) => {
                let wa = self.layout_area();
                let mouse_x = pos.x as i32;
                // anchor_x is the fixed far edge, so the gap to the mouse
                // *is* the target width — width is scroll-invariant.
                let target_w = if ed.left {
                    ed.anchor_x - mouse_x
                } else {
                    mouse_x - ed.anchor_x
                };
                let applied = self.state.resize_edge(wa, ed.left, target_w);
                // Growing the left column shifts every later column right
                // in canvas space; scroll by the same amount so only the
                // dragged edge visibly moves.
                if ed.left && applied != 0 {
                    self.state.shift_scroll(applied);
                }
                self.arrange();
                true
            }
            Some(ActiveDrag::Split(d)) => {
                if d.combined <= 0 {
                    return true;
                }
                // Only x scrolls; a row-boundary drag reads y directly.
                let canvas_pos = if d.vertical {
                    pos.y as i32
                } else {
                    pos.x as i32 + self.state.scroll_x()
                };
                let new_first = canvas_pos - d.start - d.gap / 2;
                let frac = f64::from(new_first) / f64::from(d.combined);
                self.state.resize_boundary(d.parent, d.idx, frac);
                self.arrange();
                true
            }
            None => false,
        }
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// The topmost float whose chrome frame band (frame rect minus client
    /// rect) contains `pos`. Frames overlap whatever lies beneath them, so
    /// the button handler must check this before surface routing — the
    /// press would otherwise fall through to the client underneath.
    pub fn float_frame_at(&self, pos: Point<f64, Logical>) -> Option<Win> {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        self.float_stack.iter().copied().find(|&fw| {
            self.managed.float(fw).is_some_and(|(_, f)| {
                rect_contains(f.frame_rect(), mx, my)
                    && !(mx >= f.x && mx < f.x + f.w && my >= f.y && my < f.y + f.h)
            })
        })
    }

    /// Priority-ordered hit-test of everything clickable on the chrome,
    /// shared by `on_chrome_button` (dispatch) and `hover_cursor`
    /// (feedback) — a single ordering both consume, so click handling and
    /// hover feedback can never drift apart (master's invariant).
    fn hit_test(&self, mx: i32, my: i32) -> Hit {
        if let Some((leaf, kind)) = self
            .widgets
            .btn_regions
            .iter()
            .find(|(r, _, _)| rect_contains(*r, mx, my))
            .map(|(_, l, k)| (*l, *k))
        {
            return Hit::Btn(leaf, kind);
        }
        // Compressed taskbar tiles overlap like fanned cards, rightmost on
        // top; reverse iteration matches draw order so the topmost tile
        // wins. The corner "x" badge outranks the tile bodies.
        if let Some(win) = self
            .widgets
            .taskbar_regions
            .iter()
            .rev()
            .find(|t| rect_contains(t.close, mx, my))
            .map(|t| t.win)
        {
            return Hit::TaskbarClose(win);
        }
        if let Some(win) = self
            .widgets
            .taskbar_regions
            .iter()
            .rev()
            .find(|t| rect_contains(t.rect, mx, my))
            .map(|t| t.win)
        {
            return Hit::TaskbarTile(win);
        }
        if let Some(i) = self
            .widgets
            .quick_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, i)| *i)
        {
            return Hit::QuickLaunch(i);
        }
        if let Some(leaf) = self
            .widgets
            .title_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, l)| *l)
        {
            return Hit::Title(leaf);
        }
        // "+" buttons sit centred inside their drag handle's larger hit
        // region — check the narrower "+" rects first so they aren't
        // shadowed by the handles.
        if let Some(at) = self
            .widgets
            .plus_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, at)| *at)
        {
            return Hit::Plus(at);
        }
        if let Some(b) = self
            .widgets
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, b)| *b)
        {
            return Hit::Handle(b);
        }
        if let Some(&(_, left)) = self
            .widgets
            .edge_handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            return Hit::Edge(left);
        }
        if let Some(leaf) = self
            .prev_frame_rect
            .iter()
            .find(|(l, r)| self.state.tree.is_leaf(**l) && rect_contains(**r, mx, my))
            .map(|(l, _)| *l)
        {
            return Hit::LeafBody(leaf);
        }
        Hit::Miss
    }

    /// Pick the pointer shape for a hover position on the chrome,
    /// mirroring master's `hover_cursor`: resize arrows over gap/edge drag
    /// handles, the hand over clickable things, the "disabled" shape over
    /// a disabled titlebar button, the arrow otherwise. Consumes the same
    /// `hit_test` ordering as `on_chrome_button`, so the advertised cursor
    /// always matches the click.
    pub fn hover_cursor(&self, pos: Point<f64, Logical>) -> CursorIcon {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        // Float frames take the press outright (see on_chrome_button) and
        // advertise the hand, like master's frame windows did.
        if self.float_frame_at(pos).is_some() {
            return CursorIcon::Pointer;
        }
        match self.hit_test(mx, my) {
            Hit::Btn(leaf, kind) => {
                // Mirror compose_frame's enabled/disabled choice for the
                // button art (a minimized leaf's whole-frame region is
                // always a live restore button).
                if let Some(&frame) = self.prev_frame_rect.get(&leaf) {
                    let meta = leaf_meta(
                        &self.state.tree,
                        self.parents.get(&leaf).copied(),
                        leaf,
                        frame,
                    );
                    let disabled = !meta.minimized
                        && match kind {
                            BtnKind::Close | BtnKind::Minimize => meta.parent_dir.is_none(),
                            BtnKind::Split => !meta.can_split,
                        };
                    if disabled {
                        return CursorIcon::NotAllowed;
                    }
                }
                CursorIcon::Pointer
            }
            Hit::TaskbarClose(_)
            | Hit::TaskbarTile(_)
            | Hit::QuickLaunch(_)
            | Hit::Title(_)
            | Hit::Plus(_) => CursorIcon::Pointer,
            Hit::Handle(b) => {
                // A gap next to a minimized leaf can't be dragged; don't
                // advertise a resize that won't happen.
                if !b.resizable {
                    CursorIcon::Default
                } else if b.dir == Dir::V {
                    CursorIcon::NsResize
                } else {
                    CursorIcon::EwResize
                }
            }
            Hit::Edge(_) => CursorIcon::EwResize,
            Hit::LeafBody(_) | Hit::Miss => CursorIcon::Default,
        }
    }

    /// Act on a split-control button click. `secondary` is a right-click,
    /// which on the split button picks the opposite split direction.
    pub fn click_split_button(&mut self, leaf: NodeId, kind: BtnKind, secondary: bool) {
        let wa = self.layout_area();
        let frame = self
            .prev_frame_rect
            .get(&leaf)
            .copied()
            .unwrap_or(FrameRect {
                x: 0,
                y: 0,
                w: wa.w,
                h: wa.h,
            });
        let meta = leaf_meta(
            &self.state.tree,
            self.parents.get(&leaf).copied(),
            leaf,
            frame,
        );
        match kind {
            BtnKind::Split => {
                if !meta.can_split {
                    return;
                }
                let base = if meta.wider { Dir::H } else { Dir::V };
                let dir = if secondary {
                    match base {
                        Dir::V => Dir::H,
                        Dir::H => Dir::V,
                    }
                } else {
                    base
                };
                self.state.focus_leaf(leaf);
                let pre = self.prev_frame_rect.get(&leaf).copied();
                self.animate = self.state.split_focused(dir);
                // Carry the pre-split frame so content slides from its old
                // spot.
                if self.animate {
                    if let Some(rect) = pre {
                        self.prev_frame_rect
                            .insert(self.state.focused_leaf_valid(), rect);
                    }
                }
            }
            BtnKind::Close => {
                if meta.parent_dir.is_none() {
                    return;
                }
                self.state.focus_leaf(leaf);
                self.animate = self.state.close_focused();
            }
            BtnKind::Minimize => {
                if meta.parent_dir.is_none() {
                    return;
                }
                self.animate = self.state.toggle_minimize(leaf);
            }
        }
        self.commit_layout();
    }

    /// Bring a managed tiled window into view and focus it: into its split
    /// if it has one, otherwise into the focused split. `animate` requests
    /// a transition, but only when rects actually moved.
    pub fn bring_into_layout(&mut self, win: Win, animate: bool) {
        let changed = match self.state.activate_client(win) {
            Activation::NotFound => {
                let leaf = self.state.focused_leaf_valid();
                self.state.assign_to_leaf(win, leaf);
                true
            }
            Activation::Unminimized => true,
            Activation::Unchanged => false,
        };
        if animate {
            self.animate = changed;
        }
        self.commit_layout();
    }

    /// Shared epilogue for every layout-mutating action: invalidate drags
    /// whose tree snapshot went stale, keep the focused split in view
    /// (gliding unless an animation is about to run), re-arrange.
    pub fn commit_layout(&mut self) {
        self.drag = None;
        let wa = self.layout_area();
        self.state.clamp_scroll(wa, 0);
        self.state.ensure_in_view(wa);
        // An animation's placements are computed from scroll_x at arrange
        // time and held for the whole transition; a concurrent glide would
        // make them stale every frame, so land it. Otherwise leave the
        // target so step_scroll glides the viewport over.
        if self.animate {
            self.state.land_scroll();
        }
        self.arrange();
    }

    /// Accumulated horizontal scroll (in wheel-click units) pans the
    /// canvas. Carries the sub-pixel remainder between events: a slow
    /// continuous swipe can deliver less than a pixel per event, and
    /// truncating each independently would discard the whole gesture.
    pub fn apply_hscroll(&mut self, delta: f64) {
        let wa = self.layout_area();
        let px_f = delta.mul_add(f64::from(theme::SCROLL_STEP), self.hscroll_frac);
        let px = px_f as i32;
        self.hscroll_frac = px_f - f64::from(px);
        if px == 0 {
            return;
        }
        // scroll_delta only moves the target; step_scroll (redraw tick)
        // glides scroll_x toward it frame by frame, so a fast swipe keeps
        // re-aiming a moving target instead of jumping.
        self.state.scroll_delta(wa, px);
        self.arrange();
    }

    /// Whether a swipe pans the canvas: always over the chrome (gaps,
    /// taskbar, empty splits), only with Mod4 held over a client window —
    /// so a swipe doesn't fight an app's own horizontal scrolling.
    pub fn hscroll_allowed(&self, over_client: bool) -> bool {
        if !over_client {
            return true;
        }
        self.seat
            .get_keyboard()
            .is_some_and(|k| k.modifier_state().logo)
    }
}
