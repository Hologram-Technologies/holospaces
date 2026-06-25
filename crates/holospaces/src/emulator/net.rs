//! **CC-16 — a userspace TCP/IP NAT with a pluggable egress transport.**
//!
//! A devcontainer is not a dev environment if it can't `git clone`, `apt-get`,
//! or `npm install` from the open internet. The guest OS has a real `virtio-net`
//! NIC (the [device](super) drives it), but there is no raw NIC behind a browser
//! tab — so the guest's Ethernet frames are terminated by a **userspace TCP/IP
//! stack** here (a slirp-style NAT: ARP + DHCP + the guest-facing TCP state
//! machine) and the *payload* streams are carried out over a pluggable
//! [`Egress`] transport (ADR-014). Natively that egress is a host socket
//! ([`StdEgress`]); in the browser it is a WebSocket tunnel to a relay. The NAT
//! itself is transport-agnostic and compiles to `wasm32` with the rest of the
//! peer core — only the egress seam differs per peer.
//!
//! The differential oracle is `qemu-system-riscv64`'s own user-mode (slirp)
//! network: the same kernel boots, does DHCP, opens a TCP connection through the
//! NAT, and completes an HTTP exchange. The NAT reproduces the slirp addressing
//! (`10.0.2.0/24`, gateway `10.0.2.2`) so the same guest software runs unchanged.
//!
//! Authorities: RFC 826 (ARP), RFC 791 (IPv4), RFC 768 (UDP), RFC 2131 (DHCP),
//! RFC 9293 (TCP).

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use core::cell::RefCell;

/// The IPv4 address the NAT leases the guest (slirp's default). The same address
/// `qemu-system-riscv64 -netdev user` hands out, so the oracle and the emulator
/// agree.
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
/// The NAT's gateway / DHCP-server address (`10.0.2.2`).
pub const GW_IP: [u8; 4] = [10, 0, 2, 2];
/// The DNS address the NAT advertises (`10.0.2.3`).
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
/// The subnet mask handed out (`255.255.255.0` — a `/24`).
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];
/// The MAC the NAT answers ARP with — the gateway / all-of-the-subnet MAC the
/// guest learns for every off-host address.
pub const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
/// The MAC the `virtio-net` device reports to the guest (its NIC address); the
/// guest sources its frames from it.
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

const ET_IPV4: u16 = 0x0800;
const ET_ARP: u16 = 0x0806;
const IP_PROTO_UDP: u8 = 17;
const IP_PROTO_TCP: u8 = 6;

// TCP flag bits.
const F_FIN: u8 = 0x01;
const F_SYN: u8 = 0x02;
const F_RST: u8 = 0x04;
const F_PSH: u8 = 0x08;
const F_ACK: u8 = 0x10;

/// The window the NAT advertises to the guest (no window scaling is negotiated,
/// so this is a literal byte count ≤ 65535).
const RECV_WINDOW: u16 = 0xFAF0;
/// The maximum segment the NAT emits toward the guest.
const MSS: usize = 1460;

/// The maximum number of concurrent NAT connections (guest-initiated + forwarded
/// inbound). A guest or a flood of inbound connections cannot make the connection
/// table grow without bound: a new connection beyond this is refused (the guest's
/// SYN is dropped → standard TCP backpressure; an inbound accept is closed). The
/// idle reaper keeps long-dead entries from holding slots.
const MAX_CONNS: usize = 256;

/// The per-connection cap on buffered guest→host bytes ([`Conn::from_guest`]).
/// When the buffer is at this cap the NAT stops accepting new guest payload (it
/// does not advance `rcv_nxt`, so the guest retransmits) and shrinks the
/// advertised window, applying real TCP backpressure instead of buffering without
/// limit.
const FROM_GUEST_CAP: usize = 256 * 1024;

/// The per-connection cap on buffered host→guest bytes ([`Conn::to_guest`]).
/// When at this cap the NAT stops draining the egress (host-side backpressure),
/// so a fast server cannot make the NAT buffer grow without bound.
const TO_GUEST_CAP: usize = 256 * 1024;

/// Idle polls before a connection is reaped. The NAT is pumped each device poll;
/// a connection with no activity for this many polls (a dead peer, a lost FIN, a
/// `TIME-WAIT` that has lingered) is closed and its slot freed, bounding the
/// table's residency over time.
const IDLE_REAP_POLLS: u32 = 60_000;

/// The status of an egress connection — what the NAT polls to drive the
/// guest-facing TCP state machine (open the connection, relay, tear down).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EgressStatus {
    /// The host-side connection is still being established.
    Connecting,
    /// The host-side connection is up; bytes flow both ways.
    Open,
    /// The host-side connection is closed (by the peer or on error).
    Closed,
}

/// A **pluggable egress transport** — the seam between the in-emulator TCP/IP NAT
/// and the world outside the peer (ADR-014). The NAT hands it the guest's TCP
/// *payload* streams keyed by an opaque connection id; an implementation carries
/// them to the real internet however the host allows: a direct socket on a
/// native/codespace peer ([`StdEgress`]), or a WebSocket tunnel to a relay in the
/// browser. Every method is non-blocking — the NAT polls.
pub trait Egress {
    /// Open a host-side TCP connection to `ip:port`; returns an opaque id the NAT
    /// uses for the lifetime of the connection. The connection may still be
    /// [`EgressStatus::Connecting`] when this returns.
    fn connect(&mut self, ip: [u8; 4], port: u16) -> u32;
    /// The current status of connection `id`.
    fn status(&mut self, id: u32) -> EgressStatus;
    /// Drain whatever bytes have arrived from the host side (empty if none are
    /// available right now — the call never blocks).
    fn recv(&mut self, id: u32) -> Vec<u8>;
    /// Send `data` toward the host side (buffered and flushed; never blocks).
    fn send(&mut self, id: u32, data: &[u8]);
    /// Close connection `id` and release its resources.
    fn close(&mut self, id: u32);
}

/// A **forwarded-port ingress transport** (`CC-21`) — the dual of [`Egress`]: it
/// accepts *inbound* connections from outside the peer (a host listener on a
/// forwarded port natively; a relay-served route in the browser) and bridges
/// each to a server *inside* the devcontainer, so the running-app preview a
/// Codespace surfaces works. The NAT polls it; for each accepted connection it
/// opens a TCP connection toward the guest's listening port. Non-blocking.
pub trait Ingress {
    /// Poll for a newly-accepted inbound connection: `(id, guest_port)` — a
    /// connection arrived on a forwarded port destined for the guest's listening
    /// `guest_port`. `None` if none is waiting (never blocks).
    fn poll_accept(&mut self) -> Option<(u32, u16)>;
    /// The status of connection `id`.
    fn status(&mut self, id: u32) -> EgressStatus;
    /// Drain bytes that arrived from the outside client (empty if none).
    fn recv(&mut self, id: u32) -> Vec<u8>;
    /// Send the guest server's reply back toward the outside client.
    fn send(&mut self, id: u32, data: &[u8]);
    /// Close connection `id`.
    fn close(&mut self, id: u32);
    /// Begin forwarding `guest_port` *after boot* — the live network
    /// reconfiguration the control plane drives (ADR-018, `CC-28`): the panel
    /// forwards a port and the *running* instance starts forwarding it. Returns
    /// the host port the new route is reachable on, or `None` if this transport
    /// cannot add a forward live. Default: unsupported.
    fn add_forward(&mut self, _guest_port: u16) -> Option<u16> {
        None
    }
}

/// A no-op ingress — the default when no port is forwarded (the machine has no
/// inbound connections to service).
pub struct NoIngress;
impl Ingress for NoIngress {
    fn poll_accept(&mut self) -> Option<(u32, u16)> {
        None
    }
    fn status(&mut self, _id: u32) -> EgressStatus {
        EgressStatus::Closed
    }
    fn recv(&mut self, _id: u32) -> Vec<u8> {
        Vec::new()
    }
    fn send(&mut self, _id: u32, _data: &[u8]) {}
    fn close(&mut self, _id: u32) {}
}

/// The state of one guest-originated TCP connection the NAT is bridging.
struct Conn {
    g_ip: [u8; 4],
    g_port: u16,
    r_ip: [u8; 4],
    r_port: u16,
    /// Next sequence number expected from the guest (their byte stream).
    rcv_nxt: u32,
    /// Next sequence number the NAT will send to the guest.
    snd_nxt: u32,
    /// Oldest of the NAT's sent bytes the guest has not yet acknowledged.
    snd_una: u32,
    /// Our initial send sequence (the SYN-ACK's seq).
    iss: u32,
    /// The guest's advertised receive window (unscaled).
    guest_wnd: u32,
    /// The host-side transport connection id this is bridged to — an
    /// [`Egress`] id for a guest-initiated connection, or an [`Ingress`] id for
    /// a forwarded inbound one (`ingress_id`).
    eid: u32,
    /// For a forwarded *inbound* connection (`CC-21`), the [`Ingress`] id; `None`
    /// for an ordinary guest-initiated egress connection. When set, the NAT is
    /// the *active opener* toward the guest (it sent the SYN), and payload is
    /// bridged to the ingress transport rather than the egress.
    ingress_id: Option<u32>,
    /// Whether the NAT has emitted its SYN-ACK yet (it waits for the egress to
    /// reach [`EgressStatus::Open`]). For an ingress connection this is set once
    /// the NAT's own SYN has been sent.
    synack_sent: bool,
    /// Whether the connection is open (the guest acknowledged the SYN-ACK, or —
    /// for ingress — the NAT acknowledged the guest's SYN-ACK).
    established: bool,
    /// Host-side bytes awaiting segmentation toward the guest.
    to_guest: VecDeque<u8>,
    /// Guest-side bytes awaiting delivery to the host transport (drained in
    /// [`Self::poll`] for egress, [`Self::poll_ingress`] for ingress).
    from_guest: VecDeque<u8>,
    /// Whether the guest has half-closed (sent FIN).
    guest_fin: bool,
    /// Whether the NAT has sent its own FIN.
    fin_sent: bool,
    /// Polls since the last activity on this connection (reset whenever bytes flow
    /// or state advances). Reaped once it exceeds [`IDLE_REAP_POLLS`] so a dead
    /// peer / lingering `TIME-WAIT` cannot hold a table slot forever.
    idle: u32,
}

/// The userspace network — terminates the guest's link layer and bridges its TCP
/// streams to an [`Egress`]. The [device](super) feeds it the frames the guest
/// transmits ([`Self::on_guest_frame`]), pumps it ([`Self::poll`]) so host-side
/// data and connection events become frames, and drains
/// [`Self::take_rx`] into the guest's receive queue.
pub struct Nat {
    /// Frames queued for delivery to the guest's `virtio-net` receive queue.
    rx: VecDeque<Vec<u8>>,
    /// Active TCP connections.
    conns: Vec<Conn>,
    /// The guest's MAC, learned from its frames (defaults to [`GUEST_MAC`]).
    guest_mac: [u8; 6],
    /// A monotonically increasing source of initial sequence numbers.
    iss_gen: u32,
}

impl Default for Nat {
    fn default() -> Self {
        Self::new()
    }
}

impl Nat {
    /// A fresh NAT with no connections and an empty receive queue.
    #[must_use]
    pub fn new() -> Self {
        Nat {
            rx: VecDeque::new(),
            conns: Vec::new(),
            guest_mac: GUEST_MAC,
            iss_gen: 0x1000,
        }
    }

    /// The next frame to hand to the guest's receive queue, if any.
    pub fn take_rx(&mut self) -> Option<Vec<u8>> {
        self.rx.pop_front()
    }

    /// Whether any frame is waiting for the guest.
    #[must_use]
    pub fn has_rx(&self) -> bool {
        !self.rx.is_empty()
    }

    fn next_iss(&mut self) -> u32 {
        // A deterministic, well-separated ISS per connection (reproducible — no
        // wall-clock or RNG, which the emulator forbids).
        self.iss_gen = self.iss_gen.wrapping_add(0x0001_0000);
        self.iss_gen
    }

    /// Handle one Ethernet frame the guest transmitted (its `virtio-net` TX
    /// queue). May enqueue reply frames and may open/feed/close egress
    /// connections.
    pub fn on_guest_frame(&mut self, frame: &[u8], egress: &mut dyn Egress) {
        if frame.len() < 14 {
            return;
        }
        // Learn the guest's source MAC.
        self.guest_mac.copy_from_slice(&frame[6..12]);
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let payload = &frame[14..];
        match ethertype {
            ET_ARP => self.handle_arp(payload),
            ET_IPV4 => self.handle_ipv4(payload, egress),
            _ => {}
        }
    }

    /// Pump the egress: advance pending connections (emit SYN-ACK / RST), pull
    /// host-side bytes, segment them toward the guest, and emit FINs as
    /// connections close.
    pub fn poll(&mut self, egress: &mut dyn Egress) {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut i = 0;
        while i < self.conns.len() {
            // Ingress (forwarded inbound) connections are serviced in
            // poll_ingress; here we drive only egress (guest-initiated) ones.
            if self.conns[i].ingress_id.is_some() {
                i += 1;
                continue;
            }
            let status = egress.status(self.conns[i].eid);
            // 1. Bring the connection up (or refuse it).
            if !self.conns[i].synack_sent {
                match status {
                    EgressStatus::Open => {
                        let c = &mut self.conns[i];
                        c.synack_sent = true;
                        let opts = [2u8, 4, (MSS >> 8) as u8, MSS as u8]; // MSS option
                        frames.push(tcp_to_guest(c, F_SYN | F_ACK, &opts, &[], self.guest_mac));
                        c.snd_nxt = c.iss.wrapping_add(1);
                    }
                    EgressStatus::Closed => {
                        // The host refused the connection — RST the guest.
                        let c = &mut self.conns[i];
                        frames.push(tcp_to_guest(c, F_RST | F_ACK, &[], &[], self.guest_mac));
                        egress.close(c.eid);
                        self.conns.remove(i);
                        continue;
                    }
                    EgressStatus::Connecting => {}
                }
            }
            // 2. Relay both ways while the connection is established.
            if self.conns[i].established {
                // guest → host
                let out: Vec<u8> = self.conns[i].from_guest.drain(..).collect();
                if !out.is_empty() {
                    egress.send(self.conns[i].eid, &out);
                    self.conns[i].idle = 0;
                }
                // host → guest, with backpressure: stop pulling from the egress
                // once the to-guest buffer is full, so a fast server cannot make
                // the NAT buffer grow without bound (the bytes stay in the host
                // transport until the guest has drained what we hold).
                if self.conns[i].to_guest.len() < TO_GUEST_CAP {
                    let data = egress.recv(self.conns[i].eid);
                    if !data.is_empty() {
                        self.conns[i].to_guest.extend(data);
                        self.conns[i].idle = 0;
                    }
                }
                self.segment_to_guest(i, &mut frames);
                // 3. Close: once the host side is closed and everything is drained
                //    and acknowledged, FIN the guest.
                let drained = self.conns[i].to_guest.is_empty()
                    && self.conns[i].snd_una == self.conns[i].snd_nxt;
                if status == EgressStatus::Closed && drained && !self.conns[i].fin_sent {
                    let c = &mut self.conns[i];
                    frames.push(tcp_to_guest(c, F_FIN | F_ACK, &[], &[], self.guest_mac));
                    c.snd_nxt = c.snd_nxt.wrapping_add(1);
                    c.fin_sent = true;
                }
            }
            // 4. Reap a fully-closed connection, or one idle past the threshold
            //    (a dead peer / lingering TIME-WAIT) — bounding the table over time.
            self.conns[i].idle = self.conns[i].idle.saturating_add(1);
            let c = &self.conns[i];
            let closed = c.guest_fin && c.fin_sent && c.snd_una == c.snd_nxt;
            if closed || c.idle >= IDLE_REAP_POLLS {
                egress.close(c.eid);
                self.conns.remove(i);
                continue;
            }
            i += 1;
        }
        for f in frames {
            self.rx.push_back(f);
        }
    }

    /// Service the **forwarded inbound** (port-forward) connections (`CC-21`) —
    /// the ingress dual of [`Self::poll`]. Accept new inbound connections from
    /// the [`Ingress`] transport (the NAT opens a connection *to* the guest's
    /// listening port — it is the active opener), and relay each established one
    /// both ways. A server the devcontainer runs is thereby reachable from
    /// outside as a forwarded port (the app-preview capability).
    pub fn poll_ingress(&mut self, ingress: &mut dyn Ingress) {
        // 1. Accept any new inbound connections → open toward the guest.
        while let Some((iid, guest_port)) = ingress.poll_accept() {
            let iss = self.next_iss();
            let r_port = 49152u16.wrapping_add((iss & 0x3fff) as u16); // synthetic client port
            let mut c = Conn {
                g_ip: GUEST_IP,
                g_port: guest_port,
                r_ip: GW_IP, // the connection appears to come from the gateway
                r_port,
                rcv_nxt: 0, // learned from the guest's SYN-ACK
                snd_nxt: iss,
                snd_una: iss,
                iss,
                guest_wnd: RECV_WINDOW as u32,
                eid: iid,
                ingress_id: Some(iid),
                synack_sent: true, // the NAT's SYN is its handshake send
                established: false,
                to_guest: VecDeque::new(),
                from_guest: VecDeque::new(),
                guest_fin: false,
                fin_sent: false,
                idle: 0,
            };
            // Bounded connection table: refuse the inbound connection if the table
            // is full (close it rather than letting a flood balloon the table).
            if self.conns.len() >= MAX_CONNS {
                ingress.close(iid);
                continue;
            }
            // Send the SYN to the guest's listener (the NAT is the active opener).
            let opts = [2u8, 4, (MSS >> 8) as u8, MSS as u8];
            let frame = tcp_to_guest(&c, F_SYN, &opts, &[], self.guest_mac);
            c.snd_nxt = c.iss.wrapping_add(1); // the SYN consumes one sequence
            self.conns.push(c);
            self.rx.push_back(frame);
        }

        // 2. Relay each established ingress connection both ways.
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut i = 0;
        while i < self.conns.len() {
            let Some(iid) = self.conns[i].ingress_id else {
                i += 1;
                continue;
            };
            if self.conns[i].established {
                // guest → host (the server's reply to the external client)
                let out: Vec<u8> = self.conns[i].from_guest.drain(..).collect();
                if !out.is_empty() {
                    ingress.send(iid, &out);
                    self.conns[i].idle = 0;
                }
                // host → guest (the external client's request), with the same
                // to-guest backpressure as the egress path: stop pulling once the
                // buffer is full so a flooding client cannot balloon the NAT.
                if self.conns[i].to_guest.len() < TO_GUEST_CAP {
                    let data = ingress.recv(iid);
                    if !data.is_empty() {
                        self.conns[i].to_guest.extend(data);
                        self.conns[i].idle = 0;
                    }
                }
                self.segment_to_guest(i, &mut frames);
                let drained = self.conns[i].to_guest.is_empty()
                    && self.conns[i].snd_una == self.conns[i].snd_nxt;
                if ingress.status(iid) == EgressStatus::Closed && drained && !self.conns[i].fin_sent
                {
                    let c = &mut self.conns[i];
                    frames.push(tcp_to_guest(c, F_FIN | F_ACK, &[], &[], self.guest_mac));
                    c.snd_nxt = c.snd_nxt.wrapping_add(1);
                    c.fin_sent = true;
                }
            }
            // Once the guest has closed (its server finished the response and
            // closed the socket) and we have relayed everything, close the
            // outside client — promptly, without waiting for the guest to
            // acknowledge our FIN (a one-shot server may already be gone). The
            // response was drained to the ingress in the relay step above. Also
            // reap a connection idle past the threshold (a dead client / lost FIN)
            // — but only once the *ingress (host) side has closed*: while the host
            // transport is still Open the connection is live by definition (the
            // in-process bridge's workbench, or a forwarded-port client, is
            // holding it open and may send at any time — e.g. a guest server
            // `accept()`ing and blocking in `read()` is not a dead peer). Idle
            // is measured in poll cycles, and the run loop pumps far faster than
            // wall-clock, so without this an idle-but-open bridge connection
            // (`CC-33`) would be reaped out from under the host before it writes.
            self.conns[i].idle = self.conns[i].idle.saturating_add(1);
            let host_open = ingress.status(iid) == EgressStatus::Open;
            let c = &self.conns[i];
            let closed = c.guest_fin && c.fin_sent && c.to_guest.is_empty();
            if closed || (!host_open && c.idle >= IDLE_REAP_POLLS) {
                ingress.close(iid);
                self.conns.remove(i);
                continue;
            }
            i += 1;
        }
        for f in frames {
            self.rx.push_back(f);
        }
    }

    /// Emit data segments for connection `i` while the guest's window and the
    /// buffered host bytes allow.
    fn segment_to_guest(&mut self, i: usize, frames: &mut Vec<Vec<u8>>) {
        loop {
            let c = &self.conns[i];
            if c.to_guest.is_empty() {
                break;
            }
            let in_flight = c.snd_nxt.wrapping_sub(c.snd_una);
            let window = c.guest_wnd.saturating_sub(in_flight) as usize;
            if window == 0 {
                break;
            }
            let n = c.to_guest.len().min(MSS).min(window);
            let payload: Vec<u8> = self.conns[i].to_guest.drain(..n).collect();
            let c = &mut self.conns[i];
            frames.push(tcp_to_guest(
                c,
                F_PSH | F_ACK,
                &[],
                &payload,
                self.guest_mac,
            ));
            c.snd_nxt = c.snd_nxt.wrapping_add(n as u32);
        }
    }

    /// Answer an ARP request: the NAT owns every address in the subnet except the
    /// guest's own (it is the gateway for everything off-host), so it replies for
    /// any requested address with [`GW_MAC`] (slirp behaviour; RFC 826).
    fn handle_arp(&mut self, p: &[u8]) {
        if p.len() < 28 {
            return;
        }
        let oper = u16::from_be_bytes([p[6], p[7]]);
        if oper != 1 {
            return; // only requests
        }
        let sha = &p[8..14];
        let spa = &p[14..18];
        let tpa = [p[24], p[25], p[26], p[27]];
        if tpa == GUEST_IP {
            return; // not ours to answer
        }
        let mut arp = Vec::with_capacity(28);
        arp.extend_from_slice(&[0, 1]); // htype Ethernet
        arp.extend_from_slice(&ET_IPV4.to_be_bytes()); // ptype IPv4
        arp.push(6); // hlen
        arp.push(4); // plen
        arp.extend_from_slice(&[0, 2]); // oper reply
        arp.extend_from_slice(&GW_MAC); // sender hw = the gateway MAC
        arp.extend_from_slice(&tpa); // sender proto = the requested address
        arp.extend_from_slice(sha); // target hw = the asker
        arp.extend_from_slice(spa); // target proto = the asker
        let frame = eth(self.guest_mac, GW_MAC, ET_ARP, &arp);
        self.rx.push_back(frame);
    }

    fn handle_ipv4(&mut self, p: &[u8], egress: &mut dyn Egress) {
        if p.len() < 20 {
            return;
        }
        let ihl = ((p[0] & 0x0f) as usize) * 4;
        if p.len() < ihl {
            return;
        }
        let proto = p[9];
        let src = [p[12], p[13], p[14], p[15]];
        let dst = [p[16], p[17], p[18], p[19]];
        let l4 = &p[ihl..];
        match proto {
            IP_PROTO_UDP => self.handle_udp(src, dst, l4),
            IP_PROTO_TCP => self.handle_tcp(src, dst, l4, egress),
            _ => {}
        }
    }

    fn handle_udp(&mut self, _src: [u8; 4], _dst: [u8; 4], u: &[u8]) {
        if u.len() < 8 {
            return;
        }
        let dport = u16::from_be_bytes([u[2], u[3]]);
        let payload = &u[8..];
        // The only UDP service the NAT provides is DHCP (the guest's bootp client
        // targets server port 67).
        if dport == 67 {
            self.handle_dhcp(payload);
        }
    }

    /// The DHCP server (RFC 2131): answer DISCOVER with OFFER and REQUEST with
    /// ACK, leasing the guest [`GUEST_IP`] with the gateway, mask, and DNS.
    fn handle_dhcp(&mut self, d: &[u8]) {
        if d.len() < 240 || u32::from_be_bytes([d[236], d[237], d[238], d[239]]) != 0x6382_5363 {
            return; // not a BOOTP/DHCP message with the magic cookie
        }
        let xid = [d[4], d[5], d[6], d[7]];
        let flags = [d[10], d[11]];
        let chaddr = [d[28], d[29], d[30], d[31], d[32], d[33]];
        // Find option 53 (DHCP message type).
        let mut msg_type = 0u8;
        let mut i = 240;
        while i + 1 < d.len() {
            let opt = d[i];
            if opt == 255 {
                break;
            }
            if opt == 0 {
                i += 1;
                continue;
            }
            let len = d[i + 1] as usize;
            if opt == 53 && len == 1 && i + 2 < d.len() {
                msg_type = d[i + 2];
            }
            i += 2 + len;
        }
        // DISCOVER(1) → OFFER(2); REQUEST(3) → ACK(5).
        let reply_type = match msg_type {
            1 => 2,
            3 => 5,
            _ => return,
        };

        let mut m = Vec::with_capacity(300);
        m.push(2); // op = BOOTREPLY
        m.push(1); // htype = Ethernet
        m.push(6); // hlen
        m.push(0); // hops
        m.extend_from_slice(&xid);
        m.extend_from_slice(&[0, 0]); // secs
        m.extend_from_slice(&flags); // flags (echo broadcast bit)
        m.extend_from_slice(&[0, 0, 0, 0]); // ciaddr
        m.extend_from_slice(&GUEST_IP); // yiaddr — the leased address
        m.extend_from_slice(&GW_IP); // siaddr — next server
        m.extend_from_slice(&[0, 0, 0, 0]); // giaddr
        m.extend_from_slice(&chaddr); // chaddr (6) + 10 pad
        m.extend_from_slice(&[0u8; 10]);
        m.extend_from_slice(&[0u8; 64]); // sname
        m.extend_from_slice(&[0u8; 128]); // file
        m.extend_from_slice(&0x6382_5363u32.to_be_bytes()); // magic cookie
        m.extend_from_slice(&[53, 1, reply_type]); // message type
        m.extend_from_slice(&[54, 4]); // server identifier
        m.extend_from_slice(&GW_IP);
        m.extend_from_slice(&[51, 4]); // lease time
        m.extend_from_slice(&86400u32.to_be_bytes());
        m.extend_from_slice(&[1, 4]); // subnet mask
        m.extend_from_slice(&NETMASK);
        m.extend_from_slice(&[3, 4]); // router
        m.extend_from_slice(&GW_IP);
        m.extend_from_slice(&[6, 4]); // DNS
        m.extend_from_slice(&DNS_IP);
        m.push(255); // end

        let udp = build_udp(67, 68, &m);
        let ip = build_ipv4(GW_IP, [255, 255, 255, 255], IP_PROTO_UDP, &udp);
        // The guest is still unconfigured; address the frame to its hardware MAC.
        let frame = eth(chaddr, GW_MAC, ET_IPV4, &ip);
        self.rx.push_back(frame);
    }

    /// The guest-facing TCP state machine (RFC 9293) — the guest is always the
    /// active opener; the NAT terminates the connection and bridges its byte
    /// stream to the egress.
    fn handle_tcp(&mut self, src: [u8; 4], dst: [u8; 4], t: &[u8], egress: &mut dyn Egress) {
        if t.len() < 20 {
            return;
        }
        let sport = u16::from_be_bytes([t[0], t[1]]);
        let dport = u16::from_be_bytes([t[2], t[3]]);
        let seq = u32::from_be_bytes([t[4], t[5], t[6], t[7]]);
        let ack = u32::from_be_bytes([t[8], t[9], t[10], t[11]]);
        let data_off = ((t[12] >> 4) as usize) * 4;
        let flags = t[13];
        let window = u16::from_be_bytes([t[14], t[15]]) as u32;
        if t.len() < data_off {
            return;
        }
        let payload = &t[data_off..];

        let idx = self
            .conns
            .iter()
            .position(|c| c.g_port == sport && c.r_ip == dst && c.r_port == dport);

        // A new connection: SYN with no prior state.
        if flags & F_SYN != 0 && idx.is_none() {
            // Bounded connection table: drop the SYN if the table is full. The
            // guest will retransmit (standard TCP backpressure); the idle reaper
            // frees dead slots, so a connection flood cannot balloon the table.
            if self.conns.len() >= MAX_CONNS {
                return;
            }
            let eid = egress.connect(dst, dport);
            let iss = self.next_iss();
            self.conns.push(Conn {
                g_ip: src,
                g_port: sport,
                r_ip: dst,
                r_port: dport,
                rcv_nxt: seq.wrapping_add(1), // past the SYN
                snd_nxt: iss,
                snd_una: iss,
                iss,
                guest_wnd: window,
                eid,
                ingress_id: None,
                synack_sent: false,
                established: false,
                to_guest: VecDeque::new(),
                from_guest: VecDeque::new(),
                guest_fin: false,
                fin_sent: false,
                idle: 0,
            });
            return; // the SYN-ACK is emitted by poll() once the egress is Open
        }

        let Some(idx) = idx else {
            return; // a segment for an unknown connection — ignore
        };

        if flags & F_RST != 0 {
            self.conns.remove(idx);
            return;
        }

        self.conns[idx].guest_wnd = window;

        // Process the acknowledgement of our bytes.
        if flags & F_ACK != 0 {
            let c = &mut self.conns[idx];
            if seq_gt(ack, c.snd_una) {
                c.snd_una = ack;
            }
            // The egress established-transition: the guest acknowledged our
            // SYN-ACK. Ingress connections become established in the SYN-ACK
            // handler below (where `rcv_nxt` is also learned), so exclude them
            // here — their SYN-ACK *also* carries an ACK and must not preempt it.
            if c.ingress_id.is_none()
                && c.synack_sent
                && !c.established
                && seq_geq(c.snd_una, c.iss.wrapping_add(1))
            {
                c.established = true;
            }
        }

        let mut reply: Option<Vec<u8>> = None;

        // For a *forwarded inbound* connection (CC-21) the NAT is the active
        // opener: it sent the SYN, and this is the guest server's SYN-ACK.
        // Record the guest's ISN, mark the connection open, and ACK it.
        if self.conns[idx].ingress_id.is_some()
            && !self.conns[idx].established
            && flags & F_SYN != 0
        {
            let c = &mut self.conns[idx];
            c.rcv_nxt = seq.wrapping_add(1); // past the guest's SYN
            c.established = true;
            reply = Some(tcp_to_guest(c, F_ACK, &[], &[], self.guest_mac));
        }

        // Accept in-order payload and buffer it for the host transport (drained
        // in poll/poll_ingress — handle_tcp stays transport-agnostic).
        if !payload.is_empty() && self.conns[idx].established {
            // Backpressure: only accept the segment if the guest→host buffer has
            // room. If it is full we drop the segment (do not advance rcv_nxt) so
            // the guest retransmits once the host side has drained, and the
            // shrunken advertised window (below) tells it to back off — bounding
            // the buffer instead of letting it grow without limit.
            let has_room = self.conns[idx].from_guest.len() < FROM_GUEST_CAP;
            if seq == self.conns[idx].rcv_nxt && has_room {
                let p = payload.to_vec();
                self.conns[idx].from_guest.extend(p);
                self.conns[idx].rcv_nxt =
                    self.conns[idx].rcv_nxt.wrapping_add(payload.len() as u32);
                self.conns[idx].idle = 0;
            }
            // ACK the current receive point (re-ACK on a retransmit), advertising
            // the connection's *current* free window so the guest sees backpressure.
            let c = &mut self.conns[idx];
            reply = Some(tcp_ack_windowed(c, self.guest_mac));
        }

        // The guest's FIN (half-close): acknowledge it, mark it, and start our own
        // close so the host transport is released.
        if flags & F_FIN != 0 && !self.conns[idx].guest_fin {
            let fin_seq = seq.wrapping_add(payload.len() as u32);
            if fin_seq == self.conns[idx].rcv_nxt {
                self.conns[idx].rcv_nxt = self.conns[idx].rcv_nxt.wrapping_add(1);
                self.conns[idx].guest_fin = true;
                let c = &mut self.conns[idx];
                if !c.fin_sent {
                    // ACK the FIN, then send our FIN (consumes one sequence).
                    let ackf = tcp_to_guest(c, F_FIN | F_ACK, &[], &[], self.guest_mac);
                    c.snd_nxt = c.snd_nxt.wrapping_add(1);
                    c.fin_sent = true;
                    reply = Some(ackf);
                } else {
                    reply = Some(tcp_to_guest(c, F_ACK, &[], &[], self.guest_mac));
                }
            }
        }

        if let Some(f) = reply {
            self.rx.push_back(f);
        }
        // The host-transport close (egress.close / ingress.close) and the final
        // reap happen in poll/poll_ingress, where the transport is in scope.
    }
}

/// Build a TCP segment from the remote endpoint toward the guest, wrapped in IPv4
/// and Ethernet (the NAT impersonates the remote peer, Law-L1-style: the guest
/// believes it talks to the server directly).
fn tcp_to_guest(c: &Conn, flags: u8, opts: &[u8], payload: &[u8], guest_mac: [u8; 6]) -> Vec<u8> {
    let tcp = build_tcp(
        c.r_ip,
        c.g_ip,
        c.r_port,
        c.g_port,
        c.snd_nxt,
        c.rcv_nxt,
        flags,
        RECV_WINDOW,
        opts,
        payload,
    );
    let ip = build_ipv4(c.r_ip, c.g_ip, IP_PROTO_TCP, &tcp);
    eth(guest_mac, GW_MAC, ET_IPV4, &ip)
}

/// A bare `ACK` toward the guest that advertises the connection's **current free
/// receive window** — the room left in [`Conn::from_guest`] before [`FROM_GUEST_CAP`]
/// (clamped to the unscaled 16-bit window). As the host side falls behind and the
/// buffer fills, the advertised window shrinks toward zero, so the guest applies
/// real TCP backpressure instead of the NAT buffering without limit.
fn tcp_ack_windowed(c: &Conn, guest_mac: [u8; 6]) -> Vec<u8> {
    let free = FROM_GUEST_CAP.saturating_sub(c.from_guest.len());
    let window = free.min(RECV_WINDOW as usize) as u16;
    let tcp = build_tcp(
        c.r_ip,
        c.g_ip,
        c.r_port,
        c.g_port,
        c.snd_nxt,
        c.rcv_nxt,
        F_ACK,
        window,
        &[],
        &[],
    );
    let ip = build_ipv4(c.r_ip, c.g_ip, IP_PROTO_TCP, &tcp);
    eth(guest_mac, GW_MAC, ET_IPV4, &ip)
}

// ── framing helpers ────────────────────────────────────────────────────────

fn eth(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len());
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// The internet checksum (RFC 1071): the 16-bit one's-complement of the
/// one's-complement sum of the data (a trailing odd byte is padded with zero).
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn build_ipv4(src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut h = Vec::with_capacity(total);
    h.push(0x45); // version 4, IHL 5
    h.push(0); // DSCP/ECN
    h.extend_from_slice(&(total as u16).to_be_bytes());
    h.extend_from_slice(&0u16.to_be_bytes()); // identification
    h.extend_from_slice(&0x4000u16.to_be_bytes()); // flags = DF, fragment offset 0
    h.push(64); // TTL
    h.push(proto);
    h.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    h.extend_from_slice(&src);
    h.extend_from_slice(&dst);
    let csum = checksum(&h);
    h[10..12].copy_from_slice(&csum.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

fn build_udp(sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut u = Vec::with_capacity(8 + payload.len());
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    u.extend_from_slice(&0u16.to_be_bytes()); // checksum 0 = disabled (valid for IPv4)
    u.extend_from_slice(payload);
    u
}

#[allow(clippy::too_many_arguments)]
fn build_tcp(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    opts: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    debug_assert!(
        opts.len().is_multiple_of(4),
        "TCP options must be 4-byte aligned"
    );
    let data_off_words = (20 + opts.len()) / 4;
    let mut t = Vec::with_capacity(20 + opts.len() + payload.len());
    t.extend_from_slice(&sport.to_be_bytes());
    t.extend_from_slice(&dport.to_be_bytes());
    t.extend_from_slice(&seq.to_be_bytes());
    t.extend_from_slice(&ack.to_be_bytes());
    t.push((data_off_words as u8) << 4);
    t.push(flags);
    t.extend_from_slice(&window.to_be_bytes());
    t.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    t.extend_from_slice(&0u16.to_be_bytes()); // urgent pointer
    t.extend_from_slice(opts);
    t.extend_from_slice(payload);
    // Checksum over the TCP pseudo-header + the segment.
    let mut pseudo = Vec::with_capacity(12 + t.len());
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(IP_PROTO_TCP);
    pseudo.extend_from_slice(&(t.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(&t);
    let csum = checksum(&pseudo);
    t[16..18].copy_from_slice(&csum.to_be_bytes());
    t
}

/// Serial-number "greater than" (RFC 1982 / RFC 9293 §3.4): compares modulo 2³².
fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
fn seq_geq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

// ── the native egress: a direct host socket (std only) ─────────────────────

/// A port-forward entry: a guest-visible `(ip, port)` mapped to a host
/// `(address, port)` — the slirp / `guestfwd` redirection [`StdEgress`] applies.
#[cfg(feature = "std")]
type Redirect = (([u8; 4], u16), (std::string::String, u16));

/// The native **egress transport**: a direct host TCP socket per connection (the
/// peer is a process with `std::net`). The browser peer substitutes a WebSocket
/// tunnel to a relay (ADR-014); the NAT above is identical for both.
///
/// A `redirect` table maps a guest-visible `ip:port` to a host `address:port` —
/// the port-forwarding a slirp NAT performs (and the analogue of QEMU's
/// `guestfwd`), so a witness can point the guest at a controlled local server
/// without the guest software changing.
#[cfg(feature = "std")]
pub struct StdEgress {
    conns: std::collections::BTreeMap<u32, StdConn>,
    redirect: Vec<Redirect>,
    next_id: u32,
}

/// One host-side socket the native egress is bridging: its stream (absent once a
/// connect fails), an outbound buffer drained on `WouldBlock`, and a closed flag.
#[cfg(feature = "std")]
struct StdConn {
    stream: Option<std::net::TcpStream>,
    out: Vec<u8>,
    closed: bool,
}

#[cfg(feature = "std")]
impl StdEgress {
    /// A native egress with no redirects (the guest reaches the real internet
    /// addresses it dials).
    #[must_use]
    pub fn new() -> Self {
        StdEgress {
            conns: std::collections::BTreeMap::new(),
            redirect: Vec::new(),
            next_id: 1,
        }
    }

    /// Add a port-forward: a guest connection to `guest_ip:guest_port` is carried
    /// to the host `host:host_port` instead (the slirp / `guestfwd` mapping a
    /// witness uses to reach a controlled local server).
    pub fn redirect(
        mut self,
        guest_ip: [u8; 4],
        guest_port: u16,
        host: &str,
        host_port: u16,
    ) -> Self {
        self.redirect
            .push(((guest_ip, guest_port), (host.into(), host_port)));
        self
    }

    fn flush(c: &mut StdConn) {
        use std::io::Write;
        let Some(s) = c.stream.as_mut() else {
            return;
        };
        while !c.out.is_empty() {
            match s.write(&c.out) {
                Ok(0) => {
                    c.closed = true;
                    break;
                }
                Ok(n) => {
                    c.out.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    c.closed = true;
                    break;
                }
            }
        }
    }
}

#[cfg(feature = "std")]
impl Default for StdEgress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl Egress for StdEgress {
    fn connect(&mut self, ip: [u8; 4], port: u16) -> u32 {
        use std::net::{TcpStream, ToSocketAddrs};
        use std::time::Duration;
        let id = self.next_id;
        self.next_id += 1;
        // Resolve the target, honouring any redirect (port-forward).
        let target = self
            .redirect
            .iter()
            .find(|((gip, gp), _)| *gip == ip && *gp == port)
            .map(|(_, (host, hp))| format!("{host}:{hp}"));
        let addr_iter = match &target {
            Some(hostport) => hostport.to_socket_addrs(),
            None => format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port).to_socket_addrs(),
        };
        let stream = addr_iter
            .ok()
            .and_then(|mut it| it.next())
            .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok());
        let conn = match stream {
            Some(s) => {
                let _ = s.set_nonblocking(true);
                StdConn {
                    stream: Some(s),
                    out: Vec::new(),
                    closed: false,
                }
            }
            None => StdConn {
                stream: None,
                out: Vec::new(),
                closed: true,
            },
        };
        self.conns.insert(id, conn);
        id
    }

    fn status(&mut self, id: u32) -> EgressStatus {
        match self.conns.get(&id) {
            Some(c) if c.closed => EgressStatus::Closed,
            Some(_) => EgressStatus::Open,
            None => EgressStatus::Closed,
        }
    }

    fn recv(&mut self, id: u32) -> Vec<u8> {
        use std::io::Read;
        let Some(c) = self.conns.get_mut(&id) else {
            return Vec::new();
        };
        Self::flush(c);
        if c.closed {
            return Vec::new();
        }
        let Some(s) = c.stream.as_mut() else {
            return Vec::new();
        };
        let mut buf = [0u8; 8192];
        match s.read(&mut buf) {
            Ok(0) => {
                c.closed = true;
                Vec::new()
            }
            Ok(n) => buf[..n].to_vec(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Vec::new(),
            Err(_) => {
                c.closed = true;
                Vec::new()
            }
        }
    }

    fn send(&mut self, id: u32, data: &[u8]) {
        if let Some(c) = self.conns.get_mut(&id) {
            if !c.closed {
                c.out.extend_from_slice(data);
                Self::flush(c);
            }
        }
    }

    fn close(&mut self, id: u32) {
        self.conns.remove(&id);
    }
}

/// The native **ingress transport** (`CC-21`): a host `TcpListener` per forwarded
/// port. An outside connection to a forwarded host port is accepted and bridged
/// to the guest's listening `guest_port` — the running-app preview, reachable
/// from outside the devcontainer. The browser peer substitutes a relay-served
/// route; the NAT above is identical.
#[cfg(feature = "std")]
pub struct StdIngress {
    /// `(listener, guest_port)` — each host listener forwards to a guest port.
    listeners: Vec<(std::net::TcpListener, u16)>,
    conns: std::collections::BTreeMap<u32, StdConn>,
    next_id: u32,
}

#[cfg(feature = "std")]
impl StdIngress {
    /// A new ingress with no forwarded ports.
    #[must_use]
    pub fn new() -> Self {
        StdIngress {
            listeners: Vec::new(),
            conns: std::collections::BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Forward a host port to the guest's `guest_port`: bind a listener on
    /// `127.0.0.1` and return the host port chosen (pass `0` for an ephemeral
    /// port — the witness reads it back). An outside connection to that host
    /// port reaches the server listening on `guest_port` inside the devcontainer.
    pub fn forward(&mut self, host_port: u16, guest_port: u16) -> std::io::Result<u16> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", host_port))?;
        listener.set_nonblocking(true)?;
        let chosen = listener.local_addr()?.port();
        self.listeners.push((listener, guest_port));
        Ok(chosen)
    }
}

#[cfg(feature = "std")]
impl Default for StdIngress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl Ingress for StdIngress {
    fn poll_accept(&mut self) -> Option<(u32, u16)> {
        for (listener, guest_port) in &self.listeners {
            if let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                let id = self.next_id;
                self.next_id += 1;
                self.conns.insert(
                    id,
                    StdConn {
                        stream: Some(stream),
                        out: Vec::new(),
                        closed: false,
                    },
                );
                return Some((id, *guest_port));
            }
        }
        None
    }

    fn status(&mut self, id: u32) -> EgressStatus {
        match self.conns.get(&id) {
            Some(c) if c.closed => EgressStatus::Closed,
            Some(_) => EgressStatus::Open,
            None => EgressStatus::Closed,
        }
    }

    fn recv(&mut self, id: u32) -> Vec<u8> {
        use std::io::Read;
        let Some(c) = self.conns.get_mut(&id) else {
            return Vec::new();
        };
        StdEgress::flush(c);
        if c.closed {
            return Vec::new();
        }
        let Some(s) = c.stream.as_mut() else {
            return Vec::new();
        };
        let mut buf = [0u8; 8192];
        match s.read(&mut buf) {
            Ok(0) => {
                c.closed = true;
                Vec::new()
            }
            Ok(n) => buf[..n].to_vec(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Vec::new(),
            Err(_) => {
                c.closed = true;
                Vec::new()
            }
        }
    }

    fn send(&mut self, id: u32, data: &[u8]) {
        if let Some(c) = self.conns.get_mut(&id) {
            if !c.closed {
                c.out.extend_from_slice(data);
                StdEgress::flush(c);
            }
        }
    }

    fn close(&mut self, id: u32) {
        self.conns.remove(&id);
    }

    fn add_forward(&mut self, guest_port: u16) -> Option<u16> {
        // Live forward (ADR-018): bind a new host listener for `guest_port` —
        // the same host port for Codespaces parity, else an ephemeral one.
        self.forward(guest_port, guest_port)
            .or_else(|_| self.forward(0, guest_port))
            .ok()
    }
}

/// A no-op **egress** — the guest opens no outbound connections (e.g. a
/// devcontainer that only *listens*, reached over the [`LoopbackIngress`]). The
/// NAT still needs an egress; this one refuses every connect, so a guest that
/// tries to dial out simply sees the connection fail (the DHCP lease and the
/// guest's own TCP stack are the NAT's, not the egress's, so the guest still gets
/// its IP and can accept inbound connections).
pub struct NoEgress;

impl Egress for NoEgress {
    fn connect(&mut self, _ip: [u8; 4], _port: u16) -> u32 {
        0
    }
    fn status(&mut self, _id: u32) -> EgressStatus {
        EgressStatus::Closed
    }
    fn recv(&mut self, _id: u32) -> Vec<u8> {
        Vec::new()
    }
    fn send(&mut self, _id: u32, _data: &[u8]) {}
    fn close(&mut self, _id: u32) {}
}

// ── ChannelEgress — the guest's egress routed through an external router ──────
// The egress frame protocol: the SAME OPEN/DATA/CLOSE wire the `WsEgress` relay
// (`CC-16`), the holospaces-node (`CC-39`), and the router extension (`CC-41`)
// speak. ChannelEgress is the NAT-side endpoint; the page carries these frames to
// whichever router is configured (the extension's `chrome.runtime` port, or a
// node's WebSocket), which opens the real sockets a tab cannot — so the guest's
// package managers, network config, and apps reach the internet (Codespaces
// parity), the router being the gateway for arbitrary traffic.
const EGRESS_OP_OPEN: u8 = 0x01;
const EGRESS_OP_DATA: u8 = 0x02;
const EGRESS_OP_CLOSE: u8 = 0x03;
const EGRESS_OP_OPENED: u8 = 0x11;
const EGRESS_OP_RDATA: u8 = 0x12;
const EGRESS_OP_CLOSED: u8 = 0x13;
const EGRESS_OP_FAILED: u8 = 0x14;

struct ChannelConn {
    status: EgressStatus,
    inbound: VecDeque<u8>,
}

struct ChannelShared {
    conns: BTreeMap<u32, ChannelConn>,
    /// Egress frames the guest produced, awaiting the page's pump to the router.
    outbound: VecDeque<Vec<u8>>,
}

/// The page-side handle to a [`ChannelEgress`] — the seam a transport carries.
/// The page [`drain_outbound`](RouterChannel::drain_outbound)s the guest's egress
/// frames to the router (the extension's `chrome.runtime` port, or a node's
/// WebSocket) and [`feed_inbound`](RouterChannel::feed_inbound)s the router's
/// replies back. Cloneable; shares the egress state with its `ChannelEgress`.
#[derive(Clone)]
pub struct RouterChannel {
    shared: Rc<RefCell<ChannelShared>>,
}

impl RouterChannel {
    /// Take the guest's pending egress frames (`OPEN`/`DATA`/`CLOSE`) for the
    /// router to act on. Empty when the guest has sent nothing since the last drain.
    #[must_use]
    pub fn drain_outbound(&self) -> Vec<Vec<u8>> {
        self.shared.borrow_mut().outbound.drain(..).collect()
    }

    /// Take the next single egress frame for the router, or `None` if none is
    /// queued — the one-at-a-time form a wasm-bindgen seam drains in a loop.
    #[must_use]
    pub fn pop_outbound(&self) -> Option<Vec<u8>> {
        self.shared.borrow_mut().outbound.pop_front()
    }

    /// Deliver a frame the router returned (`OPENED`/`DATA`/`CLOSED`/`FAILED`)
    /// into the connection state the NAT polls.
    pub fn feed_inbound(&self, frame: &[u8]) {
        if frame.len() < 5 {
            return;
        }
        let op = frame[0];
        let id = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        let mut s = self.shared.borrow_mut();
        match op {
            EGRESS_OP_OPENED => {
                if let Some(c) = s.conns.get_mut(&id) {
                    c.status = EgressStatus::Open;
                }
            }
            EGRESS_OP_RDATA => {
                if let Some(c) = s.conns.get_mut(&id) {
                    c.inbound.extend(&frame[5..]);
                }
            }
            EGRESS_OP_CLOSED | EGRESS_OP_FAILED => {
                if let Some(c) = s.conns.get_mut(&id) {
                    c.status = EgressStatus::Closed;
                }
            }
            _ => {}
        }
    }
}

/// An [`Egress`] whose connections are carried by an external **router** over the
/// egress frame protocol — the browser peer's path to arbitrary internet through
/// the router extension (`CC-41`) or a node (`CC-39`). The NAT drives it exactly
/// as any egress (`connect`/`send`/`recv`/`close`); the frames are drained and
/// fed via the paired [`RouterChannel`]. No transport, no JS — the router seam is
/// surface-agnostic substrate code.
pub struct ChannelEgress {
    shared: Rc<RefCell<ChannelShared>>,
    next_id: u32,
}

impl ChannelEgress {
    /// A router-backed egress and the page-side [`RouterChannel`] that carries its
    /// frames to the configured router.
    #[must_use]
    pub fn new() -> (ChannelEgress, RouterChannel) {
        let shared = Rc::new(RefCell::new(ChannelShared {
            conns: BTreeMap::new(),
            outbound: VecDeque::new(),
        }));
        (
            ChannelEgress {
                shared: shared.clone(),
                next_id: 1,
            },
            RouterChannel { shared },
        )
    }
}

impl Egress for ChannelEgress {
    fn connect(&mut self, ip: [u8; 4], port: u16) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let mut s = self.shared.borrow_mut();
        s.conns.insert(
            id,
            ChannelConn {
                status: EgressStatus::Connecting,
                inbound: VecDeque::new(),
            },
        );
        let mut frame = Vec::with_capacity(11);
        frame.push(EGRESS_OP_OPEN);
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&port.to_be_bytes());
        s.outbound.push_back(frame);
        id
    }

    fn status(&mut self, id: u32) -> EgressStatus {
        self.shared
            .borrow()
            .conns
            .get(&id)
            .map_or(EgressStatus::Closed, |c| c.status)
    }

    fn recv(&mut self, id: u32) -> Vec<u8> {
        let mut s = self.shared.borrow_mut();
        match s.conns.get_mut(&id) {
            Some(c) => c.inbound.drain(..).collect(),
            None => Vec::new(),
        }
    }

    fn send(&mut self, id: u32, data: &[u8]) {
        let mut s = self.shared.borrow_mut();
        if matches!(
            s.conns.get(&id).map(|c| c.status),
            Some(EgressStatus::Closed)
        ) {
            return;
        }
        let mut frame = Vec::with_capacity(5 + data.len());
        frame.push(EGRESS_OP_DATA);
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(data);
        s.outbound.push_back(frame);
    }

    fn close(&mut self, id: u32) {
        let mut s = self.shared.borrow_mut();
        if let Some(c) = s.conns.get_mut(&id) {
            c.status = EgressStatus::Closed;
        }
        let mut frame = Vec::with_capacity(5);
        frame.push(EGRESS_OP_CLOSE);
        frame.extend_from_slice(&id.to_be_bytes());
        s.outbound.push_back(frame);
    }
}

/// One in-process loopback connection's buffers, shared between the host side
/// ([`LoopbackHandle`]) and the NAT side ([`LoopbackIngress`]).
struct LoopConn {
    /// Host → guest: bytes the workbench wrote, which the NAT drains in `recv`
    /// and delivers into the guest's listening socket.
    to_guest: VecDeque<u8>,
    /// Guest → host: the guest server's reply, which the NAT appends in `send`
    /// and the host drains via [`LoopbackHandle::recv`].
    from_guest: VecDeque<u8>,
    /// The host side closed (no more bytes will be written; the NAT should FIN
    /// the guest).
    host_closed: bool,
    /// The guest side closed (the NAT delivered the guest's FIN).
    guest_closed: bool,
}

#[derive(Default)]
struct LoopbackInner {
    next_id: u32,
    /// Dialed connections awaiting `poll_accept` (the NAT opens toward the guest).
    pending: VecDeque<(u32, u16)>,
    conns: BTreeMap<u32, LoopConn>,
}

/// The **in-process loopback ingress** (ADR-020, `CC-33`). The "outside client"
/// is the workbench in the same process as the emulator (the browser peer's
/// extension-host worker, or a host witness), so reaching a server *inside* the
/// guest is not a network round trip — it is an in-process ingress connection
/// into the emulator's own TCP stack. The host drives it through a
/// [`LoopbackHandle`] (`dial`/`send`/`recv`/`close`); the NAT services it through
/// the [`Ingress`] trait exactly as it services a native host-listener
/// ([`StdIngress`]). The two share one set of per-connection buffers; no relay,
/// no socket, no server (Law L4). This is the transport the VS Code remote
/// extension host runs over (ADR-015/ADR-020): the workbench's remote-protocol
/// connection *is* this bridge.
pub struct LoopbackIngress {
    inner: Rc<RefCell<LoopbackInner>>,
}

/// The host-side handle to a [`LoopbackIngress`]'s shared state — the
/// workbench/witness side of the bridge. Cheaply cloneable (it shares the same
/// connection table); the emulator keeps one and exposes `dial`/`send`/`recv`/
/// `close` through it.
#[derive(Clone)]
pub struct LoopbackHandle {
    inner: Rc<RefCell<LoopbackInner>>,
}

impl LoopbackIngress {
    /// Create a loopback ingress and its paired host handle (they share the
    /// connection buffers). Attach the ingress to the emulator's network device;
    /// keep the handle to dial guest listeners from the host side.
    #[must_use]
    pub fn new() -> (LoopbackIngress, LoopbackHandle) {
        let inner = Rc::new(RefCell::new(LoopbackInner {
            next_id: 1,
            ..LoopbackInner::default()
        }));
        (
            LoopbackIngress {
                inner: Rc::clone(&inner),
            },
            LoopbackHandle { inner },
        )
    }
}

impl LoopbackHandle {
    /// Dial a connection to the guest's listening `guest_port`. Returns the
    /// connection id; the NAT opens toward the guest's socket on the next pump.
    pub fn dial(&self, guest_port: u16) -> u32 {
        let mut s = self.inner.borrow_mut();
        let id = s.next_id;
        s.next_id += 1;
        s.conns.insert(
            id,
            LoopConn {
                to_guest: VecDeque::new(),
                from_guest: VecDeque::new(),
                host_closed: false,
                guest_closed: false,
            },
        );
        s.pending.push_back((id, guest_port));
        id
    }

    /// Write host bytes toward the guest server on connection `id`.
    pub fn send(&self, id: u32, data: &[u8]) {
        if let Some(c) = self.inner.borrow_mut().conns.get_mut(&id) {
            c.to_guest.extend(data.iter().copied());
        }
    }

    /// Drain the guest server's reply bytes on connection `id` (empty if none).
    pub fn recv(&self, id: u32) -> Vec<u8> {
        match self.inner.borrow_mut().conns.get_mut(&id) {
            Some(c) => c.from_guest.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// Close the host side of connection `id` (the NAT will FIN the guest).
    pub fn close(&self, id: u32) {
        if let Some(c) = self.inner.borrow_mut().conns.get_mut(&id) {
            c.host_closed = true;
        }
    }

    /// Whether connection `id` is still usable — either the guest has not closed
    /// it, or it has but the host has not yet drained the guest's final bytes.
    #[must_use]
    pub fn is_open(&self, id: u32) -> bool {
        match self.inner.borrow().conns.get(&id) {
            Some(c) => !c.guest_closed || !c.from_guest.is_empty(),
            None => false,
        }
    }
}

impl Ingress for LoopbackIngress {
    fn poll_accept(&mut self) -> Option<(u32, u16)> {
        self.inner.borrow_mut().pending.pop_front()
    }
    fn status(&mut self, id: u32) -> EgressStatus {
        match self.inner.borrow().conns.get(&id) {
            Some(c) if c.host_closed => EgressStatus::Closed,
            Some(_) => EgressStatus::Open,
            None => EgressStatus::Closed,
        }
    }
    fn recv(&mut self, id: u32) -> Vec<u8> {
        // Host → guest: deliver the bytes the host wrote into the guest socket.
        match self.inner.borrow_mut().conns.get_mut(&id) {
            Some(c) => c.to_guest.drain(..).collect(),
            None => Vec::new(),
        }
    }
    fn send(&mut self, id: u32, data: &[u8]) {
        // Guest → host: the guest server's reply, drained by the host via `recv`.
        if let Some(c) = self.inner.borrow_mut().conns.get_mut(&id) {
            c.from_guest.extend(data.iter().copied());
        }
    }
    fn close(&mut self, id: u32) {
        if let Some(c) = self.inner.borrow_mut().conns.get_mut(&id) {
            c.guest_closed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `RouterChannel` frame protocol drives the egress OPEN → DATA → CLOSE
    /// state machine: the test echoes the guest's own outbound payload back as an
    /// RDATA frame and asserts `recv` returns those bytes — exercising the frame
    /// exchange the router extension (`CC-41`) and the node (`CC-39`) implement.
    /// No socket or internet is involved.
    #[test]
    fn channel_egress_routes_a_connection_through_the_router() {
        let (mut egress, channel) = ChannelEgress::new();

        // The guest dials out → an OPEN frame is queued for the router.
        let id = egress.connect([93, 184, 216, 34], 80);
        let out = channel.drain_outbound();
        assert_eq!(out.len(), 1, "one OPEN frame queued for the router");
        assert_eq!(out[0][0], EGRESS_OP_OPEN);
        assert_eq!(
            u32::from_be_bytes([out[0][1], out[0][2], out[0][3], out[0][4]]),
            id
        );
        assert!(matches!(egress.status(id), EgressStatus::Connecting));

        // The router opens the socket and reports OPENED → the guest sees Open.
        let mut opened = vec![EGRESS_OP_OPENED];
        opened.extend_from_slice(&id.to_be_bytes());
        channel.feed_inbound(&opened);
        assert!(matches!(egress.status(id), EgressStatus::Open));

        // The guest sends; the router echoes; the guest receives the reply.
        egress.send(id, b"GET / HTTP/1.0\r\n\r\n");
        let out = channel.drain_outbound();
        assert_eq!(out[0][0], EGRESS_OP_DATA);
        let mut echo = vec![EGRESS_OP_RDATA];
        echo.extend_from_slice(&id.to_be_bytes());
        echo.extend_from_slice(&out[0][5..]);
        channel.feed_inbound(&echo);
        assert_eq!(egress.recv(id), b"GET / HTTP/1.0\r\n\r\n");

        // The guest closes → a CLOSE frame, and the connection is closed.
        egress.close(id);
        assert_eq!(channel.drain_outbound()[0][0], EGRESS_OP_CLOSE);
        assert!(matches!(egress.status(id), EgressStatus::Closed));
    }

    /// A do-nothing egress that records connects and replies with a canned
    /// payload once — enough to unit-test the NAT's framing and state machine
    /// without a real socket.
    struct MockEgress {
        connected_to: Option<([u8; 4], u16)>,
        reply: Vec<u8>,
        sent: Vec<u8>,
        delivered: bool,
        opened: bool,
    }
    impl MockEgress {
        fn new(reply: &[u8]) -> Self {
            MockEgress {
                connected_to: None,
                reply: reply.to_vec(),
                sent: Vec::new(),
                delivered: false,
                opened: false,
            }
        }
    }
    impl Egress for MockEgress {
        fn connect(&mut self, ip: [u8; 4], port: u16) -> u32 {
            self.connected_to = Some((ip, port));
            self.opened = true;
            1
        }
        fn status(&mut self, _id: u32) -> EgressStatus {
            if self.opened {
                EgressStatus::Open
            } else {
                EgressStatus::Closed
            }
        }
        fn recv(&mut self, _id: u32) -> Vec<u8> {
            if !self.delivered && !self.sent.is_empty() {
                self.delivered = true;
                core::mem::take(&mut self.reply)
            } else {
                Vec::new()
            }
        }
        fn send(&mut self, _id: u32, data: &[u8]) {
            self.sent.extend_from_slice(data);
        }
        fn close(&mut self, _id: u32) {
            self.opened = false;
        }
    }

    fn arp_request(tpa: [u8; 4]) -> Vec<u8> {
        let mut arp = Vec::new();
        arp.extend_from_slice(&[0, 1]);
        arp.extend_from_slice(&ET_IPV4.to_be_bytes());
        arp.push(6);
        arp.push(4);
        arp.extend_from_slice(&[0, 1]); // request
        arp.extend_from_slice(&GUEST_MAC);
        arp.extend_from_slice(&GUEST_IP);
        arp.extend_from_slice(&[0; 6]);
        arp.extend_from_slice(&tpa);
        eth(GW_MAC, GUEST_MAC, ET_ARP, &arp)
    }

    #[test]
    fn arp_for_any_address_is_answered_with_the_gateway_mac() {
        let mut nat = Nat::new();
        let mut eg = MockEgress::new(b"");
        nat.on_guest_frame(&arp_request([10, 0, 2, 9]), &mut eg);
        let reply = nat.take_rx().expect("an ARP reply");
        // Ethernet: to the guest, from the gateway MAC, ARP.
        assert_eq!(&reply[0..6], &GUEST_MAC);
        assert_eq!(&reply[6..12], &GW_MAC);
        assert_eq!(u16::from_be_bytes([reply[12], reply[13]]), ET_ARP);
        // ARP body: reply (oper 2), sender hw = gateway MAC, sender proto = 10.0.2.9.
        let a = &reply[14..];
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 2);
        assert_eq!(&a[8..14], &GW_MAC);
        assert_eq!(&a[14..18], &[10, 0, 2, 9]);
    }

    #[test]
    fn dhcp_discover_is_offered_the_slirp_lease() {
        let mut nat = Nat::new();
        let mut eg = MockEgress::new(b"");
        // A minimal DISCOVER.
        let mut dhcp = vec![1u8, 1, 6, 0]; // op, htype, hlen, hops
        dhcp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // xid
        dhcp.extend_from_slice(&[0; 2]); // secs
        dhcp.extend_from_slice(&[0; 2]); // flags
        dhcp.extend_from_slice(&[0; 16]); // ci/yi/si/gi addr
        dhcp.extend_from_slice(&GUEST_MAC);
        dhcp.extend_from_slice(&[0; 10]);
        dhcp.extend_from_slice(&[0; 64]);
        dhcp.extend_from_slice(&[0; 128]);
        dhcp.extend_from_slice(&0x6382_5363u32.to_be_bytes());
        dhcp.extend_from_slice(&[53, 1, 1]); // DISCOVER
        dhcp.push(255);
        let udp = build_udp(68, 67, &dhcp);
        let ip = build_ipv4([0, 0, 0, 0], [255, 255, 255, 255], IP_PROTO_UDP, &udp);
        let frame = eth([0xff; 6], GUEST_MAC, ET_IPV4, &ip);

        nat.on_guest_frame(&frame, &mut eg);
        let reply = nat.take_rx().expect("a DHCP offer");
        // Dig out the BOOTP yiaddr (offered address) — Eth(14) + IP(20) + UDP(8) + 16.
        let yiaddr = &reply[14 + 20 + 8 + 16..14 + 20 + 8 + 20];
        assert_eq!(yiaddr, &GUEST_IP, "the guest is offered 10.0.2.15");
    }

    #[test]
    fn a_syn_opens_an_egress_connection_and_is_answered_with_syn_ack() {
        let mut nat = Nat::new();
        let mut eg = MockEgress::new(b"HTTP/1.0 200 OK\r\n\r\nhi");
        // SYN from the guest to 10.0.2.9:8080.
        let syn = guest_tcp(40000, [10, 0, 2, 9], 8080, 1000, 0, F_SYN, b"");
        nat.on_guest_frame(&syn, &mut eg);
        assert_eq!(eg.connected_to, Some(([10, 0, 2, 9], 8080)));
        // poll() emits the SYN-ACK now that the (mock) egress is Open.
        nat.poll(&mut eg);
        let synack = nat.take_rx().expect("a SYN-ACK");
        let t = &synack[14 + 20..];
        assert_eq!(t[13] & (F_SYN | F_ACK), F_SYN | F_ACK, "SYN|ACK set");
        let ack = u32::from_be_bytes([t[8], t[9], t[10], t[11]]);
        assert_eq!(ack, 1001, "acks the guest's ISN+1");
    }

    /// Build a guest→remote TCP segment wrapped in IPv4 + Ethernet.
    fn guest_tcp(
        sport: u16,
        dst: [u8; 4],
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let tcp = build_tcp(
            GUEST_IP,
            dst,
            sport,
            dport,
            seq,
            ack,
            flags,
            64240,
            &[],
            payload,
        );
        let ip = build_ipv4(GUEST_IP, dst, IP_PROTO_TCP, &tcp);
        eth(GW_MAC, GUEST_MAC, ET_IPV4, &ip)
    }

    #[test]
    fn a_full_request_response_flows_through_the_nat() {
        let mut nat = Nat::new();
        let body = b"HTTP/1.0 200 OK\r\n\r\nHELLO";
        let mut eg = MockEgress::new(body);
        // Handshake.
        nat.on_guest_frame(
            &guest_tcp(40000, [10, 0, 2, 9], 8080, 1000, 0, F_SYN, b""),
            &mut eg,
        );
        nat.poll(&mut eg);
        let synack = nat.take_rx().unwrap();
        let our_isn = u32::from_be_bytes([
            synack[14 + 20 + 4],
            synack[14 + 20 + 5],
            synack[14 + 20 + 6],
            synack[14 + 20 + 7],
        ]);
        // The guest ACKs the SYN-ACK and sends the request.
        nat.on_guest_frame(
            &guest_tcp(
                40000,
                [10, 0, 2, 9],
                8080,
                1001,
                our_isn.wrapping_add(1),
                F_ACK,
                b"",
            ),
            &mut eg,
        );
        nat.on_guest_frame(
            &guest_tcp(
                40000,
                [10, 0, 2, 9],
                8080,
                1001,
                our_isn.wrapping_add(1),
                F_PSH | F_ACK,
                b"GET / HTTP/1.0\r\n\r\n",
            ),
            &mut eg,
        );
        // Pump: the buffered request is flushed to the egress, and the response
        // comes back as a data segment.
        nat.poll(&mut eg);
        assert!(
            eg.sent.starts_with(b"GET /"),
            "the request reached the egress"
        );
        nat.poll(&mut eg);
        let mut got = Vec::new();
        while let Some(frame) = nat.take_rx() {
            let t = &frame[14 + 20..];
            let off = ((t[12] >> 4) as usize) * 4;
            got.extend_from_slice(&t[off..]);
        }
        assert!(
            got.windows(body.len()).any(|w| w == body) || got == body,
            "the HTTP response reached the guest; got {got:?}"
        );
    }

    /// The in-process loopback bridge's transport semantics (ADR-020, `CC-33`):
    /// the host side dials and the NAT side accepts the same connection, and the
    /// two byte directions are wired correctly — what the host writes is what the
    /// NAT delivers to the guest (`recv`), and what the guest emits (`send`) is
    /// what the host reads. No NAT/OS needed — this pins the bridge's plumbing.
    #[test]
    fn the_loopback_bridge_carries_both_byte_directions_to_the_right_side() {
        let (mut ingress, host) = LoopbackIngress::new();

        // The host dials a guest port; the NAT side accepts exactly that.
        let id = host.dial(8080);
        assert_eq!(
            ingress.poll_accept(),
            Some((id, 8080)),
            "the dialed connection is the one the NAT opens toward the guest"
        );
        assert!(ingress.poll_accept().is_none(), "no spurious second accept");
        assert_eq!(ingress.status(id), EgressStatus::Open);

        // Host → guest: what the host writes is what the NAT delivers (its `recv`).
        host.send(id, b"GET / HTTP/1.0\r\n\r\n");
        assert_eq!(ingress.recv(id), b"GET / HTTP/1.0\r\n\r\n".to_vec());
        assert!(ingress.recv(id).is_empty(), "bytes are consumed once");

        // Guest → host: what the guest emits (the NAT's `send`) is what the host reads.
        ingress.send(id, b"HTTP/1.0 200 OK\r\n\r\nHELLO");
        assert_eq!(host.recv(id), b"HTTP/1.0 200 OK\r\n\r\nHELLO".to_vec());
        assert!(host.recv(id).is_empty(), "bytes are consumed once");

        // The guest closing leaves the connection readable until drained, then done.
        ingress.send(id, b"tail");
        ingress.close(id);
        assert!(
            host.is_open(id),
            "unread guest bytes keep the connection open"
        );
        assert_eq!(host.recv(id), b"tail".to_vec());
        assert!(
            !host.is_open(id),
            "after the guest closes and bytes are drained, it is done"
        );

        // The host closing is visible to the NAT as a closed ingress connection.
        let id2 = host.dial(9000);
        let _ = ingress.poll_accept();
        host.close(id2);
        assert_eq!(ingress.status(id2), EgressStatus::Closed);
    }

    /// An egress that just counts `connect` calls — to prove the bounded
    /// connection table refuses new connections past the cap.
    struct CountingEgress {
        connects: usize,
    }
    impl Egress for CountingEgress {
        fn connect(&mut self, _ip: [u8; 4], _port: u16) -> u32 {
            self.connects += 1;
            self.connects as u32
        }
        fn status(&mut self, _id: u32) -> EgressStatus {
            EgressStatus::Connecting
        }
        fn recv(&mut self, _id: u32) -> Vec<u8> {
            Vec::new()
        }
        fn send(&mut self, _id: u32, _data: &[u8]) {}
        fn close(&mut self, _id: u32) {}
    }

    /// The connection table is bounded: a guest that opens MAX_CONNS connections
    /// and then one more does not get a new egress connection for the overflow
    /// SYN — the table cannot balloon (remote/guest DoS). The dropped SYN is
    /// standard TCP backpressure (the guest retransmits).
    #[test]
    fn the_connection_table_is_bounded() {
        let mut nat = Nat::new();
        let mut eg = CountingEgress { connects: 0 };
        // Open exactly MAX_CONNS connections, each a distinct source port.
        for i in 0..MAX_CONNS {
            let sport = 40000u16.wrapping_add(i as u16);
            nat.on_guest_frame(
                &guest_tcp(sport, [10, 0, 2, 9], 8080, 1000, 0, F_SYN, b""),
                &mut eg,
            );
        }
        assert_eq!(eg.connects, MAX_CONNS, "the table filled to its cap");
        assert_eq!(nat.conns.len(), MAX_CONNS);
        // One more SYN (a new 4-tuple) is refused — no new egress connection.
        nat.on_guest_frame(
            &guest_tcp(60000, [10, 0, 2, 9], 8080, 1000, 0, F_SYN, b""),
            &mut eg,
        );
        assert_eq!(
            eg.connects, MAX_CONNS,
            "the overflow SYN did not open a new egress connection"
        );
        assert_eq!(
            nat.conns.len(),
            MAX_CONNS,
            "the table did not grow past the cap"
        );
    }

    /// The NAT advertises a shrinking receive window as the guest→host buffer
    /// fills, so the guest applies real backpressure instead of the NAT buffering
    /// without bound. With a full buffer the advertised window is zero.
    #[test]
    fn from_guest_backpressure_shrinks_the_advertised_window() {
        let mut nat = Nat::new();
        let mut eg = MockEgress::new(b"");
        // Establish a connection.
        nat.on_guest_frame(
            &guest_tcp(40000, [10, 0, 2, 9], 8080, 1000, 0, F_SYN, b""),
            &mut eg,
        );
        nat.poll(&mut eg);
        let synack = nat.take_rx().unwrap();
        let our_isn = u32::from_be_bytes([
            synack[14 + 20 + 4],
            synack[14 + 20 + 5],
            synack[14 + 20 + 6],
            synack[14 + 20 + 7],
        ]);
        nat.on_guest_frame(
            &guest_tcp(
                40000,
                [10, 0, 2, 9],
                8080,
                1001,
                our_isn.wrapping_add(1),
                F_ACK,
                b"",
            ),
            &mut eg,
        );
        // Simulate a backed-up host side: stuff from_guest right up to the cap so
        // the *next* accepted payload advertises a (near-)zero window.
        nat.conns[0]
            .from_guest
            .extend(core::iter::repeat_n(0u8, FROM_GUEST_CAP));
        // A payload segment now elicits a window-bearing ACK with window 0.
        nat.on_guest_frame(
            &guest_tcp(
                40000,
                [10, 0, 2, 9],
                8080,
                1001,
                our_isn.wrapping_add(1),
                F_ACK,
                b"x",
            ),
            &mut eg,
        );
        let ack = nat.take_rx().expect("a windowed ACK");
        let t = &ack[14 + 20..];
        let window = u16::from_be_bytes([t[14], t[15]]);
        assert_eq!(
            window, 0,
            "a full guest→host buffer advertises a zero window"
        );
    }
}
