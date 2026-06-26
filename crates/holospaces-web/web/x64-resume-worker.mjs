// x64-resume-worker.mjs — Phase 2: resume a deduped κ-blob INSIDE A BROWSER TAB (a Worker),
// never booting. Fetches the κ-blob fixture, verifies it (L5), X64Workspace.resume_kappa, and
// drives the resumed machine — posting timing + console back to the page.
import init, { X64Workspace, verify_kappa } from "./pkg/holospaces_web.js";

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const text = async (p) => (await (await fetch(p)).text()).trim();

self.onmessage = async () => {
  try {
    self.postMessage({ stage: "init" });
    await init();

    self.postMessage({ stage: "fetch-blob" });
    const blob = await bytes("./fixtures/x64-resume-snapshot.kblob");
    const k = await text("./fixtures/x64-resume-snapshot.kblob.kappa");

    // Law L5 — re-derive the served blob's κ before trusting it.
    self.postMessage({ stage: "verify", blobBytes: blob.length });
    if (!verify_kappa(blob, k)) {
      self.postMessage({ ok: false, error: "served κ-blob failed κ verification" });
      return;
    }

    // The whole point: resume instead of boot.
    const t0 = performance.now();
    const ws = X64Workspace.resume_kappa(blob);
    const tResume = performance.now() - t0;
    const term0 = ws.terminal();
    self.postMessage({
      stage: "resumed",
      tResume,
      consoleBytes: term0.length,
      tail: term0.split("\n").slice(-6).join("\n"),
    });

    // Prove the resumed machine is LIVE: run a chunk; the console must not regress.
    const t1 = performance.now();
    for (let i = 0; i < 8; i++) ws.run(2_000_000);
    const tRun = performance.now() - t1;
    const term1 = ws.terminal();

    self.postMessage({
      ok: term1.length >= term0.length,
      blobBytes: blob.length,
      tResume,
      tRun,
      consoleBytes: term1.length,
      grew: term1.length - term0.length,
      tail: term1.split("\n").slice(-8).join("\n"),
    });
  } catch (e) {
    self.postMessage({ ok: false, error: String((e && e.stack) || e) });
  }
};
