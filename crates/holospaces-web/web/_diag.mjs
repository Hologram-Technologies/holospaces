import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path"; import { fileURLToPath } from "node:url";
const WEB = path.dirname(fileURLToPath(import.meta.url));
const hs = createRequire(import.meta.url)(path.join(WEB,"pkg-node","holospaces_web.js"));
const blob = new Uint8Array(await readFile(path.join(WEB,"fixtures","x64-alpine-shell.kblob")));
const ws = hs.X64Workspace.resume_kappa(blob);
console.log("resumed; console len:", ws.terminal().length);
// run a little WITHOUT input — is it stable (blocked) or does it halt/produce output?
let halted=false; for(let i=0;i<20;i++){ halted = ws.run(5_000_000); } 
console.log("after idle run: halted=", halted, "console len:", ws.terminal().length);
const tail0 = ws.terminal().split("\n").slice(-3);
console.log("idle tail:", JSON.stringify(tail0));
// feed input + run
ws.feed_input(new TextEncoder().encode("\n"));   // a bare Enter should re-print the prompt if alive
for(let i=0;i<60;i++) ws.run(5_000_000);
console.log("after Enter: console len:", ws.terminal().length, "tail:", JSON.stringify(ws.terminal().split("\n").slice(-3)));
ws.feed_input(new TextEncoder().encode("echo HOLO$((6*7))\n"));
for(let i=0;i<120;i++) ws.run(5_000_000);
const out = ws.terminal();
console.log("after 'echo HOLO$((6*7))': len:", out.length);
console.log("DELTA:", JSON.stringify(out.split("\n").slice(-6)));
console.log(out.includes("HOLO42") ? "PASS — INTERACTIVE: shell computed HOLO42" : "no HOLO42 — shell not responding to input");
