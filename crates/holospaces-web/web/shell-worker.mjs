// shell-worker.mjs — resume the warm Alpine shell κ-snapshot and run it interactively.
// No boot: X64Workspace.resume_kappa reconstructs a *running* Alpine machine blocked at its
// shell prompt; the render loop ticks run() and streams the console, and keystrokes from the
// page are fed to the guest serial (ttyS0) via feed_input. "Open a .holo → type into it."
import init, { X64Workspace } from "./pkg/holospaces_web.js";
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const enc = new TextEncoder();
let ws = null;
let lastLen = -1;

async function main() {
  await init();
  self.postMessage({ stage: "fetching warm .holo (≈33 MiB)" });
  const blob = await bytes("./fixtures/x64-alpine-shell.kblob");
  self.postMessage({ stage: "resuming machine" });
  const t0 = performance.now();
  ws = X64Workspace.resume_kappa(blob);
  self.postMessage({ stage: "ready", resumeMs: Math.round(performance.now() - t0) });

  const tick = () => {
    ws.run(2_000_000); // ~2M guest insns per frame
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

main().catch((err) => self.postMessage({ error: String(err && err.stack || err) }));
