//! Rendering a served notification (see `crate::notify`) as a speech-bubble
//! popup: summary (bold) then body, word-wrapped, on a 9-slice-stretched
//! bubble sprite.

use pixel_graphics::Framebuffer;

use crate::theme::palette_color;

use super::Renderer;

/// 9-slice caps of `bubble.png` (matching cozyui's), measured on the
/// *unmirrored* art; `draw_note` mirrors it so the tail points bottom-right,
/// toward the dock.
const BUBBLE_CAP_X: usize = 21;
const BUBBLE_CAP_TOP: usize = 17;
const BUBBLE_CAP_BOTTOM: usize = 17;
/// Text padding inside the bubble; the bottom leaves room for the tail band.
const NOTE_PAD_LEFT: usize = 14;
const NOTE_PAD_RIGHT: usize = 16;
const NOTE_PAD_TOP: usize = 10;
const NOTE_PAD_BOTTOM: usize = 16;
const NOTE_TEXT_MAX_W: usize = 280;
const NOTE_MAX_LINES: usize = 8;

impl Renderer {
    /// Pixel width of a left-aligned string in the UI font. Without a font,
    /// a rough estimate keeps bubble geometry sane.
    fn text_width(&self, s: &str) -> i32 {
        match &self.font {
            Some(font) => font.text_width(s) as i32,
            None => 8 * s.chars().count() as i32,
        }
    }

    /// Render one notification as a speech bubble: summary (bold) then body,
    /// wrapped, on the 9-slice-stretched bubble sprite. The framebuffer is
    /// exactly the popup window's size; pixels outside the bubble stay
    /// `TRANSPARENT` for the caller to shape away. Critical notes (never
    /// auto-expiring, per freedesktop urgency) get a `CRIMSON` summary
    /// instead of `BLACK` so they read as urgent at a glance.
    pub fn draw_note(&self, summary: &str, body: &str, is_critical: bool) -> Framebuffer {
        let bubble = crate::assets::bubble();
        let line_h = self.font.as_ref().map_or(14, |f| f.cell_h() + 2);

        // (line, bold) — summary lines bold, body lines regular.
        let mut lines: Vec<(String, bool)> = Vec::new();
        if let Some(font) = &self.font {
            let layout = pixel_fonts::TextLayout::new(
                font,
                0,
                0,
                NOTE_TEXT_MAX_W,
                pixel_fonts::LinePlacement::Uniform { line_h },
            );
            for (text, bold) in [(summary, true), (body, false)] {
                for para in text.split('\n').filter(|p| !p.is_empty()) {
                    if lines.len() >= NOTE_MAX_LINES {
                        break;
                    }
                    // Wrapping is O(input) and the body is unauthenticated
                    // bus input of unbounded length: feed the wrapper only
                    // what can still be shown. Every glyph is at least 1px
                    // wide, so a line NOTE_TEXT_MAX_W px wide holds at most
                    // NOTE_TEXT_MAX_W chars.
                    let budget = (NOTE_MAX_LINES - lines.len()) * NOTE_TEXT_MAX_W;
                    let capped = crate::notify::cap_chars(para, budget);
                    lines.extend(layout.wrap(capped).into_iter().map(|l| (l, bold)));
                }
            }
        }
        lines.truncate(NOTE_MAX_LINES);

        let text_w = lines
            .iter()
            .map(|(l, _)| self.text_width(l) as usize)
            .max()
            .unwrap_or(0);
        // min/max, not clamp(): clamp panics if min > max, and nothing ties
        // the baked bubble art's width to the text-cap constants — if the
        // art ever grows past the cap, its width wins as the floor instead
        // of panicking on every incoming notification.
        let w = (text_w + NOTE_PAD_LEFT + NOTE_PAD_RIGHT)
            .min(NOTE_TEXT_MAX_W + NOTE_PAD_LEFT + NOTE_PAD_RIGHT)
            .max(bubble.width);
        let h = (lines.len().max(1) * line_h + NOTE_PAD_TOP + NOTE_PAD_BOTTOM)
            .max(BUBBLE_CAP_TOP + BUBBLE_CAP_BOTTOM + 1);

        let mut fb = Framebuffer::new(w, h, pixel_graphics::TRANSPARENT);
        for dy in 0..h {
            let sy = pixel_graphics::stretch_source_coord(
                dy,
                h,
                bubble.height,
                BUBBLE_CAP_TOP,
                BUBBLE_CAP_BOTTOM,
            );
            for dx in 0..w {
                // Horizontal mirror: sample the flipped column so the tail
                // (bottom-left in the art) lands bottom-right on screen.
                let sx = bubble.width
                    - 1
                    - pixel_graphics::stretch_source_coord(
                        dx,
                        w,
                        bubble.width,
                        BUBBLE_CAP_X,
                        BUBBLE_CAP_X,
                    );
                let idx = bubble.at(sx, sy);
                if idx != pixel_graphics::TRANSPARENT {
                    fb.set_pixel(dx as isize, dy as isize, idx);
                }
            }
        }

        if let Some(font) = &self.font {
            let mut y = NOTE_PAD_TOP;
            for (line, bold) in &lines {
                // Bold lines are the summary; colour it CRIMSON on critical
                // notes so they read as urgent at a glance, BLACK otherwise.
                let color = if *bold && is_critical {
                    palette_color::CRIMSON
                } else {
                    palette_color::BLACK
                };
                font.draw_text(&mut fb, line, NOTE_PAD_LEFT as isize, y as isize, color);
                if *bold {
                    // Faux bold: restrike one pixel right.
                    font.draw_text(
                        &mut fb,
                        line,
                        (NOTE_PAD_LEFT + 1) as isize,
                        y as isize,
                        color,
                    );
                }
                y += line_h;
            }
        }
        fb
    }
}
