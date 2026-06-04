//! **The egress exit — a holospaces node forwards a browser peer's guest TCP to
//! the real internet.**
//!
//! A browser tab cannot open raw sockets, so a guest's arbitrary internet
//! traffic (an `apt`/`pip`/`npm` mirror, a `git` clone, an outbound socket) must
//! leave the tab through a peer that *has* a NIC. This node is that peer — the
//! mesh's **exit node**, a device you own (a flashed low-powered board), not a
//! bespoke external proxy.
//!
//! The browser already speaks the egress protocol (the `WsEgress` framing,
//! `crates/holospaces-web/src/wsnet.rs`): a tiny binary framing multiplexes every
//! guest TCP connection over one transport, keyed by a connection id —
//!
//! | dir | opcode | body |
//! |-----|--------|------|
//! | tab → node | `0x01` OPEN  | id(4) ip(4) port(2) |
//! | tab → node | `0x02` DATA  | id(4) bytes… |
//! | tab → node | `0x03` CLOSE | id(4) |
//! | node → tab | `0x11` OPENED | id(4) |
//! | node → tab | `0x12` DATA   | id(4) bytes… |
//! | node → tab | `0x13` CLOSED | id(4) |
//! | node → tab | `0x14` FAILED | id(4) |
//!
//! [`EgressServer`] is the transport-agnostic protocol handler: it owns the real
//! `TcpStream`s and turns inbound frames into the connections + outbound frames
//! the tab reads. The node's main loop carries the frames over a WebSocket (the
//! browser's transport); the handler itself is plain `std` and testable against a
//! real socket.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

const OP_OPEN: u8 = 0x01;
const OP_DATA: u8 = 0x02;
const OP_CLOSE: u8 = 0x03;
const OP_OPENED: u8 = 0x11;
const OP_RDATA: u8 = 0x12;
const OP_CLOSED: u8 = 0x13;
const OP_FAILED: u8 = 0x14;

/// How long to wait for a guest-requested connection to establish before
/// reporting `FAILED` — bounded so one slow host cannot stall the node.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The per-connection read poll budget — short, so [`poll`](EgressServer::poll)
/// drains ready bytes without blocking the node's single loop.
const READ_TIMEOUT: Duration = Duration::from_millis(1);
/// Max bytes drained from one connection per [`poll`](EgressServer::poll) chunk.
const READ_CHUNK: usize = 16 * 1024;

/// The egress protocol handler: owns the live `TcpStream`s a browser peer's
/// guest opened through this node, and translates the egress framing to/from
/// real socket I/O. Transport-agnostic (the node's loop carries the frames over
/// a WebSocket); plain `std`, so it is exercised against a real socket in tests.
#[derive(Default)]
pub struct EgressServer {
    conns: HashMap<u32, TcpStream>,
}

impl EgressServer {
    /// A node egress handler with no open connections.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of guest connections currently open through this node.
    #[must_use]
    pub fn open_connections(&self) -> usize {
        self.conns.len()
    }

    /// Process one inbound egress frame from the browser peer, performing the
    /// real socket action it requests, and return the frames to send back to the
    /// tab (an `OPENED`/`FAILED` for an `OPEN`; nothing for `DATA`/`CLOSE`).
    pub fn handle_frame(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        if frame.len() < 5 {
            return Vec::new();
        }
        let op = frame[0];
        let id = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        match op {
            OP_OPEN => {
                if frame.len() < 11 {
                    return vec![header(OP_FAILED, id)];
                }
                let ip = Ipv4Addr::new(frame[5], frame[6], frame[7], frame[8]);
                let port = u16::from_be_bytes([frame[9], frame[10]]);
                let addr = SocketAddr::new(IpAddr::V4(ip), port);
                match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                    Ok(stream) => {
                        // Bounded-blocking reads so `poll` drains ready bytes and
                        // returns; writes stay blocking (guest payload is small,
                        // and a stalled write naturally backpressures the guest).
                        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                        self.conns.insert(id, stream);
                        vec![header(OP_OPENED, id)]
                    }
                    Err(_) => vec![header(OP_FAILED, id)],
                }
            }
            OP_DATA => {
                if let Some(stream) = self.conns.get_mut(&id) {
                    if stream.write_all(&frame[5..]).is_err() {
                        self.conns.remove(&id);
                        return vec![header(OP_CLOSED, id)];
                    }
                }
                Vec::new()
            }
            OP_CLOSE => {
                self.conns.remove(&id);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Drain ready inbound bytes (and detect closes) from every open connection,
    /// returning the `DATA`/`CLOSED` frames to deliver to the tab. The node calls
    /// this each loop turn, between reading the browser's frames.
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut closed = Vec::new();
        let mut buf = [0u8; READ_CHUNK];
        for (&id, stream) in &mut self.conns {
            match stream.read(&mut buf) {
                Ok(0) => closed.push(id), // EOF — the remote host closed.
                Ok(n) => {
                    let mut frame = header(OP_RDATA, id);
                    frame.extend_from_slice(&buf[..n]);
                    out.push(frame);
                }
                // A bounded-read timeout (no data yet) is not a close.
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(_) => closed.push(id),
            }
        }
        for id in closed {
            self.conns.remove(&id);
            out.push(header(OP_CLOSED, id));
        }
        out
    }
}

/// A 5-byte egress frame header: `op | id(4 BE)`.
fn header(op: u8, id: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5);
    frame.push(op);
    frame.extend_from_slice(&id.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// A localhost TCP echo server on an ephemeral port; returns its address.
    fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        addr
    }

    fn open_frame(id: u32, addr: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(v4) = addr else {
            panic!("ipv4")
        };
        let mut f = header(OP_OPEN, id);
        f.extend_from_slice(&v4.ip().octets());
        f.extend_from_slice(&v4.port().to_be_bytes());
        f
    }

    fn data_frame(id: u32, bytes: &[u8]) -> Vec<u8> {
        let mut f = header(OP_DATA, id);
        f.extend_from_slice(bytes);
        f
    }

    /// The node connects a guest stream to a real host, forwards the guest's
    /// bytes, and returns the host's reply as egress frames — the exit path a
    /// browser tab's `curl`/`apt` rides.
    #[test]
    fn forwards_guest_tcp_to_a_real_host_and_back() {
        let addr = echo_server();
        let mut node = EgressServer::new();

        // OPEN → the node connects and reports OPENED.
        let resp = node.handle_frame(&open_frame(7, addr));
        assert_eq!(
            resp,
            vec![header(OP_OPENED, 7)],
            "the node opened the connection"
        );
        assert_eq!(node.open_connections(), 1);

        // DATA → forwarded to the host; the echo comes back as an RDATA frame.
        node.handle_frame(&data_frame(7, b"hello internet"));
        let mut got = Vec::new();
        for _ in 0..200 {
            for frame in node.poll() {
                if frame.first() == Some(&OP_RDATA) {
                    got.extend_from_slice(&frame[5..]);
                }
            }
            if got == b"hello internet" {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, b"hello internet", "the host's reply reached the tab");

        // CLOSE → the node drops the connection.
        node.handle_frame(&[OP_CLOSE, 0, 0, 0, 7]);
        assert_eq!(node.open_connections(), 0, "the connection is closed");
    }

    /// A connection to an unreachable address fails loudly (`FAILED`), it is not
    /// silently dropped — the guest learns the connection did not establish.
    #[test]
    fn an_unreachable_host_reports_failed() {
        let mut node = EgressServer::new();
        // Port 1 on localhost: nothing listens — connect fails fast.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let resp = node.handle_frame(&open_frame(3, addr));
        assert_eq!(resp, vec![header(OP_FAILED, 3)]);
        assert_eq!(node.open_connections(), 0);
    }

    /// SEC-7 (boundary) — the egress is **content-blind**: it forwards the guest's
    /// payload as opaque bytes and never perceives or alters it. The
    /// `EgressServer` holds only sockets — no `KappaStore`, no identity, no
    /// base-frame — so an arbitrary binary payload (every byte value, including
    /// bytes that resemble frame opcodes or κ-content) is delivered byte-identical
    /// through the node. The node is a pipe, not an observer.
    #[test]
    fn the_egress_forwards_opaque_content_without_perceiving_it() {
        let addr = echo_server();
        let mut node = EgressServer::new();
        node.handle_frame(&open_frame(9, addr));

        // Every byte value 0..=255 — opaque payload the node must not interpret.
        let payload: Vec<u8> = (0u16..256).map(|b| b as u8).collect();
        node.handle_frame(&data_frame(9, &payload));

        let mut got = Vec::new();
        for _ in 0..200 {
            for frame in node.poll() {
                if frame.first() == Some(&OP_RDATA) {
                    got.extend_from_slice(&frame[5..]);
                }
            }
            if got.len() >= payload.len() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            got, payload,
            "opaque content is forwarded byte-identical, never perceived"
        );
    }
}
