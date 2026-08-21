//! Layout state plus every mutation of the column strip and the scroll
//! bookkeeping — there is exactly one layout (no workspaces/tags). Windows
//! and splits are paired one to one for life: a new window opens as its own
//! split, and a dying window takes its split with it. With no window open
//! the strip is empty and the screen is bare wallpaper.

use crate::layout::{Boundary, ColWidth, Insert, Layout, NodeId, Pos, Rect, Side, Win};
use crate::theme;

/// Where a dragged split lands when it is dropped, in the vocabulary of
/// the three relocations `State` performs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveDrop {
    /// Its own column at strip position `idx` — what the empty places
    /// mean: a gap between two columns, or the canvas past the strip's
    /// ends. They name a place rather than a neighbour, so a split leaves
    /// its stack for one even when the place is beside its own column.
    ColumnAt(usize),
    /// Its own column before (`true`) or after `dst`'s — what a drop onto
    /// something means: a split's frame, or a taskbar tile.
    Column(NodeId, bool),
    /// A row above (`true`) or below `dst` within `dst`'s stack.
    Stack(NodeId, bool),
}

/// Outcome of `activate_client`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activation {
    /// `c` occupied a minimized leaf that is now shown — rects changed.
    Unminimized,
    /// `c` already occupied a shown leaf — nothing a redraw would show moved.
    Unchanged,
}

pub struct State {
    pub layout: Layout,
    /// The split holding layout focus; `None` only while the strip is
    /// empty. Private so every write outside `#[cfg(test)]` goes through
    /// `focus_leaf` (which accepts only live leaf ids) and every read
    /// through `focused_leaf_valid` (the focused leaf can still be
    /// *removed* by a later mutation, so reads re-validate) — a dangling
    /// focus is never handed out.
    focused_leaf: Option<NodeId>,
    /// Where the next window to map opens, when a launch asked for a side
    /// (the taskbar's quick-launch compass). Held as the split it was
    /// aimed at plus that side, so a stale aim — its split closed before
    /// the window arrived — is simply dropped. Consumed by the next
    /// `place_new_window`, which otherwise opens a column right of the
    /// focused split.
    aim: Option<(NodeId, Side)>,
    /// Current and target scroll offsets — `scroll_x` glides toward
    /// `scroll_target` one frame at a time via `step_scroll`, driven by the
    /// main event loop while they differ (`scroll_animating`). Private so
    /// every mutation goes through the clamping/landing/stepping methods
    /// below.
    scroll_x: i32,
    scroll_target: i32,
    /// Extra scrollable width past the strip reserved for the docked
    /// sidebar (see `Comp::manage_dock`), so scrolling all the way right
    /// reveals it even though it sits outside the strip and doesn't
    /// affect `compute`'s leaf geometry. Zero when nothing is docked.
    /// Private so the only write (`set_dock_extra`) deliberately preserves
    /// the scroll against the changed range, leaving re-clamping to
    /// `clamp_scroll`.
    dock_extra: i32,
}

impl State {
    pub fn new() -> Self {
        Self {
            layout: Layout::new(),
            focused_leaf: None,
            aim: None,
            scroll_x: 0,
            scroll_target: 0,
            dock_extra: 0,
        }
    }

    /// The focused split, or `None` while the strip holds no window.
    pub fn focused_leaf_valid(&self) -> Option<NodeId> {
        match self.focused_leaf {
            Some(l) if self.layout.is_leaf(l) => Some(l),
            _ => self.layout.first_leaf(),
        }
    }

    /// Point focus at `leaf`. Anything that isn't a live leaf is ignored:
    /// callers can hold ids captured before an intervening mutation, and
    /// focus must never come to rest on a split `compute` doesn't lay out.
    pub fn focus_leaf(&mut self, leaf: NodeId) {
        if self.layout.is_leaf(leaf) {
            self.focused_leaf = Some(leaf);
        }
    }

    /// Where the focused split sits, or `None` while the strip is empty.
    fn focused_pos(&self) -> Option<Pos> {
        self.layout.locate(self.focused_leaf_valid()?)
    }

    // --- window placement helpers ---

    /// Place a newly mapped window: where the launch that started it
    /// aimed (`aim_next_window`, whose split must still be alive), else in
    /// a fresh column immediately right of the focused one. Every window
    /// lives in exactly one split from map to destroy — there is no
    /// off-layout stash and no split waiting empty for it.
    ///
    /// `want_w` is the frame width the window's own first-commit size asks
    /// for (`None` when the client stated nothing). It sizes the fresh
    /// column instead of `theme::default_col_w`; a window joining a stack
    /// shares its column's width with its siblings, so their deliberate
    /// arrangement wins there. Never below the chrome's minimum; a window
    /// asking for the whole viewport strip (or more) gets
    /// `ColWidth::Viewport`, so it keeps tracking the viewport — panels
    /// reserving exclusive zones still resize it. The first window on an
    /// empty strip tracks the viewport unless it asked for a width.
    pub fn place_new_window(&mut self, wa: Rect, c: Win, want_w: Option<i32>) {
        if self.layout.find_leaf_for_client(c).is_some() {
            return;
        }
        let max_w = (wa.w - 2 * theme::GAP).max(theme::min_split_w());
        let want = want_w.map(|w| {
            if w >= max_w {
                ColWidth::Viewport
            } else {
                ColWidth::Px(w.max(theme::min_split_w()))
            }
        });
        let new = match self.next_insert() {
            Insert::Col(idx) => self.open_column(wa, idx, c, want),
            Insert::Row { col, idx } => match self.layout.insert_row(col, idx, c) {
                Some(new) => new,
                // A dead column index, which `next_insert` never names —
                // and a window without a split must stay impossible.
                None => self.open_column(wa, col, c, want),
            },
        };
        self.focus_leaf(new);
    }

    /// Open a column at strip position `idx` holding `c`, at its stated
    /// width (`want`) or the default — the viewport-tracking one while the
    /// strip is still empty.
    fn open_column(&mut self, wa: Rect, idx: usize, c: Win, want: Option<ColWidth>) -> NodeId {
        let width = want.unwrap_or(if self.layout.ncols() == 0 {
            ColWidth::Viewport
        } else {
            ColWidth::Px(theme::default_col_w(wa.w))
        });
        self.layout.insert_column(idx, width, c)
    }

    /// Open the next window that maps on `side` of the focused split — the
    /// taskbar compass's wedges, which choose a side before the launched
    /// window exists. A second aim before that window arrives replaces the
    /// first: only one window can be the next one.
    pub fn aim_next_window(&mut self, side: Side) {
        self.aim = self.focused_leaf_valid().map(|leaf| (leaf, side));
    }

    /// Where the next window opens, consuming any aim: the side a launch
    /// asked for, resolved against the split it named (dropped when that
    /// split is gone), else a fresh column right of the focused one — the
    /// strip's first column while it is empty.
    fn next_insert(&mut self) -> Insert {
        let aimed = self
            .aim
            .take()
            .and_then(|(leaf, side)| Some((self.layout.locate(leaf)?, side)));
        match aimed {
            Some((pos, Side::Left)) => Insert::Col(pos.col),
            Some((pos, Side::Right)) => Insert::Col(pos.col + 1),
            Some((pos, Side::Up)) => Insert::Row {
                col: pos.col,
                idx: pos.row,
            },
            Some((pos, Side::Down)) => Insert::Row {
                col: pos.col,
                idx: pos.row + 1,
            },
            None => Insert::Col(self.focused_pos().map_or(0, |p| p.col + 1)),
        }
    }

    /// A window is gone: its split collapses out of the strip — windows
    /// and splits live and die together. A whole column vanishes without
    /// its neighbours resizing (the strip just gets shorter); a row leaves
    /// its stack to the surviving rows, which reclaim the height; the last
    /// window leaves the strip empty. Focus follows to the nearest
    /// surviving neighbour only when the dying split held it. Returns
    /// whether the layout changed.
    pub fn unpin_client(&mut self, c: Win) -> bool {
        let Some(lid) = self.layout.find_leaf_for_client(c) else {
            return false;
        };
        let had_focus = self.focused_leaf_valid() == Some(lid);
        let new_focus = self.layout.remove(lid);
        if had_focus {
            self.focused_leaf = new_focus;
        }
        true
    }

    /// Focus whatever split currently shows `c`, un-minimizing it —
    /// activation means the user (or a pager) wants the window visible, and
    /// a minimized leaf can't hold focus (see `focused_client` for why).
    /// Reports whether that changed anything a redraw would show, so callers
    /// can skip animating a transition that moves no rects (a plain refocus
    /// of an already-visible window). Every managed tiled window occupies a
    /// leaf for its whole life, so there is no not-found case.
    pub fn activate_client(&mut self, c: Win) -> Activation {
        let Some(lid) = self.layout.find_leaf_for_client(c) else {
            debug_assert!(false, "activate_client: {c:#x} occupies no leaf");
            return Activation::Unchanged;
        };
        let was_minimized = self.layout.leaf(lid).is_some_and(|l| l.minimized);
        if let Some(l) = self.layout.leaf_mut(lid) {
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
    /// target would mean focusing an unviewable window.
    pub fn focused_client(&self) -> Option<Win> {
        let l = self.layout.leaf(self.focused_leaf_valid()?)?;
        (!l.minimized).then_some(l.client)
    }

    // --- focus / move between splits ---

    fn adjacent_leaf(&self, from: NodeId, next: bool) -> Option<NodeId> {
        let leaves = self.layout.collect_leaves();
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
        let Some(from) = self.focused_leaf_valid() else {
            return false;
        };
        if let Some(l) = self.adjacent_leaf(from, next) {
            self.focus_leaf(l);
            true
        } else {
            false
        }
    }

    /// Move the focused split past its neighbour in strip order (the whole
    /// split relocates, window and all), wrapping around the ends like
    /// `focus_direction` does. Within a stack this reorders the rows;
    /// across a column edge the split leaves as its own column beside the
    /// neighbour's. Returns whether the strip changed.
    pub fn move_focused_split(&mut self, wa: Rect, next: bool) -> bool {
        let leaves = self.layout.collect_leaves();
        if leaves.len() < 2 {
            return false;
        }
        let Some(src) = self.focused_leaf_valid() else {
            return false;
        };
        let Some(cur) = leaves.iter().position(|&l| l == src) else {
            return false;
        };
        let n = leaves.len();
        // Stepping past an end wraps to the far end, which flips which side
        // of the destination the split lands on.
        let (dst, before) = if next {
            if cur + 1 < n {
                (leaves[cur + 1], false)
            } else {
                (leaves[0], true)
            }
        } else if cur > 0 {
            (leaves[cur - 1], true)
        } else {
            (leaves[n - 1], false)
        };
        let same_col = match (self.layout.locate(src), self.layout.locate(dst)) {
            (Some(s), Some(d)) => s.col == d.col,
            _ => return false,
        };
        let changed = if same_col {
            self.layout.move_into_stack(src, dst, before)
        } else {
            self.move_leaf_beside(wa, src, dst, before)
        };
        if changed {
            self.focus_leaf(src);
        }
        changed
    }

    /// Relocate split `src` into its own column beside `dst`'s (`before` =
    /// left), keeping `src` focused so the reorder visibly follows the
    /// gesture that asked for it. Returns whether the strip changed.
    pub fn move_leaf_beside(&mut self, wa: Rect, src: NodeId, dst: NodeId, before: bool) -> bool {
        let default = ColWidth::Px(theme::default_col_w(wa.w));
        if !self.layout.move_beside_column(src, dst, before, default) {
            return false;
        }
        self.focus_leaf(src);
        true
    }

    /// Relocate split `src` into its own column at strip position `idx`,
    /// keeping it focused — the drop into a gap or onto the bare canvas
    /// past the strip (see `MoveDrop::ColumnAt`). Returns whether the
    /// strip changed.
    pub fn move_leaf_to_column(&mut self, wa: Rect, src: NodeId, idx: usize) -> bool {
        let default = ColWidth::Px(theme::default_col_w(wa.w));
        if !self.layout.move_to_column(src, idx, default) {
            return false;
        }
        self.focus_leaf(src);
        true
    }

    /// Relocate split `src` into `dst`'s stack, above (`before`) or below
    /// its row — the horizontal-gap drop. Keeps `src` focused. Returns
    /// whether the strip changed.
    pub fn move_leaf_into_stack(&mut self, src: NodeId, dst: NodeId, before: bool) -> bool {
        if !self.layout.move_into_stack(src, dst, before) {
            return false;
        }
        self.focus_leaf(src);
        true
    }

    /// Toggle a leaf's minimized flag (the layout collapses it to min size,
    /// and its whole frame becomes the restore button). Returns whether the
    /// flag changed.
    pub fn toggle_minimize(&mut self, leaf: NodeId) -> bool {
        match self.layout.leaf_mut(leaf) {
            Some(l) => {
                l.minimized = !l.minimized;
                true
            }
            None => false,
        }
    }

    // --- resize ---

    /// Grow or shrink the focused split by one keyboard step. A stacked
    /// split trades height with its row neighbour (their sum is exactly
    /// conserved); a lone-in-column split just changes its column's width —
    /// the strip absorbs the difference and no sibling moves. Returns
    /// whether anything changed.
    pub fn resize_focused(&mut self, wa: Rect, grow: bool) -> bool {
        let Some(leaf) = self.focused_leaf_valid() else {
            return false;
        };
        let Some(pos) = self.layout.locate(leaf) else {
            return false;
        };
        if self.layout.leaf(leaf).is_some_and(|l| l.minimized) {
            return false;
        }
        if self.layout.col_len(pos.col) > 1 {
            let delta = if grow {
                theme::RESIZE_STEP
            } else {
                -theme::RESIZE_STEP
            };
            let other = if pos.row + 1 < self.layout.col_len(pos.col) {
                Pos {
                    row: pos.row + 1,
                    ..pos
                }
            } else {
                Pos {
                    row: pos.row - 1,
                    ..pos
                }
            };
            return self.transfer_row_frac(pos, other, delta);
        }
        let cur_w = self.layout.col_px(pos.col, wa.w, theme::GAP);
        let step = (wa.w / 20).max(1);
        let target = cur_w + if grow { step } else { -step };
        let new_w = target.max(theme::min_split_w());
        if new_w == cur_w {
            return false;
        }
        self.layout.set_col_width(pos.col, ColWidth::Px(new_w));
        true
    }

    /// Move `delta` of the column's height share from `other` to `pos`,
    /// capped at what each side can actually give so the pair's sum is
    /// exactly conserved — clamping both ends independently would let the
    /// total drift, silently resizing every other row.
    fn transfer_row_frac(&mut self, pos: Pos, other: Pos, delta: f64) -> bool {
        let (Some(cur), Some(cur_other)) = (self.layout.row_frac(pos), self.layout.row_frac(other))
        else {
            return false;
        };
        let min_r = theme::MIN_SPLIT_FRAC;
        let (lo, hi) = ((min_r - cur).min(0.0), (cur_other - min_r).max(0.0));
        let delta = delta.clamp(lo, hi);
        if delta == 0.0 {
            return false;
        }
        self.layout.set_row_frac(pos, cur + delta);
        self.layout.set_row_frac(other, cur_other - delta);
        true
    }

    /// Apply a stack-gap drag: re-split rows `idx` and `idx + 1` of `col`
    /// so the upper one occupies fraction `first_px / combined_px` of
    /// their combined height (their sum is preserved). The gap between two
    /// *columns* has no drag of its own — each half of it belongs to the
    /// window on that side, and resizes that column (`resize_col`).
    pub fn resize_rows(&mut self, col: usize, idx: usize, first_px: i32, combined_px: i32) {
        if combined_px <= 0 {
            return;
        }
        let frac = (f64::from(first_px) / f64::from(combined_px))
            .clamp(theme::MIN_SPLIT_FRAC, 1.0 - theme::MIN_SPLIT_FRAC);
        let (a, b) = (Pos { col, row: idx }, Pos { col, row: idx + 1 });
        let (Some(fa), Some(fb)) = (self.layout.row_frac(a), self.layout.row_frac(b)) else {
            return;
        };
        let combined = fa + fb;
        self.layout.set_row_frac(a, combined * frac);
        self.layout.set_row_frac(b, combined * (1.0 - frac));
    }

    /// Resize column `col` to `target_w` pixels: the column absorbs the
    /// whole delta and the strip grows/shrinks with it; no sibling
    /// resizes (later columns slide in canvas space). Refused when the
    /// column is pinned (minimized) — its visible width is the gap, not a
    /// real width, so the drag is meaningless. Returns the applied delta.
    ///
    /// For a left-side drag, the column's *start* is what's meant to
    /// track the mouse (growing toward the screen edge), but the strip is
    /// laid out left-to-right from a fixed origin — so growing the column
    /// shifts every later column's canvas-space x right by the delta. The
    /// caller nudges `scroll_x`/`scroll_target` by the same delta so those
    /// columns stay put on screen and only the dragged edge visibly moves.
    pub fn resize_col(&mut self, wa: Rect, col: usize, target_w: i32) -> i32 {
        if self.layout.col_pinned(col) {
            return 0;
        }
        let old_w = self.layout.col_px(col, wa.w, theme::GAP);
        let new_w = target_w.max(theme::min_split_w());
        let delta = new_w - old_w;
        if delta == 0 {
            return 0;
        }
        self.layout.set_col_width(col, ColWidth::Px(new_w));
        delta
    }

    /// Resize the leftmost or rightmost column (the outer canvas-edge
    /// handles' target) — see `resize_col`.
    pub fn resize_edge(&mut self, wa: Rect, left: bool, target_w: i32) -> i32 {
        let Some(last) = self.layout.ncols().checked_sub(1) else {
            return 0;
        };
        self.resize_col(wa, if left { 0 } else { last }, target_w)
    }

    // --- canvas ---

    /// The scrollable strip's width: the columns end to end plus margins,
    /// exactly (`Layout::strip_w`). Narrower than the viewport is
    /// meaningful — the leftover is wallpaper margin.
    pub fn canvas_w(&self, wa: Rect) -> i32 {
        self.layout.strip_w(wa.w, theme::GAP)
    }

    /// Record the extra scroll room the docked sidebar needs (zero when
    /// nothing is docked); called once per arrange. Scroll positions are
    /// deliberately *not* re-clamped here: an edge drag parks them outside
    /// `[min_scroll, max_scroll]` to hold a wallpaper margin at the dragged edge
    /// (see `shift_scroll`), and this runs on every arrange, so clamping
    /// here would yank that margin shut on the next hover repaint.
    /// Mutations that change the scroll range out from under the user
    /// (structural layout changes, viewport resizes, dock removal) call
    /// `clamp_scroll` explicitly instead.
    pub fn set_dock_extra(&mut self, dock_extra: i32) {
        self.dock_extra = dock_extra;
    }

    /// Pull both scroll positions back into `[min_scroll, max_scroll]`,
    /// against the dock room last recorded by `set_dock_extra`. This is
    /// the companion to `set_dock_extra` not clamping: structural layout
    /// changes, viewport resizes and dock removal shrink the scroll range
    /// and must not strand the viewport past the content, while edge-drag
    /// margins (scroll out of range on purpose) survive everything that
    /// doesn't call this.
    pub fn clamp_scroll(&mut self, wa: Rect) {
        let (min_scroll, max_scroll) = (Self::min_scroll(wa), self.max_scroll(wa));
        self.scroll_target = self.scroll_target.clamp(min_scroll, max_scroll);
        self.scroll_x = self.scroll_x.clamp(min_scroll, max_scroll);
    }

    /// The dock scroll room last supplied to `set_dock_extra`.
    pub fn dock_extra(&self) -> i32 {
        self.dock_extra
    }

    // --- scroll ---

    pub fn scroll_x(&self) -> i32 {
        self.scroll_x
    }

    /// Land the scroll: snap the current offset to the target. Used where a
    /// glide would be wrong — landing before an edge/split drag arms (so its
    /// anchor math stays exact) and landing before a layout animation starts
    /// (whose placements are computed from `scroll_x` at arrange time, so a
    /// concurrently-gliding scroll would make them stale each frame).
    pub fn land_scroll(&mut self) {
        self.scroll_x = self.scroll_target;
    }

    /// Per-frame fraction of the remaining distance closed by `step_scroll`,
    /// tuned for the event loop's 16ms frame cadence: snappy enough to keep
    /// pace with a trackpad swipe's moving target, while still reading as a
    /// glide alongside the 280ms layout animation (`comp::anim::ANIM_DURATION`).
    const SCROLL_GLIDE_K: f64 = 0.25;
    /// Below this remaining distance, `step_scroll` snaps rather than
    /// asymptotically approaching forever.
    const SCROLL_SNAP_PX: i32 = 1;

    /// Advance the scroll glide by one frame toward `scroll_target`: an
    /// exponential approach that snaps once within `SCROLL_SNAP_PX`. A
    /// target that moves mid-glide (fresh scroll input) is simply re-aimed —
    /// there's no fixed-duration tween to restart. Returns whether the glide
    /// is still in flight, so callers know whether to keep stepping.
    pub fn step_scroll(&mut self) -> bool {
        let delta = self.scroll_target - self.scroll_x;
        if delta.abs() <= Self::SCROLL_SNAP_PX {
            self.scroll_x = self.scroll_target;
        } else {
            self.scroll_x += (f64::from(delta) * Self::SCROLL_GLIDE_K).round() as i32;
        }
        self.scroll_animating()
    }

    /// Whether `scroll_x` has not yet caught up to `scroll_target` — the
    /// event loop keeps stepping frames (and stays non-blocking) while this
    /// holds, exactly like it does for the layout animation
    /// (`ChromeView::anim`).
    pub fn scroll_animating(&self) -> bool {
        self.scroll_x != self.scroll_target
    }

    /// Shift both offsets by `delta` without clamping — used by the
    /// left-side resize drags (canvas edge or window border) to keep
    /// on-screen columns stationary while the strip width changes
    /// underneath (the strip lays out from a fixed origin, so resizing a
    /// column moves every later column in canvas space). A shrink can
    /// legitimately take the scroll below `min_scroll`; see `max_scroll`
    /// for what out-of-range scroll means.
    pub fn shift_scroll(&mut self, delta: i32) {
        self.scroll_x += delta;
        self.scroll_target += delta;
    }

    /// Lower end of the *scrollable* range: wallpaper padding left of the
    /// strip, nearly a viewport of it — at the limit the first column
    /// starts exactly at the viewport's right edge, so the whole strip
    /// can be panned out of view.
    pub fn min_scroll(wa: Rect) -> i32 {
        theme::GAP - wa.w
    }

    /// Upper end of the *scrollable* range. The current scroll can still
    /// sit outside `[min_scroll, max_scroll]`: scroll past `max_scroll`
    /// is margin right of the strip (a right-edge shrink narrows the
    /// strip under an unmoved scroll, via `shift_scroll`). Such a margin
    /// holds until a scroll gesture (`scroll_to` clamps) or a
    /// range-shrinking mutation (`clamp_scroll`) repositions the viewport.
    pub fn max_scroll(&self, wa: Rect) -> i32 {
        (self.canvas_w(wa) + self.dock_extra - wa.w).max(0)
    }

    pub fn scroll_to(&mut self, wa: Rect, target: i32) {
        self.scroll_target = target.clamp(Self::min_scroll(wa), self.max_scroll(wa));
    }

    pub fn scroll_delta(&mut self, wa: Rect, delta: i32) {
        let t = self.scroll_target + delta;
        self.scroll_to(wa, t);
    }

    /// Geometry of every leaf in canvas coordinates.
    pub fn compute(&self, wa: Rect) -> std::collections::HashMap<NodeId, Rect> {
        self.layout.compute(wa, theme::GAP)
    }

    /// Gaps between adjacent splits, for the drag handles.
    pub fn boundaries(&self, wa: Rect) -> Vec<Boundary> {
        self.layout.boundaries(wa, theme::GAP)
    }

    /// Canvas-space x-span `(start_x, width)` of the leftmost/rightmost
    /// column — used to seed and drive an edge-of-strip resize drag (see
    /// `resize_edge`). With a single column, `left`/`right` both describe
    /// the same span; `None` on an empty strip, which has no edges to drag.
    pub fn edge_span(&self, wa: Rect, left: bool) -> Option<(i32, i32)> {
        let gap = theme::GAP;
        let start_x = wa.x + gap;
        let n = self.layout.ncols();
        if n == 0 {
            return None;
        }
        if left {
            Some((start_x, self.layout.col_px(0, wa.w, gap)))
        } else {
            let before: i32 = (0..n - 1).map(|i| self.layout.col_px(i, wa.w, gap)).sum();
            let gaps_before = gap * i32::try_from(n - 1).unwrap_or(0);
            Some((
                start_x + before + gaps_before,
                self.layout.col_px(n - 1, wa.w, gap),
            ))
        }
    }

    /// Where a split dropped on bare wallpaper lands: the strip's own
    /// margins name a place just like the gaps between splits do. Left of
    /// the first column or right of the last — the empty canvas beyond the
    /// strip — the split becomes a column at that end; over a column's own
    /// width, in its top or bottom margin, it joins that column's stack at
    /// the near end. `None` only for an empty strip, which has no margins
    /// to speak of. Screen coordinates: the scroll is applied here.
    pub fn margin_drop(&self, wa: Rect, mx: i32, my: i32) -> Option<MoveDrop> {
        let last_col = self.layout.ncols().checked_sub(1)?;
        let geos = self.compute(wa);
        let head = |col: usize| self.layout.leaf_at(Pos { col, row: 0 });
        // A column's rows all share its x-span, so its first row states it.
        let span = |col: usize| {
            let g = geos.get(&head(col)?)?;
            Some((g.x - self.scroll_x, g.w))
        };
        let (first_x, _) = span(0)?;
        if mx < first_x {
            return Some(MoveDrop::ColumnAt(0));
        }
        let (last_x, last_w) = span(last_col)?;
        if mx >= last_x + last_w {
            return Some(MoveDrop::ColumnAt(last_col + 1));
        }
        let col = (0..=last_col).find(|&c| span(c).is_some_and(|(x, w)| mx >= x && mx < x + w))?;
        let top = head(col)?;
        let bottom = self.layout.leaf_at(Pos {
            col,
            row: self.layout.col_len(col).checked_sub(1)?,
        })?;
        if geos.get(&top).is_some_and(|g| my < g.y) {
            Some(MoveDrop::Stack(top, true))
        } else {
            Some(MoveDrop::Stack(bottom, false))
        }
    }

    /// Scroll so the focused split sits inside the viewport (one gap margin).
    pub fn ensure_in_view(&mut self, wa: Rect) {
        let Some(focused) = self.focused_leaf_valid() else {
            return;
        };
        let geos = self.compute(wa);
        let geo = match geos.get(&focused) {
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

    /// Scroll so the focused split's column spans screen-x `px` — keyboard
    /// delivery is hover-based, so a keyboard focus move slides the split
    /// under the pointer rather than warping the pointer to the split.
    /// Minimal adjustment against `scroll_target` (the pending position,
    /// so it composes with `ensure_in_view`). Deliberately unclamped: the
    /// last column reaching a pointer on the far side of the viewport
    /// needs margin past the strip's end, which the scroll model already
    /// tolerates (see `max_scroll`; `min_scroll` grants nearly a viewport
    /// of the same padding leftward). A pointer inside the viewport can't
    /// push the target below `min_scroll`.
    pub fn align_focus_to(&mut self, wa: Rect, px: i32) {
        let Some(focused) = self.focused_leaf_valid() else {
            return;
        };
        let geos = self.compute(wa);
        let Some(g) = geos.get(&focused) else {
            return;
        };
        let left = g.x - self.scroll_target;
        let right = left + g.w;
        if px < left {
            self.scroll_target = g.x - px;
        } else if px >= right {
            self.scroll_target = g.x + g.w - 1 - px;
        }
        debug_assert!(self.scroll_target >= Self::min_scroll(wa));
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
    const GAP: i32 = crate::theme::GAP;

    fn leaf_clients(s: &State) -> Vec<Win> {
        s.layout
            .collect_leaves()
            .into_iter()
            .map(|l| s.layout.leaf(l).unwrap().client)
            .collect()
    }

    /// A compass zone aims the next window at that side of the focused
    /// split: left/right open a neighbouring column, up/down a row of the
    /// focused split's own stack.
    #[test]
    fn an_aimed_window_opens_on_that_side() {
        for (side, expected) in [
            (Side::Left, vec![2, 1]),
            (Side::Right, vec![1, 2]),
            (Side::Up, vec![2, 1]),
            (Side::Down, vec![1, 2]),
        ] {
            let mut s = State::new();
            s.place_new_window(WA, 1, None);
            s.aim_next_window(side);
            s.place_new_window(WA, 2, None);
            assert_eq!(leaf_clients(&s), expected, "{side:?}");
            let stacked = matches!(side, Side::Up | Side::Down);
            assert_eq!(s.layout.ncols(), if stacked { 1 } else { 2 }, "{side:?}");
            assert_eq!(s.focused_client(), Some(2), "{side:?}");
        }
    }

    /// An aim is spent by the window it was made for: the one after it
    /// opens where an unaimed window would.
    #[test]
    fn an_aim_lasts_for_one_window_only() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        s.place_new_window(WA, 3, None);
        assert_eq!(s.layout.ncols(), 2, "the third window opened a column");
    }

    /// An aim whose split closed before the window arrived is dropped —
    /// the window opens beside the focused split instead.
    #[test]
    fn an_aim_at_a_closed_split_is_dropped() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.aim_next_window(Side::Down);
        s.unpin_client(1);
        s.place_new_window(WA, 3, None);
        assert_eq!(s.layout.ncols(), 2, "a column, not a row of a dead stack");
        assert_eq!(leaf_clients(&s), vec![2, 3]);
    }

    /// The strip starts empty: the first window is its only column, and
    /// closing it leaves nothing behind.
    #[test]
    fn an_empty_strip_holds_no_split() {
        let mut s = State::new();
        assert_eq!(s.focused_leaf_valid(), None);
        assert_eq!(s.focused_client(), None);
        s.place_new_window(WA, 1, None);
        assert_eq!(s.focused_client(), Some(1));
        assert_eq!(s.layout.collect_leaves().len(), 1);
        assert!(s.unpin_client(1));
        assert_eq!(s.layout.collect_leaves(), Vec::new());
        assert_eq!(s.focused_leaf_valid(), None);
    }

    /// A stated preferred width sizes the window's column: the first
    /// column stops tracking the viewport, and a fresh column opens at the
    /// hint instead of `default_col_w`.
    #[test]
    fn place_honors_the_windows_preferred_width() {
        let mut s = State::new();
        s.place_new_window(WA, 1, Some(500));
        assert_eq!(s.layout.widths(WA.w, GAP)[0], 500, "first column");
        s.place_new_window(WA, 2, Some(300));
        assert_eq!(s.layout.widths(WA.w, GAP)[1], 300, "fresh column");
    }

    /// Preferred widths are clamped to sane bounds: a hint at (or past)
    /// the viewport strip's width becomes `Viewport` — the column keeps
    /// tracking viewport changes like the first one — and one below
    /// the chrome's minimum is raised to it. No hint falls back to the
    /// default width.
    #[test]
    fn place_clamps_preferred_width_and_defaults_without_one() {
        let mut s = State::new();
        s.place_new_window(WA, 1, Some(5000));
        assert_eq!(s.layout.widths(WA.w, GAP)[0], WA.w - 2 * GAP);
        assert_eq!(s.layout.col_width(0), Some(ColWidth::Viewport));
        s.place_new_window(WA, 2, Some(1));
        assert_eq!(s.layout.widths(WA.w, GAP)[1], crate::theme::min_split_w());
        s.place_new_window(WA, 3, None);
        assert_eq!(
            s.layout.widths(WA.w, GAP)[2],
            crate::theme::default_col_w(WA.w)
        );
    }

    /// A hint never resizes the column a window joins as a row — its
    /// width is shared with siblings the user already arranged.
    #[test]
    fn place_hint_leaves_a_joined_stacks_width_alone() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        let before = s.layout.widths(WA.w, GAP)[0];
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, Some(300));
        assert_eq!(s.layout.widths(WA.w, GAP)[0], before);
        assert_eq!(s.layout.col_len(0), 2, "joined the stack");
    }

    /// With the focused split occupied, a new window opens in a fresh
    /// column immediately right of the focused one, and gets the focus.
    #[test]
    fn place_opens_a_column_right_of_the_focused_one() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        assert_eq!(leaf_clients(&s), vec![1, 2, 3]);
        assert_eq!(s.focused_client(), Some(3));

        // Opening from the middle lands between, not at the end.
        s.focus_leaf(s.layout.find_leaf_for_client(2).unwrap());
        s.place_new_window(WA, 4, None);
        assert_eq!(leaf_clients(&s), vec![1, 2, 4, 3]);
    }

    /// A stacked split sends an unaimed new window to a fresh column
    /// beside its whole stack, not into the stack.
    #[test]
    fn place_from_a_stacked_split_opens_a_column() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 3, None);
        assert_eq!(
            s.layout.locate(s.layout.find_leaf_for_client(3).unwrap()),
            Some(Pos { col: 0, row: 1 }),
            "the row the aim asked for"
        );
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.place_new_window(WA, 4, None);
        assert_eq!(s.layout.ncols(), 3, "new column, no stack growth");
        assert_eq!(
            s.layout.locate(s.layout.find_leaf_for_client(4).unwrap()),
            Some(Pos { col: 1, row: 0 }),
            "right of window 1's stack, before window 2"
        );
        assert_eq!(s.focused_client(), Some(4));
    }

    /// Opening and closing a column never resizes the other columns; the
    /// strip absorbs the difference.
    #[test]
    fn open_close_never_resizes_neighbours() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        let l2 = s.layout.find_leaf_for_client(2).unwrap();
        let before = s.compute(WA);
        let strip = s.canvas_w(WA);
        assert!(s.unpin_client(2), "a column was removed");
        let after = s.compute(WA);
        for w in [1, 3] {
            let l = s.layout.find_leaf_for_client(w).unwrap();
            assert_eq!(before[&l].w, after[&l].w, "window {w} kept its width");
        }
        assert_eq!(s.canvas_w(WA), strip - before[&l2].w - GAP);
    }

    /// Closing a split stacked inside a column still merges: the stack
    /// neighbour reclaims the height and the strip is untouched.
    #[test]
    fn unpin_in_a_stack_still_merges() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        let strip = s.canvas_w(WA);
        let full = {
            let g = s.compute(WA);
            let l1 = s.layout.find_leaf_for_client(1).unwrap();
            let l2 = s.layout.find_leaf_for_client(2).unwrap();
            g[&l1].h + g[&l2].h + GAP
        };
        assert!(s.unpin_client(2), "the stack collapsed");
        let l1 = s.layout.find_leaf_for_client(1).unwrap();
        assert_eq!(s.compute(WA)[&l1].h, full, "height reclaimed");
        assert_eq!(s.canvas_w(WA), strip, "no column left the strip");
    }

    /// A destroyed window takes its split with it; focus moves to the
    /// surviving neighbour only when the dying split held it.
    #[test]
    fn unpin_collapses_the_split() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        assert!(s.unpin_client(2), "collapse is a layout change");
        assert_eq!(s.layout.collect_leaves().len(), 1);
        assert_eq!(s.focused_client(), Some(1));
    }

    #[test]
    fn unpin_keeps_focus_when_it_was_elsewhere() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.unpin_client(2);
        assert_eq!(s.focused_client(), Some(1));
    }

    /// A split dropped on bare wallpaper lands where that wallpaper is:
    /// beyond either end of the strip it becomes a column at that end,
    /// and in a column's top/bottom margin it joins that column's stack.
    #[test]
    fn margin_drops_land_at_the_strips_edges() {
        let mut s = State::new();
        s.place_new_window(WA, 1, Some(300));
        s.place_new_window(WA, 2, Some(300));
        let leaf = |c: Win| s.layout.find_leaf_for_client(c).unwrap();
        // Columns at 20..320 and 340..640, rows spanning 20..780.
        assert_eq!(
            s.margin_drop(WA, 5, 400),
            Some(MoveDrop::ColumnAt(0)),
            "left of the strip"
        );
        assert_eq!(
            s.margin_drop(WA, 700, 400),
            Some(MoveDrop::ColumnAt(2)),
            "right of the strip"
        );
        assert_eq!(
            s.margin_drop(WA, 100, 5),
            Some(MoveDrop::Stack(leaf(1), true)),
            "the first column's top margin"
        );
        assert_eq!(
            s.margin_drop(WA, 100, WA.h - 5),
            Some(MoveDrop::Stack(leaf(1), false)),
            "its bottom margin"
        );
    }

    /// The margins name a column position, not a neighbour, so the *top*
    /// row of a stack leaves it just like the bottom row does — naming a
    /// neighbour would name the dragged split itself and move nothing.
    #[test]
    fn a_stacks_top_row_leaves_it_for_the_margins() {
        for (mx, expect) in [(5, vec![1, 2]), (700, vec![2, 1])] {
            let mut s = State::new();
            s.place_new_window(WA, 1, Some(300));
            s.aim_next_window(Side::Down);
            s.place_new_window(WA, 2, None);
            let top = s.layout.find_leaf_for_client(1).unwrap();
            let MoveDrop::ColumnAt(idx) = s.margin_drop(WA, mx, 400).expect("a margin") else {
                panic!("the canvas past the strip names a column position");
            };
            assert!(s.move_leaf_to_column(WA, top, idx), "mx={mx}");
            assert_eq!(s.layout.ncols(), 2, "mx={mx}");
            assert_eq!(leaf_clients(&s), expect, "mx={mx}");
        }
    }

    /// A column's bottom margin names its *last* row, so a drop there
    /// joins the stack below everything already in it.
    #[test]
    fn a_stacks_bottom_margin_names_its_last_row() {
        let mut s = State::new();
        s.place_new_window(WA, 1, Some(300));
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        let bottom = s.layout.find_leaf_for_client(2).unwrap();
        assert_eq!(
            s.margin_drop(WA, 100, WA.h - 5),
            Some(MoveDrop::Stack(bottom, false))
        );
    }

    /// An empty strip has no margins to drop into.
    #[test]
    fn an_empty_strip_takes_no_margin_drop() {
        assert_eq!(State::new().margin_drop(WA, 100, 100), None);
    }

    /// Focus cycling walks the strip order and wraps at the ends.
    #[test]
    fn focus_direction_cycles_and_wraps() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        assert!(s.focus_direction(true));
        assert_eq!(s.focused_client(), Some(1), "wrapped past the end");
        assert!(s.focus_direction(false));
        assert_eq!(s.focused_client(), Some(3));
    }

    /// Mod4+Shift+brackets swap the focused split with its strip-order
    /// neighbour; wrapping moves it to the far end.
    #[test]
    fn move_focused_split_swaps_with_neighbour() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        s.focus_leaf(s.layout.find_leaf_for_client(2).unwrap());
        assert!(s.move_focused_split(WA, true));
        assert_eq!(leaf_clients(&s), vec![1, 3, 2]);
        assert_eq!(s.focused_client(), Some(2), "focus follows the move");
        assert!(s.move_focused_split(WA, true), "wraps to the front");
        assert_eq!(leaf_clients(&s), vec![2, 1, 3]);
    }

    /// Moving within a stack reorders the rows instead of leaving the
    /// column.
    #[test]
    fn move_focused_split_reorders_within_a_stack() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        assert!(s.move_focused_split(WA, true));
        assert_eq!(s.layout.ncols(), 1, "stayed one column");
        assert_eq!(leaf_clients(&s), vec![2, 1]);
    }

    /// A minimized leaf's window is hidden, so it can't be the focused
    /// client; activation un-minimizes and refocuses it.
    #[test]
    fn activate_unminimizes_and_focuses() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        let l2 = s.layout.find_leaf_for_client(2).unwrap();
        s.toggle_minimize(l2);
        assert_eq!(s.focused_client(), None, "minimized leaf shows nothing");
        assert_eq!(s.activate_client(2), Activation::Unminimized);
        assert_eq!(s.focused_client(), Some(2));
        assert_eq!(s.activate_client(2), Activation::Unchanged);
    }

    /// Growing a lone column widens only that column; the strip follows.
    #[test]
    fn resize_focused_lone_column_moves_only_the_strip() {
        let mut s = State::new();
        for w in [1, 2] {
            s.place_new_window(WA, w, None);
        }
        let l1 = s.layout.find_leaf_for_client(1).unwrap();
        let before = s.compute(WA);
        let strip = s.canvas_w(WA);
        assert!(s.resize_focused(WA, true));
        let after = s.compute(WA);
        assert_eq!(before[&l1].w, after[&l1].w, "neighbour untouched");
        let l2 = s.layout.find_leaf_for_client(2).unwrap();
        let grown = after[&l2].w - before[&l2].w;
        assert!(grown > 0);
        assert_eq!(s.canvas_w(WA), strip + grown);
    }

    /// Growing a stacked split trades height with its row neighbour; the
    /// pair's sum is conserved.
    #[test]
    fn resize_focused_stacked_conserves_the_pair() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        let rows = s.layout.collect_leaves();
        s.focus_leaf(rows[0]);
        let before = s.compute(WA);
        let pair = before[&rows[0]].h + before[&rows[1]].h;
        assert!(s.resize_focused(WA, true));
        let after = s.compute(WA);
        assert!(after[&rows[0]].h > before[&rows[0]].h);
        assert_eq!(after[&rows[0]].h + after[&rows[1]].h, pair);
    }

    /// A stack-gap drag preserves the two rows' combined height.
    #[test]
    fn resize_gap_preserves_row_sum() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.aim_next_window(Side::Down);
        s.place_new_window(WA, 2, None);
        let rows = s.layout.collect_leaves();
        let before = s.compute(WA);
        let pair = before[&rows[0]].h + before[&rows[1]].h;
        s.resize_rows(0, 0, pair / 4, pair);
        let after = s.compute(WA);
        assert_eq!(after[&rows[0]].h + after[&rows[1]].h, pair);
        assert!(after[&rows[0]].h < before[&rows[0]].h);
    }

    #[test]
    fn resize_edge_shrinks_lone_leaf() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        let target = WA.w / 2;
        let delta = s.resize_edge(WA, false, target);
        assert!(delta < 0);
        let l1 = s.layout.find_leaf_for_client(1).unwrap();
        assert_eq!(s.compute(WA)[&l1].w, target);
        assert!(s.canvas_w(WA) < WA.w, "wallpaper margin appears");
    }

    #[test]
    fn resize_edge_grows_left_column_and_strip() {
        let mut s = State::new();
        for w in [1, 2] {
            s.place_new_window(WA, w, None);
        }
        let strip = s.canvas_w(WA);
        let (_, w0) = s.edge_span(WA, true).unwrap();
        let delta = s.resize_edge(WA, true, w0 + 150);
        assert_eq!(delta, 150);
        assert_eq!(s.canvas_w(WA), strip + 150);
        let l2 = s.layout.find_leaf_for_client(2).unwrap();
        let widths = s.layout.widths(WA.w, GAP);
        assert_eq!(widths[0], w0 + 150);
        assert_eq!(s.compute(WA)[&l2].w, widths[1], "right column untouched");
    }

    #[test]
    fn resize_edge_leaves_minimized_column_alone() {
        let mut s = State::new();
        for w in [1, 2] {
            s.place_new_window(WA, w, None);
        }
        let focused = s.focused_leaf_valid().expect("a window is open");
        s.toggle_minimize(focused); // rightmost pinned
        assert_eq!(s.resize_edge(WA, false, 500), 0);
    }

    // --- scroll behavior ---

    #[test]
    fn step_scroll_approaches_target() {
        let mut s = State::new();
        for w in [1, 2, 3, 4, 5] {
            s.place_new_window(WA, w, None);
        }
        s.scroll_to(WA, 200);
        assert!(s.step_scroll());
        assert!(s.scroll_x() > 0 && s.scroll_x() < 200);
    }

    #[test]
    fn step_scroll_snaps_within_threshold() {
        let mut s = State::new();
        for w in [1, 2, 3, 4, 5] {
            s.place_new_window(WA, w, None);
        }
        s.scroll_to(WA, 1);
        assert!(!s.step_scroll(), "snapped: glide over");
        assert_eq!(s.scroll_x(), 1);
    }

    #[test]
    fn step_scroll_moving_target_reaims() {
        let mut s = State::new();
        for w in [1, 2, 3, 4, 5] {
            s.place_new_window(WA, w, None);
        }
        s.scroll_to(WA, 200);
        s.step_scroll();
        let mid = s.scroll_x();
        s.scroll_to(WA, 0);
        s.step_scroll();
        assert!(s.scroll_x() < mid, "glide re-aims at the new target");
    }

    #[test]
    fn shift_scroll_stays_exact_not_a_glide() {
        let mut s = State::new();
        s.shift_scroll(-40);
        assert_eq!(s.scroll_x(), -40);
        assert!(!s.scroll_animating(), "both offsets moved together");
    }

    /// `ensure_in_view` scrolls a focused off-viewport column into view.
    #[test]
    fn ensure_in_view_reaches_the_focused_column() {
        let mut s = State::new();
        for w in [1, 2, 3, 4, 5, 6] {
            s.place_new_window(WA, w, None);
        }
        s.land_scroll();
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.ensure_in_view(WA);
        s.land_scroll();
        let l1 = s.layout.find_leaf_for_client(1).unwrap();
        let geo = s.compute(WA)[&l1];
        assert!(geo.x - s.scroll_x() >= WA.x, "left edge visible");
    }

    /// Scrolling to the far left parks the whole strip past the
    /// viewport's right edge — the wallpaper padding left of the canvas.
    #[test]
    fn min_scroll_pans_the_strip_fully_out_of_view() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.scroll_to(WA, i32::MIN);
        s.land_scroll();
        assert_eq!(s.scroll_x(), State::min_scroll(WA));
        let first = s.layout.first_leaf().expect("a window is open");
        let geo = s.compute(WA)[&first];
        assert!(
            geo.x - s.scroll_x() >= WA.x + WA.w,
            "first column starts past the right viewport edge"
        );
    }

    /// A border drag resizes only the grabbed column: siblings keep their
    /// widths and the strip absorbs the delta.
    #[test]
    fn resize_col_leaves_sibling_widths_alone() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        let strip = s.canvas_w(WA);
        let before = s.layout.widths(WA.w, GAP);
        let applied = s.resize_col(WA, 1, before[1] + 120);
        assert_eq!(applied, 120);
        let after = s.layout.widths(WA.w, GAP);
        assert_eq!(after[1], before[1] + 120);
        assert_eq!((after[0], after[2]), (before[0], before[2]));
        assert_eq!(s.canvas_w(WA), strip + 120);
    }

    /// clamp_scroll never strands the viewport past shrunken content, but
    /// an in-range scroll survives it untouched.
    #[test]
    fn clamp_scroll_pulls_back_into_range() {
        let mut s = State::new();
        for w in [1, 2, 3, 4, 5] {
            s.place_new_window(WA, w, None);
        }
        let max = s.max_scroll(WA);
        assert!(max > 0);
        s.scroll_to(WA, max);
        s.land_scroll();
        while s.layout.collect_leaves().len() > 1 {
            let last = *s.layout.collect_leaves().last().unwrap();
            let win = s.layout.leaf(last).unwrap().client;
            s.unpin_client(win);
            s.clamp_scroll(WA);
        }
        assert_eq!(s.scroll_x(), 0, "single column: nothing to scroll");
    }
}
