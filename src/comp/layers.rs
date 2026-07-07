//! wlr-layer-shell: native panels, OSDs, and the launcher. Every layer
//! surface lands on the single output's `LayerMap`, which positions it and
//! tracks exclusive zones. Exclusive zones shrink the tiling area like a
//! panel's struts would have on X11; a Top/Overlay surface requesting
//! exclusive keyboard interactivity (rofi) holds the keyboard while
//! mapped. The taskbar strip is not zone-aware: it keeps the output's
//! bottom rows, so a bottom-anchored exclusive panel would overlap it.

use smithay::delegate_layer_shell;
use smithay::desktop::{layer_map_for_output, LayerSurface, WindowSurfaceType};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};

use super::Comp;

impl WlrLayerShellHandler for Comp {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
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
        {
            let mut map = layer_map_for_output(&self.output);
            if let Some(layer) = map
                .layer_for_surface(surface.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .cloned()
            {
                map.unmap_layer(&layer);
            }
        }
        self.sync_layer_zone();
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
        let zone = {
            let mut map = layer_map_for_output(&self.output);
            map.arrange();
            map.non_exclusive_zone()
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
        let initial_configure_sent =
            smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .expect("layer surface data on layer surface")
                    .lock()
                    .expect("no poisoned layer data")
                    .initial_configure_sent
            });
        if self.sync_layer_zone() {
            self.arrange();
        }
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        } else if self.exclusive_layer_surface().as_ref() == Some(surface) {
            // Layers commit per frame; only re-point the keyboard when
            // this surface is owed it and doesn't hold it yet.
            let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
            if keyboard.current_focus().as_ref() != Some(surface) {
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

    /// The topmost layer surface under `pos` across `layers` (checked
    /// front-to-back), with its global surface coordinates — the layer
    /// legs of `surface_under`.
    pub fn layer_surface_under(
        &self,
        layers: &[Layer],
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let map = layer_map_for_output(&self.output);
        layers.iter().find_map(|&l| {
            let layer = map.layer_under(l, pos)?;
            let loc = map.layer_geometry(layer)?.loc;
            layer
                .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
        })
    }
}
