//! The ex-underlay, split into independently-textured pieces so scrolling
//! and layout animation stay pure GPU element placement.
//!
//! The chrome that renders behind the client windows — the wallpaper, every
//! placed leaf's frame and the bottom taskbar — is
//! not one full-output framebuffer but a set of separately-cached pieces,
//! each an 8bpp palette-indexed `pixel_graphics::Framebuffer` uploaded as an
//! `R8` GPU texture the palette shader resolves (see `crate::render::indexed`):
//!
//! * **wallpaper** — one full-output opaque texture, rebuilt only when the
//!   output size changes (a resize rescales the image too);
//! * **leaf chrome** — one leaf-sized texture per placed leaf (border,
//!   titlebar text/icon, the baked split-control buttons, or the minimized
//!   restore strip), rebuilt only when that leaf's content fingerprint
//!   (`LeafKey`) changes; its corners are transparent, so it is not opaque;
//! * **taskbar** — one strip-sized texture over the bottom bar (tiles,
//!   close badges, separator, and quick-launch icons, whose shards reach
//!   above the bar and grow the strip to suit), rebuilt only when its
//!   fingerprint (`TaskbarKey`) changes; transparent between tiles so the
//!   wallpaper shows through.
//!
//! Each frame `redraw` builds render elements from the cached textures and
//! positions them (`comp::scene`): a scroll only moves the elements, never
//! touching a texture. A content change re-renders and re-uploads just its
//! own piece. Layout animation (`comp::anim`) interpolates each leaf's full
//! rect, and an animating leaf re-renders at its interpolated size each
//! tick — so borders stay a constant thickness and titlebars stay crisp as
//! the frame scales, which GPU texture scaling could not do; only the
//! leaves actually resizing pay, idle leaves and steady-state frames stay
//! cached.

use std::collections::HashMap;
use std::rc::Rc;

use super::Comp;
use crate::icon::Icon;
use crate::layout::{NodeId, Side};
use crate::render::indexed::{IndexedProgram, IndexedTexture};
use crate::render::{BtnIcon, LeafView, Renderer, SliceSpec, TitleInfo};
use crate::theme;
use crate::widgets::{BtnKind, FrameRect, Placement, N_BTNS};
use crate::Index;
use pixel_graphics::Framebuffer;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Logical, Point, Size};

/// The independently-cached ex-underlay pieces (see the module docs). Each
/// piece re-renders and re-uploads only when its own content fingerprint
/// changes; positions are pure element placement in `redraw`.
#[derive(Default)]
pub struct ChromePieces {
    wallpaper: WallpaperPiece,
    /// The static frame sprites (border, both restore strips), uploaded once
    /// and sliced over every leaf by the nine-slice shader.
    art: Option<FrameArt>,
    /// Per-leaf frame identity and titlebar-contents strip, keyed by leaf
    /// id; stale entries are dropped as leaves vanish.
    leaves: HashMap<NodeId, LeafPiece>,
    taskbar: TaskbarPiece,
}

/// The shared static frame art: each sprite uploaded once as an `R8`
/// texture, paired with how it slices over a destination rect. Dropped by
/// `invalidate_chrome` with everything else GL.
pub struct FrameArt {
    border: (IndexedTexture, SliceSpec),
    min_v: (IndexedTexture, SliceSpec),
    min_h: (IndexedTexture, SliceSpec),
}

/// Which static sprite a leaf's frame slices: the window border, or the
/// restore strip along either axis when minimized.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FrameMode {
    Border,
    MinV,
    MinH,
}

impl FrameArt {
    pub(crate) fn get(&self, mode: FrameMode) -> (&IndexedTexture, &SliceSpec) {
        let (tex, spec) = match mode {
            FrameMode::Border => &self.border,
            FrameMode::MinV => &self.min_v,
            FrameMode::MinH => &self.min_h,
        };
        (tex, spec)
    }
}

/// One leaf's cached frame state: the identity its GPU-sliced frame element
/// keeps across frames (so the damage tracker sees an unchanged leaf as
/// undamaged; the commit bumps when a *uniform* changes — accent or sprite —
/// which geometry tracking can't see), plus its titlebar-contents strip.
struct LeafPiece {
    id: Id,
    commit: CommitCounter,
    accent: Index,
    mode: FrameMode,
    /// The `w`x`tb_h` strip holding the icon/label, title text and baked
    /// split-control buttons; `None` while minimized. The band fill behind it is
    /// the frame's top margin.
    titlebar: Option<(TitlebarKey, IndexedTexture)>,
}

/// One leaf's frame draw data for the scene: everything `output_elements`
/// needs to build the nine-slice element and the titlebar strip element.
pub struct LeafFrame<'a> {
    /// The frame element's destination rect: the leaf rect, or the restore
    /// strip centred in it when minimized.
    pub dst: FrameRect,
    pub art: &'a IndexedTexture,
    pub spec: &'a SliceSpec,
    pub id: Id,
    pub commit: CommitCounter,
    pub accent: Index,
    /// The titlebar strip texture at its origin (the leaf's top-left).
    pub titlebar: Option<(Point<i32, Logical>, &'a IndexedTexture)>,
}

impl ChromePieces {
    /// The wallpaper element's texture (bottom of the group).
    pub fn wallpaper_element(&self) -> Option<&IndexedTexture> {
        self.wallpaper.tex.as_ref()
    }

    /// The shared static frame art, for the scene's float frames.
    pub fn frame_art(&self) -> Option<&FrameArt> {
        self.art.as_ref()
    }

    /// Each placed leaf's frame draw data at its rect from `tick_layout`
    /// (interpolated mid-slide, settled otherwise): the static art its
    /// element slices, its persistent identity, and its titlebar strip.
    pub fn leaf_elements<'a>(&'a self, rects: &[(NodeId, FrameRect)]) -> Vec<LeafFrame<'a>> {
        let Some(art) = &self.art else {
            return Vec::new();
        };
        rects
            .iter()
            .filter_map(|(leaf, rect)| {
                let piece = self.leaves.get(leaf)?;
                let (tex, spec) = art.get(piece.mode);
                Some(LeafFrame {
                    dst: frame_dst(*rect, piece.mode, tex.size()),
                    art: tex,
                    spec,
                    id: piece.id.clone(),
                    commit: piece.commit,
                    accent: piece.accent,
                    titlebar: piece
                        .titlebar
                        .as_ref()
                        .map(|(_, t)| (Point::<i32, Logical>::from((rect.x, rect.y)), t)),
                })
            })
            .collect()
    }

    /// The taskbar strip element: its texture with its top-left origin.
    pub fn taskbar_element(&self) -> Option<(Point<i32, Logical>, &IndexedTexture)> {
        self.taskbar
            .tex
            .as_ref()
            .map(|t| (Point::<i32, Logical>::from(self.taskbar.origin), t))
    }
}

/// Where a leaf's frame element actually draws within its rect: the whole
/// rect for a bordered leaf; the restore strip centred across the short
/// axis at the sprite's native size when minimized (clamped into the rect —
/// the CPU renderer clipped to a leaf-sized buffer, elements don't clip).
fn frame_dst(
    rect: FrameRect,
    mode: FrameMode,
    sprite: Size<i32, smithay::utils::Buffer>,
) -> FrameRect {
    match mode {
        FrameMode::Border => rect,
        FrameMode::MinV => {
            let w = sprite.w.min(rect.w);
            FrameRect {
                x: rect.x + (rect.w - w) / 2,
                y: rect.y,
                w,
                h: rect.h,
            }
        }
        FrameMode::MinH => {
            let h = sprite.h.min(rect.h);
            FrameRect {
                x: rect.x,
                y: rect.y + (rect.h - h) / 2,
                w: rect.w,
                h,
            }
        }
    }
}

/// The full-output opaque wallpaper texture and the size it was built for;
/// an output resize (which also rescales the image) rebuilds it.
#[derive(Default)]
struct WallpaperPiece {
    tex: Option<IndexedTexture>,
    size: (i32, i32),
}

/// The taskbar strip texture with its fingerprint and top-left origin.
#[derive(Default)]
struct TaskbarPiece {
    key: Option<TaskbarKey>,
    tex: Option<IndexedTexture>,
    origin: (i32, i32),
}

/// One leaf's frame draw data: the border/titlebar view plus the baked
/// split-control buttons (kept visible during a slide — a 280ms cosmetic
/// difference, cheaper than re-rendering buttonless per tick).
struct LeafPaint {
    w: i32,
    h: i32,
    accent: Index,
    minimized: bool,
    title: Option<TitlePaint>,
    /// The baked buttons (`leaf_btn_rects`' fixed capacity), inline so
    /// gathering a paint per leaf per frame never allocates.
    buttons: [Option<BtnPaint>; N_BTNS],
}

/// A leaf titlebar's contents (drawn only when unminimized).
struct TitlePaint {
    label: char,
    icon: Option<Rc<Icon>>,
    title: Rc<str>,
}

/// One baked split-control button, its centre relative to the leaf origin.
#[derive(Clone, Copy, PartialEq)]
struct BtnPaint {
    cx: i32,
    cy: i32,
    icon: BtnIcon,
    accent: Index,
}

/// A titlebar strip's content fingerprint: the derived key deciding whether
/// the strip must be re-rendered. Everything `draw_titlebar_strip` and the
/// baked buttons consult appears here; the height is always `tb_h`. Icons
/// compare by their process-unique id and titles by their string contents.
/// Building one is allocation-free (the title `Rc` is a refcount bump), so
/// deriving it per leaf per frame just to compare is cheap.
#[derive(PartialEq)]
struct TitlebarKey {
    w: i32,
    accent: Index,
    title: Option<(char, Option<u64>, Rc<str>)>,
    buttons: [Option<BtnPaint>; N_BTNS],
}

impl LeafPaint {
    fn titlebar_key(&self) -> TitlebarKey {
        TitlebarKey {
            w: self.w,
            accent: self.accent,
            title: self
                .title
                .as_ref()
                .map(|t| (t.label, t.icon.as_ref().map(|i| i.id()), t.title.clone())),
            buttons: self.buttons,
        }
    }

    /// The sprite the frame slices at this paint's state.
    fn mode(&self) -> FrameMode {
        if !self.minimized {
            FrameMode::Border
        } else if self.w < self.h {
            FrameMode::MinV
        } else {
            FrameMode::MinH
        }
    }

    fn view(&self) -> LeafView {
        LeafView {
            w: self.w,
            tb_h: theme::tb_h(),
            bw: theme::BORDER_LEFT,
            accent_index: self.accent,
            titlebar: self.title.as_ref().map(|t| TitleInfo {
                label: t.label,
                icon: t.icon.clone(),
                title: t.title.clone(),
            }),
            buttons: true,
        }
    }
}

/// The taskbar strip's draw data: the tiles, separator and quick-launch
/// icons, in output-space coordinates.
struct TaskbarPaint {
    w: i32,
    h: i32,
    origin: (i32, i32),
    tiles: Vec<TilePaint>,
    sep: Option<FrameRect>,
    quick: Vec<QuickPaint>,
}

struct TilePaint {
    rect: FrameRect,
    close: FrameRect,
    icon: Option<Rc<Icon>>,
    label: char,
    accent: Index,
}

struct QuickPaint {
    rect: FrameRect,
    icon: Option<Rc<Icon>>,
    label: char,
    /// How far this icon has broken into its four compass shards, 0 while
    /// it is whole.
    split: f32,
    /// The wedge under the pointer, traced in cream; `None` on every icon
    /// the compass is not on.
    hover: Option<Side>,
}

/// The taskbar's content fingerprint (mirrors `LeafKey`'s role): window
/// set/order, per-tile accent/highlight/icon, the separator, and the visible
/// quick-launch entries.
struct TaskbarKey {
    w: i32,
    h: i32,
    origin: (i32, i32),
    tiles: Vec<(FrameRect, FrameRect, Option<u64>, char, Index)>,
    sep: Option<FrameRect>,
    quick: Vec<QuickKey>,
}

fn tile_key(t: &TilePaint) -> (FrameRect, FrameRect, Option<u64>, char, Index) {
    (
        t.rect,
        t.close,
        t.icon.as_ref().map(|i| i.id()),
        t.label,
        t.accent,
    )
}

/// One quick-launch icon's fingerprint: its slot, the icon and letter in
/// it, the shard geometry it has broken into, and the wedge it traces in
/// cream.
type QuickKey = (FrameRect, Option<u64>, char, (i32, i32), Option<Side>);

fn quick_key(q: &QuickPaint) -> QuickKey {
    (
        q.rect,
        q.icon.as_ref().map(|i| i.id()),
        q.label,
        // The phase as the two integers it actually draws as, so a sweep
        // repaints the strip only on the frames whose pixels differ.
        crate::render::shard_steps(q.split),
        q.hover,
    )
}

impl TaskbarKey {
    /// Whether `p` would rebuild into exactly this key — the steady-state
    /// per-frame check, comparing in place instead of building (and
    /// allocating) a fresh key just to throw it away.
    fn matches(&self, p: &TaskbarPaint) -> bool {
        self.w == p.w
            && self.h == p.h
            && self.origin == p.origin
            && self.sep == p.sep
            && self.tiles.len() == p.tiles.len()
            && self.quick.len() == p.quick.len()
            && self
                .tiles
                .iter()
                .zip(&p.tiles)
                .all(|(k, t)| *k == tile_key(t))
            && self
                .quick
                .iter()
                .zip(&p.quick)
                .all(|(k, q)| *k == quick_key(q))
    }
}

impl TaskbarPaint {
    fn key(&self) -> TaskbarKey {
        TaskbarKey {
            w: self.w,
            h: self.h,
            origin: self.origin,
            tiles: self.tiles.iter().map(tile_key).collect(),
            sep: self.sep,
            quick: self.quick.iter().map(quick_key).collect(),
        }
    }
}

/// Refresh one leaf's cached frame state from `paint`: bump the frame
/// element's commit when a shader uniform changes (accent or sprite — the
/// damage tracker sees geometry itself), and re-render/re-upload the
/// titlebar strip when its content fingerprint changes. No leaf-sized
/// buffer exists anywhere: the frame is the shared art sliced on the GPU,
/// and the strip is `w`x`tb_h`.
fn update_leaf(
    chrome: &Renderer,
    indexed: &mut IndexedProgram,
    renderer: &mut GlesRenderer,
    cache: &mut HashMap<NodeId, LeafPiece>,
    leaf: NodeId,
    paint: &LeafPaint,
) {
    let mode = paint.mode();
    let piece = cache.entry(leaf).or_insert_with(|| LeafPiece {
        id: Id::new(),
        commit: CommitCounter::default(),
        accent: paint.accent,
        mode,
        titlebar: None,
    });
    if piece.accent != paint.accent || piece.mode != mode {
        piece.accent = paint.accent;
        piece.mode = mode;
        piece.commit.increment();
    }
    if paint.minimized || (paint.title.is_none() && paint.buttons.iter().all(Option::is_none)) {
        piece.titlebar = None;
        return;
    }
    let key = paint.titlebar_key();
    if piece.titlebar.as_ref().is_some_and(|(k, _)| *k == key) {
        return;
    }
    // Transparent so the frame's titlebar band (drawn behind by the sliced
    // border element) shows through between the icon, text and buttons.
    let mut fb = Framebuffer::new(
        paint.w.max(1) as usize,
        theme::tb_h().max(1) as usize,
        pixel_graphics::TRANSPARENT,
    );
    chrome.draw_titlebar_strip(&mut fb, &paint.view());
    for b in paint.buttons.iter().flatten() {
        chrome.draw_button(&mut fb, b.cx, b.cy, b.icon, b.accent);
    }
    // Reuse the previous texture's GL storage when the size matches.
    let mut tex = piece.titlebar.take().map(|(_, t)| t);
    indexed.upload(renderer, &mut tex, &fb, false);
    piece.titlebar = Some((key, tex.expect("titlebar strip uploaded")));
}

/// Render the taskbar strip into its texture, reusing it when the
/// fingerprint is unchanged. The strip starts transparent so the wallpaper
/// shows between tiles.
fn render_taskbar(
    chrome: &Renderer,
    indexed: &mut IndexedProgram,
    renderer: &mut GlesRenderer,
    piece: &mut TaskbarPiece,
    paint: &TaskbarPaint,
) {
    piece.origin = paint.origin;
    if piece.tex.is_some() && piece.key.as_ref().is_some_and(|k| k.matches(paint)) {
        return;
    }
    let mut fb = Framebuffer::new(
        paint.w.max(1) as usize,
        paint.h.max(1) as usize,
        pixel_graphics::TRANSPARENT,
    );
    let oy = paint.origin.1;
    let shift = |r: FrameRect| FrameRect {
        x: r.x,
        y: r.y - oy,
        w: r.w,
        h: r.h,
    };
    for t in &paint.tiles {
        chrome.draw_taskbar_tile(&mut fb, shift(t.rect), t.icon.as_deref(), t.label, t.accent);
        let c = shift(t.close);
        crate::render::draw_close_badge(&mut fb, c.x, c.y, c.w);
    }
    if let Some(sep) = paint.sep {
        crate::render::draw_taskbar_sep(&mut fb, shift(sep));
    }
    for q in &paint.quick {
        chrome.draw_quick_item(
            &mut fb,
            shift(q.rect),
            q.icon.as_deref(),
            q.label,
            q.split,
            q.hover,
        );
    }
    indexed.upload(renderer, &mut piece.tex, &fb, false);
    piece.key = Some(paint.key());
}

impl Comp {
    /// Drop every cached chrome texture so the next `update_chrome_pieces`
    /// re-renders and re-uploads all of them. Called after a VT switch, whose
    /// device re-activation can lose the GL textures.
    #[cfg_attr(not(feature = "tty"), allow(dead_code))]
    pub fn invalidate_chrome(&mut self) {
        self.view.pieces = ChromePieces::default();
    }

    /// Re-render any chrome piece whose content fingerprint changed and drop
    /// the textures of leaves that vanished. `leaf_rects` are this
    /// frame's leaf rects from `tick_layout` (interpolated mid-animation,
    /// settled otherwise); a leaf whose rect actually changed re-renders at
    /// the new size (its `LeafKey` carries w/h), while an unchanged rect hits
    /// the cache — so a scroll, or a leaf idle during another's animation,
    /// repaints nothing. The wallpaper and taskbar depend on the output size
    /// and settled widgets, not `leaf_rects`, so they never churn per tick.
    pub fn update_chrome_pieces(&mut self, leaf_rects: &[(NodeId, FrameRect)]) {
        let size = self.output_size();
        let (ow, oh) = (size.w.max(1), size.h.max(1));

        // Gather (immutable) before any texture upload borrows the pieces.
        // Each leaf paints at its rect for this frame, pairing it with the
        // placement for the client/title/parent lookups its content needs.
        let leaf_paints: Vec<(NodeId, LeafPaint)> = leaf_rects
            .iter()
            .filter_map(|&(leaf, rect)| {
                self.view
                    .placed
                    .iter()
                    .find(|p| p.leaf == leaf)
                    .map(|p| (leaf, self.leaf_paint(p, rect)))
            })
            .collect();
        let taskbar_paint = self.taskbar_paint(ow, oh);

        // The static frame sprites: once per GL lifetime (invalidate_chrome
        // drops them with everything else).
        if self.view.pieces.art.is_none() {
            let mut upload = |fb: &Framebuffer| {
                let mut tex = None;
                self.view
                    .indexed
                    .upload(self.backend.renderer(), &mut tex, fb, false);
                tex.expect("frame art uploaded")
            };
            let (border_fb, border_spec) = self.view.chrome.border_art();
            let (min_v_fb, min_v_spec) = self.view.chrome.minimized_art(true);
            let (min_h_fb, min_h_spec) = self.view.chrome.minimized_art(false);
            self.view.pieces.art = Some(FrameArt {
                border: (upload(&border_fb), border_spec),
                min_v: (upload(&min_v_fb), min_v_spec),
                min_h: (upload(&min_h_fb), min_h_spec),
            });
        }

        // Wallpaper: only on load / resize.
        if self.view.pieces.wallpaper.tex.is_none() || self.view.pieces.wallpaper.size != (ow, oh) {
            let fb = self.view.chrome.wallpaper_base(ow as u32, oh as u32);
            self.view.indexed.upload(
                self.backend.renderer(),
                &mut self.view.pieces.wallpaper.tex,
                &fb,
                true,
            );
            self.view.pieces.wallpaper.size = (ow, oh);
        }

        // Leaves: refresh changed ones, drop vanished ones.
        for (leaf, paint) in &leaf_paints {
            update_leaf(
                &self.view.chrome,
                &mut self.view.indexed,
                self.backend.renderer(),
                &mut self.view.pieces.leaves,
                *leaf,
                paint,
            );
        }
        self.view
            .pieces
            .leaves
            .retain(|l, _| leaf_paints.iter().any(|(p, _)| p == l));

        // Taskbar strip.
        render_taskbar(
            &self.view.chrome,
            &mut self.view.indexed,
            self.backend.renderer(),
            &mut self.view.pieces.taskbar,
            &taskbar_paint,
        );
    }

    /// One leaf's frame draw data at `rect` (its interpolated rect
    /// mid-animation, `p.target` otherwise): accent, title (only when
    /// unminimized and occupied), minimized state and the baked split-control
    /// buttons. The frame paints at `rect`'s size, so borders and titlebar
    /// re-render crisp as the frame scales during a layout transition.
    fn leaf_paint(&self, p: &Placement, rect: FrameRect) -> LeafPaint {
        let minimized = self.state.layout.leaf(p.leaf).is_some_and(|l| l.minimized);
        let accent = crate::widgets::leaf_color_index(&self.state.layout, p.leaf);
        let title = if minimized {
            None
        } else {
            self.managed.get(p.client).map(|window| TitlePaint {
                label: crate::shell::toplevel_label(window),
                icon: self.icon_for(p.client),
                title: crate::shell::toplevel_title(window),
            })
        };
        LeafPaint {
            w: rect.w,
            h: rect.h,
            accent,
            minimized,
            title,
            buttons: self.leaf_buttons(p.leaf, rect, minimized, accent),
        }
    }

    /// The split-control buttons baked into a leaf's titlebar: right-aligned
    /// in `rect` (the shared `leaf_btn_rects` geometry the hit-regions use, so
    /// a click lands where the button drew). Positioned relative to `rect`'s
    /// origin, so mid-animation they ride the interpolated titlebar. A
    /// minimized leaf draws none — its whole restore strip is the button.
    fn leaf_buttons(
        &self,
        leaf: NodeId,
        rect: FrameRect,
        minimized: bool,
        accent: Index,
    ) -> [Option<BtnPaint>; N_BTNS] {
        let mut out = [None; N_BTNS];
        if minimized {
            return out;
        }
        // A stacked split collapses to a row (short/wide) when minimized,
        // so its button previews that with the horizontal glyph.
        let stacked = self.state.layout.stacked(leaf);
        let paints = crate::widgets::leaf_btn_rects(rect).map(|(kind, r)| BtnPaint {
            cx: r.x + r.w / 2 - rect.x,
            cy: r.y + r.h / 2 - rect.y,
            icon: match kind {
                BtnKind::Minimize if stacked => BtnIcon::MinimizeH,
                BtnKind::Minimize => BtnIcon::Minimize,
                BtnKind::Close => BtnIcon::Close,
            },
            accent,
        });
        for (slot, b) in out.iter_mut().zip(paints) {
            *slot = Some(b);
        }
        out
    }

    /// The taskbar strip's draw data: one tile per split's window, in split
    /// order (accent trace, corner close badge), the separator, and the
    /// visible quick-launch icons. The strip spans the full output width
    /// and the bottom `theme::TASKBAR_H` pixels, plus the band above them a
    /// split icon's shards reach into.
    fn taskbar_paint(&self, ow: i32, oh: i32) -> TaskbarPaint {
        let aim = self.quick_aim();
        // A split icon reaches above the bar, so the strip runs up to where
        // one would reach whether or not any is split — it is transparent
        // outside its tiles either way, and a fixed extent means a hover
        // rewrites the texture in place instead of reallocating it.
        let bar_top = (oh - theme::TASKBAR_H).max(0);
        let shard_top = self
            .view
            .widgets
            .quick_regions
            .iter()
            .map(|t| t.icon.y - crate::render::QUICK_ICON_REACH)
            .min();
        let origin_y = shard_top.map_or(bar_top, |y| bar_top.min(y)).max(0);
        let tiles = self
            .view
            .widgets
            .taskbar_regions
            .iter()
            .map(|t| TilePaint {
                rect: t.rect,
                close: t.close,
                icon: self.icon_for(t.win),
                label: self
                    .managed
                    .get(t.win)
                    .map_or('?', crate::shell::toplevel_label),
                accent: t.accent,
            })
            .collect();
        let quick = self
            .view
            .widgets
            .quick_regions
            .iter()
            .filter_map(|t| {
                self.view.quick.get(t.slot).map(|q| QuickPaint {
                    rect: t.icon,
                    icon: q.icon.clone(),
                    label: q.label,
                    split: self.quick_split_phase(t.slot),
                    hover: aim.filter(|&(slot, _)| slot == t.slot).map(|(_, z)| z),
                })
            })
            .collect();
        TaskbarPaint {
            w: ow,
            h: oh - origin_y,
            origin: (0, origin_y),
            tiles,
            sep: self.view.widgets.taskbar_sep,
            quick,
        }
    }
}
