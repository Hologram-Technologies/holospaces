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

use alloc::collections::VecDeque;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

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
                }
                // host → guest
                let data = egress.recv(self.conns[i].eid);
                if !data.is_empty() {
                    self.conns[i].to_guest.extend(data);
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
            // 4. Reap a fully-closed connection.
            let c = &self.conns[i];
            if c.guest_fin && (c.fin_sent && c.snd_una == c.snd_nxt) {
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
            };
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
                }
                // host → guest (the external client's request)
                let data = ingress.recv(iid);
                if !data.is_empty() {
                    self.conns[i].to_guest.extend(data);
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
            // response was drained to the ingress in the relay step above.
            let c = &self.conns[i];
            if c.guest_fin && c.fin_sent && c.to_guest.is_empty() {
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
            if seq == self.conns[idx].rcv_nxt {
                let p = payload.to_vec();
                self.conns[idx].from_guest.extend(p);
                self.conns[idx].rcv_nxt =
                    self.conns[idx].rcv_nxt.wrapping_add(payload.len() as u32);
            }
            // ACK the current receive point (re-ACK on a retransmit).
            let c = &mut self.conns[idx];
            reply = Some(tcp_to_guest(c, F_ACK, &[], &[], self.guest_mac));
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
