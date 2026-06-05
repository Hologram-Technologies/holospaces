// holospaces egress — the service worker that gives a browser-tab guest raw
// internet access locally, via the **Direct Sockets API** (`TCPSocket`).
//
// A browser tab cannot open raw sockets; this extension can. It is a *local
// egress node in the browser*: it speaks the exact same egress protocol the
// browser peer already uses for a holospaces-node over a WebSocket
// (`crates/holospaces-web/src/wsnet.rs`, CC-16) and that the node implements with
// `std::net::TcpStream` (`crates/holospaces-node/src/egress.rs`, CC-39) — only
// here each guest connection is a `TCPSocket` opened by the extension. So the
// guest's `apt`/`pip`/`npm`, a `git` clone, ssh, an outbound socket all work on a
// Chromebook with no node, no relay, no proxy.
//
// The egress framing (multiplexes every guest connection over one channel by id):
//   tab → ext  0x01 OPEN  id(4) ip(4) port(2)
//   tab → ext  0x02 DATA  id(4) bytes…
//   tab → ext  0x03 CLOSE id(4)
//   ext → tab  0x11 OPENED id(4)
//   ext → tab  0x12 DATA   id(4) bytes…
//   ext → tab  0x13 CLOSED id(4)
//   ext → tab  0x14 FAILED id(4)
//
// The holospaces tab connects over `chrome.runtime.connect` (only the operator's
// own origins are allowed, per `externally_connectable`); the guest's egress
// frames flow over that port, exactly as they would to a node's WebSocket.

const OP_OPEN = 0x01;
const OP_DATA = 0x02;
const OP_CLOSE = 0x03;
const OP_OPENED = 0x11;
const OP_RDATA = 0x12;
const OP_CLOSED = 0x13;
const OP_FAILED = 0x14;

const u32be = (n) => new Uint8Array([(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255]);
const header = (op, id) => { const b = new Uint8Array(5); b[0] = op; b.set(u32be(id), 1); return b; };
function frame(op, id, body) {
  if (!body || !body.length) return header(op, id);
  const f = new Uint8Array(5 + body.length);
  f.set(header(op, id), 0);
  f.set(body, 5);
  return f;
}

// ── Content role: CORS-free fetch, STREAMED over a port ──────────────────────
// A service worker's fetch() is CORS-exempt (host_permissions), so it pulls the
// CORS-blocked registries/CDNs (Docker Hub, ghcr) the page cannot — the image
// layers the browser peer assembles into the rootfs. Image layers are large, so
// the body is streamed in chunks: a single JSON array for a multi-MB layer would
// exceed the runtime-message size limit. The page opens a port named
// "holospaces-content" and drives one fetch per { type:"fetch", id, url } message.
function serveContent(port) {
  port.onMessage.addListener(async (msg) => {
    if (!msg || msg.type !== "fetch" || typeof msg.url !== "string") return;
    try {
      const resp = await fetch(msg.url, {
        method: msg.method || "GET",
        headers: msg.headers || {},
        redirect: "follow",
      });
      const buf = new Uint8Array(await resp.arrayBuffer());
      const CHUNK = 256 * 1024;
      port.postMessage({
        type: "head",
        id: msg.id,
        status: resp.status,
        contentType: resp.headers.get("content-type") || "",
        total: buf.length,
      });
      for (let o = 0; o < buf.length; o += CHUNK) {
        port.postMessage({ type: "chunk", id: msg.id, bytes: Array.from(buf.subarray(o, o + CHUNK)) });
      }
      port.postMessage({ type: "end", id: msg.id });
    } catch (e) {
      port.postMessage({ type: "error", id: msg.id, error: String(e) });
    }
  });
}

chrome.runtime.onConnectExternal.addListener((port) => {
  // The content channel (chunked registry fetches) vs the egress channel (raw
  // sockets) — distinguished by the port name.
  if (port.name === "holospaces-content") {
    serveContent(port);
    return;
  }
  // ── Egress role: a raw socket per guest connection (keyed by conn id) ──
  const conns = new Map(); // id -> { writer, socket }
  const send = (bytes) => { try { port.postMessage(Array.from(bytes)); } catch {} };

  // OPEN: open a raw TCPSocket to the destination; pump its reads back as RDATA.
  async function open(id, ip, port_) {
    try {
      const host = `${ip[0]}.${ip[1]}.${ip[2]}.${ip[3]}`;
      // Direct Sockets — the raw socket a tab cannot have, but the extension can.
      const socket = new TCPSocket(host, port_);
      const { readable, writable } = await socket.opened;
      const writer = writable.getWriter();
      conns.set(id, { writer, socket });
      send(frame(OP_OPENED, id));

      // Pump host → guest until the remote closes.
      const reader = readable.getReader();
      (async () => {
        try {
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value && value.length) send(frame(OP_RDATA, id, value));
          }
        } catch {
          /* read error — fall through to CLOSED */
        }
        close(id);
        send(frame(OP_CLOSED, id));
      })();
    } catch {
      conns.delete(id);
      send(frame(OP_FAILED, id)); // unreachable host — reported, not a silent drop.
    }
  }

  async function data(id, bytes) {
    const c = conns.get(id);
    if (!c) return;
    try {
      await c.writer.write(bytes);
    } catch {
      close(id);
      send(frame(OP_CLOSED, id));
    }
  }

  function close(id) {
    const c = conns.get(id);
    if (!c) return;
    conns.delete(id);
    try { c.writer.close(); } catch {}
    try { c.socket.close(); } catch {}
  }

  // Guest egress frames arrive as plain arrays over the port (structured-clone).
  port.onMessage.addListener((msg) => {
    const f = msg instanceof Uint8Array ? msg : Uint8Array.from(msg);
    if (f.length < 5) return;
    const op = f[0];
    const id = (f[1] << 24) | (f[2] << 16) | (f[3] << 8) | f[4];
    if (op === OP_OPEN) {
      if (f.length < 11) { send(frame(OP_FAILED, id)); return; }
      open(id, [f[5], f[6], f[7], f[8]], (f[9] << 8) | f[10]);
    } else if (op === OP_DATA) {
      data(id, f.subarray(5));
    } else if (op === OP_CLOSE) {
      close(id);
    }
  });

  // The tab went away — drop every socket it owned.
  port.onDisconnect.addListener(() => {
    for (const id of [...conns.keys()]) close(id);
  });
});
