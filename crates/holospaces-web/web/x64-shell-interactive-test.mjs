// x64-shell-interactive-test.mjs — HEADLESS interactive witness through the REAL wasm (the exact
// emulator code the browser loader runs). Resume the warm Alpine shell κ-blob, type a sequence of
// commands via feed_input (ttyS0 RX), tick run() like the render loop, and assert the shell EXECUTES
// each — builtins AND external fork+exec commands — proving the κ-disk now survives resume.
// Run: node x64-shell-interactive-test.mjs
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));

const blob = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-alpine-shell.kblob")));
const ws = hs.X64Workspace.resume_kappa(blob);
console.log("resumed; initial console tail:", JSON.stringify(ws.terminal().slice(-40)));

const enc = new TextEncoder();
const cmds = [
  ["echo HOLO$((6*7))\n", "HOLO42"],
  ["uname -m\n", "x86_64"],
  ["cat /etc/alpine-release\n", "3.20"],
  ["echo $((100+23))\n", "123"],
  ["pwd\n", "/"],
];

let allOk = true;
for (const [line, want] of cmds) {
  const before = ws.terminal().length;
  ws.feed_input(enc.encode(line));
  let ok = false;
  let ticks = 0;
  for (let i = 0; i < 80; i++) {
    ws.run(2_000_000);
    ticks++;
    const out = ws.terminal().slice(before);
    if (out.includes(want)) {
      ok = true;
      break;
    }
  }
  allOk &&= ok;
  console.log(`  ${line.trim().padEnd(24)} -> ${want.padEnd(8)} [${ok ? "OK" : "MISS"}] (${ticks} ticks)`);
}

console.log("\nFULL session console:\n" + ws.terminal().split("\n").slice(-12).join("\n"));
if (!allOk) {
  console.error("\nFAIL: a command did not execute through the wasm.");
  process.exit(1);
}
console.log("\nPASS: resumed Alpine shell executes builtins AND external commands through the wasm (κ-disk survives resume).");
