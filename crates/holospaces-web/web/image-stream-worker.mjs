// image-stream-worker.mjs — CC-65 G5: resume ANY image's warm .holo in a browser with CC-64 instant
// paint. Generic over the fixture: the page sends {start, kblob, header} (paths), we paint the tiny
// console header instantly (the boot-log / ready banner already in the snapshot), then background-load the
// kblob and go live — the painted text is a byte-prefix of the live terminal (seamless). For a server
// image the "terminal" is the boot log + its HOLO-SERVED-N heartbeats, which keep growing once live.
// Resumes from ./pkg (the committed browser wasm). NOTE (CC-66): a κ-snapshot only round-trips through a
// wasm built from the SAME core revision that produced it. To run a current-core image kblob in-browser,
// rebuild ./pkg from the current core AND regenerate the kblob in the same step (atomic — see build-site.sh
// `--no-opt` invocation), then this path serves any image. (The shell kblob + committed ./pkg are one
// vintage; CC-65 server kblobs are another.)
import init, { X64Workspace } from "./pkg/holospaces_web.js";

let bytesFetched = 0;
const fetchCounted = async (p) => {
  const buf = await (await fetch(p)).arrayBuffer();
  bytesFetched += buf.byteLength;
  return new Uint8Array(buf);
};
const enc = new TextEncoder();
let ws = null;
let lastLen = -1;

async function run(kblobPath, headerPath) {
  const t0 = performance.now();
  // Phase 1: INSTANT PAINT — the console header only (~KiB), no wasm, no kblob.
  const headerText = new TextDecoder().decode(await fetchCounted(headerPath));
  self.postMessage({ term: headerText, painted: true, paintMs: Math.round(performance.now() - t0), paintBytes: bytesFetched });

  // Phase 2: BACKGROUND — bring up the live machine while the header is already on screen.
  await init();
  const blob = await fetchCounted(kblobPath);
  const tr = performance.now();
  ws = X64Workspace.resume_kappa(blob);
  self.postMessage({ live: true, resumeMs: Math.round(performance.now() - tr), liveMs: Math.round(performance.now() - t0), totalBytes: bytesFetched });

  // Phase 3: LIVE — tick the machine; terminal() starts with the same header text.
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
  if (d && d.start) {
    run(d.kblob, d.header).catch((err) => self.postMessage({ error: String((err && err.stack) || err) }));
  } else if (d && d.key !== undefined && ws) {
    ws.feed_input(enc.encode(d.key));
  }
};
