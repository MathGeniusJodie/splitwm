mod menu;
mod render;
mod state;
mod theme;
mod tree;
mod wm;

/// A `pixel-graphics` palette index, threaded through as the accent-colour
/// representation for splits so border rendering can palette-swap them.
pub type Index = pixel_graphics::Index;

fn main() {
    if let Err(e) = wm::run() {
        eprintln!("splitwm: fatal: {e:?}");
        std::process::exit(1);
    }
}
