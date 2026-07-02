//! Chrome art baked into the binary at build time (see build.rs):
//! palette-indexed sprites plus the na16 palette. No decoding or
//! quantization at startup.

use pixel_graphics::{Palette, Rgb, Sprite};

include!(concat!(env!("OUT_DIR"), "/baked_assets.rs"));

pub(crate) fn palette() -> Palette {
    Palette::from_colors(
        PALETTE_BYTES
            .chunks_exact(3)
            .map(|c| Rgb {
                r: c[0],
                g: c[1],
                b: c[2],
            })
            .collect(),
    )
}
