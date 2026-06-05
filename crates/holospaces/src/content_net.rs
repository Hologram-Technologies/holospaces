//! **The uor-native content network — "the browser as a router" (`CC-38`).**
//!
//! A peer reaches another peer's content the same content-addressed way on every
//! deployment surface — browser, bare-metal, or native host. The substrate
//! supplies the mechanism: [`hologram_net_bare::BareNetSync`] is a
//! [`KappaSync`](hologram_substrate_core::KappaSync) that speaks the uor-native
//! frame protocol (`fetch`/`announce`/`discover`, **verify-on-receipt** —
//! SPINE-4) over a [`NetworkInterface`](hologram_bare_hal::NetworkInterface) HAL.
//! This module supplies a *portable* `NetworkInterface` — [`PacketLink`](crate::content_net::PacketLink) — so the
//! **same** `BareNetSync` drives the content network in a wasm tab and on a
//! bare-metal board alike (the module is `no_std` + `alloc`, in the portable peer
//! core). Because both surfaces run the identical `BareNetSync` over the identical
//! frame codec, a **browser peer and a bare-metal peer interoperate by
//! construction** — the `CC-38` witness exchanges content between two such peers
//! and the bare-metal build gate (`thumbv7em-none-eabi`) compiles the same path.
//!
//! [`PacketLink`](crate::content_net::PacketLink) holds only lock-protected frame queues (`Send + Sync`, as the
//! HAL requires) — **no transport, no JS**. A *pump* connects a link to a wire:
//! a WebRTC data channel between browser tabs (a browser feature, bound in the
//! `holospaces-web` peer), a real NIC on bare metal, or — for the witness — an
//! in-process [`loopback_pair`](crate::content_net::PacketLink::loopback_pair). The link is portable
//! substrate code; only the pump is surface-specific.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
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

/// The transport-facing end of a [`PacketLink`] — the handle a *pump* uses to
/// move frames between the link and a real wire (a WebRTC data channel between
/// browser tabs, a NIC on bare metal). The pump pushes inbound frames it
/// received from the wire ([`push_inbound`](Self::push_inbound)) and drains the
/// frames the link wants to transmit ([`pop_outbound`](Self::pop_outbound)) to
/// the wire. It carries no JS and no transport itself — it is the `Send + Sync`
/// substrate seam between the portable link and the surface-specific transport.
pub struct TransportEndpoint {
    inbox: FrameQueue,
    outbox: FrameQueue,
    rx_waker: WakerCell,
}

impl TransportEndpoint {
    /// Deliver a frame received from the wire to the link, and wake any fetch
    /// awaiting a response (the transport's RX-ready signal).
    pub fn push_inbound(&self, frame: Vec<u8>) {
        self.inbox.lock().push_back(frame);
        if let Some(w) = self.rx_waker.lock().take() {
            w.wake();
        }
    }

    /// Take the next frame the link wants to transmit to the wire, if any.
    #[must_use]
    pub fn pop_outbound(&self) -> Option<Vec<u8>> {
        self.outbox.lock().pop_front()
    }
}

impl PacketLink {
    /// A **transport-backed** link and its [`TransportEndpoint`]: the link's
    /// `transmit` enqueues onto the endpoint's outbound queue (drained by the
    /// pump to the wire), and the pump's `push_inbound` feeds the link's
    /// `receive`. The single-peer counterpart to [`loopback_pair`] for a real
    /// transport — the link is a portable `NetworkInterface`, the pump (WebRTC /
    /// NIC) lives entirely outside it.
    ///
    /// [`loopback_pair`]: PacketLink::loopback_pair
    #[must_use]
    pub fn with_transport(mtu: u32) -> (PacketLink, TransportEndpoint) {
        let inbox: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
        let outbox: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
        let rx_waker: WakerCell = Arc::new(Mutex::new(None));
        let link = PacketLink {
            mac: [0x02, 0, 0, 0, 0, 0x7],
            mtu,
            inbox: inbox.clone(),
            outbox: outbox.clone(),
            rx_waker: rx_waker.clone(),
            // No in-process peer; the pump fires the wakers via the endpoint.
            peer_waker: Arc::new(Mutex::new(None)),
        };
        let endpoint = TransportEndpoint {
            inbox,
            outbox,
            rx_waker,
        };
        (link, endpoint)
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

/// A **forging** content-network peer — the adversary the verify-on-receipt law
/// (SPINE-4 / Law L5) exists to defeat. It answers **every** inbound `fetch`,
/// for any κ, with the same attacker-chosen `forged` bytes (bytes that do not
/// re-derive to the requested κ). On the wire this is a well-formed
/// `FETCH_RES_OK` frame; the fetcher's `BareNetSync` re-derives the bytes against
/// the requested κ and **rejects** them. This is not a fallback or a mock — it is
/// a genuine malicious responder, used by the witness to prove a forged response
/// is refused rather than silently accepted.
#[must_use]
pub fn forging_peer(link: PacketLink, forged: Vec<u8>) -> BareNetSync {
    let forged = Arc::new(forged);
    // Resolve *any* requested κ to the attacker's bytes — the responder claims to
    // hold every κ, and serves the forgery.
    let local_get: LocalResolver = Arc::new(move |_k| Some(Bytes::from(forged.as_ref().clone())));
    // It advertises nothing truthful; discovery returns no κs.
    let local_iter: LocalIterator = Arc::new(Vec::new);
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

/// A content-network peer bound to a **single transport** — the browser tab's
/// (or any surface's) seam to a live wire. It answers inbound fetches from its
/// `store` and fetches missing content from the peer across the transport,
/// verifying every byte on receipt (SPINE-4). The surface's *pump* (a WebRTC
/// data channel between tabs, a NIC on bare metal) carries frames via
/// [`inbound`](Self::inbound) / [`outbound`](Self::outbound); the peer itself is
/// portable and transport-agnostic. This is the encapsulated handle a deployment
/// peer holds, so it never has to touch the substrate sync type directly.
pub struct ContentPeer {
    sync: Arc<BareNetSync>,
    wire: TransportEndpoint,
}

impl ContentPeer {
    /// Bind a content peer over a fresh transport-backed link, answering fetches
    /// from `store`.
    #[must_use]
    pub fn new(mtu: u32, store: Arc<dyn KappaStore>) -> Self {
        let (link, wire) = PacketLink::with_transport(mtu);
        Self {
            sync: Arc::new(peer(link, store)),
            wire,
        }
    }

    /// Bind a **forging** content peer over a fresh transport-backed link — a
    /// malicious responder that answers every fetch with `forged` bytes (which do
    /// not re-derive to the requested κ). The honest counterpart to [`new`]; the
    /// witness uses it to prove a forged response is rejected on receipt
    /// (SPINE-4 / Law L5), carried over a real transport (a WebRTC data channel).
    ///
    /// [`new`]: Self::new
    #[must_use]
    pub fn new_forging(mtu: u32, forged: Vec<u8>) -> Self {
        let (link, wire) = PacketLink::with_transport(mtu);
        Self {
            sync: Arc::new(forging_peer(link, forged)),
            wire,
        }
    }

    /// Deliver a frame received from the transport and service it — answer an
    /// inbound request from local content, or record a response for an awaiting
    /// [`fetch`](Self::fetch) — then wake that fetch.
    pub fn inbound(&self, frame: Vec<u8>) {
        self.wire.push_inbound(frame);
        let _ = self.sync.poll();
    }

    /// Drain the next frame the peer wants to send over the transport, if any.
    #[must_use]
    pub fn outbound(&self) -> Option<Vec<u8>> {
        self.wire.pop_outbound()
    }

    /// A future fetching `kappa` from the peer across the transport, verified on
    /// receipt. Poll it as the transport delivers frames (via [`inbound`]); it
    /// resolves to the bytes, or `None` if no peer holds `kappa`. The future
    /// owns its peer handle, so it is `'static` (the caller may store and poll it
    /// across transport round-trips).
    ///
    /// [`inbound`]: Self::inbound
    #[must_use]
    pub fn fetch(&self, kappa: KappaLabel71) -> Pin<Box<dyn Future<Output = Option<Bytes>>>> {
        let sync = self.sync.clone();
        Box::pin(async move { sync.fetch(&kappa).await.ok().flatten() })
    }

    /// A future that **announces** to the peer that this node holds `kappa` — the
    /// uor-native `BareNetSync` [`announce`](KappaSync::announce), which emits a
    /// `KIND_ANNOUNCE` frame onto the link. The frame leaves over the transport's
    /// outbound queue ([`outbound`](Self::outbound)); the surface pump carries it
    /// across the wire (a WebRTC data channel between tabs). The future owns its
    /// peer handle, so it is `'static` (the caller may store and poll it across
    /// transport round-trips, exactly like [`fetch`](Self::fetch)).
    #[must_use]
    pub fn announce(&self, kappa: KappaLabel71) -> Pin<Box<dyn Future<Output = ()>>> {
        let sync = self.sync.clone();
        Box::pin(async move { sync.announce(&kappa).await })
    }

    /// A future that **discovers** which κs the peer holds — the uor-native
    /// `BareNetSync` [`discover`](KappaSync::discover). It emits a
    /// `KIND_DISCOVER_REQ` frame; the responding peer answers with a
    /// `KIND_DISCOVER_RES` listing its locally-held κs, which this peer records.
    /// The future resolves to a snapshot of the κs known so far. As with
    /// [`fetch`](Self::fetch), drive it by pumping frames over the transport
    /// ([`inbound`](Self::inbound) / [`outbound`](Self::outbound)) and polling; the
    /// owned peer handle makes it `'static`.
    #[must_use]
    pub fn discover(&self) -> Pin<Box<dyn Future<Output = Vec<KappaLabel71>>>> {
        let sync = self.sync.clone();
        Box::pin(async move { sync.discover(None, usize::MAX).await })
    }
}

/// Drive a `fetcher`'s fetch of `kappa` against a `responder`, carrying frames
/// over their [`TransportEndpoint`]s — the exact seam a live transport (a WebRTC
/// data channel) bridges, exercised here by a synchronous in-test "wire" that
/// shuttles each side's outbound frames to the other's inbound. This is the
/// reference for the surface-specific pump: the protocol is identical, only the
/// `push_inbound`/`pop_outbound` carrier differs. Returns the verified bytes, or
/// `None` if no peer holds `kappa`.
#[must_use]
pub fn drive_fetch_over_transport(
    fetcher: &BareNetSync,
    fetcher_wire: &TransportEndpoint,
    responder: &BareNetSync,
    responder_wire: &TransportEndpoint,
    kappa: &KappaLabel71,
) -> Option<Bytes> {
    let mut fut = fetcher.fetch(kappa);
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    for _ in 0..256 {
        if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
            return result.ok().flatten();
        }
        // Carry the fetcher's outbound frames over the "wire" to the responder,
        // let it answer, then carry its replies back — what the pump does each
        // time the transport signals readiness.
        while let Some(frame) = fetcher_wire.pop_outbound() {
            responder_wire.push_inbound(frame);
        }
        let _ = responder.poll();
        while let Some(frame) = responder_wire.pop_outbound() {
            fetcher_wire.push_inbound(frame);
        }
    }
    None
}
