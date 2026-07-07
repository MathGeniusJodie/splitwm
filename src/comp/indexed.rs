//! Palette-indexed chrome straight to the GPU.
//!
//! Every piece of software-drawn chrome (the wallpaper+frames+taskbar
//! underlay, per-float frames, notification bubbles, cursor sprites) is an
//! 8bpp palette-indexed `pixel_graphics::Framebuffer`. Rather than expand
//! each index to ARGB on the CPU, the raw index bytes upload as a GL `R8`
//! texture and a custom fragment shader does the palette lookup on the GPU:
//! the na16 palette is baked into the shader as a `const vec4` array, and a
//! texel's index selects its entry (index `TRANSPARENT` -> fully
//! transparent). Output is premultiplied — the palette is opaque, so opaque
//! entries pass through and the transparent slot is all zeros.
//!
//! The palette is uploaded once as a 256x1 `RGBA` texture and sampled on a
//! second sampler: GLSL ES 1.00 (the version smithay's fixed vertex shader
//! forces) has no array constructors, so a `const vec4[256]` in the shader
//! source will not compile. A texture lookup is portable and just as cheap.
//!
//! `IndexedProgram` compiles the shader once per renderer and owns the
//! palette texture and reused upload staging buffer; each chrome source owns
//! an `IndexedTexture` (its persistent GL texture, element id, and commit
//! counter). Re-uploading bumps the commit so the damage tracker repaints
//! the whole element, matching the previous full-buffer redraw.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName,
    UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use pixel_graphics::Framebuffer;

/// Texture unit the palette lookup texture is bound on while drawing (unit 0
/// is the indexed texture, bound by `render_texture_from_to` itself).
const PALETTE_UNIT: i32 = 1;

/// The compiled palette shader, the palette lookup texture, and the reused
/// upload staging buffer, one per renderer. Lives in `Comp`; every chrome
/// source uploads and draws through it.
pub struct IndexedProgram {
    program: GlesTexProgram,
    /// 256x1 `RGBA` palette, index -> premultiplied colour, sampled by the
    /// shader on `PALETTE_UNIT`. Held as a `GlesTexture` so its GL id stays
    /// valid (and is freed) for the program's whole life.
    palette_tex: GlesTexture,
    /// Row-packed `R8` bytes for the current upload, recycled across frames
    /// (chrome re-uploads are frequent; a per-upload allocation would be
    /// churn).
    staging: Vec<u8>,
}

/// One chrome source's GPU texture: the `R8` indexed pixels plus the
/// identity the damage tracker follows it by. The element id and commit
/// counter persist across frames so an unchanged source reports no damage
/// and a re-upload reports full damage.
pub struct IndexedTexture {
    id: Id,
    texture: GlesTexture,
    commit: CommitCounter,
    size: Size<i32, Buffer>,
    /// Whether every texel is an opaque palette colour (the wallpaper-backed
    /// underlay); false when the source has transparent texels (frames,
    /// notes, cursor).
    opaque: bool,
}

impl IndexedProgram {
    /// Compile the palette shader and upload the palette texture on
    /// `renderer` (once, at `Comp::new`).
    pub fn new(renderer: &mut GlesRenderer) -> IndexedProgram {
        let program = renderer
            .compile_custom_texture_shader(
                SHADER_SOURCE,
                &[UniformName::new("palette", UniformType::_1i)],
            )
            .expect("compile palette shader");
        let data = palette_rgba();
        let size = Size::<i32, Buffer>::from((256, 1));
        let tex_id = renderer
            .with_context(|gl| unsafe { create_texture(gl, size, ffi::RGBA8, ffi::RGBA, &data) })
            .expect("create palette texture");
        // A genuine RGBA8 texture, so the format is honest here.
        let palette_tex =
            unsafe { GlesTexture::from_raw(renderer, Some(ffi::RGBA8), true, tex_id, size) };
        IndexedProgram {
            program,
            palette_tex,
            staging: Vec::new(),
        }
    }

    /// Upload `fb`'s indices into `target`, reusing the shared staging
    /// buffer. For the chrome buffers re-uploaded as the layout changes
    /// (underlay, float frames, notes), where a per-frame allocation would
    /// be churn.
    pub fn upload(
        &mut self,
        renderer: &mut GlesRenderer,
        target: &mut Option<IndexedTexture>,
        fb: &Framebuffer,
        opaque: bool,
    ) {
        upload_into(renderer, target, fb, opaque, &mut self.staging);
    }

    /// Upload `fb` with a throwaway staging buffer, for one-off uploads (the
    /// cursor cache, one per shape) that can't borrow the shared staging
    /// mutably while a scene holds the program shared.
    pub fn upload_owned(
        &self,
        renderer: &mut GlesRenderer,
        target: &mut Option<IndexedTexture>,
        fb: &Framebuffer,
        opaque: bool,
    ) {
        upload_into(renderer, target, fb, opaque, &mut Vec::new());
    }

    /// A render element drawing `tex` at `loc` (output-relative, scale 1)
    /// with the given kind and alpha.
    pub fn element(
        &self,
        tex: &IndexedTexture,
        loc: Point<i32, Physical>,
        kind: Kind,
    ) -> IndexedElement {
        IndexedElement {
            id: tex.id.clone(),
            texture: tex.texture.clone(),
            program: self.program.clone(),
            palette_tex: self.palette_tex.tex_id(),
            commit: tex.commit,
            loc,
            size: tex.size,
            opaque: tex.opaque,
            kind,
        }
    }
}

/// Pack `fb`'s rows into `staging` and upload them into `target`, creating
/// the GL texture on first use and refreshing it in place while its size is
/// unchanged (bumping the commit so the damage tracker repaints the whole
/// element). `opaque` states whether every texel is a real palette colour;
/// it never changes for a given source.
fn upload_into(
    renderer: &mut GlesRenderer,
    target: &mut Option<IndexedTexture>,
    fb: &Framebuffer,
    opaque: bool,
    staging: &mut Vec<u8>,
) {
    let size = Size::<i32, Buffer>::from((fb.width as i32, fb.height as i32));
    staging.clear();
    staging.reserve(fb.width * fb.height);
    for y in 0..fb.height {
        staging.extend_from_slice(fb.row(y as isize));
    }

    match target {
        Some(t) if t.size == size => {
            let tex_id = t.texture.tex_id();
            let _ = renderer.with_context(|gl| unsafe {
                upload_sub(gl, tex_id, size, staging);
            });
            t.commit.increment();
        }
        _ => {
            let tex_id = renderer
                .with_context(|gl| unsafe { create_texture(gl, size, ffi::R8, ffi::RED, staging) })
                .expect("indexed texture upload");
            // The `internal_format` is only consulted to pick a shader
            // variant (`variant_for_format`): `RGBA8` + opaque selects the
            // plain `sampler2D` variant. The real texture is `R8`, which that
            // selector panics on, so it must not be passed here.
            let texture =
                unsafe { GlesTexture::from_raw(renderer, Some(ffi::RGBA8), opaque, tex_id, size) };
            *target = Some(IndexedTexture {
                id: Id::new(),
                texture,
                commit: CommitCounter::default(),
                size,
                opaque,
            });
        }
    }
}

/// Create a `NEAREST`/`CLAMP_TO_EDGE` texture from tightly packed bytes.
/// `NEAREST` keeps the lookup exact (no blending between indices or palette
/// entries); `UNPACK_ALIGNMENT 1` handles the arbitrary widths (the default
/// 4-byte row alignment would shear odd-width `R8` rows).
unsafe fn create_texture(
    gl: &ffi::Gles2,
    size: Size<i32, Buffer>,
    internal: u32,
    format: u32,
    data: &[u8],
) -> u32 {
    let mut tex = 0;
    gl.GenTextures(1, &mut tex);
    gl.BindTexture(ffi::TEXTURE_2D, tex);
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_MIN_FILTER,
        ffi::NEAREST as i32,
    );
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_MAG_FILTER,
        ffi::NEAREST as i32,
    );
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_WRAP_S,
        ffi::CLAMP_TO_EDGE as i32,
    );
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_WRAP_T,
        ffi::CLAMP_TO_EDGE as i32,
    );
    gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);
    gl.TexImage2D(
        ffi::TEXTURE_2D,
        0,
        internal as i32,
        size.w,
        size.h,
        0,
        format,
        ffi::UNSIGNED_BYTE,
        data.as_ptr().cast(),
    );
    gl.BindTexture(ffi::TEXTURE_2D, 0);
    tex
}

/// Refresh an existing same-size `R8` texture's contents in place.
unsafe fn upload_sub(gl: &ffi::Gles2, tex: u32, size: Size<i32, Buffer>, data: &[u8]) {
    gl.BindTexture(ffi::TEXTURE_2D, tex);
    gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);
    gl.TexSubImage2D(
        ffi::TEXTURE_2D,
        0,
        0,
        0,
        size.w,
        size.h,
        ffi::RED,
        ffi::UNSIGNED_BYTE,
        data.as_ptr().cast(),
    );
    gl.BindTexture(ffi::TEXTURE_2D, 0);
}

/// A chrome buffer drawn with the palette shader. Its `draw` hands the
/// program to `render_texture_from_to`, so the GPU resolves indices to
/// colours; everything else (damage, opaque regions) mirrors a 1:1
/// unscaled texture element.
pub struct IndexedElement {
    id: Id,
    texture: GlesTexture,
    program: GlesTexProgram,
    /// The shared palette texture's GL id, bound on `PALETTE_UNIT` for the
    /// duration of `draw` (it outlives every element, owned by
    /// `IndexedProgram`).
    palette_tex: u32,
    commit: CommitCounter,
    loc: Point<i32, Physical>,
    size: Size<i32, Buffer>,
    opaque: bool,
    kind: Kind,
}

impl Element for IndexedElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.size.to_f64())
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::new(self.loc, Size::from((self.size.w, self.size.h)))
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.opaque {
            OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
        } else {
            OpaqueRegions::default()
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<GlesRenderer> for IndexedElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        // Bind the palette on its unit (leaving unit 0 active, where
        // `render_texture_from_to` binds the indexed texture); the shader's
        // `palette` sampler reads it there. Unbind afterwards so no stray
        // binding leaks into smithay's other draws.
        let palette_tex = self.palette_tex;
        frame.with_context(|gl| unsafe {
            gl.ActiveTexture(ffi::TEXTURE0 + PALETTE_UNIT as u32);
            gl.BindTexture(ffi::TEXTURE_2D, palette_tex);
            gl.ActiveTexture(ffi::TEXTURE0);
        })?;
        let result = frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
            Some(&self.program),
            &[Uniform::new("palette", PALETTE_UNIT)],
        );
        frame.with_context(|gl| unsafe {
            gl.ActiveTexture(ffi::TEXTURE0 + PALETTE_UNIT as u32);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
        })?;
        result
    }
}

/// The 256-entry palette as a row of premultiplied `RGBA` texels: index ->
/// colour, index `TRANSPARENT` (255) -> a transparent texel. The palette is
/// opaque, so opaque entries pass through and only the transparent slot is
/// zeroed.
fn palette_rgba() -> [u8; 256 * 4] {
    let palette = crate::assets::palette();
    let mut data = [0u8; 256 * 4];
    for (i, texel) in data.chunks_exact_mut(4).enumerate() {
        let index = i as u8;
        if index != pixel_graphics::TRANSPARENT {
            let c = palette.color(index);
            texel.copy_from_slice(&[c.r, c.g, c.b, 0xFF]);
        }
    }
    data
}

/// The palette fragment shader: the stock `texture.frag` shape (so smithay's
/// fixed `#version 100` vertex shader and its `//_DEFINES_` substitution both
/// apply). The indexed texture's `R8` red channel is the palette index,
/// recovered as a texel centre in the 256-wide `palette` lookup texture (see
/// the module docs on why the palette is a texture, not a `const` array).
/// Output is premultiplied, multiplied by `alpha` like the stock shader.
const SHADER_SOURCE: &str = "\
#version 100

//_DEFINES_

precision mediump float;
uniform sampler2D tex;
uniform sampler2D palette;
uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    float index = texture2D(tex, v_coords).r * 255.0;
    vec2 lookup = vec2((index + 0.5) / 256.0, 0.5);
    vec4 color = texture2D(palette, lookup) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
";
