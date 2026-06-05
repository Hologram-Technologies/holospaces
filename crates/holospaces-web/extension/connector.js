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
export async function routerFetch(url, extensionId = HOLOSPACES_EGRESS_EXTENSION_ID) {
  if (!egressExtensionAvailable() || !extensionId) return null;
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendMessage(extensionId, { type: "holospaces-fetch", url }, (resp) => {
        if (chrome.runtime.lastError || !resp || !resp.ok) {
          resolve(null);
        } else {
          resolve({ status: resp.status, body: Uint8Array.from(resp.body) });
        }
      });
    } catch {
      resolve(null);
    }
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
