//! **CC-49 — the content network's live browser transport: a real WebRTC data
//! channel between two browser peers.**
//!
//! `CC-38` proved the uor-native content-network *protocol* ([`BareNetSync`] over
//! a portable `NetworkInterface`) is identical on every surface, with an in-test
//! pump standing in for the link. This module binds the **surface-specific
//! transport** the `CC-38` / ADR-006 note named as the open frontier: a genuine
//! [`RtcDataChannel`](web_sys::RtcDataChannel), peer-to-peer between two browser
//! tabs, with **no central operator** (Law L1, UOR-native: no server). The data
//! channel is the wire; signaling (the SDP offer/answer and ICE candidates) is
//! exchanged *out of band* by the peers themselves — pasted between tabs, carried
//! over an existing peer, or relayed by any content-blind channel — never by a
//! bespoke server.
//!
//! [`WebRtcLink`] is the browser surface's **transport**: it owns an
//! [`RtcPeerConnection`](web_sys::RtcPeerConnection) and one ordered, reliable
//! data channel and shuttles opaque content-network frames across it
//! ([`send`](WebRtcLink::send) / [`recv`](WebRtcLink::recv)). It is the
//! browser-side analog of a real NIC's TX/RX: the `NetworkInterface` the
//! uor-native `BareNetSync` actually drives is the portable
//! [`PacketLink`](holospaces::content_net::PacketLink) inside a
//! [`Console`](crate::Console)'s content peer — the **same** interface, with the
//! **same** `BareNetSync`, a bare-metal peer drives (`CC-38`). The product pump
//! [`Console::cn_pump`](crate::Console::cn_pump) couples the two — draining the
//! link's `PacketLink` onto this channel and feeding the channel's frames back
//! into it — so a deployed tab fetches over WebRTC entirely through the product
//! API, with no test glue (`CC-49`).
//!
//! The link never inspects a frame and never touches content addressing:
//! verify-on-receipt (SPINE-4 / Law L5) happens inside the content peer, exactly
//! as on every other surface. A forged response carried over the channel is
//! therefore rejected on re-derivation, and a κ no peer holds resolves to nothing
//! — the channel changes the carrier, not the law.
//!
//! [`BareNetSync`]: holospaces::content_net

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use js_sys::Reflect;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelType, RtcIceCandidate, RtcIceCandidateInit, RtcPeerConnection,
    RtcPeerConnectionIceEvent, RtcSdpType, RtcSessionDescriptionInit,
};

/// The data channel's `onmessage` callback — queues each inbound frame.
type MessageClosure = Closure<dyn FnMut(MessageEvent)>;
/// The data channel's `onopen` callback — marks the channel ready.
type OpenClosure = Closure<dyn FnMut(JsValue)>;

/// State the data-channel callbacks fill and the [`WebRtcLink`] drains: the
/// frames received from the wire (queued for the content peer) and the local ICE
/// candidates gathered (queued for out-of-band signaling to the remote peer).
struct Shared {
    /// Inbound content-network frames received on the data channel, in order.
    inbound: VecDeque<Vec<u8>>,
    /// Local ICE candidates gathered so far, as JSON strings to hand to the peer.
    local_ice: VecDeque<String>,
    /// The open data channel, once negotiated and ready (`onopen`). Until then
    /// outbound frames are buffered by the caller's pump (it checks [`is_open`]).
    channel: Option<RtcDataChannel>,
    open: bool,
    /// The answerer's channel closures (its channel arrives via `ondatachannel`,
    /// after construction); held here so they outlive the connection. The
    /// offerer's channel closures are held on the link directly.
    answerer_closures: Vec<JsValue>,
}

/// One end of a peer-to-peer content-network transport over a real WebRTC data
/// channel — the browser surface's wire. It carries a [`Console`](crate::Console)'s
/// content-network frames to and from another browser peer (no server between);
/// the product pump [`Console::cn_pump`](crate::Console::cn_pump) couples it to
/// the `BareNetSync`-driven `NetworkInterface`, so a deployed tab fetches over it.
#[wasm_bindgen]
pub struct WebRtcLink {
    pc: RtcPeerConnection,
    shared: Rc<RefCell<Shared>>,
    // The closures must outlive the connection; held so they are not dropped.
    _on_ice: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
    _on_datachannel: Closure<dyn FnMut(RtcDataChannelEvent)>,
    /// The message/open closures for the channel this end *created* (the offerer);
    /// the answerer's channel arrives via `ondatachannel` and wires its own.
    _on_message: RefCell<Option<MessageClosure>>,
    _on_open: RefCell<Option<OpenClosure>>,
}

/// Normalize a session description's line endings to CRLF, as RFC 4566 requires.
/// An SDP carried out of band between peers — pasted into a text box, copied
/// through the clipboard, or shuttled by a content-blind channel — routinely
/// loses its `\r`s (browsers normalize text-area input to `\n`). `RTCPeerConnection`
/// then rejects it ("Failed to parse SessionDescription … Invalid SDP line"), so
/// the deployed paste-between-tabs signaling needs this hygiene at the seam. A
/// line already ending in CRLF is left as is (we strip a stray `\r` then re-add).
fn normalize_sdp(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len() + 16);
    for line in sdp.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // SDP terminates every line, including the last, with CRLF.
        if line.is_empty() && out.ends_with("\r\n") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// Attach `onmessage` (queue inbound frames) and `onopen` (mark ready) to a data
/// channel, returning the closures to keep alive. Shared by the offerer (which
/// creates the channel) and the answerer (which receives it via `ondatachannel`).
fn wire_channel(
    channel: &RtcDataChannel,
    shared: &Rc<RefCell<Shared>>,
) -> (MessageClosure, OpenClosure) {
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);

    let s_msg = shared.clone();
    let on_message = Closure::wrap(Box::new(move |e: MessageEvent| {
        let data = e.data();
        // Binary frames arrive as ArrayBuffer (set above). A content-network frame
        // is opaque bytes; the content peer parses and verifies it.
        if data.is_instance_of::<js_sys::ArrayBuffer>() {
            let bytes = js_sys::Uint8Array::new(&data).to_vec();
            s_msg.borrow_mut().inbound.push_back(bytes);
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let s_open = shared.clone();
    let chan = channel.clone();
    let on_open = Closure::wrap(Box::new(move |_e: JsValue| {
        let mut s = s_open.borrow_mut();
        s.open = true;
        s.channel = Some(chan.clone());
    }) as Box<dyn FnMut(JsValue)>);
    channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    (on_message, on_open)
}

#[wasm_bindgen]
impl WebRtcLink {
    /// Open one end of a peer-to-peer link.
    ///
    /// `initiator` is the offerer: it creates the data channel and the SDP offer
    /// ([`create_offer`](Self::create_offer)). The other end is the answerer: it
    /// receives the channel via `ondatachannel` after
    /// [`accept_offer`](Self::accept_offer). Either end can then fetch from the
    /// other — the content network is symmetric, no client/server roles.
    ///
    /// With no `iceServers` configured the connection uses only **host
    /// candidates** (loopback / LAN) — sufficient for two peers reachable to each
    /// other directly, and entirely serverless. A deployment may add STUN/TURN for
    /// NAT traversal without changing this transport or the protocol it carries.
    #[wasm_bindgen(constructor)]
    pub fn new(initiator: bool) -> Result<WebRtcLink, JsValue> {
        let config = RtcConfiguration::new();
        let pc = RtcPeerConnection::new_with_configuration(&config)?;

        let shared = Rc::new(RefCell::new(Shared {
            inbound: VecDeque::new(),
            local_ice: VecDeque::new(),
            channel: None,
            open: false,
            answerer_closures: Vec::new(),
        }));

        // Gather local ICE candidates for out-of-band signaling to the peer.
        let s_ice = shared.clone();
        let on_ice = Closure::wrap(Box::new(move |e: RtcPeerConnectionIceEvent| {
            if let Some(cand) = e.candidate() {
                // Serialize the candidate to a stable JSON the peer can re-hydrate.
                let obj = js_sys::Object::new();
                let _ = Reflect::set(&obj, &"candidate".into(), &cand.candidate().into());
                let _ = Reflect::set(
                    &obj,
                    &"sdpMid".into(),
                    &cand.sdp_mid().map_or(JsValue::NULL, JsValue::from),
                );
                let _ = Reflect::set(
                    &obj,
                    &"sdpMLineIndex".into(),
                    &cand.sdp_m_line_index().map_or(JsValue::NULL, JsValue::from),
                );
                if let Ok(json) = js_sys::JSON::stringify(&obj) {
                    if let Some(j) = json.as_string() {
                        s_ice.borrow_mut().local_ice.push_back(j);
                    }
                }
            }
        }) as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
        pc.set_onicecandidate(Some(on_ice.as_ref().unchecked_ref()));

        let on_message_cell: RefCell<Option<MessageClosure>> = RefCell::new(None);
        let on_open_cell: RefCell<Option<OpenClosure>> = RefCell::new(None);

        if initiator {
            // The offerer creates the (ordered, reliable) channel up front.
            let init = RtcDataChannelInit::new();
            init.set_ordered(true);
            let channel = pc.create_data_channel_with_data_channel_dict("uor-content-net", &init);
            let (m, o) = wire_channel(&channel, &shared);
            *on_message_cell.borrow_mut() = Some(m);
            *on_open_cell.borrow_mut() = Some(o);
        }

        // The answerer receives the channel via ondatachannel; wire it then and
        // park its closures in `shared` so they outlive this event (and the link).
        let s_dc = shared.clone();
        let on_datachannel = Closure::wrap(Box::new(move |e: RtcDataChannelEvent| {
            let channel = e.channel();
            let (m, o) = wire_channel(&channel, &s_dc);
            let mut s = s_dc.borrow_mut();
            s.answerer_closures.push(m.into_js_value());
            s.answerer_closures.push(o.into_js_value());
        }) as Box<dyn FnMut(RtcDataChannelEvent)>);
        pc.set_ondatachannel(Some(on_datachannel.as_ref().unchecked_ref()));

        Ok(WebRtcLink {
            pc,
            shared,
            _on_ice: on_ice,
            _on_datachannel: on_datachannel,
            _on_message: on_message_cell,
            _on_open: on_open_cell,
        })
    }

    /// (Offerer) Create the SDP offer and set it as the local description; returns
    /// the offer SDP to hand to the peer out of band (paste / existing peer).
    pub async fn create_offer(&self) -> Result<String, JsValue> {
        let offer = JsFuture::from(self.pc.create_offer()).await?;
        let sdp = Reflect::get(&offer, &"sdp".into())?
            .as_string()
            .ok_or_else(|| JsValue::from_str("offer has no sdp"))?;
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        desc.set_sdp(&sdp);
        JsFuture::from(self.pc.set_local_description(&desc)).await?;
        Ok(sdp)
    }

    /// (Answerer) Accept the peer's offer SDP, set it remote, create the answer
    /// and set it local; returns the answer SDP to hand back to the peer.
    pub async fn accept_offer(&self, offer_sdp: String) -> Result<String, JsValue> {
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        remote.set_sdp(&normalize_sdp(&offer_sdp));
        JsFuture::from(self.pc.set_remote_description(&remote)).await?;

        let answer = JsFuture::from(self.pc.create_answer()).await?;
        let sdp = Reflect::get(&answer, &"sdp".into())?
            .as_string()
            .ok_or_else(|| JsValue::from_str("answer has no sdp"))?;
        let local = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        local.set_sdp(&sdp);
        JsFuture::from(self.pc.set_local_description(&local)).await?;
        Ok(sdp)
    }

    /// (Offerer) Accept the peer's answer SDP, completing the negotiation.
    pub async fn accept_answer(&self, answer_sdp: String) -> Result<(), JsValue> {
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        remote.set_sdp(&normalize_sdp(&answer_sdp));
        JsFuture::from(self.pc.set_remote_description(&remote)).await?;
        Ok(())
    }

    /// Add a remote ICE candidate (the JSON the peer produced via
    /// [`take_ice`](Self::take_ice)) to this connection.
    pub async fn add_ice(&self, candidate_json: String) -> Result<(), JsValue> {
        let obj = js_sys::JSON::parse(&candidate_json)?;
        let cand = Reflect::get(&obj, &"candidate".into())?
            .as_string()
            .unwrap_or_default();
        let init = RtcIceCandidateInit::new(&cand);
        if let Some(mid) = Reflect::get(&obj, &"sdpMid".into())?.as_string() {
            init.set_sdp_mid(Some(&mid));
        }
        if let Some(idx) = Reflect::get(&obj, &"sdpMLineIndex".into())?.as_f64() {
            init.set_sdp_m_line_index(Some(idx as u16));
        }
        let cand = RtcIceCandidate::new(&init)?;
        JsFuture::from(
            self.pc
                .add_ice_candidate_with_opt_rtc_ice_candidate(Some(&cand)),
        )
        .await?;
        Ok(())
    }

    /// Drain the local ICE candidates gathered so far, as JSON strings to hand to
    /// the peer out of band. Call repeatedly while negotiating (candidates arrive
    /// over a few event-loop turns).
    #[must_use]
    pub fn take_ice(&self) -> Vec<JsValue> {
        let mut s = self.shared.borrow_mut();
        s.local_ice.drain(..).map(JsValue::from).collect()
    }

    /// Whether the data channel is open and ready to carry frames.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.shared.borrow().open
    }

    /// Send a content-network frame to the peer over the data channel. The pump
    /// drains a [`Console`](crate::Console)'s `cn_outbound` and sends each frame
    /// here. Returns an error if the channel is not yet open (the pump should wait
    /// for [`is_open`](Self::is_open)).
    pub fn send(&self, frame: &[u8]) -> Result<(), JsValue> {
        let s = self.shared.borrow();
        match s.channel.as_ref() {
            Some(ch) => ch.send_with_u8_array(frame),
            None => Err(JsValue::from_str("data channel not open")),
        }
    }

    /// Take the next content-network frame received from the peer over the data
    /// channel, or `undefined` if none is queued. The pump feeds each into a
    /// [`Console`](crate::Console)'s `cn_inbound`.
    #[must_use]
    pub fn recv(&self) -> Option<Vec<u8>> {
        self.shared.borrow_mut().inbound.pop_front()
    }

    /// Close the connection and its data channel.
    pub fn close(&self) {
        if let Some(ch) = self.shared.borrow().channel.as_ref() {
            ch.close();
        }
        self.pc.close();
    }
}
