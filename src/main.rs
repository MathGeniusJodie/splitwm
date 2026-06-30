mod menu;
mod render;
mod state;
mod theme;
mod tree;
mod wm;

fn main() {
    if let Err(e) = wm::run() {
        eprintln!("splitwm: fatal: {e:?}");
        std::process::exit(1);
    }
}
