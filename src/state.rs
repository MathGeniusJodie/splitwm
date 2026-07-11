//! Layout state plus every mutation of the column strip and the scroll
//! bookkeeping — there is exactly one layout (no workspaces/tags). Windows
//! and splits are paired for life: a new window opens in (or as) its own
//! split, and a dying window usually takes its split with it; only empty
//! placeholder splits exist without a window, never the reverse.

use crate::layout::{Boundary, ColWidth, GapAt, Insert, Layout, NodeId, Pos, Rect, Win};
use crate::theme;

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
    /// Private so every write outside `#[cfg(test)]` goes through
    /// `focus_leaf` (which accepts only live leaf ids) and every read
    /// through `focused_leaf_valid` (the focused leaf can still be
    /// *removed* by a later mutation, so reads re-validate) — a dangling
    /// focus is never handed out. Tests assign the field directly to set up
    /// states.
    focused_leaf: NodeId,
    /// Current and target scroll offsets — `scroll_x` glides toward
    /// `scroll_target` one frame at a time via `step_scroll`, driven by the
    /// main event loop while they differ (`scroll_animating`). Private so
    /// every mutation goes through the clamping/landing/stepping methods
    /// below.
    scroll_x: i32,
    scroll_target: i32,
    /// Extra scrollable width past the strip reserved for the docked
    /// sidebar (see `Wm::manage_dock`), so scrolling all the way right
    /// reveals it even though it sits outside the strip and doesn't
    /// affect `compute`'s leaf geometry. Zero when nothing is docked.
    /// Private so the only writes (`set_dock_extra`, `clamp_scroll`)
    /// re-clamp or deliberately preserve the scroll against the changed
    /// range.
    dock_extra: i32,
}

impl State {
    pub fn new() -> Self {
        let layout = Layout::new();
        let first = layout.first_leaf();
        Self {
            layout,
            focused_leaf: first,
            scroll_x: 0,
            scroll_target: 0,
            dock_extra: 0,
        }
    }

    pub fn focused_leaf_valid(&self) -> NodeId {
        if self.layout.is_leaf(self.focused_leaf) {
            self.focused_leaf
        } else {
            self.layout.first_leaf()
        }
    }

    /// Point focus at `leaf`. Anything that isn't a live leaf is ignored:
    /// callers can hold ids captured before an intervening mutation, and
    /// focus must never come to rest on a split `compute` doesn't lay out.
    pub fn focus_leaf(&mut self, leaf: NodeId) {
        if self.layout.is_leaf(leaf) {
            self.focused_leaf = leaf;
        }
    }

    /// The column holding the focused split.
    fn focused_col(&self) -> usize {
        self.layout
            .locate(self.focused_leaf_valid())
            .expect("focused_leaf_valid returns a live leaf")
            .col
    }

    // --- window placement helpers ---

    /// Place a newly mapped window: into the focused split if that split
    /// is an empty placeholder, else into a fresh column immediately right
    /// of the focused one — the same insertion the gap `+` button
    /// performs. An empty split that isn't focused attracts nothing; it
    /// waits for a window placed *into* it deliberately. Every window
    /// lives in exactly one split from map to destroy — there is no
    /// off-layout stash.
    ///
    /// `want_w` is the frame width the window's own first-commit size asks
    /// for (`None` when the client stated nothing). It sizes the fresh
    /// column instead of `theme::default_col_w`, and re-sizes a lone
    /// placeholder column the window fills — a stacked placeholder's width
    /// is shared with its siblings, so their deliberate arrangement wins
    /// there. Never below the chrome's minimum; a window asking for the
    /// whole viewport strip (or more) gets `ColWidth::Viewport`, so it
    /// keeps tracking the viewport like the bootstrap column — panels
    /// reserving exclusive zones still resize it.
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
        let focused = self.focused_leaf_valid();
        if self
            .layout
            .leaf(focused)
            .is_some_and(|l| l.client.is_none())
        {
            if let (Some(width), false) = (want, self.layout.stacked(focused)) {
                let pos = self.layout.locate(focused).expect("focused leaf is live");
                self.layout.set_col_width(pos.col, width);
            }
            if let Some(l) = self.layout.leaf_mut(focused) {
                l.show(c);
            }
            return;
        }
        let col = self.focused_col();
        let width = want.unwrap_or(ColWidth::Px(theme::default_col_w(wa.w)));
        let new = self.layout.insert_column(col + 1, width);
        if let Some(l) = self.layout.leaf_mut(new) {
            l.show(c);
        }
        self.focus_leaf(new);
    }

    /// The next time `c` is unpinned (its window destroyed), leave its
    /// split behind as an empty placeholder instead of collapsing it — the
    /// taskbar close badge's semantics. The mark lives on the leaf
    /// (`Leaf::keep_on_close`), so it dies with the split it modifies.
    pub fn retain_split_on_close(&mut self, c: Win) {
        if let Some(lid) = self.layout.find_leaf_for_client(c) {
            if let Some(l) = self.layout.leaf_mut(lid) {
                l.keep_on_close = true;
            }
        }
    }

    /// A window is gone: clear it from its split and collapse that split —
    /// windows and splits live and die together — unless the close was
    /// marked placeholder-keeping (`retain_split_on_close`) or the split is
    /// the strip's sole one (the layout always keeps one). Focus follows to
    /// the nearest surviving neighbour only when the dying split held it.
    /// Returns whether the layout changed (a split collapsed).
    pub fn unpin_client(&mut self, c: Win) -> bool {
        let Some(lid) = self.layout.find_leaf_for_client(c) else {
            return false;
        };
        let mut keep = false;
        if let Some(l) = self.layout.leaf_mut(lid) {
            l.client = None;
            keep = std::mem::take(&mut l.keep_on_close);
        }
        if keep {
            return false;
        }
        self.collapse_leaf(lid)
    }

    /// Collapse `leaf` out of the strip. A whole column vanishes without
    /// its neighbours resizing (the strip just gets shorter); a row leaves
    /// its stack to the surviving rows, which reclaim the height. Refused
    /// for the strip's sole split. Focus follows to the nearest surviving
    /// neighbour only when the collapsed leaf held it. Returns whether the
    /// layout changed.
    fn collapse_leaf(&mut self, leaf: NodeId) -> bool {
        let had_focus = self.focused_leaf_valid() == leaf;
        let Some(new_focus) = self.layout.remove(leaf) else {
            return false;
        };
        if had_focus {
            self.focus_leaf(new_focus);
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
        let l = self.layout.leaf(self.focused_leaf_valid())?;
        if l.minimized {
            return None;
        }
        l.client
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
        if let Some(l) = self.adjacent_leaf(self.focused_leaf_valid(), next) {
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
        let src = self.focused_leaf_valid();
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

    /// Toggle a leaf's minimized flag (the layout collapses it to min size).
    /// Refused for the strip's sole split: it has no siblings to yield space
    /// to, and its whole-frame restore button is disabled (`LeafMeta::sole`),
    /// so a minimized sole split would be a full-screen strip with no way
    /// back. Returns whether the flag changed.
    pub fn toggle_minimize(&mut self, leaf: NodeId) -> bool {
        if self.layout.sole_split() {
            return false;
        }
        match self.layout.leaf_mut(leaf) {
            Some(l) => {
                l.minimized = !l.minimized;
                true
            }
            None => false,
        }
    }

    // --- splitting / inserting ---

    /// Stack a new empty split below the focused one; the existing window
    /// keeps the major share of the row, and the new placeholder takes the
    /// focus — like every other insert, so the next window opened lands in
    /// the split the user just made room in. Refused for a minimized leaf:
    /// a minimized child cloned from it would be a split state the rest of
    /// the system treats as impossible. Returns whether the split
    /// happened, so callers that queue an animation for the action can
    /// cancel it on refusal.
    pub fn split_focused(&mut self) -> bool {
        let leaf = self.focused_leaf_valid();
        if self.layout.leaf(leaf).is_none_or(|l| l.minimized) {
            return false;
        }
        match self.layout.split_below(leaf, theme::SPLIT_RATIO) {
            Some(new) => {
                self.focus_leaf(new);
                true
            }
            None => false,
        }
    }

    /// Insert a new empty split at `at` — the "+" buttons' semantics. A
    /// column insert gets the default width; a row insert joins the stack
    /// at the average share. The new split becomes focused.
    pub fn insert_at(&mut self, wa: Rect, at: Insert) -> NodeId {
        let new = match at {
            Insert::Col(idx) => self
                .layout
                .insert_column(idx, ColWidth::Px(theme::default_col_w(wa.w))),
            Insert::Row { col, idx } => match self.layout.insert_row(col, idx) {
                Some(id) => id,
                None => return self.focused_leaf_valid(),
            },
        };
        self.focus_leaf(new);
        new
    }

    /// Open a fresh empty column immediately right of the focused one —
    /// the ⊞ button's wide-window action. The new split becomes focused.
    pub fn open_column_right(&mut self, wa: Rect) -> NodeId {
        let col = self.focused_col();
        self.insert_at(wa, Insert::Col(col + 1))
    }

    /// Split the focused column into two side by side whose widths sum to
    /// its current width — the titlebar ⊞'s wide-window action. The window
    /// keeps the golden major share and the fresh placeholder to its right
    /// takes the minor, so the pair occupies exactly the space the window
    /// had. Both shares hold to the chrome minimum, at the cost of the sum
    /// growing past the original when the column was too narrow to halve.
    /// The placeholder becomes focused.
    pub fn split_column_right(&mut self, wa: Rect) -> NodeId {
        let col = self.focused_col();
        let w = self.layout.widths(wa.w, theme::GAP)[col];
        let minor =
            ((f64::from(w) * (1.0 - theme::SPLIT_RATIO)).round() as i32).max(theme::min_split_w());
        let major = (w - minor).max(theme::min_split_w());
        self.layout.set_col_width(col, ColWidth::Px(major));
        let new = self.layout.insert_column(col + 1, ColWidth::Px(minor));
        self.focus_leaf(new);
        new
    }

    /// Remove `leaf` if it is an empty placeholder. An occupied split is
    /// never removed directly — it collapses when its window dies
    /// (`unpin_client`), so a window can't be left splitless. Refused for
    /// the strip's sole split. Focus moves to the nearest surviving
    /// neighbour when the removed leaf held it.
    pub fn remove_empty_leaf(&mut self, leaf: NodeId) -> bool {
        if self.layout.leaf(leaf).is_none_or(|l| l.client.is_some()) {
            return false;
        }
        self.collapse_leaf(leaf)
    }

    // --- resize ---

    /// Grow or shrink the focused split by one keyboard step. A stacked
    /// split trades height with its row neighbour (their sum is exactly
    /// conserved); a lone-in-column split just changes its column's width —
    /// the strip absorbs the difference and no sibling moves. Returns
    /// whether anything changed.
    pub fn resize_focused(&mut self, wa: Rect, grow: bool) -> bool {
        let leaf = self.focused_leaf_valid();
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
        let widths = self.layout.widths(wa.w, theme::GAP);
        let step = (wa.w / 20).max(1);
        let target = widths[pos.col] + if grow { step } else { -step };
        let new_w = target.max(theme::min_split_w());
        if new_w == widths[pos.col] {
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

    /// Apply a gap drag. A column gap sets the left column's width to the
    /// dragged size (later columns slide; nothing resizes); a row gap sets
    /// the split so the upper row occupies fraction `frac` of the two
    /// rows' combined height (their sum is preserved).
    pub fn resize_gap(&mut self, at: GapAt, first_px: i32, combined_px: i32) {
        match at {
            GapAt::Col(idx) => {
                self.layout
                    .set_col_width(idx, ColWidth::Px(first_px.max(theme::min_split_w())));
            }
            GapAt::Row { col, idx } => {
                if combined_px <= 0 {
                    return;
                }
                let frac = (f64::from(first_px) / f64::from(combined_px))
                    .clamp(theme::MIN_SPLIT_FRAC, 1.0 - theme::MIN_SPLIT_FRAC);
                let (a, b) = (Pos { col, row: idx }, Pos { col, row: idx + 1 });
                let (Some(fa), Some(fb)) = (self.layout.row_frac(a), self.layout.row_frac(b))
                else {
                    return;
                };
                let combined = fa + fb;
                self.layout.set_row_frac(a, combined * frac);
                self.layout.set_row_frac(b, combined * (1.0 - frac));
            }
        }
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
        let old_w = self.layout.widths(wa.w, theme::GAP)[col];
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
        let col = if left { 0 } else { self.layout.ncols() - 1 };
        self.resize_col(wa, col, target_w)
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

    /// Pull both scroll positions back into `[min_scroll, max_scroll]`. This is the
    /// companion to `set_dock_extra` not clamping: structural layout
    /// changes, viewport resizes and dock removal shrink the scroll range
    /// and must not strand the viewport past the content, while edge-drag
    /// margins (scroll out of range on purpose) survive everything that
    /// doesn't call this.
    pub fn clamp_scroll(&mut self, wa: Rect, dock_extra: i32) {
        self.dock_extra = dock_extra;
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
    /// glide alongside the 280ms layout animation (`Wm::ANIM_DURATION`).
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
    /// holds, exactly like it does for `Wm::anim`.
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

    /// Gaps between adjacent splits, for drag handles / insert buttons.
    pub fn boundaries(&self, wa: Rect) -> Vec<Boundary> {
        self.layout.boundaries(wa, theme::GAP)
    }

    /// Canvas-space x-span `(start_x, width)` of the leftmost/rightmost
    /// column — used to seed and drive an edge-of-strip resize drag (see
    /// `resize_edge`). With a single column, `left`/`right` both describe
    /// the same span.
    pub fn edge_span(&self, wa: Rect, left: bool) -> Option<(i32, i32)> {
        let gap = theme::GAP;
        let widths = self.layout.widths(wa.w, gap);
        let start_x = wa.x + gap;
        if left {
            Some((start_x, widths[0]))
        } else {
            let n = widths.len();
            let before: i32 = widths[..n - 1].iter().sum();
            let gaps_before = gap * i32::try_from(n - 1).unwrap_or(0);
            Some((start_x + before + gaps_before, widths[n - 1]))
        }
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
    const GAP: i32 = crate::theme::GAP;

    fn leaf_clients(s: &State) -> Vec<Option<Win>> {
        s.layout
            .collect_leaves()
            .into_iter()
            .map(|l| s.layout.leaf(l).unwrap().client)
            .collect()
    }

    /// A new window fills the focused split when (and only when) that
    /// split is an empty placeholder.
    #[test]
    fn place_fills_focused_empty_placeholder() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        assert_eq!(s.focused_client(), Some(1));
        assert_eq!(s.layout.collect_leaves().len(), 1, "no new column opened");
    }

    /// A stated preferred width sizes the window's column: the bootstrap
    /// placeholder stops tracking the viewport, and a fresh column opens
    /// at the hint instead of `default_col_w`.
    #[test]
    fn place_honors_the_windows_preferred_width() {
        let mut s = State::new();
        s.place_new_window(WA, 1, Some(500));
        assert_eq!(s.layout.widths(WA.w, GAP)[0], 500, "bootstrap fill");
        s.place_new_window(WA, 2, Some(300));
        assert_eq!(s.layout.widths(WA.w, GAP)[1], 300, "fresh column");
    }

    /// Preferred widths are clamped to sane bounds: a hint at (or past)
    /// the viewport strip's width becomes `Viewport` — the column keeps
    /// tracking viewport changes like the bootstrap one — and one below
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

    /// A hint never resizes a stacked placeholder's column — its width is
    /// shared with siblings the user already arranged.
    #[test]
    fn place_hint_leaves_stacked_placeholder_width_alone() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.split_focused(); // stack below window 1; placeholder focused
        let before = s.layout.widths(WA.w, GAP)[0];
        s.place_new_window(WA, 2, Some(300));
        assert_eq!(s.layout.widths(WA.w, GAP)[0], before);
        assert_eq!(s.focused_client(), Some(2), "still fills the placeholder");
    }

    /// An empty split that is *not* focused attracts nothing: the new
    /// window opens its own column even though a placeholder exists.
    #[test]
    fn place_ignores_unfocused_placeholders() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.insert_at(WA, Insert::Col(0)); // placeholder column, focused
        let placeholder = s.focused_leaf_valid();
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.place_new_window(WA, 2, None);
        assert_eq!(s.layout.leaf(placeholder).unwrap().client, None);
        assert_eq!(s.focused_client(), Some(2));
        assert_eq!(leaf_clients(&s), vec![None, Some(1), Some(2)]);
    }

    /// With the focused split occupied, a new window opens in a fresh
    /// column immediately right of the focused one, and gets the focus.
    #[test]
    fn place_opens_a_column_right_of_the_focused_one() {
        let mut s = State::new();
        for w in [1, 2, 3] {
            s.place_new_window(WA, w, None);
        }
        assert_eq!(leaf_clients(&s), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(s.focused_client(), Some(3));

        // Opening from the middle lands between, not at the end.
        s.focus_leaf(s.layout.find_leaf_for_client(2).unwrap());
        s.place_new_window(WA, 4, None);
        assert_eq!(leaf_clients(&s), vec![Some(1), Some(2), Some(4), Some(3)]);
    }

    /// Stacking focuses the fresh placeholder, so the next window lands in
    /// the room just made; an *occupied* stacked split sends a new window
    /// to a fresh column beside its whole stack, not into the stack.
    #[test]
    fn place_from_a_stacked_split_opens_a_column() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.split_focused(); // column 0 becomes a stack; its placeholder focused
        s.place_new_window(WA, 3, None);
        assert_eq!(
            s.layout.locate(s.layout.find_leaf_for_client(3).unwrap()),
            Some(Pos { col: 0, row: 1 }),
            "fills the placeholder the split just opened"
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
        s.split_focused();
        s.focus_leaf(s.layout.collect_leaves()[1]);
        s.place_new_window(WA, 2, None); // fills the focused empty bottom row
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

    /// The last split survives its window: the layout always keeps one leaf.
    #[test]
    fn unpin_of_the_last_window_leaves_the_root_placeholder() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        assert!(!s.unpin_client(1), "no rect moved");
        assert_eq!(s.layout.collect_leaves().len(), 1);
        assert_eq!(s.focused_client(), None);
    }

    /// A placeholder-keeping close (`retain_split_on_close`, the taskbar
    /// badge) leaves the split empty instead of collapsing it, exactly
    /// once — and a later plain destroy in the same split collapses again.
    #[test]
    fn retained_close_leaves_a_placeholder_once() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.retain_split_on_close(2);
        assert!(!s.unpin_client(2));
        assert_eq!(s.layout.collect_leaves().len(), 2, "placeholder kept");
        let placeholder = s.layout.collect_leaves()[1];
        s.focus_leaf(placeholder);
        s.place_new_window(WA, 3, None);
        assert_eq!(s.layout.leaf(placeholder).unwrap().client, Some(3));
        assert!(s.unpin_client(3), "the mark was consumed: collapse again");
        assert_eq!(s.layout.collect_leaves().len(), 1);
    }

    #[test]
    fn remove_empty_leaf_refuses_occupied_and_sole() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        let occupied = s.focused_leaf_valid();
        assert!(!s.remove_empty_leaf(occupied), "occupied split");
        s.unpin_client(1);
        let sole = s.focused_leaf_valid();
        assert!(!s.remove_empty_leaf(sole), "sole placeholder");
        let extra = s.insert_at(WA, Insert::Col(s.layout.ncols()));
        assert!(s.remove_empty_leaf(extra));
    }

    #[test]
    fn remove_empty_leaf_moves_focus_to_neighbour() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.insert_at(WA, Insert::Col(s.layout.ncols()));
        assert!(s.remove_empty_leaf(s.focused_leaf_valid()));
        assert_eq!(s.focused_client(), Some(1));
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
        assert_eq!(leaf_clients(&s), vec![Some(1), Some(3), Some(2)]);
        assert_eq!(s.focused_client(), Some(2), "focus follows the move");
        assert!(s.move_focused_split(WA, true), "wraps to the front");
        assert_eq!(leaf_clients(&s), vec![Some(2), Some(1), Some(3)]);
    }

    /// Moving within a stack reorders the rows instead of leaving the
    /// column.
    #[test]
    fn move_focused_split_reorders_within_a_stack() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.split_focused();
        s.focus_leaf(s.layout.collect_leaves()[1]);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        assert!(s.move_focused_split(WA, true));
        assert_eq!(s.layout.ncols(), 1, "stayed one column");
        assert_eq!(leaf_clients(&s), vec![Some(2), Some(1)]);
    }

    #[test]
    fn toggle_minimize_refuses_the_sole_split() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        assert!(!s.toggle_minimize(s.focused_leaf_valid()));
        s.place_new_window(WA, 2, None);
        assert!(s.toggle_minimize(s.focused_leaf_valid()));
    }

    #[test]
    fn split_focused_refuses_a_minimized_leaf() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.toggle_minimize(s.focused_leaf_valid());
        assert!(!s.split_focused());
        assert_eq!(s.layout.collect_leaves().len(), 2);
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
        s.split_focused();
        let rows = s.layout.collect_leaves();
        s.focus_leaf(rows[0]);
        let before = s.compute(WA);
        let pair = before[&rows[0]].h + before[&rows[1]].h;
        assert!(s.resize_focused(WA, true));
        let after = s.compute(WA);
        assert!(after[&rows[0]].h > before[&rows[0]].h);
        assert_eq!(after[&rows[0]].h + after[&rows[1]].h, pair);
    }

    /// A row-gap drag preserves the two rows' combined height.
    #[test]
    fn resize_gap_preserves_row_sum() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.split_focused();
        let rows = s.layout.collect_leaves();
        let before = s.compute(WA);
        let pair = before[&rows[0]].h + before[&rows[1]].h;
        s.resize_gap(GapAt::Row { col: 0, idx: 0 }, pair / 4, pair);
        let after = s.compute(WA);
        assert_eq!(after[&rows[0]].h + after[&rows[1]].h, pair);
        assert!(after[&rows[0]].h < before[&rows[0]].h);
    }

    /// A column-gap drag sets the left column's width; the right neighbour
    /// keeps its width and slides.
    #[test]
    fn resize_gap_col_moves_only_the_left_column() {
        let mut s = State::new();
        for w in [1, 2] {
            s.place_new_window(WA, w, None);
        }
        let l1 = s.layout.find_leaf_for_client(1).unwrap();
        let l2 = s.layout.find_leaf_for_client(2).unwrap();
        let before = s.compute(WA);
        s.resize_gap(GapAt::Col(0), before[&l1].w + 100, 0);
        let after = s.compute(WA);
        assert_eq!(after[&l1].w, before[&l1].w + 100);
        assert_eq!(after[&l2].w, before[&l2].w);
        assert_eq!(after[&l2].x, before[&l2].x + 100, "right column slides");
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
        s.toggle_minimize(s.focused_leaf_valid()); // rightmost pinned
        assert_eq!(s.resize_edge(WA, false, 500), 0);
    }

    /// The "+" buttons: a column insert focuses a fresh placeholder at the
    /// gap; a row insert grows that stack.
    #[test]
    fn insert_at_places_and_focuses() {
        let mut s = State::new();
        for w in [1, 2] {
            s.place_new_window(WA, w, None);
        }
        s.insert_at(WA, Insert::Col(1));
        assert_eq!(leaf_clients(&s), vec![Some(1), None, Some(2)]);
        assert_eq!(s.focused_client(), None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.split_focused();
        s.insert_at(WA, Insert::Row { col: 0, idx: 1 });
        assert_eq!(s.layout.col_len(0), 3);
        assert_eq!(
            s.layout.locate(s.focused_leaf_valid()),
            Some(Pos { col: 0, row: 1 })
        );
    }

    /// The ⊞ button's wide-window action: a fresh focused column right of
    /// the focused one, even from inside a stack.
    #[test]
    fn open_column_right_lands_beside_the_stack() {
        let mut s = State::new();
        s.place_new_window(WA, 1, None);
        s.place_new_window(WA, 2, None);
        s.focus_leaf(s.layout.find_leaf_for_client(1).unwrap());
        s.split_focused();
        let new = s.open_column_right(WA);
        assert_eq!(s.layout.locate(new), Some(Pos { col: 1, row: 0 }));
        assert_eq!(s.focused_leaf_valid(), new);
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
        let first = s.layout.first_leaf();
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
            let win = s.layout.leaf(last).unwrap().client.unwrap();
            s.unpin_client(win);
            s.clamp_scroll(WA, 0);
        }
        assert_eq!(s.scroll_x(), 0, "single column: nothing to scroll");
    }
}
