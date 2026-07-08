// holospace-tasks — run tasks.json tasks in the devcontainer (CC-53).
//
// Registers a TaskProvider (`tasks.registerTaskProvider`, stable API) whose tasks
// are `CustomExecution`s. Each task's `Pseudoterminal` runs the command IN THE
// GUEST devcontainer over a file-exec channel on the holospace's OWN virtio-9p
// workspace (CC-15): it writes `.hs-tasks/<id>.cmd`, and the guest task-runner
// agent (a /bin/sh loop seeded into the devcontainer /init, CC-11) runs it and
// streams stdout/stderr → `<id>.out` and the exit code → `<id>.exit`, which the
// pty streams to the task terminal and reports as the task's exit code. VS Code
// feeds the pty output to the same problem collector as a shell task, so a task's
// problem matchers populate the Problems panel; background/watch tasks keep the
// pty open with the spinner. No server outside the holospace (Law L4).
//
// The web workbench DISABLES shell/process task execution in a virtual workspace,
// so this CustomExecution provider is the ONLY way tasks.json runs here.
"use strict";
const vscode = require("vscode");

const SCHEME = "holospace";
const TASK_TYPE = "holospace";

let core = null;
let out = null;
const enc = new TextEncoder();
const dec = new TextDecoder();

function log(msg) {
  console.log("[CC53] " + msg);
  if (out) out.appendLine("holospace-tasks: " + msg);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function loadCore(extBase) {
  if (core) return;
  const res = await fetch(`${extBase}/tasks-core.cjs`);
  if (!res.ok) throw new Error(`fetch tasks-core.cjs: ${res.status}`);
  const src = dec.decode(new Uint8Array(await res.arrayBuffer()));
  const module = { exports: {} };
  new Function("module", "exports", src)(module, module.exports);
  core = module.exports;
}

function holospaceFolder() {
  return (vscode.workspace.workspaceFolders || []).find((f) => f.uri.scheme === SCHEME);
}

async function readFirst(folderUri, rels) {
  for (const rel of rels) {
    try {
      const bytes = await vscode.workspace.fs.readFile(vscode.Uri.joinPath(folderUri, ...rel.split("/")));
      return dec.decode(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
    } catch { /* not there — try next */ }
  }
  return null;
}
const readTasksJson = (folderUri) => readFirst(folderUri, [".vscode/tasks.json", "tasks.json"]);
// The devcontainer spec's config locations, most specific first.
const readDevcontainerJson = (folderUri) =>
  readFirst(folderUri, [".devcontainer/devcontainer.json", ".devcontainer.json", "devcontainer.json"]);

// Map a tasks.json `problemMatcher` to the string names the Task API accepts. A
// named matcher ("$tsc", "$holospace-generic") passes through; an inline-object
// matcher cannot be expressed via the API, so we degrade honestly to none (use a
// contributed/named matcher instead).
function resolveMatchers(pm) {
  if (!pm) return [];
  if (typeof pm === "string") return [pm];
  if (Array.isArray(pm)) return pm.filter((x) => typeof x === "string");
  return [];
}

function groupOf(g) {
  if (!g) return undefined;
  if (g.kind === "build") return g.isDefault ? { ...vscode.TaskGroup.Build, isDefault: true } : vscode.TaskGroup.Build;
  if (g.kind === "test") return vscode.TaskGroup.Test;
  return undefined;
}

// The Pseudoterminal that runs ONE task in the guest over the 9p file channel.
class GuestPty {
  constructor(command, label, folderUri) {
    this.command = command;
    this.label = label;
    this.folderUri = folderUri;
    this._write = new vscode.EventEmitter();
    this._close = new vscode.EventEmitter();
    this.onDidWrite = this._write.event;
    this.onDidClose = this._close.event;
    this._closed = false;
  }
  open() {
    this._run().catch((e) => {
      this._write.fire("holospace-tasks: error — " + (e && e.message) + "\r\n");
      this._close.fire(1);
    });
  }
  close() { this._closed = true; }

  async _run() {
    const folder = this.folderUri;
    const uri = (rel) => vscode.Uri.joinPath(folder, ...rel.split("/"));
    const readOrNull = async (u) => {
      try { const b = await vscode.workspace.fs.readFile(u); return b instanceof Uint8Array ? b : new Uint8Array(b); }
      catch { return null; }
    };
    const del = async (u) => { try { await vscode.workspace.fs.delete(u); } catch { /* best effort */ } };

    const id = core.newTaskId();
    const cmdU = uri(core.cmdPath(id)), outU = uri(core.outPath(id)), exitU = uri(core.exitPath(id));
    const killU = uri(`${core.TASKS_DIR}/${id}.kill`);
    try { await vscode.workspace.fs.createDirectory(uri(core.TASKS_DIR)); } catch { /* the agent also mkdirs it */ }

    this._write.fire(`\x1b[2m$ ${this.command}\x1b[0m\r\n`);
    // Submit the request: the guest agent claims `<id>.cmd` and runs it.
    await vscode.workspace.fs.writeFile(cmdU, enc.encode(this.command + "\n"));
    log(`task "${this.label}" submitted to the guest (${id})`);

    let outLen = 0;
    const emitNew = async () => {
      const b = await readOrNull(outU);
      if (b && b.length > outLen) {
        if (outLen === 0) log(`HOLOSPACE-TASK-OUT label=${this.label} (first output from the guest)`);
        this._write.fire(dec.decode(b.subarray(outLen)).replace(/\r?\n/g, "\r\n"));
        outLen = b.length;
      }
    };

    // Poll for streamed output + the exit sentinel (up to ~10 min of activity; a
    // watch task the user stops is KILLED for real: close() → the `<id>.kill`
    // sentinel → the guest agent kills the task's process → it writes `<id>.exit`
    // with the wait status — so terminating a task genuinely stops the guest
    // process, not just the terminal.
    let killSentAt = -1;
    for (let i = 0; i < 1200; i++) {
      if (this._closed && killSentAt < 0) {
        killSentAt = i;
        try { await vscode.workspace.fs.writeFile(killU, enc.encode("1\n")); } catch { /* best effort */ }
        log(`task "${this.label}" terminate requested — kill sentinel written (${id})`);
      }
      await emitNew();
      const exitBytes = await readOrNull(exitU);
      const code = exitBytes ? core.parseExit(dec.decode(exitBytes)) : null;
      if (code != null) {
        await emitNew(); // final flush
        this._write.fire(`\r\n\x1b[2m[task '${this.label}' exited with code ${code}]\x1b[0m\r\n`);
        log(`HOLOSPACE-TASK-EXIT label=${this.label} code=${code}`);
        await del(outU); await del(exitU);
        this._close.fire(code);
        return;
      }
      // The terminal is gone; give the agent ~20s to honour the kill, then stop.
      if (killSentAt >= 0 && i - killSentAt > 50) break;
      await sleep(400);
    }
    // Timed out (or terminated without an exit report) — clean up the channel.
    await del(cmdU); await del(outU); await del(exitU); await del(killU);
    this._close.fire(this._closed ? undefined : 0);
  }
}

function makeTask(t, folderUri) {
  const definition = { type: TASK_TYPE, command: t.command, args: t.args || [], label: t.label };
  const command = core.buildCommand(t);
  const exec = new vscode.CustomExecution(async () => new GuestPty(command, t.label, folderUri));
  const task = new vscode.Task(
    definition,
    vscode.TaskScope.Workspace,
    t.label,
    "holospace",
    exec,
    resolveMatchers(t.problemMatcher),
  );
  const g = groupOf(t.group);
  if (g) task.group = g;
  if (t.detail) task.detail = t.detail;
  if (t.isBackground) task.isBackground = true;
  task.presentationOptions = { reveal: vscode.TaskRevealKind.Always, panel: vscode.TaskPanelKind.Dedicated };
  return task;
}

async function buildTasks() {
  const folder = holospaceFolder();
  if (!folder) return [];
  const specs = [];
  const text = await readTasksJson(folder.uri);
  if (text) {
    try { specs.push(...core.parseTasksJson(text)); }
    catch (e) { log("tasks.json parse error: " + (e && e.message)); }
  }
  // Surface the devcontainer's lifecycle commands (CC-22) as re-runnable tasks —
  // the same commands the container ran on create/start, one "Run Task" away.
  const dc = await readDevcontainerJson(folder.uri);
  if (dc) {
    try { specs.push(...core.parseDevcontainerLifecycle(dc)); }
    catch (e) { log("devcontainer.json parse error: " + (e && e.message)); }
  }
  return specs.map((t) => makeTask(t, folder.uri));
}

function activate(context) {
  out = vscode.window.createOutputChannel("Holospace Tasks");
  context.subscriptions.push(out);
  const extBase = context.extensionUri.toString().replace(/\/+$/, "");

  const marker = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 30);
  context.subscriptions.push(marker);

  const provider = {
    async provideTasks() {
      log("provideTasks called");
      await loadCore(extBase);
      const folder = holospaceFolder();
      log("provideTasks folder=" + (folder ? folder.uri.toString() : "NONE"));
      const tasks = await buildTasks();
      log(`provideTasks → ${tasks.length} task(s): ${tasks.map((t) => t.name).join(", ")}`);
      return tasks;
    },
    async resolveTask(task) {
      // A tasks.json `"type":"holospace"` entry VS Code hands back for execution:
      // attach a CustomExecution that runs its command in the guest.
      await loadCore(extBase);
      const folder = holospaceFolder();
      if (!folder || !task.definition || task.definition.type !== TASK_TYPE || !task.definition.command) return undefined;
      const t = {
        label: task.name || task.definition.label || task.definition.command,
        command: task.definition.command,
        args: task.definition.args || [],
        problemMatcher: undefined,
        isBackground: task.isBackground,
      };
      const command = core.buildCommand(t);
      const resolved = new vscode.Task(
        task.definition,
        task.scope || vscode.TaskScope.Workspace,
        t.label,
        "holospace",
        new vscode.CustomExecution(async () => new GuestPty(command, t.label, folder.uri)),
        task.problemMatchers,
      );
      resolved.detail = task.detail;
      return resolved;
    },
  };

  // Convenience commands that run the configured tasks through the real task
  // system (`executeTask` → the CustomExecution → the guest), so the build/run
  // tasks are one palette command away. (The built-in "Tasks: Run (Build) Task"
  // UI works too; these are the named shortcuts.)
  const runByName = async (pick) => {
    await loadCore(extBase);
    const tasks = await buildTasks();
    if (!tasks.length) { vscode.window.showWarningMessage("holospace: no tasks in .vscode/tasks.json"); return; }
    const task = pick(tasks) || tasks[0];
    log(`executing task "${task.name}" via the task system`);
    await vscode.tasks.executeTask(task);
  };
  context.subscriptions.push(
    vscode.commands.registerCommand("holospaceTasks.runBuild", () =>
      runByName((ts) => ts.find((t) => t.group === vscode.TaskGroup.Build) || ts.find((t) => t.name === "build")),
    ),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand("holospaceTasks.run", async () => {
      await loadCore(extBase);
      const tasks = await buildTasks();
      const pick = await vscode.window.showQuickPick(tasks.map((t) => t.name), { placeHolder: "Select a task to run" });
      const task = tasks.find((t) => t.name === pick);
      if (task) { log(`executing task "${task.name}" via the task system`); await vscode.tasks.executeTask(task); }
    }),
  );

  (async () => {
    await loadCore(extBase);
    context.subscriptions.push(vscode.tasks.registerTaskProvider(TASK_TYPE, provider));
    marker.text = "$(checklist) HOLOSPACE-TASKS-LIVE";
    marker.tooltip = "tasks.json tasks run in the devcontainer (CC-53)";
    marker.show();
    context.subscriptions.push(marker);
    log("TaskProvider registered for type 'holospace' (tasks.json runs in the devcontainer)");
  })().catch((e) => log("startup error — " + (e && e.message)));
}

function deactivate() {}

module.exports = { activate, deactivate };
