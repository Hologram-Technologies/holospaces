// Node self-test for the substrate-native ext-host runtime core (CC-48 S1).
//
// Verifies — without the heavy browser run — that the CommonJS module host loads a
// Node-only extension, gives it a working Node built-in surface (path/events/fs)
// and the `vscode` passthrough, and runs `activate(context)` so its contribution
// reaches the (recording) vscode API. This is the load-bearing core the browser
// witness then drives end-to-end.
const assert = require("node:assert");
const { createNodeExtHost, installFromOpenVsx, unzipVsix } = require("./node-exthost.js");

// Build a minimal STORED (uncompressed) .vsix/zip from a {path: string} map — so
// the install self-test exercises the real download+unzip path offline, without a
// deflate encoder. `unzipVsix` copies stored entries verbatim (it does not check
// CRC), so CRC fields are left 0.
function makeStoredZip(files) {
  const enc = new TextEncoder();
  const locals = [];
  const central = [];
  let offset = 0;
  for (const [name, content] of Object.entries(files)) {
    const nameB = enc.encode(name);
    const data = typeof content === "string" ? enc.encode(content) : content;
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0); lh.writeUInt16LE(20, 4); lh.writeUInt16LE(0, 6);
    lh.writeUInt16LE(0, 8); /*method=stored*/ lh.writeUInt32LE(0, 14); /*crc*/
    lh.writeUInt32LE(data.length, 18); lh.writeUInt32LE(data.length, 22);
    lh.writeUInt16LE(nameB.length, 26); lh.writeUInt16LE(0, 28);
    const local = Buffer.concat([lh, Buffer.from(nameB), Buffer.from(data)]);
    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0); ch.writeUInt16LE(20, 4); ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(0, 8); ch.writeUInt16LE(0, 10); /*method=stored*/
    ch.writeUInt32LE(0, 16); /*crc*/ ch.writeUInt32LE(data.length, 20); ch.writeUInt32LE(data.length, 24);
    ch.writeUInt16LE(nameB.length, 28); ch.writeUInt32LE(offset, 42);
    central.push(Buffer.concat([ch, Buffer.from(nameB)]));
    locals.push(local);
    offset += local.length;
  }
  const localBuf = Buffer.concat(locals);
  const centralBuf = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(central.length, 8); eocd.writeUInt16LE(central.length, 10);
  eocd.writeUInt32LE(centralBuf.length, 12); eocd.writeUInt32LE(localBuf.length, 16);
  return new Uint8Array(Buffer.concat([localBuf, centralBuf, eocd]));
}

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
  const { path, os, makeFs } = require("./node-exthost.js");
  assert.strictEqual(path.join("/a", "b", "../c"), "/a/c", "path.join normalizes");
  assert.strictEqual(os.platform(), "linux", "os reports the holospace identity");
  // path.parse/format — real extensions walk directory chains with
  // `path.parse(p).root` (editorconfig); node-exact on the cases that matter.
  assert.deepStrictEqual(
    path.parse("/a/b/c.txt"),
    { root: "/", dir: "/a/b", base: "c.txt", ext: ".txt", name: "c" },
    "path.parse matches Node for an absolute file",
  );
  assert.strictEqual(path.parse("/x").root, "/", "path.parse exposes the root (the ancestor-walk terminator)");
  assert.strictEqual(path.parse("rel/f.js").root, "", "path.parse of a relative path has no root");
  assert.strictEqual(path.format({ dir: "/a", base: "b.txt" }), "/a/b.txt", "path.format round-trips");

  // fs exposes Node's CALLBACK API over the adapter (editorconfig calls
  // fs.readFile(p, enc, cb)), and an absent file surfaces as POSIX ENOENT —
  // the branch every probe-for-config extension takes.
  const cbfs = makeFs({
    readFile: async (p) => { if (p !== "/w/ok.txt") { const e = new Error("EntryNotFound: " + p); e.code = "FileNotFound"; throw e; } return new TextEncoder().encode("data"); },
  }, {});
  const okRead = await new Promise((res, rej) => cbfs.readFile("/w/ok.txt", "utf8", (e, d) => (e ? rej(e) : res(d))));
  assert.strictEqual(okRead, "data", "fs.readFile (callback form) reads through the adapter");
  const missErr = await new Promise((res) => cbfs.readFile("/w/absent", "utf8", (e) => res(e)));
  assert.strictEqual(missErr && missErr.code, "ENOENT", "an absent file is POSIX ENOENT (FileNotFound translated at the module boundary)");

  // ── unzipVsix(): a stored .vsix round-trips to its file map ─────────────────
  const zb = makeStoredZip({ "extension/package.json": '{"name":"z"}', "extension/out/a.js": "module.exports=1;" });
  const ents = await unzipVsix(zb);
  assert.strictEqual(new TextDecoder().decode(ents["extension/out/a.js"]), "module.exports=1;", "unzipVsix extracts a stored entry verbatim");

  // ── installFromOpenVsx(): resolve + download .vsix + unzip + Node-only gate +
  // install + activate. Fake Open VSX (the real API shape): /api/{ns}/{name}/latest
  // -> { version, files:{download} }; the download URL -> the .vsix bytes. Verifies
  // the full path AND that a browser-entrypoint subject is REFUSED (CC-19, not CC-48).
  const REG = "https://fake-vsx.test";
  const goodVsix = makeStoredZip({
    "extension/package.json": JSON.stringify({ name: "good", publisher: "acme", main: "out/ext.js" }), // Node-only
    "extension/out/ext.js":
      `const vscode=require("vscode");const fs=require("fs");const path=require("path");const u=require("./util");const dep=require("helper");` +
      `const blob=fs.readFileSync(path.join(__dirname,"blob.dat"));` + // bundled resource, sync (like @one-ini's .wasm)
      `function activate(c){c.subscriptions.push(vscode.commands.registerCommand("good."+u.id()+dep.suffix()+blob.length,()=>{}));const i=vscode.window.createStatusBarItem(1,1);i.text="GOOD-ACTIVE";i.show();return{ok:true};}module.exports={activate};`,
    "extension/out/util.js": `module.exports={ id: () => "run" };`,
    "extension/out/blob.dat": new Uint8Array([1, 2, 3]), // a bundled binary resource
    // A bundled bare dependency (node_modules) — resolved by Node-style walk-up.
    "extension/node_modules/helper/package.json": JSON.stringify({ name: "helper", main: "lib/main.js" }),
    "extension/node_modules/helper/lib/main.js": `module.exports={ suffix: () => "-ok" };`,
  });
  const webVsix = makeStoredZip({
    "extension/package.json": JSON.stringify({ name: "web", publisher: "acme", main: "out/ext.js", browser: "out/web.js" }),
    "extension/out/ext.js": "module.exports={activate(){}};",
  });
  const registry = {
    [`${REG}/api/acme/good/latest`]: { version: "2.0.0", files: { download: `${REG}/d/good.vsix` } },
    [`${REG}/api/acme/web/latest`]: { version: "1.0.0", files: { download: `${REG}/d/web.vsix` } },
  };
  const bin = { [`${REG}/d/good.vsix`]: goodVsix, [`${REG}/d/web.vsix`]: webVsix };
  const fakeFetch = async (url) => {
    if (bin[url]) return { ok: true, arrayBuffer: async () => bin[url].buffer };
    const body = registry[url];
    return { ok: body != null, json: async () => body, arrayBuffer: async () => new Uint8Array().buffer };
  };

  const v3 = recordingVscode();
  const inst = await installFromOpenVsx({ vscode: v3, extId: "acme.good", fetchImpl: fakeFetch, registryBase: REG });
  assert.strictEqual(inst.api.ok, true, "installFromOpenVsx downloaded + unzipped the .vsix and activated the Node-only extension");
  assert.deepStrictEqual(v3.rec.commands, ["good.run-ok3"], "the installed extension registered its command (relative dep + bundled node_modules dep + sync readFileSync of a bundled resource all resolved)");
  assert.deepStrictEqual(v3.rec.statusBar, ["GOOD-ACTIVE"], "the installed extension's contribution reached the workbench API");

  let rejected = false;
  try { await installFromOpenVsx({ vscode: recordingVscode(), extId: "acme.web", fetchImpl: fakeFetch, registryBase: REG }); }
  catch (e) { rejected = /not Node-only/.test(String(e)); }
  assert.ok(rejected, "a browser-entrypoint extension is REFUSED (CC-19, not CC-48)");

  console.log("NODE-EXTHOST-CORE-TEST: PASS — the substrate-native ext host loads a Node-only extension (unzip .vsix + installFromOpenVsx, Node-only gate enforced), provides the Node API surface + vscode passthrough, and runs activate()");
})().catch((e) => { console.error("NODE-EXTHOST-CORE-TEST: FAIL —", e && e.stack ? e.stack : e); process.exit(1); });
