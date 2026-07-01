// resume-link-worker.mjs — THE ONE-LINK LOADER (in-tab). Given the manifest κ from the page's
// URL (`#k=<κ>`), fetch the manifest BY ITS κ (verify, L5), then stream each unique RAM page
// one-by-κ (verify-on-receipt, L5) and resume a live x86-64 machine — never holding the whole
// snapshot. Identical streaming logic to the proven `x64-stream-worker.mjs`; the only change is
// that the manifest is fetched by κ from the κ-object store (`./k/<safe(κ)>`), so the URL carries
// ONE hash. The headless equivalent is `x64-link-resume-test.mjs` (verified); the publish side is
// `make-link-bundle.mjs` (verified). Swap `byKappa` for content_net/WebRtcLink for peer delivery.
import init, { X64Workspace, kappa_manifest_pages, verify_kappa } from "./pkg/holospaces_web.js";

const safe = (k) => k.replace(/[:/]/g, "_");
const bytesOf = async (url) => new Uint8Array(await (await fetch(url)).arrayBuffer());
// The transport: fetch any κ-object by its κ. Static deploy = a directory of content-addressed
// files; a peer deploy swaps this for content_net / a WebRTC peer. Either way: verify on receipt.
const byKappa = (base) => async (k) => bytesOf(base + "k/" + safe(k));

// The live machine + its run-loop. After resume we keep the CPU ticking and stream the console
// deltas to the page; keystrokes from the page go in via feed_input. Shell I/O is light, so a few
// ms of guest cycles per tick is smooth on the interpreter (no JIT needed for interactivity).
let WS = null;
const BUDGET = 4_000_000; // guest instructions per tick; shrink if keystroke echo lags
function loop() {
  if (!WS) return;
  WS.run(BUDGET);
  const d = WS.terminal_delta();
  if (d && d.length) self.postMessage({ stage: "out", delta: d }, [d.buffer]);
  setTimeout(loop, 0); // yield to the message queue so input is processed between ticks
}

self.onmessage = async (ev) => {
  // Interactive control messages (after the machine is live).
  if (ev.data && ev.data.input) {
    if (WS) WS.feed_input(ev.data.input); // bytes typed in the tab → the guest serial console
    return;
  }
  try {
    const { kappa, base = "./" } = ev.data; // κ from location.hash; base = where the κ-objects live
    self.postMessage({ stage: "init" });
    await init();
    const fetchK = byKappa(base);

    // 1) Fetch the manifest BY ITS κ and verify it (the link is just this one hash).
    self.postMessage({ stage: "manifest" });
    const manifest = await fetchK(kappa);
    if (!verify_kappa(manifest, kappa)) throw new Error("manifest does not re-derive to its κ (L5)");
    const ks = kappa_manifest_pages(manifest);

    // 2) Stream each unique page by κ, verifying on receipt before it enters the map.
    const map = new Map();
    let got = 0;
    const CONC = 64;
    const t0 = performance.now();
    for (let i = 0; i < ks.length; i += CONC) {
      const batch = ks.slice(i, i + CONC);
      const pairs = await Promise.all(
        batch.map(async (k) => {
          const b = await fetchK(k);
          if (!verify_kappa(b, k)) throw new Error("page failed κ verification on receipt: " + k);
          return [k, b];
        }),
      );
      for (const [k, b] of pairs) {
        map.set(k, b);
        got += b.length;
      }
      self.postMessage({ stage: "streaming", fetched: Math.min(i + CONC, ks.length), total: ks.length });
    }
    const tFetch = performance.now() - t0;

    // 3) Resume from the streamed pages (Rust re-verifies each as it reconstructs), then go LIVE:
    // keep the CPU ticking and stream console deltas; the page can now type into it.
    self.postMessage({ stage: "resume" });
    const t1 = performance.now();
    WS = X64Workspace.resume_kappa_streamed(manifest, (k) => map.get(k) || null);
    const tResume = performance.now() - t1;

    self.postMessage({
      ok: true,
      kappa,
      pages: ks.length,
      mib: got / 1048576,
      manifestKiB: manifest.length / 1024,
      tFetch,
      tResume,
      screen: WS.terminal(), // the full console so far (the page seeds its terminal with it)
    });
    loop(); // drive the live machine; deltas + keystrokes flow from here on
  } catch (e) {
    self.postMessage({ ok: false, error: String((e && e.stack) || e) });
  }
};
