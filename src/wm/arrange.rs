//! Layout + compositing for `Wm`: compute placements, composite the underlay,
//! position client windows and the docked sidebar, and run layout animations.

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, ImageFormat, StackMode, Window,
};

use super::clients::WmState;
use super::types::{
    clamp_dim, ease_out_back, lerp_rect, BtnKind, Dock, FrameRect, LayoutAnim, LeafMeta,
    Placement, ShmSeg, ShmState, Wm, R,
};
use crate::render::{LeafView, TabInfo, TaskItem};
use crate::theme;
use crate::tree::{Dir, NodeId, Rect, Win};

impl Wm {
    pub(crate) fn arrange(&mut self) -> R<()> {
        let wa = self.la();

        // Canvas width, scroll clamping and the dock's extra scroll room are
        // `State`'s own invariants (see `State::update_canvas`); the WM only
        // supplies the inputs it alone knows. The dock is tucked
        // DOCK_OVERLAP px under the canvas edge, so that much less scroll
        // room is needed to bring it fully into view.
        let dock_extra = self.dock.docked.map_or(0, |d| d.w - d.overlap());
        self.state.update_canvas(wa, dock_extra);

        let leaves = self.state.tree.collect_leaves();
        let geos = self.state.compute(wa);
        let scroll_x = self.state.scroll_x();
        let focused = self.state.focused_leaf_valid();

        // Screen-space chrome rect for every on-screen leaf. `frame_rects`
        // keeps every leaf's rect, on-screen or not, so a leaf scrolled out
        // of view keeps a sane animation start / hit rect when it returns.
        let mut placed: Vec<Placement> = Vec::new();
        let mut frame_rects: HashMap<NodeId, FrameRect> = HashMap::new();
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
            frame_rects.insert(leaf, target);
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

        // Layout-changing actions animate: capture start rects and hand the
        // transition to the main event loop (`step_animation`), which steps
        // one frame per iteration so events keep flowing — animating inside
        // `arrange` with a blocking render loop would require draining and
        // stashing events itself, a re-entrancy hazard. Client windows are
        // still configured at their final rects right away (below), so focus
        // delivered right after this arrange targets a mapped window; only
        // the composited chrome interpolates. A non-animated arrange cancels
        // any transition in flight (it describes a newer layout).
        if std::mem::take(&mut self.animate) {
            let placed_from: Vec<(FrameRect, Placement)> = placed
                .iter()
                .map(|p| {
                    let from = self
                        .prev_frame_rect
                        .get(&p.leaf)
                        .copied()
                        .unwrap_or(FrameRect {
                            x: p.target.x,
                            y: p.target.y,
                            w: 1,
                            h: p.target.h,
                        });
                    (from, *p)
                })
                .collect();
            self.anim_seq += 1;
            self.anim = Some(LayoutAnim {
                seq: self.anim_seq,
                start: std::time::Instant::now(),
                placed: placed_from,
            });
            // First frame from the pre-transition rects so content visibly slides.
            self.anim_frame(0.0)?;
        } else {
            self.anim = None;
            self.compose(wa, &placed, true)?;
        }
        self.place_clients(&placed)?;
        self.place_dock()?;
        // place_clients/place_dock raise their windows to the top; re-apply
        // the shared stacking policy above tiled clients.
        self.apply_stacking()?;

        // Cache final rects as the start point for the next transition.
        self.prev_frame_rect = frame_rects;
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
        let mut fb = self.renderer.take_screen_base(w, h);
        {
            let m = &mut fb;
            for p in placed {
                let view = self.leaf_view(p.leaf, p.target.w, p.target.h);
                self.renderer.draw_leaf(m, p.target.x, p.target.y, &view);
                if p.focused {
                    self.renderer
                        .draw_focus_outline(m, p.target.x, p.target.y, p.target.w, p.target.h);
                }
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
                    t.in_split,
                );
                crate::render::draw_close_badge(m, t.close.x, t.close.y, t.close.w);
            }
            // Quick-launch icons after the window tiles, walled off from
            // them by the separator pill.
            if let Some(sep) = self.widgets.taskbar_sep {
                crate::render::draw_taskbar_sep(m, sep);
            }
            for &(r, i) in &self.widgets.quick_regions {
                let Some(q) = self.quick.get(i) else {
                    continue;
                };
                self.renderer.draw_taskbar_item(
                    m,
                    TaskItem {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                    },
                    q.icon.as_deref(),
                    q.label,
                    theme::palette_color::CREAM,
                    false,
                );
            }
        }
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
        self.blit_fb(self.underlay_pix, &fb)?;
        self.renderer.retire_frame(fb);
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
        let parent_dir = parent.and_then(|(p, _)| self.state.tree.branch(p).map(|b| b.dir));
        let wider = frame.w >= frame.h;
        let split_dir = if wider { Dir::H } else { Dir::V };
        LeafMeta {
            parent_dir,
            wider,
            can_split: theme::split_fits(split_dir, frame.w, frame.h),
            minimized: self.state.tree.leaf(leaf).is_some_and(|l| l.minimized),
        }
    }

    /// Position each split's window below its title bar; hide the rest.
    /// Map/unmap transitions are tracked per client (`Client::mapped`) so a
    /// self-inflicted unmap can be told apart from the client withdrawing
    /// (`ignore_unmaps`, consumed by `on_unmap`), and each transition
    /// updates the ICCCM `WM_STATE` (Normal/Iconic).
    fn place_clients(&mut self, placed: &[Placement]) -> R<()> {
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
                // Nothing clips the window to its split: one held at its
                // WM_NORMAL_HINTS minimum (see `client_rect_in_frame`)
                // overhangs the frame and paints over the neighbouring
                // split until the column is widened again.
                let min_size = self.clients.get(&c).map_or((1, 1), |cl| cl.min_size);
                let (cx, cy, cw, ch) = super::types::client_rect_in_frame(p.target, min_size);
                self.conn.configure_window(
                    c,
                    &ConfigureWindowAux::new()
                        .x(cx)
                        .y(cy)
                        .width(clamp_dim(cw))
                        .height(clamp_dim(ch))
                        .border_width(0)
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.map_window(c)?;
                visible.insert(c);
                self.note_mapped(c)?;
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
                    .width(clamp_dim(full.w.max(1)))
                    .height(clamp_dim(full.h.max(1)))
                    .border_width(0)
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(fs)?;
            visible.insert(fs);
            self.note_mapped(fs)?;
        }
        let to_hide: Vec<Win> = self
            .clients
            .iter()
            .filter(|(w, cl)| cl.mapped && !visible.contains(w))
            .map(|(&w, _)| w)
            .collect();
        for w in to_hide {
            // Record the unmap request's sequence number so `on_unmap` can
            // recognise the resulting UnmapNotify as self-inflicted.
            let cookie = self.conn.unmap_window(w)?;
            self.ignore_unmaps
                .entry(w)
                .or_default()
                .push(cookie.sequence_number() as u16);
            if let Some(cl) = self.clients.get_mut(&w) {
                cl.mapped = false;
            }
            self.set_wm_state(w, WmState::Iconic)?;
        }
        Ok(())
    }

    /// Record that `win` is mapped, setting the ICCCM `WM_STATE` to Normal
    /// on the unmapped -> mapped edge (per-transition, not per-arrange).
    fn note_mapped(&mut self, win: Win) -> R<()> {
        let newly_mapped = self
            .clients
            .get_mut(&win)
            .is_some_and(|cl| !std::mem::replace(&mut cl.mapped, true));
        if newly_mapped {
            self.set_wm_state(win, WmState::Normal)?;
        }
        Ok(())
    }

    /// Re-raise the fullscreen window (if any) above tiled clients and
    /// floats; only notifications stay above it. A fullscreen *float* also
    /// gets its full-workarea geometry re-pinned here (its frame stays
    /// unmapped; `raise_floats` may have restacked the pair, so the client
    /// is re-raised last). Callers raise notifications after this,
    /// completing the stacking policy `arrange` establishes.
    pub(crate) fn raise_fullscreen(&self) -> R<()> {
        let Some(fs) = self.fullscreen else {
            return Ok(());
        };
        if self.clients.contains_key(&fs) {
            self.conn
                .configure_window(fs, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        } else if self.floats.iter().any(|f| f.win == fs) {
            let full = self.wa();
            self.conn.configure_window(
                fs,
                &ConfigureWindowAux::new()
                    .x(full.x)
                    .y(full.y)
                    .width(clamp_dim(full.w.max(1)))
                    .height(clamp_dim(full.h.max(1)))
                    .stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    /// The dock's pinned screen geometry `(x, y, w, h)`: parked at the right
    /// end of the tiling canvas, tucked `Dock::overlap` px under it (the
    /// canvas edge overlaps the dock, not the other way round: the dock
    /// stacks just above the underlay, below every tiled client), shifted
    /// by the current scroll like any other leaf. It's (mostly) off-screen
    /// at `scroll_x = 0` and only slides fully into view once the canvas is
    /// scrolled all the way right (`State::dock_extra` extends `max_scroll`
    /// to make that reachable). Full monitor height, not `la()`'s (which is
    /// trimmed for the bottom taskbar) — the dock spans the entire screen,
    /// overlapping the taskbar strip in its column. The single formula
    /// behind `place_dock` (configuring) and `tracked_geometry` (answering
    /// denied ConfigureRequests).
    pub(crate) fn dock_geometry(&self, d: Dock) -> (i32, i32, i32, i32) {
        let wa = self.la();
        let full = self.wa();
        let canvas_w = self.state.canvas_w(wa);
        let x = wa.x + canvas_w - d.overlap() - self.state.scroll_x();
        (x, full.y, d.w.max(1), full.h.max(1))
    }

    fn place_dock(&self) -> R<()> {
        let Some(d) = self.dock.docked else {
            return Ok(());
        };
        let (x, y, w, h) = self.dock_geometry(d);
        self.conn.configure_window(
            d.win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(clamp_dim(w))
                .height(clamp_dim(h))
                .border_width(0)
                .sibling(self.underlay)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(d.win)?;
        Ok(())
    }

    // --- gap drag handles & "+" insert buttons (composited on the underlay) ---

    pub(crate) const PLUS_SZ: i32 = theme::GAP - 4;
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

    /// How long a layout transition takes, wall-clock.
    const ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(280);

    /// Composite one interpolated animation frame (chrome only, no widgets).
    /// Only the chrome animates: client windows were configured once, at
    /// their final rect, by the arrange that started the animation — moving
    /// them per frame delivered ~17 ConfigureNotifys per transition, and
    /// real apps re-layout and repaint on every one.
    fn anim_frame(&mut self, t: f32) -> R<()> {
        let Some(anim) = &self.anim else {
            return Ok(());
        };
        let e = ease_out_back(t);
        let interp: Vec<Placement> = anim
            .placed
            .iter()
            .map(|&(from, p)| Placement {
                target: lerp_rect(from, p.target, e),
                ..p
            })
            .collect();
        let wa = self.la();
        self.compose(wa, &interp, false)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Advance the in-flight layout animation by wall-clock time (called by
    /// the main event loop once per frame-paced iteration). `cut` snaps to
    /// the end immediately — set when input or structural events arrived, so
    /// nothing queues behind eye candy. The final frame recomposes with
    /// widgets, matching what a non-animated arrange would have left.
    pub(crate) fn step_animation(&mut self, cut: bool) -> R<()> {
        let Some(anim) = &self.anim else {
            return Ok(());
        };
        let t = if cut {
            1.0
        } else {
            (anim.start.elapsed().as_secs_f32() / Self::ANIM_DURATION.as_secs_f32()).min(1.0)
        };
        if t >= 1.0 {
            let anim = self.anim.take().expect("checked above");
            let wa = self.la();
            let finals: Vec<Placement> = anim.placed.iter().map(|&(_, p)| p).collect();
            self.compose(wa, &finals, true)?;
            self.conn.flush()?;
            return Ok(());
        }
        self.anim_frame(t)
    }

    // --- frame blits (MIT-SHM with a chunked-PutImage fallback) ---

    /// Blit a rendered framebuffer to a drawable. With MIT-SHM the pixels
    /// are presented straight into the shared segment and shipped as one
    /// zero-copy `ShmPutImage`. The segment holds two frame-sized halves
    /// used alternately: the put goes out unchecked (errors surface as
    /// `Event::Error` like every other unchecked request), and reuse of a
    /// half is serialised by a round trip before overwriting it while a put
    /// reading it may still be in flight. In steady state that costs one
    /// round trip every other blit, and (X being FIFO) the reply queues
    /// behind the immediately preceding put — an intentional pacing point:
    /// rendering never gets more than one full frame ahead of the server.
    /// Without MIT-SHM, present into the staging buffer and fall back to
    /// chunked core-protocol `PutImage`.
    pub(crate) fn blit_fb(&mut self, drawable: Window, fb: &pixel_graphics::Framebuffer) -> R<()> {
        let (w, h) = (fb.width as u16, fb.height as u16);
        let len = fb.width * fb.height * 4;
        self.ensure_shm(len);
        if let ShmState::Active(seg) = &mut self.shm {
            if seg.pending[seg.half] {
                // Any round trip confirms every earlier request (the X
                // stream is FIFO), including both halves' puts.
                self.conn.get_input_focus()?.reply()?;
                seg.pending = [false; 2];
            }
            self.renderer.present_into_slice(fb, seg.slice(len));
            let (seg_id, offset) = (seg.seg, seg.offset());
            use x11rb::protocol::shm::ConnectionExt as _;
            self.conn.shm_put_image(
                drawable,
                self.gc,
                w,
                h,
                0,
                0,
                w,
                h,
                0,
                0,
                self.depth,
                u8::from(ImageFormat::Z_PIXMAP),
                false,
                seg_id,
                offset as u32,
            )?;
            seg.pending[seg.half] = true;
            seg.half ^= 1;
            return Ok(());
        }
        let mut buf = std::mem::take(&mut self.bgrx);
        self.renderer.present(fb, &mut buf);
        self.bgrx = buf;
        self.put_image(drawable, w, h, &self.bgrx)
    }

    /// Make sure the SHM segment exists and each of its two halves holds at
    /// least `len` bytes, creating it on first use and recreating it when a
    /// frame outgrows it (RandR growth). Failure is remembered: without the
    /// extension every blit falls back to `put_image` with no per-frame
    /// re-probing.
    fn ensure_shm(&mut self, len: usize) {
        match &self.shm {
            ShmState::Unavailable => return,
            ShmState::Active(seg) if seg.half_len() >= len => return,
            _ => {}
        }
        if let ShmState::Active(seg) = std::mem::replace(&mut self.shm, ShmState::Unavailable) {
            use x11rb::protocol::shm::ConnectionExt as _;
            // Detach the outgrown segment server-side; the mapping itself is
            // unmapped by `ShmSeg`'s Drop.
            let _ = self.conn.shm_detach(seg.seg);
        }
        // Size to the workarea when that's bigger, so the common full-screen
        // frame never triggers a second create right after a small one.
        // Doubled: the segment holds two alternating frame halves.
        let wa = self.wa();
        let len = 2 * len.max((wa.w.max(1) as usize) * (wa.h.max(1) as usize) * 4);
        match self.create_shm(len) {
            Ok(seg) => self.shm = ShmState::Active(seg),
            Err(e) => {
                eprintln!("splitwm: MIT-SHM unavailable ({e}); using chunked PutImage");
                self.shm = ShmState::Unavailable;
            }
        }
    }

    /// Create a memfd-backed shared segment of `len` bytes, map it, and
    /// attach it to the server with `ShmAttachFd` (MIT-SHM 1.2's fd-passing
    /// attach: no SysV shm ids, no /dev/shm files to leak). The fd is owned
    /// by the attach request once sent; the local mapping stays valid.
    fn create_shm(&self, len: usize) -> R<ShmSeg> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use x11rb::connection::RequestConnection;
        use x11rb::protocol::shm::{self, ConnectionExt as _};
        if self
            .conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err("MIT-SHM extension not present".into());
        }
        // Version probe doubles as an fd-passing capability check: attach-fd
        // needs 1.2, and a server that old enough to lack it errors here.
        let v = self.conn.shm_query_version()?.reply()?;
        if (v.major_version, v.minor_version) < (1, 2) {
            return Err(format!("MIT-SHM {}.{} < 1.2", v.major_version, v.minor_version).into());
        }
        let raw = unsafe { libc::memfd_create(c"splitwm-shm".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // From here the fd is owned (closed on any early return).
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        // Owns the mapping until ShmSeg takes over: the error paths below
        // (`generate_id`, a refused attach) must munmap it, not leak it.
        struct MapGuard(*mut libc::c_void, usize);
        impl Drop for MapGuard {
            fn drop(&mut self) {
                // SAFETY: mapping was created by mmap with this exact ptr/len.
                unsafe {
                    libc::munmap(self.0, self.1);
                }
            }
        }
        let guard = MapGuard(ptr, len);
        let seg = self.conn.generate_id()?;
        // Checked: an attach refusal (e.g. an SSH-forwarded display) must
        // surface here, where the caller can fall back, not as a later
        // async error on the first blit.
        self.conn.shm_attach_fd(seg, fd, false)?.check()?;
        std::mem::forget(guard);
        // SAFETY: ptr is a fresh MAP_SHARED mapping of exactly `len` bytes,
        // owned solely by the returned ShmSeg (the fd was moved into the
        // server attach; only the mapping remains on our side).
        Ok(unsafe { ShmSeg::new(seg, ptr.cast(), len) })
    }

    /// Chunked core-protocol `PutImage` fallback for servers without MIT-SHM.
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
