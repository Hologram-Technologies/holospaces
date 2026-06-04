//! End-to-end egress over the **real WebSocket transport** the browser peer uses
//! — the node serves the `WsEgress` protocol (`CC-16`), and a WebSocket client
//! (standing in for a browser tab's `WsEgress`) opens a guest connection through
//! the node to a real host and gets the reply back. This exercises the exact
//! path a Chromebook's holospace rides to reach the internet.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use tungstenite::{connect, Message};

const OP_OPEN: u8 = 0x01;
const OP_DATA: u8 = 0x02;
const OP_OPENED: u8 = 0x11;
const OP_RDATA: u8 = 0x12;

/// A localhost TCP echo server on an ephemeral port; returns its address.
fn echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 || stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    });
    addr
}

/// Start the node's WebSocket egress server on an ephemeral port; returns the
/// `ws://` URL a browser peer connects to.
fn start_node() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            holospaces_node::serve_connection(stream);
        }
    });
    format!("ws://{addr}")
}

#[test]
fn a_guest_reaches_the_internet_through_the_node_over_a_real_websocket() {
    let echo = echo_server();
    let std::net::SocketAddr::V4(echo) = echo else {
        panic!("ipv4")
    };
    let url = start_node();

    // The browser peer's WsEgress connects to the node and opens a guest TCP
    // connection (OPEN id ip port).
    let (mut ws, _resp) = connect(url.as_str()).expect("connect to the node egress");
    let id: u32 = 42;
    let mut open = vec![OP_OPEN];
    open.extend_from_slice(&id.to_be_bytes());
    open.extend_from_slice(&echo.ip().octets());
    open.extend_from_slice(&echo.port().to_be_bytes());
    ws.send(Message::Binary(open)).unwrap();

    // The node reports OPENED, then echoes the DATA we send through it.
    let opened = read_frame(&mut ws);
    assert_eq!(
        opened.first(),
        Some(&OP_OPENED),
        "the node opened the guest connection"
    );

    let mut data = vec![OP_DATA];
    data.extend_from_slice(&id.to_be_bytes());
    data.extend_from_slice(b"GET / HTTP/1.0\r\n\r\n");
    ws.send(Message::Binary(data)).unwrap();

    // The host's reply comes back as an RDATA frame for our connection id.
    let mut got = Vec::new();
    for _ in 0..50 {
        let frame = read_frame(&mut ws);
        if frame.first() == Some(&OP_RDATA) {
            assert_eq!(
                &frame[1..5],
                &id.to_be_bytes(),
                "reply for our connection id"
            );
            got.extend_from_slice(&frame[5..]);
            if got == b"GET / HTTP/1.0\r\n\r\n" {
                break;
            }
        }
    }
    assert_eq!(
        got, b"GET / HTTP/1.0\r\n\r\n",
        "the host's reply reached the browser peer through the node"
    );
}

/// Read one binary WebSocket message (skipping any control frames).
fn read_frame(ws: &mut tungstenite::WebSocket<impl Read + Write>) -> Vec<u8> {
    loop {
        match ws.read().expect("read frame from the node") {
            Message::Binary(b) => return b,
            Message::Close(_) => return Vec::new(),
            _ => {}
        }
    }
}
