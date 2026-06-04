# holospaces egress extension — local internet for a browser-tab guest

A browser tab has no raw sockets, so a holospace's guest (a real Linux + the
devcontainer's binaries, running in the tab) cannot reach the internet on its
own. There are three ways to give it one — and this extension is the one that
needs **no other device at all**:

| Egress surface | How | When |
|---|---|---|
| **holospaces-node** (CC-39) | a flashed device you own forwards guest TCP over a WebSocket | you have a node on your network |
| **mesh** (CC-38) | route over the WebRTC content mesh to an exit peer | a peer in your mesh can exit |
| **this extension** | **Direct Sockets** (`TCPSocket`) opened *locally in the browser* | a self-contained Chromebook — nothing else needed |

The extension is a **local egress node in the browser**. It speaks the *exact
same* egress protocol the browser peer uses for a node (the OPEN/DATA/CLOSE
framing, CC-16; the node implements it with `std::net::TcpStream`,
`crates/holospaces-node/src/egress.rs`) — only here each guest connection is a
`TCPSocket` the extension opens. So `apt`/`pip`/`npm`, a `git` clone, ssh, an
outbound socket all work on a Chromebook with **no node, no relay, no proxy**.

## Why an extension (and not the page)

Two extension powers a page does not have:

1. **Direct Sockets** (`TCPSocket`/`UDPSocket` in the service worker) — raw
   TCP/UDP to arbitrary hosts. This is the one that gives the guest *arbitrary*
   internet. (Direct Sockets is a powerful, gated capability; depending on the
   Chrome channel it may require an enterprise policy or a `chrome://flags`
   opt-in. Confirm against your Chrome version.)
2. **CORS-free `fetch()`** (`host_permissions`) — the service worker is exempt
   from CORS, so it can pull the CORS-blocked registries/CDNs (Docker Hub, ghcr)
   the page cannot, and feed them to the content network as κ-content. (The
   content path; the socket path above is the egress one.)

It is the **operator's own** extension, installed by them — self-sovereign, like
a node is a device you own. Only the operator's holospaces origins may talk to it
(`externally_connectable`), and it forwards content it cannot perceive (the
egress is content-blind — SEC-7).

## Files

- `manifest.json` — MV3; Direct Sockets + `host_permissions` + the operator's
  origins in `externally_connectable`.
- `background.js` — the service worker: the egress protocol over `TCPSocket`
  (mirrors the proven node `EgressServer`).
- `connector.js` — the page side: the holospaces tab opens a `chrome.runtime`
  port and hands the guest's egress frames to the extension, exactly as it would
  a node's WebSocket.

## Install (developer / unpacked)

1. `chrome://extensions` → enable Developer mode → **Load unpacked** → this
   folder.
2. Copy the extension id into the holospaces page (or set
   `HOLOSPACES_EGRESS_EXTENSION_ID` in `connector.js` before publishing).
3. The guest's network then exits through the extension's sockets — local,
   no node.

## Integration + verification status

The extension and connector are complete artifacts. Binding them to the browser
peer's guest networking is an **extension-backed `Egress`** (the wasm NAT drains
outbound frames to the connector and feeds inbound frames from it) — the local
analogue of `WsEgress` (which targets a node's WebSocket); the egress *mechanism*
is the same one CC-16/CC-39 prove. End-to-end verification needs a real Chrome
with the extension loaded and Direct Sockets enabled (it cannot run in headless
CI), so it is exercised manually, not in the hermetic gate.
