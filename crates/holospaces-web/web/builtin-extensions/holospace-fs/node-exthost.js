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
  // Node's path.parse/format — guest extensions walk directory chains with
  // `path.parse(p).root` (editorconfig et al.); a shim without them throws
  // "path.parse is not a function" from inside the guest extension's listeners.
  const parse = (p) => {
    const root = p.startsWith("/") ? "/" : "";
    const base = basename(p);
    const ext = base === "." || base === ".." ? "" : extname(p);
    const dir = dirname(p);
    const d = p === "/" ? "/" : dir === "." && !p.startsWith("./") ? "" : dir;
    return { root, dir: d, base, ext, name: ext ? base.slice(0, -ext.length) : base };
  };
  const format = (o) => {
    const dir = o.dir || o.root || "";
    const base = o.base != null ? o.base : (o.name || "") + (o.ext || "");
    return dir && dir !== "/" ? `${dir}/${base}` : `${dir}${base}`;
  };
  return { sep: "/", delimiter: ":", normalize: norm, join, dirname, basename, extname, isAbsolute, resolve, relative, parse, format, posix: null };
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
function makeFs(adapter, fileMap) {
  const a = adapter || {};
  const fm = fileMap || {};
  const enc = new TextEncoder();
  // A bundled extension file (from the unzipped .vsix, already in memory) read
  // synchronously — this is what a load-time `fs.readFileSync` actually wants (a
  // resource shipped inside the extension, e.g. a `.wasm` blob), as opposed to a
  // workspace file (which lives on the holospace FS and is async-only). Returns the
  // raw bytes, or null if the path is not a bundled file.
  const bundled = (p) => {
    const key = String(p);
    if (fm[key] != null) return fm[key];
    return null;
  };
  // A guest extension branches on POSIX `err.code === "ENOENT"` for an absent
  // file (the common probe-for-config pattern); the holospace FS throws vscode's
  // FileSystemError ("FileNotFound"/"EntryNotFound"). Translate at this module
  // boundary so absent files read as absent, not as an opaque failure.
  const enoent = (p, op) => Object.assign(new Error(`ENOENT: no such file or directory, ${op} '${p}'`), { code: "ENOENT", errno: -2, path: String(p) });
  const missing = (e) => e && (e.code === "FileNotFound" || e.code === "ENOENT" || /EntryNotFound|FileNotFound|ENOENT|entry not found/i.test(e.message || ""));
  const posix = (op) => (fn) => async (p, ...rest) => {
    try { return await fn(p, ...rest); }
    catch (e) { throw missing(e) ? enoent(p, op) : e; }
  };
  const promises = {
    readFile: posix("open")(async (p, opts) => {
      const b = bundled(p);
      const bytes = b != null ? b : await a.readFile(String(p));
      const encoding = typeof opts === "string" ? opts : opts && opts.encoding;
      return encoding ? new TextDecoder().decode(bytes) : bytes;
    }),
    writeFile: async (p, data) => a.writeFile(String(p), typeof data === "string" ? enc.encode(data) : data),
    mkdir: async (p, opts) => a.mkdir ? a.mkdir(String(p), opts) : undefined,
    readdir: posix("scandir")(async (p) => (a.readdir ? a.readdir(String(p)) : [])),
    stat: posix("stat")(async (p) => (a.stat ? a.stat(String(p)) : { isFile: () => true, isDirectory: () => false })),
    access: async (p) => { if (a.exists && !(await a.exists(String(p)))) throw enoent(p, "access"); },
    rm: async (p, opts) => a.rm ? a.rm(String(p), opts) : undefined,
    unlink: async (p) => a.rm ? a.rm(String(p)) : undefined,
  };
  promises.lstat = promises.stat;
  promises.realpath = async (p) => String(p);
  promises.readlink = async (p) => { throw Object.assign(new Error("EINVAL: not a symlink, readlink '" + p + "'"), { code: "EINVAL" }); };
  // Node's CALLBACK-style fs API over the same impls — real extensions call
  // `fs.readFile(p, cb)` / `fs.stat(p, cb)` (editorconfig et al.); a module
  // object without them throws "fs.readFile is not a function" from inside the
  // extension's listeners. Last argument is the callback; options are optional.
  const cbify = (fn) => (...args) => {
    const cb = args[args.length - 1];
    if (typeof cb !== "function") throw new TypeError("fs callback API requires a callback");
    fn(...args.slice(0, -1)).then((r) => cb(null, r), (e) => cb(e));
  };
  return {
    promises,
    readFile: cbify(promises.readFile),
    writeFile: cbify(promises.writeFile),
    mkdir: cbify(promises.mkdir),
    readdir: cbify(promises.readdir),
    stat: cbify(promises.stat),
    lstat: cbify(promises.lstat),
    access: cbify(promises.access),
    unlink: cbify(promises.unlink),
    rm: cbify(promises.rm),
    realpath: cbify(promises.realpath),
    readlink: cbify(promises.readlink),
    exists: (p, cb) => promises.access(p).then(() => cb(true), () => cb(false)),
    existsSync: (p) => (bundled(p) != null ? true : a.existsSync ? !!a.existsSync(String(p)) : false),
    readFileSync: (p, opts) => {
      // Bundled extension resources (the unzipped .vsix) are in memory → real sync
      // read. A workspace file is on the holospace FS (async over the bridge) and
      // cannot be read synchronously here — extensions needing that are out of the
      // Node-only baseline (use fs.promises).
      const b = bundled(p) ?? (a.readFileSync ? a.readFileSync(String(p)) : null);
      if (b == null) throw new Error(`ENOENT (sync): '${p}' is not a bundled extension file; use fs.promises for holospace files`);
      const encoding = typeof opts === "string" ? opts : opts && opts.encoding;
      return encoding ? new TextDecoder().decode(b) : b;
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
    fs: makeFs(fsAdapter, fileMap), "node:fs": makeFs(fsAdapter, fileMap),
    "fs/promises": makeFs(fsAdapter, fileMap).promises, "node:fs/promises": makeFs(fsAdapter, fileMap).promises,
  };

  const cache = Object.create(null);

  // Resolve a `base` path as a file (with .js/.json/index.js + package.json `main`
  // for a directory) against the in-memory file map. Returns the key or null.
  const resolveBase = (base) => {
    const cands = [base, base + ".js", base + ".json"];
    const pj = pathMod.join(base, "package.json");
    if (fileMap[pj] != null) {
      try {
        const m = JSON.parse(srcOf(fileMap[pj])).main;
        if (m) cands.push(pathMod.join(base, m), pathMod.join(base, m) + ".js", pathMod.join(base, m, "index.js"));
      } catch { /* ignore */ }
    }
    cands.push(pathMod.join(base, "index.js"));
    for (const c of cands) if (fileMap[c] != null) return c;
    return null;
  };

  // Node-style module resolution: relative/absolute ids resolve against the
  // requiring module's dir; bare ids (a package) walk up `node_modules` from the
  // requiring dir to the extension root — exactly how the bundled deps in a .vsix
  // are laid out (`extension/node_modules/<pkg>`).
  const resolveLocal = (from, id) => {
    if (id.startsWith(".") || id.startsWith("/")) {
      return resolveBase(id.startsWith("/") ? id : pathMod.join(pathMod.dirname(from), id));
    }
    let dir = pathMod.dirname(from);
    for (;;) {
      const hit = resolveBase(pathMod.join(dir, "node_modules", id));
      if (hit) return hit;
      if (!dir || dir === "/" || dir === ".") break;
      const parent = pathMod.dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
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

// ── Read a `.vsix` (a ZIP) into a file map, browser-safe ──────────────────────
//
// Open VSX's per-file API serves only curated files (README, package.json), not
// source — so the extension's code comes from its `.vsix` (the download URL). A
// `.vsix` is a plain ZIP; entries live under `extension/`. This is a minimal ZIP
// reader: it walks the central directory and inflates each entry with the
// platform `DecompressionStream("deflate-raw")` (present in browsers and Node ≥18)
// — no zip library, no host filesystem. Returns a map of entry path -> Uint8Array.
async function unzipVsix(buf) {
  const u8 = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  // End Of Central Directory: scan back for signature 0x06054b50.
  let eocd = -1;
  for (let i = u8.length - 22; i >= 0 && i >= u8.length - 22 - 65536; i--) {
    if (dv.getUint32(i, true) === 0x0605_4b50) { eocd = i; break; }
  }
  if (eocd < 0) throw new Error("unzipVsix: not a zip (no EOCD)");
  const count = dv.getUint16(eocd + 10, true);
  let p = dv.getUint32(eocd + 16, true); // central directory offset
  const inflateRaw = async (slice) =>
    new Uint8Array(await new Response(new Response(slice).body.pipeThrough(new DecompressionStream("deflate-raw"))).arrayBuffer());
  const out = {};
  for (let n = 0; n < count; n++) {
    if (dv.getUint32(p, true) !== 0x0201_4b50) break; // central file header
    const method = dv.getUint16(p + 10, true);
    const compSize = dv.getUint32(p + 20, true);
    const nameLen = dv.getUint16(p + 28, true);
    const extraLen = dv.getUint16(p + 30, true);
    const commentLen = dv.getUint16(p + 32, true);
    const localOff = dv.getUint32(p + 42, true);
    const name = new TextDecoder().decode(u8.subarray(p + 46, p + 46 + nameLen));
    // Local header: data starts after its own name+extra fields.
    const lNameLen = dv.getUint16(localOff + 26, true);
    const lExtraLen = dv.getUint16(localOff + 28, true);
    const dataStart = localOff + 30 + lNameLen + lExtraLen;
    const comp = u8.subarray(dataStart, dataStart + compSize);
    if (!name.endsWith("/")) {
      out[name] = method === 0 ? comp.slice() : await inflateRaw(comp);
    }
    p += 46 + nameLen + extraLen + commentLen;
  }
  return out;
}

// ── Install + activate a Node-only Open VSX extension in the substrate-native host
//
// The full CC-48 install path: resolve the extension on Open VSX, download its
// `.vsix`, unzip it, REFUSE it unless its (unzipped) manifest is genuinely Node-only
// (a `main`, no `browser` entrypoint — so it cannot run in vscode-web's web host and
// its activation proves the substrate-native host did the work), then run `activate`
// against the unzipped files. No host filesystem, no Node process, no emulated guest
// — the extension's JS runs on the browser peer's own engine with the holospace's
// primitives behind the Node API.
async function installFromOpenVsx({ vscode, extId, fsAdapter, fetchImpl, registryBase = "https://open-vsx.org" } = {}) {
  const doFetch = fetchImpl || (typeof fetch !== "undefined" ? fetch : null);
  if (!doFetch) throw new Error("installFromOpenVsx: no fetch available");
  if (!extId || !extId.includes(".")) throw new Error(`installFromOpenVsx: bad extension id '${extId}'`);
  const dot = extId.indexOf(".");
  const ns = extId.slice(0, dot), name = extId.slice(dot + 1);

  const meta = await (await doFetch(`${registryBase}/api/${ns}/${name}/latest`)).json();
  const version = meta && meta.version;
  if (!version) throw new Error(`installFromOpenVsx: ${extId} not found on Open VSX`);
  const vsixUrl = meta.files && meta.files.download;
  if (!vsixUrl) throw new Error(`installFromOpenVsx: no .vsix download for ${extId}`);

  // Download + unzip the .vsix; its files live under `extension/`.
  const vsixBytes = new Uint8Array(await (await doFetch(vsixUrl)).arrayBuffer());
  const entries = await unzipVsix(vsixBytes);
  const extensionPath = `/ext/${ns}.${name}`;
  const files = {};
  for (const [path, bytes] of Object.entries(entries)) {
    if (path.startsWith("extension/")) files[pathMod.join(extensionPath, path.slice("extension/".length))] = bytes;
  }
  const pkgKey = pathMod.join(extensionPath, "package.json");
  if (files[pkgKey] == null) throw new Error(`installFromOpenVsx: ${extId} .vsix has no extension/package.json`);
  const packageJson = JSON.parse(srcOf(files[pkgKey]));

  // The CC-48 gate: the subject MUST be Node-only (verified from the .vsix manifest).
  const nodeOnly = typeof packageJson.main === "string" && packageJson.main.length > 0 && packageJson.browser == null;
  if (!nodeOnly) {
    throw new Error(
      `installFromOpenVsx: ${extId} is not Node-only (browser=${JSON.stringify(packageJson.browser)}, main=${JSON.stringify(packageJson.main)}) — ` +
        "a browser-entrypoint extension is CC-19 (web host), not the CC-48 substrate-native host",
    );
  }

  const host = createNodeExtHost({ vscode, fsAdapter, extensionPath, files });
  const activated = await host.activate(packageJson);
  return { host, extId, version, packageJson, fileCount: Object.keys(files).length, ...activated };
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
module.exports = { createNodeExtHost, installFromOpenVsx, unzipVsix, EventEmitter, path: pathMod, os: osMod, makeFs };
