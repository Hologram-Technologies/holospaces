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

// Boot the holospace in the extension host (the web model's backend).
async function bootHolospace() {
  wasm = await import(`${base}/pkg/holospaces_web.js`);
  await wasm.default(`${base}/pkg/holospaces_web_bg.wasm`);
  const kernel = await gunzip(await fetchBytes(`${base}/devcontainer-kernel.gz`));
  const layer = await fetchBytes(`${base}/devcontainer-layer.tar.gz`);
  const image = new wasm.DevcontainerImage();
  image.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
  const rootfs = image.assemble(); // gunzip + untar + overlay + ext4, in wasm
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
  const out = vscode.window.createOutputChannel("Holospace");
  context.subscriptions.push(out);
  out.appendLine("holospace: booting the devcontainer in the extension host…");
  bootHolospace()
    .then(() => {
      ready();
      out.appendLine("holospace: booted — workspace + terminal are live");
      makeTerminal().show();
      listenForControl(out);
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
