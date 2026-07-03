//! Menu-related methods for `Wm`.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode, Window};

use super::types::{FrameRect, Wm, R};
use crate::menu::{frame_size, Item, MENU_BORDER, MENU_ROW_H};
use crate::tree::NodeId;

impl Wm {
    /// Resolve one menu column's `Icon=` names into decoded icons via the
    /// cache. Only successes are cached: a failed lookup retries on the
    /// next open (cheap — the filesystem scan underneath is cached with its
    /// own expiry in `menu::find_icon_file`), so an icon that appears on
    /// disk mid-session starts showing without a WM restart.
    fn resolve_menu_icons(
        &mut self,
        names: &[Option<String>],
    ) -> Vec<Option<std::rc::Rc<crate::icon::Icon>>> {
        // Same wholesale-clear policy as the renderer's icon caches: menus
        // are bounded in practice, but no cache here grows without a cap.
        if self.menu.icon_cache.len() >= 512 {
            self.menu.icon_cache.clear();
        }
        names
            .iter()
            .map(|n| {
                let n = n.as_ref()?;
                if let Some(hit) = self.menu.icon_cache.get(n) {
                    return Some(hit.clone());
                }
                let img = crate::menu::find_icon_file(n)
                    .and_then(|p| crate::icon::load_png(&p))?;
                let icon =
                    std::rc::Rc::new(crate::icon::quantize(self.renderer.palette(), &img));
                self.menu.icon_cache.insert(n.clone(), icon.clone());
                Some(icon)
            })
            .collect()
    }

    /// Open the launcher menu for `leaf`, with its bottom-right corner anchored
    /// at screen (ax, ay) so it rises above the bottom taskbar.
    pub(crate) fn open_menu(&mut self, leaf: NodeId, ax: i32, ay: i32) -> R<()> {
        let labels = self.menu.tree.main.labels.clone();
        let icon_names = self.menu.tree.main.icons.clone();
        self.menu.main_icons = self.resolve_menu_icons(&icon_names);
        let any_icon = self.menu.main_icons.iter().any(Option::is_some);
        let cw = self.renderer.menu_content_w(&labels, true, any_icon);
        let rows = i32::try_from(labels.len()).unwrap_or(0);
        let (w, h) = frame_size(rows, cw);
        let wa = self.wa();
        let x = (ax - w).clamp(wa.x, (wa.x + wa.w - w).max(wa.x));
        let y = (ay - h).clamp(wa.y, (wa.y + wa.h - h).max(wa.y));
        self.menu.main = FrameRect { x, y, w, h };
        self.menu.main_cw = cw;
        self.menu.main_hi = None;
        self.menu.open_cat = None;
        self.menu.target_leaf = leaf;
        self.menu.open = true;
        self.conn.configure_window(
            self.menu.main_win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(u32::try_from(w).unwrap_or(1))
                .height(u32::try_from(h).unwrap_or(1))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.menu.main_win)?;
        self.conn.unmap_window(self.menu.sub_win)?;
        self.raise_menu()?;
        self.paint_menu_main()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Raise the open launcher windows above all clients. No-op when closed.
    /// Clients are raised to the top on every `arrange`/focus, so the menu must
    /// be re-raised afterwards to stay visible.
    pub(crate) fn raise_menu(&self) -> R<()> {
        if !self.menu.open {
            return Ok(());
        }
        let above = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        self.conn.configure_window(self.menu.main_win, &above)?;
        if self.menu.open_cat.is_some() {
            self.conn.configure_window(self.menu.sub_win, &above)?;
        }
        Ok(())
    }

    pub(crate) fn close_menu(&mut self) -> R<()> {
        if !self.menu.open {
            return Ok(());
        }
        self.menu.open = false;
        self.menu.open_cat = None;
        self.conn.unmap_window(self.menu.main_win)?;
        self.conn.unmap_window(self.menu.sub_win)?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn paint_menu_main(&mut self) -> R<()> {
        let m = &self.menu.tree.main;
        let seps: Vec<bool> = m
            .items
            .iter()
            .map(|it| matches!(it, Item::Separator))
            .collect();
        let fb = self.renderer.draw_menu(
            &m.labels,
            &m.arrows,
            &seps,
            &self.menu.main_icons,
            self.menu.main_cw,
            self.menu.main_hi,
        );
        let mut buf = std::mem::take(&mut self.bgrx);
        self.renderer.present(&fb, &mut buf);
        self.bgrx = buf;
        self.put_image(
            self.menu.main_win,
            fb.width as u16,
            fb.height as u16,
            &self.bgrx,
        )?;
        Ok(())
    }

    pub(crate) fn paint_menu_sub(&mut self) -> R<()> {
        let Some(cat) = self.menu.open_cat else {
            return Ok(());
        };
        // `.get`, not indexing: `open_cat` and the tree are only coupled by
        // convention, and a stale index (e.g. after a future menu rescan)
        // must degrade to a no-op rather than a panic.
        let Some(&Item::Submenu(idx)) = self.menu.tree.main.items.get(cat) else {
            return Ok(());
        };
        let Some(sub) = self.menu.tree.subs.get(idx) else {
            return Ok(());
        };
        let seps = vec![false; sub.labels.len()];
        let fb = self.renderer.draw_menu(
            &sub.labels,
            &sub.arrows,
            &seps,
            &self.menu.sub_icons,
            self.menu.sub_cw,
            self.menu.sub_hi,
        );
        let mut buf = std::mem::take(&mut self.bgrx);
        self.renderer.present(&fb, &mut buf);
        self.bgrx = buf;
        self.put_image(
            self.menu.sub_win,
            fb.width as u16,
            fb.height as u16,
            &self.bgrx,
        )?;
        Ok(())
    }

    /// Open the submenu for main row `cat` to the right of that row.
    pub(crate) fn open_submenu(&mut self, cat: usize) -> R<()> {
        let Some(&Item::Submenu(idx)) = self.menu.tree.main.items.get(cat) else {
            return Ok(());
        };
        let Some(sub) = self.menu.tree.subs.get(idx) else {
            return Ok(());
        };
        let labels = sub.labels.clone();
        let icon_names = sub.icons.clone();
        self.menu.sub_icons = self.resolve_menu_icons(&icon_names);
        let any_icon = self.menu.sub_icons.iter().any(Option::is_some);
        let cw = self.renderer.menu_content_w(&labels, false, any_icon);
        let rows = i32::try_from(labels.len()).unwrap_or(0);
        let (w, h) = frame_size(rows, cw);
        let wa = self.wa();
        let row_y = self.menu.main.y + MENU_BORDER + i32::try_from(cat).unwrap_or(0) * MENU_ROW_H;
        let y = (row_y - MENU_BORDER).min(wa.y + wa.h - h).max(wa.y);
        // Prefer the right side; flip left if it would overflow.
        let right_x = self.menu.main.x + self.menu.main.w - MENU_BORDER;
        let x = if right_x + w <= wa.x + wa.w {
            right_x
        } else {
            self.menu.main.x - w + MENU_BORDER
        };
        self.menu.sub_cw = cw;
        self.menu.sub_hi = None;
        self.menu.open_cat = Some(cat);
        self.conn.configure_window(
            self.menu.sub_win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(u32::try_from(w).unwrap_or(1))
                .height(u32::try_from(h).unwrap_or(1))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.menu.sub_win)?;
        self.raise_menu()?;
        self.paint_menu_sub()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Row index under window-local y, or None for the border padding.
    pub(crate) fn menu_row_at(ly: i32, n: usize) -> Option<usize> {
        let inner = ly - MENU_BORDER;
        if inner < 0 {
            return None;
        }
        let row = (inner / MENU_ROW_H) as usize;
        (row < n).then_some(row)
    }

    pub(crate) fn on_menu_motion(&mut self, win: Window, ly: i32) -> R<()> {
        if win == self.menu.main_win {
            let n = self.menu.tree.main.labels.len();
            let row = Self::menu_row_at(ly, n)
                .filter(|&r| !matches!(self.menu.tree.main.items.get(r), Some(Item::Separator)));
            if row != self.menu.main_hi {
                self.menu.main_hi = row;
                self.paint_menu_main()?;
            }
            // Hovering a category opens its submenu; hovering anything else closes it.
            let hovered_cat =
                row.filter(|&r| matches!(self.menu.tree.main.items.get(r), Some(Item::Submenu(_))));
            match hovered_cat {
                Some(r) => {
                    if self.menu.open_cat != Some(r) {
                        self.open_submenu(r)?;
                    }
                }
                None => {
                    if self.menu.open_cat.is_some() {
                        self.menu.open_cat = None;
                        self.conn.unmap_window(self.menu.sub_win)?;
                    }
                }
            }
        } else if win == self.menu.sub_win {
            if let Some(cat) = self.menu.open_cat {
                if let Some(&Item::Submenu(idx)) = self.menu.tree.main.items.get(cat) {
                    let n = self.menu.tree.subs.get(idx).map_or(0, |s| s.labels.len());
                    let row = Self::menu_row_at(ly, n);
                    if row != self.menu.sub_hi {
                        self.menu.sub_hi = row;
                        self.paint_menu_sub()?;
                    }
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Pointer left a menu window: drop its hover highlight. The main
    /// column falls back to highlighting the open category (so a pointer
    /// travelling through the submenu keeps the row it came from lit)
    /// rather than clearing outright.
    pub(crate) fn on_menu_leave(&mut self, win: Window) -> R<()> {
        if win == self.menu.main_win {
            if self.menu.main_hi != self.menu.open_cat {
                self.menu.main_hi = self.menu.open_cat;
                self.paint_menu_main()?;
                self.conn.flush()?;
            }
        } else if win == self.menu.sub_win && self.menu.sub_hi.is_some() {
            self.menu.sub_hi = None;
            self.paint_menu_sub()?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub(crate) fn on_menu_click(&mut self, win: Window, ly: i32) -> R<()> {
        let cmd = if win == self.menu.main_win {
            let n = self.menu.tree.main.labels.len();
            match Self::menu_row_at(ly, n).and_then(|r| self.menu.tree.main.items.get(r).zip(Some(r)))
            {
                Some((Item::Launch(c), _)) => Some(c.clone()),
                // Clicking a category just (re)opens its submenu.
                Some((Item::Submenu(_), r)) => {
                    self.open_submenu(r)?;
                    return Ok(());
                }
                Some((Item::Separator, _)) | None => return Ok(()),
            }
        } else if win == self.menu.sub_win {
            match self.menu.open_cat {
                Some(cat) => match self.menu.tree.main.items.get(cat) {
                    Some(&Item::Submenu(idx)) => {
                        let Some(sub) = self.menu.tree.subs.get(idx) else {
                            return Ok(());
                        };
                        match Self::menu_row_at(ly, sub.labels.len())
                            .and_then(|r| sub.items.get(r))
                        {
                            Some(Item::Launch(c)) => Some(c.clone()),
                            _ => return Ok(()),
                        }
                    }
                    _ => return Ok(()),
                },
                None => return Ok(()),
            }
        } else {
            return Ok(());
        };
        if let Some(cmd) = cmd {
            // Route the new window into the leaf the menu was opened for.
            let leaf = self.menu.target_leaf;
            if self.state.tree.is_leaf(leaf) {
                self.state.focused_leaf = leaf;
            }
            self.spawn(&cmd);
            self.close_menu()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::{MENU_BORDER, MENU_ROW_H};

    /// Row hit-testing must respect the border padding and the row count.
    #[test]
    fn row_hit_testing_matches_geometry() {
        assert_eq!(Wm::menu_row_at(-3, 5), None);
        assert_eq!(Wm::menu_row_at(MENU_BORDER - 1, 5), None);
        assert_eq!(Wm::menu_row_at(MENU_BORDER, 5), Some(0));
        assert_eq!(Wm::menu_row_at(MENU_BORDER + MENU_ROW_H - 1, 5), Some(0));
        assert_eq!(Wm::menu_row_at(MENU_BORDER + MENU_ROW_H, 5), Some(1));
        assert_eq!(Wm::menu_row_at(MENU_BORDER + 5 * MENU_ROW_H, 5), None);
        assert_eq!(Wm::menu_row_at(MENU_BORDER, 0), None);
    }
}
