//! Client window lifecycle for `Wm`: adopting/managing/unmanaging windows,
//! the docked sidebar, app icons, focus, spawning, and the small ICCCM/EWMH
//! surface (WM_STATE, WM_DELETE_WINDOW, _NET_CLIENT_LIST, _NET_ACTIVE_WINDOW).

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonIndex, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt, CreateWindowAux, EventMask, GrabMode, InputFocus, MapState, ModMask, PropMode,
    StackMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::types::{clamp_dim, Client, FloatWin, FocusModel, Wm, R};
use crate::icon::{self, Icon};
use crate::theme;
use crate::tree::Win;

/// ICCCM WM_STATE values.
pub(crate) const WM_STATE_WITHDRAWN: u32 = 0;
pub(crate) const WM_STATE_NORMAL: u32 = 1;
pub(crate) const WM_STATE_ICONIC: u32 = 3;

impl Wm {
    /// Adopt windows already mapped on the root when splitwm starts, so
    /// taking over from a previous window manager doesn't lose whatever was
    /// on screen. A well-behaved WM adds every client it reparents to its
    /// SaveSet, so once it exits (releasing `SUBSTRUCTURE_REDIRECT`, which
    /// is what let us become the WM in the first place) the X server
    /// auto-reparents surviving client windows back onto root, still
    /// mapped — exactly what a normal `MapRequest` handles, just batched
    /// here once at startup instead of arriving one at a time.
    pub(crate) fn manage_existing_windows(&mut self) -> R<()> {
        let children = self.conn.query_tree(self.root)?.reply()?.children;
        for win in children {
            if win == self.underlay || win == self.menu.main_win || win == self.menu.sub_win {
                continue;
            }
            let Ok(attrs) = self.conn.get_window_attributes(win)?.reply() else {
                continue;
            };
            if attrs.override_redirect
                || attrs.map_state != MapState::VIEWABLE
                || attrs.class != WindowClass::INPUT_OUTPUT
            {
                continue;
            }
            // A window destroyed between query_tree and managing it (easy to
            // race during a --replace handover) must not abort the whole
            // startup — log and adopt the rest, matching the main loop's
            // per-event error containment.
            if let Err(e) = self.manage(win, true) {
                eprintln!("splitwm: failed to adopt existing window {win:#x}: {e}");
            }
        }
        // `manage` defers per-window layout work during adoption
        // (`already_mapped`): one arrange/focus/client-list pass here covers
        // the whole batch instead of one full recomposite per window.
        self.update_client_list()?;
        self.arrange()?;
        let f = self.state.focused_client();
        self.focus(f)?;
        let adopted: Vec<Win> = self.clients.keys().copied().collect();
        for win in adopted {
            self.sync_wm_state(win)?;
        }
        Ok(())
    }

    /// Record the ICCCM `WM_STATE` matching whether we currently have the
    /// window mapped (Normal) or hidden (Iconic).
    fn sync_wm_state(&self, win: Win) -> R<()> {
        let mapped = self.clients.get(&win).is_some_and(|c| c.mapped);
        self.set_wm_state(
            win,
            if mapped {
                WM_STATE_NORMAL
            } else {
                WM_STATE_ICONIC
            },
        )
    }

    /// Shared adoption prologue: select the events we need from `win`, strip
    /// its core border (chrome is ours), and optionally install the
    /// click-to-focus passive button-1 grab.
    fn select_and_grab(&self, win: Win, mask: EventMask, grab: bool) -> R<()> {
        self.conn
            .change_window_attributes(win, &ChangeWindowAttributesAux::new().event_mask(mask))?;
        if grab {
            self.conn.grab_button(
                true,
                win,
                EventMask::BUTTON_PRESS,
                GrabMode::SYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                ButtonIndex::M1,
                ModMask::ANY,
            )?;
        }
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;
        Ok(())
    }

    /// Start managing `win`. `already_mapped` distinguishes adopted
    /// already-viewable windows (whose next unmap by us must be recorded in
    /// `ignore_unmaps`, and whose arrange/focus work is batched by
    /// `manage_existing_windows`) from fresh `MapRequest`s that are not yet
    /// mapped.
    pub(crate) fn manage(&mut self, win: Win, already_mapped: bool) -> R<()> {
        if self.is_notification(win) {
            return self.manage_notification(win);
        }
        if self.wants_float(win) {
            return self.manage_float(win);
        }
        if self.matches_dock(win) {
            if self.dock.win.is_none() {
                return self.manage_dock(win);
            }
            eprintln!(
                "splitwm: second '{}' dock window {win:#x}; tiling it normally",
                self.dock.title
            );
        }

        // Class -> label; app icon from _NET_WM_ICON, falling back to the
        // icon theme (some apps, e.g. Electron ones, set the property late
        // or not at all — see `on_icon_change` for the late case).
        let class = self.client_identity(win);
        let label = class.chars().next().map_or('?', |c| c.to_ascii_uppercase());
        let icon = self.fetch_icon(win).or_else(|| self.theme_icon(&class));
        let icon_slot = self.assign_icon_slot(&class);

        // Place the client into the focused split (displacing any occupant).
        self.state.pin_client(win);

        self.select_and_grab(
            win,
            EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
            true,
        )?;

        self.clients.insert(
            win,
            Client {
                label,
                icon,
                icon_rotated: None,
                class: class.clone(),
                icon_slot,
                mapped: already_mapped,
                min_size: self
                    .size_hints(win)
                    .and_then(|h| h.min_size)
                    .map_or((1, 1), |(w, h)| (w.max(1), h.max(1))),
                focus: self.focus_model(win),
            },
        );
        self.refresh_icon_rotations(&class);
        if !self.bar_order.contains(&win) {
            self.bar_order.push(win);
        }
        self.state.activate_client(win);
        // During startup adoption (`already_mapped`) the arrange/focus/
        // client-list/WM_STATE work is batched by `manage_existing_windows`
        // after the loop — once for all adopted windows.
        if !already_mapped {
            self.update_client_list()?;
            // The full layout epilogue, not a bare arrange: the focused leaf
            // may be scrolled out of view, and only `commit_layout`'s
            // ensure_in_view/land_scroll brings it (and the new window) back
            // into the viewport where place_clients maps it. A new window
            // takes the keyboard, so any focused dialog yields it first.
            self.focused_float = None;
            self.commit_layout()?;
            // The arrange has mapped it (or left it hidden); record the
            // ICCCM state.
            self.sync_wm_state(win)?;
        }
        // EWMH allows requesting fullscreen by setting the property before
        // mapping; the ClientMessage path only covers later requests.
        if self.wants_fullscreen(win) {
            self.set_fullscreen(win, true)?;
        }
        Ok(())
    }

    /// Pin `win` (`Wm::dock_title`) as a borderless window parked past
    /// the right end of the scrolling canvas, revealed by scrolling all the
    /// way right (see `place_dock`/`State::dock_extra`): it never enters
    /// `clients`/the split tree/taskbar, so it gets none of their chrome or
    /// focus cycling, and normal tiled columns never lay out under it. Its
    /// size is whatever it asked for at creation time, kept fixed for the
    /// rest of the session.
    fn manage_dock(&mut self, win: Win) -> R<()> {
        let width = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(240, |g| i32::from(g.width));
        self.dock.win = Some(win);
        self.dock.w = width.max(1);

        self.select_and_grab(win, EventMask::STRUCTURE_NOTIFY, true)?;
        // The dock is a mapped managed client too: give it the ICCCM
        // WM_STATE some toolkits misbehave without (see `set_wm_state`).
        self.set_wm_state(win, WM_STATE_NORMAL)?;

        // arrange() calls place_dock() against the freshly computed canvas,
        // so no separate initial placement is needed here.
        self.arrange()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Every atom in the ATOM-list property `prop` on `win`; empty on any
    /// failure. The shared read behind protocol/window-type/state checks.
    fn prop_atoms(&self, win: Win, prop: u32) -> Vec<u32> {
        self.conn
            .get_property(false, win, prop, AtomEnum::ATOM, 0, 32)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| Some(r.value32()?.collect()))
            .unwrap_or_default()
    }

    /// Whether `win` lists `atom` in `WM_PROTOCOLS`.
    fn supports_protocol(&self, win: Win, atom: u32) -> bool {
        self.prop_atoms(win, self.atoms.wm_protocols)
            .contains(&atom)
    }

    /// Whether `win`'s `_NET_WM_STATE` *property* (set before mapping, per
    /// EWMH) already asks for fullscreen — the ClientMessage path only
    /// covers requests made after the window is managed.
    fn wants_fullscreen(&self, win: Win) -> bool {
        self.prop_atoms(win, self.atoms.net_wm_state)
            .contains(&self.atoms.net_wm_state_fullscreen)
    }

    /// Whether `win` should float instead of tiling: a transient
    /// (`WM_TRANSIENT_FOR`), a declared dialog
    /// (`_NET_WM_WINDOW_TYPE_DIALOG`), or a fixed-size window
    /// (`WM_NORMAL_HINTS` min == max — it can't be resized, so stretching
    /// it into a split only produces gravel).
    fn wants_float(&self, win: Win) -> bool {
        if self.transient_for(win).is_some()
            || self.is_window_type(win, self.atoms.net_wm_window_type_dialog)
        {
            return true;
        }
        self.size_hints(win).is_some_and(|h| {
            matches!((h.min_size, h.max_size),
                (Some((minw, minh)), Some((maxw, maxh)))
                    if minw == maxw && minh == maxh && minw > 0 && minh > 0)
        })
    }

    /// `WM_TRANSIENT_FOR`'s target window, if set (and not the root, which
    /// some toolkits use to mean "transient for the whole session").
    fn transient_for(&self, win: Win) -> Option<Win> {
        let r = self
            .conn
            .get_property(
                false,
                win,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let parent = r.value32()?.next()?;
        (parent != x11rb::NONE && parent != self.root).then_some(parent)
    }

    fn size_hints(&self, win: Win) -> Option<x11rb::properties::WmSizeHints> {
        x11rb::properties::WmSizeHints::get_normal_hints(&self.conn, win)
            .ok()?
            .reply()
            .ok()?
    }

    /// ICCCM focus model from `WM_HINTS.input` (defaults to true when unset,
    /// per ICCCM) and `WM_TAKE_FOCUS` membership in `WM_PROTOCOLS`.
    fn focus_model(&self, win: Win) -> FocusModel {
        let input = x11rb::properties::WmHints::get(&self.conn, win)
            .ok()
            .and_then(|c| c.reply().ok().flatten())
            .and_then(|h| h.input)
            .unwrap_or(true);
        let take_focus = self.supports_protocol(win, self.atoms.wm_take_focus);
        FocusModel { input, take_focus }
    }

    /// Frame insets around a float's client window: the same border art the
    /// splits use — `tb_h` above (the titlebar strip), `BORDER_LEFT` on the
    /// other three sides, matching `place_clients`'s insets.
    pub(crate) const fn float_insets() -> (i32, i32) {
        (theme::BORDER_LEFT, theme::tb_h())
    }

    /// Float `win` (see `FloatWin`): show it at its requested size,
    /// centered over its transient parent's split frame when that parent is
    /// a tiled client currently on screen, otherwise centered in the
    /// workarea. A chrome frame window (split border + titlebar, no control
    /// buttons) is stacked just below it; dragging the frame moves the pair.
    /// It takes focus immediately (a dialog exists to be answered).
    fn manage_float(&mut self, win: Win) -> R<()> {
        let parent = self.transient_for(win);
        let geo = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok());
        let (mut w, mut h) = geo.map_or((400, 300), |g| {
            (i32::from(g.width).max(1), i32::from(g.height).max(1))
        });
        // An adopted window a previous WM stretched into a split can be
        // bigger than its own maximum; snap it back to the size it wants.
        if let Some((maxw, maxh)) = self.size_hints(win).and_then(|hints| hints.max_size) {
            (w, h) = (w.min(maxw.max(1)), h.min(maxh.max(1)));
        }
        // Keep the frame's outer size within the u16 wire type: an absurd
        // requested size would make the `u16::try_from(..).unwrap_or(1)`
        // below collapse the frame to 1px around a full-size client.
        let (bw, tb) = Self::float_insets();
        w = w.clamp(1, i32::from(u16::MAX) - 2 * bw);
        h = h.clamp(1, i32::from(u16::MAX) - tb - bw);
        // Center over the parent's frame when we know it, else the workarea.
        let around = parent
            .and_then(|p| self.state.tree.find_leaf_for_client(p))
            .and_then(|l| self.prev_frame_rect.get(&l).copied())
            .unwrap_or_else(|| self.la());
        let wa = self.la();
        let x = (around.x + (around.w - w) / 2).clamp(wa.x, (wa.x + wa.w - w).max(wa.x));
        let y = (around.y + (around.h - h) / 2).clamp(wa.y, (wa.y + wa.h - h).max(wa.y));

        // The dialog inherits its transient parent's split accent so the
        // chrome visibly ties them together.
        let accent = parent
            .and_then(|p| self.state.tree.find_leaf_for_client(p))
            .map_or(theme::FALLBACK_ACCENT_INDEX, |l| self.leaf_color_index(l));
        let class = self.client_identity(win);
        let label = class.chars().next().map_or('?', |c| c.to_ascii_uppercase());
        let icon = self.fetch_icon(win).or_else(|| self.theme_icon(&class));

        self.select_and_grab(win, EventMask::STRUCTURE_NOTIFY, true)?;
        // The chrome frame: our own override-redirect window, painted with
        // the split border art and shaped so its rounded corners are
        // click-through. Button events on it start a move drag.
        let frame = self.conn.generate_id()?;
        self.conn.create_window(
            self.depth,
            frame,
            self.root,
            i16::try_from(x - bw).unwrap_or(0),
            i16::try_from(y - tb).unwrap_or(0),
            u16::try_from(w + 2 * bw).unwrap_or(1),
            u16::try_from(h + tb + bw).unwrap_or(1),
            0,
            WindowClass::INPUT_OUTPUT,
            0, // CopyFromParent
            &CreateWindowAux::new()
                .override_redirect(1)
                .cursor(self.cursors.hand)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::BUTTON1_MOTION,
                ),
        )?;
        self.configure_float_frame(win, frame, x, y, w, h)?;
        let focus = self.focus_model(win);
        self.floats.push(FloatWin {
            win,
            frame,
            parent,
            focus,
            x,
            y,
            w,
            h,
            accent,
            icon,
            label,
        });
        self.conn.map_window(frame)?;
        self.conn.map_window(win)?;
        self.update_client_list()?;
        self.restack_float(win)?;
        self.paint_float_frame(frame)?;
        self.set_wm_state(win, WM_STATE_NORMAL)?;
        self.focus_float(win)?;
        self.raise_notifications()?;
        // Same pre-map `_NET_WM_STATE` fullscreen honouring as tiled clients.
        if self.wants_fullscreen(win) {
            self.set_fullscreen(win, true)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Configure a float pair to its tracked geometry: the client window at
    /// `(x, y, w, h)` and the chrome frame around it, extended by
    /// `float_insets`. The single geometry formula behind float manage,
    /// self-resize (ConfigureRequest) and fullscreen restore.
    pub(crate) fn configure_float_frame(
        &self,
        win: Win,
        frame: Win,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> R<()> {
        let (bw, tb) = Self::float_insets();
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(clamp_dim(w))
                .height(clamp_dim(h))
                .border_width(0),
        )?;
        self.conn.configure_window(
            frame,
            &ConfigureWindowAux::new()
                .x(x - bw)
                .y(y - tb)
                .width(clamp_dim(w + 2 * bw))
                .height(clamp_dim(h + tb + bw)),
        )?;
        Ok(())
    }

    /// Raise a float as a unit: frame to the top, client just above it.
    fn restack_float(&self, win: Win) -> R<()> {
        let Some(f) = self.floats.iter().find(|f| f.win == win) else {
            return Ok(());
        };
        self.conn.configure_window(
            f.frame,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .sibling(f.frame)
                .stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    /// Render a float's chrome into its frame window: the split border +
    /// titlebar icon via `draw_leaf` (control buttons are drawn separately
    /// for splits, so none appear here), shaped to the opaque pixels.
    pub(crate) fn paint_float_frame(&mut self, frame: Win) -> R<()> {
        let Some(f) = self.floats.iter().find(|f| f.frame == frame) else {
            return Ok(());
        };
        let (bw, tb) = Self::float_insets();
        let (fw, fh) = (f.w + 2 * bw, f.h + tb + bw);
        let view = crate::render::LeafView {
            w: fw,
            h: fh,
            tb_h: tb,
            bw,
            accent_index: f.accent,
            tab: Some(crate::render::TabInfo {
                label: f.label,
                icon: f.icon.clone(),
            }),
            minimized: false,
        };
        let mut fb = pixel_graphics::Framebuffer::new(
            fw.max(1) as usize,
            fh.max(1) as usize,
            pixel_graphics::TRANSPARENT,
        );
        self.renderer.draw_leaf(&mut fb, 0, 0, &view);
        self.shape_to_opaque(frame, &fb)?;
        self.blit_fb(frame, &fb)
    }

    /// Move a float (client + frame) so its client origin lands at (x, y),
    /// keeping at least a grabbable strip on screen.
    pub(crate) fn move_float(&mut self, win: Win, x: i32, y: i32) -> R<()> {
        let (bw, tb) = Self::float_insets();
        let wa = self.wa();
        let Some(f) = self.floats.iter_mut().find(|f| f.win == win) else {
            return Ok(());
        };
        // Clamp so the titlebar can't leave the screen (the frame is the
        // only handle there is to drag it back with). `on_screen_strip` is
        // how much of the float must stay reachable on either axis; it
        // happens to equal the titlebar height, but is used here as a
        // general leftover-strip size, not as a titlebar measurement.
        let on_screen_strip = theme::tb_h();
        let x = x.clamp(wa.x - f.w + on_screen_strip, wa.x + wa.w - on_screen_strip);
        let y = y.clamp(wa.y + tb, wa.y + wa.h - on_screen_strip);
        (f.x, f.y) = (x, y);
        let frame = f.frame;
        self.conn
            .configure_window(frame, &ConfigureWindowAux::new().x(x - bw).y(y - tb))?;
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().x(x).y(y))?;
        Ok(())
    }

    /// Give input focus to a float and remember it as the keyboard target.
    pub(crate) fn focus_float(&mut self, win: Win) -> R<()> {
        let Some(f) = self.floats.iter().find(|f| f.win == win) else {
            return Ok(());
        };
        self.give_focus(win, f.focus)?;
        self.focused_float = Some(win);
        self.restack_float(win)?;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_active_window,
            AtomEnum::WINDOW,
            &[win],
        )?;
        Ok(())
    }

    /// A float went away: drop it and hand focus back to its transient
    /// parent (if tiled and visible) or the focused split.
    pub(crate) fn forget_float(&mut self, win: Win) -> R<()> {
        let Some(idx) = self.floats.iter().position(|f| f.win == win) else {
            return Ok(());
        };
        let gone = self.floats.remove(idx);
        if self.fullscreen == Some(win) {
            self.fullscreen = None;
        }
        self.conn.destroy_window(gone.frame)?;
        self.update_client_list()?;
        if self.drags.float.is_some_and(|d| d.win == win) {
            self.drags.float = None;
        }
        let parent = gone.parent;
        if self.focused_float == Some(win) {
            self.focused_float = None;
            let back = parent
                .filter(|p| self.state.tree.find_leaf_for_client(*p).is_some())
                .or_else(|| self.state.focused_client());
            if let Some(b) = back {
                self.state.activate_client(b);
            }
            self.focus(back)?;
        }
        Ok(())
    }

    /// Restack every float (frame + client pair) above the tiled clients
    /// (arrange raises tiled windows; floats must stay above them, below
    /// notifications).
    pub(crate) fn raise_floats(&self) -> R<()> {
        for f in &self.floats {
            self.restack_float(f.win)?;
        }
        Ok(())
    }

    /// Whether `win` declares `_NET_WM_WINDOW_TYPE_NOTIFICATION` (the type
    /// property is a preference-ordered list; any entry counts).
    fn is_notification(&self, win: Win) -> bool {
        self.is_window_type(win, self.atoms.net_wm_window_type_notification)
    }

    fn is_window_type(&self, win: Win, wanted: u32) -> bool {
        self.prop_atoms(win, self.atoms.net_wm_window_type)
            .contains(&wanted)
    }

    /// Pin `win` as a notification: never in the split tree or taskbar,
    /// stacked above everything at the bottom-right of the screen (above the
    /// taskbar strip), at whatever size it requested. Newer notifications
    /// stack upward above older ones.
    fn manage_notification(&mut self, win: Win) -> R<()> {
        self.select_and_grab(win, EventMask::STRUCTURE_NOTIFY, false)?;
        if !self.notes.foreign.iter().any(|n| n.win == win) {
            // One geometry query at manage time; size updates thereafter
            // come from the window's own ConfigureRequests.
            let (w, h) = self
                .conn
                .get_geometry(win)
                .ok()
                .and_then(|c| c.reply().ok())
                .map_or((1, 1), |g| (i32::from(g.width), i32::from(g.height)));
            self.notes
                .foreign
                .push(super::types::ForeignNote { win, w, h });
        }
        self.place_notifications()?;
        self.conn.map_window(win)?;
        // Notifications are mapped managed windows too: record the ICCCM
        // WM_STATE (see `set_wm_state`).
        self.set_wm_state(win, WM_STATE_NORMAL)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Position every notification: stacked bottom-up starting just above
    /// the taskbar (oldest at the bottom; see `stack_note_pile` for the
    /// shared geometry). Our own served-notification popups continue the
    /// same pile upward (see `place_note_popups`), so both kinds share one
    /// stack instead of overlapping — which is also why a foreign-pile
    /// change must re-place the popups.
    pub(crate) fn place_notifications(&self) -> R<()> {
        let wa = self.wa();
        let bottom = wa.y + wa.h - Self::taskbar_h();
        self.stack_note_pile(self.notes.foreign.iter().map(|n| (n.win, n.w, n.h)), bottom)?;
        self.place_note_popups()
    }

    /// Restack every notification to the top, preserving their relative
    /// order — arrange()/focus() raise tiled clients, so notifications must
    /// be re-raised afterwards to stay on top of everything.
    pub(crate) fn raise_notifications(&self) -> R<()> {
        for n in &self.notes.foreign {
            self.conn.configure_window(
                n.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        for p in &self.notes.popups {
            self.conn.configure_window(
                p.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    /// Stop tracking a closed notification and re-stack the survivors.
    pub(crate) fn forget_notification(&mut self, win: Win) -> R<()> {
        self.notes.foreign.retain(|n| n.win != win);
        self.place_notifications()?;
        Ok(())
    }

    /// Stop managing `win` (destroyed or withdrawn): drop all bookkeeping,
    /// re-tile, and keep focus inside the leaf the window lived in.
    pub(crate) fn forget_client(&mut self, win: Win) -> R<()> {
        let known = self.clients.remove(&win).is_some();
        // A window can occupy a leaf/taskbar slot without an entry in
        // `clients` if `manage` errored out partway (it pins into the tree
        // before its X requests); clean the layout up regardless, or the
        // split shows a phantom occupant forever.
        let in_layout = self.state.tree.find_leaf_for_client(win).is_some()
            || self.state.taskbar.contains(&win);
        if !known && !in_layout {
            return Ok(());
        }
        if self.fullscreen == Some(win) {
            self.fullscreen = None;
        }
        self.bar_order.retain(|&w| w != win);
        self.ignore_unmaps.remove(&win);
        self.state.unpin_client(win);
        self.update_client_list()?;
        self.arrange()?;
        let next = self.state.focused_client();
        self.focus(next)?;
        Ok(())
    }

    /// Ask `win` to close via `WM_DELETE_WINDOW` when it participates in
    /// the protocol (giving it a chance to prompt/save); fall back to
    /// disconnecting its client only when it doesn't.
    pub(crate) fn close_client(&self, win: Win) -> R<()> {
        if self.supports_protocol(win, self.atoms.wm_delete_window) {
            // A real timestamp, not CURRENT_TIME: some clients use it to
            // arbitrate focus races on their "save changes?" prompt. A close
            // is always user-initiated, so the last input event's time is
            // fresh (and 0 degrades to CURRENT_TIME before any input).
            let msg = ClientMessageEvent::new(
                32,
                win,
                self.atoms.wm_protocols,
                [self.atoms.wm_delete_window, self.last_event_time, 0, 0, 0],
            );
            self.conn.send_event(false, win, EventMask::NO_EVENT, msg)?;
        } else {
            self.conn.kill_client(win)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Remap every managed client on the way out (quit or WM handover):
    /// layout hiding uses plain unmaps, and a departing WM that leaves
    /// windows unmapped strands them — the next WM only adopts viewable
    /// windows.
    pub(crate) fn restore_clients(&mut self) -> R<()> {
        let wins: Vec<Win> = self.clients.keys().copied().collect();
        for win in wins {
            self.conn.map_window(win)?;
            self.set_wm_state(win, WM_STATE_NORMAL)?;
        }
        if let Some(dock) = self.dock.win {
            self.conn.map_window(dock)?;
        }
        for f in &self.floats {
            self.conn.map_window(f.win)?;
        }
        // A plain flush is not enough right before process exit: the server
        // can notice the connection hang up before draining what we just
        // wrote and drop it. A round trip proves everything was processed.
        self.conn.sync()?;
        Ok(())
    }

    // --- ICCCM / EWMH bookkeeping ---

    /// Set the ICCCM `WM_STATE` property (Normal/Iconic/Withdrawn) — some
    /// toolkits (notably Java's) misbehave without it.
    pub(crate) fn set_wm_state(&self, win: Win, state: u32) -> R<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.wm_state,
            self.atoms.wm_state,
            &[state, 0],
        )?;
        Ok(())
    }

    /// A managed client (re)set its `WM_CLASS` or title: if it now matches
    /// the dock identity (and nothing is docked yet), pull it out of tiling
    /// and dock it — a toolkit that sets its identifying property only
    /// after mapping would otherwise leave the dock tiled as an ordinary
    /// window forever.
    pub(crate) fn on_dock_identity_change(&mut self, win: Win, changed_atom: u32) -> R<()> {
        if self.dock.win.is_some() {
            return Ok(());
        }
        let Some(client) = self.clients.get(&win) else {
            return Ok(());
        };
        // Title changes are frequent (terminals retitle per prompt) and can
        // only affect dock identity for windows with no WM_CLASS at all
        // (the title is matches_dock's last-resort fallback). Don't pay the
        // property round trips for a title change on a classed window.
        let class_changed = changed_atom == u32::from(AtomEnum::WM_CLASS);
        if !class_changed && client.class.as_ref() != "?" {
            return Ok(());
        }
        if !self.matches_dock(win) {
            return Ok(());
        }
        self.clients.remove(&win);
        self.bar_order.retain(|&w| w != win);
        self.ignore_unmaps.remove(&win);
        if self.fullscreen == Some(win) {
            self.fullscreen = None;
        }
        self.state.unpin_client(win);
        self.update_client_list()?;
        // Drop the click-to-focus grab `manage` installed: `manage_dock`
        // re-issues the identical passive grab, and grabbing a combination
        // that is already grabbed raises BadAccess.
        self.conn
            .ungrab_button(ButtonIndex::M1, win, ModMask::ANY)?;
        self.manage_dock(win)
    }

    /// Refresh `_NET_CLIENT_LIST` on the root: managed tiled windows in
    /// `bar_order` (mapping order) plus the floats and the dock — they're
    /// managed client windows too, and panels/pagers should see them.
    pub(crate) fn update_client_list(&self) -> R<()> {
        let mut list = self.bar_order.clone();
        list.extend(self.floats.iter().map(|f| f.win));
        list.extend(self.dock.win);
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_client_list,
            AtomEnum::WINDOW,
            &list,
        )?;
        Ok(())
    }

    // --- EWMH fullscreen ---

    /// Apply an `_NET_WM_STATE_FULLSCREEN` change for a managed window —
    /// tiled client or float: track it, mirror the state onto the window's
    /// `_NET_WM_STATE` property, and re-arrange (which positions/stacks the
    /// fullscreen window). Only one window is fullscreen at a time; a
    /// second request replaces the first (its property is cleared so it
    /// doesn't believe it's still fullscreen).
    pub(crate) fn set_fullscreen(&mut self, win: Win, on: bool) -> R<()> {
        let is_client = self.clients.contains_key(&win);
        let is_float = self.floats.iter().any(|f| f.win == win);
        if !is_client && !is_float {
            return Ok(());
        }
        if on {
            if let Some(prev) = self.fullscreen.replace(win) {
                if prev != win {
                    self.set_net_wm_state_fullscreen(prev, false)?;
                    // If the replaced window was a fullscreen float, put its
                    // frame and geometry back (no-op for tiled clients).
                    self.restore_float_geometry(prev)?;
                }
            }
            self.set_net_wm_state_fullscreen(win, true)?;
        } else {
            if self.fullscreen != Some(win) {
                return Ok(());
            }
            self.fullscreen = None;
            self.set_net_wm_state_fullscreen(win, false)?;
            if is_float {
                self.restore_float_geometry(win)?;
            }
        }
        if is_client {
            self.state.activate_client(win);
            self.arrange()?;
            self.focus(Some(win))?;
        } else {
            // Float: `arrange`'s fullscreen block applies/keeps the
            // full-workarea geometry while it's active.
            self.arrange()?;
            if on {
                // Hide the chrome frame; the client alone covers the screen.
                if let Some(f) = self.floats.iter().find(|f| f.win == win) {
                    self.conn.unmap_window(f.frame)?;
                }
            }
            self.focus_float(win)?;
        }
        Ok(())
    }

    /// Re-show a float's chrome frame and restore its remembered geometry
    /// after leaving fullscreen. No-op for windows that aren't floats.
    fn restore_float_geometry(&mut self, win: Win) -> R<()> {
        let Some(f) = self.floats.iter().find(|f| f.win == win) else {
            return Ok(());
        };
        let (frame, x, y, w, h) = (f.frame, f.x, f.y, f.w, f.h);
        self.conn.map_window(frame)?;
        self.configure_float_frame(win, frame, x, y, w, h)?;
        self.paint_float_frame(frame)?;
        self.restack_float(win)?;
        Ok(())
    }

    /// Mirror our fullscreen bookkeeping onto the client's `_NET_WM_STATE`
    /// property (the whole list is just this one state — we support no
    /// others).
    fn set_net_wm_state_fullscreen(&self, win: Win, on: bool) -> R<()> {
        let states: &[u32] = if on {
            &[self.atoms.net_wm_state_fullscreen]
        } else {
            &[]
        };
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            self.atoms.net_wm_state,
            AtomEnum::ATOM,
            states,
        )?;
        Ok(())
    }

    // --- identity & icons ---

    /// WM_CLASS's class string (second of the "instance\0class\0" pair),
    /// used both as the taskbar label's source letter and to group windows
    /// of the same app for icon color-rotation (`assign_icon_slot`).
    fn client_identity(&self, win: Win) -> Rc<str> {
        let class = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        let parts: Vec<&[u8]> = class.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
        let name = parts
            .get(1)
            .or_else(|| parts.first())
            .copied()
            .unwrap_or(b"?");
        Rc::from(String::from_utf8_lossy(name).as_ref())
    }

    /// Whether `win` is the dock: either half of its `WM_CLASS`
    /// ("instance\0class\0") equals `DockState::title`, falling back to the
    /// window title only when it sets no `WM_CLASS` at all (the stock dock
    /// app doesn't). Class is preferred because a title is client-controlled
    /// free text that changes at runtime — matching on title alone would let
    /// any window titling itself "cozyui" (a browser tab, say) get yanked
    /// out of tiling and pinned as the dock; a window that *does* declare a
    /// class must match on that alone.
    fn matches_dock(&self, win: Win) -> bool {
        let class = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        let mut parts = class
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .peekable();
        if parts.peek().is_some() {
            return parts.any(|part| part == self.dock.title.as_bytes());
        }
        self.client_title(win).as_ref() == self.dock.title
    }

    /// The window's title — `_NET_WM_NAME` (UTF-8) with a latin-1 `WM_NAME`
    /// fallback. Only consulted as the dock identity of last resort for
    /// windows that never set `WM_CLASS` (see `matches_dock`).
    fn client_title(&self, win: Win) -> Rc<str> {
        let read = |atom: u32, ty: u32| -> Option<Vec<u8>> {
            let v = self
                .conn
                .get_property(false, win, atom, ty, 0, 256)
                .ok()?
                .reply()
                .ok()?
                .value;
            (!v.is_empty()).then_some(v)
        };
        let name = read(self.atoms.net_wm_name, self.atoms.utf8_string)
            .or_else(|| read(u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)))
            .unwrap_or_default();
        Rc::from(String::from_utf8_lossy(&name).as_ref())
    }

    /// First hue-rotation slot (see `theme::icon_hue_rotation`) not already
    /// held by another open window of `class`, so windows of one app stay
    /// distinguishable while a free slot remains. Freeing is implicit: once
    /// a window is unmanaged it drops out of `self.clients`, so its slot no
    /// longer counts as used.
    // Only `theme::ICON_HUE_STEPS` distinct hues exist for disambiguating
    // same-class windows; once that many are already in use, this silently
    // returns `None` (no rotated icon) for the next one instead of erroring
    // or reusing a hue, so windows past the step count share an icon look.
    fn assign_icon_slot(&self, class: &str) -> Option<usize> {
        let used: std::collections::HashSet<usize> = self
            .clients
            .values()
            .filter(|c| c.class.as_ref() == class)
            .filter_map(|c| c.icon_slot)
            .collect();
        (0..theme::ICON_HUE_STEPS).find(|s| !used.contains(s))
    }

    /// Render (once) the hue-rotated icon variant for every window of
    /// `class`, as soon as the class has two windows open. Slot 0 is a 0°
    /// rotation and keeps using the base icon; already-rendered variants
    /// are kept (the slot is persistent for the window's lifetime).
    fn refresh_icon_rotations(&mut self, class: &Rc<str>) {
        let wins: Vec<Win> = self
            .clients
            .iter()
            .filter(|(_, c)| c.class == *class)
            .map(|(&w, _)| w)
            .collect();
        if wins.len() < 2 {
            return;
        }
        for win in wins {
            let (slot, icon) = match self.clients.get(&win) {
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
            if let Some(c) = self.clients.get_mut(&win) {
                c.icon_rotated = Some(rotated);
            }
        }
    }

    /// The icon to draw for `win`: the pre-rendered hue-rotated variant
    /// while another window of the same app class is open (same-app
    /// disambiguation), the plain icon otherwise.
    pub(crate) fn icon_for(&self, win: Win) -> Option<Rc<Icon>> {
        let client = self.clients.get(&win)?;
        let siblings = self
            .clients
            .values()
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
    /// preferably >=) the tab height. The property is a list of
    /// `width, height, w*h ARGB pixels` blocks packed as 32-bit CARDINALs.
    fn fetch_icon(&self, win: Win) -> Option<Rc<Icon>> {
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

    /// Resolve `class` against the icon theme (the same lookup the launcher
    /// menu uses), for clients that don't provide `_NET_WM_ICON`.
    fn theme_icon(&self, class: &str) -> Option<Rc<Icon>> {
        let path = crate::menu::find_icon_file(class)
            .or_else(|| crate::menu::find_icon_file(&class.to_lowercase()))?;
        let img = icon::load_png(&path)?;
        Some(Rc::new(icon::quantize(self.renderer.palette(), &img)))
    }

    /// `_NET_WM_ICON` changed on a managed window: refetch and redraw. Apps
    /// that set the property only after mapping (Electron, notably) would
    /// otherwise keep whatever `manage` resolved at map time.
    pub(crate) fn on_icon_change(&mut self, win: Win) -> R<()> {
        let Some(class) = self.clients.get(&win).map(|c| c.class.clone()) else {
            return Ok(());
        };
        let Some(icon) = self.fetch_icon(win) else {
            return Ok(());
        };
        let client = self.clients.get_mut(&win).expect("checked above");
        client.icon = Some(icon);
        client.icon_rotated = None;
        self.refresh_icon_rotations(&class);
        self.arrange()
    }

    // --- focus & spawning ---

    /// A guaranteed-fresh server timestamp: append zero bytes to a property
    /// on our never-mapped selection owner (which has `PROPERTY_CHANGE`
    /// selected) and read the time off the resulting PropertyNotify — the
    /// standard ICCCM trick. Events drained while waiting are stashed in
    /// `pending_events` for the main loop, preserving their order.
    fn fresh_timestamp(&mut self) -> R<u32> {
        self.conn.change_property8(
            PropMode::APPEND,
            self.sel_owner,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            &[],
        )?;
        self.conn.flush()?;
        // Deadline-bounded, not an unbounded wait: if the notify is lost or
        // the server wedges with the socket still open, blocking forever
        // here bricks the whole WM. On timeout, degrade to CURRENT_TIME —
        // a possibly-ignored focus request beats a hang. Connection errors
        // still propagate.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match super::wait_event_deadline(&self.conn, Some(deadline))? {
                Some(x11rb::protocol::Event::PropertyNotify(e)) if e.window == self.sel_owner => {
                    self.last_event_time = e.time;
                    self.last_event_instant = std::time::Instant::now();
                    return Ok(e.time);
                }
                Some(ev) => self.pending_events.push(ev),
                None => return Ok(CURRENT_TIME),
            }
        }
    }

    /// Deliver input focus per the ICCCM model: `SetInputFocus` only when
    /// `WM_HINTS.input` allows it, plus a `WM_TAKE_FOCUS` handshake for
    /// clients that asked for one — with a real timestamp, not
    /// `CURRENT_TIME`, so a slow client can't steal focus back across a race.
    /// A `SetInputFocus` older than the server's last focus change is
    /// silently ignored, so a harvested timestamp that has gone stale
    /// (nothing we receive carries times while the user types into a
    /// client) is replaced with a freshly fetched one.
    fn give_focus(&mut self, win: Win, model: FocusModel) -> R<()> {
        let time = self.focus_timestamp()?;
        if model.input {
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, win, time)?;
        }
        if model.take_focus {
            let msg = ClientMessageEvent::new(
                32,
                win,
                self.atoms.wm_protocols,
                [self.atoms.wm_take_focus, time, 0, 0, 0],
            );
            self.conn.send_event(false, win, EventMask::NO_EVENT, msg)?;
        }
        Ok(())
    }

    /// The timestamp `SetInputFocus`/`WM_TAKE_FOCUS` should carry: the last
    /// harvested event time while it's fresh, a freshly fetched server time
    /// once it has gone stale (a stale timestamp is silently ignored if
    /// focus moved more recently).
    fn focus_timestamp(&mut self) -> R<u32> {
        let stale = self.last_event_time == 0
            || self.last_event_instant.elapsed() > std::time::Duration::from_secs(2);
        // `?`, not unwrap_or: fresh_timestamp already degrades to
        // CURRENT_TIME on timeout, so an Err from it is a real (likely
        // connection) failure that must not be silently eaten here.
        if stale {
            self.fresh_timestamp()
        } else {
            Ok(self.last_event_time)
        }
    }

    pub(crate) fn focus(&mut self, win: Option<Win>) -> R<()> {
        match win {
            Some(w) if self.clients.contains_key(&w) => {
                self.focused_float = None;
                let model = self.clients[&w].focus;
                self.give_focus(w, model)?;
                // Raising the focused client puts it above everything;
                // re-apply arrange's stacking policy above it (floats, then
                // fullscreen, then notifications; the menu below).
                self.conn
                    .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
                self.raise_floats()?;
                self.raise_fullscreen()?;
                self.raise_notifications()?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    self.atoms.net_active_window,
                    AtomEnum::WINDOW,
                    &[w],
                )?;
            }
            _ => {
                self.focused_float = None;
                let time = self.focus_timestamp()?;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, time)?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    self.atoms.net_active_window,
                    AtomEnum::WINDOW,
                    &[x11rb::NONE],
                )?;
            }
        }
        // Raising the focused client puts it above everything, including an
        // open launcher menu; the menu must stay on top (`arrange` does the
        // same after its raises).
        self.raise_menu()?;
        Ok(())
    }

    pub(crate) fn spawn_terminal(&self) {
        let term = std::env::var("TERMINAL").unwrap_or_else(|_| "xterm".into());
        self.spawn(&term);
    }

    /// Spawn a shell command, detached. `sh -c "cmd &"` reparents the real
    /// command to init immediately; waiting on the short-lived `sh` itself
    /// (it exits as soon as it has forked) is what actually prevents a
    /// zombie per launch — dropping the `Child` would leave it unreaped.
    ///
    /// When systemd-run is available the command is placed in its own
    /// transient scope under app.slice, like a desktop-environment launcher
    /// would; Chromium/Electron apps otherwise try to move themselves out of
    /// the shared session scope and log a spurious UnitExists error.
    #[allow(clippy::unused_self)]
    pub(crate) fn spawn(&self, cmd: &str) {
        // Both paths hand `cmd` to `/bin/sh -c` as one quoted word, so a
        // command line containing `;`/`&&` behaves identically whether or
        // not systemd-run is available (a bare `{cmd} &` fallback would
        // background only the last statement of a compound command).
        let line = if Self::have_systemd_run() {
            format!(
                "systemd-run --user --scope --slice=app.slice --collect --quiet -- /bin/sh -c {} &",
                shell_quote(cmd)
            )
        } else {
            format!("/bin/sh -c {} &", shell_quote(cmd))
        };
        match std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(line)
            .spawn()
        {
            Ok(mut sh) => {
                // Reap the short-lived `sh` off-thread: it exits as soon as
                // it has forked, but even that wait doesn't belong on the
                // event loop.
                std::thread::spawn(move || {
                    let _ = sh.wait();
                });
            }
            Err(e) => eprintln!("splitwm: failed to spawn '{cmd}': {e}"),
        }
    }

    /// Whether `systemd-run` exists and a user manager is reachable.
    /// Checked once and cached; false on non-systemd setups or bare X
    /// sessions. The probe is a synchronous D-Bus round trip, so `run`
    /// warms it at startup rather than letting the first launch pay for it
    /// inside the event loop.
    pub(crate) fn have_systemd_run() -> bool {
        use std::sync::OnceLock;
        static HAVE: OnceLock<bool> = OnceLock::new();
        *HAVE.get_or_init(|| {
            std::process::Command::new("systemd-run")
                .args(["--user", "--scope", "--collect", "--quiet", "--", "true"])
                .status()
                .is_ok_and(|s| s.success())
        })
    }
}

/// Single-quote `s` for use as one `sh` word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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
