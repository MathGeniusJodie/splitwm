//! The bottom taskbar's own drawing: icon tiles (with drop shadow and
//! shown-in-a-split highlight), the separator pill before the quick-launch
//! icons, the close badge on each tile, and the quick-launch icons, which
//! break into the four compass shards under the pointer.

use pixel_graphics::{Framebuffer, Paint as PgPaint, PaletteIndex};

use crate::icon::Icon;
use crate::layout::Side;
use crate::theme::palette_color;
use crate::Index;

use super::{fill, fill_paint, Renderer};

/// Pixel offset (down and right) of a taskbar icon's drop shadow from the
/// icon itself.
const SHADOW_OFFSET: i32 = 2;

/// Flat colour a taskbar icon's drop shadow silhouette is drawn in.
const SHADOW_COLOR: Index = palette_color::BLACK;

/// How far each shard of a fully split quick-launch icon slides out along
/// its own axis, in pixels. The four slide apart rather than being cut
/// apart: the seams between them are the space they leave behind, so they
/// land on the compass diagonals without the drawing pass knowing where
/// those run.
const SHARD_SPREAD: i32 = 5;

/// How much wider a quick-launch icon grows, in pixels across, as it splits
/// — the icon comes forward as it breaks up.
const SHARD_GROWTH: i32 = 10;

/// Thickness of the border traced around a quick-launch icon's silhouette.
const ICON_BORDER: i32 = 2;

/// How far a slot's icon sits inside the slot rect on every side.
const SLOT_ICON_INSET: i32 = 3;

/// How far past its slot a fully split quick-launch icon reaches: the
/// growth it takes on each side, the slide of its shards, and the border
/// around them, less the room the slot already leaves. The taskbar strip
/// runs this far above the bar so a split icon has somewhere to go.
pub const QUICK_ICON_REACH: i32 = SHARD_GROWTH / 2 + SHARD_SPREAD + ICON_BORDER - SLOT_ICON_INSET;

/// One slot's icon as it sits on the bar: the content that fills it, the
/// box it is scaled into, the slot centre the letter fallback sits at, and
/// how far it has broken into its four compass shards. One value, so the
/// box and the shards can never be built from different phases.
struct SlotIcon<'a> {
    icon: Option<&'a Icon>,
    label: char,
    /// Top-left corner of the icon box, and its side.
    x: i32,
    y: i32,
    size: i32,
    /// How far each shard has slid out along its own axis — the other half
    /// of what the split phase resolves to.
    spread: i32,
    /// What is traced around this icon's silhouette; everything not
    /// traced is shadowed.
    trace: Trace,
}

impl<'a> SlotIcon<'a> {
    /// The icon of slot `r`, its box grown and its shards slid out by
    /// `split` (0 = whole, at the resting size) about the slot's centre.
    fn new(
        r: crate::layout::Rect,
        icon: Option<&'a Icon>,
        label: char,
        split: f32,
        trace: Trace,
    ) -> Self {
        let (grown, spread) = shard_steps(split);
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        let size = r.h.min(r.w) - 2 * SLOT_ICON_INSET + grown;
        Self {
            icon,
            label,
            x: cx - size / 2,
            y: cy - size / 2,
            size,
            spread,
            trace,
        }
    }

    /// The slot's centre, where the letter fallback sits. Truncating
    /// division puts the box back on it exactly.
    fn centre(&self) -> (i32, i32) {
        (self.x + self.size / 2, self.y + self.size / 2)
    }

    /// The wedge a pixel of the icon belongs to — `compass_side`, the same
    /// answer the compass hit-test gives, so a shard covers exactly the
    /// wedge that launches its direction. Doubled coordinates keep the
    /// centre of an even-sided box exact.
    fn wedge(&self, px: i32, py: i32) -> Side {
        crate::widgets::compass_side(
            2 * px + 1 - (2 * self.x + self.size),
            2 * py + 1 - (2 * self.y + self.size),
        )
    }

    /// Where a pixel of `wedge` lands once its shard has slid out. The
    /// four slide apart rather than being cut apart, so the seams between
    /// them are the space they leave behind.
    fn place(&self, wedge: Side, px: i32, py: i32) -> (i32, i32) {
        match wedge {
            Side::Left => (px - self.spread, py),
            Side::Right => (px + self.spread, py),
            Side::Up => (px, py - self.spread),
            Side::Down => (px, py + self.spread),
        }
    }

    /// The colour tracing a pixel of `wedge`, if anything does; `None`
    /// means it is shadowed instead.
    fn traced(&self, wedge: Side) -> Option<Index> {
        match self.trace {
            Trace::Nothing => None,
            Trace::Whole(accent) => Some(accent),
            Trace::Aimed(side) => (wedge == side).then_some(palette_color::CREAM),
        }
    }
}

/// The only two integers a split phase resolves to on screen: how much
/// wider the icon box is, and how far its shards have slid. Two phases
/// that agree here draw the same pixels, so this is the granularity a
/// repaint fingerprint needs — anything finer redraws for nothing.
pub fn shard_steps(split: f32) -> (i32, i32) {
    let split = split.max(0.0);
    (
        (SHARD_GROWTH as f32 * split) as i32,
        (SHARD_SPREAD as f32 * split) as i32,
    )
}

/// What traces a slot icon's silhouette.
#[derive(Clone, Copy)]
enum Trace {
    /// Nothing: the icon is shadowed whole, the resting look.
    Nothing,
    /// The whole silhouette, in the accent of the split the window is
    /// shown in — how a taskbar tile marks itself.
    Whole(Index),
    /// Only the shard under the pointer, in cream — a quick-launch icon
    /// the compass is aimed at. Its other three shards stay shadowed.
    Aimed(Side),
}

/// The stamp that traces one icon pixel: every offset within
/// `ICON_BORDER` of it, corners rounded off, so an icon's silhouette comes
/// out ringed by a border of that thickness. Lazy, so stamping a pixel
/// allocates nothing.
fn border_stamp() -> impl Iterator<Item = (i32, i32)> {
    let r = ICON_BORDER;
    (-r..=r)
        .flat_map(move |oy| (-r..=r).map(move |ox| (ox, oy)))
        .filter(move |&(ox, oy)| ox * ox + oy * oy <= r * r + 1)
}

impl Renderer {
    /// Draw one taskbar window tile: the app icon (or letter-glyph
    /// fallback) centred in its slot directly on the bar background,
    /// traced in the accent of the split the window is shown in.
    pub fn draw_taskbar_tile(
        &self,
        fb: &mut Framebuffer,
        r: crate::layout::Rect,
        icon: Option<&Icon>,
        label: char,
        accent: Index,
    ) {
        self.draw_slot(fb, r, icon, label, 0.0, Trace::Whole(accent));
    }

    /// Draw one quick-launch icon, broken `split` of the way into the four
    /// compass shards (0 = whole, 1 = fully apart) and grown as it breaks
    /// up, so the icon states the same four choices its compass does.
    /// The shard under the pointer (`hover`) is traced in cream; the
    /// others keep the drop shadow, as does an icon resting whole with
    /// nothing aimed at it.
    pub fn draw_quick_item(
        &self,
        fb: &mut Framebuffer,
        r: crate::layout::Rect,
        icon: Option<&Icon>,
        label: char,
        split: f32,
        hover: Option<Side>,
    ) {
        self.draw_slot(
            fb,
            r,
            icon,
            label,
            split,
            hover.map_or(Trace::Nothing, Trace::Aimed),
        );
    }

    /// Draw one taskbar slot: its backing, then the icon (or letter) over
    /// it, both broken `split` of the way into their shards.
    fn draw_slot(
        &self,
        fb: &mut Framebuffer,
        r: crate::layout::Rect,
        icon: Option<&Icon>,
        label: char,
        split: f32,
        trace: Trace,
    ) {
        let slot = SlotIcon::new(r, icon, label, split, trace);
        self.draw_slot_backing(fb, &slot);
        self.draw_slot_icon(fb, &slot);
    }

    /// Walk the pixels of whatever fills a slot: its app icon, or the
    /// letter standing in for one. Both are silhouettes, so the shard,
    /// trace and shadow passes need no idea which they are looking at —
    /// only the letter's size is fixed, where an icon scales with the box.
    fn for_each_slot_pixel(&self, slot: &SlotIcon, paint: impl FnMut(i32, i32, Index)) {
        self.for_each_slot_pixel_where(slot, false, paint);
    }

    /// Like `for_each_slot_pixel`, but visits only the rim of the icon's (or
    /// letter's) silhouette — the pixels a traced border actually needs to
    /// grow outward from.
    fn for_each_slot_edge_pixel(&self, slot: &SlotIcon, paint: impl FnMut(i32, i32, Index)) {
        self.for_each_slot_pixel_where(slot, true, paint);
    }

    fn for_each_slot_pixel_where(
        &self,
        slot: &SlotIcon,
        edge_only: bool,
        paint: impl FnMut(i32, i32, Index),
    ) {
        match slot.icon {
            Some(img) => {
                self.for_each_icon_pixel_where(img, slot.x, slot.y, slot.size, edge_only, paint);
            }
            None => {
                let (cx, cy) = slot.centre();
                self.for_each_glyph_pixel_where(slot.label, cx, cy, edge_only, paint);
            }
        }
    }

    /// Draw what sits behind a slot's icon: whatever the shards say is
    /// traced gets a border around its own silhouette, and everything else
    /// drops a shadow. Both follow the shards, so a split icon's backing
    /// breaks along exactly the seams they open and travels out with them
    /// instead of lying under them uncut. Shadows go down first: where a
    /// trace meets a neighbouring shard's shadow, the trace wins.
    fn draw_slot_backing(&self, fb: &mut Framebuffer, slot: &SlotIcon) {
        // A wholly traced icon casts no shadow, and an untraced one has
        // nothing to trace: only an aimed icon needs both walks.
        if !matches!(slot.trace, Trace::Whole(_)) {
            self.for_each_slot_pixel(slot, |px, py, _| {
                let wedge = slot.wedge(px, py);
                if slot.traced(wedge).is_some() {
                    return;
                }
                let (sx, sy) = slot.place(wedge, px, py);
                fb.set_pixel(
                    (sx + SHADOW_OFFSET) as isize,
                    (sy + SHADOW_OFFSET) as isize,
                    SHADOW_COLOR,
                );
            });
        }
        if matches!(slot.trace, Trace::Nothing) {
            return;
        }
        // Trace by stamping each rim pixel's own neighbourhood; the icon
        // pass covers the middle back up, leaving a border of `ICON_BORDER`
        // around the silhouette. Interior pixels need no stamp of their
        // own — their growth lands entirely inside what their rim
        // neighbours already throw.
        self.for_each_slot_edge_pixel(slot, |px, py, _| {
            let wedge = slot.wedge(px, py);
            let Some(color) = slot.traced(wedge) else {
                return;
            };
            let (sx, sy) = slot.place(wedge, px, py);
            for (ox, oy) in border_stamp() {
                fb.set_pixel((sx + ox) as isize, (sy + oy) as isize, color);
            }
        });
    }

    /// A slot's own content, each pixel moved out with its shard.
    fn draw_slot_icon(&self, fb: &mut Framebuffer, slot: &SlotIcon) {
        self.for_each_slot_pixel(slot, |px, py, i| {
            let (sx, sy) = slot.place(slot.wedge(px, py), px, py);
            fb.set_pixel(sx as isize, sy as isize, i);
        });
    }
}

/// Draw the vertical pill separating the taskbar's window tiles from its
/// quick-launch icons: a cream rounded bar, corners notched pixel-art style
/// like the tiles around it.
pub fn draw_taskbar_sep(fb: &mut Framebuffer, r: crate::layout::Rect) {
    fill(fb, r.x + 1, r.y, r.w - 2, r.h, palette_color::CREAM);
    fill(fb, r.x, r.y + 2, r.w, r.h - 4, palette_color::CREAM);
}

/// Inset of the diagonal cross's endpoints from the badge's corners, as a
/// percentage of the badge's overall size; picked by eye so the "x" strokes
/// clear the 1px border drawn around the badge.
const CLOSE_BADGE_INSET_PCT: i32 = 32;

/// Draw the small close ("x") badge in the bottom-right corner of a taskbar
/// tile: a dark square with a cross, always visible so the close affordance
/// needs no hover state.
pub fn draw_close_badge(fb: &mut Framebuffer, x: i32, y: i32, sz: i32) {
    fill_paint(
        fb,
        x + 1,
        y,
        sz - 2,
        sz,
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
    );
    fill_paint(
        fb,
        x,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
    );
    fill_paint(
        fb,
        x + sz - 1,
        y + 1,
        1,
        sz - 2,
        PgPaint::Solid(PaletteIndex::new(palette_color::BLACK)),
    );

    // 2px-thick diagonal cross.
    let inset = sz * CLOSE_BADGE_INSET_PCT / 100;
    let span = sz - 2 * inset;
    for i in 0..span {
        for t in 0..2 {
            let px = x + inset + i;
            let ny = y + inset + i + t; // "\" stroke
            let sy = y + sz - 1 - inset - i + t; // "/" stroke
            if px >= 0 && ny >= 0 {
                fb.set_pixel(px as isize, ny as isize, palette_color::CREAM);
            }
            if px >= 0 && sy >= 0 {
                fb.set_pixel(px as isize, sy as isize, palette_color::CREAM);
            }
        }
    }
}
