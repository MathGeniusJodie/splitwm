//! Pointer semantics on the chrome: the priority-ordered hit-test, click
//! dispatch, gap/edge/float/split-move drags, and canvas panning. The
//! surface under the pointer and the modifier state are already ours — no
//! server round-trips, no caching. The shared layout commands the clicks
//! invoke live in `comp::actions`.

use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point};

use super::Comp;
use crate::layout::{GapAt, NodeId, Pos, Side, Win};
use crate::state::MoveDrop;
use crate::theme;
use crate::widgets::{compass_rect, compass_zone, rect_contains, BtnKind};

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

/// Relocating a split, grabbed by its titlebar or its taskbar tile. The
/// drop lands on release (`Comp::end_drag`): onto the left/right half of
/// another split's frame or taskbar tile, placing the dragged split
/// before/after it.
#[derive(Clone, Copy)]
pub enum MoveDrag {
    /// Pressed but inert until the pointer travels `MOVE_DRAG_THRESHOLD`
    /// from `press` — a plain click must stay a click.
    Armed { leaf: NodeId, press: (i32, i32) },
    /// Past the threshold: motion is consumed and release drops the split.
    Active { leaf: NodeId },
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

/// Dragging the gap between two stacked rows: it re-splits the pair,
/// fraction = (pointer - start) / combined extent of the two rows. Only
/// a *stack* gap drags this way — each half of a column gap belongs to
/// the window on that side and resizes that column (`BorderDrag`), so a
/// column boundary can never reach here.
#[derive(Clone, Copy)]
pub struct GapDrag {
    pub col: usize,
    pub idx: usize,
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

/// The quick-launch hover compass currently on screen: the tile it
/// belongs to (its icon rect centres the compass, its hover rect keeps it
/// up) — re-resolved per read, never stored.
pub type QuickCompass = crate::widgets::QuickTile;

/// What a click on the chrome resolved to, in priority order.
enum Hit {
    Btn(NodeId, BtnKind),
    TaskbarClose(Win),
    TaskbarTile(Win, NodeId),
    /// A quick-launch icon, with the compass wedge the press landed in:
    /// every point of the compass names one, so a launch from the bar
    /// always states where its window opens.
    QuickLaunch(usize, Side),
    Title(NodeId),
    /// The gap between two stacked rows, with the drag it arms and
    /// whether that drag can move anything (a pinned neighbour's height
    /// ignores the stored share).
    RowGap(GapDrag, bool),
    Edge(bool),
    /// A window frame's left (`true`) / right border band.
    Border(NodeId, bool),
    LeafBody(NodeId),
    Miss,
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
            let hit = self.windows.float_stack.iter().copied().find(|&fw| {
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
                self.interaction.drag = Some(ActiveDrag::Float(FloatDrag { win: fw, dx, dy }));
                self.focus_float(fw);
                return true;
            }
        }
        // Hit regions describe the final layout, but an animation may still
        // be drawing chrome mid-slide; snap it so the click lands on what
        // the user sees.
        if self.view.anim.is_some() {
            self.finish_animation();
        }
        match self.hit_test(mx, my) {
            _ if secondary => {}
            Hit::Btn(leaf, kind) => self.click_split_button(leaf, kind),
            Hit::TaskbarClose(win) => self.close_client(win),
            // Press = click (focus + scroll the split into view); further
            // travel turns it into a split-move drag, dropped on release.
            Hit::TaskbarTile(win, leaf) => {
                // Focusing a split by tile/title/body is a deliberate
                // focus move: it reclaims the keyboard from a clicked
                // layer panel.
                self.windows.focused_layer = None;
                self.bring_into_layout(win, true);
                // Armed after `bring_into_layout`: its `commit_layout`
                // clears `self.interaction.drag`.
                self.arm_move_drag(leaf, mx, my);
            }
            Hit::QuickLaunch(i, side) => {
                if let Some(cmd) = self.view.quick.get(i).map(|q| q.cmd.clone()) {
                    // The wedge only aims: the split appears when the
                    // launched window maps, on the side clicked.
                    self.state.aim_next_window(side);
                    self.spawn(&cmd);
                }
            }
            Hit::Title(leaf) => {
                self.windows.focused_layer = None;
                self.state.focus_leaf(leaf);
                self.arrange();
                self.arm_move_drag(leaf, mx, my);
            }
            Hit::LeafBody(leaf) => {
                self.windows.focused_layer = None;
                self.state.focus_leaf(leaf);
                self.arrange();
            }
            Hit::RowGap(drag, resizable) => {
                // A gap next to a minimized leaf can't be dragged (its
                // pixel size is pinned); ignore the press.
                if resizable {
                    self.interaction.drag = Some(ActiveDrag::Gap(drag));
                }
            }
            Hit::Edge(left) => {
                self.state.land_scroll();
                let wa = self.layout_area();
                if let Some((start_x, w)) = self.state.edge_span(wa, left) {
                    let canvas_anchor = if left { start_x + w } else { start_x };
                    let anchor_x = canvas_anchor - self.state.scroll_x();
                    self.interaction.drag = Some(ActiveDrag::Edge(EdgeDrag { left, anchor_x }));
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
                    self.interaction.drag = Some(ActiveDrag::Border(BorderDrag {
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
        match self.interaction.drag {
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
                // A stack lays out down the column's height, which doesn't
                // scroll: screen y is canvas y.
                let new_first = pos.y as i32 - d.start - d.gap / 2;
                self.state.resize_rows(d.col, d.idx, new_first, d.combined);
                self.arrange();
                true
            }
            Some(ActiveDrag::Move(md)) => match md {
                MoveDrag::Armed { leaf, press } => {
                    let (mx, my) = (pos.x as i32, pos.y as i32);
                    let travelled =
                        (mx - press.0).abs().max((my - press.1).abs()) >= MOVE_DRAG_THRESHOLD;
                    if travelled {
                        self.interaction.drag = Some(ActiveDrag::Move(MoveDrag::Active { leaf }));
                    }
                    travelled
                }
                MoveDrag::Active { .. } => true,
            },
            None => false,
        }
    }

    /// Button release: an active split-move drag drops here. Every other
    /// drag (and a still-armed move, i.e. a click) just ends.
    pub fn end_drag(&mut self, pos: Point<f64, Logical>) {
        let drag = self.interaction.drag.take();
        let Some(ActiveDrag::Move(MoveDrag::Active { leaf })) = drag else {
            return;
        };
        if !self.state.layout.is_leaf(leaf) {
            return;
        }
        let (mx, my) = (pos.x as i32, pos.y as i32);
        let Some(drop) = self.move_drop_target(mx, my) else {
            return;
        };
        let wa = self.layout_area();
        let changed = match drop {
            MoveDrop::ColumnAt(idx) => self.state.move_leaf_to_column(wa, leaf, idx),
            MoveDrop::Column(dst, before) => self.state.move_leaf_beside(wa, leaf, dst, before),
            MoveDrop::Stack(dst, before) => self.state.move_leaf_into_stack(leaf, dst, before),
        };
        if changed {
            self.view.animate = true;
            self.commit_layout();
        }
    }

    /// Where a split-move drop at (`mx`, `my`) lands, by what's under the
    /// pointer: a *gap* adopts the gap's own orientation — a vertical gap
    /// takes the dragged split out of wherever it was and makes it a new
    /// column right there, a horizontal gap slots it into that stack.
    /// Anywhere over the taskbar's tile strip re-slots by tile centres —
    /// before the first tile whose centre lies right of the pointer,
    /// after the last one otherwise — so the gaps between tiles and the
    /// strip's ends take drops too, and a drop inside a tile keeps the
    /// left-half-before / right-half-after rule. A split frame places it
    /// as a column before/after the target's (split down the middle),
    /// using the same last-arrange rects `LeafBody` hits. Bare wallpaper
    /// — the canvas beyond the strip's ends, and the margins above and
    /// below it — is the strip's own margin (`State::margin_drop`), so
    /// every point of the canvas takes a drop.
    fn move_drop_target(&self, mx: i32, my: i32) -> Option<MoveDrop> {
        if let Some(&(_, b)) = self
            .view
            .widgets
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            return match b.at {
                // The gap between columns `idx` and `idx + 1` is where a
                // new column `idx + 1` goes.
                GapAt::Col(idx) => Some(MoveDrop::ColumnAt(idx + 1)),
                GapAt::Row { col, idx } => {
                    let dst = self.state.layout.leaf_at(Pos { col, row: idx })?;
                    Some(MoveDrop::Stack(dst, false))
                }
            };
        }
        if my >= self.output_size().h - theme::TASKBAR_H {
            // The strip ends where the quick-launch separator starts; a
            // drop over the quick icons means nothing.
            let strip_end = self.view.widgets.taskbar_sep.map_or(i32::MAX, |s| s.x);
            let tiles = &self.view.widgets.taskbar_regions;
            let dst = tiles
                .iter()
                .find(|t| mx < t.rect.x + t.rect.w / 2)
                .map(|t| MoveDrop::Column(t.leaf, true))
                .or_else(|| tiles.last().map(|t| MoveDrop::Column(t.leaf, false)));
            return dst.filter(|_| mx < strip_end);
        }
        if let Some(p) = self
            .view
            .placed
            .iter()
            .find(|p| rect_contains(p.target, mx, my))
        {
            return Some(MoveDrop::Column(p.leaf, mx < p.target.x + p.target.w / 2));
        }
        self.state.margin_drop(self.layout_area(), mx, my)
    }

    /// Arm a split-move drag on a fresh titlebar/tile press (see
    /// `MoveDrag`).
    fn arm_move_drag(&mut self, leaf: NodeId, mx: i32, my: i32) {
        self.interaction.drag = Some(ActiveDrag::Move(MoveDrag::Armed {
            leaf,
            press: (mx, my),
        }));
    }

    /// The topmost float whose chrome frame band (frame rect minus client
    /// rect) contains `pos`. Frames overlap whatever lies beneath them, so
    /// the button handler must check this before surface routing — the
    /// press would otherwise fall through to the client underneath.
    pub fn float_frame_at(&self, pos: Point<f64, Logical>) -> Option<Win> {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        self.windows.float_stack.iter().copied().find(|&fw| {
            self.managed.float(fw).is_some_and(|(_, f)| {
                rect_contains(f.frame_rect(), mx, my)
                    && !(mx >= f.x && mx < f.x + f.w && my >= f.y && my < f.y + f.h)
            })
        })
    }

    /// The compass to draw and hit-test, re-resolved from this arrange's
    /// quick-launch regions.
    pub fn quick_compass(&self) -> Option<QuickCompass> {
        let slot = self.interaction.quick_hover?;
        self.view
            .widgets
            .quick_regions
            .iter()
            .find(|t| t.slot == slot)
            .copied()
    }

    /// Which wedge of the shown compass the pointer is in, for the
    /// drawing pass; `None` only when no compass is up.
    pub fn quick_compass_zone(&self) -> Option<Side> {
        let compass = self.quick_compass()?;
        let pos = self.pointer.current_location();
        Some(compass_zone(compass.icon, pos.x as i32, pos.y as i32))
    }

    /// Re-aim the compass at the pointer: it appears as soon as the
    /// pointer is over a quick-launch icon's hover rect (they tile the
    /// bar's quick-launch run, so there is no neutral spot between two
    /// icons) and stays while the pointer roams that icon's own compass —
    /// the wedges reach well past the bar, so leaving it must not dismiss
    /// what the user is aiming at. Entering a neighbouring icon's rect
    /// hands the compass straight over.
    pub fn update_quick_hover(&mut self, pos: Point<f64, Logical>) {
        let (mx, my) = (pos.x as i32, pos.y as i32);
        let entered = self
            .view
            .widgets
            .quick_regions
            .iter()
            .find(|t| rect_contains(t.hover, mx, my))
            .map(|t| t.slot);
        let held = self
            .quick_compass()
            .filter(|c| rect_contains(compass_rect(c.icon), mx, my))
            .map(|c| c.slot);
        self.interaction.quick_hover = entered.or(held);
    }

    /// The split of column `col` at screen-y `my`: which window a press in
    /// the gap flanking that column is aiming at. A `my` between two rows
    /// (the stack's own gap) falls back to the column's first row, so
    /// every point of the gap names a window.
    fn column_leaf_at(&self, col: usize, my: i32) -> Option<NodeId> {
        let first = self.state.layout.leaf_at(Pos { col, row: 0 })?;
        for row in 0..self.state.layout.col_len(col) {
            let Some(leaf) = self.state.layout.leaf_at(Pos { col, row }) else {
                continue;
            };
            if self
                .view
                .frame_rects
                .get(&leaf)
                .is_some_and(|r| my >= r.y && my < r.y + r.h)
            {
                return Some(leaf);
            }
        }
        Some(first)
    }

    /// Priority-ordered hit-test of everything clickable on the chrome,
    /// shared by `on_chrome_button` (dispatch) and `hover_cursor`
    /// (feedback) — a single ordering both consume, so click handling and
    /// hover feedback can never drift apart (master's invariant).
    fn hit_test(&self, mx: i32, my: i32) -> Hit {
        if let Some((leaf, kind)) = self
            .view
            .widgets
            .btn_regions
            .iter()
            .find(|(r, _, _)| rect_contains(*r, mx, my))
            .map(|(_, l, k)| (*l, *k))
        {
            return Hit::Btn(leaf, kind);
        }
        // The hovered quick-launch icon's compass is drawn over whatever
        // it overlaps (tiles, the separator, its neighbours) and over the
        // icon in its own middle, so it takes the click everywhere it
        // reaches — its own hover rect included, which the wedges don't
        // quite cover at the bar's edges. Only the hovered slot has one,
        // so this can never shadow another icon.
        if let Some(c) = self.quick_compass() {
            let square = compass_rect(c.icon);
            if rect_contains(square, mx, my) || rect_contains(c.hover, mx, my) {
                return Hit::QuickLaunch(c.slot, compass_zone(square, mx, my));
            }
        }
        // Compressed taskbar tiles overlap like fanned cards, rightmost on
        // top; reverse iteration matches draw order so the topmost tile
        // wins. Each tile's own "x" badge outranks its body, but a later
        // (higher) tile's body outranks an earlier badge it covers — the
        // hit order is exactly the paint order, reversed.
        for t in self.view.widgets.taskbar_regions.iter().rev() {
            if rect_contains(t.close, mx, my) {
                return Hit::TaskbarClose(t.win);
            }
            if rect_contains(t.rect, mx, my) {
                return Hit::TaskbarTile(t.win, t.leaf);
            }
        }
        // A press on an icon whose compass has not come up yet (no motion
        // preceded it) still picks the wedge its point falls in.
        if let Some(t) = self
            .view
            .widgets
            .quick_regions
            .iter()
            .find(|t| rect_contains(t.hover, mx, my))
        {
            return Hit::QuickLaunch(t.slot, compass_zone(t.icon, mx, my));
        }
        if let Some(leaf) = self
            .view
            .widgets
            .title_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, l)| *l)
        {
            return Hit::Title(leaf);
        }
        if let Some((rect, b)) = self
            .view
            .widgets
            .handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(r, b)| (*r, *b))
        {
            return match b.at {
                // The gap between two columns belongs to the windows
                // either side of it: its left half drags the left
                // column's right border, its right half the right
                // column's left border — the same drag their own border
                // bands arm, so band and gap-half read as one strip with
                // nothing neutral between them.
                GapAt::Col(idx) => {
                    let right = mx >= rect.x + rect.w / 2;
                    let col = if right { idx + 1 } else { idx };
                    match self.column_leaf_at(col, my) {
                        Some(leaf) => Hit::Border(leaf, right),
                        None => Hit::Miss,
                    }
                }
                GapAt::Row { col, idx } => Hit::RowGap(
                    GapDrag {
                        col,
                        idx,
                        start: b.start,
                        combined: b.first + b.second,
                        gap: theme::GAP,
                    },
                    b.resizable,
                ),
            };
        }
        if let Some(&(_, left)) = self
            .view
            .widgets
            .edge_handle_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
        {
            return Hit::Edge(left);
        }
        if let Some((leaf, frame)) = self
            .view
            .frame_rects
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
            Hit::Btn(..)
            | Hit::TaskbarClose(_)
            | Hit::TaskbarTile(..)
            | Hit::QuickLaunch(..)
            | Hit::Title(_) => CursorIcon::Pointer,
            // A gap next to a minimized leaf can't be dragged; don't
            // advertise a resize that won't happen.
            Hit::RowGap(_, resizable) => {
                if resizable {
                    CursorIcon::NsResize
                } else {
                    CursorIcon::Default
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

    /// The tiled window keystrokes land in for the current pointer
    /// position — keyboard delivery follows the mouse, not the focus
    /// outline. Within the focused column's horizontal span the focused
    /// window wins even when the pointer sits above or below it (a
    /// stacked sibling, a gap, the taskbar): a keyboard focus move
    /// inside a stack can't scroll its destination under the pointer,
    /// so the column is the hover unit for the focused split.
    /// Everywhere else the pointer must be inside a window's frame
    /// (border and titlebar included); over wallpaper, gaps, or bare
    /// chrome nothing takes the keyboard. A window's popups count as the
    /// window: hovering a menu or tooltip is hovering the app it belongs
    /// to. Floats, the dock, and layer surfaces sit above the tiled
    /// plane and are opaque to hover: their keyboard focus stays
    /// click-driven (`keyboard_override`, `focused_layer`), and typing
    /// through them into a covered window would be typing into
    /// something the pointer visibly isn't in. An override-redirect X11
    /// window is also some app's menu or tooltip, but with no protocol
    /// link back to it — hover holds still there instead: clients roll
    /// their menus up the instant the parent loses focus, so the
    /// keyboard must not move at all.
    pub fn hover_target(&self) -> Option<Win> {
        // A fullscreen client covers the output; the pointer is in it
        // wherever it is.
        if let Some(fs) = self.fullscreen() {
            return Some(fs);
        }
        let pos = self.pointer.current_location();
        if self.float_frame_at(pos).is_some() {
            return None;
        }
        // A surface hit resolves plane opacity (floats, dock, o-r
        // windows, layer surfaces) and catches a tiled window's popups
        // reaching outside its frame.
        let direct = match self.surface_under(pos) {
            Some((s, _)) => {
                // A popup's surface is not its window's root surface;
                // resolve it through the popup tree to the toplevel
                // that owns it.
                let root = self
                    .popups
                    .find_popup(&s)
                    .and_then(|k| smithay::desktop::find_popup_root_surface(&k).ok())
                    .unwrap_or_else(|| s.clone());
                let win = self.managed.win_for_surface(&root);
                match win.and_then(|w| self.managed.kind_of(w)) {
                    Some(crate::shell::Kind::Tiled) => win,
                    Some(_) => return None,
                    None if self
                        .or_windows
                        .iter()
                        .any(|o| o.surface.wl_surface().as_ref() == Some(&s)) =>
                    {
                        return self.interaction.hover_win;
                    }
                    None => return None,
                }
            }
            None => None,
        };
        let (mx, my) = (pos.x as i32, pos.y as i32);
        if let (Some(&frame), Some(client)) = (
            self.state
                .focused_leaf_valid()
                .and_then(|l| self.view.frame_rects.get(&l)),
            self.state.focused_client(),
        ) {
            if mx >= frame.x && mx < frame.x + frame.w {
                return Some(client);
            }
        }
        if direct.is_some() {
            return direct;
        }
        // Frame rects cover the border and titlebar the surface
        // hit-test misses; a minimized leaf shows no window, so its
        // frame (a restore button) delivers to nothing.
        self.view
            .frame_rects
            .iter()
            .find(|(l, r)| self.state.layout.is_leaf(**l) && rect_contains(**r, mx, my))
            .and_then(|(l, _)| self.state.layout.leaf(*l))
            .and_then(|leaf| (!leaf.minimized).then_some(leaf.client))
    }

    /// Act on a split-control button click: close politely closes the
    /// leaf's window (the split follows on its death, so a "do you want to
    /// save?" refusal keeps it), minimize collapses the leaf to its
    /// restore strip.
    pub fn click_split_button(&mut self, leaf: NodeId, kind: BtnKind) {
        match kind {
            BtnKind::Close => {
                if let Some(win) = self.state.layout.leaf(leaf).map(|l| l.client) {
                    self.close_client(win);
                }
            }
            BtnKind::Minimize => {
                self.view.animate = self.state.toggle_minimize(leaf);
                self.commit_layout();
            }
        }
    }

    /// Accumulated horizontal scroll (in wheel-click units) pans the
    /// canvas. Carries the sub-pixel remainder between events: a slow
    /// continuous swipe can deliver less than a pixel per event, and
    /// truncating each independently would discard the whole gesture.
    pub fn apply_hscroll(&mut self, delta: f64) {
        let wa = self.layout_area();
        let px_f = delta.mul_add(f64::from(theme::SCROLL_STEP), self.interaction.hscroll_frac);
        let px = px_f as i32;
        self.interaction.hscroll_frac = px_f - f64::from(px);
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
