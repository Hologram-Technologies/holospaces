// open-worker.mjs — the browser `open(κ)` worker (CC-76).
//
// Turns one link into a live app, 100% serverless. Two ways to open:
//   • ?k=<path.holo>          — resume a self-contained warm blob (X64Workspace.resume_kappa).
//   • ?k=blake3:<hex>&store=… — REAL κ-URL: the handle is a content-addressed κ. Fetch the manifest by κ,
//                               fetch each UNIQUE page by κ, and resume_kappa_streamed — every page
//                               L5-verified in wasm, so a tampered/missing page is refused. Only the
//                               unique pages cross the wire.
// Either way: enable the in-tab loopback bridge, dial the guest's server, hand the page its real response.
import init, { X64Workspace, kappa_manifest_pages, verify_kappa } from "./pkg/holospaces_web.js";

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const enc = new TextEncoder();
const dec = new TextDecoder();
const safe = (k) => k.replace(/:/g, "_"); // ":" is not a valid filename char (Windows)
let ws = null;

async function open(msg) {
  await init();
  const port = Number(msg.port || 8080);
  const t0 = performance.now();

  if ((msg.src || "").startsWith("blake3:")) {
    // ── real κ-URL: content-addressed, L5-verified page fetch ──
    const store = msg.store || "./fixtures/store";
    const kappa = msg.src;
    self.postMessage({ stage: "fetching", note: `fetching manifest by κ ${kappa.slice(0, 20)}…` });
    const manifest = await bytes(`${store}/${safe(kappa)}`);
    if (!verify_kappa(manifest, kappa)) { self.postMessage({ stage: "error", error: "manifest κ mismatch (L5)" }); return; }
    const pageKappas = kappa_manifest_pages(manifest);
    self.postMessage({ stage: "fetching", note: `fetching ${pageKappas.length} unique pages by κ` });
    const map = new Map();
    let done = 0;
    const CONC = 32;
    for (let i = 0; i < pageKappas.length; i += CONC) {
      await Promise.all(pageKappas.slice(i, i + CONC).map(async (k) => {
        map.set(k, await bytes(`${store}/${safe(k)}`));
        if (++done % 500 === 0) self.postMessage({ stage: "fetching", note: `pages ${done}/${pageKappas.length}` });
      }));
    }
    self.postMessage({ stage: "resuming" });
    ws = X64Workspace.resume_kappa_streamed(manifest, (k) => map.get(k) || null); // wasm verifies each page (L5)
  } else {
    // ── self-contained warm blob ──
    self.postMessage({ stage: "fetching", note: `fetching warm .holo: ${msg.src}` });
    const blob = await bytes(msg.src);
    self.postMessage({ stage: "resuming" });
    ws = X64Workspace.resume_kappa(blob);
  }

  const resumeMs = Math.round(performance.now() - t0);
  self.postMessage({ stage: "resumed", resumeMs, term: ws.terminal() });

  if (!ws.enable_loopback()) { self.postMessage({ stage: "error", error: "no virtio-net device (enable_loopback failed)" }); return; }
  for (let i = 0; i < 10; i++) ws.run(5_000_000); // let the guest schedule (server blocked in accept())

  const id = ws.dial_guest(port);
  if (id === undefined || id === null) { self.postMessage({ stage: "error", error: `dial_guest(${port}) failed` }); return; }
  ws.guest_send(id, enc.encode("GET / HTTP/1.0\r\nHost: app\r\n\r\n"));

  let raw = new Uint8Array(0);
  const append = (a, b) => { const c = new Uint8Array(a.length + b.length); c.set(a); c.set(b, a.length); return c; };
  for (let i = 0; i < 400; i++) {
    ws.run(2_000_000);
    const chunk = ws.guest_recv(id);
    if (chunk && chunk.length) raw = append(raw, chunk);
    self.postMessage({ stage: "streaming", term: ws.terminal() });
    if (raw.length && looksComplete(raw)) break;
  }

  const text = dec.decode(raw);
  const sep = text.indexOf("\r\n\r\n");
  const status = (text.split("\r\n", 1)[0] || "").trim();
  const body = sep >= 0 ? text.slice(sep + 4) : text;
  self.postMessage({ stage: "rendered", resumeMs, status, body, term: ws.terminal() });
}

function looksComplete(raw) {
  const head = dec.decode(raw.slice(0, Math.min(raw.length, 512)));
  const sep = head.indexOf("\r\n\r\n");
  if (sep < 0) return false;
  const m = /content-length:\s*(\d+)/i.exec(head);
  if (m) return raw.length >= sep + 4 + Number(m[1]);
  return raw.length > sep + 4;
}

self.onmessage = (e) => {
  open(e.data).catch((err) => self.postMessage({ stage: "error", error: String((err && err.stack) || err) }));
};
