//! **The uor-native content network — "the browser as a router" (`CC-38`).**
//!
//! A peer reaches another peer's content the same content-addressed way on every
//! deployment surface — browser, bare-metal, or native host. The substrate
//! supplies the mechanism: [`hologram_net_bare::BareNetSync`] is a [`KappaSync`]
//! that speaks the uor-native frame protocol (`fetch`/`announce`/`discover`,
//! **verify-on-receipt** — SPINE-4) over a [`NetworkInterface`] HAL. This module
//! supplies a *portable* `NetworkInterface` — [`PacketLink`] — so the **same**
//! `BareNetSync` drives the content network in a wasm tab and on a bare-metal
//! board alike (the module is `no_std` + `alloc`, in the portable peer core).
//! Because both surfaces run the identical `BareNetSync` over the identical
//! frame codec, a **browser peer and a bare-metal peer interoperate by
//! construction** — the `CC-38` witness exchanges content between two such peers
//! and the bare-metal build gate (`thumbv7em-none-eabi`) compiles the same path.
//!
//! [`PacketLink`] holds only lock-protected frame queues (`Send + Sync`, as the
//! HAL requires) — **no transport, no JS**. A *pump* connects a link to a wire:
//! a WebRTC data channel between browser tabs (a browser feature, bound in the
//! `holospaces-web` peer), a real NIC on bare metal, or — for the witness — an
//! in-process [`loopback_pair`](PacketLink::loopback_pair). The link is portable
//! substrate code; only the pump is surface-specific.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::task::{Context, Poll, Waker};

use hologram_bare_hal::{NetworkInterface, NicError};
use hologram_net_bare::{BareNetSync, LocalIterator, LocalResolver};
use hologram_substrate_core::{Bytes, KappaLabel71, KappaStore, KappaSync};
use spin::Mutex;

/// A frame queue shared between a link and its peer / transport pump.
type FrameQueue = Arc<Mutex<VecDeque<Vec<u8>>>>;
/// The waker a link fires when a frame is delivered to a waiting peer.
type WakerCell = Arc<Mutex<Option<Waker>>>;

/// One end of a packet link — a [`NetworkInterface`] over two frame queues.
///
/// `inbox` holds frames destined for *this* end (drained by
/// [`receive`](NetworkInterface::receive)); `outbox` is the peer's inbox (filled
/// by [`transmit`](NetworkInterface::transmit)). A transport pump (a WebRTC data
/// channel, a real NIC, or the loopback pairing) connects the two ends; the link
/// itself is transport-agnostic, `Send + Sync` substrate code.
pub struct PacketLink {
    mac: [u8; 6],
    mtu: u32,
    inbox: FrameQueue,
    outbox: FrameQueue,
    rx_waker: WakerCell,
    /// The peer's rx waker — fired by *our* `transmit` so the peer wakes on a new frame.
    peer_waker: WakerCell,
}

impl PacketLink {
    /// A handle to feed frames *into* this link from a transport pump (a WebRTC
    /// data channel's `onmessage` pushes inbound bytes here, then wakes the
    /// awaiting fetch). Returns this end's inbox queue and rx waker cell.
    #[must_use]
    pub fn ingress(&self) -> (FrameQueue, WakerCell) {
        (self.inbox.clone(), self.rx_waker.clone())
    }

    /// A handle to drain frames *out of* this link to a transport pump (the send
    /// side of a WebRTC data channel drains the outbox to the wire).
    #[must_use]
    pub fn egress(&self) -> FrameQueue {
        self.outbox.clone()
    }

    /// Cross-wire two links so a frame transmitted on one is received on the
    /// other — an in-process peer link with no external transport. Each end's
    /// `outbox` is the other's `inbox`, and each `transmit` fires the other's rx
    /// waker. The substrate's content network, peer-to-peer, no server between
    /// (the loopback stands in for a real wire / WebRTC data channel).
    #[must_use]
    pub fn loopback_pair(mtu: u32) -> (PacketLink, PacketLink) {
        let a_inbox: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
        let b_inbox: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
        let a_waker: WakerCell = Arc::new(Mutex::new(None));
        let b_waker: WakerCell = Arc::new(Mutex::new(None));
        let a = PacketLink {
            mac: [0x02, 0, 0, 0, 0, 0xa],
            mtu,
            inbox: a_inbox.clone(),
            outbox: b_inbox.clone(),
            rx_waker: a_waker.clone(),
            peer_waker: b_waker.clone(),
        };
        let b = PacketLink {
            mac: [0x02, 0, 0, 0, 0, 0xb],
            mtu,
            inbox: b_inbox,
            outbox: a_inbox,
            rx_waker: b_waker,
            peer_waker: a_waker,
        };
        (a, b)
    }
}

impl NetworkInterface for PacketLink {
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn mtu(&self) -> u32 {
        self.mtu
    }

    fn transmit(&self, frame: &[u8]) -> Result<usize, NicError> {
        self.outbox.lock().push_back(frame.to_vec());
        // Wake the peer's awaiting fetch so it drains the frame we just delivered.
        if let Some(w) = self.peer_waker.lock().take() {
            w.wake();
        }
        Ok(frame.len())
    }

    fn receive(&self, buffer: &mut [u8]) -> Result<usize, NicError> {
        let Some(frame) = self.inbox.lock().pop_front() else {
            return Ok(0);
        };
        if frame.len() > buffer.len() {
            // The MTU bounds frame size; a frame larger than the caller's buffer
            // is a protocol violation, not silent truncation (SPINE-6).
            return Err(NicError::HardwareFault(frame.len() as u32));
        }
        buffer[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }

    fn register_rx_waker(&self, waker: Waker) {
        *self.rx_waker.lock() = Some(waker);
    }
}

/// Wrap a content `store` and a [`PacketLink`] into a substrate content-network
/// peer ([`BareNetSync`]) — it answers inbound `fetch` requests from `store` and
/// fetches missing content from the link's peer, verifying every byte on receipt
/// (SPINE-4). The same call builds a browser peer and a bare-metal peer.
#[must_use]
pub fn peer(link: PacketLink, store: Arc<dyn KappaStore>) -> BareNetSync {
    let get_store = store.clone();
    let local_get: LocalResolver = Arc::new(move |k| get_store.get(k).ok().flatten());
    let local_iter: LocalIterator = Arc::new(move || store.iterate());
    BareNetSync::new(Arc::new(link), local_get, local_iter)
}

/// Drive a `fetcher`'s uor-native [`fetch`](KappaSync::fetch) of `kappa` to
/// completion against a directly-linked `responder`, over an in-process
/// [`PacketLink`] pair. The fetch future suspends awaiting the response frame;
/// because the loopback is synchronous, a bounded tick loop — poll the fetch,
/// then let the responder answer the pending request — converges without a full
/// async runtime (the content-network protocol exchange, exact and
/// self-contained). Returns the **verified** bytes, or `None` if no peer holds
/// `kappa` (a response that re-derives wrong is rejected inside `BareNetSync`).
///
/// A live deployment drives the same `fetch` future from the transport's
/// RX-ready notification (`register_rx_waker`) instead of this synchronous tick;
/// the protocol is identical.
#[must_use]
pub fn drive_fetch(
    fetcher: &BareNetSync,
    responder: &BareNetSync,
    kappa: &KappaLabel71,
) -> Option<Bytes> {
    // `BareNetSync::fetch` (async_trait) returns an already-boxed, pinned future.
    let mut fut = fetcher.fetch(kappa);
    // The synchronous loopback driver observes wakes by the next poll, not by
    // scheduling, so a no-op waker is exactly right (no `unsafe` hand-rolled vtable).
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    // REQ → (responder answers) → RES settles in a couple of ticks; the bound is
    // a fail-loud backstop, not a policy retry count (SPINE-6).
    for _ in 0..256 {
        if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
            return result.ok().flatten();
        }
        // Let the responder drain the request we just sent and reply.
        let _ = responder.poll();
    }
    None
}
