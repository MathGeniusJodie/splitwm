//! Pointer semantics on the chrome: the priority-ordered hit-test, click
//! dispatch, gap/edge drags, canvas panning, and the shared layout-commit
//! epilogue. Ported from master's `wm/input/pointer.rs` + `scroll.rs`;
//! floats join in M6. Unlike X11, the surface under the pointer and the
//! modifier state are already ours — no server round-trips, no caching.

use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point};

use super::Comp;
use crate::layout::{Boundary, Dir, GapAt, Insert, NodeId, Win};
use crate::state::Activation;
use crate::theme;
use crate::widgets::{leaf_meta, BtnKind, FrameRect};

/// An in-progress drag, keyed off button-1 press on a handle/edge/float
/// frame, a titlebar, or a taskbar tile.
#[derive(Clone, Copy)]
pub enum ActiveDrag {
    Gap(GapDrag),
    Edge(EdgeDrag),
    Border(BorderDrag),
    Float(FloatDrag),
    Move(MoveDrag),
}

/// Relocating a split, grabbed by its titlebar or its taskbar tile. Armed
/// on press but inert until the pointer travels `MOVE_DRAG_THRESHOLD` from
/// `press` — a plain click must stay a click. The drop lands on release
/// (`Comp::end_drag`): onto the left/right half of another split's frame or
/// taskbar tile, placing the dragged split before/after it.
#[derive(Clone, Copy)]
pub struct MoveDrag {
    pub leaf: NodeId,
    pub press: (i32, i32),
    pub active: bool,
}

/// Pointer travel (in px, Chebyshev) before a titlebar/tile press becomes a
/// split-move drag rather than a click.
const MOVE_DRAG_THRESHOLD: i32 = 8;

/// Moving a float by its chrome frame: the pointer's offset into the
/// client rect is pinned for the whole gesture.
#[derive(Clone, Copy)]
pub struct FloatDrag {
    pub win: Win,
    pub dx: i32,
    pub dy: i32,
}

/// Dragging a gap between two columns or two stacked rows. A column gap
/// sets the left column's width outright; a row gap re-splits the pair,
/// fraction = (pointer - start) / combined extent of the two rows.
#[derive(Clone, Copy)]
pub struct GapDrag {
    pub at: GapAt,
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

/// Dragging a window frame's left/right border band: only that column
/// resizes — the strip grows/shrinks and siblings slide, unlike a gap
/// drag, which moves the shared boundary between the pair. The column's
/// far edge stays fixed at `anchor_x` (screen space), like an edge drag.
#[derive(Clone, Copy)]
pub struct BorderDrag {
    pub leaf: NodeId,
    pub left: bool,
    pub anchor_x: i32,
}

/// What a click on the chrome resolved to, in priority order.
enum Hit {
    Btn(NodeId, BtnKind),
    TaskbarClose(Win),
    TaskbarTile(Win, NodeId),
    QuickLaunch(usize),
    Title(NodeId),
    Plus(Insert),
    Handle(Boundary),
    Edge(bool),
    /// A window frame's left (`true`) / right border band.
    Border(NodeId, bool),
    LeafBody(NodeId),
    Miss,
}

const fn rect_contains(r: FrameRect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// Where a split-move drop resolved to: a new column beside `dst`'s, or a
/// row of `dst`'s stack (`bool` is before/above).
enum MoveDrop {
    Column(NodeId, bool),
    Stack(NodeId, bool),
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
            // The tile badge closes only the window: its split stays behind
            // as an empty placeholder (the titlebar close takes both).
            Hit::TaskbarClose(win) => {
                self.state.retain_split_on_close(win);
                self.close_client(win);
            }
            // Press = click (focus + scroll the split into view); further
            // travel turns it into a split-move drag, dropped on release.
            Hit::TaskbarTile(win, leaf) => {
                self.bring_into_layout(win, true);
                // Armed after `bring_into_layout`: its `commit_layout`
                // clears `self.drag`.
                self.arm_move_drag(leaf, mx, my);
            }
            Hit::QuickLaunch(i) => {
                if let Some(cmd) = self.quick.get(i).map(|q| q.cmd.clone()) {
                    self.spawn(&cmd);
                }
            }
            Hit::Title(leaf) => {
                self.state.focus_leaf(leaf);
                self.arrange();
                self.arm_move_drag(leaf, mx, my);
            }
            Hit::LeafBody(leaf) => {
                self.state.focus_leaf(leaf);
                self.arrange();
            }
            Hit::Plus(at) => {
                let wa = self.layout_area();
                self.state.insert_at(wa, at);
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
                    self.drag = Some(ActiveDrag::Gap(GapDrag {
                        at: b.at,
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
            Hit::Border(leaf, left) => {
                self.state.focus_leaf(leaf);
                self.state.land_scroll();
                let wa = self.layout_area();
                if let Some(&geo) = self.state.compute(wa).get(&leaf) {
                    // Row geometry spans its whole column, so geo's x-span
                    // is the column's regardless of stacking.
                    let canvas_anchor = if left { geo.x + geo.w } else { geo.x };
                    let anchor_x = canvas_anchor - self.state.scroll_x();
                    self.drag = Some(ActiveDrag::Border(BorderDrag {
                        leaf,
                        left,
                        anchor_x,
                    }));
                }
                self.arrange();
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
            Some(ActiveDrag::Border(bd)) => {
                let wa = self.layout_area();
                let mouse_x = pos.x as i32;
                // anchor_x is the fixed far edge, so the gap to the mouse
                // *is* the target width — width is scroll-invariant.
                let target_w = if bd.left {
                    bd.anchor_x - mouse_x
                } else {
                    mouse_x - bd.anchor_x
                };
                if let Some(pos) = self.state.layout.locate(bd.leaf) {
                    let applied = self.state.resize_col(wa, pos.col, target_w);
                    // Growing the column shifts every later column right in
                    // canvas space; scroll by the same amount so the anchor
                    // edge and everything right of it stay put and only the
                    // dragged border (and columns left of it) visibly move.
                    if bd.left && applied != 0 {
                        self.state.shift_scroll(applied);
                    }
                    self.arrange();
                }
                true
            }
            Some(ActiveDrag::Gap(d)) => {
                if d.combined <= 0 {
                    return true;
                }
                // Only x scrolls; a row-boundary drag reads y directly.
                let canvas_pos = match d.at.dir() {
                    Dir::V => pos.y as i32,
                    Dir::H => pos.x as i32 + self.state.scroll_x(),
                };
                let new_first = canvas_pos - d.start - d.gap / 2;
                self.state.resize_gap(d.at, new_first, d.combined);
                self.arrange();
                true
            }
            Some(ActiveDrag::Move(mut md)) => {
                let (mx, my) = (pos.x as i32, pos.y as i32);
                if !md.active
                    && (mx - md.press.0).abs().max((my - md.press.1).abs()) >= MOVE_DRAG_THRESHOLD
                {
                    md.active = true;
                    self.drag = Some(ActiveDrag::Move(md));
                }
                md.active
            }
            None => false,
        }
    }

    /// Button release: an active split-move drag drops here. Every other
    /// drag (and an un-armed move, i.e. a click) just ends.
    pub fn end_drag(&mut self, pos: Point<f64, Logical>) {
        let drag = self.drag.take();
        let Some(ActiveDrag::Move(md)) = drag else {
            return;
        };
        if !md.active || !self.state.layout.is_leaf(md.leaf) {
            return;
        }
        let (mx, my) = (pos.x as i32, pos.y as i32);
        let Some(drop) = self.move_drop_target(mx, my) else {
            return;
        };
        let wa = self.layout_area();
        let changed = match drop {
            MoveDrop::Column(dst, before) => self.state.move_leaf_beside(wa, md.leaf, dst, before),
            MoveDrop::Stack(dst, before) => self.state.move_leaf_into_stack(md.leaf, dst, before),
        };
        if changed {
            self.animate = true;
            self.commit_layout();
        }
    }

    /// Where a split-move drop at (`mx`, `my`) lands, by what's under the
    /// pointer: a *gap* adopts the gap's own orientation — a vertical gap
    /// makes the dragged split a new column right there, a horizontal gap
    /// slots it into that stack. Anywhere over the taskbar's tile strip
    /// re-slots by tile centres — before the first tile whose centre lies
    /// right of the pointer, after the last one otherwise — so the gaps
    /// between tiles and the strip's ends take drops too, and a drop
    /// inside a tile keeps the left-half-before / right-half-after rule.
    /// A split frame places it as a column before/after the target's
    /// (split down the middle), using the same last-arrange rects
    /// `LeafBody` hits.
    fn move_drop_target(&self, mx: i32, my: i32) -> Option<MoveDrop> {
        if let Some(&(_, b)) = self
            .widgets
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            let anchor = |pos| self.state.layout.leaf_at(pos);
            return match b.at {
                GapAt::Col(idx) => {
                    let dst = anchor(crate::layout::Pos { col: idx, row: 0 })?;
                    Some(MoveDrop::Column(dst, false))
                }
                GapAt::Row { col, idx } => {
                    let dst = anchor(crate::layout::Pos { col, row: idx })?;
                    Some(MoveDrop::Stack(dst, false))
                }
            };
        }
        if my >= self.output_size().h - theme::TASKBAR_H {
            // The strip ends where the quick-launch separator starts; a
            // drop over the quick icons means nothing.
            let strip_end = self.widgets.taskbar_sep.map_or(i32::MAX, |s| s.x);
            let tiles = &self.widgets.taskbar_regions;
            let dst = tiles
                .iter()
                .find(|t| mx < t.rect.x + t.rect.w / 2)
                .map(|t| MoveDrop::Column(t.leaf, true))
                .or_else(|| tiles.last().map(|t| MoveDrop::Column(t.leaf, false)));
            return dst.filter(|_| mx < strip_end);
        }
        self.placed
            .iter()
            .find(|p| rect_contains(p.target, mx, my))
            .map(|p| MoveDrop::Column(p.leaf, mx < p.target.x + p.target.w / 2))
    }

    /// Arm a split-move drag on a fresh titlebar/tile press (see
    /// `MoveDrag`).
    fn arm_move_drag(&mut self, leaf: NodeId, mx: i32, my: i32) {
        self.drag = Some(ActiveDrag::Move(MoveDrag {
            leaf,
            press: (mx, my),
            active: false,
        }));
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
        if let Some((win, leaf)) = self
            .widgets
            .taskbar_regions
            .iter()
            .rev()
            .find(|t| rect_contains(t.rect, mx, my))
            .map(|t| (t.win, t.leaf))
        {
            return Hit::TaskbarTile(win, leaf);
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
        if let Some((leaf, frame)) = self
            .prev_frame_rect
            .iter()
            .find(|(l, r)| self.state.layout.is_leaf(**l) && rect_contains(**r, mx, my))
            .map(|(l, r)| (*l, *r))
        {
            // The frame's side border bands resize the column; anything
            // else on the frame (a minimized leaf's whole frame is a
            // button, caught above) is a plain body click.
            if mx < frame.x + theme::BORDER_LEFT {
                return Hit::Border(leaf, true);
            }
            if mx >= frame.x + frame.w - theme::BORDER_RIGHT {
                return Hit::Border(leaf, false);
            }
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
                // Mirror `leaf_buttons`' enabled/disabled choice for the
                // button art (a minimized leaf's whole-frame region is
                // always a live restore button).
                if let Some(&frame) = self.prev_frame_rect.get(&leaf) {
                    let meta = leaf_meta(&self.state.layout, leaf, frame);
                    let disabled = !meta.minimized
                        && match kind {
                            BtnKind::Close => !meta.occupied && meta.sole,
                            BtnKind::Minimize => meta.sole,
                            BtnKind::Split => meta.split_dir.is_none(),
                        };
                    if disabled {
                        return CursorIcon::NotAllowed;
                    }
                }
                CursorIcon::Pointer
            }
            Hit::TaskbarClose(_)
            | Hit::TaskbarTile(..)
            | Hit::QuickLaunch(_)
            | Hit::Title(_)
            | Hit::Plus(_) => CursorIcon::Pointer,
            Hit::Handle(b) => {
                // A gap next to a minimized leaf can't be dragged; don't
                // advertise a resize that won't happen.
                if !b.resizable {
                    CursorIcon::Default
                } else if b.at.dir() == Dir::V {
                    CursorIcon::NsResize
                } else {
                    CursorIcon::EwResize
                }
            }
            Hit::Edge(_) => CursorIcon::EwResize,
            Hit::Border(leaf, _) => {
                // A pinned (all-minimized) column refuses the resize; its
                // frame is a restore button anyway, so this is belt and
                // braces for the empty-band edges.
                match self.state.layout.locate(leaf) {
                    Some(p) if !self.state.layout.col_pinned(p.col) => CursorIcon::EwResize,
                    _ => CursorIcon::Default,
                }
            }
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
        let meta = leaf_meta(&self.state.layout, leaf, frame);
        match kind {
            BtnKind::Split => {
                // Right-click flips the advertised action where the
                // flipped one is possible: a column insert always is, a
                // stack insert needs the frame's height.
                let dir = if secondary {
                    match meta.split_dir {
                        Some(Dir::H) => theme::stack_fits(frame.h).then_some(Dir::V),
                        Some(Dir::V) => Some(Dir::H),
                        // A disabled button (advertised NotAllowed) stays
                        // inert on right-click too.
                        None => None,
                    }
                } else {
                    meta.split_dir
                };
                self.state.focus_leaf(leaf);
                self.animate = match dir {
                    Some(Dir::H) => {
                        self.state.split_column_right(wa);
                        true
                    }
                    Some(Dir::V) => self.state.split_focused(),
                    None => return,
                };
            }
            BtnKind::Close => {
                return self.close_split(leaf);
            }
            BtnKind::Minimize => {
                if meta.sole {
                    return;
                }
                self.animate = self.state.toggle_minimize(leaf);
            }
        }
        self.commit_layout();
    }

    /// Close the split at `leaf` — the titlebar close button's and
    /// `Action::Close`'s shared semantics. Window and split live and die
    /// together: an occupied split's close politely closes the window, and
    /// the split collapses when it actually dies (`unpin_client` — so a
    /// "do you want to save?" refusal keeps the split). An empty
    /// placeholder is removed on the spot; the sole placeholder is the one
    /// split that can't go.
    pub fn close_split(&mut self, leaf: NodeId) {
        match self.state.layout.leaf(leaf).and_then(|l| l.client) {
            Some(win) => self.close_client(win),
            None => self.animate = self.state.remove_empty_leaf(leaf),
        }
        self.commit_layout();
    }

    /// Focus a managed tiled window's split and scroll it into view (via
    /// `commit_layout`'s `ensure_in_view`), un-minimizing it. `animate`
    /// requests a transition, but only when rects actually moved.
    pub fn bring_into_layout(&mut self, win: Win, animate: bool) {
        let changed = match self.state.activate_client(win) {
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
        self.keyboard.modifier_state().logo
    }
}
