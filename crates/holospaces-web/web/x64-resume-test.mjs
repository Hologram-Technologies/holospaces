// x64-resume-test.mjs — κ-snapshot Step 4 witness.
//
// Resume a host-sealed x86-64 machine snapshot inside the COMPILED wasm (the
// `wasm-pack --target nodejs` build of holospaces-web), with NO boot in the
// runtime. Proves the wasm-bindgen `X64Workspace.resume` path brings the machine
// back BIT-EXACT (the console comes back identical to the host's) and LIVE (it
// continues executing) — a running userspace from a snapshot in milliseconds,
// versus the multi-minute cold boot that produced it.
//
// Prereqs:
//   1) cargo test -p holospaces --test cc44_x64_boot generate_x64_resume_fixture -- --ignored
//      (writes web/fixtures/x64-resume-snapshot.bin + x64-resume-console.txt)
//   2) wasm-pack build crates/holospaces-web --target nodejs --out-dir web/pkg-node
// Run:  node crates/holospaces-web/web/x64-resume-test.mjs

import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

function fail(msg) {
  console.error("FAIL: " + msg);
  process.exit(1);
}

const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));

const snap = new Uint8Array(
  await readFile(path.join(WEB, "fixtures", "x64-resume-snapshot.bin")),
);
const expected = await readFile(
  path.join(WEB, "fixtures", "x64-resume-console.txt"),
  "utf8",
);

// Law L5 — a real deployment re-derives the snapshot's κ before trusting OPFS/wire bytes.
const k = hs.kappa(snap);
if (!hs.verify_kappa(snap, k)) fail("snapshot κ self-verify");

// The whole point: resume instead of boot.
const t0 = performance.now();
const ws = hs.X64Workspace.resume(snap);
const tResume = performance.now() - t0;

// Bit-exact: the resumed machine's console is byte-identical to the host's at snapshot time.
const term0 = ws.terminal();
if (term0 !== expected) {
  fail(
    `console NOT preserved bit-exact after resume (got ${term0.length} B, want ${expected.length} B)`,
  );
}

// Live: continue executing from the resumed instruction; the console must not regress.
const t1 = performance.now();
ws.run(8_000_000);
const tRun = performance.now() - t1;
const term1 = ws.terminal();
if (term1.length < term0.length) fail("console shrank after running the resumed machine");

console.log("PASS — x64 κ-snapshot RESUME witness (compiled wasm, nodejs target):");
console.log(`  flat snapshot : ${(snap.length / 1048576).toFixed(0)} MiB   κ=${k.slice(0, 24)}…`);
console.log(`  resume        : ${tResume.toFixed(1)} ms  → console BIT-EXACT (${term0.length} B preserved)`);
console.log(`  run 8M        : ${tRun.toFixed(0)} ms  → machine LIVE, console ${term1.length} B (+${term1.length - term0.length})`);

// Phase 1 — the CONTENT-ADDRESSED, deduplicated κ-blob: the browser persists THIS to OPFS, not
// the flat snapshot. Re-suspend the resumed machine as a κ-blob, then resume a fresh machine from
// it — bit-exact, from only the unique pages.
const blob = ws.suspend_kappa();
const t2 = performance.now();
const ws2 = hs.X64Workspace.resume_kappa(blob);
const tKappa = performance.now() - t2;
if (ws2.terminal() !== term0) fail("κ-blob resume did not reproduce the console bit-exact");
const reduction = (snap.length / blob.length).toFixed(1);
console.log(`  κ-blob        : ${(blob.length / 1048576).toFixed(1)} MiB  (${reduction}x smaller than flat — unique pages only)`);
console.log(`  κ-blob resume : ${tKappa.toFixed(1)} ms  → console BIT-EXACT, deduped`);
console.log("  → a running guest from a snapshot in milliseconds, persisting only the unique pages.");
process.exit(0);
