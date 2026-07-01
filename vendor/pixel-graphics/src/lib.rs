use std::error::Error;
use std::fs::File;
use std::io::BufReader;

/// A palette index stored in sprite pixels. `TRANSPARENT` is the only
/// non-palette value; palettes never contain transparent colors.
pub type Index = u8;
pub const TRANSPARENT: Index = 0xFF;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl From<Rgb> for Rgba {
    fn from(color: Rgb) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
            a: 255,
        }
    }
}

impl Rgba {
    const fn rgb(self) -> Rgb {
        Rgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }
}

/// What a palette slot resolves to when painted. Checkerboards are valid
/// anywhere a solid color is; their phase is anchored to destination
/// coordinates in fat-pixel units so overlapping dithers mesh.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Paint {
    Solid(Index),
    Checker(Index, Index),
    Transparent,
}

impl Paint {
    const fn pick(self, cell_x: usize, cell_y: usize) -> Option<Index> {
        match self {
            Self::Solid(index) => Some(index),
            Self::Checker(even, odd) => Some(if (cell_x + cell_y).is_multiple_of(2) {
                even
            } else {
                odd
            }),
            Self::Transparent => None,
        }
    }
}

/// Per-draw index remap. Indices without an explicit entry pass through.
pub struct Swap {
    paints: Vec<Paint>,
    uniform: Option<Paint>,
}

impl Swap {
    pub const fn identity() -> Self {
        Self {
            paints: Vec::new(),
            uniform: None,
        }
    }

    /// Every opaque pixel becomes `paint` (silhouettes, shadows, tints).
    pub const fn uniform(paint: Paint) -> Self {
        Self {
            paints: Vec::new(),
            uniform: Some(paint),
        }
    }

    pub fn from_indices(indices: &[Index]) -> Self {
        Self {
            paints: indices.iter().map(|&index| Paint::Solid(index)).collect(),
            uniform: None,
        }
    }

    #[allow(dead_code)]
    pub fn set(mut self, index: Index, paint: Paint) -> Self {
        let slot = index as usize;
        if self.paints.len() <= slot {
            let len = self.paints.len() as Index;
            self.paints.extend((len..=index).map(Paint::Solid));
        }
        self.paints[slot] = paint;
        self
    }

    fn paint(&self, index: Index) -> Paint {
        if index == TRANSPARENT {
            return Paint::Transparent;
        }
        if let Some(uniform) = self.uniform {
            return uniform;
        }
        self.paints
            .get(index as usize)
            .copied()
            .unwrap_or(Paint::Solid(index))
    }
}

/// Precomputed `Index` -> output BGRA table for the present pass.
pub type PresentLut = [[u8; Framebuffer::BYTES_PER_PIXEL]; 256];

pub struct Palette {
    colors: Vec<Rgb>,
}

impl Palette {
    pub fn load(path: &str) -> Result<Self, Box<dyn Error>> {
        Self::from_pixels(decode_png(path)?, path)
    }

    /// As `load`, but for a PNG already loaded into memory (e.g.
    /// `include_bytes!`), so callers can embed the palette in the binary.
    pub fn load_bytes(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        Self::from_pixels(decode_png_bytes(bytes)?.2, "<embedded bytes>")
    }

    fn from_pixels(pixels: Vec<Rgba>, label: &str) -> Result<Self, Box<dyn Error>> {
        let colors = pixels.into_iter().map(Rgba::rgb).collect::<Vec<_>>();
        if colors.is_empty() {
            return Err(format!("palette PNG has no colors: {label}").into());
        }
        Ok(Self::from_colors(colors))
    }

    pub const fn from_colors(colors: Vec<Rgb>) -> Self {
        Self { colors }
    }

    #[allow(dead_code)]
    pub const fn len(&self) -> usize {
        self.colors.len()
    }

    pub fn color(&self, index: Index) -> Rgb {
        self.colors[Self::wrap(index as usize, self.colors.len())]
    }

    /// Like `index % len` but branch-only for in-range indices; `%` is an
    /// integer division and this runs per pixel in the raster loops.
    const fn wrap(index: usize, len: usize) -> usize {
        if index < len { index } else { index % len }
    }

    /// Resolve an index to a final *palette index*, deferring the
    /// index->color step to present time. This is what the indexed
    /// framebuffer stores. Indices pass through literally — the only theme
    /// layer is the background, which is painted with an explicit `Paint`
    /// at draw time rather than by hijacking palette slots.
    #[allow(clippy::unused_self)]
    pub const fn resolve_index(
        &self,
        index: Index,
        _cell_x: usize,
        _cell_y: usize,
    ) -> Option<Index> {
        if index == TRANSPARENT {
            return None;
        }
        Some(index)
    }

    /// Per-draw paint -> a final palette index (checker pick applied).
    #[allow(clippy::unused_self)]
    pub const fn resolve_paint_index(
        &self,
        paint: Paint,
        cell_x: usize,
        cell_y: usize,
    ) -> Option<Index> {
        paint.pick(cell_x, cell_y)
    }

    /// Precompute a 256-entry index -> output BGRA table for the present pass.
    /// `TRANSPARENT` and out-of-range indices collapse to opaque black; the
    /// root framebuffer is cleared to an opaque index so they don't occur in
    /// practice.
    pub fn present_lut(&self) -> Box<PresentLut> {
        let mut lut = Box::new([[0u8, 0, 0, 0xFF]; 256]);
        for (i, entry) in lut.iter_mut().enumerate() {
            if i as Index != TRANSPARENT {
                *entry = Framebuffer::color_bytes(self.color(i as Index).into());
            }
        }
        lut
    }

    pub fn nearest_index(&self, color: Rgb) -> Index {
        self.colors
            .iter()
            .enumerate()
            .min_by_key(|(_, candidate)| color_distance(**candidate, color))
            .map_or(0, |(index, _)| index as Index)
    }

    pub fn exact_index(&self, color: Rgb) -> Option<Index> {
        self.colors
            .iter()
            .position(|candidate| *candidate == color)
            .map(|index| index as Index)
    }

    pub fn closest_to_white_index(&self) -> Index {
        self.nearest_index(Rgb {
            r: 255,
            g: 255,
            b: 255,
        })
    }

    /// index-in-self -> nearest index-in-other; basis for cross-palette
    /// sprite import and palette migration.
    pub fn mapping_to(&self, other: &Self) -> Vec<Index> {
        self.colors
            .iter()
            .map(|&color| other.nearest_index(color))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, w: usize, h: usize) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(self, x: i16, y: i16) -> bool {
        let x = x.max(0) as usize;
        let y = y.max(0) as usize;
        self.contains_point(x, y)
    }

    pub const fn contains_point(self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub const fn local(self, x: i16, y: i16) -> (i16, i16) {
        (x - self.x as i16, y - self.y as i16)
    }
}

/// Indexed pixel art: one palette index per pixel, `TRANSPARENT` for holes.
pub struct Sprite {
    pub width: usize,
    pub height: usize,
    pixels: Vec<Index>,
}

impl Sprite {
    /// Decode a PNG whose colors are interpreted via `source`, storing
    /// indices in `target`'s space.
    pub fn load(
        path: &str,
        source: &Palette,
        target: &Palette,
    ) -> Result<Self, Box<dyn Error>> {
        let (width, height, pixels) = decode_png_with_size(path)?;
        Self::from_pixels(width, height, pixels, source, target, path)
    }

    pub fn load_native(path: &str, palette: &Palette) -> Result<Self, Box<dyn Error>> {
        Self::load(path, palette, palette)
    }

    /// As `load`, but for a PNG already loaded into memory (e.g.
    /// `include_bytes!`), so callers can embed art in the binary.
    pub fn load_bytes(
        bytes: &[u8],
        source: &Palette,
        target: &Palette,
    ) -> Result<Self, Box<dyn Error>> {
        let (width, height, pixels) = decode_png_bytes(bytes)?;
        Self::from_pixels(width, height, pixels, source, target, "<embedded bytes>")
    }

    pub fn load_native_bytes(bytes: &[u8], palette: &Palette) -> Result<Self, Box<dyn Error>> {
        Self::load_bytes(bytes, palette, palette)
    }

    fn from_pixels(
        width: usize,
        height: usize,
        pixels: Vec<Rgba>,
        source: &Palette,
        target: &Palette,
        label: &str,
    ) -> Result<Self, Box<dyn Error>> {
        if pixels.len() != width * height {
            return Err(format!(
                "PNG pixel count mismatch for {label}: got {}, expected {}",
                pixels.len(),
                width * height
            )
            .into());
        }
        let lut = source.mapping_to(target);
        let pixels = pixels
            .into_iter()
            .map(|color| {
                if color.a == 0 {
                    return TRANSPARENT;
                }
                let source_index = source
                    .exact_index(color.rgb())
                    .unwrap_or_else(|| source.nearest_index(color.rgb()));
                lut[source_index as usize]
            })
            .collect();
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn at(&self, x: usize, y: usize) -> Index {
        self.pixels[y * self.width + x]
    }

    pub fn is_opaque(&self, x: usize, y: usize) -> bool {
        self.at(x, y) != TRANSPARENT
    }

    #[allow(dead_code)]
    pub fn region(&self, src: Rect) -> Self {
        let w = src.w.min(self.width.saturating_sub(src.x));
        let h = src.h.min(self.height.saturating_sub(src.y));
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                pixels.push(self.at(src.x + x, src.y + y));
            }
        }
        Self {
            width: w,
            height: h,
            pixels,
        }
    }

    #[allow(dead_code)]
    pub fn flip_h(&self) -> Self {
        self.map_coords(|x, y| (self.width - 1 - x, y), self.width, self.height)
    }

    #[allow(dead_code)]
    pub fn flip_v(&self) -> Self {
        self.map_coords(|x, y| (x, self.height - 1 - y), self.width, self.height)
    }

    /// Rotate 90 degrees clockwise.
    #[allow(dead_code)]
    pub fn rot90(&self) -> Self {
        self.map_coords(|x, y| (y, self.height - 1 - x), self.height, self.width)
    }

    fn map_coords(
        &self,
        source: impl Fn(usize, usize) -> (usize, usize),
        width: usize,
        height: usize,
    ) -> Self {
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let (sx, sy) = source(x, y);
                pixels.push(self.at(sx, sy));
            }
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Keep pixels only where `mask` is opaque.
    #[allow(dead_code)]
    pub fn mask(&self, mask: &Self) -> Self {
        let mut pixels = self.pixels.clone();
        for y in 0..self.height {
            for x in 0..self.width {
                let masked = x >= mask.width || y >= mask.height || !mask.is_opaque(x, y);
                if masked {
                    pixels[y * self.width + x] = TRANSPARENT;
                }
            }
        }
        Self {
            width: self.width,
            height: self.height,
            pixels,
        }
    }

    /// Remap every pixel index from one palette's space to another's.
    #[allow(dead_code)]
    pub fn convert(&self, from: &Palette, to: &Palette) -> Self {
        let lut = from.mapping_to(to);
        let pixels = self
            .pixels
            .iter()
            .map(|&index| {
                if index == TRANSPARENT {
                    TRANSPARENT
                } else {
                    lut[index as usize % lut.len()]
                }
            })
            .collect();
        Self {
            width: self.width,
            height: self.height,
            pixels,
        }
    }
}

/// Indexed render target: one palette index per pixel, `TRANSPARENT` for
/// see-through pixels. The output BGRA format is produced once per frame by
/// `present_into`, fully decoupling rendering from the framebuffer layout.
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pixels: Vec<Index>,
}

impl Framebuffer {
    /// Output bytes per pixel — only relevant at present time.
    pub const BYTES_PER_PIXEL: usize = 4;

    pub fn new(width: usize, height: usize, fill: Index) -> Self {
        Self {
            width,
            height,
            pixels: vec![fill; width * height],
        }
    }

    const fn color_bytes(color: Rgba) -> [u8; Self::BYTES_PER_PIXEL] {
        [color.b, color.g, color.r, color.a]
    }

    const fn pixel_offset(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    fn row_indices(&self, y: usize, x: usize, width: usize) -> &[Index] {
        let start = self.pixel_offset(x, y);
        &self.pixels[start..start + width]
    }

    fn row_indices_mut(&mut self, y: usize, x: usize, width: usize) -> &mut [Index] {
        let start = self.pixel_offset(x, y);
        &mut self.pixels[start..start + width]
    }

    /// Convert the whole framebuffer to output BGRA bytes via `lut` — the only
    /// place the index->color step happens, once per frame.
    pub fn present_into(&self, out: &mut [u8], lut: &[[u8; Self::BYTES_PER_PIXEL]; 256]) {
        for (pixel, &index) in out
            .chunks_exact_mut(Self::BYTES_PER_PIXEL)
            .zip(self.pixels.iter())
        {
            pixel.copy_from_slice(&lut[index as usize]);
        }
    }

    /// Convert a sub-rectangle to output BGRA bytes, row by row, into `out`
    /// (tightly packed at `rect.w` pixels per row).
    pub fn present_rect_into(
        &self,
        rect: Rect,
        out: &mut [u8],
        lut: &[[u8; Self::BYTES_PER_PIXEL]; 256],
    ) {
        for y in 0..rect.h {
            let src = self.row_indices(rect.y + y, rect.x, rect.w);
            let dst_start = y * rect.w * Self::BYTES_PER_PIXEL;
            let dst = &mut out[dst_start..dst_start + rect.w * Self::BYTES_PER_PIXEL];
            for (pixel, &index) in dst.chunks_exact_mut(Self::BYTES_PER_PIXEL).zip(src) {
                pixel.copy_from_slice(&lut[index as usize]);
            }
        }
    }

    pub fn clear(&mut self, fill: Index) {
        self.pixels.fill(fill);
    }

    /// Fill the framebuffer from `sprite`, clamping source coords at the
    /// sprite's edges so pixels past its extent repeat the border row/column.
    /// Transparent pixels leave the existing framebuffer content intact.
    pub fn fill_from_sprite(&mut self, sprite: &Sprite, palette: &Palette) {
        let sx_map: Vec<usize> = (0..self.width).map(|x| x.min(sprite.width - 1)).collect();
        let mut row = vec![TRANSPARENT; self.width];
        for y in 0..self.height {
            let sy = y.min(sprite.height - 1);
            let mut opaque_row = true;
            for (x, &sx) in sx_map.iter().enumerate() {
                match palette.resolve_index(sprite.at(sx, sy), sx, sy) {
                    Some(index) => row[x] = index,
                    None => opaque_row = false,
                }
            }
            if opaque_row {
                self.row_indices_mut(y, 0, self.width).copy_from_slice(&row);
            } else {
                for (x, &sx) in sx_map.iter().enumerate() {
                    if let Some(index) = palette.resolve_index(sprite.at(sx, sy), sx, sy) {
                        self.set_pixel(x, y, index);
                    }
                }
            }
        }
    }

    /// Write a single pixel index (bounds-checked). Cheaper than a 1x1
    /// `fill_rect` in per-pixel loops.
    pub fn set_pixel(&mut self, x: usize, y: usize, index: Index) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = self.pixel_offset(x, y);
        self.pixels[offset] = index;
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, index: Index) {
        if x >= self.width || y >= self.height {
            return;
        }

        let width = w.min(self.width - x);
        let height = h.min(self.height - y);
        for py in y..y + height {
            self.row_indices_mut(py, x, width).fill(index);
        }
    }

    /// Fill a rect with `paint`, so checkerboards land with their phase
    /// anchored to framebuffer coords.
    pub fn fill_rect_paint(&mut self, x: usize, y: usize, w: usize, h: usize, paint: Paint) {
        if x >= self.width || y >= self.height {
            return;
        }
        let width = w.min(self.width - x);
        let height = h.min(self.height - y);

        // Match the variant once instead of per pixel; each arm then has a
        // tight inner loop over row slices.
        match paint {
            Paint::Solid(index) => self.fill_rect(x, y, w, h, index),
            Paint::Checker(even, odd) => {
                for py in y..y + height {
                    let row = self.row_indices_mut(py, x, width);
                    for (i, slot) in row.iter_mut().enumerate() {
                        let px = x + i;
                        *slot = if (px + py).is_multiple_of(2) {
                            even
                        } else {
                            odd
                        };
                    }
                }
            }
            Paint::Transparent => {}
        }
    }

    pub fn draw_sprite(
        &mut self,
        sprite: &Sprite,
        dest_x: isize,
        dest_y: isize,
        palette: &Palette,
    ) {
        self.draw_sprite_full(
            sprite,
            Rect::new(0, 0, sprite.width, sprite.height),
            dest_x,
            dest_y,
            None,
            palette,
            None,
        );
    }

    pub fn draw_sprite_swapped(
        &mut self,
        sprite: &Sprite,
        dest_x: isize,
        dest_y: isize,
        palette: &Palette,
        swap: &Swap,
    ) {
        self.draw_sprite_full(
            sprite,
            Rect::new(0, 0, sprite.width, sprite.height),
            dest_x,
            dest_y,
            None,
            palette,
            Some(swap),
        );
    }

    /// Every opaque pixel painted as `paint`: shadows, silhouettes.
    pub fn draw_sprite_silhouette(
        &mut self,
        sprite: &Sprite,
        dest_x: isize,
        dest_y: isize,
        palette: &Palette,
        paint: Paint,
    ) {
        self.draw_sprite_swapped(sprite, dest_x, dest_y, palette, &Swap::uniform(paint));
    }

    pub fn draw_sprite_region(
        &mut self,
        sprite: &Sprite,
        src: Rect,
        dest_x: isize,
        dest_y: isize,
        palette: &Palette,
    ) {
        self.draw_sprite_full(sprite, src, dest_x, dest_y, None, palette, None);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_sprite_full(
        &mut self,
        sprite: &Sprite,
        src: Rect,
        dest_x: isize,
        dest_y: isize,
        clip: Option<Rect>,
        palette: &Palette,
        swap: Option<&Swap>,
    ) {
        let width = src.w.min(sprite.width.saturating_sub(src.x));
        let height = src.h.min(sprite.height.saturating_sub(src.y));

        // Unswapped, unclipped draws write rows directly instead of going
        // through set_pixel per pixel — the common case for full-size sprite blits.
        if swap.is_none() && clip.is_none() && dest_x >= 0 && dest_y >= 0 {
            let dest_x = dest_x as usize;
            let dest_y = dest_y as usize;
            if dest_x >= self.width || dest_y >= self.height {
                return;
            }
            let copy_w = width.min(self.width - dest_x);
            let copy_h = height.min(self.height - dest_y);
            for y in 0..copy_h {
                let row = self.row_indices_mut(dest_y + y, dest_x, copy_w);
                for (x, slot) in row.iter_mut().enumerate() {
                    let index = sprite.at(src.x + x, src.y + y);
                    let Some(resolved) = palette.resolve_index(index, dest_x + x, dest_y + y)
                    else {
                        continue;
                    };
                    *slot = resolved;
                }
            }
            return;
        }

        for y in 0..height {
            for x in 0..width {
                let index = sprite.at(src.x + x, src.y + y);
                let dx = dest_x + x as isize;
                let dy = dest_y + y as isize;
                if dx < 0 || dy < 0 {
                    continue;
                }

                let dx = dx as usize;
                let dy = dy as usize;
                if clip.is_some_and(|clip| !clip.contains_point(dx, dy)) {
                    continue;
                }

                let resolved = swap.map_or_else(
                    || palette.resolve_index(index, dx, dy),
                    |swap| palette.resolve_paint_index(swap.paint(index), dx, dy),
                );
                let Some(resolved) = resolved else {
                    continue;
                };
                self.set_pixel(dx, dy, resolved);
            }
        }
    }

    /// 9-slice blit: stretch `image` to `w`x`h` at `(x, y)`, keeping the four
    /// corner caps unscaled and tiling/stretching only the middle bands.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_resized(
        &mut self,
        image: &Sprite,
        palette: &Palette,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        left_cap: usize,
        right_cap: usize,
        top_cap: usize,
        bottom_cap: usize,
    ) {
        for dy in 0..h {
            let sy = stretch_source_coord(dy, h, image.height, top_cap, bottom_cap);
            for dx in 0..w {
                let sx = stretch_source_coord(dx, w, image.width, left_cap, right_cap);
                let Some(color) = palette.resolve_index(image.at(sx, sy), x + dx, y + dy) else {
                    continue;
                };
                self.set_pixel(x + dx, y + dy, color);
            }
        }
    }

    pub fn blit_from(&mut self, src: &Self, dest_x: usize, dest_y: usize) {
        if dest_x >= self.width || dest_y >= self.height {
            return;
        }

        let copy_width = src.width.min(self.width - dest_x);
        let copy_height = src.height.min(self.height - dest_y);
        for y in 0..copy_height {
            let src_row = src.row_indices(y, 0, copy_width);
            let dst_row = self.row_indices_mut(dest_y + y, dest_x, copy_width);
            for (&src_index, dst_index) in src_row.iter().zip(dst_row.iter_mut()) {
                if src_index != TRANSPARENT {
                    *dst_index = src_index;
                }
            }
        }
    }
}

fn decode_png(path: &str) -> Result<Vec<Rgba>, Box<dyn Error>> {
    Ok(decode_png_with_size(path)?.2)
}

pub fn decode_png_with_size(path: &str) -> Result<(usize, usize, Vec<Rgba>), Box<dyn Error>> {
    let file = File::open(path)?;
    decode_png_reader(BufReader::new(file), path)
}

/// As `decode_png_with_size`, but for a PNG already loaded into memory (e.g.
/// `include_bytes!`), so callers can embed art in the binary instead of
/// reading it from disk at runtime.
pub fn decode_png_bytes(bytes: &[u8]) -> Result<(usize, usize, Vec<Rgba>), Box<dyn Error>> {
    decode_png_reader(bytes, "<embedded bytes>")
}

fn decode_png_reader(
    reader: impl std::io::Read,
    label: &str,
) -> Result<(usize, usize, Vec<Rgba>), Box<dyn Error>> {
    let mut decoder = png::Decoder::new(reader);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data)?;
    let bytes = &data[..info.buffer_size()];

    let mut pixels = Vec::with_capacity((info.width * info.height) as usize);
    match info.color_type {
        png::ColorType::Rgb => {
            for chunk in bytes.chunks_exact(3) {
                pixels.push(Rgba {
                    r: chunk[0],
                    g: chunk[1],
                    b: chunk[2],
                    a: 255,
                });
            }
        }
        png::ColorType::Rgba => {
            for chunk in bytes.chunks_exact(4) {
                pixels.push(Rgba {
                    r: chunk[0],
                    g: chunk[1],
                    b: chunk[2],
                    a: chunk[3],
                });
            }
        }
        png::ColorType::Indexed => {
            let palette = reader
                .info()
                .palette
                .as_ref()
                .ok_or("indexed PNG has no palette")?;
            let trns = reader.info().trns.as_deref().unwrap_or(&[]);
            for &idx in bytes {
                let base = idx as usize * 3;
                if base + 2 >= palette.len() {
                    return Err(
                        format!("indexed PNG palette index {idx} out of bounds in {label}").into(),
                    );
                }
                let a = trns.get(idx as usize).copied().unwrap_or(255);
                pixels.push(Rgba {
                    r: palette[base],
                    g: palette[base + 1],
                    b: palette[base + 2],
                    a,
                });
            }
        }
        other => return Err(format!("unsupported PNG color type: {other:?}").into()),
    }

    Ok((info.width as usize, info.height as usize, pixels))
}

/// Map a destination coordinate back to a source coordinate for a 9-slice
/// stretch: the `start_cap`/`end_cap` edges map 1:1, the middle band scales to
/// fill whatever space remains. Handles the degenerate case where the
/// destination is smaller than the combined caps without panicking.
pub fn stretch_source_coord(
    dest: usize,
    dest_len: usize,
    src_len: usize,
    start_cap: usize,
    end_cap: usize,
) -> usize {
    debug_assert!(src_len > 0, "stretching an empty sprite");
    debug_assert!(
        src_len >= start_cap + end_cap,
        "9-slice caps larger than the sprite"
    );

    if dest_len <= start_cap + end_cap {
        let src_middle = src_len.saturating_sub(start_cap + end_cap).max(1);
        let dest_middle = dest_len.saturating_sub(start_cap + end_cap).max(1);
        let last = src_len.saturating_sub(1);
        if dest < start_cap.min(dest_len) {
            return dest.min(last);
        }
        if dest >= dest_len.saturating_sub(end_cap) {
            return src_len.saturating_sub(dest_len - dest).min(last);
        }
        return start_cap + (dest - start_cap) * src_middle / dest_middle;
    }

    if dest < start_cap {
        return dest;
    }
    if dest >= dest_len - end_cap {
        return src_len - (dest_len - dest);
    }

    let src_middle = src_len - start_cap - end_cap;
    let dest_middle = dest_len - start_cap - end_cap;
    start_cap + (dest - start_cap) * src_middle / dest_middle
}

const fn color_distance(a: Rgb, b: Rgb) -> u32 {
    let dr = a.r as i32 - b.r as i32;
    let dg = a.g as i32 - b.g as i32;
    let db = a.b as i32 - b.b as i32;
    (dr * dr + dg * dg + db * db) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_is_identity_when_sizes_match() {
        for dest in 0..30 {
            assert_eq!(stretch_source_coord(dest, 30, 30, 10, 10), dest);
        }
    }

    #[test]
    fn stretch_preserves_caps_and_stays_in_bounds() {
        for (dest_len, src_len) in [(60, 30), (30, 30), (12, 30), (3, 30)] {
            for dest in 0..dest_len {
                let src = stretch_source_coord(dest, dest_len, src_len, 10, 10);
                assert!(src < src_len, "dest {dest}/{dest_len} mapped to {src}");
            }
            if dest_len > 20 {
                assert_eq!(stretch_source_coord(0, dest_len, src_len, 10, 10), 0);
                assert_eq!(
                    stretch_source_coord(dest_len - 1, dest_len, src_len, 10, 10),
                    src_len - 1
                );
            }
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    #[test]
    #[ignore = "perf benchmark; run with --release --ignored"]
    fn bench_draw_sprite_full() {
        const W: usize = 256;
        const H: usize = 256;
        const ITERS: usize = 2000;

        // 32-color palette.
        let colors: Vec<Rgb> = (0..32)
            .map(|i| Rgb {
                r: (i * 8) as u8,
                g: (255 - i * 8) as u8,
                b: (i * 4) as u8,
            })
            .collect();
        let palette = Palette::from_colors(colors);

        // Opaque sprite, indices 0..31 cycling (all in range -> wrap never idivs).
        let pixels: Vec<Index> = (0..W * H).map(|n| (n % 32) as Index).collect();
        let sprite = Sprite {
            width: W,
            height: H,
            pixels,
        };

        let mut fb = Framebuffer::new(W, H, 0);
        let src = Rect::new(0, 0, W, H);

        // warmup
        for _ in 0..50 {
            fb.draw_sprite_full(&sprite, src, 0, 0, None, &palette, None);
        }

        let start = Instant::now();
        for _ in 0..ITERS {
            fb.draw_sprite_full(
                black_box(&sprite),
                black_box(src),
                0,
                0,
                None,
                black_box(&palette),
                None,
            );
        }
        black_box(&fb);
        let elapsed = start.elapsed();

        let pixels_total = (W * H * ITERS) as f64;
        let ns = elapsed.as_nanos() as f64;
        eprintln!(
            "draw_sprite_full: {ITERS} iters of {W}x{H} in {:?} => {:.2} ns/iter, {:.3} ns/pixel, {:.1} Mpix/s",
            elapsed,
            ns / ITERS as f64,
            ns / pixels_total,
            pixels_total / ns * 1000.0,
        );
    }
}
