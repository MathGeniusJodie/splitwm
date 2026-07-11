//! Scene assembly: one frame's render elements, front-to-back.
//!
//! `output_elements` composites everything an output frame is made of:
//! client surfaces of every kind (tiled, floats, dock, layer, o-r), the
//! software-drawn chrome pieces cached in `comp::pieces`, and the focus
//! outline's GPU solid strips. Stacking within the ex-underlay's slot,
//! front-to-back: the focus outline, the taskbar (in front of the leaf
//! frames, as the old single buffer drew it last), the plus buttons, the
//! leaf frames (which never overlap, so their order is free), then the
//! opaque wallpaper at the back.

use super::pieces::{FrameArt, FrameMode, LeafFrame};
use super::Comp;
use crate::layout::NodeId;
use crate::render::indexed::{IndexedElement, IndexedProgram, IndexedTexture, NineSliceElement};
use crate::widgets::FrameRect;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::{CropRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::render_elements;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

render_elements! {
    /// Everything one output frame is made of: client surfaces of every
    /// kind (tiled, floats, dock, layer, o-r), the software-drawn chrome
    /// pieces (wallpaper, leaf frames, plus buttons, taskbar, float frames,
    /// notes, cursor) that the palette shader resolves straight from their
    /// indexed GPU textures, and the focused split's 2px focus outline as
    /// GPU solid strips.
    pub OutputElement<=GlesRenderer>;
    Float=WaylandSurfaceRenderElement<GlesRenderer>,
    Tiled=CropRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    Chrome=IndexedElement,
    Frame=NineSliceElement,
    Solid=smithay::backend::renderer::element::solid::SolidColorRenderElement,
}

/// Borrows of everything `output_elements` composites, so `redraw` can hand
/// the scene to a backend while that backend is itself mutably borrowed out
/// of `Comp`.
pub struct Scene<'a> {
    pub or_windows: &'a [super::xwayland::OrWindow],
    pub note_popups: &'a [super::notifications::NotePopup],
    pub note_rects: &'a [(u32, crate::widgets::FrameRect)],
    pub float_stack: &'a [crate::layout::Win],
    pub managed: &'a crate::shell::Managed,
    /// Every visible tiled/fullscreen window at the client rect it paints in
    /// this frame (interpolated mid-animation, settled otherwise), frontmost
    /// first. Replaces rendering straight from the `Space`, whose locations
    /// only know the settled layout.
    pub tiled: &'a [TiledPlace],
    pub output: &'a smithay::output::Output,
    pub dock_place: &'a Option<(smithay::desktop::Window, crate::layout::Rect)>,
    /// The dock layer surface's scrolled position (`Comp::layer_dock_place`);
    /// it renders there instead of where the `LayerMap` pinned it.
    pub layer_dock: &'a Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        smithay::utils::Point<i32, smithay::utils::Logical>,
    )>,
    pub indexed: &'a IndexedProgram,
    /// The full-output opaque wallpaper texture (bottom of the ex-underlay
    /// group). `None` only before the first `update_chrome_pieces`, which
    /// every redraw runs before building the scene.
    pub wallpaper: Option<&'a IndexedTexture>,
    /// Each placed leaf's frame draw data for this frame (rects interpolated
    /// mid-animation, settled otherwise): the shared border art sliced over
    /// the leaf rect by the GPU, plus its titlebar-contents strip texture.
    pub leaf_chrome: &'a [LeafFrame<'a>],
    /// The shared static frame art, for the float frames drawn inline in
    /// `output_elements` (leaves resolve theirs in `leaf_elements`). `None`
    /// only before the first `update_chrome_pieces`, like `wallpaper`.
    pub frame_art: Option<&'a FrameArt>,
    /// The "+" insert-button textures with the gap/edge origins they draw
    /// at; empty while a layout animation runs.
    pub plus: &'a [(Point<i32, Logical>, &'a IndexedTexture)],
    /// The taskbar strip texture and its top-left origin.
    pub taskbar: Option<(Point<i32, Logical>, &'a IndexedTexture)>,
    /// The focused split's 2px outline as four solid strips (empty when no
    /// leaf holds focus), stacked just over the leaf group so a focus change
    /// moves them without re-uploading any texture.
    pub focus_outline: &'a [smithay::backend::renderer::element::solid::SolidColorRenderElement],
}

/// One tiled window's draw placement for a frame: the client rect its
/// buffer paints in, anchored at the rect's origin and cropped to it. A
/// buffer smaller than the rect (a grower mid-slide, or the ease-out-back
/// overshoot carrying the rect past the final size) stretches to fill the
/// rect instead of leaving a background sliver; a larger one (a shrinker
/// whose configure is deferred to the animation's end) is only cropped,
/// never squashed.
pub struct TiledPlace {
    pub window: smithay::desktop::Window,
    pub rect: FrameRect,
}

/// Append render elements for every layer surface on `layer`, topmost
/// first (matching `elements`' front-to-back order). A surface matching
/// `override_loc` renders at that position instead of its map geometry
/// (the dock panel riding the canvas scroll).
fn layer_elements(
    renderer: &mut GlesRenderer,
    map: &smithay::desktop::LayerMap,
    layer: smithay::wayland::shell::wlr_layer::Layer,
    override_loc: &Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        smithay::utils::Point<i32, smithay::utils::Logical>,
    )>,
    elements: &mut Vec<OutputElement>,
) {
    use smithay::backend::renderer::element::AsRenderElements as _;
    for l in map.layers_on(layer).rev() {
        let loc = match override_loc {
            Some((s, p)) if s == l.wl_surface() => *p,
            _ => match map.layer_geometry(l) {
                Some(geo) => geo.loc,
                None => continue,
            },
        };
        elements.extend(l.render_elements::<OutputElement>(
            renderer,
            loc.to_physical(1),
            1.0.into(),
            1.0,
        ));
    }
}

/// A chrome element drawing `tex` at `loc` (output-relative, scale 1).
fn chrome_at(
    indexed: &IndexedProgram,
    tex: &IndexedTexture,
    loc: Point<i32, Logical>,
) -> OutputElement {
    OutputElement::Chrome(indexed.element(tex, loc.to_physical(1), Kind::Unspecified))
}

/// One frame's render elements, front-to-back: Overlay layer surfaces
/// topmost, override-redirect X11 windows (rofi, menus), notification
/// bubbles, the Top layer, floats with their frame chrome, the tiled and
/// fullscreen windows, the dock, the Bottom layer, then the ex-underlay
/// group — the focus outline, the taskbar, the plus buttons, the leaf
/// frames, and the opaque wallpaper — and the Background layer behind
/// everything.
pub fn output_elements(renderer: &mut GlesRenderer, scene: &Scene<'_>) -> Vec<OutputElement> {
    use smithay::backend::renderer::element::AsRenderElements as _;
    use smithay::utils::{Logical, Point};
    use smithay::wayland::shell::wlr_layer::Layer;

    let layer_map = smithay::desktop::layer_map_for_output(scene.output);
    let mut elements: Vec<OutputElement> = Vec::new();
    layer_elements(renderer, &layer_map, Layer::Overlay, &None, &mut elements);
    for or in scene.or_windows.iter().rev() {
        let Some(surface) = or.surface.wl_surface() else {
            continue;
        };
        let loc = or.rect.loc.to_physical(1);
        elements.extend(
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &surface,
                loc,
                1.0,
                1.0,
                smithay::backend::renderer::element::Kind::Unspecified,
            )
            .into_iter()
            .map(OutputElement::Float),
        );
    }
    // Notification bubbles above floats (master raised them so a focused
    // dialog never buries an incoming note).
    for (id, rect) in scene.note_rects {
        let Some(p) = scene.note_popups.iter().find(|p| p.id == *id) else {
            continue;
        };
        elements.push(chrome_at(
            scene.indexed,
            &p.tex,
            Point::<i32, Logical>::from((rect.x, rect.y)),
        ));
    }
    layer_elements(renderer, &layer_map, Layer::Top, &None, &mut elements);
    for &fw in scene.float_stack {
        let Some((window, f)) = scene.managed.float(fw) else {
            continue;
        };
        let loc = (Point::<i32, Logical>::from((f.x, f.y)) - window.geometry().loc).to_physical(1);
        elements.extend(window.render_elements::<OutputElement>(renderer, loc, 1.0.into(), 1.0));
        // The float's frame, like a leaf's: the titlebar strip texture in
        // front, the shared border art sliced over the frame rect behind.
        let rect = f.frame_rect();
        if let Some(tex) = f.frame.texture() {
            elements.push(chrome_at(
                scene.indexed,
                tex,
                Point::<i32, Logical>::from((rect.x, rect.y)),
            ));
        }
        if let Some(art) = scene.frame_art {
            let (btex, spec) = art.get(FrameMode::Border);
            let dst = Rectangle::new(
                Point::<i32, Logical>::from((rect.x, rect.y)).to_physical(1),
                Size::from((rect.w.max(1), rect.h.max(1))),
            );
            elements.push(OutputElement::Frame(scene.indexed.nine_slice_element(
                btex,
                f.frame_id.clone(),
                CommitCounter::default(),
                dst,
                spec,
                crate::render::ACCENT_SWAP_FROM,
                crate::render::accent_swap_to(f.accent),
            )));
        }
    }
    // Tiled and fullscreen windows at their per-frame client rects (the
    // Space still tracks them for input, but its locations only know the
    // settled layout). Each buffer anchors at its rect's origin, stretches
    // per-axis when the rect outgrows it, and crops to the rect — so a
    // mid-animation buffer at the wrong size never spills over the frame
    // borders or leaves a wallpaper sliver. Popups draw in front of their
    // window, uncropped and unscaled: menus legitimately overflow the leaf
    // rect (`Comp::unconstrained_popup_geometry` keeps them inside the
    // output instead), and as transient surfaces they skip the animation
    // stretch.
    for t in scene.tiled {
        use smithay::wayland::seat::WaylandFocus as _;
        let Some(surface) = t.window.wl_surface() else {
            continue;
        };
        let geo = t.window.geometry();
        let origin = Point::<i32, Logical>::from((t.rect.x, t.rect.y));
        let scale = Scale {
            x: (t.rect.w as f64 / geo.size.w.max(1) as f64).max(1.0),
            y: (t.rect.h as f64 / geo.size.h.max(1) as f64).max(1.0),
        };
        let crop = Rectangle::new(
            origin.to_physical(1),
            Size::from((t.rect.w.max(1), t.rect.h.max(1))),
        );
        for (popup, offset) in smithay::desktop::PopupManager::popups_for_surface(&surface) {
            let loc = (origin + offset - popup.geometry().loc).to_physical(1);
            elements.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    popup.wl_surface(),
                    loc,
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(OutputElement::Float),
            );
        }
        elements.extend(
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &surface,
                (origin - geo.loc).to_physical(1),
                1.0,
                1.0,
                Kind::Unspecified,
            )
            .into_iter()
            .filter_map(|e: WaylandSurfaceRenderElement<GlesRenderer>| {
                let scaled = RescaleRenderElement::from_element(e, origin.to_physical(1), scale);
                CropRenderElement::from_element(scaled, 1.0, crop)
            })
            .map(OutputElement::Tiled),
        );
    }
    if let Some((window, rect)) = scene.dock_place {
        let loc =
            (Point::<i32, Logical>::from((rect.x, rect.y)) - window.geometry().loc).to_physical(1);
        elements.extend(window.render_elements::<OutputElement>(renderer, loc, 1.0.into(), 1.0));
    }
    // Bottom layer surfaces (cozyui's native sidebar) sit above the chrome
    // pieces: the wallpaper is the opaque back of the group, so "above the
    // wallpaper, below the windows" can only mean above the whole group.
    // The dock panel among them rides the canvas scroll (`layer_dock`),
    // parked past its right end like the XWayland dock, so columns scrolled
    // over it cover it and scrolling right reveals it.
    layer_elements(
        renderer,
        &layer_map,
        Layer::Bottom,
        scene.layer_dock,
        &mut elements,
    );
    // The focus outline traces just inside the focused frame, over the leaf
    // frames but under every client window (already stacked above). Its own
    // solid elements move with the focused rect, so a focus switch never
    // re-uploads a texture.
    elements.extend(
        scene
            .focus_outline
            .iter()
            .cloned()
            .map(OutputElement::Solid),
    );
    // The taskbar draws in front of the leaf frames (the old single buffer
    // drew it last, so its pixels won any overlap with a leaf frame reaching
    // into the bottom strip).
    if let Some((loc, tex)) = scene.taskbar {
        elements.push(chrome_at(scene.indexed, tex, loc));
    }
    // Plus buttons sit in the gaps between frames; they never overlap a
    // frame, so their order relative to the leaf group is cosmetic.
    for (loc, tex) in scene.plus {
        elements.push(chrome_at(scene.indexed, tex, *loc));
    }
    // Leaf frames: non-overlapping, so relative order is free. Each is the
    // shared border art sliced over the leaf rect in the shader, with the
    // titlebar-contents strip (icon, title, baked buttons) in front of it.
    for f in scene.leaf_chrome {
        if let Some((loc, tex)) = &f.titlebar {
            elements.push(chrome_at(scene.indexed, tex, *loc));
        }
        let dst = Rectangle::new(
            Point::<i32, Logical>::from((f.dst.x, f.dst.y)).to_physical(1),
            Size::from((f.dst.w.max(1), f.dst.h.max(1))),
        );
        elements.push(OutputElement::Frame(scene.indexed.nine_slice_element(
            f.art,
            f.id.clone(),
            f.commit,
            dst,
            f.spec,
            crate::render::ACCENT_SWAP_FROM,
            crate::render::accent_swap_to(f.accent),
        )));
    }
    // The wallpaper is this session's opaque background; a foreign
    // Background surface (a wallpaper client) stacks behind it, occluded,
    // rather than being allowed to cover the leaf frames and taskbar.
    if let Some(tex) = scene.wallpaper {
        elements.push(chrome_at(
            scene.indexed,
            tex,
            Point::<i32, Logical>::from((0, 0)),
        ));
    }
    layer_elements(
        renderer,
        &layer_map,
        Layer::Background,
        &None,
        &mut elements,
    );
    elements
}

impl Comp {
    /// `tiled_places` at the *settled* rects (`view.placed` targets),
    /// ignoring any in-flight animation — for the input paths, which must
    /// see the layout the user is aiming at, not a mid-slide frame.
    pub fn settled_tiled_places(&self) -> Vec<TiledPlace> {
        let settled: Vec<_> = self
            .view
            .placed
            .iter()
            .map(|p| (p.leaf, p.target))
            .collect();
        self.tiled_places(&settled)
    }

    /// Every visible tiled window's draw placement for this frame: the
    /// fullscreen client over the whole output (frontmost), then each
    /// placed, unminimized leaf's client at the client rect inside the
    /// leaf's rect for this frame — interpolated mid-animation, settled
    /// otherwise (`leaf_rects` comes from `tick_layout`).
    pub fn tiled_places(&self, leaf_rects: &[(NodeId, FrameRect)]) -> Vec<TiledPlace> {
        let mut out = Vec::new();
        let fullscreen = self.fullscreen();
        if let Some(window) = fullscreen.and_then(|fs| self.managed.get(fs)) {
            let size = self.output_size();
            out.push(TiledPlace {
                window: window.clone(),
                rect: FrameRect {
                    x: 0,
                    y: 0,
                    w: size.w,
                    h: size.h,
                },
            });
        }
        for &(leaf, rect) in leaf_rects {
            let Some(l) = self.state.layout.leaf(leaf) else {
                continue;
            };
            let Some(c) = l.client else {
                continue;
            };
            if l.minimized || Some(c) == fullscreen {
                continue;
            }
            let Some(window) = self.managed.get(c) else {
                continue;
            };
            let (cx, cy, cw, ch) = crate::shell::client_rect_in_frame(rect, (1, 1));
            out.push(TiledPlace {
                window: window.clone(),
                rect: FrameRect {
                    x: cx,
                    y: cy,
                    w: cw,
                    h: ch,
                },
            });
        }
        out
    }
}
