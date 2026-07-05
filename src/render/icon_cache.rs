//! Blitting an `Icon` (app icon) into the framebuffer, and the per-(icon,
//! size) scaled-and-quantized index cache behind it. Shared by the titlebar
//! icon (`chrome`) and the taskbar tiles (`taskbar`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pixel_graphics::Rgb as PgRgb;

use crate::icon::Icon;
use crate::Index;

use super::{insert_capped, Renderer};

/// Palette index is a valid `Index` for every real colour, so a distinct
/// out-of-band value marks "no pixel here" in the icon index cache.
pub(super) const TRANSPARENT_INDEX: Index = Index::MAX;

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
        self.for_each_icon_pixel(img, dx, dy, size, |px, py, i| fb.set_pixel(px, py, i));
    }

    /// Walk `img`'s cached `size`x`size` nearest-scaled index buffer
    /// (`cached_icon_indices`), invoking `paint` at each opaque pixel's
    /// destination `(px, py)` and palette index — the scale/clip/
    /// skip-transparent logic shared by `draw_icon` (paints the icon) and
    /// `taskbar::draw_icon_shadow` (paints an offset, flattened silhouette)
    /// so it's written once for both.
    pub(super) fn for_each_icon_pixel(
        &self,
        img: &Icon,
        dx: i32,
        dy: i32,
        size: i32,
        mut paint: impl FnMut(usize, usize, Index),
    ) {
        if img.w == 0 || img.h == 0 || size < 1 {
            return;
        }
        let sz = size as usize;
        let idx = self.cached_icon_indices(img, size);
        for ty in 0..size {
            let py = dy + ty;
            if py < 0 {
                continue;
            }
            for tx in 0..size {
                let px = dx + tx;
                if px < 0 {
                    continue;
                }
                let i = idx[ty as usize * sz + tx as usize];
                if i == TRANSPARENT_INDEX {
                    continue;
                }
                paint(px as usize, py as usize, i);
            }
        }
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
