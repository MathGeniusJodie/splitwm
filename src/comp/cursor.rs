//! The compositor-drawn pointer. Every named shape resolves to one of
//! master's four hand-drawn `cursor_*` sprites — arrow, hand, disabled,
//! I-beam — grouped by intent, so cursor-shape-v1 requests always show
//! splitwm's own art. Images upload lazily, once per shape, as indexed
//! `R8` textures like all chrome. A client that commits its own cursor
//! pixels through `wl_pointer.set_cursor` (Inkscape's tool cursors;
//! XWayland forwards X11 cursors the same way) is rendered verbatim from
//! its surface tree instead.
//!
//! Consumed by the backends that composite the cursor themselves: tty and
//! winit both draw the sprite into the frame over a hidden host cursor.
//! Headless renders no cursor, so harness screenshots stay pointer-free.

use std::collections::HashMap;

use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::with_states;

use super::scene::OutputElement;
use crate::render::indexed::{IndexedProgram, IndexedTexture};

/// One uploaded cursor image and its hotspot.
type CursorBuf = (IndexedTexture, Point<i32, Logical>);

/// Named shapes uploaded so far, each keyed by the `CursorIcon` that named
/// it (icons sharing a sprite still upload once apiece — the cache is by
/// name, not by sprite).
pub struct CursorCache {
    cache: HashMap<CursorIcon, CursorBuf>,
}

impl CursorCache {
    pub fn new() -> CursorCache {
        CursorCache {
            cache: HashMap::new(),
        }
    }

    /// The image for a named shape, uploading it on first request. Every
    /// shape maps onto a baked sprite, so this always yields an image.
    pub fn get(
        &mut self,
        renderer: &mut GlesRenderer,
        indexed: &IndexedProgram,
        icon: CursorIcon,
    ) -> &CursorBuf {
        self.cache
            .entry(icon)
            .or_insert_with(|| sprite_buf(renderer, indexed, icon))
    }
}

/// The hand-drawn cursor art, palette-indexed like all chrome, keyed by
/// intent onto the four sprites: I-beam for text, hand for links and
/// drags, circle-slash for refusals, arrow for everything else. Hotspots:
/// arrow tip, fingertip, circle center, I-beam center — matching master's
/// RENDER cursors.
fn sprite_buf(
    renderer: &mut GlesRenderer,
    indexed: &IndexedProgram,
    icon: CursorIcon,
) -> CursorBuf {
    let (sprite, hot) = match icon {
        // Selectable text: the I-beam, hot-spotted at its center.
        CursorIcon::Text | CursorIcon::VerticalText => (crate::assets::cursor_text(), None),
        // Links, grabs, and drags: the pointing/grabbing hand.
        CursorIcon::Pointer
        | CursorIcon::Grab
        | CursorIcon::Grabbing
        | CursorIcon::Move
        | CursorIcon::AllScroll => (crate::assets::cursor_hand(), Some((11, 0))),
        // A refused action: the circle with a line through it.
        CursorIcon::NotAllowed | CursorIcon::NoDrop => {
            (crate::assets::cursor_disabled(), Some((12, 12)))
        }
        // The arrow covers the rest: the default, resize edges and corners,
        // wait/progress, crosshair, zoom, and the badge cursors.
        _ => (crate::assets::cursor_pointer(), Some((4, 0))),
    };
    let hot = hot.unwrap_or((sprite.width as i32 / 2, sprite.height as i32 / 2));
    let palette = crate::assets::palette();
    // The sprite's holes are TRANSPARENT-indexed; a fresh framebuffer starts
    // transparent and `draw_sprite` leaves those texels untouched, so the
    // indexed buffer carries the sprite's own transparency to the shader.
    let mut fb =
        pixel_graphics::Framebuffer::new(sprite.width, sprite.height, pixel_graphics::TRANSPARENT);
    fb.draw_sprite(&sprite, 0, 0, &palette);
    let mut tex = None;
    indexed.upload_owned(renderer, &mut tex, &fb, false);
    (tex.expect("cursor sprite uploaded"), hot.into())
}

/// The composited cursor's render elements: the named shape's sprite, a
/// client cursor surface's tree, or nothing when the pointer is hidden.
/// Cursor-kind elements let the DRM compositor place them on the hardware
/// cursor plane.
pub fn cursor_elements(
    renderer: &mut GlesRenderer,
    indexed: &IndexedProgram,
    loc: Point<f64, Logical>,
    status: &CursorImageStatus,
    cache: &mut CursorCache,
) -> Vec<OutputElement> {
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Named(icon) => {
            let (tex, hotspot) = cache.get(renderer, indexed, *icon);
            let loc = hotspot_origin(loc, *hotspot);
            vec![OutputElement::Chrome(indexed.element(
                tex,
                loc.to_physical(1),
                Kind::Cursor,
            ))]
        }
        CursorImageStatus::Surface(surface) => {
            // The hotspot rides on the surface as its cursor-role data,
            // updated by each set_cursor request.
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map_or_else(Point::default, |data| data.lock().unwrap().hotspot)
            });
            let loc = hotspot_origin(loc, hotspot);
            render_elements_from_surface_tree(
                renderer,
                surface,
                loc.to_physical(1),
                1.0,
                1.0,
                Kind::Cursor,
            )
        }
    }
}

/// The image's top-left corner: the pointer location pulled back by the
/// hotspot, snapped to the pixel grid.
fn hotspot_origin(loc: Point<f64, Logical>, hotspot: Point<i32, Logical>) -> Point<i32, Logical> {
    Point::from((
        (loc.x - f64::from(hotspot.x)).round() as i32,
        (loc.y - f64::from(hotspot.y)).round() as i32,
    ))
}
