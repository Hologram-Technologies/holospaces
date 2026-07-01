// make-x64-console-fixture.mjs — regenerate the CC-64 instant-paint "header".
//
// The warm shell .holo (x64-alpine-shell.kblob) carries its console in the snapshot, ALREADY at the
// `holo$` prompt. CC-64 paints that text instantly (≈16 KiB) before streaming the 38 MiB machine, so
// the header MUST be a byte-exact prefix of the resumed terminal. This script derives it straight from
// the kblob — run it whenever the kblob is regenerated, so the header can never drift (the cc64 suite's
// G2b drift-guard fails loudly if it does). No emulator/Rust changes; pure fixture derivation.
//
// Run:  node make-x64-console-fixture.mjs [<kblob-path> <out-path> [require-suffix]]
//   default (no args): the warm Alpine shell, requiring the header to end at the `holo$` prompt.
//   server image:  node make-x64-console-fixture.mjs fixtures/x64-server-image.kblob fixtures/x64-server-image.console.txt HOLO-SERVED
import { X64Workspace } from "./pkg-node/holospaces_web.js";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const a = process.argv.slice(2);
const kblob = path.resolve(WEB, a[0] || "fixtures/x64-alpine-shell.kblob");
const out = path.resolve(WEB, a[1] || "fixtures/x64-alpine-shell.console.txt");
const requireSuffix = a.length >= 1 ? (a[2] || "") : "holo$"; // default-shell keeps the prompt check

const ws = X64Workspace.resume_kappa(new Uint8Array(readFileSync(kblob)));
const term = ws.terminal();
if (requireSuffix && !term.includes(requireSuffix)) {
  console.error(`refusing to write: resumed terminal does not contain ${JSON.stringify(requireSuffix)} (got ${term.length} B)`);
  process.exit(1);
}
writeFileSync(out, term);
console.log(`wrote ${out} (${Buffer.byteLength(term)} bytes) — instant-paint header in sync with ${path.basename(kblob)}`);
