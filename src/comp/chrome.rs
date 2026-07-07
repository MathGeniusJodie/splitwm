//! Compositing the software-rendered chrome under the client windows.
//!
//! The ported `render::Renderer` draws wallpaper + leaf frames into one
//! palette-indexed full-output `Framebuffer` (the X11 version's underlay);
//! here it is presented into a `MemoryRenderBuffer` (smithay owns the
//! texture upload and damage) and rendered as the bottom element of every
//! frame. Nothing allocates per frame: the indexed framebuffer recycles
//! via take_screen_base/retire_frame, and the memory buffer is rewritten
//! only when the chrome is actually dirty.

use super::Comp;
use crate::render::{BtnIcon, LeafView, TitleInfo};
use crate::theme;
use crate::tree::Dir;
use crate::widgets::{leaf_meta, BtnKind, Placement};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::render_elements;
use smithay::utils::{Buffer as BufferCoords, Rectangle, Transform};

render_elements! {
    /// Everything one output frame is made of: client surfaces of every
    /// kind (tiled, floats, dock, layer, o-r) and the software-drawn
    /// chrome buffers (underlay, float frames, notes, cursor).
    pub OutputElement<=GlesRenderer>;
    Float=WaylandSurfaceRenderElement<GlesRenderer>,
    Chrome=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Borrows of everything `output_elements` composites, so `redraw` can
/// hand the scene to a backend while that backend is itself mutably
/// borrowed out of `Comp`.
pub struct Scene<'a> {
    pub or_windows: &'a [crate::comp::xwayland::OrWindow],
    pub note_popups: &'a [super::notifications::NotePopup],
    pub note_rects: &'a [(u32, crate::widgets::FrameRect)],
    pub float_stack: &'a [crate::tree::Win],
    pub managed: &'a crate::shell::Managed,
    pub space: &'a smithay::desktop::Space<smithay::desktop::Window>,
    pub output: &'a smithay::output::Output,
    pub dock_place: &'a Option<(smithay::desktop::Window, crate::tree::Rect)>,
    pub chrome_buf: &'a MemoryRenderBuffer,
}

/// Append render elements for every layer surface on `layer`, topmost
/// first (matching `elements`' front-to-back order).
fn layer_elements(
    renderer: &mut GlesRenderer,
    map: &smithay::desktop::LayerMap,
    layer: smithay::wayland::shell::wlr_layer::Layer,
    elements: &mut Vec<OutputElement>,
) {
    use smithay::backend::renderer::element::AsRenderElements as _;
    for l in map.layers_on(layer).rev() {
        let Some(geo) = map.layer_geometry(l) else {
            continue;
        };
        elements.extend(l.render_elements::<OutputElement>(
            renderer,
            geo.loc.to_physical(1),
            1.0.into(),
            1.0,
        ));
    }
}

/// One frame's render elements, front-to-back: Overlay layer surfaces
/// topmost, override-redirect X11 windows (rofi, menus), notification
/// bubbles, the Top layer, floats with their frame chrome, the
/// tiled/fullscreen Space, the dock, the Bottom layer, the chrome
/// underlay, the Background layer behind everything.
pub fn output_elements(renderer: &mut GlesRenderer, scene: &Scene<'_>) -> Vec<OutputElement> {
    use smithay::backend::renderer::element::AsRenderElements as _;
    use smithay::utils::{Logical, Point};
    use smithay::wayland::shell::wlr_layer::Layer;

    let layer_map = smithay::desktop::layer_map_for_output(scene.output);
    let mut elements: Vec<OutputElement> = Vec::new();
    layer_elements(renderer, &layer_map, Layer::Overlay, &mut elements);
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
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (f64::from(rect.x), f64::from(rect.y)),
            &p.buf,
            None,
            None,
            None,
            smithay::backend::renderer::element::Kind::Unspecified,
        ) {
            Ok(el) => elements.push(OutputElement::Chrome(el)),
            Err(err) => tracing::error!("note element: {err}"),
        }
    }
    layer_elements(renderer, &layer_map, Layer::Top, &mut elements);
    for &fw in scene.float_stack {
        let Some((window, f)) = scene.managed.float(fw) else {
            continue;
        };
        let loc = (Point::<i32, Logical>::from((f.x, f.y)) - window.geometry().loc).to_physical(1);
        elements.extend(window.render_elements::<OutputElement>(renderer, loc, 1.0.into(), 1.0));
        let rect = f.frame_rect();
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (f64::from(rect.x), f64::from(rect.y)),
            &f.frame_buf,
            None,
            None,
            None,
            smithay::backend::renderer::element::Kind::Unspecified,
        ) {
            Ok(el) => elements.push(OutputElement::Chrome(el)),
            Err(err) => tracing::error!("float frame element: {err}"),
        }
    }
    // The Space's windows in stacking order, via the region renderer —
    // NOT space_render_elements, which draws every layer surface itself
    // (in an order that buries Overlay under floats and puts Background
    // over the chrome underlay) and locks the LayerMap this function
    // already holds.
    if let Some(geo) = scene.space.output_geometry(scene.output) {
        elements.extend(
            scene
                .space
                .render_elements_for_region(renderer, &geo, 1.0, 1.0)
                .into_iter()
                .map(OutputElement::Float),
        );
    }
    if let Some((window, rect)) = scene.dock_place {
        let loc =
            (Point::<i32, Logical>::from((rect.x, rect.y)) - window.geometry().loc).to_physical(1);
        elements.extend(window.render_elements::<OutputElement>(renderer, loc, 1.0.into(), 1.0));
    }
    // Bottom layer surfaces (cozyui's native sidebar) sit above the
    // chrome underlay: the wallpaper is baked into that opaque buffer, so
    // "above the wallpaper, below the windows" can only mean above the
    // whole underlay. A zone-respecting panel overlaps no leaf frame —
    // the exclusive zone already shrank the layout — only the taskbar
    // rows in its column, exactly like the dock.
    layer_elements(renderer, &layer_map, Layer::Bottom, &mut elements);
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (0.0, 0.0),
        scene.chrome_buf,
        None,
        None,
        None,
        smithay::backend::renderer::element::Kind::Unspecified,
    ) {
        Ok(el) => elements.push(OutputElement::Chrome(el)),
        Err(err) => tracing::error!("chrome element: {err}"),
    }
    // The chrome underlay's wallpaper is this session's background layer;
    // a foreign Background surface (a wallpaper client) stacks behind the
    // opaque underlay, occluded, rather than being allowed to cover the
    // leaf frames and taskbar drawn into the same buffer.
    layer_elements(renderer, &layer_map, Layer::Background, &mut elements);
    elements
}

/// ease-out-back: slight overshoot past the target, then settle.
fn ease_out_back(t: f32) -> f32 {
    let c = 1.1_f32;
    let t = t - 1.0;
    let inner = (c + 1.0).mul_add(t, c);
    (t * t).mul_add(inner, 1.0)
}

fn lerp_rect(
    a: crate::widgets::FrameRect,
    b: crate::widgets::FrameRect,
    p: f32,
) -> crate::widgets::FrameRect {
    let l = |s: i32, e: i32| s + ((e - s) as f32 * p) as i32;
    crate::widgets::FrameRect {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        w: l(a.w, b.w).max(1),
        h: l(a.h, b.h).max(1),
    }
}

/// How long a layout transition takes, wall-clock.
const ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(280);

/// An in-flight layout animation, stepped by the redraw tick (~60 Hz).
/// Client windows are already at their final rects (placed by the arrange
/// that started this); only the composited chrome interpolates.
pub struct LayoutAnim {
    pub start: std::time::Instant,
    /// Each animated leaf's start rect paired with its target placement.
    pub placed: Vec<(crate::widgets::FrameRect, Placement)>,
}

impl Comp {
    /// Redraw the underlay for the current placements. Runs only when
    /// something chrome-visible changed (`chrome_dirty`), not per frame.
    pub fn compose_chrome(&mut self) {
        let placed = self.placed.clone();
        self.compose_frame(&placed, true);
    }

    /// Advance the in-flight layout animation by wall-clock time (called
    /// once per redraw tick). The final frame recomposes with widgets,
    /// matching what a non-animated arrange would have left.
    pub fn step_animation(&mut self) {
        let Some(anim) = &self.anim else {
            return;
        };
        let t = (anim.start.elapsed().as_secs_f32() / ANIM_DURATION.as_secs_f32()).min(1.0);
        if t >= 1.0 {
            self.finish_animation();
            return;
        }
        let e = ease_out_back(t);
        let interp: Vec<Placement> = anim
            .placed
            .iter()
            .map(|&(from, p)| Placement {
                target: lerp_rect(from, p.target, e),
                ..p
            })
            .collect();
        self.compose_frame(&interp, false);
    }

    /// Snap an in-flight animation to its end state (chrome with widgets).
    pub fn finish_animation(&mut self) {
        if self.anim.take().is_some() {
            self.compose_chrome();
        }
    }

    /// Composite the wallpaper, every placed leaf's chrome, the taskbar,
    /// and (unless mid-animation) the widgets, into the chrome buffer.
    fn compose_frame(&mut self, placed: &[Placement], widgets: bool) {
        let size = self.output_size();
        let (w, h) = (size.w.max(1), size.h.max(1));

        let mut fb = self.chrome.take_screen_base(w as u32, h as u32);
        for p in placed {
            let view = self.leaf_view(p);
            self.chrome
                .draw_leaf(&mut fb, p.target.x, p.target.y, &view);
            if p.focused {
                self.chrome
                    .draw_focus_outline(&mut fb, p.target.x, p.target.y, p.target.w, p.target.h);
            }
        }

        if widgets {
            // "+" insert buttons in the gaps and at the canvas edges.
            for (r, _) in &self.widgets.plus_regions {
                crate::render::draw_plus(&mut fb, r.x + r.w / 2, r.y + r.h / 2, r.w);
            }

            // Split-control buttons over each titlebar. A minimized leaf's
            // region is the whole frame (a single restore button); draw_leaf's
            // winmin art already shows it, so no glyph is drawn on top.
            for (r, leaf, kind) in &self.widgets.btn_regions {
                let Some(p) = placed.iter().find(|p| p.leaf == *leaf) else {
                    continue;
                };
                let meta = leaf_meta(
                    &self.state.tree,
                    self.parents.get(leaf).copied(),
                    *leaf,
                    p.target,
                );
                if meta.minimized {
                    continue;
                }
                let (icon, disabled) = match kind {
                    // A V-branch parent means this leaf collapses to a row
                    // (short/wide) when minimized, so its button previews that
                    // with the horizontal glyph.
                    BtnKind::Minimize => (
                        if meta.parent_dir == Some(Dir::V) {
                            BtnIcon::MinimizeH
                        } else {
                            BtnIcon::Minimize
                        },
                        meta.parent_dir.is_none(),
                    ),
                    BtnKind::Split => (
                        if meta.wider {
                            BtnIcon::VSplit
                        } else {
                            BtnIcon::HSplit
                        },
                        !meta.can_split,
                    ),
                    BtnKind::Close => (BtnIcon::Close, meta.parent_dir.is_none()),
                };
                self.chrome.draw_button(
                    &mut fb,
                    r.x + r.w / 2,
                    r.y + r.h / 2,
                    icon,
                    disabled,
                    crate::widgets::leaf_color_index(&self.state.tree, *leaf),
                );
            }
        }

        // Bottom bar: one tile per managed window; split-visible windows
        // get an accent highlight box, and every tile carries a corner
        // close badge. Icons arrive in M8; the class-initial glyph stands
        // in meanwhile.
        for t in &self.widgets.taskbar_regions {
            let label = self.managed.get(t.win).map_or('?', |w| {
                crate::widgets::label_from_class(&crate::shell::toplevel_app_id(w))
            });
            let icon = self.icon_for(t.win);
            self.chrome.draw_taskbar_item(
                &mut fb,
                t.rect,
                icon.as_deref(),
                label,
                t.accent,
                t.in_split,
            );
            crate::render::draw_close_badge(&mut fb, t.close.x, t.close.y, t.close.w);
        }
        if let Some(sep) = self.widgets.taskbar_sep {
            crate::render::draw_taskbar_sep(&mut fb, sep);
        }
        for &(r, i) in &self.widgets.quick_regions {
            let Some(q) = self.quick.get(i) else {
                continue;
            };
            self.chrome.draw_taskbar_item(
                &mut fb,
                r,
                q.icon.as_deref(),
                q.label,
                theme::palette_color::CREAM,
                false,
            );
        }

        if self.chrome_size != (w, h) {
            self.chrome_buf =
                MemoryRenderBuffer::new(Fourcc::Argb8888, (w, h), 1, Transform::Normal, None);
            self.chrome_size = (w, h);
        }
        let full: Rectangle<i32, BufferCoords> = Rectangle::from_size((w, h).into());
        let chrome = &self.chrome;
        self.chrome_buf
            .render()
            .draw(|buf| {
                chrome.present_into_slice(&fb, buf);
                Ok::<_, std::convert::Infallible>(vec![full])
            })
            .expect("present chrome into memory buffer");
        self.chrome.retire_frame(fb);
    }

    /// What a leaf's chrome shows: accent, title, minimized state. Icons
    /// arrive in M8 (`TitleInfo.icon` stays `None` until then).
    fn leaf_view(&self, p: &Placement) -> LeafView {
        let titlebar = p
            .active_client
            .and_then(|c| self.managed.get(c).map(|w| (c, w)))
            .map(|(c, window)| TitleInfo {
                label: crate::widgets::label_from_class(&crate::shell::toplevel_app_id(window)),
                icon: self.icon_for(c),
                title: crate::shell::toplevel_title(window),
            });
        LeafView {
            w: p.target.w,
            h: p.target.h,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: crate::widgets::leaf_color_index(&self.state.tree, p.leaf),
            titlebar,
            minimized: self.state.tree.leaf(p.leaf).is_some_and(|l| l.minimized),
            buttons: true,
        }
    }
}
