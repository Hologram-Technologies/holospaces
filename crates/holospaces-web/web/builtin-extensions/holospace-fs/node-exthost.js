// CC-48 — the substrate-native Node-API extension host (runtime core).
//
// Runs a Node-only VS Code extension's `main` on the browser peer's OWN JavaScript
// engine — native speed, in the tab, no emulated guest and no Node process on the
// host — by providing the two surfaces a Node-only extension needs:
//
//   • the `vscode` extension API — passed through to the real workbench API the
//     host already holds (so contributions reach the genuine workbench);
//   • the Node built-in surface (`path`, `os`, `util`, `events`, `process`,
//     `Buffer`, `assert`, `fs`/`fs/promises`) — pure-JS, browser-safe, with `fs`
//     backed by the HOLOSPACE'S OWN filesystem (CC-15) via an injected adapter, not
//     a host filesystem.
//
// This is the native-exec discipline (the interpreter wall is the emulated guest /
// wasmi userland; this runs on the substrate peer's native JS engine). It is the
// load-bearing core CC-48's witness drives; designed to be exercised both in the
// browser (by the holospace-fs builtin) and in Node (the self-test below), so the
// module system + API surface are verifiable without the heavy browser run.
//
// Pure CommonJS, no bundler, no Node-host dependency: every built-in is provided
// here, so the same code runs unchanged in a browser worker.

"use strict";

// ── A minimal, browser-safe EventEmitter (Node `events`) ──────────────────────
class EventEmitter {
  constructor() { this._ev = Object.create(null); }
  on(name, fn) { (this._ev[name] ||= []).push(fn); return this; }
  once(name, fn) {
    const w = (...a) => { this.off(name, w); fn(...a); };
    return this.on(name, w);
  }
  off(name, fn) {
    const a = this._ev[name];
    if (a) { const i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); }
    return this;
  }
  removeListener(name, fn) { return this.off(name, fn); }
  removeAllListeners(name) { if (name) delete this._ev[name]; else this._ev = Object.create(null); return this; }
  emit(name, ...args) {
    const a = this._ev[name];
    if (!a || !a.length) return false;
    for (const fn of a.slice()) fn(...args);
    return true;
  }
  listeners(name) { return (this._ev[name] || []).slice(); }
}

// ── `path` (POSIX; the devcontainer/workspace axis is POSIX) ───────────────────
const pathMod = (() => {
  const norm = (p) => {
    const abs = p.startsWith("/");
    const out = [];
    for (const seg of p.split("/")) {
      if (!seg || seg === ".") continue;
      if (seg === "..") { if (out.length && out[out.length - 1] !== "..") out.pop(); else if (!abs) out.push(".."); }
      else out.push(seg);
    }
    let s = out.join("/");
    if (abs) s = "/" + s;
    return s || (abs ? "/" : ".");
  };
  const join = (...parts) => norm(parts.filter((p) => p != null && p !== "").join("/"));
  const dirname = (p) => { const i = p.replace(/\/+$/, "").lastIndexOf("/"); return i <= 0 ? (i === 0 ? "/" : ".") : p.slice(0, i); };
  const basename = (p, ext) => { let b = p.replace(/\/+$/, "").split("/").pop() || ""; if (ext && b.endsWith(ext)) b = b.slice(0, -ext.length); return b; };
  const extname = (p) => { const b = basename(p); const i = b.lastIndexOf("."); return i > 0 ? b.slice(i) : ""; };
  const isAbsolute = (p) => p.startsWith("/");
  const resolve = (...parts) => { let cur = "/"; for (const p of parts) { if (!p) continue; cur = p.startsWith("/") ? p : join(cur, p); } return norm(cur); };
  const relative = (from, to) => {
    const f = norm(from).split("/").filter(Boolean), t = norm(to).split("/").filter(Boolean);
    let i = 0; while (i < f.length && i < t.length && f[i] === t[i]) i++;
    return [...f.slice(i).map(() => ".."), ...t.slice(i)].join("/") || ".";
  };
  return { sep: "/", delimiter: ":", normalize: norm, join, dirname, basename, extname, isAbsolute, resolve, relative, posix: null };
})();
pathMod.posix = pathMod;

// ── `os` (the holospace identity, not the host) ───────────────────────────────
const osMod = {
  EOL: "\n",
  platform: () => "linux",
  arch: () => "wasm32",
  homedir: () => "/root",
  tmpdir: () => "/tmp",
  hostname: () => "holospace",
  type: () => "Linux",
  release: () => "0.0.0-holospace",
  cpus: () => [{ model: "holospace-substrate", speed: 0, times: {} }],
  totalmem: () => 0,
  freemem: () => 0,
  networkInterfaces: () => ({}),
  userInfo: () => ({ username: "root", homedir: "/root", shell: "/bin/sh", uid: 0, gid: 0 }),
};

// ── `util` (the subset extensions actually use) ───────────────────────────────
const utilMod = {
  inherits(ctor, superCtor) { ctor.super_ = superCtor; Object.setPrototypeOf(ctor.prototype, superCtor.prototype); },
  inspect: (o) => { try { return JSON.stringify(o); } catch { return String(o); } },
  format: (...a) => a.map((x) => (typeof x === "string" ? x : utilMod.inspect(x))).join(" "),
  promisify: (fn) => (...args) => new Promise((res, rej) => fn(...args, (e, v) => (e ? rej(e) : res(v)))),
  deprecate: (fn) => fn,
  TextEncoder, TextDecoder,
  types: { isPromise: (x) => !!x && typeof x.then === "function" },
};

// Build the `fs` shim over an injected holospace FS adapter (CC-15). The adapter
// is async (reads route over the bridge); the sync `fs` surface is intentionally
// minimal — extensions that need heavy sync fs are out of the Node-only baseline.
function makeFs(adapter) {
  const a = adapter || {};
  const enc = new TextEncoder();
  const promises = {
    readFile: async (p, opts) => {
      const bytes = await a.readFile(String(p));
      const encoding = typeof opts === "string" ? opts : opts && opts.encoding;
      return encoding ? new TextDecoder().decode(bytes) : bytes;
    },
    writeFile: async (p, data) => a.writeFile(String(p), typeof data === "string" ? enc.encode(data) : data),
    mkdir: async (p, opts) => a.mkdir ? a.mkdir(String(p), opts) : undefined,
    readdir: async (p) => (a.readdir ? a.readdir(String(p)) : []),
    stat: async (p) => (a.stat ? a.stat(String(p)) : { isFile: () => true, isDirectory: () => false }),
    access: async (p) => { if (a.exists && !(await a.exists(String(p)))) throw new Error("ENOENT: " + p); },
    rm: async (p, opts) => a.rm ? a.rm(String(p), opts) : undefined,
    unlink: async (p) => a.rm ? a.rm(String(p)) : undefined,
  };
  return {
    promises,
    existsSync: (p) => (a.existsSync ? !!a.existsSync(String(p)) : false),
    readFileSync: (p, opts) => {
      if (!a.readFileSync) throw new Error("fs.readFileSync unsupported in the substrate-native host (use fs.promises)");
      const bytes = a.readFileSync(String(p));
      const encoding = typeof opts === "string" ? opts : opts && opts.encoding;
      return encoding ? new TextDecoder().decode(bytes) : bytes;
    },
    constants: { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1 },
  };
}

// ── The CommonJS module host ──────────────────────────────────────────────────
//
// `vscode` is the real workbench API (passthrough). `fsAdapter` backs `fs` with
// the holospace FS (CC-15). `files` is the extension's content (a map of POSIX
// path → source string / Uint8Array), as fetched from Open VSX (the .vsix) — no
// host filesystem is touched.
function createNodeExtHost({ vscode, fsAdapter, files, extensionPath = "/extension", processEnv = {} } = {}) {
  if (!vscode) throw new Error("createNodeExtHost requires the vscode API");
  const fileMap = files || {};
  const processShim = {
    platform: "linux", arch: "wasm32", version: "v20.0.0-holospace",
    versions: { node: "20.0.0", v8: "0.0" }, env: { ...processEnv },
    argv: ["node", extensionPath], cwd: () => extensionPath, pid: 1,
    nextTick: (fn, ...a) => Promise.resolve().then(() => fn(...a)),
    hrtime: (() => { const h = (p) => { const t = (typeof performance !== "undefined" ? performance.now() : 0) * 1e6; const ns = Math.floor(t); const r = [Math.floor(ns / 1e9), ns % 1e9]; return p ? [r[0] - p[0], r[1] - p[1]] : r; }; h.bigint = () => BigInt(0); return h; })(),
    on() {}, once() {}, off() {}, emit() { return false; },
    exit() { throw new Error("process.exit() is not permitted in the substrate-native ext host"); },
  };

  const builtins = {
    vscode,
    path: pathMod, "node:path": pathMod,
    os: osMod, "node:os": osMod,
    util: utilMod, "node:util": utilMod,
    events: Object.assign(EventEmitter, { EventEmitter, default: EventEmitter }),
    "node:events": Object.assign(EventEmitter, { EventEmitter }),
    assert: makeAssert(),
    process: processShim, "node:process": processShim,
    buffer: { Buffer: globalThis.Buffer || makeBuffer() }, "node:buffer": { Buffer: globalThis.Buffer || makeBuffer() },
    fs: makeFs(fsAdapter), "node:fs": makeFs(fsAdapter),
    "fs/promises": makeFs(fsAdapter).promises, "node:fs/promises": makeFs(fsAdapter).promises,
  };

  const cache = Object.create(null);

  const resolveLocal = (from, id) => {
    const base = id.startsWith("/") ? id : pathMod.join(pathMod.dirname(from), id);
    const cands = [base, base + ".js", base + ".json", pathMod.join(base, "index.js")];
    // package.json "main" for a directory require.
    const pj = pathMod.join(base, "package.json");
    if (fileMap[pj] != null) {
      try { const m = JSON.parse(srcOf(fileMap[pj])).main; if (m) cands.unshift(pathMod.join(base, m), pathMod.join(base, m) + ".js"); } catch { /* ignore */ }
    }
    for (const c of cands) if (fileMap[c] != null) return c;
    return null;
  };

  function requireFrom(fromPath) {
    return function require(id) {
      if (Object.prototype.hasOwnProperty.call(builtins, id)) return builtins[id];
      const resolved = resolveLocal(fromPath, id);
      if (!resolved) throw new Error(`Cannot find module '${id}' (from ${fromPath}) in the substrate-native ext host`);
      if (cache[resolved]) return cache[resolved].exports;
      return evalModule(resolved);
    };
  }

  function evalModule(modPath) {
    const src = srcOf(fileMap[modPath]);
    if (modPath.endsWith(".json")) {
      const mod = { exports: JSON.parse(src) };
      cache[modPath] = mod;
      return mod.exports;
    }
    const module = { exports: {} };
    cache[modPath] = module; // cache before eval (circular deps)
    const dirname = pathMod.dirname(modPath);
    const fn = new Function(
      "module", "exports", "require", "__dirname", "__filename", "process", "Buffer", "global", "globalThis",
      src,
    );
    fn(module, module.exports, requireFrom(modPath), dirname, modPath, processShim, builtins.buffer.Buffer, globalThis, globalThis);
    return module.exports;
  }

  return {
    builtins,
    /** Load the extension's `main` and run `activate(context)`; returns the context. */
    async activate(packageJson, contextOverrides = {}) {
      const main = (packageJson && packageJson.main) || "extension.js";
      const mainPath = main.startsWith("/") ? main : pathMod.join(extensionPath, main);
      const real = resolveLocal(extensionPath + "/_", main.startsWith("/") ? main : "./" + main)
        || (fileMap[mainPath] != null ? mainPath : (fileMap[mainPath + ".js"] != null ? mainPath + ".js" : null));
      if (!real) throw new Error(`extension main '${main}' not found in the extension files`);
      const exports = evalModule(real);
      if (typeof exports.activate !== "function") throw new Error("extension has no activate()");
      const context = {
        subscriptions: [],
        extensionPath,
        extensionUri: vscode.Uri ? vscode.Uri.parse("holospace://extension") : { path: extensionPath },
        globalState: memState(), workspaceState: memState(),
        asAbsolutePath: (rel) => pathMod.join(extensionPath, rel),
        environmentVariableCollection: { replace() {}, append() {}, prepend() {}, clear() {} },
        secrets: { get: async () => undefined, store: async () => {}, delete: async () => {} },
        extensionMode: 1,
        ...contextOverrides,
      };
      const result = await exports.activate(context);
      return { context, exports, api: result };
    },
  };
}

function memState() {
  const m = new Map();
  return { get: (k, d) => (m.has(k) ? m.get(k) : d), update: async (k, v) => { m.set(k, v); }, keys: () => [...m.keys()] };
}
function srcOf(v) { return typeof v === "string" ? v : new TextDecoder().decode(v); }
function makeAssert() {
  const assert = (c, m) => { if (!c) throw new Error(m || "assertion failed"); };
  assert.ok = assert;
  assert.equal = (a, b, m) => { if (a != b) throw new Error(m || `${a} != ${b}`); };
  assert.strictEqual = (a, b, m) => { if (a !== b) throw new Error(m || `${a} !== ${b}`); };
  assert.deepStrictEqual = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m || "not deep-equal"); };
  return assert;
}
function makeBuffer() {
  // Minimal Buffer over Uint8Array for the browser (Node provides the real one).
  class Buf extends Uint8Array {
    static from(d, enc) { return typeof d === "string" ? new Buf(new TextEncoder().encode(d)) : new Buf(d); }
    static alloc(n) { return new Buf(n); }
    static isBuffer(x) { return x instanceof Buf || x instanceof Uint8Array; }
    toString(enc) { return new TextDecoder(enc === "hex" ? undefined : enc).decode(this); }
  }
  return Buf;
}

// CommonJS export: the holospace-fs builtin `require`s this (its ext host loads as
// CommonJS, like `extension.js`), and the Node self-test does too. Kept CommonJS
// (no top-level `export`) so a single file is both `require`-able in the browser
// ext host and runnable under Node.
module.exports = { createNodeExtHost, EventEmitter, path: pathMod, os: osMod, makeFs };
