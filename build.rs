//! Bakes the chrome art PNGs (crate root, alongside Cargo.toml) into
//! palette-indexed binaries at compile time, so the binary embeds raw index
//! data instead of PNGs and does no decoding or quantization at startup.
//! The runtime `png` dependency remains only for user wallpapers, which are
//! arbitrary files decoded at runtime.
//!
//! Outputs in OUT_DIR:
//! - `palette.bin`: RGB triples of the na16 palette
//! - `<name>.bin`: row-major palette indices per sprite
//! - `baked_assets.rs`: accessors, included by `src/assets.rs`

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pixel_graphics::{Palette, Sprite};

const PALETTE_PNG: &str = "assets/na16-1x.png";

const SPRITES: &[&str] = &[
    "assets/bubble.png",
    "assets/close.png",
    "assets/cursor_disabled.png",
    "assets/cursor_hand.png",
    "assets/cursor_pointer.png",
    "assets/close_disabled.png",
    "assets/hsplit.png",
    "assets/hsplit_disabled.png",
    "assets/minimize.png",
    "assets/minimize_disabled.png",
    "assets/minimize_h.png",
    "assets/minimize_h_disabled.png",
    "assets/vsplit.png",
    "assets/vsplit_disabled.png",
    "assets/winborder.png",
    "assets/winmin.png",
    "assets/winmin_h.png",
];

fn main() {
    println!("cargo::rerun-if-changed={PALETTE_PNG}");
    for path in SPRITES {
        println!("cargo::rerun-if-changed={path}");
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let palette = Palette::load(PALETTE_PNG)
        .unwrap_or_else(|e| panic!("failed to load palette {PALETTE_PNG}: {e}"));
    let mut out = String::new();

    let palette_bytes: Vec<u8> = (0..palette.len())
        .flat_map(|i| {
            let c = palette.color(i as u8);
            [c.r, c.g, c.b]
        })
        .collect();
    std::fs::write(out_dir.join("palette.bin"), &palette_bytes).unwrap();
    out.push_str(
        "pub(crate) static PALETTE_BYTES: &[u8] = \
         include_bytes!(concat!(env!(\"OUT_DIR\"), \"/palette.bin\"));\n",
    );

    for path in SPRITES {
        bake_sprite(Path::new(path), &palette, &out_dir, &mut out);
    }

    std::fs::write(out_dir.join("baked_assets.rs"), out).unwrap();
}

fn bake_sprite(path: &Path, palette: &Palette, out_dir: &Path, out: &mut String) {
    let path_str = path.to_str().unwrap();
    let sprite = Sprite::load_native(path_str, palette)
        .unwrap_or_else(|e| panic!("failed to load sprite {path_str}: {e}"));
    let name = path.file_stem().unwrap().to_str().unwrap();

    let sprite = &sprite;
    let pixels: Vec<u8> = (0..sprite.height)
        .flat_map(|y| (0..sprite.width).map(move |x| sprite.at(x, y)))
        .collect();
    std::fs::write(out_dir.join(format!("{name}.bin")), &pixels).unwrap();

    writeln!(
        out,
        "pub(crate) fn {name}() -> Sprite {{ Sprite::from_indices({w}, {h}, \
         include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.bin\")).to_vec()) }}",
        w = sprite.width,
        h = sprite.height,
    )
    .unwrap();
}
