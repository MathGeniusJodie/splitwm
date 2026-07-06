mod assets;
mod icon;
mod launch;
mod notify;
mod oklch;
mod ping;
mod render;
mod state;
mod theme;
mod tree;
mod wm;

/// A `pixel-graphics` palette index, threaded through as the accent-colour
/// representation for splits so border rendering can palette-swap them.
pub type Index = pixel_graphics::Index;

fn main() {
    let mut replace = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--replace" => replace = true,
            other => {
                eprintln!("splitwm: unknown argument '{other}'\nusage: splitwm [--replace]");
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = wm::run(replace) {
        eprintln!("splitwm: fatal: {e}");
        std::process::exit(1);
    }
}
