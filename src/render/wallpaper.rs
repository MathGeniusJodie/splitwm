//! Wallpaper loading: decode, scale-to-cover, dither onto the na16 palette,
//! and a disk cache keyed on the source file's identity so a full-screen
//! image only pays the decode+dither pass once per (path, size, palette).

use pixel_graphics::{magick_decode_rgba, Framebuffer, Rgb as PgRgb, Rgba};

use crate::theme::palette_color;
use crate::Index;

use super::Renderer;

/// A loaded wallpaper together with the (path, w, h) it was produced from,
/// so `Renderer::set_wallpaper` can recognise a repeat request (e.g. a
/// same-size root ConfigureNotify) and skip the decode+dither pass.
pub(super) struct Wallpaper {
    pub(super) src: (String, i32, i32),
    pub(super) fb: Framebuffer,
}

impl Renderer {
    /// Load+scale a PNG wallpaper to cover `w`x`h`, quantized onto the na16
    /// palette. Returns whether it loaded. No-op when the same wallpaper is
    /// already loaded at this size (e.g. a same-size root ConfigureNotify).
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
        let mut h = b"splitwm-wallpaper-v1\n".to_vec();
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
        // Scale-to-cover with nearest-neighbour sampling, then quantize to
        // the palette with serpentine Floyd-Steinberg error diffusion so the
        // 16-colour result reads as smooth gradients instead of hard bands.
        let scale = (dw as f32 / sw as f32).max(dh as f32 / sh as f32);
        let ox = (sw as f32).mul_add(-scale, dw as f32) / 2.0;
        let oy = (sh as f32).mul_add(-scale, dh as f32) / 2.0;
        let mut indices = vec![palette_color::BLACK; dw * dh];
        // Two rows of per-channel accumulated error: current and next.
        let mut err_cur = vec![[0.0f32; 3]; dw];
        let mut err_next = vec![[0.0f32; 3]; dw];
        for y in 0..dh {
            let sy = (((y as f32 - oy) / scale) as usize).min(sh - 1);
            let ltr = y % 2 == 0;
            // Index flip instead of a boxed reversed iterator: this loop
            // body runs once per output pixel.
            for xi in 0..dw {
                let x = if ltr { xi } else { dw - 1 - xi };
                let sx = (((x as f32 - ox) / scale) as usize).min(sw - 1);
                let c = pixels[sy * sw + sx];
                let want = [
                    (f32::from(c.r) + err_cur[x][0]).clamp(0.0, 255.0),
                    (f32::from(c.g) + err_cur[x][1]).clamp(0.0, 255.0),
                    (f32::from(c.b) + err_cur[x][2]).clamp(0.0, 255.0),
                ];
                let index = self.palette.nearest_index(PgRgb {
                    r: want[0] as u8,
                    g: want[1] as u8,
                    b: want[2] as u8,
                });
                indices[y * dw + x] = index;
                let got = self.palette.inner().color(index);
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
        Some(indices)
    }

    /// Decode a wallpaper image (any format ImageMagick reads) into RGBA
    /// pixels via `pixel_graphics::magick_decode_rgba`.
    fn decode_image(path: &str) -> Option<(usize, usize, Vec<Rgba>)> {
        // Widest wallpaper dimension worth keeping: caps the pixel buffer
        // and the O(w*h) dither pass below (decode itself happens in the
        // magick process, under its own resource limits).
        const MAX_DIM: usize = 16_384;
        magick_decode_rgba(path, MAX_DIM)
    }
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
        let header = b"splitwm-wallpaper-v1\ntest-header".to_vec();

        store_cached_wallpaper(&cache, &header, &indices).unwrap();
        assert_eq!(
            load_cached_wallpaper(&cache, &header, 3, 2).as_deref(),
            Some(indices.as_slice())
        );

        // Any header mismatch (stale mtime/size/palette/...) is a miss, as
        // is a body of the wrong length for the requested dimensions.
        assert!(load_cached_wallpaper(&cache, b"splitwm-wallpaper-v1\nother", 3, 2).is_none());
        assert!(load_cached_wallpaper(&cache, &header, 4, 2).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wallpaper_cache_path_varies_with_header() {
        // Two different (path, size, palette) identities must land in two
        // different cache files, so switching wallpapers (or resizing a
        // monitor) never evicts the other's cached entry.
        let a = wallpaper_cache_path(b"splitwm-wallpaper-v1\nfoo.png-100x100").unwrap();
        let b = wallpaper_cache_path(b"splitwm-wallpaper-v1\nbar.png-200x200").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());

        // Same header, same path — deterministic, so a second lookup of the
        // same wallpaper actually hits its own cache file.
        let a2 = wallpaper_cache_path(b"splitwm-wallpaper-v1\nfoo.png-100x100").unwrap();
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
            let header = format!("splitwm-wallpaper-v1\nentry-{i}").into_bytes();
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

    #[test]
    fn fnv1a_is_deterministic_and_sensitive_to_input() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"hellp"));
    }
}
