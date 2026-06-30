//! Software rendering of leaf decorations (tab bar, focus border, content
//! background) with tiny-skia. Produces a BGRX byte buffer ready for X
//! `PutImage` on a depth-24 `TrueColor` visual.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::unnecessary_cast,
    clippy::many_single_char_names,
    clippy::tuple_array_conversions
)]

use std::rc::Rc;

use fontdue::Font;
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Rect as SkRect, Stroke,
    Transform,
};

use crate::theme;

pub struct Renderer {
    font: Font,
    /// Screen-sized scaled wallpaper; frame backgrounds copy their slice of it.
    wallpaper: Option<Pixmap>,
}

/// A decoded application icon (non-premultiplied ARGB pixels, row-major).
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub argb: Vec<u32>,
}

pub struct TabInfo {
    pub label: char,
    pub color: u32, // ARGB accent for this split
    pub icon: Option<Rc<Icon>>,
}

pub struct LeafView {
    pub w: i32,
    pub h: i32, // frame height (content height + gap)
    pub tb_h: i32,
    pub bw: i32,
    pub focused: bool,
    /// Accent colour of the split (focus-border tint when focused).
    pub accent: u32,
    /// The split's single window, if any.
    pub tab: Option<TabInfo>,
}

fn argb(c: u32) -> Color {
    let a = ((c >> 24) & 0xff) as u8;
    let r = ((c >> 16) & 0xff) as u8;
    let g = ((c >> 8) & 0xff) as u8;
    let b = (c & 0xff) as u8;
    Color::from_rgba8(r, g, b, a)
}

const TAB_PAD_H: f32 = 22.0;
const TAB_SLANT: f32 = 0.364; // tan(20deg)
const TAB_GAP: f32 = 6.0;
const TAB_CORNER: f32 = 9.0;

/// X (leaf-local) and width of the title tab's clickable slot.
pub fn tab_slot(bw: i32, tb_h: i32, i: i32) -> (f32, f32) {
    let icon = tb_h as f32 - 4.0;
    let slot = TAB_PAD_H + icon + TAB_PAD_H + TAB_GAP;
    let tw = TAB_PAD_H.mul_add(2.0, icon);
    ((i as f32).mul_add(slot, bw as f32 + 4.0), tw)
}

impl Renderer {
    pub fn new() -> Self {
        let font = load_system_font();
        Self {
            font,
            wallpaper: None,
        }
    }

    /// Load+scale a PNG wallpaper to cover `w`x`h`. Returns whether it loaded.
    pub fn set_wallpaper(&mut self, path: &str, w: i32, h: i32) -> bool {
        self.wallpaper = load_wallpaper_pixmap(path, w, h);
        self.wallpaper.is_some()
    }

    /// A fresh screen-sized pixmap initialised with the wallpaper (or the
    /// solid background colour). All leaf chrome is composited onto this.
    pub fn screen_base(&self, w: u32, h: u32) -> Pixmap {
        let (w, h) = (w.max(1), h.max(1));
        // The wallpaper is pre-scaled to the screen size, so a clone (raw
        // memcpy) reproduces the base far cheaper than re-running the alpha
        // blend through draw_pixmap on every single frame.
        if let Some(wp) = &self.wallpaper {
            if wp.width() == w && wp.height() == h {
                return wp.clone();
            }
        }
        let mut pm = Pixmap::new(w, h).unwrap();
        if let Some(wp) = &self.wallpaper {
            pm.as_mut().draw_pixmap(
                0,
                0,
                wp.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        } else {
            pm.fill(argb(theme::WALLPAPER));
        }
        pm
    }

    /// Draw one leaf's chrome (content panel, focus border, tab bar) into the
    /// shared screen pixmap at screen offset (ox, oy). The background (gaps and
    /// the strip behind the tab bar) is whatever was already composited, so
    /// the wallpaper shows through — no opaque per-leaf box.
    pub fn draw_leaf(&self, pm: &mut PixmapMut, ox: f32, oy: f32, v: &LeafView) {
        let tf = Transform::from_translate(ox, oy);
        let tb_h = v.tb_h as f32;
        let bw = v.bw as f32;
        let content_top = tb_h;
        let content_h = (v.h as f32) - tb_h;

        // Content background panel (rounded) just inside the border.
        let panel = rounded_rect(
            bw,
            content_top,
            2.0f32.mul_add(-bw, v.w as f32).max(1.0),
            (content_h - bw).max(1.0),
            theme::BORDER_RADIUS,
        );
        let mut bg = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        bg.set_color(argb(0xff00_0000));
        pm.fill_path(&panel, &bg, FillRule::Winding, tf, None);

        // Focus border around content.
        let border_col = if v.focused {
            v.accent | 0xff00_0000
        } else {
            theme::COLOR_HANDLE
        };
        let mut stroke_paint = Paint::default();
        stroke_paint.set_color(argb(border_col));
        stroke_paint.anti_alias = true;
        let stroke = Stroke {
            width: bw,
            ..Default::default()
        };
        let border = rounded_rect(
            bw / 2.0,
            content_top - bw / 2.0,
            (v.w as f32 - bw).max(1.0),
            (content_h - bw / 2.0).max(1.0),
            theme::BORDER_RADIUS + bw / 2.0,
        );
        pm.stroke_path(&border, &stroke_paint, &stroke, tf, None);

        self.draw_tabs(pm, ox, oy, v, tb_h);
    }

    fn draw_tabs(&self, pm: &mut PixmapMut, ox: f32, oy: f32, v: &LeafView, tb_h: f32) {
        let Some(tab) = &v.tab else {
            return;
        };
        let tf = Transform::from_translate(ox, oy);
        let icon = tb_h - 4.0;
        let (x, tw) = tab_slot(v.bw, tb_h as i32, 0);
        let path = tab_path(x, 0.0, tw, tb_h);
        let mut p = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color(argb(blend(tab.color, 0xff20_2020)));
        pm.fill_path(&path, &p, FillRule::Winding, tf, None);
        let mut sp = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        sp.set_color(argb(tab.color | 0xff00_0000));
        pm.stroke_path(
            &path,
            &sp,
            &Stroke {
                width: 2.0,
                ..Default::default()
            },
            tf,
            None,
        );
        // Centered app icon, or a letter glyph as fallback (absolute coords).
        let cx = ox + x + tw / 2.0;
        let cy = oy + tb_h / 2.0;
        if let Some(img) = &tab.icon {
            let isz = (icon * 0.92).round();
            draw_icon(pm, img, cx - isz / 2.0, cy - isz / 2.0, isz);
        } else {
            self.draw_glyph(pm, tab.label, cx, cy + 2.0, icon * 0.7, theme::COLOR_FG);
        }
    }

    fn draw_glyph(&self, pm: &mut PixmapMut, ch: char, cx: f32, cy: f32, px: f32, color: u32) {
        let (metrics, bitmap) = self.font.rasterize(ch, px);
        if metrics.width == 0 || metrics.height == 0 {
            return;
        }
        let ox = (cx - metrics.width as f32 / 2.0).round() as i32;
        let oy = (cy - metrics.height as f32 / 2.0).round() as i32;
        let pw = pm.width() as i32;
        let ph = pm.height() as i32;
        let data = pm.data_mut();
        let [cr, cg, cb] = [
            ((color >> 16) & 0xff) as u32,
            ((color >> 8) & 0xff) as u32,
            (color & 0xff) as u32,
        ];
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let a = u32::from(bitmap[gy * metrics.width + gx]);
                if a == 0 {
                    continue;
                }
                let px_ = ox + gx as i32;
                let py_ = oy + gy as i32;
                if px_ < 0 || py_ < 0 || px_ >= pw || py_ >= ph {
                    continue;
                }
                let idx = ((py_ * pw + px_) * 4) as usize;
                // tiny-skia pixmap is premultiplied RGBA; blend glyph over.
                for (k, cc) in [cr, cg, cb].iter().enumerate() {
                    let dst = u32::from(data[idx + k]);
                    data[idx + k] = ((cc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }

    /// Draw one taskbar entry: a rounded background tile with the app icon
    /// (or letter-glyph fallback) centred in it.
    pub fn draw_taskbar_item(
        &self,
        pm: &mut PixmapMut,
        r: TaskItem,
        icon: Option<&Icon>,
        label: char,
        color: u32,
        highlight: bool,
    ) {
        let bgp = rounded_rect(r.x, r.y, r.w.max(1.0), r.h.max(1.0), 8.0);
        let mut bg = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        bg.set_color(argb(theme::COLOR_BTN_BG));
        pm.fill_path(&bgp, &bg, FillRule::Winding, Transform::identity(), None);

        let cx = r.x + r.w / 2.0;
        let cy = r.y + r.h / 2.0;
        let isz = r.h.min(r.w) - 6.0;
        if let Some(img) = icon {
            draw_icon(pm, img, cx - isz / 2.0, cy - isz / 2.0, isz);
        } else {
            self.draw_glyph(pm, label, cx, cy + 2.0, isz * 0.7, color);
        }

        // Windows currently shown in a split get an accent box around them.
        if highlight {
            let mut sp = Paint::<'_> {
                anti_alias: true,
                ..Default::default()
            };
            sp.set_color(argb(color | 0xff00_0000));
            pm.stroke_path(
                &bgp,
                &sp,
                &Stroke {
                    width: 2.0,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
}

/// A taskbar tile rectangle (screen coords) for the renderer.
#[derive(Clone, Copy)]
pub struct TaskItem {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Blit `img` scaled to a `size`x`size` box at (dx, dy), alpha-blending each
/// source pixel over the (premultiplied RGBA) pixmap.
fn draw_icon(pm: &mut PixmapMut, img: &Icon, dx: f32, dy: f32, size: f32) {
    if img.w == 0 || img.h == 0 || size < 1.0 {
        return;
    }
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let data = pm.data_mut();
    let isz = size as i32;
    let ox = dx.round() as i32;
    let oy = dy.round() as i32;
    for ty in 0..isz {
        let sy = (ty as u32 * img.h / isz as u32).min(img.h - 1);
        let py = oy + ty;
        if py < 0 || py >= ph {
            continue;
        }
        for tx in 0..isz {
            let sx = (tx as u32 * img.w / isz as u32).min(img.w - 1);
            let px = ox + tx;
            if px < 0 || px >= pw {
                continue;
            }
            let s = img.argb[(sy * img.w + sx) as usize];
            let a = (s >> 24) & 0xff;
            if a == 0 {
                continue;
            }
            let (sr, sg, sb) = ((s >> 16) & 0xff, (s >> 8) & 0xff, s & 0xff);
            let idx = ((py * pw + px) * 4) as usize;
            // Source is straight ARGB; pixmap is premultiplied RGBA.
            for (k, sc) in [sr, sg, sb].iter().enumerate() {
                let dst = u32::from(data[idx + k]);
                data[idx + k] = ((sc * a + dst * (255 - a)) / 255) as u8;
            }
            data[idx + 3] = 255;
        }
    }
}

/// Draw a rounded "pill" gap drag-handle into the screen pixmap.
pub fn draw_handle(pm: &mut PixmapMut, x: f32, y: f32, w: f32, h: f32, hot: bool) {
    let path = rounded_rect(x, y, w.max(1.0), h.max(1.0), w / 2.0);
    let mut p = Paint::<'_> {
        anti_alias: true,
        ..Default::default()
    };
    p.set_color(argb(if hot {
        theme::COLOR_FG
    } else {
        theme::COLOR_HANDLE
    }));
    pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
}

/// Draw a translucent rounded "+" insert button centred at (cx, cy).
pub fn draw_plus(pm: &mut PixmapMut, cx: f32, cy: f32, sz: f32) {
    let half = sz / 2.0;
    let bgp = rounded_rect(cx - half, cy - half, sz, sz, sz * 0.28);
    let mut bg = Paint::<'_> {
        anti_alias: true,
        ..Default::default()
    };
    bg.set_color(argb(theme::COLOR_BTN_BG));
    pm.fill_path(&bgp, &bg, FillRule::Winding, Transform::identity(), None);

    let arm = sz * 0.28;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - arm, cy);
    pb.line_to(cx + arm, cy);
    pb.move_to(cx, cy - arm);
    pb.line_to(cx, cy + arm);
    if let Some(path) = pb.finish() {
        let mut p = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color(argb(theme::COLOR_FG));
        let stroke = Stroke {
            width: 2.5,
            ..Default::default()
        };
        pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }
}

/// The split-control buttons drawn at the right of each leaf's tab bar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnIcon {
    MinimizeV,
    MinimizeH,
    ExpandV,
    ExpandH,
    Swap,
    VSplit,
    HSplit,
    Close,
}

/// Draw one round split-control button centred at (cx, cy): a circular
/// background plus its 2px stroked glyph, ported from splitwm `icons.lua`.
pub fn draw_button(
    pm: &mut PixmapMut,
    cx: f32,
    cy: f32,
    size: f32,
    icon: BtnIcon,
    disabled: bool,
    picked: bool,
) {
    let mut bg = Paint::<'_> {
        anti_alias: true,
        ..Default::default()
    };
    bg.set_color(argb(if picked {
        theme::COLOR_FG
    } else {
        theme::COLOR_BTN_BG
    }));
    let mut cb = PathBuilder::new();
    cb.push_circle(cx, cy, size / 2.0);
    pm.fill_path(
        &cb.finish().unwrap(),
        &bg,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let col = if disabled {
        theme::COLOR_FG_DISABLED
    } else if picked {
        theme::COLOR_BG
    } else {
        theme::COLOR_FG
    };
    if let Some(path) = build_glyph_path(cx, cy, size, icon) {
        let mut p = Paint::<'_> {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color(argb(col));
        let stroke = Stroke {
            width: 2.0,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }
}

/// Build the glyph path for a button icon, or None if empty.
fn build_glyph_path(cx: f32, cy: f32, size: f32, icon: BtnIcon) -> Option<tiny_skia::Path> {
    // Glyph geometry uses a local (w, h) box; (ox, oy) is its top-left corner.
    let (w, h) = (size, size);
    let (ox, oy) = (cx - w / 2.0, cy - h / 2.0);
    let mut pb = PathBuilder::new();
    let mut seg = |pts: &[(f32, f32)]| {
        for (i, &(px, py)) in pts.iter().enumerate() {
            if i == 0 {
                pb.move_to(ox + px, oy + py);
            } else {
                pb.line_to(ox + px, oy + py);
            }
        }
    };
    let (cxl, cyl) = (w / 2.0, h / 2.0);
    match icon {
        BtnIcon::Close => {
            let s = 4.0;
            seg(&[(cxl - s, cyl - s), (cxl + s, cyl + s)]);
            seg(&[(cxl + s, cyl - s), (cxl - s, cyl + s)]);
        }
        BtnIcon::VSplit | BtnIcon::HSplit => {
            let (bw, bh) = (10.0, 10.0);
            let (bx, by) = (cxl - bw / 2.0, cyl - bh / 2.0);
            seg(&[
                (bx, by),
                (bx + bw, by),
                (bx + bw, by + bh),
                (bx, by + bh),
                (bx, by),
            ]);
            if icon == BtnIcon::VSplit {
                seg(&[(cxl, by + 1.0), (cxl, by + bh - 1.0)]);
            } else {
                seg(&[(bx + 1.0, cyl), (bx + bw - 1.0, cyl)]);
            }
        }
        BtnIcon::MinimizeH => {
            let (g, a) = (3.0, 4.0);
            seg(&[
                (cxl - g - a, cyl - a),
                (cxl - g, cyl),
                (cxl - g - a, cyl + a),
            ]);
            seg(&[
                (cxl + g + a, cyl - a),
                (cxl + g, cyl),
                (cxl + g + a, cyl + a),
            ]);
        }
        BtnIcon::ExpandH => {
            let (g, a) = (3.0, 4.0);
            seg(&[(cxl - g, cyl - a), (cxl - g - a, cyl), (cxl - g, cyl + a)]);
            seg(&[(cxl + g, cyl - a), (cxl + g + a, cyl), (cxl + g, cyl + a)]);
        }
        BtnIcon::MinimizeV => {
            let (g, a) = (3.0, 4.0);
            seg(&[
                (cxl - a, cyl - g - a),
                (cxl, cyl - g),
                (cxl + a, cyl - g - a),
            ]);
            seg(&[
                (cxl - a, cyl + g + a),
                (cxl, cyl + g),
                (cxl + a, cyl + g + a),
            ]);
        }
        BtnIcon::ExpandV => {
            let (g, a) = (3.0, 4.0);
            seg(&[(cxl - a, cyl - g), (cxl, cyl - g - a), (cxl + a, cyl - g)]);
            seg(&[(cxl - a, cyl + g), (cxl, cyl + g + a), (cxl + a, cyl + g)]);
        }
        BtnIcon::Swap => {
            let (s, ay) = (4.0, 3.0);
            seg(&[(cxl - s, cyl - ay), (cxl + s, cyl - ay)]);
            seg(&[
                (cxl + s - 3.0, cyl - ay - 2.0),
                (cxl + s, cyl - ay),
                (cxl + s - 3.0, cyl - ay + 2.0),
            ]);
            seg(&[(cxl + s, cyl + ay), (cxl - s, cyl + ay)]);
            seg(&[
                (cxl - s + 3.0, cyl + ay - 2.0),
                (cxl - s, cyl + ay),
                (cxl - s + 3.0, cyl + ay + 2.0),
            ]);
        }
    }
    pb.finish()
}

const fn blend(top: u32, bottom: u32) -> u32 {
    let a = ((top >> 24) & 0xff) as u32;
    let r = ((((top >> 16) & 0xff) * a + ((bottom >> 16) & 0xff) * (255 - a)) / 255) & 0xff;
    let g = ((((top >> 8) & 0xff) * a + ((bottom >> 8) & 0xff) * (255 - a)) / 255) & 0xff;
    let b = (((top & 0xff) * a + (bottom & 0xff) * (255 - a)) / 255) & 0xff;
    0xff00_0000 | (r << 16) | (g << 8) | b
}

/// A tab: rounded-top trapezoid, wider at the bottom (slanted sides at 20°),
/// approximating splitwm's `tab_path`.
fn tab_path(x: f32, y: f32, w: f32, h: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    let slant = h * TAB_SLANT;
    let r = TAB_CORNER;
    let bl = x; // bottom-left
    let br = x + w; // bottom-right
    let tl = x + slant; // top-left
    let tr = x + w - slant; // top-right
    pb.move_to(bl, y + h);
    pb.line_to(tl, y + r);
    pb.quad_to(r.mul_add(TAB_SLANT, tl), y, tl + r, y);
    pb.line_to(tr - r, y);
    pb.quad_to(r.mul_add(-TAB_SLANT, tr), y, tr, y + r);
    pb.line_to(br, y + h);
    pb.close();
    pb.finish().unwrap()
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.1 {
        let mut pb = PathBuilder::new();
        pb.push_rect(SkRect::from_xywh(x, y, w, h).unwrap());
        return pb.finish().unwrap();
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

// --- application launcher menu ---

pub const MENU_ROW_H: i32 = 26;
pub const MENU_BORDER: i32 = 8;
const MENU_PAD_X: f32 = 12.0;
const MENU_FONT_PX: f32 = 14.0;
const MENU_ARROW_W: f32 = 16.0;

impl Renderer {
    /// Advance width of a left-aligned string at pixel size `px`.
    fn text_width(&self, s: &str, px: f32) -> f32 {
        s.chars()
            .map(|c| self.font.metrics(c, px).advance_width)
            .sum()
    }

    /// Inner content width (excludes the black border) for a menu column.
    pub fn menu_content_w(&self, labels: &[String], any_arrow: bool) -> i32 {
        let mut w = 0.0f32;
        for l in labels {
            w = w.max(self.text_width(l, MENU_FONT_PX));
        }
        let arrow = if any_arrow { MENU_ARROW_W } else { 0.0 };
        (MENU_PAD_X.mul_add(2.0, w) + arrow).ceil() as i32
    }

    /// Render a menu column to its own pixmap. `content_w` is the inner width
    /// (shared across a menu + submenu so columns line up); `seps` marks
    /// divider rows, `hi` is the hovered row.
    pub fn draw_menu(
        &self,
        labels: &[String],
        arrows: &[bool],
        seps: &[bool],
        content_w: i32,
        hi: Option<usize>,
    ) -> Pixmap {
        let rows = labels.len() as i32;
        let w = (content_w + 2 * MENU_BORDER).max(1) as u32;
        let h = (rows * MENU_ROW_H + 2 * MENU_BORDER).max(1) as u32;
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(argb(0xff00_0000));
        let b = MENU_BORDER as f32;
        let cw = content_w as f32;
        let mut m = pm.as_mut();
        for (i, label) in labels.iter().enumerate() {
            let ry = (i as i32 as f32).mul_add(MENU_ROW_H as f32, b);
            if seps.get(i).copied().unwrap_or(false) {
                // Faint divider line centred in the row.
                let mut p = Paint::<'_> {
                    anti_alias: false,
                    ..Default::default()
                };
                p.set_color(argb(0x33ff_ffff));
                if let Some(rect) =
                    SkRect::from_xywh(b + 4.0, ry + MENU_ROW_H as f32 / 2.0, cw - 8.0, 1.0)
                {
                    let mut pb = PathBuilder::new();
                    pb.push_rect(rect);
                    m.fill_path(
                        &pb.finish().unwrap(),
                        &p,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
                continue;
            }
            if Some(i) == hi {
                let hp = rounded_rect(b + 2.0, ry + 1.0, cw - 4.0, MENU_ROW_H as f32 - 2.0, 4.0);
                let mut p = Paint::<'_> {
                    anti_alias: true,
                    ..Default::default()
                };
                p.set_color(argb(theme::COLOR_FG_HOVER));
                m.fill_path(&hp, &p, FillRule::Winding, Transform::identity(), None);
            }
            let baseline = MENU_FONT_PX.mul_add(0.35, ry + MENU_ROW_H as f32 / 2.0);
            self.draw_text(
                &mut m,
                label,
                b + MENU_PAD_X,
                baseline,
                MENU_FONT_PX,
                theme::COLOR_FG,
            );
            if arrows.get(i).copied().unwrap_or(false) {
                // Small right-pointing triangle (▸) drawn as a path.
                let ax = b + cw - MENU_ARROW_W + 4.0;
                let ay = ry + MENU_ROW_H as f32 / 2.0;
                let mut pb = PathBuilder::new();
                pb.move_to(ax, ay - 4.0);
                pb.line_to(ax + 6.0, ay);
                pb.line_to(ax, ay + 4.0);
                pb.close();
                let mut p = Paint::<'_> {
                    anti_alias: true,
                    ..Default::default()
                };
                p.set_color(argb(theme::COLOR_FG));
                m.fill_path(
                    &pb.finish().unwrap(),
                    &p,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        pm
    }

    /// Draw a left-aligned UTF-8 string with its baseline at `y`.
    fn draw_text(&self, pm: &mut PixmapMut, text: &str, x: f32, y: f32, px: f32, color: u32) {
        let mut pen = x;
        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(ch, px);
            if metrics.width > 0 && metrics.height > 0 {
                let gx = (pen + metrics.xmin as f32).round() as i32;
                let gy = (y - (metrics.ymin + metrics.height as i32) as f32).round() as i32;
                Self::blit_coverage(pm, &bitmap, metrics.width, metrics.height, gx, gy, color);
            }
            pen += metrics.advance_width;
        }
    }

    /// Alpha-blend an 8-bit coverage bitmap in `color` onto the pixmap.
    #[allow(clippy::too_many_arguments)]
    fn blit_coverage(
        pm: &mut PixmapMut,
        bitmap: &[u8],
        bw: usize,
        bh: usize,
        ox: i32,
        oy: i32,
        color: u32,
    ) {
        let pw = pm.width() as i32;
        let ph = pm.height() as i32;
        let data = pm.data_mut();
        let [cr, cg, cb] = [
            ((color >> 16) & 0xff) as u32,
            ((color >> 8) & 0xff) as u32,
            (color & 0xff) as u32,
        ];
        for gy in 0..bh {
            for gx in 0..bw {
                let a = u32::from(bitmap[gy * bw + gx]);
                if a == 0 {
                    continue;
                }
                let (px_, py_) = (ox + gx as i32, oy + gy as i32);
                if px_ < 0 || py_ < 0 || px_ >= pw || py_ >= ph {
                    continue;
                }
                let idx = ((py_ * pw + px_) * 4) as usize;
                for (k, cc) in [cr, cg, cb].iter().enumerate() {
                    let dst = u32::from(data[idx + k]);
                    data[idx + k] = ((cc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }
}

/// Public wrapper: convert a tiny-skia pixmap to `PutImage`-ready BGRX bytes.
/// Convert a tiny-skia pixmap to X11 BGRX bytes, reusing `out`'s allocation
/// (resized as needed) so the full-screen buffer isn't reallocated each frame.
pub fn pixmap_to_bgrx(pm: &Pixmap, out: &mut Vec<u8>) {
    let src = pm.data();
    out.resize(src.len(), 0);
    for i in (0..src.len()).step_by(4) {
        // tiny-skia: R,G,B,A (premultiplied; opaque here) -> B,G,R,X
        out[i] = src[i + 2];
        out[i + 1] = src[i + 1];
        out[i + 2] = src[i];
        out[i + 3] = 0;
    }
}

/// Load a PNG wallpaper and scale it to cover a `w`x`h` area. `None` if it
/// can't be read.
fn load_wallpaper_pixmap(path: &str, w: i32, h: i32) -> Option<Pixmap> {
    let src = Pixmap::load_png(path).ok()?;
    let (dw, dh) = (w.max(1) as u32, h.max(1) as u32);
    let mut dst = Pixmap::new(dw, dh)?;
    dst.fill(argb(theme::COLOR_BG));
    let scale = (dw as f32 / src.width() as f32).max(dh as f32 / src.height() as f32);
    let ox = (src.width() as f32).mul_add(-scale, dw as f32) / 2.0;
    let oy = (src.height() as f32).mul_add(-scale, dh as f32) / 2.0;
    let tf = Transform::from_scale(scale, scale).post_translate(ox, oy);
    dst.as_mut()
        .draw_pixmap(0, 0, src.as_ref(), &PixmapPaint::default(), tf, None);
    Some(dst)
}

fn load_system_font() -> Font {
    let candidates = [
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(f) = Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return f;
            }
        }
    }
    // Last resort: scan a couple of font dirs for any ttf.
    for dir in ["/usr/share/fonts", "/usr/local/share/fonts"] {
        if let Some(f) = scan_for_font(std::path::Path::new(dir), 0) {
            return f;
        }
    }
    panic!("no usable TTF font found on system");
}

fn scan_for_font(dir: &std::path::Path, depth: u32) -> Option<Font> {
    if depth > 4 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = scan_for_font(&p, depth + 1) {
                return Some(f);
            }
        } else if p.extension().is_some_and(|x| x == "ttf") {
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(f) = Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(f);
                }
            }
        }
    }
    None
}
