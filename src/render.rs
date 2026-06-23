//! Software rendering of leaf decorations (tab bar, focus border, content
//! background) with tiny-skia. Produces a BGRX byte buffer ready for X
//! PutImage on a depth-24 TrueColor visual.

use fontdue::Font;
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapMut, Rect as SkRect, Stroke, Transform,
};

use crate::theme;

pub struct Renderer {
    font: Font,
}

pub struct TabInfo {
    pub label: char,
    pub color: u32, // ARGB accent for this client
    pub active: bool,
}

pub struct LeafView {
    pub w: i32,
    pub h: i32, // frame height (content height + gap)
    pub tb_h: i32,
    pub bw: i32,
    pub focused: bool,
    pub tabs: Vec<TabInfo>,
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

impl Renderer {
    pub fn new() -> Self {
        let font = load_system_font();
        Renderer { font }
    }

    /// Render the leaf frame. Returns BGRX bytes (4 bytes/pixel).
    pub fn render(&self, v: &LeafView) -> Vec<u8> {
        let w = v.w.max(1) as u32;
        let h = v.h.max(1) as u32;
        let mut pm = Pixmap::new(w, h).unwrap();

        // Whole frame opaque background (wallpaper colour) so gaps blend in.
        pm.fill(argb(theme::WALLPAPER));

        let tb_h = v.tb_h as f32;
        let bw = v.bw as f32;
        let content_top = tb_h;
        let content_h = (v.h as f32) - tb_h;

        // Content background panel (rounded) just inside the border.
        let panel = rounded_rect(
            bw,
            content_top,
            (v.w as f32 - 2.0 * bw).max(1.0),
            (content_h - bw).max(1.0),
            theme::BORDER_RADIUS,
        );
        let mut bg = Paint::default();
        bg.set_color(argb(0xff000000));
        bg.anti_alias = true;
        pm.fill_path(&panel, &bg, FillRule::Winding, Transform::identity(), None);

        // Focus border around content.
        let border_col = if v.focused {
            theme::COLOR_ACCENT
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
        pm.stroke_path(&border, &stroke_paint, &stroke, Transform::identity(), None);

        // Tabs.
        self.draw_tabs(&mut pm.as_mut(), v, tb_h);

        to_bgrx(&pm)
    }

    fn draw_tabs(&self, pm: &mut PixmapMut, v: &LeafView, tb_h: f32) {
        let icon = tb_h - 4.0;
        let slot = TAB_PAD_H + icon + TAB_PAD_H + TAB_GAP;
        let mut x = (v.bw as f32) + 4.0;
        for tab in &v.tabs {
            let tw = TAB_PAD_H * 2.0 + icon;
            let path = tab_path(x, 0.0, tw, tb_h);
            let mut p = Paint::default();
            p.anti_alias = true;
            let fill = if tab.active {
                blend(tab.color, 0xff202020)
            } else {
                0xff141414
            };
            p.set_color(argb(fill));
            pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
            if tab.active {
                let mut sp = Paint::default();
                sp.anti_alias = true;
                sp.set_color(argb(tab.color | 0xff000000));
                pm.stroke_path(
                    &path,
                    &sp,
                    &Stroke { width: 2.0, ..Default::default() },
                    Transform::identity(),
                    None,
                );
            }
            // Centered label glyph.
            let cx = x + tw / 2.0;
            self.draw_glyph(pm, tab.label, cx, tb_h / 2.0 + 2.0, icon * 0.7, theme::COLOR_FG);
            x += slot;
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
        let (cr, cg, cb) = (
            ((color >> 16) & 0xff) as u32,
            ((color >> 8) & 0xff) as u32,
            (color & 0xff) as u32,
        );
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let a = bitmap[gy * metrics.width + gx] as u32;
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
                    let dst = data[idx + k] as u32;
                    data[idx + k] = ((cc * a + dst * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }
}

fn blend(top: u32, bottom: u32) -> u32 {
    let a = ((top >> 24) & 0xff) as u32;
    let mix = |s: u32, d: u32| ((s * a + d * (255 - a)) / 255) & 0xff;
    let r = mix((top >> 16) & 0xff, (bottom >> 16) & 0xff);
    let g = mix((top >> 8) & 0xff, (bottom >> 8) & 0xff);
    let b = mix(top & 0xff, bottom & 0xff);
    0xff000000 | (r << 16) | (g << 8) | b
}

/// A tab: rounded-top trapezoid, wider at the bottom (slanted sides at 20°),
/// approximating splitwm's tab_path.
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
    pb.quad_to(tl + (r * TAB_SLANT), y, tl + r, y);
    pb.line_to(tr - r, y);
    pb.quad_to(tr - (r * TAB_SLANT), y, tr, y + r);
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

fn to_bgrx(pm: &Pixmap) -> Vec<u8> {
    let src = pm.data();
    let mut out = vec![0u8; src.len()];
    for i in (0..src.len()).step_by(4) {
        // tiny-skia: R,G,B,A (premultiplied; opaque here) -> B,G,R,X
        out[i] = src[i + 2];
        out[i + 1] = src[i + 1];
        out[i + 2] = src[i];
        out[i + 3] = 0;
    }
    out
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
        } else if p.extension().map_or(false, |x| x == "ttf") {
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(f) = Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(f);
                }
            }
        }
    }
    None
}
