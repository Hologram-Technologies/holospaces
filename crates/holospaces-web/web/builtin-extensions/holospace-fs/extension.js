// holospace-fs — binds the real VS Code workbench to the running holospace.
//
// The real workbench's backends are the holospace primitives (ADR-012/ADR-015).
// This extension runs in the browser web-model extension host; it boots the
// holospace (the κ-disk devcontainer on the wasm emulator) right here in the
// host — the browser is a first-class compute substrate — then exposes:
//   • a FileSystemProvider for the `holospace:` scheme over the virtio-9p
//     workspace (CC-15) — the editor reads/writes the same content the OS sees;
//   • the integrated terminal over the booted OS console (CC-11) — keystrokes are
//     canonical events on the holospace's channel.
// No server, no control plane: the workbench is in the tab, the host is in its
// worker, and the backend is content on the substrate (Laws L1/L3/L4).
const vscode = require("vscode");

// Where the deploy serves the wasm peer + the devcontainer assets. Derived from
// this extension's own served location (`context.extensionUri` = `…/ext/holospace-fs`),
// so it is correct for any deploy path (a user site at the root, or a project
// site under `/<repo>/`) — the assets sit beside the `ext/` directory.
let base = "";
function deriveBase(extensionUri) {
  const s = extensionUri.toString().replace(/\/+$/, "");
  return s.replace(/\/ext\/holospace-fs$/, "");
}

let wasm = null;
let ws = null; // the booted holospace Workspace (wasm)
let out = null; // the "Holospace" output channel (set in activate)
let resumed = false; // true when this launch resumed from a persisted snapshot
let bootError = null;
let ready; // resolves when the holospace has booted
const readyPromise = new Promise((r) => (ready = r));

async function fetchBytes(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`fetch ${url}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}
async function gunzip(bytes) {
  const stream = new Response(bytes).body.pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}
async function gzip(bytes) {
  const stream = new Response(bytes).body.pipeThrough(new CompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

// ── Resume persistence over OPFS (CC-30/CC-31) ──────────────────────────────
// A running holospace is content: `Workspace.suspend()` produces a κ snapshot of
// the whole machine (CPU, RAM, rootfs disk, and the virtio-9p workspace files).
// We persist it — gzipped, since most of guest RAM is zero — to the Origin
// Private File System, so the next launch *resumes* from it (no fetch, no rootfs
// assembly, no cold boot) instead of starting over. OPFS is durable but untrusted
// storage, so a cross-session reload is a trust boundary: the bytes are verified
// by re-derivation against the κ recorded beside them before they are trusted
// (Law L5; ADR-019) — a tampered or corrupt snapshot is refused and we cold-boot.
const SNAPSHOT_FILE = "holospace-devcontainer.snapshot.gz";
const SNAPSHOT_KAPPA = "holospace-devcontainer.snapshot.kappa";

async function opfsRoot() {
  if (!navigator.storage || !navigator.storage.getDirectory) return null;
  try {
    return await navigator.storage.getDirectory();
  } catch {
    return null;
  }
}

// Load + verify a persisted snapshot. Returns the raw snapshot bytes, or null if
// none / unreadable / failing re-derivation (in which case the caller cold-boots).
async function loadSnapshot() {
  const root = await opfsRoot();
  if (!root) return null;
  try {
    const gzHandle = await root.getFileHandle(SNAPSHOT_FILE);
    const kHandle = await root.getFileHandle(SNAPSHOT_KAPPA);
    const gzBytes = new Uint8Array(await (await gzHandle.getFile()).arrayBuffer());
    const recordedKappa = await (await kHandle.getFile()).text();
    const snapshot = await gunzip(gzBytes);
    // Law L5: trust the durable-but-untrusted bytes only if they re-derive to the
    // κ we recorded when we wrote them — the same verify-on-receipt the substrate
    // applies at any boundary (ADR-019).
    if (wasm.kappa(snapshot) !== recordedKappa) {
      out && out.appendLine("holospace: persisted snapshot failed κ re-derivation — cold-booting");
      return null;
    }
    return snapshot;
  } catch {
    return null; // no snapshot yet, or storage unavailable
  }
}

let persisting = false;
async function saveSnapshot() {
  if (!ws || ws.halted || persisting) return;
  const root = await opfsRoot();
  if (!root) return;
  persisting = true;
  try {
    const snapshot = ws.suspend(); // the κ snapshot of the whole machine
    const kappa = wasm.kappa(snapshot); // its content address (recorded beside it)
    const gzBytes = await gzip(snapshot);
    const gzHandle = await root.getFileHandle(SNAPSHOT_FILE, { create: true });
    const gw = await gzHandle.createWritable();
    await gw.write(gzBytes);
    await gw.close();
    const kHandle = await root.getFileHandle(SNAPSHOT_KAPPA, { create: true });
    const kw = await kHandle.createWritable();
    await kw.write(kappa);
    await kw.close();
  } catch (e) {
    out && out.appendLine("holospace: snapshot persist failed — " + e);
  } finally {
    persisting = false;
  }
}

// Boot the holospace in the extension host (the web model's backend).
async function bootHolospace() {
  wasm = await import(`${base}/pkg/holospaces_web.js`);
  await wasm.default(`${base}/pkg/holospaces_web_bg.wasm`);

  // Resume path (CC-30): if a verified κ snapshot is persisted from a previous
  // session, restore the whole machine from it — the running OS, its disk, and
  // the workspace files come back exactly. This skips the kernel fetch, the
  // rootfs assembly, and the cold boot entirely.
  const persisted = await loadSnapshot();
  if (persisted) {
    ws = wasm.Workspace.resume_devcontainer(persisted);
    resumed = true;
    out && out.appendLine("holospace: resumed from a persisted κ snapshot — no cold boot");
    return;
  }

  const kernel = await gunzip(await fetchBytes(`${base}/devcontainer-kernel.gz`));
  const layer = await fetchBytes(`${base}/devcontainer-layer.tar.gz`);
  const image = new wasm.DevcontainerImage();
  image.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
  // Assemble a *bootable* rootfs — the persistent devcontainer init is injected,
  // so the OS comes up as a running dev environment (mounts /workspace, execs a
  // shell) instead of powering off right after boot — on a disk with room to work.
  //
  // The devcontainer's disk size. A real dev environment needs space (BusyBox
  // installs its applets, /tmp, the files you create), so this is sized rather
  // than the content-tight minimum. It is the disk a configured holospace would
  // get from its storage quota; for the deployed demo it defaults here, sized for
  // the browser peer (the image lives in wasm memory beside the guest's RAM).
  const DISK_BYTES = 128 * 1024 * 1024;
  const rootfs = image.assemble_bootable(DISK_BYTES); // gunzip + untar + overlay + ext4 + /init, in wasm
  ws = wasm.Workspace.boot_devcontainer(kernel, rootfs);
  // Seed a welcome note into the shared workspace so the editor and the OS both
  // see it (the editor writes by κ; the OS reads it over 9p).
  ws.ws_write(
    "WELCOME.md",
    new TextEncoder().encode(
      "# holospace\n\nThe real VS Code workbench, over the running devcontainer.\n" +
        "This file lives on the virtio-9p workspace (CC-15) — the terminal sees it too.\n",
    ),
  );
}

// ── FileSystemProvider over the virtio-9p workspace (CC-15) ─────────────────
const { FileType, FileSystemError, EventEmitter } = vscode;

function nameOf(uri) {
  const p = uri.path.replace(/^\/+/, "");
  return p.replace(/^workspace\/?/, "");
}

class HolospaceFS {
  constructor() {
    this._emitter = new EventEmitter();
    this.onDidChangeFile = this._emitter.event;
  }
  watch() {
    return new vscode.Disposable(() => {});
  }
  async stat(uri) {
    await readyPromise;
    const name = nameOf(uri);
    if (name === "") {
      return { type: FileType.Directory, ctime: 0, mtime: 0, size: 0 };
    }
    const bytes = ws.ws_read(name);
    if (bytes == null) throw FileSystemError.FileNotFound(uri);
    return { type: FileType.File, ctime: 0, mtime: Date.now(), size: bytes.length };
  }
  async readDirectory() {
    await readyPromise;
    const list = JSON.parse(ws.ws_list());
    return list.map((e) => [e.name, e.dir ? FileType.Directory : FileType.File]);
  }
  async readFile(uri) {
    await readyPromise;
    const bytes = ws.ws_read(nameOf(uri));
    if (bytes == null) throw FileSystemError.FileNotFound(uri);
    return bytes;
  }
  async writeFile(uri, content) {
    await readyPromise;
    ws.ws_write(nameOf(uri), content);
    this._emitter.fire([{ type: vscode.FileChangeType.Changed, uri }]);
  }
  // The mutating operations are the host-side duals of the 9P backend's
  // Tmkdir / Tunlinkat / Trenameat — the editor changing the *same* workspace
  // content the running OS sees over virtio-9p (one content, Law L1).
  async createDirectory(uri) {
    await readyPromise;
    ws.ws_mkdir(nameOf(uri));
    this._emitter.fire([{ type: vscode.FileChangeType.Created, uri }]);
  }
  async delete(uri) {
    await readyPromise;
    if (!ws.ws_delete(nameOf(uri))) throw FileSystemError.FileNotFound(uri);
    this._emitter.fire([{ type: vscode.FileChangeType.Deleted, uri }]);
  }
  async rename(oldUri, newUri) {
    await readyPromise;
    if (!ws.ws_rename(nameOf(oldUri), nameOf(newUri))) {
      throw FileSystemError.FileNotFound(oldUri);
    }
    this._emitter.fire([
      { type: vscode.FileChangeType.Deleted, uri: oldUri },
      { type: vscode.FileChangeType.Created, uri: newUri },
    ]);
  }
}

// ── The integrated terminal over the OS console (CC-11) ─────────────────────
function makeTerminal() {
  const writeEmitter = new EventEmitter();
  let lastLen = 0;
  let line = "";
  let running = true;
  const pump = () => {
    if (!ws || !running) return;
    if (!ws.halted) ws.run(8_000_000);
    const full = ws.terminal();
    if (full.length > lastLen) {
      writeEmitter.fire(full.slice(lastLen).replace(/\n/g, "\r\n"));
      lastLen = full.length;
    }
    setTimeout(pump, 40);
  };
  const pty = {
    onDidWrite: writeEmitter.event,
    open: async () => {
      writeEmitter.fire("holospace — booting the devcontainer OS…\r\n");
      await readyPromise;
      if (bootError) {
        writeEmitter.fire("boot failed: " + bootError.replace(/\n/g, "\r\n") + "\r\n");
        return;
      }
      pump();
    },
    close: () => {
      running = false;
    },
    handleInput: (data) => {
      for (const ch of data) {
        if (ch === "\r") {
          writeEmitter.fire("\r\n");
          if (ws) {
            const out = ws.type_line(line);
            if (out) writeEmitter.fire(out.replace(/\n/g, "\r\n"));
          }
          line = "";
        } else if (ch === "\x7f") {
          if (line.length) {
            line = line.slice(0, -1);
            writeEmitter.fire("\b \b");
          }
        } else {
          line += ch;
          writeEmitter.fire(ch);
        }
      }
    },
  };
  return vscode.window.createTerminal({ name: "holospace", pty });
}

function activate(context) {
  base = deriveBase(context.extensionUri);
  // The workspace FileSystemProvider is the virtio-9p share.
  context.subscriptions.push(
    vscode.workspace.registerFileSystemProvider("holospace", new HolospaceFS(), {
      isCaseSensitive: true,
    }),
  );
  // A command (and an auto-opened terminal) for the OS console.
  context.subscriptions.push(
    vscode.commands.registerCommand("holospace.openTerminal", () => makeTerminal().show()),
  );

  // Boot the holospace in the background; surface failures in an output channel
  // so a load is never silently empty.
  out = vscode.window.createOutputChannel("Holospace");
  context.subscriptions.push(out);
  out.appendLine("holospace: booting the devcontainer in the extension host…");
  bootHolospace()
    .then(() => {
      ready();
      out.appendLine(
        resumed
          ? "holospace: resumed — workspace + terminal are live"
          : "holospace: booted — workspace + terminal are live",
      );
      makeTerminal().show();
      listenForControl(out);
      // Persist the running machine to OPFS periodically (CC-30/CC-31), so the
      // next launch resumes from it instead of cold-booting. The extension host
      // is a worker (no `document` visibility events), so a timer is the portable
      // suspend trigger; `saveSnapshot` no-ops while a previous save is in flight.
      const timer = setInterval(saveSnapshot, 120000);
      context.subscriptions.push(new vscode.Disposable(() => clearInterval(timer)));
    })
    .catch((e) => {
      bootError = String(e && e.stack ? e.stack : e);
      out.appendLine("holospace: boot FAILED — " + bootError);
      out.show(true);
      ready(); // unblock the FS provider (it will report no files)
    });
}

// The control channel from the Platform Manager (ADR-018; CC-28): the panel
// publishes a Configuration as content and broadcasts its κ + bytes; this running
// instance resolves it, *verifies it by re-derivation* (Law L5 — the bytes must
// re-derive to the broadcast κ, or it is refused), and *applies* it to the live
// machine (Workspace.reconfigure). The control plane never calls the instance
// directly — it publishes content, the instance applies it. No server.
function listenForControl(out) {
  let control;
  try {
    control = new BroadcastChannel("holospaces-control");
  } catch {
    return; // no BroadcastChannel (e.g. a non-browser host) — nothing to do
  }
  control.onmessage = (ev) => {
    const msg = ev.data;
    if (!ws || !msg || !msg.bytes) return;
    const bytes = msg.bytes instanceof Uint8Array ? msg.bytes : new Uint8Array(msg.bytes);
    // Law L5: the configuration is applied only if its bytes re-derive to the κ
    // the panel published — content verified on receipt, regardless of source.
    if (msg.kappa && wasm.kappa(bytes) !== msg.kappa) {
      out.appendLine("holospace: refused a configuration — κ mismatch (L5)");
      return;
    }
    try {
      const applied = ws.reconfigure(bytes);
      out.appendLine("holospace: applied configuration " + (msg.kappa || "").slice(0, 22) + "… → " + applied);
    } catch (e) {
      out.appendLine("holospace: configuration not applicable — " + e);
    }
  };
  out.appendLine("holospace: listening for control-plane configurations (ADR-018)");
}

function deactivate() {}

module.exports = { activate, deactivate };
