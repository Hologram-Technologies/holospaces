// holospace-tasks — the tasks engine core (CC-53).
//
// Pure, browser-safe, dependency-free: the `tasks.json` (JSONC) parse, the shell
// command a task runs, and the file-exec protocol the guest agent honours. Used
// both in the web extension host (fetch + evaluate) and under Node (the unit
// witness), so the load-bearing logic is proven without the heavy browser run.
"use strict";

// ── JSONC → JSON ──────────────────────────────────────────────────────────────
// `tasks.json` permits `//` and `/* */` comments and trailing commas. Strip them
// with a string-aware scanner (never touching bytes inside a JSON string), then
// JSON.parse. Throws on genuinely invalid JSON.
function parseJsonc(text) {
  let out = "";
  let i = 0;
  const n = text.length;
  let inStr = false;
  while (i < n) {
    const c = text[i];
    if (inStr) {
      out += c;
      if (c === "\\") { out += text[i + 1] || ""; i += 2; continue; }
      if (c === '"') inStr = false;
      i++;
      continue;
    }
    if (c === '"') { inStr = true; out += c; i++; continue; }
    if (c === "/" && text[i + 1] === "/") { while (i < n && text[i] !== "\n") i++; continue; }
    if (c === "/" && text[i + 1] === "*") { i += 2; while (i < n && !(text[i] === "*" && text[i + 1] === "/")) i++; i += 2; continue; }
    out += c;
    i++;
  }
  // Trailing commas before } or ] (now that comments/strings are handled).
  out = out.replace(/,(\s*[}\]])/g, "$1");
  return JSON.parse(out);
}

// Normalize a tasks.json document into the task list we run. Each entry keeps the
// fields the provider needs; unknown fields are ignored. A task with no command
// is dropped (nothing to run). `version` is not required (we run any 2.0.0-shaped
// document, and tolerate a bare `{ tasks: [...] }`).
function parseTasksJson(text) {
  const doc = parseJsonc(text);
  const tasks = Array.isArray(doc && doc.tasks) ? doc.tasks : [];
  return tasks
    .map((t) => normalizeTask(t))
    .filter((t) => t && t.command);
}

function normalizeTask(t) {
  if (!t || typeof t !== "object") return null;
  const label = t.label || t.taskName || (typeof t.command === "string" ? t.command : "task");
  const group =
    typeof t.group === "string"
      ? { kind: t.group, isDefault: false }
      : t.group && typeof t.group === "object"
        ? { kind: t.group.kind, isDefault: !!t.group.isDefault }
        : null;
  return {
    label: String(label),
    type: t.type || "holospace",
    command: t.command,
    args: Array.isArray(t.args) ? t.args : [],
    cwd: (t.options && t.options.cwd) || undefined,
    env: (t.options && t.options.env) || undefined,
    isBackground: !!t.isBackground,
    group,
    // The problemMatcher may be a name ("$tsc"), an array of names, or an inline
    // object; the provider maps names to the Task's matchers and contributes a
    // generic one for inline patterns.
    problemMatcher: t.problemMatcher,
    detail: t.detail,
  };
}

// ── Devcontainer lifecycle commands as tasks (CC-22 surfacing) ────────────────
// The devcontainer spec's lifecycle commands, in lifecycle order. Each may be a
// string (a shell command), an array (an argv vector), or an object of *named*
// commands (each string/array, run in parallel) — all three forms surface.
// `initializeCommand` is omitted deliberately: the spec runs it on the HOST
// before the container exists, so it is not a guest task (ADR-015: the boundary
// stays explicit rather than silently running it in the wrong place).
const LIFECYCLE_PROPS = [
  "onCreateCommand",
  "updateContentCommand",
  "postCreateCommand",
  "postStartCommand",
  "postAttachCommand",
];

// One lifecycle command value (string | argv array) → {command, args} | null.
function lifecycleCommand(v) {
  if (typeof v === "string" && v.trim()) return { command: v, args: [] };
  if (Array.isArray(v) && v.length && v.every((x) => typeof x === "string")) {
    return { command: v[0], args: v.slice(1) };
  }
  return null;
}

// Parse a devcontainer.json (JSONC) into the lifecycle tasks it declares — the
// CC-22 lifecycle commands, surfaced as re-runnable tasks (labelled
// `lifecycle: postCreateCommand`, `lifecycle: postCreateCommand (name)`, …) so
// the environment-preparing commands the container ran on create are one "Run
// Task" away. Parametric over ANY devcontainer.json; [] when none are declared.
function parseDevcontainerLifecycle(text) {
  const doc = parseJsonc(text);
  const out = [];
  for (const prop of LIFECYCLE_PROPS) {
    const v = doc && doc[prop];
    if (v == null) continue;
    if (typeof v === "object" && !Array.isArray(v)) {
      for (const [name, sub] of Object.entries(v)) {
        const c = lifecycleCommand(sub);
        if (c) out.push({ label: `lifecycle: ${prop} (${name})`, type: "holospace", command: c.command, args: c.args, isBackground: false, group: null, problemMatcher: undefined, detail: `devcontainer.json ${prop}.${name}`, lifecycle: true });
      }
    } else {
      const c = lifecycleCommand(v);
      if (c) out.push({ label: `lifecycle: ${prop}`, type: "holospace", command: c.command, args: c.args, isBackground: false, group: null, problemMatcher: undefined, detail: `devcontainer.json ${prop}`, lifecycle: true });
    }
  }
  return out;
}

// Quote a single shell argument for POSIX sh (single-quote, escaping embedded
// single quotes) — so args with spaces / metacharacters pass through verbatim.
function shQuote(s) {
  return "'" + String(s).replace(/'/g, "'\\''") + "'";
}

// The shell command a task runs in the guest: the `command` with its `args`
// appended (each quoted), prefixed by an optional `cd <cwd>` and `export`s. The
// `command` string itself is passed through unquoted (it may be a shell snippet,
// the tasks.json convention for a `shell` task), with quoted args after it.
function buildCommand(task) {
  let cmd = "";
  if (task.cwd) cmd += "cd " + shQuote(task.cwd) + " 2>/dev/null; ";
  if (task.env && typeof task.env === "object") {
    for (const [k, v] of Object.entries(task.env)) {
      if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(k)) cmd += "export " + k + "=" + shQuote(v) + "; ";
    }
  }
  cmd += String(task.command);
  for (const a of task.args || []) cmd += " " + shQuote(a);
  return cmd;
}

// ── The file-exec protocol over the 9p workspace ──────────────────────────────
// The host writes `.hs-tasks/<id>.cmd` (the shell command); the guest agent runs
// it (stdout+stderr → `<id>.out`, exit code → `<id>.exit`). These helpers name
// the files and parse the exit sentinel, shared by the Pseudoterminal and the
// unit witness (which drives the protocol against an in-memory FS).
const TASKS_DIR = ".hs-tasks";
function newTaskId() {
  // Browser/Node-safe unique id; the unit witness injects a deterministic one.
  const rnd = Math.floor(Math.random() * 1e9).toString(36);
  return "t" + Date.now().toString(36) + rnd;
}
const cmdPath = (id) => `${TASKS_DIR}/${id}.cmd`;
const outPath = (id) => `${TASKS_DIR}/${id}.out`;
const exitPath = (id) => `${TASKS_DIR}/${id}.exit`;

// Parse the `<id>.exit` contents into an integer exit code (the agent writes
// `echo $?`), tolerant of trailing whitespace; null if not a number yet.
function parseExit(text) {
  if (text == null) return null;
  const m = /^\s*(-?\d+)\s*$/.exec(String(text));
  return m ? parseInt(m[1], 10) : null;
}

const api = {
  parseJsonc, parseTasksJson, normalizeTask, shQuote, buildCommand,
  LIFECYCLE_PROPS, parseDevcontainerLifecycle,
  TASKS_DIR, newTaskId, cmdPath, outPath, exitPath, parseExit,
};
if (typeof module !== "undefined" && module.exports) module.exports = api;
