// Node self-test for the substrate-native ext-host runtime core (CC-48 S1).
//
// Verifies — without the heavy browser run — that the CommonJS module host loads a
// Node-only extension, gives it a working Node built-in surface (path/events/fs)
// and the `vscode` passthrough, and runs `activate(context)` so its contribution
// reaches the (recording) vscode API. This is the load-bearing core the browser
// witness then drives end-to-end.
const assert = require("node:assert");
const { createNodeExtHost } = require("./node-exthost.js");

// A recording `vscode` API stand-in (the browser passes the real workbench API).
function recordingVscode() {
  const rec = { commands: [], statusBar: [], info: [] };
  return {
    rec,
    StatusBarAlignment: { Left: 1, Right: 2 },
    commands: {
      registerCommand(id, fn) { rec.commands.push(id); return { dispose() {} }; },
      executeCommand() { return Promise.resolve(); },
    },
    window: {
      createStatusBarItem() { const it = { text: "", show() { rec.statusBar.push(this.text); }, hide() {}, dispose() {} }; return it; },
      showInformationMessage(m) { rec.info.push(m); return Promise.resolve(); },
      createOutputChannel() { return { appendLine() {}, append() {}, show() {}, dispose() {} }; },
    },
    workspace: { getConfiguration: () => ({ get: () => undefined }), workspaceFolders: [] },
    Uri: { parse: (s) => ({ toString: () => s, path: s }), file: (s) => ({ path: s }) },
    Disposable: class { constructor(fn) { this._fn = fn; } dispose() { this._fn && this._fn(); } },
  };
}

// A tiny Node-only extension: it `require`s `vscode` AND Node built-ins (path,
// events, a relative module), registers a command, and contributes a status-bar
// item on activate — exactly the shape an Open VSX Node-only extension has.
const files = {
  "/extension/package.json": JSON.stringify({ name: "demo", main: "out/extension.js", engines: { vscode: "^1.75.0" } }),
  "/extension/out/extension.js": `
    const vscode = require("vscode");
    const path = require("node:path");
    const { EventEmitter } = require("events");
    const greet = require("./greet");
    function activate(context) {
      const bus = new EventEmitter();
      let got = "";
      bus.on("ping", (m) => { got = m; });
      bus.emit("ping", greet(path.basename("/a/b/world.txt")));
      context.subscriptions.push(vscode.commands.registerCommand("demo.hello", () => {}));
      const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
      item.text = "demo: " + got;
      item.show();
      context.subscriptions.push(item);
      return { ok: true, got };
    }
    function deactivate() {}
    module.exports = { activate, deactivate };
  `,
  "/extension/out/greet.js": `module.exports = (name) => "hello " + name;`,
};

(async () => {
  const vscode = recordingVscode();
  const host = createNodeExtHost({ vscode, files, extensionPath: "/extension" });
  const pkg = JSON.parse(files["/extension/package.json"]);
  const { api, context } = await host.activate(pkg);

  assert.strictEqual(api.ok, true, "activate() ran and returned its API");
  assert.strictEqual(api.got, "hello world.txt", "Node built-ins (path) + a relative require + events all work inside the extension");
  assert.deepStrictEqual(vscode.rec.commands, ["demo.hello"], "the extension registered its command through the vscode passthrough");
  assert.deepStrictEqual(vscode.rec.statusBar, ["demo: hello world.txt"], "the extension's status-bar contribution reached the workbench API");
  assert.strictEqual(context.subscriptions.length, 2, "activation pushed its disposables");

  // The Node built-in surface is itself sound (path/os).
  const { path, os } = require("./node-exthost.js");
  assert.strictEqual(path.join("/a", "b", "../c"), "/a/c", "path.join normalizes");
  assert.strictEqual(os.platform(), "linux", "os reports the holospace identity");

  // ── preload(): fetch the module graph from an async per-file API (Open VSX) ──
  // Simulate Open VSX's GET /file/{path}: serve package.json + main + a relative
  // dep on demand; assert the BFS pulls exactly the graph and activate() then runs.
  const remote = {
    "extension/package.json": JSON.stringify({ name: "demo2", main: "dist/main.js" }),
    "extension/dist/main.js": `const vscode=require("vscode");const u=require("./util");function activate(c){c.subscriptions.push(vscode.commands.registerCommand("demo2."+u.id(),()=>{}));return{ok:true};}module.exports={activate};`,
    "extension/dist/util.js": `module.exports={ id: () => "go" };`,
  };
  let fetches = 0;
  const fetcher = async (p) => { fetches++; return Object.prototype.hasOwnProperty.call(remote, p) ? remote[p] : null; };
  const v2 = recordingVscode();
  const host2 = createNodeExtHost({ vscode: v2, extensionPath: "/extension" });
  const pkg2 = JSON.parse(remote["extension/package.json"]);
  await host2.preload({ packageJson: pkg2, fetcher });
  const r2 = await host2.activate(pkg2);
  assert.strictEqual(r2.api.ok, true, "preload fetched main + its relative dep; activate() ran");
  assert.deepStrictEqual(v2.rec.commands, ["demo2.go"], "the fetched relative dep resolved (util.id() -> command id)");
  assert.ok(fetches >= 2, "preload fetched the module graph over the (async) per-file API");

  console.log("NODE-EXTHOST-CORE-TEST: PASS — the substrate-native ext host loads a Node-only extension (incl. async preload from a per-file API), provides the Node API surface + vscode passthrough, and runs activate()");
})().catch((e) => { console.error("NODE-EXTHOST-CORE-TEST: FAIL —", e && e.stack ? e.stack : e); process.exit(1); });
