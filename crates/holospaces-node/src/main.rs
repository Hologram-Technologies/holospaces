//! The **holospaces node** binary — the egress exit a browser tab routes
//! through. It serves the browser peer's egress WebSocket (`WsEgress`,
//! `CC-16`): each guest TCP connection the tab opens is forwarded to the real
//! internet by the node, and the host's replies are framed back to the tab. A
//! device you flash and own is the tab's route to the network — no cloud VM, no
//! bespoke proxy.
//!
//! Listen address: `HOLOSPACES_NODE_ADDR` (default `0.0.0.0:9000`). A Chromebook
//! on the same network points its holospace's egress at `ws://<node-ip>:9000`.

use std::net::TcpListener;

fn main() {
    let addr = std::env::var("HOLOSPACES_NODE_ADDR").unwrap_or_else(|_| "0.0.0.0:9000".to_owned());
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("holospaces-node: cannot bind {addr}: {e}");
        std::process::exit(1);
    });
    eprintln!("holospaces-node: egress exit listening on ws://{addr}  (CC-16)");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move || holospaces_node::serve_connection(stream));
            }
            Err(e) => eprintln!("holospaces-node: accept failed: {e}"),
        }
    }
}
