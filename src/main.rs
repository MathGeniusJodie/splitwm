mod assets;
mod icon;
mod menu;
mod notify;
mod oklch;
mod render;
mod state;
mod theme;
mod tree;
mod wm;

/// A `pixel-graphics` palette index, threaded through as the accent-colour
/// representation for splits so border rendering can palette-swap them.
pub type Index = pixel_graphics::Index;

fn main() {
    let replace = std::env::args().skip(1).any(|a| a == "--replace");
    if let Err(e) = wm::run(replace) {
        eprintln!("splitwm: fatal: {e:?}");
        std::process::exit(1);
    }
}
