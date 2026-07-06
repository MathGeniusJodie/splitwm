//! App-icon subsystem: the `_NET_WM_ICON` cache, icon-theme fallback fetches
//! (run on a background thread since they can shell out to ImageMagick), and
//! the hue-rotation slots that keep same-app windows visually distinct.

use std::rc::Rc;

use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

use super::types::{WindowKind, Wm, R};
use crate::icon::{self, Icon};
use crate::theme;
use crate::tree::Win;

/// Minimum spacing between `_NET_WM_ICON` fetches per window (see
/// `Wm::on_icon_change`). Long enough to blunt a rewrite loop, short
/// enough that a real icon change still lands promptly.
const ICON_FETCH_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(500);

/// Per-window `_NET_WM_ICON` fetch cooldown/staleness bookkeeping, shared by
/// `Client` and `FloatWin` (a float is no less able to rewrite its icon in a
/// loop than a tiled client is). `fetched` moves up to 16 MiB of
/// client-controlled property data and ends in a full recomposite each time,
/// so `Wm::on_icon_change` rate-limits against it via `throttled`; a notify
/// that lands inside the cooldown is remembered via `stale` and re-fetched by
/// `Wm::flush_stale_icons` once the cooldown passes, so a burst's final icon
/// is never lost.
pub struct IconFreshness {
    fetched: std::time::Instant,
    stale: bool,
}

impl Default for IconFreshness {
    /// Fresh bookkeeping for a window just managed: no cooldown in effect,
    /// nothing stale.
    fn default() -> Self {
        Self {
            fetched: std::time::Instant::now(),
            stale: false,
        }
    }
}

impl IconFreshness {
    /// Whether an icon refresh requested `now` falls inside the cooldown
    /// since the last fetch. When it does, marks the refresh stale (for
    /// `due`/`flush_stale_icons` to pick up later) instead of performing it.
    /// When it doesn't, stamps `now` as the new fetch time and clears any
    /// prior staleness, on the assumption the caller will fetch immediately.
    pub fn throttled(&mut self, now: std::time::Instant) -> bool {
        if now.duration_since(self.fetched) < ICON_FETCH_COOLDOWN {
            self.stale = true;
            true
        } else {
            self.fetched = now;
            self.stale = false;
            false
        }
    }

    /// Whether a previously throttled refresh is ready to retry at `now`.
    pub fn due(&self, now: std::time::Instant) -> bool {
        self.stale && now.duration_since(self.fetched) >= ICON_FETCH_COOLDOWN
    }

    /// Whether a refresh is deferred, regardless of whether its cooldown has
    /// elapsed yet — used to keep `Wm::icons_stale` set for an entry that's
    /// stale but still inside the cooldown, so a later batch still retries
    /// it once `due` turns true.
    pub fn is_stale(&self) -> bool {
        self.stale
    }
}

/// Result of a background theme-icon fetch (see
/// `Wm::spawn_theme_icon_fetch`), tagged with the window it was resolved
/// for. By the time this arrives `win` may already be unmanaged — the
/// receiver must check before applying it, same as `Wm::on_icon_change`
/// already does for its own late-arriving fetch.
pub struct IconResult {
    pub win: Win,
    /// `None` when the theme lookup/decode failed — nothing to apply, but
    /// still worth draining so the channel doesn't grow unbounded.
    pub icon: Option<Icon>,
}

impl Wm {
    /// The non-empty, nul-separated parts of `win`'s `WM_CLASS` property
    /// ("instance\0class\0"), or empty if it has none set. Shared by every
    /// `WM_CLASS` consumer (`client_identity`, `Wm::matches_dock`) so the
    /// property fetch and its truncation cap live in exactly one place.
    pub(crate) fn wm_class_parts(&self, win: Win) -> Vec<Vec<u8>> {
        self.conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default()
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    }

    /// WM_CLASS's class string (second of the "instance\0class\0" pair),
    /// used both as the taskbar label's source letter and to group windows
    /// of the same app for icon color-rotation (`assign_icon_slot`).
    pub(crate) fn client_identity(&self, win: Win) -> Rc<str> {
        let parts = self.wm_class_parts(win);
        let name = parts
            .get(1)
            .or_else(|| parts.first())
            .map_or(&b"?"[..], |p| p.as_slice());
        Rc::from(String::from_utf8_lossy(name).as_ref())
    }

    /// The taskbar/titlebar fallback glyph for a `client_identity` class
    /// string: its first character, uppercased, or `?` when the class is
    /// empty. Shared by every chrome that labels a window by its class
    /// (`Wm::manage`, `Wm::manage_float`).
    pub(crate) fn label_from_class(class: &str) -> char {
        class.chars().next().map_or('?', |c| c.to_ascii_uppercase())
    }

    /// First hue-rotation slot (see `theme::icon_hue_rotation`) not already
    /// held by another open window of `class`, so windows of one app stay
    /// distinguishable while a free slot remains. Freeing is implicit: once
    /// a window is unmanaged it drops out of `managed`, so its slot no
    /// longer counts as used.
    // Only `theme::ICON_HUE_STEPS` distinct hues exist for disambiguating
    // same-class windows; once that many are already in use, this silently
    // returns `None` (no rotated icon) for the next one instead of erroring
    // or reusing a hue, so windows past the step count share an icon look.
    pub(crate) fn assign_icon_slot(&self, class: &str) -> Option<usize> {
        let used: std::collections::HashSet<usize> = self
            .tiled_iter()
            .map(|(_, c)| c)
            .filter(|c| c.class.as_ref() == class)
            .filter_map(|c| c.icon_slot)
            .collect();
        (0..theme::ICON_HUE_STEPS).find(|s| !used.contains(s))
    }

    /// Render (once) the hue-rotated icon variant for every window of
    /// `class`, as soon as the class has two windows open. Slot 0 is a 0°
    /// rotation and keeps using the base icon; already-rendered variants
    /// are kept (the slot is persistent for the window's lifetime).
    pub(crate) fn refresh_icon_rotations(&mut self, class: &Rc<str>) {
        let wins: Vec<Win> = self
            .tiled_iter()
            .filter(|(_, c)| c.class == *class)
            .map(|(w, _)| w)
            .collect();
        if wins.len() < 2 {
            return;
        }
        for win in wins {
            let (slot, icon) = match self.tiled_get(win) {
                Some(c) if c.icon_rotated.is_none() => (c.icon_slot, c.icon.clone()),
                _ => continue,
            };
            let (Some(slot), Some(icon)) = (slot, icon) else {
                continue;
            };
            if slot == 0 {
                continue;
            }
            let rotated = Rc::new(icon::rotate(
                self.renderer.palette(),
                &icon,
                theme::icon_hue_rotation(slot),
            ));
            if let Some(c) = self.client_mut(win) {
                c.icon_rotated = Some(rotated);
            }
        }
    }

    /// The icon to draw for `win`: the pre-rendered hue-rotated variant
    /// while another window of the same app class is open (same-app
    /// disambiguation), the plain icon otherwise.
    pub(crate) fn icon_for(&self, win: Win) -> Option<Rc<Icon>> {
        let client = self.tiled_get(win)?;
        let siblings = self
            .tiled_iter()
            .map(|(_, c)| c)
            .filter(|c| c.class == client.class)
            .count();
        if siblings >= 2 {
            if let Some(rotated) = &client.icon_rotated {
                return Some(rotated.clone());
            }
        }
        client.icon.clone()
    }

    /// Read `_NET_WM_ICON` and pick the icon whose size is closest to (but
    /// preferably >=) the titlebar height. The property is a list of
    /// `width, height, w*h ARGB pixels` blocks packed as 32-bit CARDINALs.
    pub(crate) fn fetch_icon(&self, win: Win) -> Option<Rc<Icon>> {
        // Capped read, not u32::MAX: the property is client-controlled, and
        // every other icon path bounds what a hostile client can make us
        // buffer. 4M CARDINALs (16 MiB) fits a generous multi-size icon set;
        // a bigger property just loses its trailing blocks.
        const MAX_ICON_U32S: u32 = 4 * 1024 * 1024;
        let reply = self
            .conn
            .get_property(
                false,
                win,
                self.atoms.net_wm_icon,
                AtomEnum::CARDINAL,
                0,
                MAX_ICON_U32S,
            )
            .ok()?
            .reply()
            .ok()?;
        let vals: Vec<u32> = reply.value32()?.collect();
        let want = theme::tb_h() as u32;
        let (w, h, start) = best_icon_block(&vals, want)?;
        let argb = vals[start..start + (w * h) as usize].to_vec();
        let icon = Icon::new(w, h, argb);
        // Quantize to the na16 chrome palette so app icons render as flat
        // pixel art matching the rest of the UI, and so the (rotate + snap)
        // hue-rotation for same-app disambiguation stays crisp.
        Some(Rc::new(icon::quantize(self.renderer.palette(), &icon)))
    }

    /// App icon from `_NET_WM_ICON`, falling back to the icon theme for
    /// clients that don't provide it (some apps, e.g. Electron ones, set the
    /// property late or not at all — see `on_icon_change` for the late
    /// case). The theme lookup can shell out to ImageMagick, which must not
    /// block the event loop, so a missing `_NET_WM_ICON` leaves `icon: None`
    /// for now and starts a background fetch (`spawn_theme_icon_fetch`),
    /// filled in later by `on_icon_ping` if/when it succeeds.
    pub(crate) fn resolve_icon(&mut self, win: Win, class: &Rc<str>) -> Option<Rc<Icon>> {
        let icon = self.fetch_icon(win);
        if icon.is_none() {
            self.spawn_theme_icon_fetch(win, class.clone());
        }
        icon
    }

    /// Resolve `class`'s theme icon off the event loop: `find_icon_file` can
    /// stat an NFS-backed icon theme directory and `icon::load_image` shells
    /// out to ImageMagick and waits on it (see `icon::magick_decode_rgba`) —
    /// both slow enough that running them inline in `manage` would stall
    /// every window map and all input handling for as long as they take.
    /// Runs the same theme lookup `resolve_icon` used to do inline, just
    /// off-thread; the quantize step stays on the main thread since it needs
    /// `self.renderer.palette()` (a borrow, not `Send`).
    fn spawn_theme_icon_fetch(&self, win: Win, class: Rc<str>) {
        let tx = self.icon_tx.clone();
        // `Rc<str>` isn't `Send`; the thread only needs the string data, not
        // this window's handle to it.
        let class = class.to_string();
        let atom = self.atoms.splitwm_icon;
        super::spawn_masked(move || {
            let icon = crate::launch::find_icon_file(&class)
                .or_else(|| crate::launch::find_icon_file(&class.to_lowercase()))
                .and_then(|path| icon::load_image(&path));
            // A send failure just means the WM is shutting down (the
            // receiver dropped with it); nothing to report.
            let _ = tx.send(IconResult { win, icon });
            crate::ping::ping(atom);
        });
    }

    /// `_NET_WM_ICON` changed on a managed window: refetch and redraw. Apps
    /// that set the property only after mapping (Electron, notably) would
    /// otherwise keep whatever `manage`/`manage_float` resolved at map time.
    /// Dispatches on `kind_of` since tiled clients and floats keep their
    /// `icon_fresh` bookkeeping and repaint (`repaint_chrome` vs.
    /// `paint_float_frame`) in different places.
    pub(crate) fn on_icon_change(&mut self, win: Win) -> R<()> {
        match self.kind_of(win) {
            Some(WindowKind::Tiled) => self.on_icon_change_tiled(win),
            Some(WindowKind::Float) => self.on_icon_change_float(win),
            Some(WindowKind::Dock | WindowKind::Notification) | None => Ok(()),
        }
    }

    fn on_icon_change_tiled(&mut self, win: Win) -> R<()> {
        let now = std::time::Instant::now();
        let Some(client) = self.client_mut(win) else {
            return Ok(());
        };
        if client.icon_fresh.throttled(now) {
            self.icons_stale = true;
            return Ok(());
        }
        let class = client.class.clone();
        let Some(icon) = self.fetch_icon(win) else {
            return Ok(());
        };
        let client = self
            .client_mut(win)
            .expect("present above; fetch_icon doesn't unmanage");
        client.icon = Some(icon);
        client.icon_rotated = None;
        self.refresh_icon_rotations(&class);
        self.repaint_chrome()
    }

    /// Float counterpart of `on_icon_change_tiled`: same `IconFreshness`
    /// mechanics on `FloatWin`'s own field, but repaints just this float's
    /// chrome frame instead of the shared tiled composite.
    fn on_icon_change_float(&mut self, win: Win) -> R<()> {
        let now = std::time::Instant::now();
        let Some(float) = self.float_mut(win) else {
            return Ok(());
        };
        if float.icon_fresh.throttled(now) {
            self.icons_stale = true;
            return Ok(());
        }
        let Some(icon) = self.fetch_icon(win) else {
            return Ok(());
        };
        let Some(float) = self.float_mut(win) else {
            return Ok(());
        };
        float.icon = Some(icon);
        let frame = float.frame;
        self.paint_float_frame(frame)
    }

    /// Re-fetch icons whose refresh was deferred by `on_icon_change`'s
    /// rate limit, across both tiled clients and floats. Runs once per
    /// event batch (the WM has no timers, so "after the cooldown" means the
    /// first batch that arrives past it — a pointer motion at the latest);
    /// the `icons_stale` flag keeps the usual no-stale-icons batch from
    /// paying for a clients-and-floats scan.
    pub(crate) fn flush_stale_icons(&mut self) -> R<()> {
        if !self.icons_stale {
            return Ok(());
        }
        let now = std::time::Instant::now();
        let due: Vec<Win> = self
            .tiled_iter()
            .filter(|(_, c)| c.icon_fresh.due(now))
            .map(|(w, _)| w)
            .chain(
                self.floats_iter()
                    .filter(|f| f.icon_fresh.due(now))
                    .map(|f| f.win),
            )
            .collect();
        for win in due {
            self.on_icon_change(win)?;
        }
        self.icons_stale = self.tiled_iter().any(|(_, c)| c.icon_fresh.is_stale())
            || self.floats_iter().any(|f| f.icon_fresh.is_stale());
        Ok(())
    }

    /// A background theme-icon fetch (`spawn_theme_icon_fetch`) pinged us:
    /// drain its channel and apply whatever results are ready. Mirrors
    /// `on_icon_change`'s "icon arrived late, apply it and redraw" handling;
    /// a window can easily be unmanaged by the time its fetch resolves
    /// (closed right after mapping), so results for it are just dropped,
    /// same as `on_note_ping`'s guard against a since-vanished popup. A
    /// client or float whose real `_NET_WM_ICON` arrived while this fetch was
    /// in flight already has `icon: Some(_)`, so the fallback is skipped
    /// rather than clobbering it with the generic theme icon — both
    /// `on_icon_change` and this ping race that way, for clients and floats
    /// alike. Per-item errors are contained (not `?`-propagated) for the same
    /// reason `on_note_ping` contains its own: this channel has no other
    /// wakeup, so an early return here would strand every result still
    /// queued behind the failed one.
    pub(crate) fn on_icon_ping(&mut self) -> R<()> {
        let mut changed = false;
        while let Ok(IconResult { win, icon }) = self.icon_rx.try_recv() {
            let Some(img) = icon else { continue };
            let icon = Rc::new(icon::quantize(self.renderer.palette(), &img));
            match self.kind_of(win) {
                Some(WindowKind::Tiled) => {
                    let Some(client) = self.client_mut(win) else {
                        continue;
                    };
                    if client.icon.is_none() {
                        client.icon = Some(icon);
                        client.icon_rotated = None;
                        let class = client.class.clone();
                        self.refresh_icon_rotations(&class);
                        changed = true;
                    }
                }
                Some(WindowKind::Float) => {
                    let Some(float) = self.float_mut(win) else {
                        continue;
                    };
                    if float.icon.is_none() {
                        float.icon = Some(icon);
                        let frame = float.frame;
                        if let Err(e) = self.paint_float_frame(frame) {
                            eprintln!("splitwm: failed to paint float frame after icon fetch: {e}");
                        }
                    }
                }
                Some(WindowKind::Dock | WindowKind::Notification) | None => {}
            }
        }
        if changed {
            self.repaint_chrome()?;
        }
        Ok(())
    }
}

/// Walk a `_NET_WM_ICON` value (a list of `width, height, w*h ARGB pixels`
/// blocks packed as 32-bit CARDINALs) and pick the block whose size best
/// matches `want`: the smallest whose *smaller dimension* still covers it,
/// otherwise the largest available — judging by `min(w, h)` so a degenerate
/// wide-but-short block can't beat a square one that actually covers the
/// target box. Returns `(w, h, pixel_start)`; `None` when no valid block
/// exists (empty, zero-sized, or truncated property).
fn best_icon_block(vals: &[u32], want: u32) -> Option<(u32, u32, usize)> {
    let mut i = 0;
    let mut best: Option<(u32, u32, usize)> = None;
    while i + 2 <= vals.len() {
        let (w, h) = (vals[i], vals[i + 1]);
        let start = i + 2;
        // An overflowing w*h header makes the block's extent unknowable, so
        // the walk can't step past it — stop, keeping any best already
        // found from the valid leading blocks.
        let Some(count) = (w as usize).checked_mul(h as usize) else {
            break;
        };
        if w == 0 || h == 0 || start + count > vals.len() {
            break;
        }
        let m = w.min(h);
        let better = match best {
            None => true,
            Some((bw, bh, _)) => {
                let bm = bw.min(bh);
                (m >= want && (bm < want || m < bm)) || (bm < want && m > bm)
            }
        };
        if better {
            best = Some((w, h, start));
        }
        i = start + count;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::best_icon_block;

    fn block(w: u32, h: u32) -> Vec<u32> {
        let mut v = vec![w, h];
        v.extend(std::iter::repeat_n(0, (w * h) as usize));
        v
    }

    #[test]
    fn prefers_smallest_size_covering_want() {
        let mut vals = block(16, 16);
        vals.extend(block(64, 64));
        vals.extend(block(32, 32));
        assert_eq!(best_icon_block(&vals, 27).map(|b| b.0), Some(32));
    }

    #[test]
    fn falls_back_to_largest_when_none_cover() {
        let mut vals = block(16, 16);
        vals.extend(block(24, 24));
        assert_eq!(best_icon_block(&vals, 48).map(|b| b.0), Some(24));
    }

    #[test]
    fn truncated_or_zero_blocks_stop_cleanly() {
        // Header claims 16x16 but the pixels are missing.
        assert_eq!(best_icon_block(&[16, 16, 0, 0], 16), None);
        assert_eq!(best_icon_block(&[0, 0], 16), None);
        assert_eq!(best_icon_block(&[], 16), None);
        // A huge w*h that would overflow the block walk must not panic.
        assert_eq!(best_icon_block(&[u32::MAX, u32::MAX, 0], 16), None);
    }

    #[test]
    fn valid_leading_block_survives_trailing_garbage() {
        let mut vals = block(16, 16);
        vals.extend([32, 32, 0]); // truncated second block
        assert_eq!(best_icon_block(&vals, 16).map(|b| b.0), Some(16));
        // A trailing block whose w*h overflows must not discard the valid
        // best already found.
        let mut vals = block(16, 16);
        vals.extend([u32::MAX, u32::MAX, 0]);
        assert_eq!(best_icon_block(&vals, 16).map(|b| b.0), Some(16));
    }
}
