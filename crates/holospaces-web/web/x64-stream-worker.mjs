// x64-stream-worker.mjs — Phase 4 in-tab: STREAM a resume over the network. Fetch the published
// manifest, then pull each UNIQUE page one-by-κ over the page's own fetch, VERIFYING each on
// receipt (L5), and resume the machine from the streamed pages — never holding the whole snapshot.
import init, { X64Workspace, kappa_manifest_pages, verify_kappa } from "./pkg/holospaces_web.js";

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const safe = (k) => k.replace(/[:/]/g, "_");

self.onmessage = async () => {
  try {
    self.postMessage({ stage: "init" });
    await init();

    self.postMessage({ stage: "manifest" });
    const manifest = await bytes("./fixtures/stream/manifest.bin");
    const ks = kappa_manifest_pages(manifest);

    // Stream each unique page by κ over HTTP, verifying on receipt before it enters the map.
    // Fetch in parallel batches (the transport pipelines requests; the seam is unchanged).
    const map = new Map();
    let got = 0;
    const CONC = 64;
    const t0 = performance.now();
    for (let i = 0; i < ks.length; i += CONC) {
      const batch = ks.slice(i, i + CONC);
      const pairs = await Promise.all(batch.map(async (k) => {
        const b = await bytes("./fixtures/stream/pages/" + safe(k));
        if (!verify_kappa(b, k)) throw new Error("page failed κ verification on receipt: " + k);
        return [k, b];
      }));
      for (const [k, b] of pairs) { map.set(k, b); got += b.length; }
      self.postMessage({ stage: "streaming", fetched: Math.min(i + CONC, ks.length), total: ks.length });
    }
    const tFetch = performance.now() - t0;

    // Resume from the streamed pages (Rust re-verifies each as it reconstructs).
    self.postMessage({ stage: "resume" });
    const t1 = performance.now();
    const ws = X64Workspace.resume_kappa_streamed(manifest, (k) => map.get(k) || null);
    const tResume = performance.now() - t1;
    ws.run(2_000_000); // prove it's live

    self.postMessage({
      ok: true,
      pages: ks.length,
      mib: got / 1048576,
      manifestKiB: manifest.length / 1024,
      tFetch,
      tResume,
      consoleBytes: ws.terminal().length,
      tail: ws.terminal().split("\n").slice(-6).join("\n"),
    });
  } catch (e) {
    self.postMessage({ ok: false, error: String((e && e.stack) || e) });
  }
};
