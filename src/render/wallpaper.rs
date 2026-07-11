//! Wallpaper loading: decode, scale-to-cover, dither onto the na16 palette
//! (direct binary search seeded from Floyd-Steinberg), and a disk cache keyed
//! on the source file's identity so a full-screen image only pays the
//! decode+dither pass once per (path, size, palette).

use pixel_graphics::{magick_decode_rgba, Framebuffer, Rgb as PgRgb, Rgba};

use crate::oklch::{srgb8_to_oklab, Oklab, OklabPalette};
use crate::theme::palette_color;
use crate::Index;

use super::Renderer;

/// A loaded wallpaper together with the (path, w, h) it was produced from,
/// so `Renderer::set_wallpaper` can recognise a repeat request (e.g. a
/// same-size root `ConfigureNotify`) and skip the decode+dither pass.
pub(super) struct Wallpaper {
    pub(super) src: (String, i32, i32),
    pub(super) fb: Framebuffer,
}

impl Renderer {
    /// Load+scale a PNG wallpaper to cover `w`x`h`, quantized onto the na16
    /// palette. Returns whether it loaded. No-op when the same wallpaper is
    /// already loaded at this size (e.g. a same-size root `ConfigureNotify`).
    pub fn set_wallpaper(&mut self, path: &str, w: i32, h: i32) -> bool {
        let src = (path.to_string(), w, h);
        if self.wallpaper.as_ref().is_some_and(|wp| wp.src == src) {
            return true;
        }
        self.wallpaper = self
            .load_wallpaper(path, w, h)
            .map(|fb| Wallpaper { src, fb });
        self.wallpaper.is_some()
    }

    fn load_wallpaper(&self, path: &str, w: i32, h: i32) -> Option<Framebuffer> {
        let (dw, dh) = (w.max(1) as usize, h.max(1) as usize);
        // The quantized result is cached on disk: the decode+dither pass
        // costs noticeable startup time on a full-screen image, and its
        // output only changes when the source file, target size or palette
        // does — exactly what the cache header records. Each distinct header
        // gets its own cache file (named from a hash of the header), so
        // multiple (path, size, palette) combinations coexist on disk
        // instead of thrashing a single shared slot.
        let header = self.wallpaper_cache_header(path, dw, dh);
        let cache = header.as_deref().and_then(wallpaper_cache_path);
        if let (Some(header), Some(cache)) = (header.as_deref(), cache.as_deref()) {
            if let Some(indices) = load_cached_wallpaper(cache, header, dw, dh) {
                return Some(fb_from_indices(dw, dh, &indices));
            }
        }
        let indices = self.dither_wallpaper(path, dw, dh)?;
        if let (Some(header), Some(cache)) = (header, cache) {
            // Best-effort: a failed write just means the next startup
            // re-dithers.
            let _ = store_cached_wallpaper(&cache, &header, &indices);
        }
        Some(fb_from_indices(dw, dh, &indices))
    }

    /// The validation header a cached quantization of `path` at `dw`x`dh`
    /// with the current palette must carry. `None` when the source file
    /// can't be stat'ed (it won't decode either).
    fn wallpaper_cache_header(&self, path: &str, dw: usize, dh: usize) -> Option<Vec<u8>> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        // v2: the dither algorithm (FS -> DBS) is part of the cached output's
        // identity, so the version bumps whenever it changes.
        let mut h = b"splitwm-wallpaper-v2\n".to_vec();
        h.extend((dw as u64).to_le_bytes());
        h.extend((dh as u64).to_le_bytes());
        h.extend(meta.len().to_le_bytes());
        h.extend(mtime.as_secs().to_le_bytes());
        h.extend(mtime.subsec_nanos().to_le_bytes());
        h.extend((path.len() as u64).to_le_bytes());
        h.extend(path.as_bytes());
        h.push(self.palette.inner().len() as u8);
        for i in 0..self.palette.inner().len() {
            let c = self.palette.inner().color(i as Index);
            h.extend([c.r, c.g, c.b]);
        }
        Some(h)
    }

    /// Decode `path` and scale-to-cover `dw`x`dh`, quantized onto the
    /// palette — the expensive pass behind the disk cache. Returns the
    /// row-major palette indices (the form the cache stores).
    fn dither_wallpaper(&self, path: &str, dw: usize, dh: usize) -> Option<Vec<Index>> {
        let (sw, sh, pixels) = Self::decode_image(path)?;
        // Belt-and-braces: a malformed/truncated wallpaper file must never
        // reach the sampling loop below, which indexes `pixels` unchecked.
        if sw == 0 || sh == 0 || pixels.len() < sw * sh {
            return None;
        }
        // Scale-to-cover with nearest-neighbour sampling.
        let scale = (dw as f32 / sw as f32).max(dh as f32 / sh as f32);
        let ox = (sw as f32).mul_add(-scale, dw as f32) / 2.0;
        let oy = (sh as f32).mul_add(-scale, dh as f32) / 2.0;
        let mut scaled = Vec::with_capacity(dw * dh);
        for y in 0..dh {
            let sy = (((y as f32 - oy) / scale) as usize).min(sh - 1);
            for x in 0..dw {
                let sx = (((x as f32 - ox) / scale) as usize).min(sw - 1);
                let c = pixels[sy * sw + sx];
                scaled.push(PgRgb {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                });
            }
        }
        // Quantize onto the palette with direct binary search: a serpentine
        // Floyd-Steinberg pass seeds the halftone, then DBS iteratively
        // repairs it towards a local minimum of Gaussian-filtered OKLab
        // error, trading (cached) startup time for less worming/noise than
        // error diffusion alone.
        let mut indices = error_diffuse(&self.palette, &scaled, dw, dh);
        dbs_refine(&self.palette, &scaled, &mut indices, dw, dh);
        Some(indices)
    }

    /// Decode a wallpaper image (any format `ImageMagick` reads) into RGBA
    /// pixels via `pixel_graphics::magick_decode_rgba`.
    fn decode_image(path: &str) -> Option<(usize, usize, Vec<Rgba>)> {
        // Widest wallpaper dimension worth keeping: caps the pixel buffer
        // and the O(w*h) dither pass below (decode itself happens in the
        // magick process, under its own resource limits).
        const MAX_DIM: usize = 16_384;
        magick_decode_rgba(path, MAX_DIM)
    }
}

/// Serpentine Floyd-Steinberg error diffusion of `scaled` (row-major,
/// `dw*dh` pixels) onto the palette — the seed halftone for `dbs_refine`,
/// which converges faster and to a better local minimum from an
/// error-diffused start than from a flat nearest-colour one.
fn error_diffuse(palette: &OklabPalette, scaled: &[PgRgb], dw: usize, dh: usize) -> Vec<Index> {
    let mut indices = vec![palette_color::BLACK; dw * dh];
    // Two rows of per-channel accumulated error: current and next.
    let mut err_cur = vec![[0.0f32; 3]; dw];
    let mut err_next = vec![[0.0f32; 3]; dw];
    for y in 0..dh {
        let ltr = y % 2 == 0;
        // Index flip instead of a boxed reversed iterator: this loop
        // body runs once per output pixel.
        for xi in 0..dw {
            let x = if ltr { xi } else { dw - 1 - xi };
            let c = scaled[y * dw + x];
            let want = [
                (f32::from(c.r) + err_cur[x][0]).clamp(0.0, 255.0),
                (f32::from(c.g) + err_cur[x][1]).clamp(0.0, 255.0),
                (f32::from(c.b) + err_cur[x][2]).clamp(0.0, 255.0),
            ];
            let index = palette.nearest_index(PgRgb {
                r: want[0] as u8,
                g: want[1] as u8,
                b: want[2] as u8,
            });
            indices[y * dw + x] = index;
            let got = palette.inner().color(index);
            let err = [
                want[0] - f32::from(got.r),
                want[1] - f32::from(got.g),
                want[2] - f32::from(got.b),
            ];
            // Floyd-Steinberg kernel, mirrored on right-to-left rows:
            //         *   7/16
            //  3/16  5/16  1/16
            let ahead = if ltr { x + 1 } else { x.wrapping_sub(1) };
            let behind = if ltr { x.wrapping_sub(1) } else { x + 1 };
            for ch in 0..3 {
                if ahead < dw {
                    err_cur[ahead][ch] += err[ch] * (7.0 / 16.0);
                    err_next[ahead][ch] += err[ch] * (1.0 / 16.0);
                }
                if behind < dw {
                    err_next[behind][ch] += err[ch] * (3.0 / 16.0);
                }
                err_next[x][ch] += err[ch] * (5.0 / 16.0);
            }
        }
        std::mem::swap(&mut err_cur, &mut err_next);
        err_next.fill([0.0; 3]);
    }
    indices
}

/// The eye model DBS minimises error through: a Gaussian of `DBS_SIGMA`
/// output pixels, truncated where its tails stop mattering. Wider sigmas
/// push noise into lower frequencies (smoother but blurrier placement);
/// ~1.5px suits a monitor at desktop viewing distance.
const DBS_SIGMA: f32 = 1.5;
const DBS_RADIUS: isize = 3;

/// The 1D slice of the (separable) 2D Gaussian eye filter, `2*DBS_RADIUS+1`
/// centred taps, normalised to sum 1 so filtered values stay in OKLab units
/// and `DBS_MIN_GAIN` has a scale to be meaningful against.
fn dbs_eye_filter() -> Vec<f32> {
    let taps: Vec<f32> = (-DBS_RADIUS..=DBS_RADIUS)
        .map(|k| (-(k * k) as f32 / (2.0 * DBS_SIGMA * DBS_SIGMA)).exp())
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter().map(|t| t / sum).collect()
}

/// Refine `indices` in place with direct binary search (Analoui &
/// Allebach): repeatedly sweep the image, and at each pixel adopt the
/// palette entry that most reduces the total squared error between the
/// halftone and `scaled` as seen through a Gaussian model of the eye's
/// low-pass filtering, until a sweep changes nothing. Error is measured
/// per-channel in OKLab, the same perceptual space `nearest_index` snaps
/// in.
///
/// The incremental form keeps this tractable: with `e = halftone -
/// original` and `cpp` the filter's autocorrelation, swapping pixel `n`'s
/// colour by `a` changes the total error by `a²·cpp(0) + 2a·cpe(n)` where
/// `cpe = cpp ⊛ e`, so each trial is O(1) and only an accepted swap pays
/// an O(|cpp|) update to `cpe`.
///
/// Sweeps run band-parallel: a swap only reaches `cpe` within the filter
/// support, so horizontal bands taller than that support can't interact,
/// and sweeping even bands then odd bands under `std::thread::scope` makes
/// every trial see exactly the `cpe` a serial sweep of its band would —
/// the same objective, acceptance rule and strict-decrease guarantee, just
/// a different (still deterministic for a given band layout) visit order.
/// DBS has no canonical visit order to preserve in the first place.
fn dbs_refine(
    palette: &OklabPalette,
    scaled: &[PgRgb],
    indices: &mut [Index],
    dw: usize,
    dh: usize,
) {
    // DBS strictly decreases the total error, so it terminates on its own;
    // the cap only bounds pathological inputs (photographs' accept counts
    // decay geometrically and settle within this budget, and the dirty
    // flags make near-converged sweeps almost free).
    const DBS_MAX_SWEEPS: usize = 20;

    let palette = palette.oklab_colors();
    let original: Vec<Oklab> = scaled.iter().map(|&c| srgb8_to_oklab(c)).collect();

    // cpp1d[t+2R] = Σₖ h(k)·h(k+t), the Gaussian's 1D autocorrelation;
    // the 2D filter and hence its autocorrelation are separable, so
    // cpp(x,y) = cpp1d[x]·cpp1d[y].
    let h = dbs_eye_filter();
    let taps = h.len() as isize;
    let cpp1d: Vec<f32> = (-2 * DBS_RADIUS..=2 * DBS_RADIUS)
        .map(|t| {
            (0..taps)
                .filter(|k| (0..taps).contains(&(k + t)))
                .map(|k| h[k as usize] * h[(k + t) as usize])
                .sum()
        })
        .collect();
    let cpp_origin = cpp1d[2 * DBS_RADIUS as usize] * cpp1d[2 * DBS_RADIUS as usize];

    // ΔE of swapping a pixel from `cur` to candidate `j` factors as
    // score(j) - score(cur) with score(j) = cpp(0)·|palⱼ|² + palⱼ·v(n),
    // where only v(n) = 2(cpe(n) - cpp(0)·pal_cur) depends on the pixel —
    // so the per-candidate trial is one dot product against this
    // precomputed bias.
    let cand_bias: Vec<f32> = palette
        .iter()
        .map(|p| cpp_origin * p.iter().map(|c| c * c).sum::<f32>())
        .collect();

    // cpe = cpp ⊛ e, built once by separable convolution (zero-padded
    // borders, matching the clipped in-place updates in the sweeps), then
    // kept current incrementally as swaps are accepted.
    let e: Vec<Oklab> = indices
        .iter()
        .zip(&original)
        .map(|(&i, o)| {
            let p = palette[i as usize];
            [p[0] - o[0], p[1] - o[1], p[2] - o[2]]
        })
        .collect();
    let mut cpe = convolve_x(&e, dw, dh, &cpp1d);
    cpe = convolve_y(&cpe, dw, dh, &cpp1d);

    // A pixel's best swap can only change when `cpe` changes under it, i.e.
    // when a swap lands within the filter support. Tracking that as a dirty
    // flag lets converged regions be skipped, which is what makes running
    // to full convergence affordable: late sweeps re-examine only the few
    // spots still in flux instead of the whole image.
    let mut dirty = vec![true; dw * dh];

    // Band layout: bands must be at least as tall as the update reach (so
    // same-parity bands' writes can't collide), and there's no point in
    // more than two bands per core (each parity phase runs half of them).
    let reach = 2 * DBS_RADIUS as usize;
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let bands = (dh / (2 * reach)).clamp(1, 2 * cores);
    let band_rows: Vec<std::ops::Range<usize>> = (0..bands)
        .map(|b| (dh * b / bands)..(dh * (b + 1) / bands))
        .collect();

    let sweep = DbsSweep {
        palette,
        cand_bias: &cand_bias,
        cpp1d: &cpp1d,
        cpp_origin,
        dw,
    };
    for _ in 0..DBS_MAX_SWEEPS {
        let mut changed = false;
        for parity in 0..2 {
            // Split the buffers into per-band chunks: `indices` by the rows
            // a band sweeps, `cpe`/`dirty` by those rows extended to the
            // update reach (this phase's bands are two apart, so extended
            // ranges stay disjoint and the borrows really are exclusive).
            let mut idx_rest = &mut *indices;
            let mut cpe_rest = &mut *cpe;
            let mut dirty_rest = &mut *dirty;
            let (mut idx_at, mut ext_at) = (0, 0);
            let mut jobs = Vec::new();
            for rows in band_rows.iter().skip(parity).step_by(2) {
                let ext = rows.start.saturating_sub(reach)..(rows.end + reach).min(dh);
                let idx = split_rows(&mut idx_rest, rows.start - idx_at, rows.len(), dw);
                let ext_cpe = split_rows(&mut cpe_rest, ext.start - ext_at, ext.len(), dw);
                let ext_dirty = split_rows(&mut dirty_rest, ext.start - ext_at, ext.len(), dw);
                (idx_at, ext_at) = (rows.end, ext.end);
                jobs.push((rows.start - ext.start, idx, ext_cpe, ext_dirty));
            }
            changed |= std::thread::scope(|scope| {
                let workers: Vec<_> = jobs
                    .into_iter()
                    .map(|(off, idx, ext_cpe, ext_dirty)| {
                        scope.spawn(move || sweep.band(off, idx, ext_cpe, ext_dirty))
                    })
                    .collect();
                workers.into_iter().any(|w| w.join().unwrap())
            });
        }
        if !changed {
            break;
        }
    }
}

/// Detach the chunk covering `rows` rows from the front of `*rest` after
/// discarding `skip` rows, leaving the tail in `*rest` — the disjoint
/// exclusive borrows the parallel band sweep hands its workers.
fn split_rows<'a, T>(rest: &mut &'a mut [T], skip: usize, rows: usize, dw: usize) -> &'a mut [T] {
    let (_, tail) = std::mem::take(rest).split_at_mut(skip * dw);
    let (chunk, tail) = tail.split_at_mut(rows * dw);
    *rest = tail;
    chunk
}

/// The per-pixel-invariant state of one DBS sweep, shared read-only across
/// the band workers.
#[derive(Clone, Copy)]
struct DbsSweep<'a> {
    palette: &'a [Oklab],
    cand_bias: &'a [f32],
    cpp1d: &'a [f32],
    cpp_origin: f32,
    dw: usize,
}

impl DbsSweep<'_> {
    /// Sweep one band: `indices` covers exactly the band's rows, while
    /// `cpe`/`dirty` also carry `off` extra rows above (and whatever fits
    /// below) so accepted swaps can write out to the full update reach.
    /// Returns whether any swap was accepted.
    fn band(
        &self,
        off: usize,
        indices: &mut [Index],
        cpe: &mut [Oklab],
        dirty: &mut [bool],
    ) -> bool {
        // Ignore improvements below this to keep float noise from churning:
        // ~two orders below the filtered-squared-OKLab gain of a worthwhile
        // swap, given the sum-1 filter keeps everything in OKLab units.
        const DBS_MIN_GAIN: f32 = 1e-6;

        let dw = self.dw;
        let ext_rows = (cpe.len() / dw) as isize;
        let mut changed = false;
        for row in 0..indices.len() / dw {
            let er = row + off;
            for x in 0..dw {
                let n = er * dw + x;
                if !std::mem::replace(&mut dirty[n], false) {
                    continue;
                }
                let cur = indices[row * dw + x] as usize;
                let pc = self.palette[cur];
                let v = [
                    2.0 * self.cpp_origin.mul_add(-pc[0], cpe[n][0]),
                    2.0 * self.cpp_origin.mul_add(-pc[1], cpe[n][1]),
                    2.0 * self.cpp_origin.mul_add(-pc[2], cpe[n][2]),
                ];
                let score = |j: usize| {
                    let p = self.palette[j];
                    p[0].mul_add(
                        v[0],
                        p[1].mul_add(v[1], p[2].mul_add(v[2], self.cand_bias[j])),
                    )
                };
                let mut best = (cur, score(cur));
                for j in 0..self.palette.len() {
                    let s = score(j);
                    if s < best.1 {
                        best = (j, s);
                    }
                }
                if best.1 - score(cur) >= -DBS_MIN_GAIN {
                    continue;
                }
                let cand = self.palette[best.0];
                let a = [cand[0] - pc[0], cand[1] - pc[1], cand[2] - pc[2]];
                indices[row * dw + x] = best.0 as Index;
                changed = true;
                // cpe(n') += a·cpp(n'-n) over cpp's support, clipped at the
                // chunk edge (which the band layout makes coincide with the
                // image border or the true reach of the update).
                for dy in -2 * DBS_RADIUS..=2 * DBS_RADIUS {
                    let yy = er as isize + dy;
                    if !(0..ext_rows).contains(&yy) {
                        continue;
                    }
                    let wy = self.cpp1d[(dy + 2 * DBS_RADIUS) as usize];
                    for dx in -2 * DBS_RADIUS..=2 * DBS_RADIUS {
                        let xx = x as isize + dx;
                        if !(0..dw as isize).contains(&xx) {
                            continue;
                        }
                        let w = wy * self.cpp1d[(dx + 2 * DBS_RADIUS) as usize];
                        let m = yy as usize * dw + xx as usize;
                        for ch in 0..3 {
                            cpe[m][ch] = a[ch].mul_add(w, cpe[m][ch]);
                        }
                        dirty[m] = true;
                    }
                }
            }
        }
        changed
    }
}

/// Horizontal pass of a separable 2D convolution: each output pixel is the
/// kernel-weighted sum of its row neighbours, zero-padded past the borders.
/// `kernel` has odd length and is centred.
fn convolve_x(src: &[Oklab], dw: usize, dh: usize, kernel: &[f32]) -> Vec<Oklab> {
    let radius = (kernel.len() / 2) as isize;
    let mut out = vec![[0.0f32; 3]; src.len()];
    for y in 0..dh {
        let row = y * dw;
        for x in 0..dw {
            let mut acc = [0.0f32; 3];
            for (k, &w) in kernel.iter().enumerate() {
                let xx = x as isize + k as isize - radius;
                if (0..dw as isize).contains(&xx) {
                    let s = src[row + xx as usize];
                    for ch in 0..3 {
                        acc[ch] = s[ch].mul_add(w, acc[ch]);
                    }
                }
            }
            out[row + x] = acc;
        }
    }
    out
}

/// Vertical counterpart of [`convolve_x`].
fn convolve_y(src: &[Oklab], dw: usize, dh: usize, kernel: &[f32]) -> Vec<Oklab> {
    let radius = (kernel.len() / 2) as isize;
    let mut out = vec![[0.0f32; 3]; src.len()];
    for y in 0..dh {
        for x in 0..dw {
            let mut acc = [0.0f32; 3];
            for (k, &w) in kernel.iter().enumerate() {
                let yy = y as isize + k as isize - radius;
                if (0..dh as isize).contains(&yy) {
                    let s = src[yy as usize * dw + x];
                    for ch in 0..3 {
                        acc[ch] = s[ch].mul_add(w, acc[ch]);
                    }
                }
            }
            out[y * dw + x] = acc;
        }
    }
    out
}

/// Where the quantized wallpaper cache for a given `header` lives:
/// `$XDG_CACHE_HOME/splitwm/wallpaper-<hash>` (default `~/.cache`), where
/// `<hash>` is derived from the header's identity (source path, target size,
/// palette). Distinct headers land in distinct files, so switching between
/// wallpapers — or resizing a monitor — never evicts another entry's cache.
/// `None` when neither `XDG_CACHE_HOME` nor `HOME` is set.
fn wallpaper_cache_path(header: &[u8]) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    Some(
        base.join("splitwm")
            .join(format!("wallpaper-{:016x}", fnv1a(header))),
    )
}

/// FNV-1a over `bytes`. Used instead of `DefaultHasher` because the latter's
/// output isn't guaranteed stable across Rust releases — the cache filename
/// only needs to be *a* stable function of the header, not a great one, since
/// the header itself is re-checked in full on load; a stable hash just keeps
/// existing cache files from being orphaned by a toolchain upgrade.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    bytes
        .iter()
        .fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// Read a cached quantized wallpaper: `header` followed by exactly `dw*dh`
/// palette indices. Any mismatch (stale source, different size/palette,
/// truncation) is a miss. Index *values* aren't validated — presenting runs
/// every index through a full 256-entry LUT, so corrupt bytes can only
/// render as wrong colours, never break memory safety.
fn load_cached_wallpaper(
    cache: &std::path::Path,
    header: &[u8],
    dw: usize,
    dh: usize,
) -> Option<Vec<Index>> {
    let mut bytes = std::fs::read(cache).ok()?;
    if !bytes.starts_with(header) || bytes.len() - header.len() != dw * dh {
        return None;
    }
    // Reuse the read buffer instead of copying the multi-megabyte body.
    Some(bytes.split_off(header.len()))
}

fn store_cached_wallpaper(
    cache: &std::path::Path,
    header: &[u8],
    indices: &[Index],
) -> std::io::Result<()> {
    if let Some(dir) = cache.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(header.len() + indices.len());
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(indices);
    std::fs::write(cache, bytes)?;
    evict_stale_wallpaper_caches(cache);
    Ok(())
}

/// Cap on distinct `wallpaper-*` files kept in the cache directory: enough
/// for a couple of monitor sizes plus one wallpaper switch, in keeping with
/// the "bounded in practice, but nothing should grow without a lid" policy
/// applied to the in-memory caches in `insert_capped` (see `render/mod.rs`).
/// Each distinct (path, size, palette) header gets its own file and nothing
/// ever deletes an old one on its own, so without this the directory would
/// grow by a few megabytes per wallpaper change or resolution switch,
/// forever.
const MAX_CACHED_WALLPAPERS: usize = 4;

/// Delete all but the `MAX_CACHED_WALLPAPERS` most-recently-modified
/// `wallpaper-*` files next to `just_written`, oldest first. Best-effort:
/// listing or deleting is skipped silently on error, since eviction is
/// housekeeping and must never turn a successful cache write into a failed
/// one — a missed eviction just means the directory grows a little more
/// before the next store retries it.
fn evict_stale_wallpaper_caches(just_written: &std::path::Path) {
    let Some(dir) = just_written.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("wallpaper-"))
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= MAX_CACHED_WALLPAPERS {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    for (_, path) in &files[..files.len() - MAX_CACHED_WALLPAPERS] {
        let _ = std::fs::remove_file(path);
    }
}

/// A `dw`x`dh` framebuffer holding `indices` (row-major, `dw*dh` entries).
fn fb_from_indices(dw: usize, dh: usize, indices: &[Index]) -> Framebuffer {
    let mut fb = Framebuffer::new(dw, dh, palette_color::BLACK);
    for y in 0..dh {
        for x in 0..dw {
            fb.set_pixel(x as isize, y as isize, indices[y * dw + x]);
        }
    }
    fb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_cache_round_trips_and_rejects_stale_headers() {
        let dir = std::env::temp_dir().join(format!("splitwm-wp-test-{}", std::process::id()));
        let cache = dir.join("wallpaper");
        let indices: Vec<Index> = vec![0, 1, 2, 3, 4, 5];
        let header = b"splitwm-wallpaper-v2\ntest-header".to_vec();

        store_cached_wallpaper(&cache, &header, &indices).unwrap();
        assert_eq!(
            load_cached_wallpaper(&cache, &header, 3, 2).as_deref(),
            Some(indices.as_slice())
        );

        // Any header mismatch (stale mtime/size/palette/...) is a miss, as
        // is a body of the wrong length for the requested dimensions.
        assert!(load_cached_wallpaper(&cache, b"splitwm-wallpaper-v2\nother", 3, 2).is_none());
        assert!(load_cached_wallpaper(&cache, &header, 4, 2).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wallpaper_cache_path_varies_with_header() {
        // Two different (path, size, palette) identities must land in two
        // different cache files, so switching wallpapers (or resizing a
        // monitor) never evicts the other's cached entry.
        let a = wallpaper_cache_path(b"splitwm-wallpaper-v2\nfoo.png-100x100").unwrap();
        let b = wallpaper_cache_path(b"splitwm-wallpaper-v2\nbar.png-200x200").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());

        // Same header, same path — deterministic, so a second lookup of the
        // same wallpaper actually hits its own cache file.
        let a2 = wallpaper_cache_path(b"splitwm-wallpaper-v2\nfoo.png-100x100").unwrap();
        assert_eq!(a, a2);
    }

    #[test]
    fn store_evicts_all_but_the_most_recent_n() {
        let dir =
            std::env::temp_dir().join(format!("splitwm-wp-evict-test-{}", std::process::id()));
        // More distinct entries than the cap: each `store_cached_wallpaper`
        // call should leave only the newest `MAX_CACHED_WALLPAPERS` behind.
        let n = MAX_CACHED_WALLPAPERS + 3;
        for i in 0..n {
            let cache = dir.join(format!("wallpaper-{i:016x}"));
            let header = format!("splitwm-wallpaper-v2\nentry-{i}").into_bytes();
            store_cached_wallpaper(&cache, &header, &[0, 1, 2]).unwrap();
            // Distinct mtimes so "most recent" is unambiguous even on
            // filesystems with coarse mtime resolution.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let remaining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_str().unwrap().to_string())
            .collect();
        assert_eq!(remaining.len(), MAX_CACHED_WALLPAPERS);
        for i in (n - MAX_CACHED_WALLPAPERS)..n {
            assert!(
                remaining.contains(&format!("wallpaper-{i:016x}")),
                "expected the most recent entry {i} to survive eviction, got {remaining:?}"
            );
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The quantity DBS minimises: the total squared OKLab error between
    /// halftone and original, seen through the Gaussian eye filter.
    fn filtered_error(
        palette: &OklabPalette,
        scaled: &[PgRgb],
        indices: &[Index],
        dw: usize,
        dh: usize,
    ) -> f32 {
        let e: Vec<Oklab> = indices
            .iter()
            .zip(scaled)
            .map(|(&i, &c)| {
                let (p, o) = (palette.oklab_colors()[i as usize], srgb8_to_oklab(c));
                [p[0] - o[0], p[1] - o[1], p[2] - o[2]]
            })
            .collect();
        let h = dbs_eye_filter();
        let filtered = convolve_y(&convolve_x(&e, dw, dh, &h), dw, dh, &h);
        filtered.iter().flatten().map(|v| v * v).sum()
    }

    #[test]
    fn dbs_improves_on_the_error_diffused_seed() {
        let palette = OklabPalette::new(crate::assets::palette());
        let (dw, dh) = (48, 32);
        // A diagonal colour gradient: forces dithering (almost no pixel is an
        // exact palette colour) with structure in both axes.
        let scaled: Vec<PgRgb> = (0..dw * dh)
            .map(|n| PgRgb {
                r: ((n % dw) * 255 / (dw - 1)) as u8,
                g: ((n / dw) * 255 / (dh - 1)) as u8,
                b: 128,
            })
            .collect();
        let seed = error_diffuse(&palette, &scaled, dw, dh);
        let mut refined = seed.clone();
        dbs_refine(&palette, &scaled, &mut refined, dw, dh);
        // Every swap DBS accepts strictly reduces the filtered error, so the
        // refined result must beat the seed (on a gradient it always finds
        // something to improve).
        assert_ne!(refined, seed);
        assert!(
            filtered_error(&palette, &scaled, &refined, dw, dh)
                < filtered_error(&palette, &scaled, &seed, dw, dh)
        );
    }

    #[test]
    fn fnv1a_is_deterministic_and_sensitive_to_input() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"hellp"));
    }
}
