//! Menu-related methods for `Wm`.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode, Window};

use super::types::{FrameRect, Wm, R};
use crate::menu::{frame_size, Item, MENU_BORDER, MENU_ROW_H};
use crate::tree::NodeId;

impl Wm {
    /// Resolve one menu column's `Icon=` names into decoded icons via the
    /// cache (failed lookups are cached as `None` and never retried).
    fn resolve_menu_icons(
        &mut self,
        names: &[Option<String>],
    ) -> Vec<Option<std::rc::Rc<crate::icon::Icon>>> {
        names
            .iter()
            .map(|n| {
                let n = n.as_ref()?;
                self.menu
                    .icon_cache
                    .entry(n.clone())
                    .or_insert_with(|| {
                        let img = crate::menu::find_icon_file(n)
                            .and_then(|p| crate::icon::load_png(&p))?;
                        Some(std::rc::Rc::new(crate::icon::quantize(
                            self.renderer.palette(),
                            &img,
                        )))
                    })
                    .clone()
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
        let Item::Submenu(idx) = self.menu.tree.main.items[cat] else {
            return Ok(());
        };
        let sub = &self.menu.tree.subs[idx];
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
        let Item::Submenu(idx) = self.menu.tree.main.items[cat] else {
            return Ok(());
        };
        let labels = self.menu.tree.subs[idx].labels.clone();
        let icon_names = self.menu.tree.subs[idx].icons.clone();
        self.menu.sub_icons = self.resolve_menu_icons(&icon_names);
        let any_icon = self.menu.sub_icons.iter().any(Option::is_some);
        let cw = self.renderer.menu_content_w(&labels, false, any_icon);
        let rows = i32::try_from(labels.len()).unwrap_or(0);
        let (w, h) = frame_size(rows, cw);
        let wa = self.wa();
        let row_y =
            self.menu.main.y + MENU_BORDER + i32::try_from(cat).unwrap_or(0) * MENU_ROW_H;
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
                .filter(|&r| !matches!(self.menu.tree.main.items[r], Item::Separator));
            if row != self.menu.main_hi {
                self.menu.main_hi = row;
                self.paint_menu_main()?;
            }
            // Hovering a category opens its submenu; hovering anything else closes it.
            match row.map(|r| &self.menu.tree.main.items[r]) {
                Some(Item::Submenu(_)) => {
                    if self.menu.open_cat != row {
                        self.open_submenu(row.unwrap())?;
                    }
                }
                _ => {
                    if self.menu.open_cat.is_some() {
                        self.menu.open_cat = None;
                        self.conn.unmap_window(self.menu.sub_win)?;
                    }
                }
            }
        } else if win == self.menu.sub_win {
            if let Some(cat) = self.menu.open_cat {
                if let Item::Submenu(idx) = self.menu.tree.main.items[cat] {
                    let n = self.menu.tree.subs[idx].labels.len();
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

    pub(crate) fn on_menu_click(&mut self, win: Window, ly: i32) -> R<()> {
        let cmd = if win == self.menu.main_win {
            let n = self.menu.tree.main.labels.len();
            match Self::menu_row_at(ly, n) {
                Some(r) => match &self.menu.tree.main.items[r] {
                    Item::Launch(c) => Some(c.clone()),
                    // Clicking a category just (re)opens its submenu.
                    Item::Submenu(_) => {
                        self.open_submenu(r)?;
                        return Ok(());
                    }
                    Item::Separator => return Ok(()),
                },
                None => return Ok(()),
            }
        } else if win == self.menu.sub_win {
            match self.menu.open_cat {
                Some(cat) => match self.menu.tree.main.items[cat] {
                    Item::Submenu(idx) => {
                        let n = self.menu.tree.subs[idx].labels.len();
                        match Self::menu_row_at(ly, n) {
                            Some(r) => match &self.menu.tree.subs[idx].items[r] {
                                Item::Launch(c) => Some(c.clone()),
                                _ => return Ok(()),
                            },
                            None => return Ok(()),
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
