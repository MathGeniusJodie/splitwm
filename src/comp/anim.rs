//! Layout transitions: each leaf frame's full rect eases from its start
//! toward its target (position and size, ease-out-back), stepped by the
//! redraw tick (~60 Hz). An animating leaf's chrome re-renders at the
//! interpolated size each tick so its borders scale without blurring
//! (`comp::pieces`), and its client window rides the interpolated rect
//! (`Comp::tiled_places`).
//! The hovered quick-launch icon's break into its four compass shards
//! (`QuickSplit`) eases on the same tick and off the same curve.

use super::Comp;
use crate::layout::{NodeId, Win};
use crate::widgets::{FrameRect, Placement};

/// How far a sweep started at `start` has run, 0 to 1, holding at 1 once
/// its `duration` is up.
fn progress(start: std::time::Instant, duration: std::time::Duration) -> f32 {
    (start.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.0)
}

/// ease-out-back: slight overshoot past the target, then settle.
fn ease_out_back(t: f32) -> f32 {
    let c = 1.1_f32;
    let t = t - 1.0;
    let inner = (c + 1.0).mul_add(t, c);
    (t * t).mul_add(inner, 1.0)
}

fn lerp_rect(a: FrameRect, b: FrameRect, p: f32) -> FrameRect {
    let l = |s: i32, e: i32| s + ((e - s) as f32 * p) as i32;
    FrameRect {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        w: l(a.w, b.w).max(1),
        h: l(a.h, b.h).max(1),
    }
}

/// How long a layout transition takes, wall-clock.
const ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(280);

/// An in-flight layout animation, stepped by the redraw tick (~60 Hz).
/// Each leaf frame's full rect eases from its start toward its target,
/// re-rendering at the interpolated size each tick so its borders scale
/// without blurring, and the leaf's client window rides the interpolated
/// rect (`Comp::tiled_places`). Growing clients are configured to their
/// final size by the arrange that started this; shrinking ones keep their
/// old size until the animation settles (`deferred`), so their content
/// doesn't reflow narrow inside a still-wide frame.
pub struct LayoutAnim {
    pub start: std::time::Instant,
    /// Each animated leaf's start rect paired with its target placement.
    pub placed: Vec<(FrameRect, Placement)>,
    /// Configures withheld from shrinking clients, as the final client rect
    /// each window gets when the animation ends. Dropped unsent if another
    /// arrange replaces this animation — that arrange configures (or
    /// re-defers) every tiled window itself.
    pub deferred: Vec<(Win, (i32, i32, i32, i32))>,
}

impl Comp {
    /// Advance any in-flight layout animation and report this frame's leaf
    /// rects: each placed leaf's full draw rect, interpolated toward its
    /// target (position and size, ease-out-back) while the animation runs,
    /// else its settled rect. The animating leaves' frames re-render at these
    /// sizes in `update_chrome_pieces`, so borders stay crisp as the frame
    /// scales. Also updates `focus_rect` so the outline rides the focused
    /// leaf's current interpolated rect.
    pub fn tick_layout(&mut self) -> Vec<(NodeId, FrameRect)> {
        let anim_result = self.view.anim.as_ref().map(|anim| {
            let t = progress(anim.start, ANIM_DURATION);
            if t >= 1.0 {
                return (true, Vec::new(), None);
            }
            let e = ease_out_back(t);
            let mut rects = Vec::with_capacity(anim.placed.len());
            let mut focus = None;
            for &(from, p) in &anim.placed {
                let r = lerp_rect(from, p.target, e);
                rects.push((p.leaf, r));
                if p.focused {
                    focus = Some(r);
                }
            }
            (false, rects, focus)
        });
        if let Some((done, rects, focus)) = anim_result {
            if done {
                self.finish_animation();
            } else {
                self.view.focus_rect = focus;
                return rects;
            }
        }
        self.view.focus_rect = self
            .view
            .placed
            .iter()
            .find(|p| p.focused)
            .map(|p| p.target);
        self.view
            .placed
            .iter()
            .map(|p| (p.leaf, p.target))
            .collect()
    }

    /// Snap an in-flight animation to its end state: the next frame's leaf
    /// origins settle on their targets, and shrinking clients receive the
    /// configures the animated arrange withheld. Also used when a click must
    /// land on the final layout the user is aiming at.
    pub fn finish_animation(&mut self) {
        let Some(anim) = self.view.anim.take() else {
            return;
        };
        for (win, (cx, cy, cw, ch)) in anim.deferred {
            let Some(window) = self.managed.get(win).cloned() else {
                continue;
            };
            crate::shell::configure_rect(&window, cx, cy, cw, ch);
        }
    }
}

/// How long a quick-launch icon takes to break into its shards, or to
/// close back up.
const SPLIT_DURATION: std::time::Duration = std::time::Duration::from_millis(160);

/// The hovered quick-launch icon breaking into its four compass shards.
/// One icon at a time carries this: the compass hands straight over
/// between neighbouring icons, and the shards follow it, the icon taking
/// over at the phase its predecessor had reached.
pub struct QuickSplit {
    /// The quick-launch slot whose icon is breaking up.
    slot: usize,
    /// When the current sweep started and the phase it started from: a
    /// sweep reversed mid-flight resumes from where it had got to rather
    /// than jumping to an end.
    start: std::time::Instant,
    from: f32,
    /// Whether the sweep runs toward apart (`true`) or back together.
    opening: bool,
}

impl QuickSplit {
    fn opening(slot: usize) -> Self {
        Self {
            slot,
            start: std::time::Instant::now(),
            from: 0.0,
            opening: true,
        }
    }

    /// How far apart the shards stand: 0 is whole, 1 fully apart. Rides
    /// `ease_out_back`, whose overshoot past the target pops the shards out
    /// on the way open; the same overshoot below 0 on the way shut means
    /// nothing, so it floors here rather than at every reader.
    fn phase(&self) -> f32 {
        let to = if self.opening { 1.0 } else { 0.0 };
        (to - self.from)
            .mul_add(
                ease_out_back(progress(self.start, SPLIT_DURATION)),
                self.from,
            )
            .max(0.0)
    }

    /// Aim the sweep at apart (`opening`) or back together. Turning it
    /// around keeps the phase it had reached rather than jumping to an end;
    /// aiming it where it already runs leaves it alone.
    fn aim(&mut self, opening: bool) {
        if self.opening == opening {
            return;
        }
        self.from = self.phase();
        self.start = std::time::Instant::now();
        self.opening = opening;
    }

    /// Whether the sweep has run its course — a settled split holds still,
    /// so nothing needs redrawing until the hover changes.
    pub fn settled(&self) -> bool {
        self.start.elapsed() >= SPLIT_DURATION
    }
}

impl Comp {
    /// Point the shard animation at whichever quick-launch icon the pointer
    /// is over: the hovered icon breaks apart, and the animation ends once
    /// the last one has closed back up. Runs on the redraw tick, like the
    /// layout transition.
    pub fn tick_quick_split(&mut self) {
        let hovered = self.interaction.quick_hover;
        let Some(split) = &mut self.view.quick_split else {
            self.view.quick_split = hovered.map(QuickSplit::opening);
            return;
        };
        match hovered {
            Some(slot) => {
                split.slot = slot;
                split.aim(true);
            }
            None => {
                split.aim(false);
                if split.settled() {
                    self.view.quick_split = None;
                }
            }
        }
    }

    /// How far the quick-launch icon in `slot` has broken apart, 0 while it
    /// is whole.
    pub fn quick_split_phase(&self, slot: usize) -> f32 {
        self.view
            .quick_split
            .as_ref()
            .filter(|s| s.slot == slot)
            .map_or(0.0, QuickSplit::phase)
    }
}
