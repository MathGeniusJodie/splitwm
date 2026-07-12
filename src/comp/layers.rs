//! wlr-layer-shell: native panels, OSDs, and the launcher. Every layer
//! surface lands on the single output's `LayerMap`, which positions it and
//! tracks exclusive zones. Exclusive zones shrink the tiling area like a
//! panel's struts would have on X11; a Top/Overlay surface requesting
//! exclusive keyboard interactivity (rofi) holds the keyboard while
//! mapped. The taskbar strip is not zone-aware: it keeps the output's
//! bottom rows, so a bottom-anchored exclusive panel would overlap it.
//!
//! One layer surface is special: the dock panel (cozyui's native sidebar,
//! recognized by namespace) rides the scrolling canvas like the XWayland
//! dock instead of pinning to the screen edge — its exclusive zone turns
//! into scroll room past the canvas rather than shrinking the layout, and
//! its render/input position shifts with `scroll_x`.

use smithay::delegate_layer_shell;
use smithay::desktop::{layer_map_for_output, LayerMap, LayerSurface, WindowSurfaceType};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::shell::wlr_layer::{
    Anchor, ExclusiveZone, KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface,
    LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
};

use super::Comp;
use crate::theme;

/// The native dock panel in `map`, when mapped: the Bottom layer surface
/// whose namespace is the dock identity (the same identity `matches_dock`
/// checks), anchored full-height to the right edge — the only shape the
/// dock's scroll math below understands. Any other layer surface, dock-
/// named or not, keeps plain layer-shell semantics.
fn dock_layer<'a>(map: &'a LayerMap, identity: &str) -> Option<&'a LayerSurface> {
    map.layers_on(Layer::Bottom).find(|l| {
        l.namespace().eq_ignore_ascii_case(identity)
            && l.cached_state()
                .anchor
                .contains(Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT)
    })
}

/// The dock layer surface's committed exclusive zone, zero when it asks
/// for none.
fn dock_zone(layer: &LayerSurface) -> i32 {
    match layer.cached_state().exclusive_zone {
        ExclusiveZone::Exclusive(z) => z as i32,
        _ => 0,
    }
}

impl WlrLayerShellHandler for Comp {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.globals.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        _output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // One output by design (master had one X screen): whatever output
        // the client named, the surface lands on ours. The initial
        // configure goes out on the surface's first commit.
        let mut map = layer_map_for_output(&self.output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("map layer surface: {err}");
        }
    }

    fn new_popup(
        &mut self,
        _parent: WlrLayerSurface,
        popup: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        // Already tracked when the xdg popup role was created; a second
        // track is a harmless no-op error.
        let _ = self
            .popups
            .track_popup(smithay::desktop::PopupKind::Xdg(popup));
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let identity = theme::dock_identity();
        let mut was_dock = false;
        {
            let mut map = layer_map_for_output(&self.output);
            if let Some(layer) = map
                .layer_for_surface(surface.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .cloned()
            {
                was_dock = dock_layer(&map, &identity)
                    .is_some_and(|d| d.wl_surface() == layer.wl_surface());
                map.unmap_layer(&layer);
            }
        }
        self.sync_layer_zone();
        if was_dock {
            // Re-clamp now that the scroll headroom it needed is gone.
            self.reclamp_scroll();
        }
        // arrange refocuses: the keyboard held by an exclusive layer
        // (rofi) must return to the layout when it goes.
        self.arrange();
    }
}
delegate_layer_shell!(Comp);

impl Comp {
    /// Re-arrange the layer map and refresh the cached non-exclusive
    /// zone. Returns `true` when the zone changed — the tiling layout must
    /// re-arrange then.
    pub fn sync_layer_zone(&mut self) -> bool {
        let identity = theme::dock_identity();
        let zone = {
            let mut map = layer_map_for_output(&self.output);
            map.arrange();
            let mut zone = map.non_exclusive_zone();
            // The dock panel's exclusive zone becomes scroll room past the
            // canvas (`dock_extra`) instead of a static layout shrink, so
            // columns can slide over it: add back exactly what `arrange`
            // subtracts for a full-height right-anchored surface.
            if let Some(layer) = dock_layer(&map, &identity) {
                let state = layer.cached_state();
                if matches!(state.exclusive_zone, ExclusiveZone::Exclusive(_)) {
                    zone.size.w += dock_zone(layer) + state.margin.right;
                }
            }
            zone
        };
        if zone == self.layer_zone {
            return false;
        }
        self.layer_zone = zone;
        true
    }

    /// A commit on a mapped layer surface: re-arrange the map, send the
    /// initial configure, relayout if the exclusive zone moved, and hand
    /// an exclusive-keyboard surface the keyboard the moment it maps.
    /// Returns `false` when `surface` is no layer surface.
    pub fn layer_commit(&mut self, surface: &WlSurface) -> bool {
        let Some(layer) = layer_map_for_output(&self.output)
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned()
        else {
            return false;
        };
        let initial_configure_sent = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .expect("layer surface data on layer surface")
                .lock()
                .expect("no poisoned layer data")
                .initial_configure_sent
        });
        // The dock layer's zone add-back cancels out of the cached zone, so
        // its mapping/resizing never flips `sync_layer_zone` — compare the
        // scroll room it wants against what the canvas last granted too.
        if self.sync_layer_zone() || self.dock_extra() != self.state.dock_extra() {
            self.arrange();
        }
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        } else if self.exclusive_layer_surface().as_ref() == Some(surface) {
            // Layers commit per frame; only re-point the keyboard when
            // this surface is owed it and doesn't hold it yet.
            use smithay::wayland::seat::WaylandFocus as _;
            let holds_it = self
                .keyboard
                .current_focus()
                .and_then(|t| t.wl_surface().map(std::borrow::Cow::into_owned))
                .is_some_and(|s| s == *surface);
            if !holds_it {
                self.refocus();
            }
        }
        true
    }

    /// The layer surface currently owed exclusive keyboard focus: the
    /// topmost Top/Overlay surface that requested Exclusive interactivity
    /// (the protocol's lock-screen/launcher semantics). Bottom/Background
    /// exclusivity keeps normal focus semantics per spec.
    pub fn exclusive_layer_surface(&self) -> Option<WlSurface> {
        let map = layer_map_for_output(&self.output);
        let pick = |l: Layer| {
            map.layers_on(l)
                .rev()
                .find(|s| {
                    s.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                })
                .map(|s| s.wl_surface().clone())
        };
        pick(Layer::Overlay).or_else(|| pick(Layer::Top))
    }

    /// The extra scroll room the native dock panel wants: its exclusive
    /// zone, the strip that must be fully revealable past the canvas end.
    /// Zero when no dock layer surface is mapped (or an XWayland dock
    /// window supersedes it — `dock_extra` checks that first).
    pub fn layer_dock_extra(&self) -> i32 {
        let identity = theme::dock_identity();
        let map = layer_map_for_output(&self.output);
        dock_layer(&map, &identity).map_or(0, dock_zone)
    }

    /// Where the dock layer surface sits this frame: parked at the right
    /// end of the scrolling canvas and shifted by the current scroll,
    /// exactly like the XWayland dock's `dock_geometry`. The strip past
    /// its exclusive zone stays tucked under the canvas edge when fully
    /// revealed; scrolling left carries the whole panel offscreen.
    pub fn layer_dock_place(&self) -> Option<(WlSurface, Point<i32, Logical>)> {
        let identity = theme::dock_identity();
        let (surface, geo, zone) = {
            let map = layer_map_for_output(&self.output);
            let layer = dock_layer(&map, &identity)?;
            let geo = map.layer_geometry(layer)?;
            (layer.wl_surface().clone(), geo, dock_zone(layer))
        };
        let wa = self.layout_area();
        let canvas_w = self.state.canvas_w(wa);
        let overlap = (geo.size.w - zone).max(0);
        let x = wa.x + canvas_w - overlap - self.state.scroll_x();
        Some((surface, Point::from((x, geo.loc.y))))
    }

    /// The topmost layer surface under `pos` across `layers` (checked
    /// front-to-back), with its global surface coordinates — the layer
    /// legs of `surface_under`. The dock layer surface hit-tests at its
    /// scrolled position, not where the `LayerMap` pinned it.
    pub fn layer_surface_under(
        &self,
        layers: &[Layer],
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let dock = self.layer_dock_place();
        let map = layer_map_for_output(&self.output);
        layers.iter().find_map(|&l| {
            map.layers_on(l).rev().find_map(|layer| {
                let loc = match &dock {
                    Some((s, p)) if s == layer.wl_surface() => *p,
                    _ => map.layer_geometry(layer)?.loc,
                };
                layer
                    .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
            })
        })
    }
}
