//! Pointer input: click dispatch, hit-testing, gap/float/edge drags, and
//! hover-cursor feedback. `hit_test` is the single priority-ordered lookup
//! shared by click dispatch (`on_button`) and cursor feedback
//! (`hover_cursor`), so the two can never disagree about what's under the
//! pointer.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Allow, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux, ConnectionExt,
    InputFocus, MotionNotifyEvent,
};

use super::super::types::{rect_contains, Wm, WindowKind, R};
use super::super::widgets::BtnKind;
use crate::theme;
use crate::tree::{Boundary, Dir, NodeId, Win};

/// In-progress gap/float/edge drags.
pub struct DragState {
    pub active: Option<ActiveDrag>,
}

/// A gap resize, float move, or edge resize in progress. Button 1 can only
/// ever be doing one of these at once, so they live behind a single
/// `Option` rather than three independent ones — every caller already
/// assumes mutual exclusion, so the type enforces what the logic requires.
#[derive(Clone, Copy)]
pub enum ActiveDrag {
    Split(SplitDrag),
    /// An in-progress float move, started by pressing button 1 on a float's
    /// frame window.
    Float(FloatDrag),
    Edge(EdgeDrag),
}

/// An in-progress float move: dragging a float's frame repositions the
/// frame + client pair. `dx`/`dy` are the pointer's offset from the client
/// window's origin at press time, so the window tracks the grab point.
#[derive(Clone, Copy)]
pub struct FloatDrag {
    pub win: Win,
    pub dx: i32,
    pub dy: i32,
}

/// An in-progress gap resize started by dragging a handle.
#[derive(Clone, Copy)]
pub struct SplitDrag {
    pub parent: NodeId,
    pub idx: usize,
    /// True when a horizontal gap (between stacked rows) is being dragged
    /// along y; false for a vertical gap dragged along x.
    pub vertical: bool,
    /// First (left/top) child's start along the drag axis, canvas-space.
    pub start: i32,
    pub combined: i32,
    pub gap: i32,
}

/// An in-progress edge-of-canvas resize, started by dragging the handle at
/// the canvas's outer left/right margin (see `State::resize_edge`).
#[derive(Clone, Copy)]
pub struct EdgeDrag {
    pub left: bool,
    /// Screen-space x of the resized column's *far* edge (the one not
    /// being dragged), fixed for the whole gesture — the mouse's distance
    /// from it is the column's new width directly, no scroll conversion
    /// needed.
    pub anchor_x: i32,
}

/// The pointer cursors the WM ever shows, created once at startup, plus the
/// one currently set on the underlay (so hover motion only issues a
/// `ChangeWindowAttributes` when it actually changes).
#[derive(Clone, Copy)]
pub struct Cursors {
    pub arrow: u32,
    /// Left/right double arrow, over vertical-gap and canvas-edge handles.
    pub h_resize: u32,
    /// Up/down double arrow, over horizontal-gap handles.
    pub v_resize: u32,
    /// Shown over disabled titlebar buttons (the hand-drawn circled-X
    /// sprite; `XC_X_cursor` when the server lacks RENDER cursors).
    pub disabled: u32,
    /// Pointing hand over clickable things: live titlebar buttons, boundary
    /// "+" buttons, and the taskbar (`XC_hand2` fallback).
    pub hand: u32,
    pub current: u32,
}

/// Everything clickable on the underlay, resolved by one priority-ordered
/// hit-test (`Wm::hit_test`) shared by `on_button` (dispatch) and
/// `hover_cursor` (cursor feedback) — a single ordering both consume, so
/// click handling and hover feedback can never drift apart.
#[derive(Clone, Copy)]
enum Hit {
    /// A split-control titlebar button (close/split/minimize).
    Btn(NodeId, BtnKind),
    /// The corner "x" badge on a taskbar tile.
    TaskbarClose(Win),
    /// A taskbar tile body.
    TaskbarTile(Win),
    /// A quick-launch icon in the taskbar (`Wm::quick` index).
    QuickLaunch(usize),
    /// A leaf's titlebar tab.
    Tab(NodeId),
    /// A boundary/edge "+" insert button (root-children insert index).
    Plus(usize),
    /// A gap drag handle.
    Handle(Boundary),
    /// An outer canvas-edge resize handle (`true` = left edge).
    Edge(bool),
    /// An empty split's body (no client window catches the click there).
    LeafBody(NodeId),
    Miss,
}

impl Wm {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn on_button(&mut self, e: ButtonPressEvent) -> R<()> {
        // Any click on a notification bubble dismisses it.
        if self.dismiss_note(e.event)? {
            return Ok(());
        }
        let wa = self.la();
        // Button 1 on a float's frame: focus the float and start moving it.
        if e.detail == 1 {
            if let Some(f) = self.floats.iter().find(|f| f.frame == e.event) {
                let (win, fx, fy) = (f.win, f.x, f.y);
                self.drags.active = Some(ActiveDrag::Float(FloatDrag {
                    win,
                    dx: i32::from(e.root_x) - fx,
                    dy: i32::from(e.root_y) - fy,
                }));
                self.focus_float(win)?;
                self.raise_notifications()?;
                return Ok(());
            }
        }
        // Clicks on the underlay: one shared, priority-ordered hit-test
        // (`hit_test`) resolves the target; `hover_cursor` consumes the same
        // ordering, so click dispatch and cursor feedback stay in lockstep.
        if e.event == self.underlay && (e.detail == 1 || e.detail == 3) {
            // Hit regions are computed from the *final* layout, but a
            // layout animation may still be drawing chrome mid-slide (the
            // event loop only cuts it after this batch). Snap it now so the
            // click lands on what the user sees.
            if self.anim.is_some() {
                self.step_animation(true)?;
            }
            let (mx, my) = (i32::from(e.event_x), i32::from(e.event_y));
            let hit = self.hit_test(mx, my);
            // Split-control buttons take left and right click (right picks
            // the opposite split direction); everything else is left only.
            if let Hit::Btn(leaf, kind) = hit {
                return self.click_split_button(leaf, kind, e.detail == 3);
            }
            if e.detail != 1 {
                return Ok(());
            }
            match hit {
                Hit::Btn(..) => {} // handled above
                // The corner "x" badge on a bottom-bar tile: politely close
                // that window.
                Hit::TaskbarClose(win) => return self.close_client(win),
                // A bottom-bar icon: bring that window into view and focus
                // it — via `bring_into_layout`, whose `commit_layout` also
                // scrolls a split that sits outside the viewport back in
                // (activating a window `place_clients` keeps unmapped would
                // otherwise focus an unviewable window).
                Hit::TaskbarTile(win) => {
                    self.animate = true;
                    return self.bring_into_layout(win);
                }
                // A quick-launch icon: spawn its command. The new window
                // lands wherever a normal map lands (the focused split or
                // the taskbar).
                Hit::QuickLaunch(i) => {
                    if let Some(cmd) = self.quick.get(i).map(|q| q.cmd.clone()) {
                        self.spawn(&cmd);
                    }
                    return Ok(());
                }
                // Click a title (tab) or an empty split's body to focus it.
                Hit::Tab(leaf) | Hit::LeafBody(leaf) => {
                    self.state.focus_leaf(leaf);
                    self.arrange()?;
                    self.focus(self.state.focused_client())?;
                }
                Hit::Plus(at) => {
                    self.state.insert_at_root(at);
                    self.animate = true;
                    return self.commit_layout();
                }
                Hit::Handle(b) => {
                    // A gap next to a minimized leaf can't be dragged (its
                    // pixel size is pinned); ignore the press.
                    if b.resizable {
                        self.drags.active = Some(ActiveDrag::Split(SplitDrag {
                            parent: b.parent,
                            idx: b.idx,
                            vertical: b.dir == Dir::V,
                            start: b.start,
                            combined: b.first + b.second,
                            gap: theme::GAP,
                        }));
                    }
                }
                // Outer canvas-edge resize handles: the screen-space x of
                // whichever end of the leftmost/rightmost column isn't being
                // dragged stays fixed for the whole gesture (see `EdgeDrag`).
                Hit::Edge(left) => {
                    if let Some((start_x, w)) = self.state.edge_span(wa, left) {
                        let canvas_anchor = if left { start_x + w } else { start_x };
                        let anchor_x = canvas_anchor - self.state.scroll_x();
                        self.drags.active = Some(ActiveDrag::Edge(EdgeDrag { left, anchor_x }));
                    }
                }
                Hit::Miss => {}
            }
            return Ok(());
        }
        // Click-to-focus on a client window.
        if e.detail == 1 {
            // Replay *before* any of the focus/arrange work below: the press
            // froze the pointer in a synchronous grab, and every call below
            // can fail (the clicked window may have died in the race window)
            // — an early `?` return that skipped the replay would leave the
            // pointer frozen until the server timed the grab out. Use the
            // grab event's own timestamp, not CURRENT_TIME — under latency
            // CURRENT_TIME can release a *later* grab than the one this
            // press froze.
            self.conn.allow_events(Allow::REPLAY_POINTER, e.time)?;
            match self.kind_of(e.event) {
                Some(WindowKind::Tiled) => {
                    self.state.activate_client(e.event);
                    self.arrange()?;
                    self.focus(Some(e.event))?;
                }
                Some(WindowKind::Float) => {
                    self.focus_float(e.event)?;
                    self.raise_notifications()?;
                }
                Some(WindowKind::Dock) => {
                    // Outside the tree/`clients`, so `focus()` (which only
                    // knows tiled windows) can't take it; set input focus
                    // directly. The press's own timestamp, not CURRENT_TIME
                    // — same race `give_focus` guards against.
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, e.event, e.time)?;
                    // Keep `_NET_ACTIVE_WINDOW` in step with the keyboard
                    // like every other focus path — pagers otherwise show
                    // the previous window as active while the user types
                    // into the dock.
                    self.set_net_active_window(e.event)?;
                }
                Some(WindowKind::Notification) | None => {}
            }
        }
        Ok(())
    }

    pub(crate) fn on_motion(&mut self, e: &MotionNotifyEvent) -> R<()> {
        match self.drags.active {
            Some(ActiveDrag::Float(fd)) => {
                self.move_float(
                    fd.win,
                    i32::from(e.root_x) - fd.dx,
                    i32::from(e.root_y) - fd.dy,
                )?;
                self.conn.flush()?;
            }
            Some(ActiveDrag::Edge(ed)) => {
                let wa = self.la();
                let mouse_x = i32::from(e.root_x);
                // Screen-space width: `anchor_x` is the fixed far edge, so the
                // gap to the mouse *is* the target width, no scroll conversion
                // needed — width is scroll-invariant, only position isn't.
                let target_w = if ed.left {
                    ed.anchor_x - mouse_x
                } else {
                    mouse_x - ed.anchor_x
                };
                let applied = self.state.resize_edge(wa, ed.left, target_w);
                // Growing the left column shifts every later column's
                // canvas-space x right by `applied` (`Tree::compute` always
                // lays out left-to-right from a fixed origin); scroll by the
                // same amount so they stay put on screen and only the dragged
                // edge visibly moves.
                if ed.left && applied != 0 {
                    self.state.shift_scroll(applied);
                }
                self.arrange()?;
            }
            Some(ActiveDrag::Split(d)) => {
                if d.combined <= 0 {
                    return Ok(());
                }
                // Only x scrolls; a vertical (row-boundary) drag reads y directly.
                let canvas_pos = if d.vertical {
                    i32::from(e.root_y)
                } else {
                    i32::from(e.root_x) + self.state.scroll_x()
                };
                let new_first = canvas_pos - d.start - d.gap / 2;
                let frac = f64::from(new_first) / f64::from(d.combined);
                self.state.resize_boundary(d.parent, d.idx, frac);
                self.arrange()?;
            }
            None => {
                // Not dragging: hover feedback only.
                if e.event == self.underlay {
                    let cur = self.hover_cursor(i32::from(e.event_x), i32::from(e.event_y));
                    self.set_underlay_cursor(cur)?;
                }
            }
        }
        Ok(())
    }

    /// Priority-ordered hit-test of everything clickable on the underlay,
    /// shared by `on_button` (dispatch) and `hover_cursor` (feedback).
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
        // wins. The corner "x" badge is checked before the tile bodies so
        // the badge wins the click.
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
            .tab_regions
            .iter()
            .find(|(r, _)| rect_contains(*r, mx, my))
            .map(|(_, l)| *l)
        {
            return Hit::Tab(leaf);
        }
        // "+" buttons sit centred inside their drag handle's (or the edge
        // handle's) larger hit region — check the narrower "+" rects first
        // so they aren't shadowed by the handles.
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

    /// Pick the pointer cursor for a hover position on the underlay:
    /// resize arrows over gap/edge drag handles, the hand over clickable
    /// buttons, the "disabled" cursor over a disabled titlebar button, and
    /// the plain arrow otherwise. Consumes the same `hit_test` ordering as
    /// `on_button`, so the advertised cursor always matches the click.
    fn hover_cursor(&self, mx: i32, my: i32) -> u32 {
        let c = self.cursors;
        match self.hit_test(mx, my) {
            Hit::Btn(leaf, kind) => {
                // Mirror `compose`'s enabled/disabled choice for the button
                // art (a minimized leaf's whole-frame region is always a
                // live restore button).
                if let Some(&frame) = self.prev_frame_rect.get(&leaf) {
                    let meta = self.leaf_meta(leaf, frame);
                    let disabled = !meta.minimized
                        && match kind {
                            BtnKind::Close | BtnKind::Minimize => meta.parent_dir.is_none(),
                            BtnKind::Split => !meta.can_split,
                        };
                    if disabled {
                        return c.disabled;
                    }
                }
                c.hand
            }
            Hit::TaskbarClose(_)
            | Hit::TaskbarTile(_)
            | Hit::QuickLaunch(_)
            | Hit::Tab(_)
            | Hit::Plus(_) => c.hand,
            Hit::Handle(b) => {
                // A gap next to a minimized leaf can't be dragged (its size
                // is pinned); don't advertise a resize that won't happen.
                if !b.resizable {
                    c.arrow
                } else if b.dir == Dir::V {
                    c.v_resize
                } else {
                    c.h_resize
                }
            }
            Hit::Edge(_) => c.h_resize,
            Hit::LeafBody(_) | Hit::Miss => c.arrow,
        }
    }

    /// Set the underlay's cursor, skipping the request when unchanged.
    fn set_underlay_cursor(&mut self, cursor: u32) -> R<()> {
        if self.cursors.current != cursor {
            self.cursors.current = cursor;
            self.conn.change_window_attributes(
                self.underlay,
                &ChangeWindowAttributesAux::new().cursor(cursor),
            )?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub(crate) fn on_button_release(&mut self, e: &ButtonReleaseEvent) -> R<()> {
        // Drags are button-1 gestures; a stray right/middle release mid-drag
        // must not end them.
        if e.detail != 1 {
            return Ok(());
        }
        let needs_arrange = match self.drags.active {
            Some(ActiveDrag::Split(_) | ActiveDrag::Edge(_)) => true,
            Some(ActiveDrag::Float(_)) | None => false,
        };
        self.drags.active = None;
        if needs_arrange {
            self.arrange()?;
        }
        Ok(())
    }
}
