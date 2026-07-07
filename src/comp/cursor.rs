//! The compositor-drawn pointer. Master's hand-drawn `cursor_*` sprites
//! cover the shapes splitwm itself shows (arrow, hand, disabled, text);
//! every other shape a client can name through cursor-shape-v1 resolves
//! through the xcursor theme, falling back to the sprite arrow so the
//! pointer never vanishes. Images upload lazily, once per shape.
//!
//! Consumed by the backends that composite the cursor themselves: tty
//! always, winit only for client-committed cursor surfaces (named shapes
//! ride the host's hardware cursor there). Headless renders no cursor, so
//! harness screenshots stay pointer-free.

use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::utils::{IsAlive as _, Logical, Point, Transform};

use super::chrome::OutputElement;

/// One uploadable cursor image and its hotspot.
type CursorBuf = (MemoryRenderBuffer, Point<i32, Logical>);

pub struct CursorCache {
    /// `XCURSOR_SIZE`, for picking the closest theme image.
    size: i32,
    /// `XCURSOR_THEME` (its index is read once; icon files load on demand).
    theme: xcursor::CursorTheme,
    /// Shapes resolved so far; `None` records a miss so a shape absent
    /// from the theme isn't re-searched every frame.
    cache: HashMap<CursorIcon, Option<CursorBuf>>,
}

impl CursorCache {
    pub fn new() -> CursorCache {
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        CursorCache {
            size,
            theme: xcursor::CursorTheme::load(&theme),
            cache: HashMap::new(),
        }
    }

    /// The image for a named shape: the hand-drawn sprite where one
    /// exists, the xcursor theme otherwise, the sprite arrow as the last
    /// resort (`None` never happens in practice — the sprites are baked
    /// into the binary).
    pub fn get(&mut self, icon: CursorIcon) -> Option<&CursorBuf> {
        self.ensure(icon);
        if self.cache.get(&icon).expect("ensured above").is_some() {
            return self.cache.get(&icon).expect("ensured above").as_ref();
        }
        self.ensure(CursorIcon::Default);
        self.cache
            .get(&CursorIcon::Default)
            .expect("ensured above")
            .as_ref()
    }

    fn ensure(&mut self, icon: CursorIcon) {
        if !self.cache.contains_key(&icon) {
            let buf = sprite_buf(icon).or_else(|| self.theme_buf(icon));
            self.cache.insert(icon, buf);
        }
    }

    /// Load a shape from the xcursor theme, trying the CSS name first and
    /// the pre-CSS aliases after (modern themes symlink the CSS names;
    /// older ones only carry the classics).
    fn theme_buf(&self, icon: CursorIcon) -> Option<CursorBuf> {
        let path = std::iter::once(icon.name())
            .chain(legacy_names(icon).iter().copied())
            .find_map(|name| self.theme.load_icon(name))?;
        let data = std::fs::read(path).ok()?;
        let images = xcursor::parser::parse_xcursor(&data)?;
        let img = images
            .into_iter()
            .min_by_key(|i| (i.size as i32 - self.size).abs())?;
        let buffer = MemoryRenderBuffer::from_slice(
            &img.pixels_rgba,
            Fourcc::Abgr8888,
            (img.width as i32, img.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Some((buffer, (img.xhot as i32, img.yhot as i32).into()))
    }
}

/// The hand-drawn cursor art, palette-indexed like all chrome. Hotspots:
/// arrow tip, fingertip, circle center, I-beam center — matching master's
/// RENDER cursors.
fn sprite_buf(icon: CursorIcon) -> Option<CursorBuf> {
    let (sprite, hot) = match icon {
        CursorIcon::Default => (crate::assets::cursor_pointer(), Some((4, 0))),
        CursorIcon::Pointer => (crate::assets::cursor_hand(), Some((11, 0))),
        CursorIcon::NotAllowed => (crate::assets::cursor_disabled(), Some((12, 12))),
        CursorIcon::Text => (crate::assets::cursor_text(), None),
        _ => return None,
    };
    let hot = hot.unwrap_or((sprite.width as i32 / 2, sprite.height as i32 / 2));
    let palette = crate::assets::palette();
    let mut data = Vec::with_capacity(sprite.width * sprite.height * 4);
    for y in 0..sprite.height {
        for x in 0..sprite.width {
            let index = sprite.at(x, y);
            if index == pixel_graphics::TRANSPARENT {
                data.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let c = palette.color(index);
                data.extend_from_slice(&[c.r, c.g, c.b, 0xFF]);
            }
        }
    }
    let buffer = MemoryRenderBuffer::from_slice(
        &data,
        Fourcc::Abgr8888,
        (sprite.width as i32, sprite.height as i32),
        1,
        Transform::Normal,
        None,
    );
    Some((buffer, hot.into()))
}

/// Pre-CSS xcursor names for the shapes likely to be requested or shown.
fn legacy_names(icon: CursorIcon) -> &'static [&'static str] {
    match icon {
        CursorIcon::Default => &["left_ptr"],
        CursorIcon::Pointer => &["hand2", "hand1"],
        CursorIcon::Text => &["xterm"],
        CursorIcon::EwResize => &["sb_h_double_arrow", "h_double_arrow"],
        CursorIcon::NsResize => &["sb_v_double_arrow", "v_double_arrow"],
        CursorIcon::NeswResize => &["fd_double_arrow"],
        CursorIcon::NwseResize => &["bd_double_arrow"],
        CursorIcon::Crosshair => &["cross"],
        CursorIcon::Wait => &["watch"],
        CursorIcon::Progress => &["left_ptr_watch"],
        CursorIcon::Grab => &["openhand"],
        CursorIcon::Grabbing => &["closedhand"],
        CursorIcon::Move => &["fleur"],
        CursorIcon::NotAllowed => &["crossed_circle"],
        _ => &[],
    }
}

/// The composited cursor's render elements: the client's committed cursor
/// surface when one applies, else the named shape's image. Cursor-kind
/// elements let the DRM compositor place them on the hardware cursor
/// plane.
pub fn cursor_elements(
    renderer: &mut GlesRenderer,
    loc: Point<f64, Logical>,
    status: &CursorImageStatus,
    cache: &mut CursorCache,
) -> Vec<OutputElement> {
    let icon = match status {
        CursorImageStatus::Hidden => return Vec::new(),
        CursorImageStatus::Surface(surface) if surface.alive() => {
            let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map_or_else(Point::default, |data| data.lock().unwrap().hotspot)
            });
            return smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                surface,
                (loc.to_i32_round() - hotspot).to_physical(1),
                1.0,
                1.0,
                Kind::Cursor,
            )
            .into_iter()
            .map(OutputElement::Float)
            .collect();
        }
        // A dead cursor surface: back to the arrow.
        CursorImageStatus::Surface(_) => CursorIcon::Default,
        CursorImageStatus::Named(icon) => *icon,
    };
    let Some((buf, hotspot)) = cache.get(icon) else {
        return Vec::new();
    };
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (
            loc.x - f64::from(hotspot.x),
            loc.y - f64::from(hotspot.y),
        ),
        buf,
        None,
        None,
        None,
        Kind::Cursor,
    ) {
        Ok(el) => vec![OutputElement::Chrome(el)],
        Err(err) => {
            tracing::error!("cursor element: {err}");
            Vec::new()
        }
    }
}
