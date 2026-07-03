//! Native `org.freedesktop.Notifications` daemon: splitwm doubles as the
//! session's notification server, so `notify-send` and friends land as
//! speech-bubble popups drawn by our own renderer instead of a separate
//! daemon's windows.
//!
//! Runs on its own thread (libdbus is blocking); talks to the WM thread over
//! an mpsc channel and wakes its X event loop by sending a `SPLITWM_NOTE`
//! ClientMessage to the root window (delivered to us via the
//! SUBSTRUCTURE_REDIRECT selection only a WM holds). The WM reports
//! user-dismissed popups back over a second channel so the
//! `NotificationClosed` signal can be emitted from the thread that owns the
//! bus connection.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use dbus::arg::PropMap;
use dbus::blocking::Connection;
use dbus::channel::Channel;
use dbus::message::MessageType;
use dbus::Message;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt as _, EventMask};
type R<T> = Result<T, Box<dyn std::error::Error>>;

const BUS_NAME: &str = "org.freedesktop.Notifications";
const IFACE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The atom name the WM's event loop watches for as its "notifications
/// changed, drain the channel" wakeup.
pub const PING_ATOM: &str = "SPLITWM_NOTE";

/// `expire_timeout: -1` ("server decides") becomes this.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Note {
    pub id: u32,
    pub summary: String,
    pub body: String,
    /// 0 low / 1 normal / 2 critical (freedesktop urgency hint).
    pub urgency: u8,
}

pub enum NoteMsg {
    Show(Note),
    /// Expired or closed via D-Bus; the WM should drop the popup.
    Close(u32),
}

/// Spawn the daemon thread. Returns the sender the WM uses to report a
/// popup the user dismissed (so the matching signal goes out on the bus).
/// Bus errors (no session bus, name already owned) only disable
/// notifications: they log and let the WM run on.
pub fn spawn(to_wm: Sender<NoteMsg>) -> Sender<u32> {
    let (dismiss_tx, dismiss_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = serve(&to_wm, &dismiss_rx) {
            eprintln!("splitwm: notification daemon stopped: {e}");
        }
    });
    dismiss_tx
}

fn serve(to_wm: &Sender<NoteMsg>, dismissed: &Receiver<u32>) -> R<()> {
    let conn = Connection::new_session()?;
    // Take over from a lingering daemon (e.g. xfce4-notifyd) but never queue
    // behind one: without the name we'd just be a dead letterbox.
    let granted = conn.request_name(BUS_NAME, false, true, true)?;
    if !matches!(
        granted,
        dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply::PrimaryOwner
    ) {
        return Err(format!("{BUS_NAME} is owned by another daemon").into());
    }

    // Own X connection purely for waking the WM's blocking event loop.
    let (xc, screen) = x11rb::connect(None)?;
    let root = xc.setup().roots[screen].root;
    let ping_atom = xc.intern_atom(false, PING_ATOM.as_bytes())?.reply()?.atom;
    let ping = |()| -> R<()> {
        let ev = ClientMessageEvent::new(32, root, ping_atom, [0u32; 5]);
        xc.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT, ev)?;
        xc.flush()?;
        Ok(())
    };

    let mut next_id: u32 = 1;
    let mut expiries: HashMap<u32, Instant> = HashMap::new();

    loop {
        // Popups the WM closed on click: emit the spec's "dismissed by the
        // user" close reason.
        while let Ok(id) = dismissed.try_recv() {
            expiries.remove(&id);
            emit_closed(conn.channel(), id, 2)?;
        }

        let now = Instant::now();
        let expired: Vec<u32> = expiries
            .iter()
            .filter(|&(_, t)| *t <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            expiries.remove(&id);
            to_wm
                .send(NoteMsg::Close(id))
                .map_err(|_| "wm channel closed")?;
            emit_closed(conn.channel(), id, 1)?; // 1 = expired
            ping(())?;
        }

        // Sleep until the next expiry, but never so long that a dismissal
        // report from the WM sits unserviced.
        let wait = expiries
            .values()
            .map(|t| t.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_millis(250))
            .clamp(Duration::from_millis(10), Duration::from_millis(250));
        let Some(msg) = conn.channel().blocking_pop_message(wait)? else {
            continue;
        };
        if msg.msg_type() != MessageType::MethodCall
            || msg.interface().as_deref() != Some(IFACE)
        {
            continue;
        }
        match msg.member().as_deref() {
            Some("Notify") => {
                let Some((note, timeout)) = parse_notify(&msg, &mut next_id) else {
                    continue;
                };
                let id = note.id;
                // 0 means never expire; so does critical urgency per spec.
                match timeout {
                    _ if note.urgency >= 2 => None,
                    0 => None,
                    t if t > 0 => Some(Duration::from_millis(t as u64)),
                    _ => Some(DEFAULT_TIMEOUT),
                }
                .map(|d| expiries.insert(id, Instant::now() + d));
                to_wm
                    .send(NoteMsg::Show(note))
                    .map_err(|_| "wm channel closed")?;
                ping(())?;
                reply(conn.channel(), msg.method_return().append1(id))?;
            }
            Some("CloseNotification") => {
                if let Ok(id) = msg.read1::<u32>() {
                    expiries.remove(&id);
                    to_wm
                        .send(NoteMsg::Close(id))
                        .map_err(|_| "wm channel closed")?;
                    emit_closed(conn.channel(), id, 3)?; // 3 = closed by call
                    ping(())?;
                }
                reply(conn.channel(), msg.method_return())?;
            }
            Some("GetCapabilities") => {
                reply(conn.channel(), msg.method_return().append1(vec!["body"]))?;
            }
            Some("GetServerInformation") => {
                let r = msg
                    .method_return()
                    .append3("splitwm", "splitwm", env!("CARGO_PKG_VERSION"))
                    .append1("1.2");
                reply(conn.channel(), r)?;
            }
            _ => {}
        }
    }
}

/// Decode a `Notify` call's eight arguments into a `Note` plus its raw
/// `expire_timeout`. Returns `None` on a malformed call.
fn parse_notify(msg: &Message, next_id: &mut u32) -> Option<(Note, i32)> {
    let mut it = msg.iter_init();
    let _app: String = it.read().ok()?;
    let replaces: u32 = it.read().ok()?;
    let _icon: String = it.read().ok()?;
    let summary: String = it.read().ok()?;
    let body: String = it.read().ok()?;
    let _actions: Vec<String> = it.read().ok()?;
    let hints: PropMap = it.read().ok()?;
    let timeout: i32 = it.read().ok()?;

    let id = if replaces != 0 {
        replaces
    } else {
        let id = *next_id;
        *next_id = next_id.wrapping_add(1).max(1);
        id
    };
    let urgency = hints
        .get("urgency")
        .and_then(|v| v.0.as_u64())
        .map_or(1, |u| u.min(2) as u8);
    Some((
        Note {
            id,
            summary: strip_markup(&summary),
            body: strip_markup(&body),
            urgency,
        },
        timeout,
    ))
}

fn emit_closed(ch: &Channel, id: u32, reason: u32) -> R<()> {
    let sig = Message::new_signal(PATH, IFACE, "NotificationClosed")
        .map_err(|e| format!("bad signal: {e}"))?
        .append2(id, reason);
    reply(ch, sig)
}

fn reply(ch: &Channel, msg: Message) -> R<()> {
    ch.send(msg).map_err(|()| "dbus send failed")?;
    Ok(())
}

/// The spec allows a small HTML subset in the body; we render plain text, so
/// drop tags and decode the standard entities.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    for (ent, ch) in [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&amp;", "&"), // last, so `&amp;lt;` doesn't double-decode
    ] {
        out = out.replace(ent, ch);
    }
    out
}
