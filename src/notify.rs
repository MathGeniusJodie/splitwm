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

/// Cap on outstanding notifications; also aliased by `wm::notes` as its own
/// popup cap (its private `MAX_NOTE_POPUPS`), since this daemon's
/// `Show`/`Close` traffic ultimately drives that popup pile and the two must
/// stay coherent.
pub const MAX_NOTES: usize = 8;

/// Freedesktop `NotificationClosed` reasons the WM reports back over the
/// dismiss channel (the daemon relays them onto the bus verbatim).
pub const CLOSE_REASON_DISMISSED: u32 = 2; // dismissed by the user (click)
pub const CLOSE_REASON_UNDEFINED: u32 = 4; // evicted for space; no exact fit

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
/// popup it closed as `(id, close reason)` — `CLOSE_REASON_DISMISSED` for a
/// user click, `CLOSE_REASON_UNDEFINED` for a popup-cap eviction — so the
/// matching `NotificationClosed` signal goes out on the bus with the truth.
/// Bus errors (no session bus, name already owned) only disable
/// notifications: they log and let the WM run on.
pub fn spawn(to_wm: Sender<NoteMsg>) -> Sender<(u32, u32)> {
    let (dismiss_tx, dismiss_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = serve(&to_wm, &dismiss_rx) {
            eprintln!("splitwm: notification daemon stopped: {e}");
        }
    });
    dismiss_tx
}

/// Connect to the session bus and claim the notification name. The bus
/// connection is retried with backoff: at WM startup the session bus may not
/// be up yet (dbus-launch race), and giving up on the first attempt disables
/// notifications for the whole session. A name refusal is not retried — the
/// request already asks to replace the owner, so a refusal means the owner
/// forbids replacement and that won't change.
fn connect_bus() -> R<Connection> {
    let mut last_err: Box<dyn std::error::Error> = "no attempt".into();
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        match Connection::new_session() {
            Ok(conn) => {
                // Take over from a lingering daemon (e.g. xfce4-notifyd) but
                // never queue behind one: without the name we'd just be a
                // dead letterbox.
                let granted = conn.request_name(BUS_NAME, false, true, true)?;
                if !matches!(
                    granted,
                    dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply::PrimaryOwner
                ) {
                    return Err(format!("{BUS_NAME} is owned by another daemon").into());
                }
                return Ok(conn);
            }
            Err(e) => last_err = e.into(),
        }
    }
    Err(last_err)
}

fn serve(to_wm: &Sender<NoteMsg>, dismissed: &Receiver<(u32, u32)>) -> R<()> {
    // Own X connection purely for waking the WM's blocking event loop.
    // Established before the bus so a bus failure can still be shown to the
    // user as a popup (below) rather than only a stderr line nobody sees.
    let (xc, screen) = x11rb::connect(None)?;
    let root = xc.setup().roots[screen].root;
    let ping_atom = xc.intern_atom(false, PING_ATOM.as_bytes())?.reply()?.atom;
    let ping = |()| -> R<()> {
        let ev = ClientMessageEvent::new(32, root, ping_atom, [0u32; 5]);
        xc.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT, ev)?;
        xc.flush()?;
        Ok(())
    };

    let conn = match connect_bus() {
        Ok(c) => c,
        Err(e) => {
            // Notifications are dead for the session; say so where the user
            // can see it — as the one popup this daemon will ever show.
            let _ = to_wm.send(NoteMsg::Show(Note {
                id: 0,
                summary: "Notifications unavailable".to_string(),
                body: format!("splitwm could not become the notification daemon: {e}"),
                urgency: 2,
            }));
            let _ = ping(());
            return Err(e);
        }
    };

    let mut next_id: u32 = 1;
    let mut expiries: HashMap<u32, Instant> = HashMap::new();
    // FIFO of currently-outstanding ids, oldest first; used only to enforce
    // `MAX_NOTES` by evicting the oldest popup once a new one would exceed
    // it.
    let mut order: Vec<u32> = Vec::new();
    // Bus sender (unique name) that created each outstanding id, so a
    // `replaces_id` is only honoured for the notification's own sender —
    // any client could otherwise replace/re-time another app's live
    // notification by guessing its (small, sequential) id.
    let mut owners: HashMap<u32, String> = HashMap::new();

    loop {
        // Drop owner records for ids no longer outstanding (dismissed,
        // expired, closed or evicted since last time); `order` stays tiny
        // (<= MAX_NOTES), so this sweep is trivially cheap.
        owners.retain(|id, _| order.contains(id));
        // Popups the WM closed (click-dismissal or popup-cap eviction):
        // relay the reason it reported onto the bus.
        while let Ok((id, reason)) = dismissed.try_recv() {
            expiries.remove(&id);
            order.retain(|&o| o != id);
            emit_closed(conn.channel(), id, reason);
        }

        let now = Instant::now();
        let expired: Vec<u32> = expiries
            .iter()
            .filter(|&(_, t)| *t <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            expiries.remove(&id);
            order.retain(|&o| o != id);
            to_wm
                .send(NoteMsg::Close(id))
                .map_err(|_| "wm channel closed")?;
            emit_closed(conn.channel(), id, 1); // 1 = expired
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
        if msg.msg_type() != MessageType::MethodCall {
            continue;
        }
        if msg.interface().as_deref() != Some(IFACE) {
            // Calls to other interfaces (Introspectable, Properties — sent
            // by busctl/d-feet and some client libraries on first contact)
            // must still get *a* reply: dropping them leaves the caller
            // blocked until its timeout. `default_reply` builds the
            // standard UnknownMethod error (and honours NO_REPLY_EXPECTED).
            if let Some(err) = dbus::channel::default_reply(&msg) {
                reply(conn.channel(), err)?;
            }
            continue;
        }
        match msg.member().as_deref() {
            Some("Notify") => {
                let sender = msg.sender().map(|s| s.to_string()).unwrap_or_default();
                let Some((note, timeout)) = parse_notify(&msg, &mut next_id, &order, &owners, &sender)
                else {
                    continue;
                };
                let id = note.id;
                owners.insert(id, sender);
                // A `replaces_id` re-show must not inherit the replaced
                // note's deadline: clear it unconditionally, then re-arm
                // below only if the *new* notification wants a timeout
                // (a critical/never-expire replacement would otherwise keep
                // the replaced note's expiry and get auto-closed by it).
                expiries.remove(&id);
                // 0 means never expire; so does critical urgency per spec.
                match timeout {
                    _ if note.urgency >= 2 => None,
                    0 => None,
                    t if t > 0 => Some(Duration::from_millis(t as u64)),
                    _ => Some(DEFAULT_TIMEOUT),
                }
                .map(|d| expiries.insert(id, Instant::now() + d));
                order.retain(|&o| o != id); // a `replaces_id` re-show moves to newest
                order.push(id);
                // Cap outstanding notifications: evict the oldest rather
                // than let the popup pile grow without bound.
                if order.len() > MAX_NOTES {
                    let evict = order.remove(0);
                    expiries.remove(&evict);
                    to_wm
                        .send(NoteMsg::Close(evict))
                        .map_err(|_| "wm channel closed")?;
                    // No spec reason fits "evicted for space" exactly;
                    // 4 = "undefined/reserved" is the closest fit.
                    emit_closed(conn.channel(), evict, 4);
                }
                to_wm
                    .send(NoteMsg::Show(note))
                    .map_err(|_| "wm channel closed")?;
                ping(())?;
                reply(conn.channel(), msg.method_return().append1(id))?;
            }
            Some("CloseNotification") => {
                if let Ok(id) = msg.read1::<u32>() {
                    // Only act/signal for ids that are actually still
                    // outstanding — closing an already-gone id shouldn't
                    // emit a spurious NotificationClosed.
                    if order.contains(&id) {
                        expiries.remove(&id);
                        order.retain(|&o| o != id);
                        to_wm
                            .send(NoteMsg::Close(id))
                            .map_err(|_| "wm channel closed")?;
                        emit_closed(conn.channel(), id, 3); // 3 = closed by call
                        ping(())?;
                    }
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
            // Unknown members on our own interface get the standard error
            // reply too, for the same don't-leave-callers-hanging reason.
            _ => {
                if let Some(err) = dbus::channel::default_reply(&msg) {
                    reply(conn.channel(), err)?;
                }
            }
        }
    }
}

/// Decode a `Notify` call's eight arguments into a `Note` plus its raw
/// `expire_timeout`. Returns `None` on a malformed call. `outstanding` is
/// the set of still-live ids: a fresh allocation skips them (so a
/// wrapped-around `next_id` can't alias a never-expiring notification), and
/// `replaces_id` is only honoured when it names one of them *created by the
/// same bus sender* (per `owners`/`sender`) — the spec says an unknown
/// `replaces_id` behaves like a new notification, and honouring arbitrary
/// values let any bus client resurrect stale ids or (with a guessed id)
/// replace/re-time another app's live notification.
fn parse_notify(
    msg: &Message,
    next_id: &mut u32,
    outstanding: &[u32],
    owners: &HashMap<u32, String>,
    sender: &str,
) -> Option<(Note, i32)> {
    let mut it = msg.iter_init();
    let _app: String = it.read().ok()?;
    let replaces: u32 = it.read().ok()?;
    let _icon: String = it.read().ok()?;
    let summary: String = it.read().ok()?;
    let body: String = it.read().ok()?;
    let _actions: Vec<String> = it.read().ok()?;
    let hints: PropMap = it.read().ok()?;
    let timeout: i32 = it.read().ok()?;

    let id = if replaces != 0
        && outstanding.contains(&replaces)
        && owners.get(&replaces).is_some_and(|o| o == sender)
    {
        replaces
    } else {
        loop {
            let id = *next_id;
            *next_id = next_id.wrapping_add(1).max(1);
            if !outstanding.contains(&id) {
                break id;
            }
        }
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

/// Emit `NotificationClosed(id, reason)`, best-effort: a failed signal send
/// only logs — one bus hiccup must not kill notifications for the whole
/// session (a truly dead bus surfaces at the next `blocking_pop_message`).
fn emit_closed(ch: &Channel, id: u32, reason: u32) {
    let sent = Message::new_signal(PATH, IFACE, "NotificationClosed")
        .map_err(|e| format!("bad signal: {e}").into())
        .and_then(|sig| reply(ch, sig.append2(id, reason)));
    if let Err(e) = sent {
        eprintln!("splitwm: failed to emit NotificationClosed({id}): {e}");
    }
}

fn reply(ch: &Channel, msg: Message) -> R<()> {
    ch.send(msg).map_err(|()| "dbus send failed")?;
    Ok(())
}

/// The spec allows a small HTML subset in the body; we render plain text, so
/// drop tags and decode the standard entities.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // A '<' only opens a tag if *its own* '>' arrives before any other '<'
    // and the content looks like a tag ("<b>", "</a>", "<a href=...>") —
    // plain text like "1 < 2 > 3" or "<3" must pass through literally.
    // Each candidate span is consumed at most once, so this stays O(n)
    // even on hostile input (this is an unauthenticated D-Bus endpoint).
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().expect("i is on a char boundary");
        if c == '<' {
            if let Some(rel) = rest[1..].find(['<', '>']) {
                let inner = &rest[1..1 + rel];
                if rest.as_bytes()[1 + rel] == b'>'
                    && inner.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/')
                {
                    i += 1 + rel + 1; // skip the whole tag
                    continue;
                }
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    decode_entities(&out)
}

/// The character a spec entity name (between `&` and `;`) stands for: the
/// five named XML entities plus numeric `#NNN` / `#xHH` references. `None`
/// for anything else, which then passes through literally.
fn entity_char(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "amp" => Some('&'),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Decode entities in one left-to-right pass: each `&...;` span is consumed
/// exactly once, so `&amp;lt;` yields the literal `&lt;` (no re-scan of
/// decoded output) and hostile input stays O(n) — the `;` search per `&` is
/// bounded to the longest plausible entity.
fn decode_entities(s: &str) -> String {
    const MAX_ENTITY_LEN: usize = 10; // "&#x10FFFF;"
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().expect("i is on a char boundary");
        if c == '&' {
            // Byte scan: '&' and ';' are ASCII, so byte indices here are
            // always char boundaries even in multibyte text.
            let semi = rest.as_bytes()[1..]
                .iter()
                .take(MAX_ENTITY_LEN - 1)
                .position(|&b| b == b';');
            if let Some(ch) = semi.and_then(|e| entity_char(&rest[1..1 + e])) {
                out.push(ch);
                i += 1 + semi.expect("checked by and_then") + 1;
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_markup;

    #[test]
    fn strips_tags_and_decodes_entities() {
        assert_eq!(strip_markup("<b>bold</b> &amp; <i>x</i>"), "bold & x");
        assert_eq!(strip_markup("a &lt;tag&gt; b"), "a <tag> b");
    }

    #[test]
    fn amp_decoded_last_avoids_double_decode() {
        assert_eq!(strip_markup("&amp;lt;"), "&lt;");
    }

    #[test]
    fn numeric_entities_decode() {
        assert_eq!(strip_markup("&#64;&#x41;&#X61;"), "@Aa");
        // Malformed or out-of-range references pass through literally.
        assert_eq!(strip_markup("&#xZZ; &#1114112; &nope; &"), "&#xZZ; &#1114112; &nope; &");
    }

    #[test]
    fn stray_lt_without_gt_passes_through() {
        assert_eq!(strip_markup("1 < 2"), "1 < 2");
        assert_eq!(strip_markup("<b>x</b> then 1 < 2"), "x then 1 < 2");
    }

    #[test]
    fn plain_text_angle_brackets_survive() {
        // A literal '<' followed by a later literal '>' is not a tag unless
        // the span between them actually looks like one.
        assert_eq!(strip_markup("1 < 2 > 3"), "1 < 2 > 3");
        assert_eq!(strip_markup("see a<3 b>4 lol"), "see a<3 b>4 lol");
        assert_eq!(strip_markup("x <- y -> z"), "x <- y -> z");
        assert_eq!(strip_markup("<b>1<2</b>"), "1<2");
    }

    #[test]
    fn multibyte_text_survives() {
        assert_eq!(strip_markup("héllo <em>wörld</em> ✓"), "héllo wörld ✓");
    }

    #[test]
    fn pathological_lt_run_is_fast() {
        // O(n²) would take minutes on this; O(n) is instant.
        let s = "<".repeat(200_000) + ">";
        let _ = strip_markup(&s);
    }
}
