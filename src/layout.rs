//! Pure layout math for the flat column strip.
//!
//! The layout is a left-to-right list of *columns*; a column holds one
//! split, or one vertical *stack* of splits. That is the entire shape —
//! deeper nesting is unrepresentable. Columns own their width in pixels
//! (the scrollable strip is exactly the columns laid end to end), while
//! rows within a stack hold fractions of the column's height, which is
//! always the viewport's.

use std::collections::HashMap;

pub type Win = u32;

/// Stable identity of one split, minted by `Layout` and kept for the
/// split's whole life across moves and resizes — animations, hit regions
/// and focus all key on it. A newtype rather than a second bare `u32`
/// alias so a split id and a `Win` can never be swapped at a call site.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone)]
pub struct Leaf {
    /// The window this split shows. Splits and windows are the same thing
    /// seen from two sides: a split exists because a window does, and dies
    /// with it.
    pub client: Win,
    pub minimized: bool,
    /// Persistent accent palette index for this split (kept across moves
    /// and closes), used to palette-swap the bitmap window border.
    pub color: crate::Index,
}

/// A split's place in the strip: which column, and which row within it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pos {
    pub col: usize,
    pub row: usize,
}

/// A column's width. `Viewport` tracks the viewport (the bootstrap column
/// fills the screen until something resizes it); every other column is a
/// plain pixel width. Resolved in one place (`Layout::widths`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColWidth {
    Viewport,
    Px(i32),
}

/// One stacked split: its identity, its share of the column's height, and
/// the split's own data. The leaf lives inline — a row without leaf data
/// (or an orphaned leaf) is unrepresentable.
struct Row {
    id: NodeId,
    frac: f64,
    leaf: Leaf,
}

/// One column of the strip. `rows` is private so its "at least one row"
/// invariant holds by construction.
pub struct Column {
    width: ColWidth,
    rows: Vec<Row>,
}

pub struct Layout {
    /// Empty while no window is open — the strip then shows bare
    /// wallpaper, since a split without a window cannot exist.
    columns: Vec<Column>,
    next_id: u32,
}

impl Layout {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            next_id: 1,
        }
    }

    fn gen_id(&mut self) -> NodeId {
        let id = self.next_id;
        // Id uniqueness is the core invariant: every live split is
        // addressed by its id, so a silent wraparound here would hand out
        // an id that already aliases a live split instead of failing
        // loudly.
        self.next_id = id
            .checked_add(1)
            .unwrap_or_else(|| panic!("Layout::gen_id: NodeId space exhausted (next_id={id})"));
        NodeId(id)
    }

    /// An accent index no existing leaf currently has, so two splits never
    /// look the same while a free colour remains. Falls back to the
    /// id-cycled colour (which may collide) once every leaf has a distinct
    /// entry in `theme::LEAF_PALETTE`.
    fn unused_leaf_color(&self, id: NodeId) -> crate::Index {
        let used: std::collections::HashSet<crate::Index> =
            self.rows().map(|(_, r)| r.leaf.color).collect();
        crate::theme::LEAF_PALETTE
            .into_iter()
            .find(|c| !used.contains(c))
            .unwrap_or_else(|| crate::theme::cycled_leaf_color(id.0))
    }

    fn make_row(&mut self, frac: f64, client: Win) -> Row {
        let id = self.gen_id();
        Row {
            id,
            frac,
            leaf: Leaf {
                client,
                minimized: false,
                color: self.unused_leaf_color(id),
            },
        }
    }

    /// Every row with its position, column-major (the strip's visual and
    /// focus-cycle order).
    fn rows(&self) -> impl Iterator<Item = (Pos, &Row)> {
        self.columns.iter().enumerate().flat_map(|(ci, c)| {
            c.rows
                .iter()
                .enumerate()
                .map(move |(ri, r)| (Pos { col: ci, row: ri }, r))
        })
    }

    fn row(&self, pos: Pos) -> Option<&Row> {
        self.columns.get(pos.col)?.rows.get(pos.row)
    }

    // --- queries ---

    pub fn leaf(&self, id: NodeId) -> Option<&Leaf> {
        self.rows().find(|(_, r)| r.id == id).map(|(_, r)| &r.leaf)
    }

    pub fn leaf_mut(&mut self, id: NodeId) -> Option<&mut Leaf> {
        self.columns
            .iter_mut()
            .flat_map(|c| c.rows.iter_mut())
            .find(|r| r.id == id)
            .map(|r| &mut r.leaf)
    }

    pub fn is_leaf(&self, id: NodeId) -> bool {
        self.locate(id).is_some()
    }

    /// Where `id` sits in the strip, or `None` for a dead id.
    pub fn locate(&self, id: NodeId) -> Option<Pos> {
        self.rows().find(|(_, r)| r.id == id).map(|(p, _)| p)
    }

    /// The split at `pos`, or `None` past the strip's edges.
    pub fn leaf_at(&self, pos: Pos) -> Option<NodeId> {
        self.row(pos).map(|r| r.id)
    }

    /// Split ids in strip order (columns left to right, rows top to
    /// bottom) — also the taskbar's tile order.
    pub fn collect_leaves(&self) -> Vec<NodeId> {
        self.rows().map(|(_, r)| r.id).collect()
    }

    /// The strip's first split (top-left), or `None` while it is empty.
    pub fn first_leaf(&self) -> Option<NodeId> {
        Some(self.columns.first()?.rows[0].id)
    }

    pub fn find_leaf_for_client(&self, c: Win) -> Option<NodeId> {
        self.rows()
            .find(|(_, r)| r.leaf.client == c)
            .map(|(_, r)| r.id)
    }

    pub fn ncols(&self) -> usize {
        self.columns.len()
    }

    pub fn col_len(&self, col: usize) -> usize {
        self.columns.get(col).map_or(0, |c| c.rows.len())
    }

    /// Whether `id` shares its column with other rows (its minimized form
    /// is a horizontal strip, and stack semantics apply on close).
    pub fn stacked(&self, id: NodeId) -> bool {
        self.locate(id)
            .is_some_and(|p| self.columns[p.col].rows.len() > 1)
    }

    #[cfg(test)]
    pub fn col_width(&self, col: usize) -> Option<ColWidth> {
        self.columns.get(col).map(|c| c.width)
    }

    // --- mutations ---

    /// Insert a new column at `at` (clamped) with the given width, holding
    /// `client`. Returns the new split.
    pub fn insert_column(&mut self, at: usize, width: ColWidth, client: Win) -> NodeId {
        let row = self.make_row(1.0, client);
        let id = row.id;
        let at = at.min(self.columns.len());
        self.columns.insert(
            at,
            Column {
                width,
                rows: vec![row],
            },
        );
        id
    }

    /// Insert a new row holding `client` into `col`'s stack at `at`
    /// (clamped), taking the average share of the column's height (all
    /// renormalised). Returns the new split, or `None` for a dead column
    /// index.
    #[allow(clippy::cast_precision_loss)]
    pub fn insert_row(&mut self, col: usize, at: usize, client: Win) -> Option<NodeId> {
        let n = self.columns.get(col)?.rows.len();
        let avg = self.columns[col].rows.iter().map(|r| r.frac).sum::<f64>() / n as f64;
        let row = self.make_row(avg, client);
        let id = row.id;
        let c = &mut self.columns[col];
        c.rows.insert(at.min(n), row);
        let s: f64 = c.rows.iter().map(|r| r.frac).sum();
        for r in &mut c.rows {
            r.frac /= s;
        }
        Some(id)
    }

    /// Remove the split `id`. A row leaving a stack hands its share to the
    /// surviving rows (they reclaim the height — their extent is fixed by
    /// the viewport); a lone row takes its whole column with it, and the
    /// strip just gets shorter — sibling columns are untouched. Returns
    /// the split focus should land on: the nearest surviving row in the
    /// same column, else the nearest column's first row — and `None` once
    /// the strip is empty (or for a dead id, which removes nothing).
    pub fn remove(&mut self, id: NodeId) -> Option<NodeId> {
        let p = self.locate(id)?;
        let col = &mut self.columns[p.col];
        if col.rows.len() > 1 {
            let freed = col.rows.remove(p.row).frac;
            let remaining: f64 = col.rows.iter().map(|r| r.frac).sum();
            if remaining > 0.0 {
                for r in &mut col.rows {
                    r.frac += freed * r.frac / remaining;
                }
            }
            let neighbour = p.row.min(col.rows.len() - 1);
            return Some(col.rows[neighbour].id);
        }
        self.columns.remove(p.col);
        let neighbour = p.col.min(self.columns.len().checked_sub(1)?);
        Some(self.columns[neighbour].rows[0].id)
    }

    /// Unhook `src`'s row, dissolving its column if that empties it. The
    /// carried `ColWidth` is the dissolved column's, for moves that
    /// re-create a column elsewhere. Private: the row is a momentary
    /// orphan, and both callers re-attach it before returning.
    fn detach(&mut self, src: NodeId) -> Option<(Row, Option<ColWidth>)> {
        let p = self.locate(src)?;
        let col = &mut self.columns[p.col];
        if col.rows.len() > 1 {
            let mut row = col.rows.remove(p.row);
            let freed = row.frac;
            let remaining: f64 = col.rows.iter().map(|r| r.frac).sum();
            if remaining > 0.0 {
                for r in &mut col.rows {
                    r.frac += freed * r.frac / remaining;
                }
            }
            row.frac = 1.0;
            Some((row, None))
        } else {
            let column = self.columns.remove(p.col);
            let width = column.width;
            let mut row = column.rows.into_iter().next().expect("rows non-empty");
            row.frac = 1.0;
            Some((row, Some(width)))
        }
    }

    /// Relocate split `src` (window, colour and all) into its own column
    /// left (`before`) or right of `dst`'s column. A `src` that was a
    /// whole column keeps its width; one pulled out of a stack gets
    /// `default_w`. Returns whether the strip changed.
    pub fn move_beside_column(
        &mut self,
        src: NodeId,
        dst: NodeId,
        before: bool,
        default_w: ColWidth,
    ) -> bool {
        if src == dst || !self.is_leaf(src) || !self.is_leaf(dst) {
            return false;
        }
        // Already a lone column adjacent to dst's column on the requested
        // side: the move is a no-op; doing it anyway would only churn
        // focus and animations.
        let (sp, dp) = match (self.locate(src), self.locate(dst)) {
            (Some(s), Some(d)) => (s, d),
            _ => return false,
        };
        if self.columns[sp.col].rows.len() == 1 {
            let target = if before {
                dp.col.wrapping_sub(1)
            } else {
                dp.col + 1
            };
            if sp.col == target {
                return false;
            }
        }
        let Some((row, width)) = self.detach(src) else {
            return false;
        };
        // dst survives every detach (only src's row leaves), so re-locating
        // it after the column indices shifted always succeeds.
        let dcol = self.locate(dst).expect("dst survives the detach").col;
        let at = if before { dcol } else { dcol + 1 };
        self.columns.insert(
            at,
            Column {
                width: width.unwrap_or(default_w),
                rows: vec![row],
            },
        );
        true
    }

    /// Relocate split `src` into its own column at strip position `idx`
    /// (clamped), the columns from there on shifting right — what a drop
    /// into a gap, or onto the canvas past the strip's end, means: those
    /// name a place, not a neighbour, so the split can leave a stack even
    /// when nothing else in the strip stands beside the place it lands.
    /// A `src` that was a whole column keeps its width; one pulled out of
    /// a stack gets `default_w`. `idx` counts the columns as they are
    /// *before* the move. Returns whether the strip changed — a lone
    /// column already occupying that position doesn't move.
    pub fn move_to_column(&mut self, src: NodeId, idx: usize, default_w: ColWidth) -> bool {
        let Some(sp) = self.locate(src) else {
            return false;
        };
        let lone = self.columns[sp.col].rows.len() == 1;
        // A lone column is already at `idx` both when it *is* that column
        // and when it is the one just left of it (the columns after it
        // close up as it leaves).
        if lone && (sp.col == idx || sp.col + 1 == idx) {
            return false;
        }
        let Some((row, width)) = self.detach(src) else {
            return false;
        };
        // Detaching a lone column shifted every later column one left.
        let at = if lone && idx > sp.col { idx - 1 } else { idx };
        self.columns.insert(
            at.min(self.columns.len()),
            Column {
                width: width.unwrap_or(default_w),
                rows: vec![row],
            },
        );
        true
    }

    /// Relocate split `src` into `dst`'s stack, directly above (`before`)
    /// or below `dst`'s row, at the average share. Returns whether the
    /// strip changed.
    #[allow(clippy::cast_precision_loss)]
    pub fn move_into_stack(&mut self, src: NodeId, dst: NodeId, before: bool) -> bool {
        if src == dst || !self.is_leaf(src) || !self.is_leaf(dst) {
            return false;
        }
        let Some((mut row, _)) = self.detach(src) else {
            return false;
        };
        let dp = self.locate(dst).expect("dst survives the detach");
        let col = &mut self.columns[dp.col];
        let n = col.rows.len();
        row.frac = col.rows.iter().map(|r| r.frac).sum::<f64>() / n as f64;
        let at = if before { dp.row } else { dp.row + 1 };
        col.rows.insert(at, row);
        let s: f64 = col.rows.iter().map(|r| r.frac).sum();
        for r in &mut col.rows {
            r.frac /= s;
        }
        true
    }

    pub fn set_col_width(&mut self, col: usize, width: ColWidth) {
        if let Some(c) = self.columns.get_mut(col) {
            c.width = width;
        }
    }

    pub fn row_frac(&self, pos: Pos) -> Option<f64> {
        self.row(pos).map(|r| r.frac)
    }

    pub fn set_row_frac(&mut self, pos: Pos, frac: f64) {
        if let Some(r) = self
            .columns
            .get_mut(pos.col)
            .and_then(|c| c.rows.get_mut(pos.row))
        {
            r.frac = frac;
        }
    }
}

// --- geometry ---

/// Vertical allocation within a stack: `children` is (is-minimized,
/// share) per row. A minimized row collapses to `min_sz` (the gap, so it
/// stays visually consistent with the layout's spacing); the rest split
/// what remains by their shares.
fn child_sizes(children: &[(bool, f64)], usable: i32, min_sz: i32) -> Vec<i32> {
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

/// Where a new window opens in the strip. `State::place_new_window`
/// resolves one of these — from the compass wedge that launched the
/// window, or from the focused split when nothing asked for a side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Insert {
    /// A new column at strip position `idx` (`0` = far left, `ncols()` =
    /// far right).
    Col(usize),
    /// A new row at stack position `idx` of column `col` (`0` = top, the
    /// row count = bottom).
    Row { col: usize, idx: usize },
}

/// One of the four sides a new split can open on, relative to an existing
/// split. The taskbar's quick-launch hover compass names its four zones
/// with this; `State::open_split_beside` turns one into the `Insert` it
/// means for a given split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

/// A gap between two adjacent columns, or two adjacent rows of one stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GapAt {
    /// Between column `idx` and `idx + 1` (a vertical gap).
    Col(usize),
    /// Between rows `idx` and `idx + 1` of column `col` (horizontal).
    Row { col: usize, idx: usize },
}

/// A gap's laid-out slot: the place its drag handle sits. Coordinates are
/// canvas-space.
#[derive(Clone, Copy)]
pub struct Boundary {
    pub at: GapAt,
    pub pos: i32,       // gap centre along the drag axis
    pub start: i32,     // first neighbour's start along the drag axis
    pub first: i32,     // first neighbour's size along the drag axis
    pub second: i32,    // second neighbour's size along the drag axis
    pub cross: i32,     // strip start on the cross axis
    pub cross_len: i32, // strip extent on the cross axis
    /// Whether dragging this gap can actually resize: false when a pinned
    /// (minimized) neighbour is involved — its pixel size ignores the
    /// stored share, so the handle would not track the pointer.
    pub resizable: bool,
}

impl Layout {
    /// Whether the whole column is pinned to the gap width: every row
    /// minimized (the lone-minimized-split column collapses to a thin
    /// vertical strip; a stack with any shown row keeps its width).
    pub fn col_pinned(&self, col: usize) -> bool {
        self.columns[col].rows.iter().all(|r| r.leaf.minimized)
    }

    /// Column `col`'s laid-out pixel width: `Viewport` resolves against the
    /// viewport (minus the outer margins), a pinned column to the gap.
    /// The single-column form of `widths`, for callers that would otherwise
    /// build the whole vector to index one entry.
    pub fn col_px(&self, col: usize, viewport_w: i32, gap: i32) -> i32 {
        if self.col_pinned(col) {
            gap
        } else {
            match self.columns[col].width {
                ColWidth::Viewport => (viewport_w - 2 * gap).max(gap),
                ColWidth::Px(w) => w.max(gap),
            }
        }
    }

    /// Each column's laid-out pixel width, left to right (see `col_px`).
    fn widths_iter(&self, viewport_w: i32, gap: i32) -> impl Iterator<Item = i32> + '_ {
        (0..self.columns.len()).map(move |i| self.col_px(i, viewport_w, gap))
    }

    /// Each column's laid-out pixel width as a vector; every non-test
    /// caller gets by with `col_px`/`widths_iter`.
    #[cfg(test)]
    pub fn widths(&self, viewport_w: i32, gap: i32) -> Vec<i32> {
        self.widths_iter(viewport_w, gap).collect()
    }

    /// The scrollable strip's total width: the columns end to end, plus
    /// the outer margins and inter-column gaps. Zero while the strip is
    /// empty — there is no margin around nothing.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn strip_w(&self, viewport_w: i32, gap: i32) -> i32 {
        if self.columns.is_empty() {
            return 0;
        }
        let n = self.columns.len() as i32;
        2 * gap + self.widths_iter(viewport_w, gap).sum::<i32>() + gap * (n - 1)
    }

    /// Canvas-space rect of each column (before per-row subdivision).
    fn col_rects(&self, wa: Rect, gap: i32) -> Vec<Rect> {
        let mut x = wa.x + gap;
        let y = wa.y + gap;
        let h = (wa.h - 2 * gap).max(0);
        self.widths_iter(wa.w, gap)
            .map(|w| {
                let r = Rect { x, y, w, h };
                x += w + gap;
                r
            })
            .collect()
    }

    /// Per-row pixel heights within column `ci` at column height `h`.
    /// A minimized row collapses to the gap — except in a pinned column
    /// (`col_pinned`), whose thin vertical strip spans the full height with
    /// the rows sharing it by their fracs, so each renders as a tall
    /// restore strip rather than a gap-sized square over bare wallpaper.
    fn row_sizes(&self, ci: usize, h: i32, gap: i32) -> Vec<i32> {
        let pinned = self.col_pinned(ci);
        let meta: Vec<(bool, f64)> = self.columns[ci]
            .rows
            .iter()
            .map(|r| (!pinned && r.leaf.minimized, r.frac))
            .collect();
        let n = i32::try_from(meta.len()).unwrap_or(i32::MAX);
        let usable = (h - gap * (n - 1)).max(0);
        child_sizes(&meta, usable, gap)
    }

    /// Canvas-space rect of every split, keyed by id. `wa` is the
    /// *viewport* — the strip extends past its right edge and the caller
    /// applies the scroll offset.
    pub fn compute(&self, wa: Rect, gap: i32) -> HashMap<NodeId, Rect> {
        let mut geos = HashMap::new();
        for (ci, col_rect) in self.col_rects(wa, gap).into_iter().enumerate() {
            let col = &self.columns[ci];
            let sizes = self.row_sizes(ci, col_rect.h, gap);
            let mut y = col_rect.y;
            for (row, sz) in col.rows.iter().zip(&sizes) {
                geos.insert(
                    row.id,
                    Rect {
                        y,
                        h: *sz,
                        ..col_rect
                    },
                );
                y += sz + gap;
            }
        }
        geos
    }

    /// Every gap's laid-out slot: between adjacent columns, and between
    /// adjacent rows within each stack.
    pub fn boundaries(&self, wa: Rect, gap: i32) -> Vec<Boundary> {
        let mut out = Vec::new();
        let col_rects = self.col_rects(wa, gap);
        let strip_y = wa.y + gap;
        let strip_h = (wa.h - 2 * gap).max(0);
        for (i, r) in col_rects.iter().enumerate() {
            if let Some(next) = col_rects.get(i + 1) {
                out.push(Boundary {
                    at: GapAt::Col(i),
                    pos: r.x + r.w + gap / 2,
                    start: r.x,
                    first: r.w,
                    second: next.w,
                    cross: strip_y,
                    cross_len: strip_h,
                    resizable: !self.col_pinned(i) && !self.col_pinned(i + 1),
                });
            }
            let rows = &self.columns[i].rows;
            let sizes = self.row_sizes(i, r.h, gap);
            let mut y = r.y;
            for (ri, sz) in sizes.iter().enumerate() {
                if let Some(next) = sizes.get(ri + 1) {
                    out.push(Boundary {
                        at: GapAt::Row { col: i, idx: ri },
                        pos: y + sz + gap / 2,
                        start: y,
                        first: *sz,
                        second: *next,
                        cross: r.x,
                        cross_len: r.w,
                        resizable: !rows[ri].leaf.minimized && !rows[ri + 1].leaf.minimized,
                    });
                }
                y += sz + gap;
            }
        }
        out
    }
}

#[cfg(test)]
mod child_sizes_tests {
    use super::child_sizes;

    #[test]
    fn sizes_fill_usable_exactly() {
        // Rounding must never leave the last child short or long.
        let kids = [(false, 0.3), (false, 0.3), (false, 0.4)];
        assert_eq!(child_sizes(&kids, 100, 10).iter().sum::<i32>(), 100);
    }

    #[test]
    fn minimized_children_get_min_size() {
        let kids = [(true, 0.5), (false, 0.5)];
        assert_eq!(child_sizes(&kids, 100, 10), vec![10, 90]);
    }

    #[test]
    fn all_minimized_does_not_panic_or_go_negative() {
        let kids = [(true, 0.5), (true, 0.5)];
        assert_eq!(child_sizes(&kids, 100, 10), vec![10, 10]);
    }

    #[test]
    fn skewed_ratios_never_sum_past_usable() {
        let kids = [(false, 100.0), (false, 0.0001)];
        let sizes = child_sizes(&kids, 50, 10);
        assert!(sizes.iter().sum::<i32>() <= 50);
        assert!(sizes.iter().all(|&s| s >= 0));
    }

    #[test]
    fn more_children_than_pixels_never_overlaps() {
        let kids = vec![(false, 0.1); 20];
        let sizes = child_sizes(&kids, 5, 3);
        assert!(sizes.iter().sum::<i32>() <= 5);
        assert!(sizes.iter().all(|&s| s >= 0));
    }

    #[test]
    fn zero_ratio_sum_falls_back() {
        let kids = [(false, 0.0), (false, 0.0)];
        let sizes = child_sizes(&kids, 100, 10);
        assert!(sizes.iter().sum::<i32>() <= 100);
    }

    #[test]
    fn usable_smaller_than_min_total_clamps() {
        let kids = [(true, 0.5), (true, 0.5), (false, 1.0)];
        let sizes = child_sizes(&kids, 5, 10);
        assert!(sizes.iter().all(|&s| s >= 0));
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
    const GAP: i32 = 20;

    /// `n` columns, one window each (client `1..=n`), the first tracking
    /// the viewport as the strip's first column does.
    fn columns(l: &mut Layout, n: usize) -> Vec<NodeId> {
        for i in 0..n {
            let width = if i == 0 {
                ColWidth::Viewport
            } else {
                ColWidth::Px(400)
            };
            l.insert_column(i, width, i as Win + 1);
        }
        l.collect_leaves()
    }

    #[test]
    fn an_empty_strip_lays_out_nothing() {
        let l = Layout::new();
        assert_eq!(l.first_leaf(), None);
        assert!(l.compute(WA, GAP).is_empty());
        assert!(l.boundaries(WA, GAP).is_empty());
        assert_eq!(l.strip_w(WA.w, GAP), 0);
    }

    #[test]
    fn first_column_fills_the_viewport() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 1);
        assert_eq!(l.compute(WA, GAP)[&ids[0]].w, WA.w - 2 * GAP);
        assert_eq!(l.strip_w(WA.w, GAP), WA.w);
    }

    #[test]
    fn inserting_a_column_never_resizes_the_others() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 3);
        let before = l.compute(WA, GAP);
        l.insert_column(1, ColWidth::Px(300), 9);
        let after = l.compute(WA, GAP);
        for id in ids {
            assert_eq!(before[&id].w, after[&id].w);
        }
        assert_eq!(
            l.strip_w(WA.w, GAP),
            WA.w + 300 + GAP + 400 + GAP + 400 + GAP
        );
    }

    #[test]
    fn removing_a_column_never_resizes_the_others() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 3);
        let before = l.compute(WA, GAP);
        let strip = l.strip_w(WA.w, GAP);
        let focus = l.remove(ids[1]).expect("a survivor takes the focus");
        assert_eq!(focus, ids[2], "nearest surviving column's row");
        let after = l.compute(WA, GAP);
        assert_eq!(before[&ids[0]].w, after[&ids[0]].w);
        assert_eq!(before[&ids[2]].w, after[&ids[2]].w);
        assert_eq!(l.strip_w(WA.w, GAP), strip - before[&ids[1]].w - GAP);
    }

    #[test]
    fn removing_a_stacked_row_hands_its_share_to_the_stack() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 1);
        let bottom = l.insert_row(0, 1, 2).expect("live column");
        let geos = l.compute(WA, GAP);
        let full = geos[&ids[0]].h + geos[&bottom].h + GAP;
        let focus = l.remove(bottom).expect("a survivor takes the focus");
        assert_eq!(focus, ids[0]);
        assert_eq!(l.compute(WA, GAP)[&ids[0]].h, full, "height reclaimed");
        assert_eq!(l.ncols(), 1);
    }

    #[test]
    fn removing_the_last_split_empties_the_strip() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 1);
        assert_eq!(l.remove(ids[0]), None, "nothing left to focus");
        assert!(l.collect_leaves().is_empty());
    }

    #[test]
    fn insert_row_takes_the_average_share() {
        let mut l = Layout::new();
        columns(&mut l, 1);
        l.insert_row(0, 1, 2).expect("live column");
        let mid = l.insert_row(0, 1, 3).expect("live column");
        let geos = l.compute(WA, GAP);
        let usable = WA.h - 2 * GAP - 2 * GAP;
        let third = usable / 3;
        assert!(
            (geos[&mid].h - third).abs() <= usable / 10,
            "roughly a third"
        );
        assert_eq!(l.col_len(0), 3);
    }

    #[test]
    fn move_beside_column_reorders_and_carries_width() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 3);
        l.set_col_width(2, ColWidth::Px(555));
        assert!(l.move_beside_column(ids[2], ids[0], true, ColWidth::Px(1)));
        assert_eq!(l.collect_leaves(), vec![ids[2], ids[0], ids[1]]);
        assert_eq!(l.col_width(0), Some(ColWidth::Px(555)), "width travels");
        // Already in place: a repeat is a no-op.
        assert!(!l.move_beside_column(ids[2], ids[0], true, ColWidth::Px(1)));
    }

    #[test]
    fn move_out_of_a_stack_gets_the_default_width() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        let bottom = l.insert_row(0, 1, 3).expect("live column");
        assert!(l.move_beside_column(bottom, ids[1], false, ColWidth::Px(321)));
        assert_eq!(l.col_width(2), Some(ColWidth::Px(321)));
        assert_eq!(l.col_len(0), 1, "the stack dissolved to a lone row");
    }

    /// A place, not a neighbour: a stacked row moves out into a column
    /// at the named position — including the position right beside its
    /// own column, where naming a neighbour would have named itself.
    #[test]
    fn move_to_column_pulls_a_row_out_of_its_own_stack() {
        for (idx, expect) in [(0, vec![1, 2]), (1, vec![2, 1]), (2, vec![2, 1])] {
            let mut l = Layout::new();
            let ids = columns(&mut l, 1);
            let bottom = l.insert_row(0, 1, 2).expect("live column");
            // The top row leaves the stack; the bottom row keeps column 0.
            assert!(l.move_to_column(ids[0], idx, ColWidth::Px(300)), "{idx}");
            assert_eq!(l.ncols(), 2, "{idx}");
            assert_eq!(l.col_len(0), 1, "{idx}");
            let clients: Vec<Win> = l
                .collect_leaves()
                .iter()
                .map(|&id| l.leaf(id).expect("live").client)
                .collect();
            assert_eq!(clients, expect, "{idx}");
            assert!(l.is_leaf(bottom));
        }
    }

    /// A lone column already at the named position stays put — moving it
    /// would only churn focus and animations.
    #[test]
    fn move_to_column_leaves_a_column_already_there_alone() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 3);
        assert!(
            !l.move_to_column(ids[1], 1, ColWidth::Px(300)),
            "its own slot"
        );
        assert!(
            !l.move_to_column(ids[1], 2, ColWidth::Px(300)),
            "the slot after it"
        );
        assert!(
            l.move_to_column(ids[1], 3, ColWidth::Px(300)),
            "past the end"
        );
        assert_eq!(l.collect_leaves(), vec![ids[0], ids[2], ids[1]]);
    }

    #[test]
    fn move_into_stack_joins_at_the_drop_side() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        assert!(l.move_into_stack(ids[1], ids[0], false));
        assert_eq!(l.ncols(), 1);
        assert_eq!(l.locate(ids[1]), Some(Pos { col: 0, row: 1 }));
        // Detaching a lone column while targeting a stack keeps working.
        let third = l.insert_column(1, ColWidth::Px(200), 3);
        assert!(l.move_into_stack(third, ids[0], true));
        assert_eq!(l.locate(third), Some(Pos { col: 0, row: 0 }));
        assert_eq!(l.col_len(0), 3);
    }

    #[test]
    fn detached_leaf_keeps_its_window_and_color() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        let color = l.leaf(ids[1]).expect("live").color;
        assert!(l.move_beside_column(ids[1], ids[0], true, ColWidth::Px(1)));
        assert_eq!(l.leaf(ids[1]).expect("live").client, 2);
        assert_eq!(l.leaf(ids[1]).expect("live").color, color);
    }

    #[test]
    fn minimized_lone_column_pins_to_the_gap() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        l.leaf_mut(ids[0]).expect("live").minimized = true;
        assert_eq!(l.widths(WA.w, GAP)[0], GAP);
        let b: Vec<_> = l.boundaries(WA, GAP);
        assert!(b
            .iter()
            .all(|b| !matches!(b.at, GapAt::Col(_)) || !b.resizable));
    }

    #[test]
    fn pinned_column_rows_span_the_full_height() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        let bottom = l.insert_row(0, 1, 3).expect("live column");
        l.leaf_mut(ids[0]).expect("live").minimized = true;
        l.leaf_mut(bottom).expect("live").minimized = true;
        let geos = l.compute(WA, GAP);
        let strip_h = WA.h - 2 * GAP;
        assert_eq!(
            geos[&ids[0]].h + geos[&bottom].h + GAP,
            strip_h,
            "rows share the column's full height"
        );
        // Taller than the gap-wide column, so each renders as the vertical
        // restore strip (winmin.png), not the stacked-row one.
        assert!(geos[&ids[0]].h > geos[&ids[0]].w);
        assert!(geos[&bottom].h > geos[&bottom].w);
    }

    #[test]
    fn minimized_row_pins_within_its_stack_only() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 2);
        let bottom = l.insert_row(0, 1, 3).expect("live column");
        l.leaf_mut(bottom).expect("live").minimized = true;
        let geos = l.compute(WA, GAP);
        assert_eq!(geos[&bottom].h, GAP, "row pinned");
        assert_eq!(l.widths(WA.w, GAP)[0], WA.w - 2 * GAP, "width kept");
        assert!(ids.contains(&l.first_leaf().expect("non-empty")));
    }

    #[test]
    fn splits_get_unique_colors_up_to_palette_size() {
        let mut l = Layout::new();
        let n = crate::theme::LEAF_PALETTE.len();
        columns(&mut l, n);
        let mut colors: Vec<_> = l
            .collect_leaves()
            .iter()
            .map(|&id| l.leaf(id).expect("live").color)
            .collect();
        colors.sort_unstable();
        colors.dedup();
        assert_eq!(
            colors.len(),
            n,
            "no colour repeats before the palette runs out"
        );
    }

    #[test]
    fn boundaries_cover_every_gap_once() {
        let mut l = Layout::new();
        let ids = columns(&mut l, 3);
        l.insert_row(1, 1, 4).expect("live column");
        let b = l.boundaries(WA, GAP);
        let cols = b.iter().filter(|b| matches!(b.at, GapAt::Col(_))).count();
        let rows = b
            .iter()
            .filter(|b| matches!(b.at, GapAt::Row { .. }))
            .count();
        assert_eq!((cols, rows), (2, 1));
        assert_eq!(l.locate(ids[1]), Some(Pos { col: 1, row: 0 }));
    }
}
