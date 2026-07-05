//! Trackpad/wheel horizontal-scroll canvas panning: XInput2 raw-motion
//! valuator discovery and the accumulated-delta-to-pixel conversion. There
//! is no vertical scroll behaviour of our own (see `super::events`'s legacy
//! wheel-click handling for why vertical ticks are simply dropped).

use x11rb::protocol::xinput;
use x11rb::protocol::xproto::ConnectionExt as _;

use super::super::types::{Wm, MOD4, R};

/// A device's horizontal scroll axis: which valuator carries it and how many
/// valuator units make up one wheel "click" (for scaling into pixels).
#[derive(Clone, Copy)]
pub struct HScroll {
    pub dev: u16,
    pub valuator: u16,
    pub incr: f64,
}

impl Wm {
    /// Rescan every input device for a horizontal scroll valuator. Run once
    /// at startup and again on every `XI_HierarchyChanged` (device
    /// plug/unplug).
    pub(crate) fn build_hscroll_map(&mut self) -> R<()> {
        use x11rb::protocol::xinput::ConnectionExt as _;
        let reply = self
            .conn
            .xinput_xi_query_device(super::super::XI_ALL_DEVICES)?
            .reply()?;
        self.hscroll.clear();
        for info in &reply.infos {
            for class in &info.classes {
                let xinput::DeviceClassData::Scroll(s) = &class.data else {
                    continue;
                };
                if s.scroll_type != xinput::ScrollType::HORIZONTAL {
                    continue;
                }
                let incr = super::super::fp3232_to_f64(s.increment);
                self.hscroll.push(HScroll {
                    dev: class.sourceid,
                    valuator: s.number,
                    incr: if incr == 0.0 { 120.0 } else { incr },
                });
            }
        }
        if self.debug_scroll {
            eprintln!(
                "splitwm: hscroll map rebuilt, {} device(s): {:?}",
                self.hscroll.len(),
                self.hscroll
                    .iter()
                    .map(|h| (h.dev, h.valuator, h.incr))
                    .collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    /// Sum of this raw motion event's horizontal-scroll valuator deltas
    /// (in wheel-click fractions), across every device that reported one.
    pub(crate) fn hscroll_delta(&self, e: &xinput::RawMotionEvent) -> f64 {
        if self.debug_scroll {
            eprintln!(
                "splitwm: raw motion from sourceid={} mask={:?} known_hscroll_devs={:?}",
                e.sourceid,
                e.valuator_mask,
                self.hscroll.iter().map(|h| h.dev).collect::<Vec<_>>()
            );
        }
        self.hscroll
            .iter()
            .filter(|h| h.dev == e.sourceid)
            .filter_map(|h| {
                super::super::valuator_value(&e.valuator_mask, &e.axisvalues, h.valuator)
                    .map(|v| v / h.incr)
            })
            .sum()
    }

    /// Apply an accumulated horizontal-scroll delta (wheel-click fractions)
    /// to the canvas, gated on where the pointer currently is: freely over
    /// the underlay/gaps and over the docked sidebar (it has no scrollable
    /// content of its own to fight for the swipe), only with Mod4 held over
    /// an ordinary client window (so a swipe doesn't fight an app's own
    /// horizontal scrolling).
    pub(crate) fn apply_hscroll(&mut self, delta: f64) -> R<()> {
        if !self.hscroll_allowed()? {
            return Ok(());
        }
        let wa = self.la();
        // Carry the sub-pixel remainder between batches: a slow continuous
        // swipe can deliver less than a pixel per batch, and truncating each
        // batch independently would discard the entire gesture.
        let px_f = delta.mul_add(f64::from(crate::theme::SCROLL_STEP), self.hscroll_frac);
        let px = px_f as i32;
        self.hscroll_frac = px_f - f64::from(px);
        if px == 0 {
            return Ok(());
        }
        self.state.scroll_delta(wa, px);
        self.state.land_scroll();
        if self.debug_scroll {
            let t0 = std::time::Instant::now();
            self.arrange()?;
            eprintln!("splitwm: arrange() for scroll took {:?}", t0.elapsed());
            return Ok(());
        }
        self.arrange()
    }

    /// Whether scrolling is currently allowed (see `apply_hscroll`). A swipe
    /// can call this dozens of times a second; re-querying the pointer that
    /// often would itself be a source of per-event round-trip latency, so
    /// the answer is cached for a short window — long enough to absorb a
    /// whole burst, short enough that moving the pointer under/off a window
    /// mid-swipe is still honoured almost immediately. Raw XI2 motion events
    /// carry no modifier state of their own (unlike core events), so this
    /// poll is the only way to read Mod4 — 12ms keeps releasing it mid-swipe
    /// from panning for more than a couple of scroll batches.
    pub(crate) fn hscroll_allowed(&mut self) -> R<bool> {
        if let Some((last, allowed)) = self.hscroll_gate {
            if last.elapsed() < std::time::Duration::from_millis(12) {
                return Ok(allowed);
            }
        }
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let allowed = if p.child == x11rb::NONE
            || p.child == self.underlay
            || self.dock.docked.is_some_and(|d| d.win == p.child)
        {
            true
        } else {
            u16::from(p.mask) & MOD4 != 0
        };
        if self.debug_scroll {
            eprintln!(
                "splitwm: hscroll_allowed child={} underlay={} mask={:?} -> {}",
                p.child, self.underlay, p.mask, allowed
            );
        }
        self.hscroll_gate = Some((std::time::Instant::now(), allowed));
        Ok(allowed)
    }
}
