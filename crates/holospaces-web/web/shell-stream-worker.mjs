// shell-stream-worker.mjs — CC-64 "instant paint, then live" resume of the warm Alpine .holo.
//
// The product win: the warm snapshot's console ALREADY holds the `holo$` prompt, so first paint
// needs ZERO guest instructions and ZERO of the 38 MiB RAM. We fetch a tiny console "header"
// (~16 KiB) and paint it IMMEDIATELY (sub-second, tiny transfer), THEN load the full kblob in the
// background and swap to the live machine — the painted text is a byte-prefix of the live terminal,
// so the swap is seamless. This is additive: the whole-blob resume path (shell-worker.mjs) is
// untouched; this worker streams the *paint* ahead of the machine.
import init, { X64Workspace } from "./pkg/holospaces_web.js";

// Count every byte we pull, so the test can prove "< 2 MiB transferred before the prompt".
let bytesFetched = 0;
const fetchCounted = async (p) => {
  const buf = await (await fetch(p)).arrayBuffer();
  bytesFetched += buf.byteLength;
  return new Uint8Array(buf);
};
const enc = new TextEncoder();
let ws = null;
let lastLen = -1;
const t0 = performance.now();

async function main() {
  // ── Phase 1: INSTANT PAINT — the console header only (~16 KiB), no wasm, no kblob. ──
  const header = await fetchCounted("./fixtures/x64-alpine-shell.console.txt");
  const headerText = new TextDecoder().decode(header);
  self.postMessage({
    term: headerText,
    painted: true,
    paintMs: Math.round(performance.now() - t0),
    paintBytes: bytesFetched,
  });

  // ── Phase 2: BACKGROUND — bring up the live machine while the prompt is already on screen. ──
  await init();
  const blob = await fetchCounted("./fixtures/x64-alpine-shell.kblob");
  const tr = performance.now();
  ws = X64Workspace.resume_kappa(blob);
  self.postMessage({
    live: true,
    resumeMs: Math.round(performance.now() - tr),
    liveMs: Math.round(performance.now() - t0),
    totalBytes: bytesFetched,
  });

  // ── Phase 3: LIVE — tick the machine; terminal() starts with the same header text. ──
  const tick = () => {
    ws.run(2_000_000);
    const term = ws.terminal();
    if (term.length !== lastLen) {
      lastLen = term.length;
      self.postMessage({ term });
    }
    setTimeout(tick, 0);
  };
  tick();
}

self.onmessage = (e) => {
  const d = e.data;
  if (d && d.key !== undefined && ws) ws.feed_input(enc.encode(d.key));
};

main().catch((err) => self.postMessage({ error: String((err && err.stack) || err) }));
