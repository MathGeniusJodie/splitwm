//! The `org.freedesktop.Notifications` daemon: the compositor doubles as
//! the session's notification server, on zbus instead of master's libdbus.
//!
//! Shape: the interface (id allocation, replaces validation, the
//! outstanding ledger) lives on zbus's object server; a small ticker
//! thread owns everything time- and relay-shaped — expiring notes,
//! relaying compositor-reported dismissals onto the bus, emitting queued
//! `NotificationClosed` signals. Notes and closes reach the compositor
//! over a calloop channel, so nothing here ever touches the event loop.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zbus::zvariant::Value;

const BUS_NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// `expire_timeout: -1` ("server decides") becomes this.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on outstanding notifications; the oldest is evicted past this.
pub const MAX_NOTES: usize = 8;

/// Freedesktop `NotificationClosed` reasons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseReason {
    /// `expire_timeout` elapsed.
    Expired = 1,
    /// The user dismissed it (clicked the bubble).
    Dismissed = 2,
    /// A `CloseNotification` call.
    Closed = 3,
    /// Evicted for space (no spec reason fits; undefined is the closest).
    Undefined = 4,
}

/// One notification's payload, as the compositor renders it.
#[derive(Clone)]
pub struct Note {
    pub id: u32,
    pub summary: String,
    pub body: String,
    /// 0 low / 1 normal / 2 critical (freedesktop urgency hint).
    pub urgency: u8,
}

pub enum NoteMsg {
    Show(Note),
    Close(u32),
}

/// Cap pathological text sizes once, at the door.
const NOTE_TEXT_CAP: usize = 4096;

pub(crate) fn cap_chars(s: &str, cap: usize) -> &str {
    match s.char_indices().nth(cap) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

struct Outstanding {
    id: u32,
    /// Unique bus name of the caller; `replaces_id` is honoured only for
    /// the same sender (an unknown id behaves like a new notification per
    /// spec, and honouring arbitrary values would let any client replace
    /// another app's live notification).
    owner: String,
    /// When the note auto-closes; `None` never expires (timeout 0, or
    /// critical urgency per spec).
    expiry: Option<Instant>,
}

struct Notifications {
    next_id: u32,
    notes: Vec<Outstanding>,
    to_comp: smithay::reexports::calloop::channel::Sender<NoteMsg>,
    /// Closed-signal emissions queued for the ticker thread: emitting from
    /// inside an interface method would block zbus's own executor.
    pending_closed: Vec<(u32, CloseReason)>,
}

impl Notifications {
    fn allocate_id(&mut self, replaces_id: u32, sender: &str) -> u32 {
        if replaces_id != 0
            && self
                .notes
                .iter()
                .any(|n| n.id == replaces_id && n.owner == sender)
        {
            return replaces_id;
        }
        // Fresh id, skipping any still-outstanding one so a wrapped-around
        // counter can't alias a never-expiring notification.
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.notes.iter().any(|n| n.id == self.next_id) {
                return self.next_id;
            }
        }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        _app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> u32 {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let id = self.allocate_id(replaces_id, &sender);
        let urgency = hints
            .get("urgency")
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(1);
        // 0 means never expire; so does critical urgency per spec.
        let expiry = match expire_timeout {
            _ if urgency >= 2 => None,
            0 => None,
            t if t > 0 => Some(Instant::now() + Duration::from_millis(t as u64)),
            _ => Some(Instant::now() + DEFAULT_TIMEOUT),
        };
        // A replaces_id re-show moves to newest and carries the *new*
        // expiry — inheriting the replaced note's deadline would
        // auto-close a critical/never-expire replacement.
        self.notes.retain(|n| n.id != id);
        self.notes.push(Outstanding {
            id,
            owner: sender,
            expiry,
        });
        // Cap outstanding notifications: evict the oldest rather than let
        // the popup pile grow without bound.
        if self.notes.len() > MAX_NOTES {
            let evict = self.notes.remove(0);
            let _ = self.to_comp.send(NoteMsg::Close(evict.id));
            self.pending_closed.push((evict.id, CloseReason::Undefined));
        }
        let _ = self.to_comp.send(NoteMsg::Show(Note {
            id,
            summary: cap_chars(summary, NOTE_TEXT_CAP).to_string(),
            body: cap_chars(body, NOTE_TEXT_CAP).to_string(),
            urgency,
        }));
        id
    }

    fn close_notification(&mut self, id: u32) -> zbus::fdo::Result<()> {
        // The spec requires an error when the id doesn't name an
        // outstanding notification — success would also make the daemon
        // emit a spurious NotificationClosed.
        if !self.notes.iter().any(|n| n.id == id) {
            return Err(zbus::fdo::Error::Failed("no such notification".into()));
        }
        self.notes.retain(|n| n.id != id);
        let _ = self.to_comp.send(NoteMsg::Close(id));
        self.pending_closed.push((id, CloseReason::Closed));
        Ok(())
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "splitwm".into(),
            "splitwm".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

/// Start the daemon. Notes/closes arrive on `to_comp`; the returned sender
/// reports compositor-side dismissals as `(id, reason)` so the
/// `NotificationClosed` signal lands on the bus. If the bus name can't be
/// taken (or is lost later), a critical "notifications unavailable" note is
/// injected through `to_comp` so the failure is visible on screen.
pub fn spawn(
    to_comp: smithay::reexports::calloop::channel::Sender<NoteMsg>,
) -> mpsc::Sender<(u32, CloseReason)> {
    let (dismiss_tx, dismiss_rx) = mpsc::channel::<(u32, CloseReason)>();
    let comp2 = to_comp.clone();
    std::thread::spawn(move || {
        if let Err(err) = serve(to_comp, &dismiss_rx) {
            tracing::warn!("notification daemon unavailable: {err}");
            let _ = comp2.send(NoteMsg::Show(Note {
                id: 0,
                summary: "notifications unavailable".into(),
                body: err.to_string(),
                urgency: 2,
            }));
        }
    });
    dismiss_tx
}

fn serve(
    to_comp: smithay::reexports::calloop::channel::Sender<NoteMsg>,
    dismissed: &mpsc::Receiver<(u32, CloseReason)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::blocking::connection::Builder::session()?
        .serve_at(
            PATH,
            Notifications {
                next_id: 0,
                notes: Vec::new(),
                to_comp,
                pending_closed: Vec::new(),
            },
        )?
        .build()?;
    // Take the well-known name the way master did: out-replace a lingering
    // daemon, and allow being out-replaced in turn.
    use zbus::fdo::RequestNameFlags;
    let reply = zbus::blocking::fdo::DBusProxy::new(&conn)?.request_name(
        BUS_NAME.try_into()?,
        RequestNameFlags::AllowReplacement | RequestNameFlags::ReplaceExisting,
    )?;
    use zbus::fdo::RequestNameReply;
    if !matches!(
        reply,
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
    ) {
        return Err(format!("{BUS_NAME} is owned by another daemon").into());
    }

    let iface = conn.object_server().interface::<_, Notifications>(PATH)?;

    // Everything time- and relay-shaped, at master's <=250ms cadence:
    // expiries, compositor-reported dismissals, queued Closed signals.
    let mut signals: Vec<(u32, CloseReason)> = Vec::new();
    let mut expired: Vec<u32> = Vec::new();
    loop {
        {
            let mut guard = iface.get_mut();
            while let Ok((id, reason)) = dismissed.try_recv() {
                guard.notes.retain(|n| n.id != id);
                signals.push((id, reason));
            }
            let now = Instant::now();
            for n in &guard.notes {
                if n.expiry.is_some_and(|t| t <= now) {
                    expired.push(n.id);
                }
            }
            for id in expired.drain(..) {
                guard.notes.retain(|n| n.id != id);
                let _ = guard.to_comp.send(NoteMsg::Close(id));
                signals.push((id, CloseReason::Expired));
            }
            signals.append(&mut guard.pending_closed);
        }
        for (id, reason) in signals.drain(..) {
            let _ = zbus::block_on(Notifications::notification_closed(
                iface.signal_emitter(),
                id,
                reason as u32,
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
