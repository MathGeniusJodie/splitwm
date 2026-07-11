//! App-icon resolution and same-app hue rotation. Icons come from the
//! freedesktop icon theme keyed on the window's class (xdg app_id / X11
//! `WM_CLASS`), fetched off the event loop — `find_icon_file` can stat an
//! NFS-backed theme directory and `icon::load_image` shells out to
//! ImageMagick — and land back on the loop over a calloop channel. The
//! per-pixel quantize/rotate steps stay on the main thread: they need the
//! renderer's palette and run once per icon, never per frame.

use std::rc::Rc;

use super::Comp;
use crate::icon::{self, Icon};
use crate::layout::Win;
use crate::shell::Kind;
use crate::theme;

/// An off-thread icon fetch's outcome, delivered over the icon channel.
pub struct IconResult {
    pub win: Win,
    pub icon: Option<Icon>,
}

impl Comp {
    /// First hue-rotation slot (see `theme::icon_hue_rotation`) not already
    /// held by another open tiled window of `class`, so windows of one app
    /// stay distinguishable while a free slot remains. Freeing is
    /// implicit: an unmanaged window drops out of the store, so its slot
    /// no longer counts as used. Past `ICON_HUE_STEPS` windows this
    /// returns `None` (no rotated icon) instead of reusing a hue.
    pub fn assign_icon_slot(&self, class: &str) -> Option<usize> {
        let used: std::collections::HashSet<usize> = self
            .managed
            .tiled_iter()
            .filter(|(_, w)| crate::shell::toplevel_app_id(w).eq_ignore_ascii_case(class))
            .filter_map(|(w, _)| self.managed.entry(w).and_then(|m| m.icon_slot))
            .collect();
        (0..theme::ICON_HUE_STEPS).find(|s| !used.contains(s))
    }

    /// Resolve `class`'s theme icon off the event loop; the result lands
    /// via the icon channel (`on_icon_result`).
    pub fn spawn_icon_fetch(&self, win: Win, class: String) {
        if class.is_empty() {
            return;
        }
        let tx = self.icon_tx.clone();
        std::thread::spawn(move || {
            let icon = crate::launch::find_icon_file(&class)
                .or_else(|| crate::launch::find_icon_file(&class.to_lowercase()))
                .and_then(|path| icon::load_image(&path));
            // A send failure just means the compositor is shutting down.
            let _ = tx.send(IconResult { win, icon });
        });
    }

    /// A fetched icon arrived: quantize onto the palette, store it, and
    /// re-derive the rotations for its class.
    pub fn on_icon_result(&mut self, r: IconResult) {
        let Some(icon) = r.icon else {
            return;
        };
        let quant = Rc::new(icon::quantize(self.view.chrome.palette(), &icon));
        let class = match self.managed.get(r.win) {
            Some(window) => crate::shell::toplevel_app_id(window),
            None => return, // closed while the fetch ran
        };
        let kind_tiled;
        {
            let Some(entry) = self.managed.entry_mut(r.win) else {
                return;
            };
            entry.icon = Some(quant);
            entry.icon_rotated = None;
            kind_tiled = matches!(entry.kind, Kind::Tiled);
        }
        if kind_tiled {
            // The new icon changes the leaf's and taskbar tile's content
            // fingerprints, so the next redraw's `update_chrome_pieces`
            // re-renders just those pieces.
            self.refresh_icon_rotations(&class);
        } else if let Some((_, f)) = self.managed.float_mut(r.win) {
            f.frame.mark_stale();
        }
    }

    /// Render (once) the hue-rotated icon variant for every tiled window
    /// of `class`, as soon as the class has two windows open. Slot 0 is a
    /// 0° rotation and keeps using the base icon; already-rendered
    /// variants are kept (the slot is persistent for the window's
    /// lifetime).
    pub fn refresh_icon_rotations(&mut self, class: &str) {
        let wins: Vec<Win> = self
            .managed
            .tiled_iter()
            .filter(|(_, w)| crate::shell::toplevel_app_id(w).eq_ignore_ascii_case(class))
            .map(|(w, _)| w)
            .collect();
        if wins.len() < 2 {
            return;
        }
        for win in wins {
            let (slot, icon) = match self.managed.entry(win) {
                Some(m) if m.icon_rotated.is_none() => (m.icon_slot, m.icon.clone()),
                _ => continue,
            };
            let (Some(slot), Some(icon)) = (slot, icon) else {
                continue;
            };
            if slot == 0 {
                continue;
            }
            let rotated = Rc::new(icon::rotate(
                self.view.chrome.palette(),
                &icon,
                theme::icon_hue_rotation(slot),
            ));
            if let Some(m) = self.managed.entry_mut(win) {
                m.icon_rotated = Some(rotated);
            }
        }
    }

    /// The icon to draw for `win`: the pre-rendered hue-rotated variant
    /// while another window of the same app class is open (same-app
    /// disambiguation), the plain icon otherwise.
    pub fn icon_for(&self, win: Win) -> Option<Rc<Icon>> {
        let entry = self.managed.entry(win)?;
        let class = crate::shell::toplevel_app_id(&entry.window);
        let siblings = self
            .managed
            .tiled_iter()
            .filter(|(_, w)| crate::shell::toplevel_app_id(w).eq_ignore_ascii_case(&class))
            .count();
        if siblings > 1 {
            entry.icon_rotated.clone().or_else(|| entry.icon.clone())
        } else {
            entry.icon.clone()
        }
    }
}
