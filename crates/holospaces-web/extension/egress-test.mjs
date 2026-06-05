// V&V for the holospaces egress extension (CC-41). The extension's service
// worker (`background.js`) implements the egress protocol (OPEN/DATA/CLOSE) over
// the Direct Sockets API. Direct Sockets needs a real, gated Chrome to run for
// real — but the worker's *logic* is verifiable here, hermetically: we polyfill
// `TCPSocket` (backed by `node:net`, faithful to the Direct Sockets contract —
// `new TCPSocket(host,port)`, `await .opened → {readable, writable}`, `.close()`)
// and mock `chrome.runtime`, then drive the protocol against a real echo server.
// This proves the extension is wire-compatible with the browser's WsEgress
// (CC-16) and the node's EgressServer (CC-39): same frames, same behaviour.

import net from "node:net";
import { Duplex } from "node:stream";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const dir = path.dirname(fileURLToPath(import.meta.url));
let failed = false;
const check = (ok, msg) => { console.log((ok ? "  ✓ " : "  ✗ ") + msg); if (!ok) failed = true; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── Direct Sockets (TCPSocket) polyfill, backed by node:net ──────────────────
globalThis.TCPSocket = class {
  constructor(host, port) {
    const sock = net.connect(port, host);
    this._sock = sock;
    this.opened = new Promise((resolve, reject) => {
      sock.once("connect", () => resolve(Duplex.toWeb(sock))); // {readable, writable}
      sock.once("error", reject);
    });
  }
  close() { try { this._sock.destroy(); } catch {} }
};

// ── chrome.runtime.onConnectExternal mock ────────────────────────────────────
const connectHandlers = [];
const messageHandlers = [];
globalThis.chrome = {
  runtime: {
    onConnectExternal: { addListener: (h) => connectHandlers.push(h) },
    onMessageExternal: { addListener: (h) => messageHandlers.push(h) },
  },
  permissions: { request: async () => true },
};

// Load the service worker — it registers the connection + message handlers.
eval(readFileSync(path.join(dir, "background.js"), "utf8"));

// ── A fake tab port; capture the frames the extension posts back ─────────────
const inbound = []; // frames extension → tab
const msgListeners = [];
const port = {
  postMessage: (m) => inbound.push(Uint8Array.from(m)),
  onMessage: { addListener: (cb) => msgListeners.push(cb) },
  onDisconnect: { addListener: () => {} },
};
connectHandlers[0](port); // a holospaces tab connects
const sendToExt = (frame) => msgListeners[0](Array.from(frame));

// ── frame helpers (the egress protocol) ──────────────────────────────────────
const OP_OPEN = 0x01, OP_DATA = 0x02, OP_OPENED = 0x11, OP_RDATA = 0x12, OP_FAILED = 0x14;
const u32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
const openFrame = (id, ip, p) => new Uint8Array([OP_OPEN, ...u32(id), ...ip, (p >> 8) & 255, p & 255]);
const dataFrame = (id, bytes) => { const f = new Uint8Array(5 + bytes.length); f.set([OP_DATA, ...u32(id)]); f.set(bytes, 5); return f; };
const waitFor = async (pred, ms = 3000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) { const f = inbound.find((x) => x && pred(x)); if (f) return f; await sleep(10); }
  return null;
};

// ── A real echo server (the "host" on the internet) ──────────────────────────
const echo = net.createServer((s) => s.pipe(s));
await new Promise((r) => echo.listen(0, "127.0.0.1", r));
const echoPort = echo.address().port;

try {
  // OPEN → the extension opens a real socket and reports OPENED.
  sendToExt(openFrame(7, [127, 0, 0, 1], echoPort));
  check(!!(await waitFor((f) => f[0] === OP_OPENED && f[4] === 7)),
    "OPEN → the extension opens a raw socket (Direct Sockets) and reports OPENED");

  // DATA → forwarded to the host; the echo comes back framed as RDATA.
  const payload = new TextEncoder().encode("hello, internet, via direct sockets");
  sendToExt(dataFrame(7, payload));
  let got = [];
  const t0 = Date.now();
  while (Date.now() - t0 < 3000) {
    for (let i = 0; i < inbound.length; i++) {
      const f = inbound[i];
      if (f && f[0] === OP_RDATA && f[4] === 7) { got.push(...f.subarray(5)); inbound[i] = null; }
    }
    if (new TextDecoder().decode(Uint8Array.from(got)) === "hello, internet, via direct sockets") break;
    await sleep(10);
  }
  check(new TextDecoder().decode(Uint8Array.from(got)) === "hello, internet, via direct sockets",
    "DATA is forwarded to the host and the reply framed back as RDATA (egress works)");

  // An unreachable host reports FAILED — no silent drop (matches the node, SEC-7).
  sendToExt(openFrame(9, [127, 0, 0, 1], 1)); // port 1: nothing listens
  check(!!(await waitFor((f) => f[0] === OP_FAILED && f[4] === 9)),
    "an unreachable host reports FAILED (no silent drop)");
} finally {
  echo.close();
}

// ── Content role: CORS-free fetch, STREAMED over a content port ──────────────
// The router's other half: a registry/CDN fetch a tab can't do (CORS), streamed
// in chunks (a multi-MB layer can't cross runtime messaging as one array).
// Polyfill fetch (the real one is the service worker's CORS-free fetch) and drive
// a "holospaces-content" port — proving the worker fetches + chunks the bytes the
// browser peer reassembles into the rootfs.
globalThis.fetch = async (url) => ({
  status: 200,
  headers: { get: (h) => (h.toLowerCase() === "content-type" ? "application/octet-stream" : null) },
  arrayBuffer: async () => new TextEncoder().encode("layer-bytes-for:" + url).buffer,
});
const contentMsgs = [];
const contentListeners = [];
const contentPort = {
  name: "holospaces-content",
  postMessage: (m) => contentMsgs.push(m),
  onMessage: { addListener: (cb) => contentListeners.push(cb) },
  onDisconnect: { addListener: () => {} },
};
connectHandlers[0](contentPort); // serveContent registers its listener on the port
await contentListeners[0]({
  type: "fetch",
  id: 1,
  url: "https://registry-1.docker.io/v2/library/debian/blobs/sha256:abc",
});
const head = contentMsgs.find((m) => m.type === "head" && m.id === 1);
const end = contentMsgs.find((m) => m.type === "end" && m.id === 1);
const got = [];
for (const m of contentMsgs) if (m.type === "chunk" && m.id === 1) got.push(...m.bytes);
check(
  !!head && head.status === 200 && !!end &&
    new TextDecoder().decode(Uint8Array.from(got)).startsWith("layer-bytes-for:"),
  "the router fetches CORS-blocked content and streams the layer back in chunks (content role)",
);

console.log(failed ? "EXTENSION-EGRESS-TEST: FAILED" : "EXTENSION-EGRESS-TEST: PASS (router: egress over Direct Sockets + content over CORS-free fetch)");
process.exit(failed ? 1 : 0);
