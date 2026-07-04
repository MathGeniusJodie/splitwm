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

/// Freedesktop `NotificationClosed` reasons. The daemon emits
/// Expired/Closed itself; Dismissed/Undefined are reported by the WM over
/// the dismiss channel and relayed onto the bus. An enum rather than bare
/// `u32` constants so the channel and the signal can only ever carry one of
/// the four reasons the spec defines; the wire value is produced at the
/// bus edge (`emit_closed`).
#[derive(Clone, Copy)]
pub enum CloseReason {
    /// `expire_timeout` elapsed.
    Expired = 1,
    /// Dismissed by the user (click).
    Dismissed = 2,
    /// Closed by a `CloseNotification` call.
    Closed = 3,
    /// Evicted for space; the spec has no exact fit.
    Undefined = 4,
}

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
/// popup it closed as `(id, close reason)` — `CloseReason::Dismissed` for a
/// user click, `CloseReason::Undefined` for a popup-cap eviction — so the
/// matching `NotificationClosed` signal goes out on the bus with the truth.
/// Bus errors (no session bus, name already owned) only disable
/// notifications: they log and let the WM run on.
pub fn spawn(to_wm: Sender<NoteMsg>) -> Sender<(u32, CloseReason)> {
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

/// One outstanding notification in `serve`'s ledger. Every fact about a
/// live id travels in one record (rather than parallel id-keyed maps), so
/// an id can't linger in one bookkeeping structure after leaving another.
struct Outstanding {
    id: u32,
    /// Bus *unique* name that created the id: a `replaces_id` is only
    /// honoured for the notification's own sender — any client could
    /// otherwise replace/re-time another app's live notification by
    /// guessing its (small, sequential) id. Keying by unique name means an
    /// app that drops off the bus and reconnects cannot replace its own
    /// still-live notification; accepted, since matching on well-known
    /// names or app_name would reopen the spoofing hole.
    owner: String,
    /// When the note auto-closes; `None` never expires (timeout 0, or
    /// critical urgency per spec).
    expiry: Option<Instant>,
}

fn serve(to_wm: &Sender<NoteMsg>, dismissed: &Receiver<(u32, CloseReason)>) -> R<()> {
    // Own X connection purely for waking the WM's blocking event loop.
    // Established before the bus so a bus failure can still be shown to the
    // user as a popup (below) rather than only a stderr line nobody sees.
    let (xc, screen) = x11rb::connect(None)?;
    let root = xc.setup().roots[screen].root;
    let ping_atom = xc.intern_atom(false, PING_ATOM.as_bytes())?.reply()?.atom;
    // Best-effort: the ping only wakes the WM's blocking event loop early —
    // a transient send failure must not kill the daemon thread (the WM still
    // drains the channel on its next wakeup), so log and continue, matching
    // `emit_closed`'s policy.
    let ping = || {
        let sent = (|| -> R<()> {
            let ev = ClientMessageEvent::new(32, root, ping_atom, [0u32; 5]);
            xc.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT, ev)?;
            xc.flush()?;
            Ok(())
        })();
        if let Err(e) = sent {
            eprintln!("splitwm: failed to ping the WM event loop: {e}");
        }
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
            ping();
            return Err(e);
        }
    };

    let mut next_id: u32 = 1;
    // Outstanding notifications, oldest first — the order `MAX_NOTES`
    // eviction consumes.
    let mut notes: Vec<Outstanding> = Vec::new();

    loop {
        // Popups the WM closed (click-dismissal or popup-cap eviction):
        // relay the reason it reported onto the bus.
        while let Ok((id, reason)) = dismissed.try_recv() {
            notes.retain(|n| n.id != id);
            emit_closed(conn.channel(), id, reason);
        }

        let now = Instant::now();
        let mut expired: Vec<u32> = Vec::new();
        notes.retain(|n| {
            let done = n.expiry.is_some_and(|t| t <= now);
            if done {
                expired.push(n.id);
            }
            !done
        });
        for id in expired {
            to_wm
                .send(NoteMsg::Close(id))
                .map_err(|_| "wm channel closed")?;
            emit_closed(conn.channel(), id, CloseReason::Expired);
            ping();
        }

        // Sleep until the next expiry, but never so long that a dismissal
        // report from the WM sits unserviced.
        let wait = notes
            .iter()
            .filter_map(|n| n.expiry)
            .map(|t| t.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_millis(250))
            .clamp(Duration::from_millis(10), Duration::from_millis(250));
        let Some(msg) = conn.channel().blocking_pop_message(wait)? else {
            continue;
        };
        // The bus daemon addresses `NameLost` directly to the connection
        // that lost the name (no match rule needed: unlike broadcast
        // signals, this unicast delivery doesn't depend on subscription) —
        // it means another process out-replaced us on BUS_NAME the same way
        // `connect_bus` out-replaces a lingering daemon. From then on every
        // Notify call actually reaches the new owner, not us: continuing to
        // run would make this thread a dead letterbox forever, silently
        // doing nothing while the WM still believes notifications work.
        if msg.msg_type() == MessageType::Signal
            && msg.interface().as_deref() == Some("org.freedesktop.DBus")
            && msg.member().as_deref() == Some("NameLost")
            && msg.read1::<&str>().ok() == Some(BUS_NAME)
        {
            let _ = to_wm.send(NoteMsg::Show(Note {
                id: 0,
                summary: "Notifications unavailable".to_string(),
                body: "another process took over splitwm's notification daemon role".to_string(),
                urgency: 2,
            }));
            ping();
            return Err(format!("lost ownership of {BUS_NAME}").into());
        }
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
                let Some((note, timeout)) = parse_notify(&msg, &mut next_id, &notes, &sender)
                else {
                    error_reply(
                        conn.channel(),
                        &msg,
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "malformed Notify call",
                    )?;
                    continue;
                };
                let id = note.id;
                // 0 means never expire; so does critical urgency per spec.
                let expiry = match timeout {
                    _ if note.urgency >= 2 => None,
                    0 => None,
                    t if t > 0 => Some(Instant::now() + Duration::from_millis(t as u64)),
                    _ => Some(Instant::now() + DEFAULT_TIMEOUT),
                };
                // A `replaces_id` re-show moves to newest and carries the
                // *new* expiry — inheriting the replaced note's deadline
                // would auto-close a critical/never-expire replacement.
                notes.retain(|n| n.id != id);
                notes.push(Outstanding {
                    id,
                    owner: sender,
                    expiry,
                });
                // Cap outstanding notifications: evict the oldest rather
                // than let the popup pile grow without bound.
                if notes.len() > MAX_NOTES {
                    let evict = notes.remove(0);
                    to_wm
                        .send(NoteMsg::Close(evict.id))
                        .map_err(|_| "wm channel closed")?;
                    // No spec reason fits "evicted for space" exactly;
                    // undefined/reserved is the closest fit.
                    emit_closed(conn.channel(), evict.id, CloseReason::Undefined);
                }
                to_wm
                    .send(NoteMsg::Show(note))
                    .map_err(|_| "wm channel closed")?;
                ping();
                reply(conn.channel(), msg.method_return().append1(id))?;
            }
            Some("CloseNotification") => {
                // The spec requires an error reply when the id doesn't name
                // an outstanding notification — success would also make the
                // daemon emit a spurious NotificationClosed.
                match msg.read1::<u32>() {
                    Ok(id) if notes.iter().any(|n| n.id == id) => {
                        notes.retain(|n| n.id != id);
                        to_wm
                            .send(NoteMsg::Close(id))
                            .map_err(|_| "wm channel closed")?;
                        emit_closed(conn.channel(), id, CloseReason::Closed);
                        ping();
                        reply(conn.channel(), msg.method_return())?;
                    }
                    _ => error_reply(
                        conn.channel(),
                        &msg,
                        "org.freedesktop.Notifications.Error.InvalidId",
                        "no such notification",
                    )?,
                }
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
/// the still-live ledger: a fresh allocation skips its ids (so a
/// wrapped-around `next_id` can't alias a never-expiring notification), and
/// `replaces_id` is only honoured when it names an entry *created by the
/// same bus sender* (see `Outstanding::owner`) — the spec says an unknown
/// `replaces_id` behaves like a new notification, and honouring arbitrary
/// values let any bus client resurrect stale ids or (with a guessed id)
/// replace/re-time another app's live notification.
fn parse_notify(
    msg: &Message,
    next_id: &mut u32,
    outstanding: &[Outstanding],
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
        && outstanding
            .iter()
            .any(|n| n.id == replaces && n.owner == sender)
    {
        replaces
    } else {
        loop {
            let id = *next_id;
            *next_id = next_id.wrapping_add(1).max(1);
            if !outstanding.iter().any(|n| n.id == id) {
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
            summary: strip_markup(cap_chars(&summary, NOTE_TEXT_CAP)),
            body: strip_markup(cap_chars(&body, NOTE_TEXT_CAP)),
            urgency,
        },
        timeout,
    ))
}

/// Cap on stored summary/body length, in chars. The bus is unauthenticated
/// and imposes no length limit of its own, while the popup renderer can show
/// only a few hundred chars — storing more per note would keep megabytes of
/// hostile input alive (and re-feed them to markup stripping and text
/// wrapping) for the popup's whole lifetime. Generously above what any
/// bubble can display so no legitimate note is ever visibly truncated.
const NOTE_TEXT_CAP: usize = 4096;

/// Truncate to at most `cap` chars, on a char boundary.
pub(crate) fn cap_chars(s: &str, cap: usize) -> &str {
    match s.char_indices().nth(cap) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Emit `NotificationClosed(id, reason)`, best-effort: a failed signal send
/// only logs — one bus hiccup must not kill notifications for the whole
/// session (a truly dead bus surfaces at the next `blocking_pop_message`).
fn emit_closed(ch: &Channel, id: u32, reason: CloseReason) {
    let sent = Message::new_signal(PATH, IFACE, "NotificationClosed")
        .map_err(|e| format!("bad signal: {e}").into())
        .and_then(|sig| reply(ch, sig.append2(id, reason as u32)));
    if let Err(e) = sent {
        eprintln!("splitwm: failed to emit NotificationClosed({id}): {e}");
    }
}

fn reply(ch: &Channel, msg: Message) -> R<()> {
    ch.send(msg).map_err(|()| "dbus send failed")?;
    Ok(())
}

/// Reply to `msg` with a D-Bus error: a malformed or unknown call still
/// needs *a* reply, or the caller blocks until its own timeout.
fn error_reply(ch: &Channel, msg: &Message, name: &str, text: &str) -> R<()> {
    reply(
        ch,
        msg.error(
            &name.into(),
            &std::ffi::CString::new(text).expect("static string has no NUL"),
        ),
    )
}

/// The spec allows a small HTML subset in the body; we render plain text, so
/// drop tags and decode the standard entities.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // A '<' only opens a tag if it is followed by tag-shaped content
    // ("<b>", "</a>", "<a href=...>") closed by a '>' outside any quoted
    // attribute value, before any unquoted '<' — plain text like
    // "1 < 2 > 3" or "<3" must pass through literally, and a '>' inside a
    // quoted attribute value (href="a>b") does not end the tag. The
    // tag-shape check on the first character bounds the quote-aware scan
    // to genuine tag candidates, so hostile input (this is an
    // unauthenticated D-Bus endpoint) stays O(n) in practice.
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().expect("i is on a char boundary");
        if c == '<' {
            if let Some(end) = tag_end(&rest[1..]) {
                i += 1 + end + 1; // skip the whole tag
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    decode_entities(&out)
}

/// Byte offset (within `inner`, the text after a '<') of the '>' closing a
/// tag-shaped span, or `None` when the span isn't a tag: content must start
/// like a tag name, and the '>' must come before any '<', ignoring both
/// characters inside single- or double-quoted attribute values.
fn tag_end(inner: &str) -> Option<usize> {
    if !inner.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/') {
        return None;
    }
    let mut quote: Option<u8> = None;
    for (j, &b) in inner.as_bytes().iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(j),
                b'<' => return None,
                _ => {}
            },
        }
    }
    None
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
        assert_eq!(
            strip_markup("&#xZZ; &#1114112; &nope; &"),
            "&#xZZ; &#1114112; &nope; &"
        );
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
    fn gt_inside_quoted_attribute_does_not_end_the_tag() {
        assert_eq!(strip_markup(r#"<a href="x>y">link</a>"#), "link");
        assert_eq!(strip_markup("<a title='a>b'>t</a>"), "t");
        // A quote of the other kind inside a quoted value is plain data.
        assert_eq!(strip_markup(r#"<a title="it's>fine">t</a>"#), "t");
        // An unterminated quoted value never closes: not a tag.
        assert_eq!(strip_markup(r#"<a href=" oops"#), r#"<a href=" oops"#);
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
