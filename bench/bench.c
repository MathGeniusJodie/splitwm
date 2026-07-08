/* Standalone shader benchmark for splitwm's quantize pass: GBM + EGL
 * surfaceless context on the real render node (no compositor, no window),
 * fullscreen-triangle draws into an offscreen FBO, glFinish-bracketed
 * wall-clock timing per fragment-shader variant.
 * Build: gcc -O2 -o bench bench.c -lEGL -lGLESv2 -lgbm */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "pal.h"

#define W 1920
#define H 1080

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

static GLuint compile(GLenum kind, const char *src) {
    GLuint s = glCreateShader(kind);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096];
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "compile failed:\n%s\n", log);
        exit(1);
    }
    return s;
}

static const char *VS =
    "#version 300 es\n"
    "precision highp float;\n"
    "out vec2 v_tex;\n"
    "void main() {\n"
    "    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
    "    v_tex = p;\n"
    "    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";

/* Shared head: matches FRAGMENT_HEAD + tables + snap + jitter of
 * src/comp/quantize.rs (na16 uniform dropped; each variant bakes a main). */
static const char *HEAD =
    "#version 300 es\n"
    "precision highp float;\n"
    "precision highp int;\n"
    "uniform sampler2D scene;\n"
    "in vec2 v_tex;\n"
    "out vec4 frag;\n"
    "float dither256(uvec2 fragCoord){\n"
    "    uint x = fragCoord.x ^ fragCoord.y;\n"
    "    uint y = fragCoord.y;\n"
    "    uint z = x << 16 | y;\n"
    "    z |= z << 12;\n"
    "    z &= 0xF0F0F0F0u;\n"
    "    z |= z >> 6;\n"
    "    z &= 0x33333333u;\n"
    "    z |= z << 3;\n"
    "    z &= 0xaaaaaaaau;\n"
    "    z = z >> 9 | z << 6;\n"
    "    z &= 0x7fffffu;\n"
    "    return uintBitsToFloat(floatBitsToUint(1.) | z) - 1.0;\n"
    "}\n"
    PAL_TABLES
    "int snap_na16(vec3 c) {\n"
    "    vec4 w = vec4(c * vec3(0.4735504, 0.6635135, 0.2924038),\n"
    "                  dot(c, vec3(0.299, 0.587, 0.114)));\n"
    "    float best = 1e9;\n"
    "    int at = 0;\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        vec4 d = PAL_W[i] - w;\n"
    "        float ds = dot(d, d);\n"
    "        if (ds < best) { best = ds; at = i; }\n"
    "    }\n"
    "    return at;\n"
    "}\n"
    "int snap_reg(vec3 c, out vec3 bc) {\n"
    "    vec4 w = vec4(c * vec3(0.4735504, 0.6635135, 0.2924038),\n"
    "                  dot(c, vec3(0.299, 0.587, 0.114)));\n"
    "    float best = 1e9;\n"
    "    int at = 0;\n"
    "    vec3 b = PAL[0];\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        vec4 d = PAL_W[i] - w;\n"
    "        float ds = dot(d, d);\n"
    "        if (ds < best) { best = ds; at = i; b = PAL[i]; }\n"
    "    }\n"
    "    bc = b;\n"
    "    return at;\n"
    "}\n"
    "float bayer_jitter(float u) {\n"
    "    uint b = uint(fract(u) * 256.0) & 0xFFu;\n"
    "    b = (b >> 4 | b << 4) & 0xFFu;\n"
    "    b = ((b & 0xCCu) >> 2) | ((b & 0x33u) << 2);\n"
    "    b = ((b & 0xAAu) >> 1) | ((b & 0x55u) << 1);\n"
    "    return float(b) / 256.0;\n"
    "}\n"
    "vec3 rgb332(vec3 c, float t) {\n"
    "    vec3 levels = vec3(7.0, 7.0, 3.0);\n"
    "    vec3 d = clamp(c + (t - 0.5) / levels, 0.0, 1.0);\n"
    "    return round(d * levels) / levels;\n"
    "}\n";

/* Every variant's main is the real shader's main with the mode branch
 * resolved; KNOLL is the function under test. */
static const char *MAIN =
    "void main() {\n"
    "    vec3 c = texture(scene, v_tex).rgb;\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        if (all(lessThan(abs(c - PAL[i]), vec3(0.5 / 255.0)))) {\n"
    "            frag = vec4(PAL[i], 1.0);\n"
    "            return;\n"
    "        }\n"
    "    }\n"
    "    float t = dither256(uvec2(gl_FragCoord.xy));\n"
    "    frag = vec4(KNOLL(c, t), 1.0);\n"
    "}\n";

static const char *RGB332_MAIN =
    "void main() {\n"
    "    vec3 c = texture(scene, v_tex).rgb;\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        if (all(lessThan(abs(c - PAL[i]), vec3(0.5 / 255.0)))) {\n"
    "            frag = vec4(PAL[i], 1.0);\n"
    "            return;\n"
    "        }\n"
    "    }\n"
    "    float t = dither256(uvec2(gl_FragCoord.xy));\n"
    "    frag = vec4(rgb332(c, t), 1.0);\n"
    "}\n";

/* The shipped shader before the histogram rework: plan + luma local
 * arrays, insertion sort, runtime-subscripted throughout. */
static const char *OLD =
    "#define PLAN 32\n"
    "vec3 KNOLL(vec3 c, float t) {\n"
    "    int plan[PLAN];\n"
    "    float luma[PLAN];\n"
    "    vec3 err = vec3(0.0);\n"
    "    for (int i = 0; i < PLAN; i++) {\n"
    "        int p = snap_na16(c + err);\n"
    "        err += c - PAL[p];\n"
    "        float l = dot(PAL[p], vec3(0.299, 0.587, 0.114));\n"
    "        int j = i - 1;\n"
    "        for (; j >= 0 && luma[j] > l; j--) {\n"
    "            plan[j + 1] = plan[j];\n"
    "            luma[j + 1] = luma[j];\n"
    "        }\n"
    "        plan[j + 1] = p;\n"
    "        luma[j + 1] = l;\n"
    "    }\n"
    "    float u = t * float(PLAN);\n"
    "    return PAL[plan[min(int(u + bayer_jitter(u)), PLAN - 1)]];\n"
    "}\n";

/* The current shader: packed histogram, but still one runtime-indexed
 * PAL[p] read per plan step. */
static const char *HIST =
    "#define PLAN 32\n"
    "vec3 KNOLL(vec3 c, float t) {\n"
    "    uvec4 hist = uvec4(0u);\n"
    "    vec3 err = vec3(0.0);\n"
    "    for (int i = 0; i < PLAN; i++) {\n"
    "        int p = snap_na16(c + err);\n"
    "        err += c - PAL[p];\n"
    "        hist += uvec4(equal(ivec4(p >> 2), ivec4(0, 1, 2, 3)))\n"
    "                << uint((p & 3) << 3);\n"
    "    }\n"
    "    float u = t * float(PLAN);\n"
    "    int idx = min(int(u + bayer_jitter(u)), PLAN - 1);\n"
    "    for (int i = 0; i < PAL_N - 1; i++) {\n"
    "        idx -= int((hist[i >> 2] >> uint((i & 3) << 3)) & 0xFFu);\n"
    "        if (idx < 0) return PAL[i];\n"
    "    }\n"
    "    return PAL[PAL_N - 1];\n"
    "}\n";

/* Index-free: snap hands back the winning colour in registers, so after
 * unrolling no palette access has a runtime subscript anywhere. */
static const char *NOIDX =
    "#define PLAN 32\n"
    "vec3 KNOLL(vec3 c, float t) {\n"
    "    uvec4 hist = uvec4(0u);\n"
    "    vec3 err = vec3(0.0);\n"
    "    for (int i = 0; i < PLAN; i++) {\n"
    "        vec3 bc;\n"
    "        int p = snap_reg(c + err, bc);\n"
    "        err += c - bc;\n"
    "        hist += uvec4(equal(ivec4(p >> 2), ivec4(0, 1, 2, 3)))\n"
    "                << uint((p & 3) << 3);\n"
    "    }\n"
    "    float u = t * float(PLAN);\n"
    "    int idx = min(int(u + bayer_jitter(u)), PLAN - 1);\n"
    "    for (int i = 0; i < PAL_N - 1; i++) {\n"
    "        idx -= int((hist[i >> 2] >> uint((i & 3) << 3)) & 0xFFu);\n"
    "        if (idx < 0) return PAL[i];\n"
    "    }\n"
    "    return PAL[PAL_N - 1];\n"
    "}\n";

static const char *NOIDX16 =
    "#define PLAN 16\n"
    "vec3 KNOLL(vec3 c, float t) {\n"
    "    uvec4 hist = uvec4(0u);\n"
    "    vec3 err = vec3(0.0);\n"
    "    for (int i = 0; i < PLAN; i++) {\n"
    "        vec3 bc;\n"
    "        int p = snap_reg(c + err, bc);\n"
    "        err += c - bc;\n"
    "        hist += uvec4(equal(ivec4(p >> 2), ivec4(0, 1, 2, 3)))\n"
    "                << uint((p & 3) << 3);\n"
    "    }\n"
    "    float u = t * float(PLAN);\n"
    "    int idx = min(int(u + bayer_jitter(u)), PLAN - 1);\n"
    "    for (int i = 0; i < PAL_N - 1; i++) {\n"
    "        idx -= int((hist[i >> 2] >> uint((i & 3) << 3)) & 0xFFu);\n"
    "        if (idx < 0) return PAL[i];\n"
    "    }\n"
    "    return PAL[PAL_N - 1];\n"
    "}\n";


/* --- histogram-LUT variant: the plan precomputed on a 33^3 lattice --- */

#define LUT_N 33

static unsigned char *pixels;

static int snap_cpu(const float c[3]) {
    float w[4] = {c[0] * 0.4735504f, c[1] * 0.6635135f, c[2] * 0.2924038f,
                  0.299f * c[0] + 0.587f * c[1] + 0.114f * c[2]};
    float best = 1e9f;
    int at = 0;
    for (int i = 0; i < 16; i++) {
        float pw[4] = {PAL_BYTES[i][0] / 255.0f * 0.4735504f,
                       PAL_BYTES[i][1] / 255.0f * 0.6635135f,
                       PAL_BYTES[i][2] / 255.0f * 0.2924038f,
                       (0.299f * PAL_BYTES[i][0] + 0.587f * PAL_BYTES[i][1] +
                        0.114f * PAL_BYTES[i][2]) / 255.0f};
        float ds = 0;
        for (int k = 0; k < 4; k++) ds += (pw[k] - w[k]) * (pw[k] - w[k]);
        if (ds < best) { best = ds; at = i; }
    }
    return at;
}

static unsigned *make_lut(void) {
    unsigned *lut = malloc((size_t)LUT_N * LUT_N * LUT_N * 4 * sizeof(unsigned));
    for (int b = 0; b < LUT_N; b++)
        for (int g = 0; g < LUT_N; g++)
            for (int r = 0; r < LUT_N; r++) {
                float c[3] = {r / 32.0f, g / 32.0f, b / 32.0f};
                float err[3] = {0, 0, 0};
                unsigned hist[4] = {0, 0, 0, 0};
                for (int i = 0; i < 32; i++) {
                    float ce[3] = {c[0] + err[0], c[1] + err[1], c[2] + err[2]};
                    int p = snap_cpu(ce);
                    for (int k = 0; k < 3; k++)
                        err[k] += c[k] - PAL_BYTES[p][k] / 255.0f;
                    hist[p >> 2] += 1u << ((p & 3) << 3);
                }
                unsigned *at = lut + ((size_t)(b * LUT_N + g) * LUT_N + r) * 4;
                memcpy(at, hist, sizeof hist);
            }
    return lut;
}

static const char *LUT_MAIN =
    "uniform highp usampler3D lut;\n"
    "void main() {\n"
    "    vec3 c = texture(scene, v_tex).rgb;\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        if (all(lessThan(abs(c - PAL[i]), vec3(0.5 / 255.0)))) {\n"
    "            frag = vec4(PAL[i], 1.0);\n"
    "            return;\n"
    "        }\n"
    "    }\n"
    "    uvec2 fc = uvec2(gl_FragCoord.xy);\n"
    "    float t = dither256(fc);\n"
    "    vec3 j = vec3(dither256(fc + uvec2(53u, 17u)),\n"
    "                  dither256(fc + uvec2(23u, 71u)),\n"
    "                  dither256(fc + uvec2(89u, 43u)));\n"
    "    ivec3 cell = clamp(ivec3(c * 32.0 + j), ivec3(0), ivec3(32));\n"
    "    uvec4 hist = texelFetch(lut, cell, 0);\n"
    "    float u = t * 32.0;\n"
    "    int idx = min(int(u + bayer_jitter(u)), 31);\n"
    "    vec3 col = PAL[0];\n"
    "    for (int i = 1; i < PAL_N; i++) {\n"
    "        idx -= int((hist[(i - 1) >> 2] >> uint(((i - 1) & 3) << 3)) & 0xFFu);\n"
    "        if (idx >= 0) col = PAL[i];\n"
    "    }\n"
    "    frag = vec4(col, 1.0);\n"
    "}\n";

static void dump_ppm(const char *path) {
    FILE *f = fopen(path, "w");
    fprintf(f, "P6 %d %d 255\n", W, H);
    for (int i = 0; i < W * H; i++)
        fwrite(pixels + i * 4, 1, 3, f);
    fclose(f);
}


/* --- CDF-LUT variant: cumulative plan counts in four filterable RGBA16F
 * 3D textures; hardware trilinear blends neighbouring cells' plans, so the
 * threshold compares against a smoothly varying CDF -- no cell seams, no
 * slot flooring, no jitter. --- */

static const char *CDF_MAIN =
    "uniform highp sampler3D lutA;\n"
    "uniform highp sampler3D lutB;\n"
    "uniform highp sampler3D lutC;\n"
    "uniform highp sampler3D lutD;\n"
    "void main() {\n"
    "    vec3 c = texture(scene, v_tex).rgb;\n"
    "    for (int i = 0; i < PAL_N; i++) {\n"
    "        if (all(lessThan(abs(c - PAL[i]), vec3(0.5 / 255.0)))) {\n"
    "            frag = vec4(PAL[i], 1.0);\n"
    "            return;\n"
    "        }\n"
    "    }\n"
    "    float u = dither256(uvec2(gl_FragCoord.xy)) * 32.0;\n"
    "    vec3 tc = (c * 32.0 + 0.5) / 33.0;\n"
    "    vec4 A = texture(lutA, tc);\n"
    "    vec4 B = texture(lutB, tc);\n"
    "    vec4 C = texture(lutC, tc);\n"
    "    vec4 D = texture(lutD, tc);\n"
    "    vec3 col = PAL[0];\n"
    "    if (u >= A.x) col = PAL[1];\n"
    "    if (u >= A.y) col = PAL[2];\n"
    "    if (u >= A.z) col = PAL[3];\n"
    "    if (u >= A.w) col = PAL[4];\n"
    "    if (u >= B.x) col = PAL[5];\n"
    "    if (u >= B.y) col = PAL[6];\n"
    "    if (u >= B.z) col = PAL[7];\n"
    "    if (u >= B.w) col = PAL[8];\n"
    "    if (u >= C.x) col = PAL[9];\n"
    "    if (u >= C.y) col = PAL[10];\n"
    "    if (u >= C.z) col = PAL[11];\n"
    "    if (u >= C.w) col = PAL[12];\n"
    "    if (u >= D.x) col = PAL[13];\n"
    "    if (u >= D.y) col = PAL[14];\n"
    "    if (u >= D.z) col = PAL[15];\n"
    "    frag = vec4(col, 1.0);\n"
    "}\n";

/* Split the packed-count LUT into four planes of cumulative counts. */
static float *make_cdf(const unsigned *lut, int plane) {
    size_t n = (size_t)LUT_N * LUT_N * LUT_N;
    float *out = malloc(n * 4 * sizeof(float));
    for (size_t v = 0; v < n; v++) {
        float cdf = 0;
        for (int i = 0; i < 16; i++) {
            cdf += (lut[v * 4 + (i >> 2)] >> ((i & 3) << 3)) & 0xFFu;
            if (i >= plane * 4 && i < plane * 4 + 4)
                out[v * 4 + (i & 3)] = cdf;
        }
    }
    return out;
}

static GLuint program(const char *knoll, const char *main_src) {
    char src[16384];
    snprintf(src, sizeof src, "%s%s%s", HEAD, knoll ? knoll : "", main_src);
    GLuint p = glCreateProgram();
    glAttachShader(p, compile(GL_VERTEX_SHADER, VS));
    glAttachShader(p, compile(GL_FRAGMENT_SHADER, src));
    glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        fprintf(stderr, "link failed\n");
        exit(1);
    }
    return p;
}

static unsigned char *pixels;

static void bench(const char *name, GLuint prog) {
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "scene"), 0);
    for (int i = 0; i < 3; i++)
        glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();
    /* Size the batch off one timed frame so slow variants still finish. */
    double t0 = now_ms();
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();
    double one = now_ms() - t0;
    int n = one > 0.01 ? (int)(1000.0 / one) : 1000;
    if (n < 3) n = 3;
    if (n > 300) n = 300;
    t0 = now_ms();
    for (int i = 0; i < n; i++)
        glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();
    double per = (now_ms() - t0) / n;
    /* Verify the output really is 16-colour (and defeat dead-code elim). */
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    long off = 0;
    for (int i = 0; i < W * H; i++) {
        const unsigned char *px = pixels + i * 4;
        int hit = 0;
        for (int j = 0; j < 16 && !hit; j++)
            hit = px[0] == PAL_BYTES[j][0] && px[1] == PAL_BYTES[j][1] &&
                  px[2] == PAL_BYTES[j][2];
        off += !hit;
    }
    printf("%-28s %8.3f ms/frame  (%6.1f fps)  off-palette px: %ld\n",
           name, per, 1000.0 / per, off);
}

int main(void) {
    int fd = open("/dev/dri/renderD128", O_RDWR);
    if (fd < 0) { perror("renderD128"); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    eglInitialize(dpy, NULL, NULL);
    eglBindAPI(EGL_OPENGL_ES_API);
    static const EGLint ctx_attr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attr);
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));
    printf("resolution:  %dx%d\n\n", W, H);

    /* Source texture: a smooth colour gradient, nothing on the palette,
     * so every fragment takes the dither path. */
    unsigned char *img = malloc(W * H * 4);
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned char *px = img + (y * W + x) * 4;
            px[0] = x * 255 / W;
            px[1] = y * 255 / H;
            px[2] = 255 - (x + y) * 255 / (W + H);
            px[3] = 255;
        }
    GLuint scene;
    glGenTextures(1, &scene);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, scene);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, img);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    free(img);

    GLuint fbo, target;
    glGenFramebuffers(1, &fbo);
    glGenTextures(1, &target);
    glBindTexture(GL_TEXTURE_2D, target);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glBindTexture(GL_TEXTURE_2D, scene);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
    glViewport(0, 0, W, H);
    glDisable(GL_BLEND);
    pixels = malloc(W * H * 4);

    bench("rgb332 (floor)", program(NULL, RGB332_MAIN));
    bench("knoll old (insertion sort)", program(OLD, MAIN));
    bench("knoll hist (current)", program(HIST, MAIN));
    bench("knoll no-index", program(NOIDX, MAIN));
    bench("knoll no-index PLAN=16", program(NOIDX16, MAIN));

    bench("knoll hist (current)", program(HIST, MAIN));
    dump_ppm("out-hist.ppm");
    double t0 = now_ms();
    unsigned *lut = make_lut();
    printf("lut generation (cpu):        %8.3f ms one-time\n", now_ms() - t0);
    GLuint lut_tex;
    glGenTextures(1, &lut_tex);
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_3D, lut_tex);
    glTexImage3D(GL_TEXTURE_3D, 0, GL_RGBA32UI, LUT_N, LUT_N, LUT_N, 0,
                 GL_RGBA_INTEGER, GL_UNSIGNED_INT, lut);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glActiveTexture(GL_TEXTURE0);
    GLuint lp = program(NULL, LUT_MAIN);
    glUseProgram(lp);
    glUniform1i(glGetUniformLocation(lp, "lut"), 1);
    bench("knoll hist-lut 33^3", lp);
    dump_ppm("out-lut.ppm");

    GLuint cdf_tex[4];
    glGenTextures(4, cdf_tex);
    for (int plane = 0; plane < 4; plane++) {
        float *cdf = make_cdf(lut, plane);
        glActiveTexture(GL_TEXTURE2 + plane);
        glBindTexture(GL_TEXTURE_3D, cdf_tex[plane]);
        glTexImage3D(GL_TEXTURE_3D, 0, GL_RGBA16F, LUT_N, LUT_N, LUT_N, 0,
                     GL_RGBA, GL_FLOAT, cdf);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_R, GL_CLAMP_TO_EDGE);
        free(cdf);
    }
    glActiveTexture(GL_TEXTURE0);
    GLuint cp = program(NULL, CDF_MAIN);
    glUseProgram(cp);
    glUniform1i(glGetUniformLocation(cp, "lutA"), 2);
    glUniform1i(glGetUniformLocation(cp, "lutB"), 3);
    glUniform1i(glGetUniformLocation(cp, "lutC"), 4);
    glUniform1i(glGetUniformLocation(cp, "lutD"), 5);
    bench("knoll cdf-lut trilinear", cp);
    dump_ppm("out-cdf.ppm");
    return 0;
}
