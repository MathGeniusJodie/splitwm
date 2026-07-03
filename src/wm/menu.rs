//! Menu-related methods for `Wm`. The main column and the category submenu
//! share one `MenuColumn` shape and one set of show/paint/hit/scroll
//! helpers; only their placement and row *data* (from `MenuTree`) differ.

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode, Window};

use super::types::{FrameRect, IconSlot, Wm, R};
use crate::icon::Icon;
use crate::menu::{frame_size, Item, Menu, MENU_BORDER, MENU_ROW_H};
use crate::render::MenuView;
use crate::tree::NodeId;

impl Wm {
    /// The `Menu` (row data) shown in a column: the main tree, or the open
    /// category's submenu. `None` when the submenu is asked for while no
    /// category is open (or `open_cat`/the tree drifted — they are only
    /// coupled by convention, so a stale index degrades to a no-op).
    fn column_menu(&self, sub: bool) -> Option<&Menu> {
        if !sub {
            return Some(&self.menu.tree.main);
        }
        let cat = self.menu.open_cat?;
        let &Item::Submenu(idx) = self.menu.tree.main.items.get(cat)? else {
            return None;
        };
        self.menu.tree.subs.get(idx)
    }

    /// Which column `win` is: `Some(is_sub)`, or `None` for other windows.
    fn column_for(&self, win: Window) -> Option<bool> {
        if win == self.menu.main.win {
            Some(false)
        } else if win == self.menu.sub.win {
            Some(true)
        } else {
            None
        }
    }

    /// Whether `win` is one of the (open) launcher menu's windows.
    pub(crate) fn is_menu_window(&self, win: Window) -> bool {
        self.column_for(win).is_some()
    }

    /// How many rows fit on screen; columns taller than this are clamped
    /// and wheel-scrolled (`on_menu_scroll`).
    fn menu_max_rows(&self) -> usize {
        (((self.wa().h - 2 * MENU_BORDER) / MENU_ROW_H).max(1)) as usize
    }

    /// Rows currently visible in a column (its total clamped to the screen).
    fn visible_rows(&self, rows: usize) -> usize {
        rows.min(self.menu_max_rows())
    }

    /// (Re)initialise a column's view state for the given menu data and
    /// place its window at `(x, y)` sized to the (height-clamped) frame.
    /// `place` maps the frame's size to its top-left corner, so the two
    /// columns can anchor differently. Dimensions are clamped to the CARD16
    /// wire range — a pathological label can make `content_w` arbitrary.
    fn show_column(
        &mut self,
        sub: bool,
        cw: i32,
        place: impl FnOnce(&Self, i32, i32) -> (i32, i32),
    ) -> R<()> {
        let menu = self
            .column_menu(sub)
            .expect("caller established the column's data");
        let rows = menu.labels.len();
        let icons: Vec<IconSlot> = menu.icons.iter().cloned().map(IconSlot::Pending).collect();
        let vis = self.visible_rows(rows);
        let (w, h) = frame_size(i32::try_from(vis).unwrap_or(0), cw);
        let (w, h) = (
            w.clamp(1, i32::from(u16::MAX)),
            h.clamp(1, i32::from(u16::MAX)),
        );
        let (x, y) = place(self, w, h);
        let col = if sub {
            &mut self.menu.sub
        } else {
            &mut self.menu.main
        };
        col.rect = FrameRect { x, y, w, h };
        col.cw = cw;
        col.hi = None;
        col.scroll = 0;
        col.rows = rows;
        col.icons = icons;
        let win = col.win;
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(u32::try_from(w).unwrap_or(1))
                .height(u32::try_from(h).unwrap_or(1))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(win)?;
        self.raise_menu()?;
        self.paint_column(sub)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Open the launcher menu for `leaf`, with its bottom-right corner anchored
    /// at screen (ax, ay) so it rises above the bottom taskbar.
    pub(crate) fn open_menu(&mut self, leaf: NodeId, ax: i32, ay: i32) -> R<()> {
        let m = &self.menu.tree.main;
        let any_icon = m.icons.iter().any(Option::is_some);
        let cw = self.renderer.menu_content_w(&m.labels, true, any_icon);
        self.menu.open_cat = None;
        self.menu.target_leaf = leaf;
        self.menu.open = true;
        self.conn.unmap_window(self.menu.sub.win)?;
        self.show_column(false, cw, |wm, w, h| {
            let wa = wm.wa();
            (
                (ax - w).clamp(wa.x, (wa.x + wa.w - w).max(wa.x)),
                (ay - h).clamp(wa.y, (wa.y + wa.h - h).max(wa.y)),
            )
        })
    }

    /// Open the submenu for main row `cat` to the right of that row.
    pub(crate) fn open_submenu(&mut self, cat: usize) -> R<()> {
        self.menu.open_cat = Some(cat);
        let Some(sub) = self.column_menu(true) else {
            self.menu.open_cat = None;
            return Ok(());
        };
        let any_icon = sub.icons.iter().any(Option::is_some);
        let cw = self.renderer.menu_content_w(&sub.labels, false, any_icon);
        let main = self.menu.main.rect;
        // The hovered row's *on-screen* position: `cat` is an absolute row
        // index, the window shows rows from `scroll` down.
        let row_vis = i32::try_from(cat.saturating_sub(self.menu.main.scroll)).unwrap_or(0);
        self.show_column(true, cw, move |wm, w, h| {
            let wa = wm.wa();
            let row_y = main.y + MENU_BORDER + row_vis * MENU_ROW_H;
            let y = (row_y - MENU_BORDER).min(wa.y + wa.h - h).max(wa.y);
            // Prefer the right side; flip left if it would overflow.
            let right_x = main.x + main.w - MENU_BORDER;
            let x = if right_x + w <= wa.x + wa.w {
                right_x
            } else {
                main.x - w + MENU_BORDER
            };
            (x, y)
        })
    }

    /// Raise the open launcher windows above all clients. No-op when closed.
    /// Clients are raised to the top on every `arrange`/focus, so the menu must
    /// be re-raised afterwards to stay visible.
    pub(crate) fn raise_menu(&self) -> R<()> {
        if !self.menu.open {
            return Ok(());
        }
        let above = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        self.conn.configure_window(self.menu.main.win, &above)?;
        if self.menu.open_cat.is_some() {
            self.conn.configure_window(self.menu.sub.win, &above)?;
        }
        Ok(())
    }

    pub(crate) fn close_menu(&mut self) -> R<()> {
        if !self.menu.open {
            return Ok(());
        }
        self.menu.open = false;
        self.menu.open_cat = None;
        self.conn.unmap_window(self.menu.main.win)?;
        self.conn.unmap_window(self.menu.sub.win)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Resolve one row's `Icon=` name into a decoded icon via the cache.
    /// Only successes are cached: a failed lookup retries on the next open
    /// (cheap — the filesystem scan underneath is cached with its own
    /// expiry in `menu::find_icon_file`), so an icon that appears on disk
    /// mid-session starts showing without a WM restart.
    fn resolve_icon_slot(&mut self, name: Option<&str>) -> Option<Rc<Icon>> {
        let n = name?;
        // Same wholesale-clear policy as the renderer's icon caches: menus
        // are bounded in practice, but no cache here grows without a cap.
        // Clearing drops the `Rc<Icon>`s, so re-decoded entries get *new*
        // `Icon::id`s and the renderer's per-id index/ring caches treat
        // them as fresh icons — the old entries become dead weight there
        // until its own cap-clear retires them.
        if self.menu.icon_cache.len() >= 512 {
            self.menu.icon_cache.clear();
        }
        if let Some(hit) = self.menu.icon_cache.get(n) {
            return Some(hit.clone());
        }
        let img = crate::menu::find_icon_file(n).and_then(|p| crate::icon::load_png(&p))?;
        let icon = Rc::new(crate::icon::quantize(self.renderer.palette(), &img));
        self.menu.icon_cache.insert(n.to_string(), icon.clone());
        Some(icon)
    }

    /// Repaint a column: resolve the visible rows' icons (lazily, so a big
    /// column never stats+decodes off-screen rows), then render and blit
    /// the visible slice.
    fn paint_column(&mut self, sub: bool) -> R<()> {
        if self.column_menu(sub).is_none() {
            return Ok(());
        }
        let col = if sub { &self.menu.sub } else { &self.menu.main };
        let vis = self.visible_rows(col.rows);
        let start = col.scroll.min(col.rows.saturating_sub(vis));
        let range = start..(start + vis).min(col.rows);

        // Decode icons for rows entering view (first hover/scroll only).
        let pending: Vec<(usize, Option<String>)> = self.column_icons(sub)[range.clone()]
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                IconSlot::Pending(name) => Some((start + i, name.clone())),
                IconSlot::Ready(_) => None,
            })
            .collect();
        for (i, name) in pending {
            let icon = self.resolve_icon_slot(name.as_deref());
            self.column_icons_mut(sub)[i] = IconSlot::Ready(icon);
        }

        let menu = self.column_menu(sub).expect("checked above");
        let col = if sub { &self.menu.sub } else { &self.menu.main };
        let seps: Vec<bool> = menu.items[range.clone()]
            .iter()
            .map(|it| matches!(it, Item::Separator))
            .collect();
        let icons: Vec<Option<Rc<Icon>>> = col.icons[range.clone()]
            .iter()
            .map(|s| match s {
                IconSlot::Ready(icon) => icon.clone(),
                IconSlot::Pending(_) => None,
            })
            .collect();
        let fb = self.renderer.draw_menu(&MenuView {
            labels: &menu.labels[range.clone()],
            arrows: &menu.arrows[range.clone()],
            seps: &seps,
            icons: &icons,
            content_w: col.cw,
            hi: col
                .hi
                .and_then(|h| h.checked_sub(start))
                .filter(|&r| r < range.len()),
            icon_col: menu.icons.iter().any(Option::is_some),
        });
        let win = col.win;
        self.blit_fb(win, &fb)
    }

    fn column_icons(&self, sub: bool) -> &[IconSlot] {
        if sub {
            &self.menu.sub.icons
        } else {
            &self.menu.main.icons
        }
    }

    fn column_icons_mut(&mut self, sub: bool) -> &mut [IconSlot] {
        if sub {
            &mut self.menu.sub.icons
        } else {
            &mut self.menu.main.icons
        }
    }

    pub(crate) fn paint_menu_main(&mut self) -> R<()> {
        self.paint_column(false)
    }

    pub(crate) fn paint_menu_sub(&mut self) -> R<()> {
        self.paint_column(true)
    }

    /// Absolute row index under window-local (lx, ly), or None for the
    /// border padding — including the left/right border strips, so a click
    /// beside a row's text can't activate it. `scroll` is the column's
    /// first visible row, `n` its total row count, `cw` its content width.
    pub(crate) fn menu_row_at(lx: i32, ly: i32, scroll: usize, n: usize, cw: i32) -> Option<usize> {
        if lx < MENU_BORDER || lx >= MENU_BORDER + cw {
            return None;
        }
        let inner = ly - MENU_BORDER;
        if inner < 0 {
            return None;
        }
        let row = scroll + (inner / MENU_ROW_H) as usize;
        (row < n).then_some(row)
    }

    /// The absolute row under (lx, ly) in the given column.
    fn column_row_at(&self, sub: bool, lx: i32, ly: i32) -> Option<usize> {
        let col = if sub { &self.menu.sub } else { &self.menu.main };
        let row = Self::menu_row_at(lx, ly, col.scroll, col.rows, col.cw)?;
        // Below the last visible row lies the bottom border.
        (row < col.scroll + self.visible_rows(col.rows)).then_some(row)
    }

    /// Wheel scroll over a column taller than the screen: shift the visible
    /// window of rows and repaint. No-op for columns that fit.
    pub(crate) fn on_menu_scroll(&mut self, win: Window, down: bool) -> R<()> {
        let Some(sub) = self.column_for(win) else {
            return Ok(());
        };
        let vis = self.menu_max_rows();
        let col = if sub {
            &mut self.menu.sub
        } else {
            &mut self.menu.main
        };
        if col.rows <= vis {
            return Ok(());
        }
        const WHEEL_ROWS: usize = 3;
        let max = col.rows - vis;
        let new = if down {
            (col.scroll + WHEEL_ROWS).min(max)
        } else {
            col.scroll.saturating_sub(WHEEL_ROWS)
        };
        if new != col.scroll {
            col.scroll = new;
            self.paint_column(sub)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub(crate) fn on_menu_motion(&mut self, win: Window, lx: i32, ly: i32) -> R<()> {
        let Some(sub) = self.column_for(win) else {
            return Ok(());
        };
        let row = self.column_row_at(sub, lx, ly);
        if !sub {
            let row =
                row.filter(|&r| !matches!(self.menu.tree.main.items.get(r), Some(Item::Separator)));
            if row != self.menu.main.hi {
                self.menu.main.hi = row;
                self.paint_column(false)?;
            }
            // Hovering a category opens its submenu; hovering anything else
            // closes it.
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
                        self.conn.unmap_window(self.menu.sub.win)?;
                    }
                }
            }
        } else if self.menu.open_cat.is_some() && row != self.menu.sub.hi {
            self.menu.sub.hi = row;
            self.paint_column(true)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Pointer left a menu window: drop its hover highlight. The main
    /// column falls back to highlighting the open category (so a pointer
    /// travelling through the submenu keeps the row it came from lit)
    /// rather than clearing outright.
    pub(crate) fn on_menu_leave(&mut self, win: Window) -> R<()> {
        if win == self.menu.main.win {
            if self.menu.main.hi != self.menu.open_cat {
                self.menu.main.hi = self.menu.open_cat;
                self.paint_column(false)?;
                self.conn.flush()?;
            }
        } else if win == self.menu.sub.win && self.menu.sub.hi.is_some() {
            self.menu.sub.hi = None;
            self.paint_column(true)?;
            self.conn.flush()?;
        }
        Ok(())
    }

    pub(crate) fn on_menu_click(&mut self, win: Window, lx: i32, ly: i32) -> R<()> {
        let Some(sub) = self.column_for(win) else {
            return Ok(());
        };
        let Some(row) = self.column_row_at(sub, lx, ly) else {
            return Ok(());
        };
        let Some(menu) = self.column_menu(sub) else {
            return Ok(());
        };
        let cmd = match menu.items.get(row) {
            Some(Item::Launch(c)) => c.clone(),
            // Clicking a main-column category just (re)opens its submenu.
            Some(Item::Submenu(_)) if !sub => return self.open_submenu(row),
            _ => return Ok(()),
        };
        // Route the new window into the leaf the menu was opened for.
        let leaf = self.menu.target_leaf;
        if self.state.tree.is_leaf(leaf) {
            self.state.focused_leaf = leaf;
        }
        self.spawn(&cmd);
        self.close_menu()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::{MENU_BORDER, MENU_ROW_H};

    const CW: i32 = 100;
    const X: i32 = MENU_BORDER + 1; // inside the content, past the border

    /// Row hit-testing must respect the border padding (all four sides),
    /// the scroll offset, and the row count.
    #[test]
    fn row_hit_testing_matches_geometry() {
        assert_eq!(Wm::menu_row_at(X, -3, 0, 5, CW), None);
        assert_eq!(Wm::menu_row_at(X, MENU_BORDER - 1, 0, 5, CW), None);
        assert_eq!(Wm::menu_row_at(X, MENU_BORDER, 0, 5, CW), Some(0));
        assert_eq!(
            Wm::menu_row_at(X, MENU_BORDER + MENU_ROW_H - 1, 0, 5, CW),
            Some(0)
        );
        assert_eq!(
            Wm::menu_row_at(X, MENU_BORDER + MENU_ROW_H, 0, 5, CW),
            Some(1)
        );
        assert_eq!(
            Wm::menu_row_at(X, MENU_BORDER + 5 * MENU_ROW_H, 0, 5, CW),
            None
        );
        assert_eq!(Wm::menu_row_at(X, MENU_BORDER, 0, 0, CW), None);
    }

    /// Clicks in the left/right border strips must not land on a row.
    #[test]
    fn side_borders_do_not_hit_rows() {
        let y = MENU_BORDER + 1;
        assert_eq!(Wm::menu_row_at(0, y, 0, 5, CW), None);
        assert_eq!(Wm::menu_row_at(MENU_BORDER - 1, y, 0, 5, CW), None);
        assert_eq!(Wm::menu_row_at(MENU_BORDER, y, 0, 5, CW), Some(0));
        assert_eq!(Wm::menu_row_at(MENU_BORDER + CW - 1, y, 0, 5, CW), Some(0));
        assert_eq!(Wm::menu_row_at(MENU_BORDER + CW, y, 0, 5, CW), None);
    }

    /// A scrolled column reports absolute row indices.
    #[test]
    fn scroll_offsets_hit_rows() {
        assert_eq!(Wm::menu_row_at(X, MENU_BORDER, 7, 20, CW), Some(7));
        assert_eq!(
            Wm::menu_row_at(X, MENU_BORDER + MENU_ROW_H, 7, 20, CW),
            Some(8)
        );
        // Scrolled past the end: no row.
        assert_eq!(Wm::menu_row_at(X, MENU_BORDER, 20, 20, CW), None);
    }
}
