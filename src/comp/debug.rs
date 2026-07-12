//! The harness's debug channel: a line protocol on stdin, opt-in via
//! `SPLITWM_DEBUG_CHANNEL=1`, driving the compositor the way the keyboard
//! would. Every command is acked on stdout (`ok …` / `err …`) so a driver
//! can synchronize on completion; `shot` acks only once the image is on
//! disk (see `backend::headless`).
//!
//! Commands:
//! - `key <chord>` — dispatch a `theme::BINDINGS` chord, e.g.
//!   `key super+shift+c`. The chord is resolved through the same
//!   `binding_action` table the keyboard dispatcher uses; only bound
//!   chords exist here (there is nothing to forward a plain key *to*).
//! - `spawn <cmd>` — launch a client the way quick-launch would, for
//!   drives that need something other than `$TERMINAL`.
//! - `motion <x> <y>` — move the pointer, with enter/leave and hit
//!   tracking as if the mouse moved there.
//! - `click <x> <y>` — motion there, then a full left press+release
//!   through the same dispatch as a real button.
//! - `press <x> <y>` / `release <x> <y>` — the two halves of `click`
//!   separately, so a driver can drag: press, motions, release.
//! - `scroll <clicks>` — pan the canvas by wheel clicks (the Mod4+wheel
//!   path), positive scrolls right.
//! - `shot <path>` — write the next composited frame to `path`
//!   (headless backend only).
//! - `cursor` — report what the pointer shows right now: `hidden` or a
//!   named shape (`default`, `ew-resize`, …).
//! - `focus` — report who holds the keyboard: `none`, a managed window's
//!   class/app_id, or `unmanaged` (o-r window, layer surface). Lets a
//!   driver observe focus on clients that aren't the test's own Wayland
//!   connection (XWayland windows).
//! - `layout` — report the splits in depth-first (left-to-right) order,
//!   one token per leaf: its window's title, or `-` for an empty
//!   placeholder. This is also the taskbar's tile order, so a driver can
//!   verify reorders without pixel-reading the bar.

use std::io::Read as _;

use smithay::backend::input::ButtonState;
use smithay::input::keyboard::{xkb, ModifiersState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::utils::{Logical, Point};

use super::Comp;

pub fn insert_channel(handle: &LoopHandle<'static, Comp>) {
    let mut pending: Vec<u8> = Vec::new();
    handle
        .insert_source(
            Generic::new(std::io::stdin(), Interest::READ, Mode::Level),
            move |_, stdin, comp| {
                // Level-triggered readiness: at least one byte is readable,
                // so a single read never blocks the loop.
                let mut chunk = [0u8; 4096];
                // SAFETY: stdin is neither dropped nor replaced here.
                let n = match unsafe { stdin.get_mut() }.read(&mut chunk) {
                    Ok(n) => n,
                    Err(err) => {
                        tracing::warn!("debug channel read: {err}");
                        return Ok(PostAction::Remove);
                    }
                };
                if n == 0 {
                    // Driver hung up; the channel is done for this session.
                    return Ok(PostAction::Remove);
                }
                pending.extend_from_slice(&chunk[..n]);
                while let Some(eol) = pending.iter().position(|&b| b == b'\n') {
                    command(comp, String::from_utf8_lossy(&pending[..eol]).trim());
                    pending.drain(..=eol);
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("insert debug channel source");
}

fn command(comp: &mut Comp, line: &str) {
    if line.is_empty() {
        return;
    }
    match line.split_once(' ') {
        Some(("key", chord)) => match chord_action(chord) {
            Some(action) => {
                comp.do_action(action);
                println!("ok key {chord}");
            }
            None => println!("err key {chord}: no such binding"),
        },
        Some(("spawn", cmd)) => {
            crate::launch::spawn(cmd);
            println!("ok spawn {cmd}");
        }
        Some(("motion", xy)) => match parse_xy(xy) {
            Some(pos) => {
                comp.pointer_moved(pos, comp.start.elapsed().as_millis() as u32);
                println!("ok motion {xy}");
            }
            None => println!("err motion {xy}: want <x> <y>"),
        },
        Some((cmd @ ("click" | "press" | "release"), args)) => match parse_click(args) {
            Some((pos, button)) => {
                let time = comp.start.elapsed().as_millis() as u32;
                comp.pointer_moved(pos, time);
                if cmd != "release" {
                    comp.pointer_button(button, ButtonState::Pressed, time);
                }
                if cmd != "press" {
                    comp.pointer_button(button, ButtonState::Released, time);
                }
                println!("ok {cmd} {args}");
            }
            None => println!("err {cmd} {args}: want <x> <y> [left|right|middle]"),
        },
        Some(("scroll", clicks)) => match clicks.parse::<f64>() {
            Ok(clicks) => {
                comp.apply_hscroll(clicks);
                println!("ok scroll {clicks}");
            }
            Err(err) => println!("err scroll {clicks}: {err}"),
        },
        Some(("shot", path)) => {
            // The ack for the success path comes from the headless render,
            // after the file is written.
            if !comp.backend.request_shot(path) {
                println!("err shot {path}: this backend cannot read frames back");
            }
        }
        None if line == "focus" => {
            use smithay::wayland::seat::WaylandFocus as _;
            let focus = comp
                .keyboard
                .current_focus()
                .and_then(|t| t.wl_surface().map(std::borrow::Cow::into_owned));
            let name = match &focus {
                None => "none".into(),
                Some(surface) => match comp.managed.win_for_surface(surface) {
                    Some(win) => comp
                        .managed
                        .get(win)
                        .map_or_else(|| "unmanaged".into(), crate::shell::toplevel_app_id),
                    None => "unmanaged".into(),
                },
            };
            println!("ok focus {name}");
        }
        None if line == "layout" => {
            let tokens: Vec<String> = comp
                .state
                .layout
                .collect_leaves()
                .into_iter()
                .map(|l| {
                    comp.state
                        .layout
                        .leaf(l)
                        .and_then(|lf| lf.client)
                        .and_then(|c| comp.managed.get(c))
                        .map_or_else(
                            || "-".to_string(),
                            |w| crate::shell::toplevel_title(w).to_string(),
                        )
                })
                .collect();
            println!("ok layout {}", tokens.join(" "));
        }
        None if line == "cursor" => {
            use smithay::input::pointer::CursorImageStatus;
            let name = match &comp.cursor_status {
                CursorImageStatus::Hidden => "hidden",
                CursorImageStatus::Named(icon) => icon.name(),
                CursorImageStatus::Surface(_) => "surface",
            };
            println!("ok cursor {name}");
        }
        _ => println!("err {line}: unknown command"),
    }
}

fn parse_xy(xy: &str) -> Option<Point<f64, Logical>> {
    let (x, y) = xy.split_once(' ')?;
    Some((x.trim().parse::<f64>().ok()?, y.trim().parse::<f64>().ok()?).into())
}

/// `<x> <y> [left|right|middle]` — the button defaults to left.
fn parse_click(args: &str) -> Option<(Point<f64, Logical>, u32)> {
    // input-event-codes BTN_* values, as delivered by libinput.
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    let mut it = args.split_whitespace();
    let x = it.next()?.parse::<f64>().ok()?;
    let y = it.next()?.parse::<f64>().ok()?;
    let button = match it.next() {
        None | Some("left") => BTN_LEFT,
        Some("right") => BTN_RIGHT,
        Some("middle") => BTN_MIDDLE,
        Some(_) => return None,
    };
    it.next().is_none().then_some(((x, y).into(), button))
}

/// Resolve `super+shift+c`-style chords against `theme::BINDINGS`, through
/// the same lookup the keyboard dispatcher uses. Key names are xkb keysym
/// names (`v`, `bracketright`, `XF86AudioRaiseVolume`), case-insensitive.
fn chord_action(chord: &str) -> Option<crate::theme::Action> {
    let parts: Vec<&str> = chord.split('+').collect();
    let (key, mod_names) = parts.split_last()?;
    let mut mods = ModifiersState::default();
    for name in mod_names {
        match name.to_ascii_lowercase().as_str() {
            "super" | "mod4" | "logo" => mods.logo = true,
            "shift" => mods.shift = true,
            "alt" | "mod1" => mods.alt = true,
            "ctrl" | "control" => mods.ctrl = true,
            _ => return None,
        }
    }
    let sym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    super::actions::binding_action(&mods, sym.raw())
}
