//! Bakes the chrome art PNGs (crate root, alongside Cargo.toml) into
//! palette-indexed binaries at compile time, so the binary embeds raw index
//! data instead of PNGs and does no decoding or quantization at startup.
//! Decoded via ImageMagick (`pixel_graphics::magick_decode_rgba`), the same
//! path `src/icon.rs` uses at runtime for user wallpapers and theme icons —
//! this crate has no Rust PNG decoder of its own.
//!
//! Outputs in OUT_DIR:
//! - `palette.bin`: RGB triples of the na16 palette
//! - `<name>.bin`: row-major palette indices per sprite
//! - `baked_assets.rs`: accessors, included by `src/assets.rs`

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pixel_graphics::{magick_decode_rgba, Palette, Sprite};

const PALETTE_PNG: &str = "assets/na16-1x.png";

/// Widest asset dimension worth keeping; these are known local assets, so
/// this is just a sanity bound, not a hostile-input defense.
const MAX_DIM: usize = 16_384;

fn main() {
    // A directory path makes cargo watch every file under it, so this one
    // line covers edits, additions, and deletions of any asset.
    println!("cargo::rerun-if-changed=assets");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let (_, _, palette_pixels) = magick_decode_rgba(PALETTE_PNG, MAX_DIM)
        .unwrap_or_else(|| panic!("failed to load palette {PALETTE_PNG}"));
    let palette = Palette::from_rgba(&palette_pixels);
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

    // Bake every PNG in assets/ (the palette image itself is the one
    // exception): the sprite list is the directory, so a dropped-in asset
    // can never be silently missing from the binary. Sorted so the
    // generated file is stable across filesystems.
    let mut sprites: Vec<PathBuf> = std::fs::read_dir("assets")
        .expect("assets/ directory must exist")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "png") && p != Path::new(PALETTE_PNG))
        .collect();
    sprites.sort();
    for path in &sprites {
        bake_sprite(path, &palette, &out_dir, &mut out);
    }

    std::fs::write(out_dir.join("baked_assets.rs"), out).unwrap();
}

fn bake_sprite(path: &Path, palette: &Palette, out_dir: &Path, out: &mut String) {
    let path_str = path.to_str().unwrap();
    let (width, height, pixels) = magick_decode_rgba(path_str, MAX_DIM)
        .unwrap_or_else(|| panic!("failed to load sprite {path_str}"));
    let sprite = Sprite::native_from_rgba(width, height, &pixels, palette);
    let name = path.file_stem().unwrap().to_str().unwrap();

    let sprite = &sprite;
    let pixels: Vec<u8> = (0..sprite.height)
        .flat_map(|y| (0..sprite.width).map(move |x| sprite.at(x, y)))
        .collect();
    std::fs::write(out_dir.join(format!("{name}.bin")), &pixels).unwrap();

    // dead_code allowed: every PNG in assets/ is baked (the list is
    // enumerated from the directory), including art not yet referenced.
    writeln!(
        out,
        "#[allow(dead_code)] pub(crate) fn {name}() -> Sprite {{ Sprite::from_indices({w}, {h}, \
         include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.bin\")).to_vec()) }}",
        w = sprite.width,
        h = sprite.height,
    )
    .unwrap();
}
