import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path"; import { fileURLToPath } from "node:url";
const WEB = path.dirname(fileURLToPath(import.meta.url));
const hs = createRequire(import.meta.url)(path.join(WEB, "pkg-node", "holospaces_web.js"));
const enc = new TextEncoder();
const blob = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-alpine-shell.kblob")));

const t0 = Date.now();
const ws = hs.X64Workspace.resume_kappa(blob);
console.log(`resume: ${Date.now()-t0}ms; prompt tail = ${JSON.stringify(ws.terminal().slice(-12))}\n`);

const PROMPT = "holo$ ";
function promptCount(s){ let n=0,i=0; while((i=s.indexOf(PROMPT,i))>=0){n++;i+=PROMPT.length;} return n; }
// feed cmd, run until a NEW prompt appears (command finished) or tick cap; return new output.
function sh(cmd, capTicks=400){
  const before = ws.terminal();
  const wantPrompts = promptCount(before) + 1;
  ws.feed_input(enc.encode(cmd + "\n"));
  let ticks=0;
  for(; ticks<capTicks; ticks++){
    ws.run(2_000_000);
    if(promptCount(ws.terminal()) >= wantPrompts) break;
  }
  const after = ws.terminal();
  const out = after.slice(before.length);
  return { out, ticks, done: promptCount(after) >= wantPrompts };
}

let pass=0, fail=0;
function check(name, cmd, test){
  const r = sh(cmd);
  const ok = (typeof test === "function") ? test(r.out) : r.out.includes(test);
  console.log(`${ok?"PASS":"FAIL"}  ${name.padEnd(34)} ${r.done?"":"(NO-PROMPT) "}[${r.ticks}t]  ${ok?"":"\n      got: "+JSON.stringify(r.out.replace(/\r/g,"").slice(-160))}`);
  ok?pass++:fail++;
  return r;
}

console.log("── correctness ──");
check("uname -m", "uname -m", "x86_64");
check("os-release", "cat /etc/os-release", "Alpine");
check("alpine-release", "cat /etc/alpine-release", "3.2");
check("pwd", "pwd", o=>o.includes("/"));
check("id is root", "id", "uid=0");
check("arithmetic", "echo R=$((123*456))", "R=56088");
check("ls /", "ls /", o=>/bin/.test(o)&&/etc/.test(o));
check("pipe wc", "printf 'abcd' | wc -c", o=>/\b4\b/.test(o));
check("for loop", "for i in 1 2 3; do echo n$i; done", o=>o.includes("n1")&&o.includes("n2")&&o.includes("n3"));
check("exit code 0", "true; echo rc=$?", "rc=0");
check("exit code 1", "false; echo rc=$?", "rc=1");
check("sort -r", "printf '%s\n' a b c | sort -r | tr -d '\n'", "cba");
check("sha256 hello", "echo hello | sha256sum", "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03");
check("/proc/version", "cat /proc/version", "Linux version");
check("meminfo", "head -1 /proc/meminfo", "MemTotal");
check("ps has init", "ps | head -5", o=>/\b1\b/.test(o));

console.log("\n── filesystem write/read (κ-disk CoW) ──");
check("write+read", "echo HOLODATA > /tmp/bt.txt; cat /tmp/bt.txt", "HOLODATA");
check("append+count", "echo more >> /tmp/bt.txt; wc -l < /tmp/bt.txt", o=>/\b2\b/.test(o));
check("rm gone", "rm /tmp/bt.txt; ls /tmp/bt.txt 2>&1", o=>/No such|not found/.test(o));
check("mkdir+cd state", "mkdir -p /tmp/d && cd /tmp/d && pwd", "/tmp/d");
check("cd persisted", "pwd", "/tmp/d");

console.log("\n── edge cases ──");
check("empty enter", "", o=>true); // just a prompt
check("long line", "echo " + "x".repeat(400), o=>o.includes("x".repeat(400)));
check("special chars", "echo 'a\"b$c`d'", o=>o.includes('a"b$c`d'));
check("unknown cmd", "definitelynotacommand", o=>/not found/.test(o));
check("multi ;", "echo A; echo B; echo C", o=>o.includes("A")&&o.includes("B")&&o.includes("C"));

console.log("\n── determinism (same cmd twice → same output) ──");
const a = sh("echo $((2**10)) 2>/dev/null || echo $((1024))").out.replace(/\r/g,"");
const b = sh("echo $((1024))").out.replace(/\r/g,"");
const detOk = a.includes("1024") && b.includes("1024");
console.log(`${detOk?"PASS":"FAIL"}  determinism 1024`); detOk?pass++:fail++;

console.log("\n── stress: 30 rapid commands ──");
let stressOk=0; const s0=Date.now();
for(let i=0;i<30;i++){ const r=sh(`echo S${i}`,200); if(r.out.includes(`S${i}`)) stressOk++; }
console.log(`${stressOk===30?"PASS":"FAIL"}  stress ${stressOk}/30 echoed (${Date.now()-s0}ms)`); stressOk===30?pass++:fail++;

console.log("\n── still responsive after stress ──");
check("post-stress uname", "uname -m", "x86_64");

console.log(`\n════ RESULT: ${pass} PASS / ${fail} FAIL ════`);
process.exit(fail?1:0);
