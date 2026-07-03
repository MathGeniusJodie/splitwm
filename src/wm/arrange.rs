//! Layout + compositing for `Wm`: compute placements, composite the underlay,
//! position client windows and the docked sidebar, and run layout animations.

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, ImageFormat, StackMode, Window,
};

use super::clients::{WM_STATE_ICONIC, WM_STATE_NORMAL};
use super::types::{ease_out_back, lerp_rect, BtnKind, FrameRect, LeafMeta, Placement, Wm, R};
use crate::render::{LeafView, TabInfo, TaskItem};
use crate::theme;
use crate::tree::{Dir, Node, NodeId, Rect, Win};

impl Wm {
    pub(crate) fn arrange(&mut self) -> R<()> {
        let wa = self.la();
        let gap = theme::GAP;

        // Grow the canvas if the tree is wider than the viewport. Width
        // demand is measured in *columns* (`Tree::h_units`), not leaves — a
        // vertical stack of any depth still occupies one column, so it must
        // not open up phantom scroll space. Each column gets a comfortable
        // minimum so splits don't get crushed. Any manual edge-of-canvas
        // resize (`State::resize_edge`) layers on top via `canvas_w_extra`
        // so it isn't clobbered by this recompute — not reclamped to `wa.w`
        // afterward, since a deliberate edge-shrink can legitimately take
        // the canvas narrower than the viewport (leaving margin on the far
        // side); `resize_edge`'s own per-column `min_split_w` clamp is what
        // actually keeps this sane.
        let columns = self.state.tree.h_units().max(1);
        let min_col_w = (theme::min_split_w() + 2 * gap).max(wa.w / 3);
        let needed = columns.saturating_mul(min_col_w);
        let canvas_w = needed.max(wa.w) + self.state.canvas_w_extra;
        self.state.canvas_w = Some(canvas_w);
        // The dock is tucked DOCK_OVERLAP px under the canvas edge, so that
        // much less scroll room is needed to bring it fully into view.
        self.state.dock_extra = if self.dock.win.is_some() {
            self.dock.w - self.dock_overlap()
        } else {
            0
        };

        let leaves = self.state.tree.collect_leaves();
        let geos = self.state.compute(wa);
        let scroll_x = self.state.scroll_x;
        let focused = self.state.focused_leaf_valid();

        // Screen-space chrome rect for every on-screen leaf.
        let mut placed: Vec<Placement> = Vec::new();
        for &leaf in &leaves {
            let Some(geo) = geos.get(&leaf).copied() else {
                continue;
            };
            let target = FrameRect {
                x: geo.x - scroll_x,
                y: geo.y,
                w: geo.w.max(1),
                h: geo.h.max(1),
            };
            if target.x + target.w <= wa.x || target.x >= wa.x + wa.w {
                continue;
            }
            let active_client = self.state.tree.leaf(leaf).and_then(|l| l.client);
            placed.push(Placement {
                leaf,
                target,
                active_client,
                focused: focused == leaf,
            });
        }

        // Parent lookups for this layout, from one arena walk — per-event
        // consumers (`hover_cursor`, `click_split_button`) read this rather
        // than paying `find_parent`'s full scan per call.
        self.parents = self.state.tree.parent_map();

        // Drag-handle / "+" / tab hit-regions for the current layout.
        self.compute_widgets(wa, &placed);
        self.compute_taskbar();

        if std::mem::take(&mut self.animate) {
            self.run_layout_animation(wa, &placed)?;
        }
        self.compose(wa, &placed, true)?;
        self.place_clients(&placed)?;
        self.place_dock(wa, canvas_w)?;
        // place_clients/place_dock raise their windows to the top; keep
        // floats above tiled clients, notifications above those, and an open
        // launcher menu above everything (an arrange can be triggered while
        // it's open).
        self.raise_floats()?;
        // Fullscreen covers floats too; only notifications and the menu stay
        // above it. A fullscreen *float* also gets its full-workarea
        // geometry re-pinned here (its frame stays unmapped; `raise_floats`
        // above may have restacked the pair, so re-raise the client last).
        if let Some(fs) = self.fullscreen {
            if self.clients.contains_key(&fs) {
                self.conn.configure_window(
                    fs,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )?;
            } else if self.floats.iter().any(|f| f.win == fs) {
                let full = self.wa();
                self.conn.configure_window(
                    fs,
                    &ConfigureWindowAux::new()
                        .x(full.x)
                        .y(full.y)
                        .width(u32::try_from(full.w.max(1)).unwrap_or(1))
                        .height(u32::try_from(full.h.max(1)).unwrap_or(1))
                        .stack_mode(StackMode::ABOVE),
                )?;
            }
        }
        self.raise_notifications()?;
        self.raise_menu()?;

        // Cache final rects as the start point for the next transition.
        self.prev_frame_rect = placed.iter().map(|p| (p.leaf, p.target)).collect();
        self.conn.flush()?;
        Ok(())
    }

    /// Composite the wallpaper, every placed leaf's chrome, and (optionally)
    /// the drag handles / "+" buttons onto the single underlay window.
    fn compose(&mut self, _layout: Rect, placed: &[Placement], widgets: bool) -> R<()> {
        use crate::render::BtnIcon as I;
        // The underlay (and base pixmap) always cover the full screen, even
        // though the split layout only uses the area above the taskbar.
        let wa = self.wa();
        let (w, h) = (wa.w.max(1) as u32, wa.h.max(1) as u32);
        let mut fb = self.renderer.screen_base(w, h);
        {
            let m = &mut fb;
            for p in placed {
                let view = self.leaf_view(p.leaf, p.target.w, p.target.h);
                self.renderer.draw_leaf(m, p.target.x, p.target.y, &view);
            }
            if widgets {
                for (r, _) in &self.widgets.plus_regions {
                    crate::render::draw_plus(m, r.x + r.w / 2, r.y + r.h / 2, r.w);
                }
                // Split-control buttons. Look each leaf's final frame up so the
                // icon/enabled state matches the post-arrange geometry.
                // `btn_regions` has up to 3 entries per leaf (close/split/
                // minimize); parent lookups come from one `parent_map` walk
                // rather than a full-arena `find_parent` scan per leaf.
                let metas = self.leaf_metas(placed);
                for (r, leaf, kind) in &self.widgets.btn_regions {
                    let Some(&meta) = metas.get(leaf) else {
                        continue;
                    };
                    // A minimized leaf's region is the whole frame (a single
                    // restore button); `draw_leaf`'s winmin.png already shows
                    // it, so no button glyph is drawn on top.
                    if meta.minimized {
                        continue;
                    }
                    let (icon, disabled) = match kind {
                        // A V-branch parent means this leaf collapses to a
                        // row (short/wide) when minimized, so its button
                        // previews that with the horizontal glyph.
                        BtnKind::Minimize => (
                            if meta.parent_dir == Some(Dir::V) {
                                I::MinimizeH
                            } else {
                                I::Minimize
                            },
                            meta.parent_dir.is_none(),
                        ),
                        BtnKind::Split => (
                            if meta.wider { I::VSplit } else { I::HSplit },
                            !meta.can_split,
                        ),
                        BtnKind::Close => (I::Close, meta.parent_dir.is_none()),
                    };
                    self.renderer.draw_button(
                        m,
                        r.x + r.w / 2,
                        r.y + r.h / 2,
                        icon,
                        disabled,
                        self.leaf_color_index(*leaf),
                    );
                }
            }
            // Bottom bar: one tile per managed window; split-visible windows
            // get an accent highlight box, and every tile carries a corner
            // close badge.
            for t in &self.widgets.taskbar_regions {
                let icon = self.icon_for(t.win);
                self.renderer.draw_taskbar_item(
                    m,
                    TaskItem {
                        x: t.rect.x,
                        y: t.rect.y,
                        w: t.rect.w,
                        h: t.rect.h,
                    },
                    icon.as_deref(),
                    self.clients.get(&t.win).map_or('?', |c| c.label),
                    t.accent,
                    t.on_screen,
                );
                crate::render::draw_close_badge(m, t.close.x, t.close.y, t.close.w);
            }
            // Launcher "+" at the right end of the bar.
            let pr = self.widgets.taskbar_plus;
            crate::render::draw_plus(m, pr.x + pr.w / 2, pr.y + pr.h / 2, pr.w);
        }
        let mut buf = std::mem::take(&mut self.bgrx);
        self.renderer.present(&fb, &mut buf);
        self.bgrx = buf;
        // Blit into a pixmap installed as the underlay's background, not the
        // window itself: the server then repaints regions exposed by moving
        // (shaped) clients synchronously from the pixmap, instead of flashing
        // the black background pixel until our Expose handler catches up.
        let (pw, ph) = (w as u16, h as u16);
        if self.underlay_pix_size != (pw, ph) {
            if self.underlay_pix != 0 {
                self.conn.free_pixmap(self.underlay_pix)?;
            }
            let pix = self.conn.generate_id()?;
            self.conn
                .create_pixmap(self.depth, pix, self.underlay, pw, ph)?;
            self.underlay_pix = pix;
            self.underlay_pix_size = (pw, ph);
            self.conn.change_window_attributes(
                self.underlay,
                &ChangeWindowAttributesAux::new().background_pixmap(pix),
            )?;
        }
        self.put_image(self.underlay_pix, pw, ph, &self.bgrx)?;
        self.conn.clear_area(false, self.underlay, 0, 0, 0, 0)?;
        Ok(())
    }

    /// Each split's persistent accent palette index, stored on the leaf so it
    /// survives splits and closes; palette-swaps the bitmap window border and
    /// colours the bottom-bar highlight.
    pub(crate) fn leaf_color_index(&self, leaf: NodeId) -> crate::Index {
        self.state
            .tree
            .leaf(leaf)
            .map_or(theme::FALLBACK_ACCENT_INDEX, |l| l.color)
    }

    fn leaf_view(&self, leaf: NodeId, w: i32, h: i32) -> LeafView {
        let win = self.state.tree.leaf(leaf).and_then(|l| l.client);
        let client = win.and_then(|w| self.clients.get(&w));
        let accent_index = self.leaf_color_index(leaf);
        let tab = client.map(|c| TabInfo {
            label: c.label,
            icon: win.and_then(|w| self.icon_for(w)),
        });
        LeafView {
            w,
            h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index,
            tab,
            minimized: self.state.tree.leaf(leaf).is_some_and(|l| l.minimized),
        }
    }

    /// Parent direction / split-eligibility metadata used to choose each
    /// split-control button's icon and enabled state. Parent lookups come
    /// from `self.parents` (rebuilt each arrange), so per-motion callers
    /// like `hover_cursor` don't pay a full arena scan.
    pub(crate) fn leaf_meta(&self, leaf: NodeId, frame: FrameRect) -> LeafMeta {
        self.leaf_meta_inner(leaf, frame, self.parents.get(&leaf).copied())
    }

    /// `leaf_meta` for every placement at once.
    fn leaf_metas(&self, placed: &[Placement]) -> HashMap<NodeId, LeafMeta> {
        placed
            .iter()
            .map(|p| {
                (
                    p.leaf,
                    self.leaf_meta_inner(p.leaf, p.target, self.parents.get(&p.leaf).copied()),
                )
            })
            .collect()
    }

    fn leaf_meta_inner(
        &self,
        leaf: NodeId,
        frame: FrameRect,
        parent: Option<(NodeId, usize)>,
    ) -> LeafMeta {
        let parent_dir = parent.and_then(|(p, _)| match self.state.tree.get(p) {
            Some(Node::Branch { dir, .. }) => Some(*dir),
            _ => None,
        });
        let gap = theme::GAP;
        let wider = frame.w >= frame.h;
        let can_v = frame.w >= 2 * theme::min_split_w() + gap;
        let can_h = frame.h >= 2 * theme::tb_h() + gap;
        LeafMeta {
            parent_dir,
            wider,
            can_split: if wider { can_v } else { can_h },
            minimized: self.state.tree.leaf(leaf).is_some_and(|l| l.minimized),
        }
    }

    /// Position each split's window below its title bar; hide the rest.
    /// Map/unmap transitions are tracked per client (`Client::mapped`) so a
    /// self-inflicted unmap can be told apart from the client withdrawing
    /// (`ignore_unmaps`, consumed by `on_unmap`), and each transition
    /// updates the ICCCM `WM_STATE` (Normal/Iconic).
    fn place_clients(&mut self, placed: &[Placement]) -> R<()> {
        let tb_h = theme::tb_h();
        let bw = theme::BORDER_LEFT;
        let mut visible: std::collections::HashSet<Win> = std::collections::HashSet::new();
        let fullscreen = self.fullscreen.filter(|w| self.clients.contains_key(w));
        for p in placed {
            let minimized = self.state.tree.leaf(p.leaf).is_some_and(|l| l.minimized);
            if let Some(c) = p.active_client {
                if minimized || Some(c) == fullscreen {
                    // The fullscreen client is configured below, over the
                    // whole workarea; don't fight it with split geometry.
                    continue;
                }
                let r = p.target;
                // Never configure a client below its WM_NORMAL_HINTS
                // minimum, so apps aren't handed geometry they can't
                // honour. Nothing clips the window to its split: one held
                // at its minimum overhangs the frame and paints over the
                // neighbouring split until the column is widened again.
                let (min_w, min_h) = self.clients.get(&c).map_or((1, 1), |cl| cl.min_size);
                let cw = (r.w - 2 * bw).max(min_w).max(1);
                let ch = (r.h - tb_h - bw).max(min_h).max(1);
                self.conn.configure_window(
                    c,
                    &ConfigureWindowAux::new()
                        .x(r.x + bw)
                        .y(r.y + tb_h)
                        .width(u32::try_from(cw).unwrap_or(1))
                        .height(u32::try_from(ch).unwrap_or(1))
                        .border_width(0)
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.map_window(c)?;
                visible.insert(c);
                let newly_mapped = self
                    .clients
                    .get_mut(&c)
                    .is_some_and(|cl| !std::mem::replace(&mut cl.mapped, true));
                if newly_mapped {
                    self.set_wm_state(c, WM_STATE_NORMAL)?;
                }
            }
        }
        // The fullscreen client covers the whole workarea above every tiled
        // client, regardless of where (or whether) its split is on screen —
        // marked visible before `to_hide` is computed so it can't be
        // mapped-then-hidden in the same pass.
        if let Some(fs) = fullscreen {
            let full = self.wa();
            self.conn.configure_window(
                fs,
                &ConfigureWindowAux::new()
                    .x(full.x)
                    .y(full.y)
                    .width(u32::try_from(full.w.max(1)).unwrap_or(1))
                    .height(u32::try_from(full.h.max(1)).unwrap_or(1))
                    .border_width(0)
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(fs)?;
            visible.insert(fs);
            let newly_mapped = self
                .clients
                .get_mut(&fs)
                .is_some_and(|cl| !std::mem::replace(&mut cl.mapped, true));
            if newly_mapped {
                self.set_wm_state(fs, WM_STATE_NORMAL)?;
            }
        }
        let to_hide: Vec<Win> = self
            .clients
            .iter()
            .filter(|(w, cl)| cl.mapped && !visible.contains(w))
            .map(|(&w, _)| w)
            .collect();
        for w in to_hide {
            *self.ignore_unmaps.entry(w).or_insert(0) += 1;
            self.conn.unmap_window(w)?;
            if let Some(cl) = self.clients.get_mut(&w) {
                cl.mapped = false;
            }
            self.set_wm_state(w, WM_STATE_ICONIC)?;
        }
        Ok(())
    }

    /// Position the docked sidebar at the right end of the tiling canvas,
    /// tucked `theme::DOCK_OVERLAP` px under it (the canvas edge overlaps
    /// the dock, not the other way round: the dock stacks just above the
    /// underlay, below every tiled client) in canvas space, then shift by
    /// the current scroll like any other leaf. It's (mostly) off-screen at
    /// `scroll_x = 0` and only slides fully into view once the canvas is
    /// scrolled all the way right (`State::dock_extra` extends `max_scroll`
    /// to make that reachable); a no-op if nothing is docked.
    /// `theme::DOCK_OVERLAP` clamped to the dock's own width — an overlap
    /// wider than the dock would otherwise shove its right edge permanently
    /// away from the screen edge (fully tucked is the useful maximum).
    fn dock_overlap(&self) -> i32 {
        theme::DOCK_OVERLAP.min(self.dock.w)
    }

    fn place_dock(&self, wa: Rect, canvas_w: i32) -> R<()> {
        let Some(win) = self.dock.win else {
            return Ok(());
        };
        // Full monitor height, not `la()`'s (which is trimmed for the
        // bottom taskbar) — the dock spans the entire screen, overlapping
        // the taskbar strip in its column.
        let full = self.wa();
        let x = wa.x + canvas_w - self.dock_overlap() - self.state.scroll_x;
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(full.y)
                .width(u32::try_from(self.dock.w).unwrap_or(1))
                .height(u32::try_from(full.h.max(1)).unwrap_or(1))
                .border_width(0)
                .sibling(self.underlay)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(win)?;
        Ok(())
    }

    // --- gap drag handles & "+" insert buttons (composited on the underlay) ---

    pub(crate) const PLUS_SZ: i32 = 22;
    /// Total px trimmed off the gap to get the drag-handle pill width.
    pub(crate) const HANDLE_INSET: i32 = 10;

    /// A `PLUS_SZ`-square hit/draw rect centred horizontally on `vis_x`.
    pub(crate) const fn plus_rect(vis_x: i32, y: i32) -> FrameRect {
        FrameRect {
            x: vis_x - Self::PLUS_SZ / 2,
            y,
            w: Self::PLUS_SZ,
            h: Self::PLUS_SZ,
        }
    }

    /// Animate the placed leaves from their previous rect (or a collapsed
    /// sliver, for freshly-created leaves) to their target with an
    /// ease-out-back curve, re-compositing the underlay each frame.
    ///
    /// Driven by wall-clock time, not a fixed frame count: each frame does a
    /// full-screen software recomposite + blit (not cheap), so we step by how
    /// much real time has elapsed and always finish in `DURATION`, ending
    /// exactly on the target. Frames are paced to ~60 Hz — an unpaced loop
    /// would hammer the server with full-screen `PutImage`s as fast as the
    /// socket accepts them (and pin a core) for no visible benefit.
    fn run_layout_animation(&mut self, wa: Rect, placed: &[Placement]) -> R<()> {
        use std::time::{Duration, Instant};
        const DURATION: Duration = Duration::from_millis(280);
        const FRAME: Duration = Duration::from_millis(16);
        let starts: Vec<FrameRect> = placed
            .iter()
            .map(|p| {
                self.prev_frame_rect
                    .get(&p.leaf)
                    .copied()
                    .unwrap_or(FrameRect {
                        x: p.target.x,
                        y: p.target.y,
                        w: 1,
                        h: p.target.h,
                    })
            })
            .collect();
        let start = Instant::now();
        let mut cut_short = false;
        loop {
            let frame_start = Instant::now();
            // Drain whatever arrived while the last frame rendered: a
            // keypress or click cuts the animation short (snap to the final
            // frame) instead of queueing 280 ms of input behind it. Every
            // drained event is stashed for the main loop to process next.
            while let Some(ev) = self.conn.poll_for_event()? {
                // Any input or structural event cuts the animation short:
                // input so 280 ms of keypresses don't queue behind eye candy,
                // structural (map/unmap/destroy/configure/client-message) so
                // a window appearing or dying mid-animation is handled
                // promptly instead of waiting out the transition.
                use x11rb::protocol::Event as E;
                cut_short |= matches!(
                    ev,
                    E::KeyPress(_)
                        | E::ButtonPress(_)
                        | E::MapRequest(_)
                        | E::UnmapNotify(_)
                        | E::DestroyNotify(_)
                        | E::ConfigureRequest(_)
                        | E::ClientMessage(_)
                );
                self.pending_events.push(ev);
            }
            let t = if cut_short {
                1.0
            } else {
                (start.elapsed().as_secs_f32() / DURATION.as_secs_f32()).min(1.0)
            };
            let e = ease_out_back(t);
            let interp: Vec<Placement> = placed
                .iter()
                .zip(&starts)
                .map(|(p, s)| Placement {
                    leaf: p.leaf,
                    target: lerp_rect(*s, p.target, e),
                    active_client: p.active_client,
                    focused: p.focused,
                })
                .collect();
            // Only the chrome animates. Client windows are configured once,
            // at the final rect (by the arrange that called us): moving them
            // per frame delivered ~17 ConfigureNotifys per transition, and
            // real apps re-layout and repaint on every one.
            self.compose(wa, &interp, false)?;
            self.conn.flush()?;
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(FRAME.saturating_sub(frame_start.elapsed()));
        }
        Ok(())
    }

    pub(crate) fn put_image(&self, drawable: Window, w: u16, h: u16, data: &[u8]) -> R<()> {
        let gc = self.gc;
        let stride = w as usize * 4;
        // Chunk by rows to stay under the maximum request length.
        let overhead = 64;
        let max_rows = (((self.max_req_bytes.saturating_sub(overhead)) / stride).max(1)) as u16;
        let mut y = 0u16;
        while y < h {
            let rows = max_rows.min(h - y);
            let start = y as usize * stride;
            let end = start + rows as usize * stride;
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                gc,
                w,
                rows,
                0,
                i16::try_from(y).unwrap_or(i16::MAX),
                0,
                self.depth,
                &data[start..end],
            )?;
            y += rows;
        }
        Ok(())
    }
}
