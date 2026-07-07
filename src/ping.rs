//! Wakes the WM's blocking event loop from a background thread by sending a
//! `ClientMessage` to root (delivered to the WM's own connection via the
//! `SUBSTRUCTURE_REDIRECT` selection only a WM holds). Shared by the
//! notification-daemon thread (`crate::notify`) and background theme-icon
//! fetch threads (`crate::wm::icons`) so a fresh connect/auth handshake is
//! paid once for the process rather than once per thread or per ping.

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{Atom, ClientMessageEvent, ConnectionExt as _, EventMask};
use x11rb::rust_connection::RustConnection;

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// The connection every ping goes out over, plus the root window
/// `send_event` targets it at.
struct PingConn {
    xc: RustConnection,
    root: u32,
}

/// Shared across every caller for the lifetime of the process.
/// `RustConnection` is `Send + Sync` (its I/O is internally synchronized), so
/// unlike the WM's own connection — not safe to touch off the main thread —
/// this one is fine to share. `None` means an earlier connect attempt
/// failed; that's cached rather than retried, since a missed ping only
/// delays a result already sitting in its own channel until the WM's next
/// natural wakeup, never loses it, so paying a fresh connect-and-auth
/// handshake for no better outcome isn't worth it.
static PING: std::sync::OnceLock<Option<PingConn>> = std::sync::OnceLock::new();

/// Wake the WM's blocking event loop by sending `atom` as a `ClientMessage` to
/// root. Best-effort: a failed ping (whether the connect on first use or this
/// send) only delays whatever the caller already queued elsewhere (an mpsc
/// message, a D-Bus reply) until the WM's next natural wakeup, so failures
/// are logged and otherwise ignored rather than killing the calling thread.
/// Takes an already-interned `atom` rather than a name: atoms are
/// server-global, so a value interned on the WM's own connection (or any
/// connection) is valid here too, sparing every ping an extra round trip to
/// re-intern it.
pub fn ping(atom: Atom) {
    let conn = PING.get_or_init(|| match x11rb::connect(None) {
        Ok((xc, screen)) => {
            let root = xc.setup().roots[screen].root;
            Some(PingConn { xc, root })
        }
        Err(e) => {
            eprintln!("splitwm: failed to open the event-loop ping connection: {e}");
            None
        }
    });
    let Some(PingConn { xc, root }) = conn else {
        return;
    };
    let send = || -> R<()> {
        let ev = ClientMessageEvent::new(32, *root, atom, [0u32; 5]);
        xc.send_event(false, *root, EventMask::SUBSTRUCTURE_REDIRECT, ev)?;
        xc.flush()?;
        Ok(())
    };
    if let Err(e) = send() {
        eprintln!("splitwm: failed to ping the WM event loop: {e}");
    }
}
