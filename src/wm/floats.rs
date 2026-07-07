//! Floating window management: dialogs/transients/fixed-size clients that
//! never enter the split tree. Each float pairs its client window with a
//! chrome frame window (our own override-redirect border + titlebar) drawn
//! by the shared leaf renderer; dragging the frame moves the pair.

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, EventMask, StackMode, Window, WindowClass,
};

use super::input::ActiveDrag;
use super::types::{clamp_dim, FocusModel, Wm, R};
use crate::icon::Icon;
use crate::theme;
use crate::tree::Win;

/// A floating window: a dialog/transient (`WM_TRANSIENT_FOR` or
/// `_NET_WM_WINDOW_TYPE_DIALOG`) or a fixed-size client (min == max in
/// `WM_NORMAL_HINTS`). Never in the split tree/taskbar: shown at
/// its requested size, centered over its parent's split (or the workarea),
/// stacked above every tiled client, focused on map and click but not part
/// of Mod4+Tab cycling.
pub struct FloatWin {
    pub win: Win,
    /// Our own chrome window stacked just below `win`: the split border art
    /// (border + titlebar, no control buttons), draggable to move the float.
    pub frame: Window,
    /// `WM_TRANSIENT_FOR` target, used for centering and for handing focus
    /// back when the float goes away.
    pub parent: Option<Win>,
    pub focus: FocusModel,
    /// Client-window screen geometry (the frame extends `BORDER_LEFT` /
    /// `tb_h` around it), tracked so drags and repaints don't need a
    /// `GetGeometry` round trip.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Accent palette index for the chrome — the transient parent's split
    /// colour when it has one, so the dialog visibly belongs to it.
    pub accent: crate::Index,
    /// Titlebar app icon, resolved at manage time and kept live by
    /// `Wm::on_icon_change` (a late or changed `_NET_WM_ICON`).
    pub icon: Option<Rc<Icon>>,
    pub label: char,
    /// `_NET_WM_NAME`/`WM_NAME`, kept live by `Wm::on_title_change`.
    pub title: Rc<str>,
    /// `_NET_WM_ICON` fetch cooldown/staleness bookkeeping, mirroring
    /// `Client::icon_fresh` (see `IconFreshness`).
    pub icon_fresh: super::icons::IconFreshness,
}

impl Wm {
    /// Whether `win` should float instead of tiling: a transient
    /// (`WM_TRANSIENT_FOR`), a declared dialog
    /// (`_NET_WM_WINDOW_TYPE_DIALOG`), or a fixed-size window
    /// (`WM_NORMAL_HINTS` min == max — it can't be resized, so stretching
    /// it into a split only produces gravel).
    pub(crate) fn wants_float(&self, win: Win) -> bool {
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

    /// Frame insets around a float's client window: the same border art the
    /// splits use — `tb_h` above (the titlebar strip), `BORDER_LEFT` on the
    /// left/right sides, `BORDER_BOTTOM` below, matching `client_rect_in_frame`'s
    /// insets.
    fn float_insets() -> (i32, i32, i32) {
        (theme::BORDER_LEFT, theme::tb_h(), theme::BORDER_BOTTOM)
    }

    /// Float `win` (see `FloatWin`): show it at its requested size,
    /// centered over its transient parent's split frame when that parent is
    /// a tiled client currently on screen, otherwise centered in the
    /// workarea. A chrome frame window (split border + titlebar, no control
    /// buttons) is stacked just below it; dragging the frame moves the pair.
    /// It takes focus immediately (a dialog exists to be answered).
    pub(crate) fn manage_float(&mut self, win: Win) -> R<()> {
        let parent = self.transient_for(win);
        let (mut w, mut h) = self.geometry(win).map_or((400, 300), |g| {
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
        let (bw, tb, bb) = Self::float_insets();
        w = w.clamp(1, i32::from(u16::MAX) - 2 * bw);
        h = h.clamp(1, i32::from(u16::MAX) - tb - bb);
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
        let label = Self::label_from_class(&class);
        let icon = self.resolve_icon(win, &class);
        let title = self.client_title(win);

        self.select_and_grab(
            win,
            EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
            true,
        )?;
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
            u16::try_from(h + tb + bb).unwrap_or(1),
            0,
            WindowClass::INPUT_OUTPUT,
            0, // CopyFromParent
            &x11rb::protocol::xproto::CreateWindowAux::new()
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
        self.add_float(FloatWin {
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
            title,
            icon_fresh: super::icons::IconFreshness::default(),
        });
        self.conn.map_window(frame)?;
        self.conn.map_window(win)?;
        self.update_client_list()?;
        self.restack_float(win)?;
        self.paint_float_frame(frame)?;
        self.set_wm_state(win, super::clients::WmState::Normal)?;
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
    /// self-resize (`ConfigureRequest`) and fullscreen restore.
    pub(crate) fn configure_float_frame(
        &self,
        win: Win,
        frame: Win,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> R<()> {
        let (bw, tb, bb) = Self::float_insets();
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
                .height(clamp_dim(h + tb + bb)),
        )?;
        Ok(())
    }

    /// Raise a float as a unit: frame to the top, client just above it.
    pub(crate) fn restack_float(&self, win: Win) -> R<()> {
        let Some(f) = self.float_get(win) else {
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
        let Some(f) = self.floats_iter().find(|f| f.frame == frame) else {
            return Ok(());
        };
        let (bw, tb, bb) = Self::float_insets();
        let (fw, fh) = (f.w + 2 * bw, f.h + tb + bb);
        let view = crate::render::LeafView {
            w: fw,
            h: fh,
            tb_h: tb,
            bw,
            accent_index: f.accent,
            titlebar: Some(crate::render::TitleInfo {
                label: f.label,
                icon: f.icon.clone(),
                title: f.title.clone(),
            }),
            minimized: false,
            buttons: false,
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
        let (bw, tb, _bb) = Self::float_insets();
        let wa = self.wa();
        let Some(f) = self.float_mut(win) else {
            return Ok(());
        };
        // Clamp so the titlebar can't leave the screen (the frame is the
        // only handle there is to drag it back with). `on_screen_strip` is
        // how much of the float must stay reachable on either axis; it
        // happens to equal the titlebar height, but is used here as a
        // general leftover-strip size, not as a titlebar measurement.
        let on_screen_strip = theme::tb_h();
        // min/max, not clamp(): clamp panics if min > max, which a degenerate
        // workarea (narrower/shorter than the strip the float needs) can
        // trigger. max() is applied first and min() last, so on a crossed
        // range the upper bound wins and the origin is pulled back from the
        // far edge rather than left past the near one.
        let x = (x.max(wa.x - f.w + on_screen_strip)).min(wa.x + wa.w - on_screen_strip);
        let y = (y.max(wa.y + tb)).min(wa.y + wa.h - on_screen_strip);
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
        let Some(f) = self.float_get(win) else {
            return Ok(());
        };
        let focus = f.focus;
        self.give_focus(win, focus)?;
        self.set_focused_float(win);
        self.restack_float(win)?;
        self.set_net_active_window(win)
    }

    /// A float went away: drop it and hand focus back to its transient
    /// parent (if tiled and visible) or the focused split.
    pub(crate) fn forget_float(&mut self, win: Win) -> R<()> {
        let Some(gone) = self.remove_float(win) else {
            return Ok(());
        };
        self.clear_fullscreen_if(win);
        self.conn.destroy_window(gone.frame)?;
        self.update_client_list()?;
        self.drags
            .active
            .take_if(|d| matches!(d, ActiveDrag::Float(fd) if fd.win == win));
        let parent = gone.parent;
        if self.clear_focused_float_if(win) {
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
        for f in self.floats_iter() {
            self.restack_float(f.win)?;
        }
        Ok(())
    }
}
