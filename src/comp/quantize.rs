//! The colour-depth post-pass behind `Action::CycleColorMode`: the whole
//! composited frame quantized to a 256-colour (RGB332) or 24-colour
//! (palette) lattice with ordered dithering, as a GPU pass over a normal
//! true-colour
//! buffer — the retro look of a C8 framebuffer without asking KMS for an
//! indexed scanout format no modern driver honours.
//!
//! When a quantized mode is active the frame renders in two passes: the
//! scene's elements composite damage-tracked into a persistent offscreen
//! texture (always `Transform::Normal`; the output transform belongs to the
//! second pass), and the backend's real target draws a single fullscreen
//! [`QuantizeElement`] instead. That element samples the scene texture and
//! ordered-dithers each pixel against Jodie's analytic Bayer threshold
//! (`dither256`) with the classic lattice dither for a bit-partitioned
//! palette — wobble each channel by one lattice step and round it to the
//! lattice — at the mode's channel level counts: 8-8-4 for RGB332, 3-4-2
//! for the palette mode. The dither falls out of the lattice geometry with
//! no tuned constants, and a pixel already exactly on an na16 colour passes
//! through untouched in both modes — the WM's chrome and wallpaper are na16
//! art, and re-dithering finished pixel art would dissolve it into moire
//! noise. In the palette mode the lattice point is only an address: a 3x4x2
//! 3D LUT ([`PALETTE_LUT`]) remaps each of the 24 lattice points to its
//! assigned palette colour — the 16 na16 colours plus 8 saturated brights —
//! so the mode lands on the WM's palette instead of the raw lattice
//! primaries.
//! Scene damage flows through a [`DamageBag`] into the element's
//! `damage_since`, so partial redraws survive the indirection.
//!
//! smithay's custom texture shaders are pinned to GLSL ES 1.00 (its fixed
//! vertex shader), and the dither needs integer bit-twiddling, so this pass
//! owns a raw GLSL ES 3.00 program and draws with it inside
//! `GlesFrame::with_context`, projecting vertices through the frame's own
//! matrix. The na16 palette is baked into the fragment source as constant
//! tables at compile time. The GL objects compile lazily on the first
//! quantized frame; a GLES2-only context fails that compile once, logs, and
//! leaves the toggle inert.

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement};
use smithay::backend::renderer::gles::{ffi, GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{
    CommitCounter, DamageBag, DamageSet, DamageSnapshot, OpaqueRegions,
};
use smithay::backend::renderer::{Bind as _, Color32F, Offscreen as _};
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Size, Transform};

use pixel_graphics::Rgb;

use super::chrome::OutputElement;

/// The three output colour depths `Action::CycleColorMode` cycles through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// Pass-through: the scene's elements render straight to the target.
    True,
    /// The 256-colour RGB332 lattice, ordered-dithered per channel (plus
    /// exact na16 pixels passing through for the chrome).
    Rgb332,
    /// The 24-colour mode: the same dither on a 3x4x2 lattice, then each
    /// lattice point remapped to its assigned palette colour (na16 plus
    /// eight saturated brights) through [`PALETTE_LUT`].
    Palette,
}

impl ColorMode {
    fn next(self) -> ColorMode {
        match self {
            ColorMode::True => ColorMode::Rgb332,
            ColorMode::Rgb332 => ColorMode::Palette,
            ColorMode::Palette => ColorMode::True,
        }
    }

    /// The dither lattice's per-channel level counts minus one (the
    /// `levels` uniform); `None` in pass-through mode.
    fn levels(self) -> Option<[f32; 3]> {
        match self {
            ColorMode::True => None,
            ColorMode::Rgb332 => Some([7.0, 7.0, 3.0]),
            ColorMode::Palette => Some([2.0, 3.0, 1.0]),
        }
    }
}

/// The post-pass' state: the selected mode plus lazily-created GL objects.
/// Lives in `Comp`; every backend routes its element list through [`wrap`]
/// (`Quantize::wrap`).
pub struct Quantize {
    mode: ColorMode,
    /// GL program/LUTs/scene buffer, created on the first quantized frame.
    gpu: Option<Gpu>,
    /// The ESSL 3.00 compile failed (a GLES2-only context): quantized modes
    /// are unavailable and `cycle` no-ops with a log.
    unsupported: bool,
}

/// The compiled program with its uniform locations and the vertex plumbing.
/// One per renderer, same lifetime class as `IndexedProgram`.
struct Gpu {
    program: u32,
    u_proj: i32,
    u_size: i32,
    u_levels: i32,
    u_remap: i32,
    /// The lattice→palette remap as a 3x4x2 3D texture (see
    /// [`PALETTE_LUT`]).
    lut: u32,
    vao: u32,
    vbo: u32,
    scene: Option<SceneBuffer>,
}

/// The offscreen texture pass A composites into, with the damage tracker
/// that keeps its repaints partial and the identity the outer damage
/// tracker follows the fullscreen element by.
struct SceneBuffer {
    texture: GlesTexture,
    tracker: OutputDamageTracker,
    size: Size<i32, Physical>,
    id: Id,
    /// Scene damage per frame, replayed to the outer damage tracker through
    /// the element's `damage_since`.
    damage: DamageBag<i32, Physical>,
    /// Whether the texture already holds a rendered frame (buffer age 1 for
    /// the tracker; 0 forces the initial full paint).
    rendered: bool,
}

impl Quantize {
    pub fn new() -> Quantize {
        Quantize {
            mode: ColorMode::True,
            gpu: None,
            unsupported: false,
        }
    }

    /// Step to the next mode (`Mod4+C`). The redraw that follows sees the
    /// new mode in `wrap`; the damage reset repaints the whole output even
    /// between the two quantized modes, where the element is otherwise
    /// unchanged.
    pub fn cycle(&mut self) {
        if self.unsupported {
            tracing::warn!("color mode cycle ignored: quantize shader unavailable");
            return;
        }
        self.mode = self.mode.next();
        tracing::info!("color mode: {:?}", self.mode);
        if let Some(scene) = self.gpu.as_mut().and_then(|gpu| gpu.scene.as_mut()) {
            scene.damage.reset();
        }
    }

    /// Drop the GL objects (VT re-activation may have lost texture
    /// contents); everything is rebuilt on the next quantized frame.
    #[cfg_attr(not(feature = "tty"), allow(dead_code))]
    pub fn invalidate(&mut self) {
        self.gpu = None;
    }

    /// Route one frame's elements through the post-pass. In `True` mode (or
    /// when the shader can't compile) the elements pass through untouched;
    /// otherwise they composite into the scene texture and the caller
    /// receives the single fullscreen quantize element to draw instead.
    pub fn wrap(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: Vec<OutputElement>,
        size: Size<i32, Physical>,
        clear: Color32F,
    ) -> Vec<OutputElement> {
        let Some(levels) = self.mode.levels() else {
            return elements;
        };
        if self.unsupported {
            return elements;
        }
        if self.gpu.is_none() {
            match Gpu::new(renderer) {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(err) => {
                    tracing::warn!("quantize shader unavailable, staying true-colour: {err}");
                    self.unsupported = true;
                    self.mode = ColorMode::True;
                    return elements;
                }
            }
        }
        let gpu = self.gpu.as_mut().expect("gpu just ensured");
        if let Err(err) = gpu.render_scene(renderer, &elements, size, clear) {
            tracing::error!("quantize scene pass: {err}");
            return elements;
        }
        let scene = gpu.scene.as_ref().expect("scene just rendered");
        vec![OutputElement::Quantize(QuantizeElement {
            id: scene.id.clone(),
            texture: scene.texture.clone(),
            damage: scene.damage.snapshot(),
            size: scene.size,
            program: gpu.program,
            u_proj: gpu.u_proj,
            u_size: gpu.u_size,
            u_levels: gpu.u_levels,
            u_remap: gpu.u_remap,
            lut: gpu.lut,
            vao: gpu.vao,
            vbo: gpu.vbo,
            levels,
            remap: self.mode == ColorMode::Palette,
        })]
    }
}

impl Gpu {
    fn new(renderer: &mut GlesRenderer) -> Result<Gpu, String> {
        let fragment = fragment_source();
        renderer
            .with_context(|gl| unsafe {
                let program = link_program(gl, &fragment)?;
                let name = |s: &str| {
                    let c = std::ffi::CString::new(s).expect("static uniform name");
                    gl.GetUniformLocation(program, c.as_ptr())
                };
                let (u_proj, u_size, u_levels, u_remap) =
                    (name("proj"), name("size"), name("levels"), name("remap"));
                // The sampler units never change; set them once.
                gl.UseProgram(program);
                gl.Uniform1i(name("scene"), 0);
                gl.Uniform1i(name("lut"), 1);
                gl.UseProgram(0);

                // The lattice→palette remap texture. NEAREST +
                // CLAMP_TO_EDGE: the shader samples it at exact lattice
                // colours, and 1.0 must land on the last texel, not wrap
                // back to the first.
                let texels = lut_texels();
                let mut lut = 0;
                gl.GenTextures(1, &mut lut);
                gl.BindTexture(ffi::TEXTURE_3D, lut);
                gl.TexImage3D(
                    ffi::TEXTURE_3D,
                    0,
                    ffi::RGBA8 as i32,
                    3,
                    4,
                    2,
                    0,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    texels.as_ptr().cast(),
                );
                for param in [ffi::TEXTURE_MIN_FILTER, ffi::TEXTURE_MAG_FILTER] {
                    gl.TexParameteri(ffi::TEXTURE_3D, param, ffi::NEAREST as i32);
                }
                for param in [
                    ffi::TEXTURE_WRAP_S,
                    ffi::TEXTURE_WRAP_T,
                    ffi::TEXTURE_WRAP_R,
                ] {
                    gl.TexParameteri(ffi::TEXTURE_3D, param, ffi::CLAMP_TO_EDGE as i32);
                }
                gl.BindTexture(ffi::TEXTURE_3D, 0);

                // One VAO owning the sole attribute (per-vertex vec2
                // positions from `vbo`), bound only while this pass draws
                // so smithay's own attribute state is never disturbed.
                let (mut vao, mut vbo) = (0, 0);
                gl.GenVertexArrays(1, &mut vao);
                gl.GenBuffers(1, &mut vbo);
                gl.BindVertexArray(vao);
                gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
                gl.EnableVertexAttribArray(0);
                gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE, 0, std::ptr::null());
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                gl.BindVertexArray(0);
                Ok::<_, String>(Gpu {
                    program,
                    u_proj,
                    u_size,
                    u_levels,
                    u_remap,
                    lut,
                    vao,
                    vbo,
                    scene: None,
                })
            })
            .map_err(|err| format!("gl context: {err}"))?
    }

    /// Pass A: composite `elements` into the scene texture, tracking damage.
    fn render_scene(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[OutputElement],
        size: Size<i32, Physical>,
        clear: Color32F,
    ) -> Result<(), String> {
        if self.scene.as_ref().is_none_or(|s| s.size != size) {
            let texture: GlesTexture = renderer
                .create_buffer(
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    size.to_logical(1).to_buffer(1, Transform::Normal),
                )
                .map_err(|err| format!("scene texture: {err}"))?;
            // 1:1 sampling; NEAREST keeps the quantize exact at the cost of
            // nothing.
            let tex_id = texture.tex_id();
            renderer
                .with_context(|gl| unsafe {
                    gl.BindTexture(ffi::TEXTURE_2D, tex_id);
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
                    gl.BindTexture(ffi::TEXTURE_2D, 0);
                })
                .map_err(|err| format!("scene texture params: {err}"))?;
            self.scene = Some(SceneBuffer {
                texture,
                // Always `Normal`: the second pass applies the output
                // transform when it projects the fullscreen quad.
                tracker: OutputDamageTracker::new(size, 1.0, Transform::Normal),
                size,
                id: Id::new(),
                damage: DamageBag::default(),
                rendered: false,
            });
        }
        let scene = self.scene.as_mut().expect("scene just ensured");
        let mut fb = renderer
            .bind(&mut scene.texture)
            .map_err(|err| format!("bind scene texture: {err}"))?;
        let age = usize::from(scene.rendered);
        let res = scene
            .tracker
            .render_output(renderer, &mut fb, age, elements, clear)
            .map_err(|err| format!("render: {err:?}"))?;
        scene.rendered = true;
        if let Some(damage) = res.damage {
            scene.damage.add(damage.iter().copied());
        }
        Ok(())
    }
}

/// The fullscreen element drawing the quantized scene: the one thing the
/// backend's real target renders while a quantized mode is active. Damage,
/// commit and opacity mirror the scene texture; `draw` is the raw ESSL 3.00
/// pass.
pub struct QuantizeElement {
    id: Id,
    texture: GlesTexture,
    damage: DamageSnapshot<i32, Physical>,
    size: Size<i32, Physical>,
    program: u32,
    u_proj: i32,
    u_size: i32,
    u_levels: i32,
    u_remap: i32,
    lut: u32,
    vao: u32,
    vbo: u32,
    /// The `levels` uniform: the mode's per-channel lattice steps.
    levels: [f32; 3],
    /// The `remap` uniform: route lattice colours through the palette LUT
    /// (palette mode only).
    remap: bool,
}

impl Element for QuantizeElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.damage.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((f64::from(self.size.w), f64::from(self.size.h))))
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::from_size(self.size)
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.damage
            .damage_since(commit)
            .unwrap_or_else(|| DamageSet::from_slice(&[Rectangle::from_size(self.size)]))
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&[Rectangle::from_size(self.size)])
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for QuantizeElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        // Two triangles per damage rect, in the same physical coordinates
        // every element's dst uses; the frame's projection maps them to
        // clip space (and applies the output transform).
        let mut verts: Vec<f32> = Vec::with_capacity(damage.len() * 12);
        for rect in damage {
            let x0 = (dst.loc.x + rect.loc.x) as f32;
            let y0 = (dst.loc.y + rect.loc.y) as f32;
            let x1 = x0 + rect.size.w as f32;
            let y1 = y0 + rect.size.h as f32;
            verts.extend_from_slice(&[x0, y0, x1, y0, x1, y1, x0, y0, x1, y1, x0, y1]);
        }
        if verts.is_empty() {
            return Ok(());
        }
        let proj = *frame.projection();
        let scene_tex = self.texture.tex_id();
        frame.with_context(|gl| unsafe {
            // Save the state this pass touches; every other piece of GL
            // state is (re)set per draw by smithay itself.
            let mut prev_program = 0;
            gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut prev_program);
            let mut prev_vao = 0;
            gl.GetIntegerv(ffi::VERTEX_ARRAY_BINDING, &mut prev_vao);
            let mut prev_vbo = 0;
            gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut prev_vbo);
            let blend = gl.IsEnabled(ffi::BLEND) == ffi::TRUE;

            gl.UseProgram(self.program);
            gl.UniformMatrix3fv(self.u_proj, 1, ffi::FALSE, proj.as_ptr());
            gl.Uniform2f(self.u_size, self.size.w as f32, self.size.h as f32);
            gl.Uniform3f(self.u_levels, self.levels[0], self.levels[1], self.levels[2]);
            gl.Uniform1i(self.u_remap, i32::from(self.remap));
            gl.ActiveTexture(ffi::TEXTURE1);
            gl.BindTexture(ffi::TEXTURE_3D, self.lut);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, scene_tex);
            gl.BindVertexArray(self.vao);
            gl.BindBuffer(ffi::ARRAY_BUFFER, self.vbo);
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                std::mem::size_of_val(verts.as_slice()) as isize,
                verts.as_ptr().cast(),
                ffi::STREAM_DRAW,
            );
            if blend {
                gl.Disable(ffi::BLEND);
            }
            gl.DrawArrays(ffi::TRIANGLES, 0, (verts.len() / 2) as i32);
            if blend {
                gl.Enable(ffi::BLEND);
            }
            gl.BindBuffer(ffi::ARRAY_BUFFER, prev_vbo as u32);
            gl.BindVertexArray(prev_vao as u32);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.ActiveTexture(ffi::TEXTURE1);
            gl.BindTexture(ffi::TEXTURE_3D, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.UseProgram(prev_program as u32);
        })
    }
}

// --- the baked palette ---

/// The na16 palette as a plain colour list.
fn na16_colors() -> Vec<Rgb> {
    let palette = crate::assets::palette();
    (0..palette.len()).map(|i| palette.color(i as u8)).collect()
}

/// The lattice→palette remap: the palette colour standing in for each of
/// the 24 lattice points, indexed `[r][g][b]` at the mode's channel levels
/// (r ∈ {0, ½, 1}; g ∈ {0, ⅓, ⅔, 1}; b ∈ {0, 1} of full scale). A superset
/// of the na16 palette (asserted in tests): every na16 colour is reachable
/// for the chrome, and the eight extra slots hold saturated brights the
/// na16 gamut lacks. Spelled as colours because the baked palette's index
/// order is an artifact of the asset build.
const PALETTE_LUT: [[[u32; 2]; 4]; 3] = [
    [
        [0x1f0e1c, 0x000080],
        [0x004000, 0x17434b],
        [0x008000, 0x34859d],
        [0x00bf00, 0x00bf80],
    ],
    [
        [0x3e2137, 0x584563],
        [0x9d303b, 0x70377f],
        [0x647d34, 0x8c8fae],
        [0x80bf00, 0x7ec4c1],
    ],
    [
        [0xff0000, 0xff0080],
        [0x9a6348, 0xd26471],
        [0xe4943a, 0xd79b7d],
        [0xc0c741, 0xf5edba],
    ],
];

/// [`PALETTE_LUT`] as 3x4x2 RGBA8 texels — width R, height G, depth B, so
/// `texture(lut, q)` addresses it with the lattice colour directly.
fn lut_texels() -> [u8; 3 * 4 * 2 * 4] {
    let mut data = [0; 3 * 4 * 2 * 4];
    for (i, texel) in data.chunks_exact_mut(4).enumerate() {
        let (r, g, b) = (i % 3, (i / 3) % 4, i / 12);
        let rgb = PALETTE_LUT[r][g][b];
        texel.copy_from_slice(&[(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 0xff]);
    }
    data
}

// --- the ESSL 3.00 program ---

const VERTEX_SOURCE: &str = "\
#version 300 es
precision highp float;
uniform mat3 proj;
uniform vec2 size;
layout(location = 0) in vec2 pos;
out vec2 v_tex;

void main() {
    v_tex = pos / size;
    gl_Position = vec4(proj * vec3(pos, 1.0), 1.0);
}
";

/// The fragment source up to the baked palette table (see
/// [`fragment_source`]). `dither256` is Jodie's analytic Bayer threshold:
/// bit-interleaving the (xor-folded) fragment coordinates into a float's
/// mantissa yields the classic recursive Bayer matrix at 256x256 without a
/// lookup table, in [0, 1). Position-stable, so static content never
/// shimmers.
const FRAGMENT_HEAD: &str = "\
#version 300 es
precision highp float;
precision highp int;
uniform sampler2D scene;
// The mode's per-channel lattice steps (levels minus one): RGB332 is
// vec3(7, 7, 3), the palette mode is vec3(2, 3, 1).
uniform vec3 levels;
// The lattice→palette remap (3x4x2, NEAREST): the palette colour assigned
// to each lattice point, addressed by the lattice colour itself.
uniform lowp sampler3D lut;
// Route lattice colours through the remap (palette mode only).
uniform bool remap;
in vec2 v_tex;
out vec4 frag;

float dither256(uvec2 fragCoord){
    uint x = fragCoord.x ^ fragCoord.y;
    uint y = fragCoord.y;
    uint z = x << 16 | y;
    z |= z << 12;
    z &= 0xF0F0F0F0u;
    z |= z >> 6;
    z &= 0x33333333u;
    z |= z << 3;
    z &= 0xaaaaaaaau;
    z = z >> 9 | z << 6;
    z &= 0x7fffffu;
    return uintBitsToFloat(
        floatBitsToUint(1.) | z
    ) - 1.0;
}
";

/// The fragment source after the baked palette table.
const FRAGMENT_BODY: &str = "\
vec3 softplus(vec3 z) { return max(z, vec3(0.0)) + log(1.0 + exp(-abs(z))); }

// Smooth double-sided clamp: softplus at each bound (easing over ~1/k of
// range), so out-of-range colours ease into the range instead of
// flattening against a wall. Currently uncalled.
vec3 softClamp(vec3 x, float lo, float hi, float k) {
    vec3 f = lo + softplus(k * (x - lo)) / k;
    return hi - softplus(k * (hi - f)) / k;
}

// The classic ordered dither for a bit-partitioned palette: wobble each
// channel by one lattice step around the pixel and round to the lattice —
// per-channel, analytic, exact. (The chrome-passthrough colours are the 16
// na16 entries handled in main; the lattice itself needs no substitution.)
vec3 lattice(vec3 c, float t) {
    vec3 d = clamp(c + (t - 0.5) / levels, 0.0, 1.0);
    return round(d * levels) / levels;
}

// A pixel exactly on an na16 colour short-circuits untouched in both modes
// — the WM's chrome and wallpaper are na16 art, and re-dithering finished
// pixel art would dissolve it into moire noise.
void main() {
    vec3 c = texture(scene, v_tex).rgb;
    for (int i = 0; i < PAL_N; i++) {
        if (all(lessThan(abs(c - PAL[i]), vec3(0.5 / 255.0)))) {
            frag = vec4(PAL[i], 1.0);
            return;
        }
    }
    float t = dither256(uvec2(gl_FragCoord.xy));
    vec3 q = lattice(c, t);
    // In the palette mode the lattice point is only an address: the LUT
    // hands back the palette colour assigned to it.
    if (remap) q = texture(lut, q).rgb;
    frag = vec4(q, 1.0);
}
";

/// The full fragment source: [`FRAGMENT_HEAD`], then the na16 palette baked
/// as a constant table (the chrome-passthrough colours), then the body.
fn fragment_source() -> String {
    let na16 = na16_colors();
    let srgb: Vec<String> = na16
        .iter()
        .map(|c| {
            format!(
                "vec3({:.9}, {:.9}, {:.9})",
                f32::from(c.r) / 255.0,
                f32::from(c.g) / 255.0,
                f32::from(c.b) / 255.0
            )
        })
        .collect();
    let n = na16.len();
    format!(
        "{FRAGMENT_HEAD}\nconst int PAL_N = {n};\nconst vec3 PAL[{n}] = vec3[{n}](\n    {}\n);\n\n{FRAGMENT_BODY}",
        srgb.join(",\n    ")
    )
}

/// Compile and link the pass' program, mapping GL's info logs into the
/// error string (this is where a GLES2-only context bails out).
unsafe fn link_program(gl: &ffi::Gles2, fragment: &str) -> Result<u32, String> {
    let shader = |kind: u32, source: &str| -> Result<u32, String> {
        let shader = gl.CreateShader(kind);
        gl.ShaderSource(shader, 1, &source.as_ptr().cast(), &(source.len() as i32));
        gl.CompileShader(shader);
        let mut ok = 0;
        gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut ok);
        if ok == i32::from(ffi::TRUE) {
            return Ok(shader);
        }
        let log = info_log(|len, out| gl.GetShaderInfoLog(shader, 1024, len, out));
        gl.DeleteShader(shader);
        Err(format!("shader compile: {log}"))
    };
    let vert = shader(ffi::VERTEX_SHADER, VERTEX_SOURCE)?;
    let frag = shader(ffi::FRAGMENT_SHADER, fragment).inspect_err(|_| gl.DeleteShader(vert))?;
    let program = gl.CreateProgram();
    gl.AttachShader(program, vert);
    gl.AttachShader(program, frag);
    gl.LinkProgram(program);
    // Flagged for deletion now, they die with the program.
    gl.DetachShader(program, vert);
    gl.DetachShader(program, frag);
    gl.DeleteShader(vert);
    gl.DeleteShader(frag);
    let mut ok = 0;
    gl.GetProgramiv(program, ffi::LINK_STATUS, &mut ok);
    if ok == i32::from(ffi::TRUE) {
        return Ok(program);
    }
    let log = info_log(|len, out| gl.GetProgramInfoLog(program, 1024, len, out));
    gl.DeleteProgram(program);
    Err(format!("program link: {log}"))
}

fn info_log(get: impl FnOnce(*mut i32, *mut i8)) -> String {
    let mut buf = vec![0u8; 1024];
    let mut len = 0;
    get(&mut len, buf.as_mut_ptr().cast());
    buf.truncate(len.max(0) as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{na16_colors, PALETTE_LUT};

    /// The remap must contain every na16 colour (the chrome passthrough
    /// and the lattice must agree on the WM's own palette) and waste no
    /// slots on duplicates.
    #[test]
    fn palette_lut_covers_na16_without_duplicates() {
        let remap: Vec<u32> = PALETTE_LUT.iter().flatten().flatten().copied().collect();
        let unique: std::collections::HashSet<u32> = remap.iter().copied().collect();
        assert_eq!(unique.len(), remap.len());
        for c in na16_colors() {
            let rgb = u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b);
            assert!(unique.contains(&rgb), "na16 colour {rgb:06x} missing");
        }
    }
}
