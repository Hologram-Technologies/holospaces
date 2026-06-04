//! # holospaces-node
//!
//! The holospaces **bare-metal / edge peer** — a node for low-powered devices
//! that a browser tab routes through. holospaces keeps a devcontainer's *compute*
//! in the browser (the emulator boots a real OS in the tab), but a tab has no
//! NIC and no durable storage. A flashed node supplies both, as a first-class
//! holospaces peer (the substrate runs on **browser and bare-metal** alike, Law
//! L1) — not a bespoke external proxy:
//!
//! - [`egress`] — the **exit node**: it forwards the browser guest's arbitrary
//!   TCP to the real internet (`apt`/`pip`/`npm`, a `git` clone, an outbound
//!   socket), speaking the same egress framing the browser's `WsEgress` already
//!   uses (`CC-16`). A device you own is the tab's route to the internet.
//! - [`storage`] — **storage-sync**: a persistent, file-backed `KappaStore`
//!   (`hologram-store-native`) served over the substrate's HTTP-CAS protocol
//!   (`GET /cas/{κ}`), so the operator's content survives across browser reloads
//!   and devices, fetched trustlessly (verify-on-receipt, `CC-20`/`CC-38`).
//! - [`ota`] — **OTA from GitHub Pages**: the node fetches its own updates from
//!   the holospaces Pages site, κ-addressed and verified by re-derivation (Law L5;
//!   the cold-start gateway as the update source — a forged update is refused),
//!   and stages them for the next restart.
//!
//! The node is plain `std` so it cross-compiles to small Linux SBCs; the
//! `no_std` content-network core it shares with the browser
//! (`holospaces::content_net`, `CC-38`) is what lets the smallest microcontroller
//! variants participate too.

pub mod egress;
pub mod ota;
pub mod storage;

pub use egress::EgressServer;

use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::Duration;

use tungstenite::error::Error as WsError;
use tungstenite::{accept, Message};

/// How often a connection's loop wakes to drain host bytes to the tab when the
/// browser is sending nothing — bounded so replies are timely without a busy
/// spin on a low-powered node.
const TURN_TIMEOUT: Duration = Duration::from_millis(20);

/// Serve one browser peer over its **egress WebSocket** (`WsEgress`, `CC-16`):
/// complete the WebSocket handshake on `stream`, then shuttle the guest's egress
/// frames (`OPEN`/`DATA`/`CLOSE`) to an [`EgressServer`] — which forwards them to
/// the real internet — and frame the host's replies back to the tab, until
/// either side hangs up. Plain blocking I/O, one connection per node thread —
/// right for a low-powered device, and the unit on which the node's egress is
/// tested end-to-end over a real WebSocket.
pub fn serve_connection(stream: TcpStream) {
    let mut ws = match accept(stream) {
        Ok(ws) => ws,
        Err(_) => return, // not a WebSocket client / handshake failed
    };
    // Bound each read so the loop drains host bytes even while the tab is quiet
    // (set after the handshake so a slow handshake is not cut off).
    let _ = ws.get_mut().set_read_timeout(Some(TURN_TIMEOUT));

    let mut egress = EgressServer::new();
    loop {
        // 1) A guest frame from the tab (OPEN / DATA / CLOSE), if one is ready.
        match ws.read() {
            Ok(Message::Binary(frame)) => {
                for out in egress.handle_frame(&frame) {
                    if ws.send(Message::Binary(out)).is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {} // ping/pong/text — ignore
            // A read timeout means no frame this turn — fall through to poll.
            Err(WsError::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(_) => return, // connection error / closed
        }

        // 2) Deliver any host bytes / closes back to the tab.
        for out in egress.poll() {
            if ws.send(Message::Binary(out)).is_err() {
                return;
            }
        }
        if ws.flush().is_err() {
            return;
        }
    }
}
