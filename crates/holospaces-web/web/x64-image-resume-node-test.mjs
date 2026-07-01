// CC-65 G5 (headless-wasm witness) — a real SERVER IMAGE resumes in WASM and KEEPS SERVING.
//
// Loads the warm server-image .holo into the SAME wasm core the browser runs (pkg-test, built from the
// current Rust core) and proves, off any GPU/DOM:
//   • the instant-paint header (fixtures/x64-server-image.console.txt) is a byte-exact PREFIX of the
//     resumed terminal (the paint is real, not a splash),
//   • after resuming, the server KEEPS SERVING — HOLO-SERVED-N grows beyond the snapshot value, i.e. the
//     server process + its loopback TCP sockets are alive in wasm.
// The browser adds instant-paint timing on top (image-stream.html); this is the deterministic core gate.
import { X64Workspace } from "./pkg-test/holospaces_web.js";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const F = (p) => path.join(WEB, p);
const maxServed = (s) => (s.match(/HOLO-SERVED-(\d+)/g) || []).map((x) => +x.split("-")[2]).reduce((a, b) => Math.max(a, b), 0);
const fail = (m) => { console.log("CC-65 G5 FAIL: " + m); process.exit(1); };

const kblob = new Uint8Array(readFileSync(F("fixtures/x64-server-image.kblob")));
const header = readFileSync(F("fixtures/x64-server-image.console.txt"), "utf8");

const ws = X64Workspace.resume_kappa(kblob);
const term0 = ws.terminal();
const servedAtResume = maxServed(term0);

// Paint-is-real: the header is a byte-prefix of the resumed terminal.
const trim = (s) => s.replace(/\s+$/, "");
if (!(trim(term0).startsWith(trim(header)) && trim(header).length > 100)) {
  fail(`header is not a prefix of the resumed terminal (header ${header.length}B, term0 ${term0.length}B)`);
}
console.log(`CC-65 G5 paint-is-real: header ${header.length}B is a prefix of the resumed terminal → PASS  [served@resume=${servedAtResume}]`);

// Keeps serving: run the resumed machine; HOLO-SERVED must grow.
let after = servedAtResume;
for (let i = 0; i < 200 && after <= servedAtResume; i++) {
  ws.run(4_000_000);
  after = maxServed(ws.terminal());
}
if (after <= servedAtResume) fail(`resumed server stopped serving (HOLO-SERVED stuck at ${servedAtResume})`);
console.log(`CC-65 G5 keeps-serving-in-wasm: HOLO-SERVED ${servedAtResume} → ${after} → PASS`);

console.log(`\nCC-65 G5 VERDICT: PASS — a server image resumes in the browser's wasm core and keeps serving (${servedAtResume}→${after}).`);
process.exit(0);
