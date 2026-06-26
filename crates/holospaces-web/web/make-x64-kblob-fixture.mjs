// make-x64-kblob-fixture.mjs — derive the deduped κ-blob fixture from the flat snapshot fixture
// (no reboot): load the flat snapshot in the compiled wasm, X64Workspace.suspend_kappa(), and
// write the κ-blob + its κ for the in-tab resume witness (x64-resume-tab-test.mjs).
import { readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));

const flat = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-resume-snapshot.bin")));
const ws = hs.X64Workspace.resume(flat);
const blob = ws.suspend_kappa();
const k = hs.kappa(blob);
await writeFile(path.join(WEB, "fixtures", "x64-resume-snapshot.kblob"), blob);
await writeFile(path.join(WEB, "fixtures", "x64-resume-snapshot.kblob.kappa"), k);
console.log(`wrote κ-blob: ${(blob.length / 1048576).toFixed(1)} MiB (${blob.length} B), κ=${k.slice(0, 30)}…`);
