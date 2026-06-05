// Page-side connector to the **holospaces egress extension** — the local egress
// surface. The holospaces tab uses this to hand the guest's egress frames to the
// extension (which opens the raw sockets a tab cannot), receiving the host's
// replies back. It is the *exact same* OPEN/DATA/CLOSE framing the browser peer
// sends a holospaces-node over a WebSocket (`wsnet.rs`, CC-16) — the carrier is
// just a `chrome.runtime` port to the extension instead of a `ws://` socket, so
// the guest's networking is unchanged; only where it exits differs.
//
// Usage: pass the channel this returns to the browser peer's egress (an
// extension-backed `Egress` that drains outbound frames here and feeds inbound
// frames from `onFrame`) — the local analogue of pointing the guest at a node.

/// The published extension id (set when the extension is published to the store;
/// for an unpacked dev load, read it from chrome://extensions and pass it in).
export const HOLOSPACES_EGRESS_EXTENSION_ID = "";

/// Whether a page can talk to an installed holospaces egress extension at all
/// (the extension declares this origin in `externally_connectable`).
export function egressExtensionAvailable() {
  return typeof chrome !== "undefined" && !!chrome.runtime && !!chrome.runtime.connect;
}

/// Fetch a URL through the router's **content role** — the extension's CORS-free
/// `fetch()` pulls registries/CDNs the page cannot (the image layers the browser
/// peer assembles into the devcontainer rootfs). Returns `{status, body}` (body a
/// `Uint8Array`), or `null` if no extension is reachable / the fetch failed.
let _contentPort = null, _fetchSeq = 0;
const _pendingFetch = new Map();

function contentChannel(extensionId) {
  if (_contentPort) return _contentPort;
  if (!egressExtensionAvailable() || !extensionId) return null;
  const port = chrome.runtime.connect(extensionId, { name: "holospaces-content" });
  port.onMessage.addListener((m) => {
    const p = _pendingFetch.get(m.id);
    if (!p) return;
    if (m.type === "head") { p.status = m.status; p.contentType = m.contentType; }
    else if (m.type === "chunk") { p.chunks.push(Uint8Array.from(m.bytes)); }
    else if (m.type === "end") {
      _pendingFetch.delete(m.id);
      const total = p.chunks.reduce((n, c) => n + c.length, 0);
      const body = new Uint8Array(total);
      let o = 0;
      for (const c of p.chunks) { body.set(c, o); o += c.length; }
      p.resolve({ status: p.status, contentType: p.contentType, body });
    } else if (m.type === "error") { _pendingFetch.delete(m.id); p.resolve(null); }
  });
  port.onDisconnect.addListener(() => {
    _contentPort = null;
    for (const [, p] of _pendingFetch) p.resolve(null);
    _pendingFetch.clear();
  });
  _contentPort = port;
  return port;
}

export async function routerFetch(url, headers = {}, extensionId = HOLOSPACES_EGRESS_EXTENSION_ID) {
  const port = contentChannel(extensionId);
  if (!port) return null;
  const id = ++_fetchSeq;
  return new Promise((resolve) => {
    _pendingFetch.set(id, { chunks: [], status: 0, contentType: "", resolve });
    try { port.postMessage({ type: "fetch", id, url, headers }); }
    catch { _pendingFetch.delete(id); resolve(null); }
  });
}

/// Open the egress channel to the extension. Returns `{ send, onFrame, close }`:
/// `send(frame)` posts a guest egress frame (OPEN/DATA/CLOSE), `onFrame(cb)`
/// delivers the extension's frames (OPENED/DATA/CLOSED/FAILED), `close()` tears
/// the channel down (the extension drops every socket the tab owned). Returns
/// `null` if no extension is reachable.
export function connectEgress(extensionId = HOLOSPACES_EGRESS_EXTENSION_ID) {
  if (!egressExtensionAvailable() || !extensionId) return null;
  let port;
  try {
    port = chrome.runtime.connect(extensionId);
  } catch {
    return null;
  }
  const listeners = [];
  port.onMessage.addListener((msg) => {
    const f = msg instanceof Uint8Array ? msg : Uint8Array.from(msg);
    for (const cb of listeners) cb(f);
  });
  return {
    send: (frame) => port.postMessage(Array.from(frame)),
    onFrame: (cb) => listeners.push(cb),
    close: () => {
      try {
        port.disconnect();
      } catch {
        /* already gone */
      }
    },
  };
}
