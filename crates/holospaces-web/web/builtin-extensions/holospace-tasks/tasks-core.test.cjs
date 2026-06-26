// CC-53 (fast core witness) — the tasks engine's JSONC parse, command build, and
// file-exec protocol, verified deterministically under Node (mirrors
// holospace-search/search-core.test.cjs). The browser witness (tasks-test.mjs)
// then proves it wired into the real workbench against a guest.
"use strict";
const core = require("./tasks-core.cjs");

let pass = 0;
const ok = (c, m) => { if (!c) throw new Error("FAIL: " + m); pass++; console.log("  ✓", m); };

// ── parseJsonc (comments + trailing commas, string-aware) ────────────────────
{
  const j = core.parseJsonc('{\n  // a line comment\n  "a": "x", /* block */ "b": [1, 2,],\n  "c": "has // not a comment and /* nope */",\n}');
  ok(j.a === "x" && j.b.length === 2 && j.b[1] === 2, "JSONC strips comments + trailing commas");
  ok(j.c === "has // not a comment and /* nope */", "JSONC does NOT strip // or /* */ inside a string");
}

// ── parseTasksJson (normalize, drop command-less, group/background) ──────────
{
  const text = `{
    "version": "2.0.0",
    "tasks": [
      // the default build task
      { "label": "build", "type": "shell", "command": "make", "args": ["-j", "4"], "group": { "kind": "build", "isDefault": true } },
      { "label": "test", "type": "holospace", "command": "echo hi", "options": { "cwd": "sub dir", "env": { "FOO": "bar baz" } } },
      { "label": "watch", "type": "shell", "command": "tsc -w", "isBackground": true },
      { "label": "no-command", "type": "shell" }
    ]
  }`;
  const tasks = core.parseTasksJson(text);
  ok(tasks.length === 3, "command-less task is dropped (3 of 4 kept)");
  const build = tasks.find((t) => t.label === "build");
  ok(build.command === "make" && build.args.length === 2, "command + args parsed");
  ok(build.group && build.group.kind === "build" && build.group.isDefault === true, "group {build,isDefault} parsed");
  ok(tasks.find((t) => t.label === "watch").isBackground === true, "isBackground parsed");
  ok(tasks.find((t) => t.label === "test").cwd === "sub dir", "options.cwd parsed");
}

// ── buildCommand (quoting, cwd, env) ────────────────────────────────────────
{
  ok(core.shQuote("a b") === "'a b'", "shQuote wraps spaces");
  ok(core.shQuote("it's") === "'it'\\''s'", "shQuote escapes an embedded single quote");

  const cmd = core.buildCommand({ command: "make", args: ["-j", "4 cores"], cwd: "my dir", env: { FOO: "a b" } });
  ok(cmd === "cd 'my dir' 2>/dev/null; export FOO='a b'; make '-j' '4 cores'", "buildCommand composes cd + export + command + quoted args");

  const snippet = core.buildCommand({ command: "echo hi && exit 3", args: [] });
  ok(snippet === "echo hi && exit 3", "a shell-snippet command passes through unquoted (with no args)");
}

// ── file-exec protocol: paths + exit parse ──────────────────────────────────
{
  const id = "tABC";
  ok(core.cmdPath(id) === ".hs-tasks/tABC.cmd", "cmd path");
  ok(core.outPath(id) === ".hs-tasks/tABC.out", "out path");
  ok(core.exitPath(id) === ".hs-tasks/tABC.exit", "exit path");
  ok(core.parseExit("3\n") === 3 && core.parseExit("  0  ") === 0 && core.parseExit("-1") === -1, "parseExit reads the agent's echo $?");
  ok(core.parseExit("") === null && core.parseExit(null) === null, "parseExit is null until the code is written");
  ok(core.newTaskId() !== core.newTaskId(), "newTaskId is unique");
}

// ── end-to-end protocol against an in-memory FS + a simulated guest agent ────
{
  // The host writes <id>.cmd; a simulated agent runs it and writes <id>.out +
  // <id>.exit — exactly the contract the real guest /init agent honours.
  const fs = new Map();
  const id = "trun";
  // host: request
  fs.set(core.cmdPath(id), core.buildCommand({ command: "echo line1; echo line2; exit 7", args: [] }));
  // agent: run the command (simulated) → stream output, then exit code
  const script = fs.get(core.cmdPath(id));
  ok(/echo line1; echo line2; exit 7/.test(script), "the agent receives the built shell command");
  fs.set(core.outPath(id), "line1\nline2\n");
  fs.set(core.exitPath(id), "7\n");
  // host: read back
  ok(fs.get(core.outPath(id)) === "line1\nline2\n", "host reads the streamed output");
  ok(core.parseExit(fs.get(core.exitPath(id))) === 7, "host reads the non-zero exit status");
}

console.log(`TASKS-CORE-TEST: PASS (${pass} checks)`);
process.exit(0);
