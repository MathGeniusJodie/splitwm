//! Blitting an `Icon` (app icon) into the framebuffer, and the per-(icon,
//! size) scaled-and-quantized index cache behind it. Shared by the titlebar
//! icon (`chrome`) and the taskbar tiles (`taskbar`).
//! The letter that stands in for an icon that never loaded is cached the
//! same way, per character: it is a silhouette like any other, and every
//! pass that walks an icon walks a letter too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pixel_graphics::Rgb as PgRgb;

use crate::icon::Icon;
use crate::Index;

use super::{insert_capped, Renderer};

/// Palette index is a valid `Index` for every real colour, so a distinct
/// out-of-band value marks "no pixel here" in the icon index cache — the
/// same out-of-band value the framebuffers use.
pub(super) const TRANSPARENT_INDEX: Index = pixel_graphics::TRANSPARENT;

/// Entry cap on the icon render caches. Entries for dropped icons are never
/// individually evicted (nothing tracks icon lifetimes here), so the maps
/// are wholesale-cleared at this size — icon churn (e.g. repeated
/// `_NET_WM_ICON` updates) then costs an occasional re-render instead of
/// unbounded growth. Live icons repopulate on the next frame.
const ICON_CACHE_CAP: usize = 256;

/// A per-(icon id, size) render cache; `Rc` payloads make a hit a refcount
/// bump rather than a buffer copy.
pub(super) type IconCache<T> = RefCell<HashMap<(u64, i32), Rc<[T]>>>;

impl Renderer {
    /// Blit `img` nearest-scaled to a `size`x`size` box at (dx, dy). Icons
    /// are pre-quantized to palette colours, so each drawn pixel resolves to
    /// a palette index (nearest match) and alpha thresholds at 50%.
    pub(super) fn draw_icon(
        &self,
        fb: &mut pixel_graphics::Framebuffer,
        img: &Icon,
        dx: i32,
        dy: i32,
        size: i32,
    ) {
        self.for_each_icon_pixel(img, dx, dy, size, |px, py, i| {
            fb.set_pixel(px as isize, py as isize, i);
        });
    }

    /// Walk `img`'s cached `size`x`size` nearest-scaled index buffer
    /// (`cached_icon_indices`), invoking `paint` at each opaque pixel's
    /// destination `(px, py)` and palette index — the scale/skip-transparent
    /// logic shared by `draw_icon` (paints the icon) and the taskbar's
    /// shadow and shard passes so it's written once for all of them.
    /// Destinations are signed and unclipped: a caller may move a pixel
    /// anywhere before drawing it (the shards do), and `set_pixel` drops
    /// whatever lands off the buffer.
    pub(super) fn for_each_icon_pixel(
        &self,
        img: &Icon,
        dx: i32,
        dy: i32,
        size: i32,
        paint: impl FnMut(i32, i32, Index),
    ) {
        if img.w == 0 || img.h == 0 || size < 1 {
            return;
        }
        let sz = size as usize;
        walk_indices(&self.cached_icon_indices(img, size), sz, dx, dy, paint);
    }

    /// Walk the ink of `ch` in `label_font`, centred at (cx, cy), reporting
    /// each pixel exactly as `for_each_icon_pixel` reports an icon's.
    pub(super) fn for_each_glyph_pixel(
        &self,
        ch: char,
        cx: i32,
        cy: i32,
        paint: impl FnMut(i32, i32, Index),
    ) {
        let Some((w, h, idx)) = self.cached_glyph_indices(ch) else {
            return;
        };
        walk_indices(&idx, w, cx - w as i32 / 2, cy - h as i32 / 2, paint);
    }

    /// The index buffer of `ch` rendered in `label_font`, with its cell
    /// size. The font draws into a framebuffer rather than enumerating
    /// itself, so the glyph is rasterized into a scratch cell — once per
    /// character, since the label font has a single fixed size, and every
    /// pass over a lettered slot then replays the same buffer.
    fn cached_glyph_indices(&self, ch: char) -> Option<(usize, usize, Rc<[Index]>)> {
        let font = self.label_font.as_ref()?;
        let mut buf = [0u8; 4];
        let s = &*ch.encode_utf8(&mut buf);
        let (w, h) = (font.text_width(s), font.cell_h());
        if w == 0 || h == 0 {
            return None;
        }
        if let Some(v) = self.glyph_idx_cache.borrow().get(&ch) {
            return Some((w, h, Rc::clone(v)));
        }
        let mut cell = pixel_graphics::Framebuffer::new(w, h, pixel_graphics::TRANSPARENT);
        font.draw_text(&mut cell, s, 0, 0, self.fg);
        let idx: Rc<[Index]> = (0..h).flat_map(|y| cell.row(y as isize).to_vec()).collect();
        insert_capped(
            &mut self.glyph_idx_cache.borrow_mut(),
            ICON_CACHE_CAP,
            ch,
            Rc::clone(&idx),
        );
        Some((w, h, idx))
    }

    /// The `size`x`size` nearest-scaled palette-index buffer for `img`
    /// (`TRANSPARENT_INDEX` where alpha < 50%), computed once per
    /// icon+size and reused every frame after. Aspect-preserving: the
    /// icon's larger dimension maps to `size` and the other scales
    /// proportionally, centred — a non-square `_NET_WM_ICON` block renders
    /// letterboxed on transparent padding instead of stretched.
    pub(super) fn cached_icon_indices(&self, img: &Icon, size: i32) -> Rc<[u8]> {
        // Callers (`draw_icon`) pre-check dims; the `img.h - 1` /
        // `img.w - 1` below would wrap to u32::MAX on a zero-sized icon,
        // and the cast lints that would flag it are allowed module-wide.
        debug_assert!(
            img.w > 0 && img.h > 0 && size >= 1,
            "cached_icon_indices needs non-empty icon and positive size"
        );
        let key = (img.id(), size);
        if let Some(v) = self.icon_idx_cache.borrow().get(&key) {
            return Rc::clone(v);
        }
        let sz = size as usize;
        let (iw, ih) = (img.w as usize, img.h as usize);
        let (dw, dh) = if iw >= ih {
            (sz, (ih * sz / iw).max(1))
        } else {
            ((iw * sz / ih).max(1), sz)
        };
        let (ox, oy) = ((sz - dw) / 2, (sz - dh) / 2);
        let mut idx = vec![TRANSPARENT_INDEX; sz * sz];
        for ty in 0..dh {
            let sy = (ty * ih / dh).min(ih - 1);
            for tx in 0..dw {
                let sx = (tx * iw / dw).min(iw - 1);
                let s = img.argb[sy * iw + sx];
                if (s >> 24) & 0xff < 128 {
                    continue;
                }
                idx[(oy + ty) * sz + ox + tx] = self.palette.nearest_index(PgRgb {
                    r: ((s >> 16) & 0xff) as u8,
                    g: ((s >> 8) & 0xff) as u8,
                    b: (s & 0xff) as u8,
                });
            }
        }
        let idx: Rc<[u8]> = idx.into();
        insert_capped(
            &mut self.icon_idx_cache.borrow_mut(),
            ICON_CACHE_CAP,
            key,
            Rc::clone(&idx),
        );
        idx
    }
}

/// Walk a `w`-wide index buffer laid out row-major, invoking `paint` at
/// each opaque pixel, positioned at (`dx`, `dy`). Destinations are signed
/// and unclipped: a caller may move a pixel anywhere before drawing it
/// (the taskbar's shards do), and `set_pixel` drops whatever lands off the
/// buffer.
fn walk_indices(idx: &[Index], w: usize, dx: i32, dy: i32, mut paint: impl FnMut(i32, i32, Index)) {
    for (n, &i) in idx.iter().enumerate() {
        if i == TRANSPARENT_INDEX {
            continue;
        }
        let (tx, ty) = (n % w, n / w);
        paint(dx + tx as i32, dy + ty as i32, i);
    }
}
