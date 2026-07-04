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
//
// CRITICAL — the persisted state is keyed by the holospace's IDENTITY (its κ),
// never a single global slot. OPFS is per-origin, shared by every workbench tab,
// so a fixed filename would make every holospace read and write the *same*
// snapshot: launching holospace B would resume A's machine (A's files, A's idle
// shell), and a deleted holospace's remnants would bleed into a new one. Keying
// by κ (identity is what-not-where, Law L1) gives each holospace its own slot —
// the same holospace resumes its own state across sessions; distinct holospaces
// never collide. `holoKey` is set in `activate` from the workspace folder κ.
let holoKey = "default"; // sanitized holospace identity; the OPFS namespace key
// Sanitize a holospace identity (its κ) to a safe OPFS filename component. The
// Platform Manager's delete/terminate cleanup (index.html `holoKeyOf`) MUST use
// the same mapping, or it would fail to clear what the workbench wrote. An empty
// identity (the single-holospace demo) is the lone "default" slot.
function sanitizeHoloKey(authority) {
  return String(authority || "").replace(/[^A-Za-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "default";
}
// The OPFS filenames for a holospace key — every durable per-instance artifact is
// namespaced by the identity, so distinct holospaces never share a slot.
function namesFor(key) {
  return {
    snapshot: `holospace.${key}.snapshot.gz`,
    kappa: `holospace.${key}.snapshot.kappa`,
    scrollback: `holospace.${key}.scrollback.gz`,
  };
}
function snapshotFile() {
  return namesFor(holoKey).snapshot;
}
function snapshotKappaFile() {
  return namesFor(holoKey).kappa;
}
function scrollbackFile() {
  return namesFor(holoKey).scrollback;
}
// Derive the OPFS namespace key from the launched holospace's κ — carried in the
// workspace folder URI authority (`holospace://<κ>/workspace`, set by
// build-workbench from `?id=<κ>`).
function deriveHoloKey() {
  try {
    const folder = vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0];
    return sanitizeHoloKey(folder && folder.uri && folder.uri.authority);
  } catch {
    return "default";
  }
}

async function opfsRoot() {
  if (!navigator.storage || !navigator.storage.getDirectory) return null;
  try {
    return await navigator.storage.getDirectory();
  } catch {
    return null;
  }
}

// The terminal scrollback captured beside the last snapshot, for replay on resume
// (set by loadSnapshot). The machine snapshot is κ-pure — it does NOT carry the
// console output buffer (a projection of the past, not future-affecting machine
// state), so a freshly restored machine's console is empty. Without this, a
// resumed *idle* shell (the steady state when the periodic snapshot is taken)
// would show a blank terminal — the OS is live but silent until you type. The
// terminal layer, not the machine, restores what the user was looking at.
let resumeScrollback = null;

// Load + verify a persisted snapshot for THIS holospace. Returns the raw snapshot
// bytes, or null if none / unreadable / failing re-derivation (caller cold-boots).
async function loadSnapshot() {
  const root = await opfsRoot();
  if (!root) return null;
  try {
    const gzHandle = await root.getFileHandle(snapshotFile());
    const kHandle = await root.getFileHandle(snapshotKappaFile());
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
    // Best-effort: load the scrollback saved beside it (resume the visible terminal).
    try {
      const sbHandle = await root.getFileHandle(scrollbackFile());
      resumeScrollback = await gunzip(new Uint8Array(await (await sbHandle.getFile()).arrayBuffer()));
    } catch {
      resumeScrollback = null;
    }
    return snapshot;
  } catch {
    return null; // no snapshot yet, or storage unavailable
  }
}

async function writeOpfs(root, name, bytes) {
  const handle = await root.getFileHandle(name, { create: true });
  const w = await handle.createWritable();
  await w.write(bytes);
  await w.close();
}

// The bootable rootfs the Manager provisioned for this holospace (CC-42), staged
// in OPFS under `provisioned/<holospace id>`. `null` when none was staged (a
// direct workbench open with no Manager — the workbench-machinery tests).
async function readProvisioned(id) {
  const root = await opfsRoot();
  if (!root) return null;
  try {
    const dir = await root.getDirectoryHandle("provisioned", { create: false });
    const handle = await dir.getFileHandle(id, { create: false });
    const file = await handle.getFile();
    return new Uint8Array(await file.arrayBuffer());
  } catch {
    return null; // not provisioned
  }
}

// Open the OPFS pack file backing the paged κ-disk's store, behind a synchronous
// access handle (worker-only — which is where the extension host, and thus the
// emulator, runs). A fresh pack per boot (truncated); the disk's sectors are
// content-addressed into it, so they live off the wasm heap. `null` if OPFS sync
// access is unavailable (then the caller falls back to the in-RAM κ-disk).
async function openDiskStore(id) {
  const root = await opfsRoot();
  if (!root || !id) return null;
  try {
    const dir = await root.getDirectoryHandle("disk-store", { create: true });
    const fh = await dir.getFileHandle(`${id}.pack`, { create: true });
    const handle = await fh.createSyncAccessHandle();
    handle.truncate(0); // a fresh pack each boot
    return handle;
  } catch {
    return null; // no sync access handle (not a worker, or unsupported)
  }
}

// A synchronous read handle on the provisioned rootfs file — so the κ-disk can be
// streamed sector-by-sector into its store without ever reading the whole image
// into RAM. `null` if it was not provisioned or sync access is unavailable.
async function openProvisionedHandle(id) {
  const root = await opfsRoot();
  if (!root || !id) return null;
  try {
    const dir = await root.getDirectoryHandle("provisioned", { create: false });
    const fh = await dir.getFileHandle(id, { create: false });
    return await fh.createSyncAccessHandle();
  } catch {
    return null;
  }
}

// A synchronous read handle on the rootfs OCCUPANCY sidecar the tracked assembler
// wrote (provisioned/<id>.occ — packed little-endian u64 block indices). It lets the
// boot page an arbitrarily large, build-capable disk O(content): only the occupied
// blocks are read, never the declared size. `null` if absent (a pre-occupancy
// provision), in which case the caller falls back to the O(disk) streamed boot.
async function openOccupancyHandle(id) {
  const root = await opfsRoot();
  if (!root || !id) return null;
  try {
    const dir = await root.getDirectoryHandle("provisioned", { create: false });
    const fh = await dir.getFileHandle(`${id}.occ`, { create: false });
    return await fh.createSyncAccessHandle();
  } catch {
    return null;
  }
}

let persisting = false;
async function saveSnapshot() {
  // Snapshot/resume is a riscv64 Workspace capability; the aarch64 terminal core
  // does not expose `suspend` yet (the continued build), so skip there.
  if (!ws || ws.halted || persisting || typeof ws.suspend !== "function") return;
  const root = await opfsRoot();
  if (!root) return;
  persisting = true;
  try {
    const snapshot = ws.suspend(); // the κ snapshot of the whole machine
    const kappa = wasm.kappa(snapshot); // its content address (recorded beside it)
    await writeOpfs(root, snapshotFile(), await gzip(snapshot));
    await writeOpfs(root, snapshotKappaFile(), new TextEncoder().encode(kappa));
    // The visible terminal scrollback, gzipped, beside the snapshot — replayed on
    // resume so the user comes back to the session they left, not a blank screen.
    // Bounded to a recent tail: enough to restore context, not a whole session's
    // history (the machine state is the snapshot; this is just what's on screen).
    const SCROLLBACK_TAIL = 128 * 1024;
    const full = ws.terminal();
    const tail = full.length > SCROLLBACK_TAIL ? full.slice(full.length - SCROLLBACK_TAIL) : full;
    await writeOpfs(root, scrollbackFile(), await gzip(new TextEncoder().encode(tail)));
  } catch (e) {
    out && out.appendLine("holospace: snapshot persist failed — " + e);
  } finally {
    persisting = false;
  }
}

// Boot the holospace in the extension host (the web model's backend).
// Whether this launch booted the bridged (networked) devcontainer, which runs a
// language server reachable over the in-process substrate bridge (ADR-020). A
// networked machine's snapshot does not yet carry the virtio-net device + live
// connection state, so the bridged devcontainer *cold-boots* each session rather
// than resuming (a documented frontier); the resume path (CC-30/31) stays for the
// non-networked machine and is exercised by its witnesses.
let bridged = false;

async function bootHolospace() {
  wasm = await import(`${base}/pkg/holospaces_web.js`);
  await wasm.default(`${base}/pkg/holospaces_web_bg.wasm`);

  // Resume path (CC-30): if a verified κ snapshot is persisted from a previous
  // session, restore the whole machine from it — the running OS, its disk, and
  // the workspace files come back exactly. This skips the kernel fetch, the
  // rootfs assembly, and the cold boot entirely. (Skipped for the bridged
  // devcontainer — see `bridged`.)
  const persisted = await loadSnapshot();
  if (persisted) {
    ws = wasm.Workspace.resume_devcontainer(persisted);
    resumed = true;
    out && out.appendLine("holospace: resumed from a persisted κ snapshot — no cold boot");
    return;
  }

  const folder = vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0];
  const holoId = folder && folder.uri ? folder.uri.authority : "";
  const query = folder && folder.uri && folder.uri.query
    ? new URLSearchParams(folder.uri.query)
    : new URLSearchParams();
  // The holospace's architecture (ADR-021) selects the guest kernel + the CPU
  // core; the per-guest egress node (CC-39), if set, rides the same folder query.
  // The Manager launches amd64 by default (the registry/Codespaces architecture);
  // a BARE workbench open (no Manager, no provisioned image) falls back to the
  // self-contained riscv64 demo boot, which needs no router or provisioning.
  const arch = query.get("arch") || "riscv64";
  const egress = query.get("egress");
  // A real arm64 Linux for aarch64, a real amd64 vmlinux for x64, else the
  // networked RISC-V kernel.
  const kernel = await gunzip(
    await fetchBytes(
      arch === "aarch64"
        ? `${base}/devcontainer-arm64-kernel.gz`
        : arch === "x64"
          ? `${base}/devcontainer-x64-kernel.gz`
          : `${base}/devcontainer-net-kernel.gz`,
    ),
  );

  if (arch === "x64") {
    // x86-64 holospace (the DEFAULT architecture): boot the provisioned amd64
    // image on the x64 core, paged from OPFS (CC-43/CC-44/CC-45) — a real amd64
    // devcontainer with the full device surface: terminal (CC-11), the shared
    // virtio-9p workspace (CC-15 — editor, tasks, search, SCM all bind to it),
    // and the router-backed network, identical to the other cores (Law L4).
    if (holoId) {
      const rootfsHandle = await openProvisionedHandle(holoId);
      if (rootfsHandle) {
        const diskHandle = await openDiskStore(holoId);
        if (diskHandle) {
          // PREFERRED: occupancy-paged — only the blocks the provision actually wrote
          // are read, so an arbitrarily large build-capable disk boots O(content).
          const occHandle = await openOccupancyHandle(holoId);
          if (occHandle) {
            ws = wasm.X64Workspace.bootDevcontainerOpfsStreamedOccupancy(kernel, rootfsHandle, occHandle, diskHandle);
            out && out.appendLine("holospace: booted the provisioned amd64 image on the x64 core (CC-45) — O(content) occupancy-paged from OPFS");
          } else {
            // Fallback for a pre-occupancy provision: page every sector (O(disk)).
            ws = wasm.X64Workspace.boot_devcontainer_opfs_streamed(kernel, rootfsHandle, diskHandle);
            out && out.appendLine("holospace: booted the provisioned amd64 image on the x64 core (CC-44) — paged from OPFS");
          }
          bridged = false;
        } else {
          try { rootfsHandle.close(); } catch {}
        }
      }
    }
    if (!ws) {
      // BLANK / no-provisioning fallback: boot the BUNDLED amd64 busybox layer
      // in-RAM — no registry pull, no router extension. A user launches a blank
      // amd64 scratchpad and gets a shell + editor + tasks + search immediately.
      const layer = await fetchBytes(`${base}/devcontainer-x64-layer.tar.gz`);
      const image = new wasm.DevcontainerImage();
      image.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
      const rootfs = image.assemble_bootable(128 * 1024 * 1024);
      ws = wasm.X64Workspace.boot_devcontainer(kernel, rootfs);
      bridged = false;
      out && out.appendLine("holospace: booted a blank amd64 devcontainer on the x64 core from the bundled layer (no router)");
    }
  } else if (arch === "aarch64") {
    // aarch64 holospace: boot the provisioned arm64 image on the AArch64 core,
    // paged from OPFS (CC-37/CC-46) — a real arm64 devcontainer with the same
    // full device surface (terminal, virtio-9p workspace, router-backed network).
    if (holoId) {
      const rootfsHandle = await openProvisionedHandle(holoId);
      if (rootfsHandle) {
        const diskHandle = await openDiskStore(holoId);
        if (diskHandle) {
          ws = wasm.Aarch64Workspace.boot_devcontainer_opfs_streamed(kernel, rootfsHandle, diskHandle);
          bridged = false;
          out && out.appendLine("holospace: booted the provisioned arm64 image on the AArch64 core (CC-37) — paged from OPFS");
        } else {
          try { rootfsHandle.close(); } catch {}
        }
      }
    }
    if (!ws) {
      // BLANK / no-provisioning fallback: boot the BUNDLED arm64 busybox layer
      // in-RAM — no registry pull, no router (parity with the x64 + riscv64
      // blank paths).
      const layer = await fetchBytes(`${base}/devcontainer-arm64-layer.tar.gz`);
      const image = new wasm.DevcontainerImage();
      image.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
      const rootfs = image.assemble_bootable(128 * 1024 * 1024);
      ws = wasm.Aarch64Workspace.boot_devcontainer(kernel, rootfs);
      bridged = false;
      out && out.appendLine("holospace: booted a blank arm64 devcontainer on the AArch64 core from the bundled layer (no router)");
    }
  } else {
  // PREFERRED: the streaming **paged κ-disk**. Page the provisioned rootfs
  // straight from its OPFS file into an OPFS-backed κ-store, sector-by-sector —
  // neither the full image nor the assembled disk is ever held in wasm RAM
  // ("the KappaStore IS the memory, RAM is a cache"), so a large image boots
  // without OOM. Needs sync access handles (worker-only — which is where this
  // runs) on both the rootfs and the store pack, and no egress-node override.
  if (holoId && !egress) {
    const rootfsHandle = await openProvisionedHandle(holoId);
    if (rootfsHandle) {
      const diskHandle = await openDiskStore(holoId);
      if (diskHandle) {
        ws = wasm.Workspace.boot_devcontainer_routed_opfs_streamed(kernel, rootfsHandle, diskHandle);
        bridged = false;
        out && out.appendLine("holospace: booted the provisioned image (CC-42) — streamed paged κ-disk from OPFS (no full image in RAM)");
      } else {
        try { rootfsHandle.close(); } catch {}
      }
    }
  }

  // FALLBACK: read the rootfs into RAM and boot the in-RAM / node-egress path —
  // an egress-node override, or OPFS sync access unavailable, or no provisioned
  // image (the workbench-machinery tests open the workbench directly with no
  // Manager → the language-server base fixture; a real no-router launch is gated
  // in the Manager, so a user never sees the fixture in place of their repo).
  if (!ws) {
    let rootfs = holoId ? await readProvisioned(holoId) : null;
    const provisioned = !!rootfs;
    if (!rootfs) {
      const layer = await fetchBytes(`${base}/devcontainer-lsp-layer.tar.gz`);
      const image = new wasm.DevcontainerImage();
      image.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
      const DISK_BYTES = 128 * 1024 * 1024;
      rootfs = image.assemble_bootable(DISK_BYTES);
    }
    bridged = !egress && !provisioned;
    const diskHandle = provisioned && !egress ? await openDiskStore(holoId) : null;
    ws = egress
      ? wasm.Workspace.boot_devcontainer_net(kernel, rootfs, egress)
      : provisioned
        ? diskHandle
          ? wasm.Workspace.boot_devcontainer_routed_opfs(kernel, rootfs, diskHandle)
          : wasm.Workspace.boot_devcontainer_routed(kernel, rootfs)
        : wasm.Workspace.boot_devcontainer_bridged(kernel, rootfs);
    if (out) {
      out.appendLine(provisioned
        ? (diskHandle ? "holospace: booted the provisioned image (CC-42) — disk paged from OPFS"
                      : "holospace: booted the provisioned image (CC-42)")
        : "holospace: booted the language-server base fixture");
    }
  }
  } // end the riscv64 boot branch

  // Seed the shared workspace (the editor + the OS both see these over
  // virtio-9p, CC-15 — every core exposes the same ws_* surface; the guards are
  // pure capability detection). A REPOSITORY holospace gets a clone-ready
  // welcome (its content is the repo's, not ours); a blank/demo one gets the
  // scratchpad sample (a source file, a tasks.json, a devcontainer.json) so
  // there is something to edit, run, and search immediately.
  const repoUrl = query.get("repo");
  if (ws && typeof ws.ws_write === "function" && repoUrl) {
    ws.ws_write(
      "WELCOME.md",
      new TextEncoder().encode(
        `# ${(repoUrl.replace(/\/+$/, "").match(/([^/]+?)(?:\.git)?$/) || [])[1] || "holospace"}\n\n` +
          `This holospace runs the devcontainer of ${repoUrl}.\n\n` +
          "Clone the repository into the shared workspace from the terminal:\n\n" +
          "```sh\ngit clone " + repoUrl + " .\n```\n\n" +
          "The editor and the devcontainer share this workspace over virtio-9p (CC-15) — " +
          "files, tasks (Run Task), search, and source control all operate on the same content.\n",
      ),
    );
  } else if (ws && typeof ws.ws_write === "function") {
    ws.ws_write(
      "WELCOME.md",
      new TextEncoder().encode(
        "# holospace\n\nThe real VS Code workbench, over the running devcontainer.\n" +
          "This file lives on the virtio-9p workspace (CC-15) — the terminal sees it too.\n" +
          "Open `main.rs` — language intelligence comes from a server in the OS over the substrate bridge (CC-18/CC-33).\n",
      ),
    );
    // Seed a source file the language server can analyze (the editor + OS share it).
    ws.ws_write(
      "main.rs",
      new TextEncoder().encode("fn greet(name) {\n  // TODO: greet\n  return greet(name)\n}\n"),
    );
    // Seed a sample `.vscode/tasks.json` (CC-53) — the holospace-tasks provider
    // runs these in the devcontainer over 9p. A nested path, so use the
    // nested-path API (the riscv64 Workspace has it; the guard above covers the
    // cores that do not).
    if (typeof ws.ws_write_path === "function") {
      ws.ws_write_path(
        ".vscode/tasks.json",
        new TextEncoder().encode(
          JSON.stringify(
            {
              version: "2.0.0",
              tasks: [
                {
                  // The default build task (Ctrl+Shift+B): emits a problem line
                  // (the matcher turns it into a diagnostic), some output, and a
                  // non-zero exit — so output, exit status, and problem matchers
                  // are all exercised by one run in the devcontainer.
                  label: "build",
                  type: "holospace",
                  command: "echo 'main.rs:2:5: warning: TODO found here'; echo 'build complete'; exit 2",
                  group: { kind: "build", isDefault: true },
                  problemMatcher: "$holospace-generic",
                },
                { label: "hello", type: "holospace", command: "echo 'Hello from the devcontainer'" },
                {
                  // A background/watch task: long-running (a watcher loop), with
                  // begin/end patterns so the workbench shows the spinner while a
                  // "build cycle" is active — and the UI stays usable meanwhile.
                  label: "watch",
                  type: "holospace",
                  isBackground: true,
                  command: "while true; do echo BUILD-START; sleep 3; echo BUILD-DONE; sleep 3; done",
                  problemMatcher: "$holospace-watch",
                },
              ],
            },
            null,
            2,
          ) + "\n",
        ),
      );
      // Seed a sample `.devcontainer/devcontainer.json` (CC-22/CC-53) — the
      // holospace-tasks provider surfaces its lifecycle commands as re-runnable
      // tasks, exactly as it would for any repository's devcontainer config.
      ws.ws_write_path(
        ".devcontainer/devcontainer.json",
        new TextEncoder().encode(
          JSON.stringify(
            {
              name: "holospace demo",
              postCreateCommand: "echo 'lifecycle: environment prepared'",
            },
            null,
            2,
          ) + "\n",
        ),
      );
    }
  }
}

// ── FileSystemProvider over the virtio-9p workspace (CC-15) ─────────────────
const { FileType, FileSystemError, EventEmitter } = vscode;

// The workspace-relative POSIX path of a `holospace://…/workspace/<path>` URI —
// the share-relative path the nested 9p API addresses (`.git/HEAD`,
// `src/main.rs`). Already nested (only the `workspace/` prefix is stripped).
function nameOf(uri) {
  const p = uri.path.replace(/^\/+/, "");
  return p.replace(/^workspace\/?/, "");
}

// Whether the booted core exposes the virtio-9p workspace with the nested-path
// API (`CC-15`/`CC-51`). The riscv64 Workspace does; the aarch64 / x86-64
// terminal cores do not yet (their workspace is empty until 9p parity lands) —
// the editor then reflects the real, empty state, never fakes it.
function has9p() {
  return ws && typeof ws.ws_read_path === "function";
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
    if (!has9p()) throw FileSystemError.FileNotFound(uri);
    const s = ws.ws_stat_path(name);
    if (s == null) throw FileSystemError.FileNotFound(uri);
    const { dir, size } = JSON.parse(s);
    return {
      type: dir ? FileType.Directory : FileType.File,
      ctime: 0,
      mtime: Date.now(),
      size,
    };
  }
  // Read a directory by its full nested path — so the explorer shows the real
  // repository tree (`src/…`, the `.git` directory), not only the root.
  async readDirectory(uri) {
    await readyPromise;
    if (!has9p()) return []; // a core without the 9p workspace yet
    const json = ws.ws_list_path(nameOf(uri));
    if (json == null) return [];
    return JSON.parse(json).map((e) => [e.name, e.dir ? FileType.Directory : FileType.File]);
  }
  async readFile(uri) {
    await readyPromise;
    if (!has9p()) throw FileSystemError.FileNotFound(uri);
    const bytes = ws.ws_read_path(nameOf(uri));
    if (bytes == null) throw FileSystemError.FileNotFound(uri);
    return bytes;
  }
  async writeFile(uri, content) {
    await readyPromise;
    if (!has9p()) throw FileSystemError.NoPermissions(uri);
    ws.ws_write_path(nameOf(uri), content);
    this._emitter.fire([{ type: vscode.FileChangeType.Changed, uri }]);
  }
  // The mutating operations are the host-side duals of the 9P backend's
  // Tmkdir / Tunlinkat / Trenameat — the editor (and the Source Control Git
  // engine) changing the *same* nested workspace content the running OS sees
  // over virtio-9p (one content, Law L1).
  async createDirectory(uri) {
    await readyPromise;
    if (!has9p()) throw FileSystemError.NoPermissions(uri);
    ws.ws_mkdir_path(nameOf(uri));
    this._emitter.fire([{ type: vscode.FileChangeType.Created, uri }]);
  }
  async delete(uri) {
    await readyPromise;
    if (!has9p()) throw FileSystemError.FileNotFound(uri);
    if (!ws.ws_delete_path(nameOf(uri))) throw FileSystemError.FileNotFound(uri);
    this._emitter.fire([{ type: vscode.FileChangeType.Deleted, uri }]);
  }
  async rename(oldUri, newUri) {
    await readyPromise;
    if (!has9p()) throw FileSystemError.FileNotFound(oldUri);
    if (!ws.ws_rename_path(nameOf(oldUri), nameOf(newUri))) {
      throw FileSystemError.FileNotFound(oldUri);
    }
    this._emitter.fire([
      { type: vscode.FileChangeType.Deleted, uri: oldUri },
      { type: vscode.FileChangeType.Created, uri: newUri },
    ]);
  }
}

// ── The integrated terminal over the OS console (CC-11) ─────────────────────
// A *real* terminal over the devcontainer OS console (CC-11). The OS console is
// already a proper tty: the guest echoes typed characters, the shell does its own
// line editing (backspace, history, arrows), and Ctrl-C raises SIGINT — so the
// terminal must get out of the way and pass *raw bytes both directions*:
//   • input  — every keystroke (incl. control bytes 0x03/0x04 and arrow escapes,
//     and xterm's own replies like the `\x1b[6n` cursor-position report the
//     shell's line editor asks for) goes straight to the guest via `feed_input`;
//   • output — `terminal_delta()` returns only the newly-produced bytes since the
//     last frame (O(new), not O(total) like re-reading the whole buffer), written
//     verbatim — the guest's tty already emits CRLF (ONLCR), so we do not re-wrap.
// No JS line buffer, no local echo: the OS owns the line discipline, as a Codespace
// terminal's remote does.
const encoder = new TextEncoder();
const decoder = new TextDecoder();
function makeTerminal() {
  const writeEmitter = new EventEmitter();
  let running = true;
  const pump = () => {
    if (!ws || !running) return;
    if (!ws.halted) ws.run(8_000_000);
    const delta = ws.terminal_delta(); // only the bytes produced since last frame
    if (delta.length) writeEmitter.fire(decoder.decode(delta));
    setTimeout(pump, 40);
  };
  const pty = {
    onDidWrite: writeEmitter.event,
    open: async () => {
      writeEmitter.fire(
        resumed ? "holospace — resuming your devcontainer…\r\n" : "holospace — booting the devcontainer OS…\r\n",
      );
      await readyPromise;
      if (bootError) {
        writeEmitter.fire("boot failed: " + bootError.replace(/\n/g, "\r\n") + "\r\n");
        return;
      }
      // Resume the *visible* session: the machine snapshot is κ-pure (no console
      // output buffer), so replay the scrollback persisted beside it — the user
      // comes back to the prompt they left, not a blank screen. Consumed once.
      if (resumeScrollback && resumeScrollback.length) {
        writeEmitter.fire(decoder.decode(resumeScrollback));
        resumeScrollback = null;
      }
      pump();
    },
    close: () => {
      running = false;
    },
    // Raw input: the bytes the user typed go straight to the guest console; the OS
    // tty echoes and edits them, and Ctrl-C (0x03) reaches the foreground process
    // as SIGINT. This also carries xterm's automatic replies to terminal queries
    // (e.g. the cursor-position report), which the shell's line editor relies on.
    handleInput: (data) => {
      if (ws) ws.feed_input(encoder.encode(data));
    },
  };
  return vscode.window.createTerminal({ name: "holospace", pty });
}

// ── Language intelligence over the in-process bridge (CC-18 deployed; ADR-020) ──
// The devcontainer OS runs a language server (`/usr/bin/lsp-demo --listen 7000`);
// this connects an LSP client to it over the in-process substrate bridge
// (`Workspace.dial_guest` → `guest_send`/`guest_recv`, CC-33) and wires its
// hovers + diagnostics into the editor through the published `vscode.languages`
// API. The editor's language intelligence comes from a server *in the OS* — the
// VS Code remote model (ADR-015), in the browser tab, with no Node extension host.
function findBytes(buf, needle) {
  outer: for (let i = 0; i + needle.length <= buf.length; i++) {
    for (let j = 0; j < needle.length; j++) if (buf[i + j] !== needle[j]) continue outer;
    return i;
  }
  return -1;
}
const HDR_SEP = new TextEncoder().encode("\r\n\r\n");

function startLanguageClient(context, out) {
  // The in-OS language server is reached over the in-process loopback bridge
  // (CC-33), a riscv64 Workspace capability; the aarch64 terminal core has no
  // loopback yet (the continued build), so skip the language client there.
  if (!ws || typeof ws.dial_guest !== "function") {
    out && out.appendLine("holospace: language client skipped (no in-OS bridge on this core yet)");
    return;
  }
  const PORT = 7000;
  const diagnostics = vscode.languages.createDiagnosticCollection("holospace");
  context.subscriptions.push(diagnostics);
  const lspUri = (uri) => uri.toString();
  let connId = null;
  let nextId = 1;
  let initialized = false;
  let inbuf = new Uint8Array(0);
  const pending = new Map();

  const send = (msg) => {
    if (connId == null) return;
    const body = encoder.encode(JSON.stringify(msg));
    const header = encoder.encode(`Content-Length: ${body.length}\r\n\r\n`);
    const frame = new Uint8Array(header.length + body.length);
    frame.set(header, 0);
    frame.set(body, header.length);
    ws.guest_send(connId, frame);
  };
  const request = (method, params) =>
    new Promise((resolve) => {
      const id = nextId++;
      pending.set(id, resolve);
      send({ jsonrpc: "2.0", id, method, params });
    });
  const notify = (method, params) => send({ jsonrpc: "2.0", method, params });

  const dispatch = (msg) => {
    if (msg.id != null && pending.has(msg.id)) {
      pending.get(msg.id)(msg.result);
      pending.delete(msg.id);
    } else if (msg.method === "textDocument/publishDiagnostics" && msg.params) {
      const p = msg.params;
      const list = (p.diagnostics || []).map((d) => {
        const r = new vscode.Range(
          d.range.start.line,
          d.range.start.character,
          d.range.end.line,
          d.range.end.character,
        );
        const sev = d.severity === 1 ? vscode.DiagnosticSeverity.Error : vscode.DiagnosticSeverity.Warning;
        return new vscode.Diagnostic(r, d.message, sev);
      });
      try {
        diagnostics.set(vscode.Uri.parse(p.uri), list);
      } catch {
        /* a uri the editor cannot parse — skip */
      }
    }
  };

  // Drain the server's reply bytes and parse complete `Content-Length` frames.
  const drain = () => {
    if (connId == null) return;
    const bytes = ws.guest_recv(connId);
    if (!bytes.length) return;
    const merged = new Uint8Array(inbuf.length + bytes.length);
    merged.set(inbuf, 0);
    merged.set(bytes, inbuf.length);
    inbuf = merged;
    for (;;) {
      const hdrEnd = findBytes(inbuf, HDR_SEP);
      if (hdrEnd < 0) break;
      const header = decoder.decode(inbuf.subarray(0, hdrEnd));
      const m = /Content-Length:\s*(\d+)/i.exec(header);
      if (!m) break;
      const len = parseInt(m[1], 10);
      const start = hdrEnd + HDR_SEP.length;
      if (inbuf.length < start + len) break; // body not fully arrived
      const body = decoder.decode(inbuf.subarray(start, start + len));
      inbuf = inbuf.slice(start + len);
      try {
        dispatch(JSON.parse(body));
      } catch {
        /* malformed frame — skip */
      }
    }
  };

  // A machine pump independent of the terminal, so the OS (and its server) runs
  // even before a terminal is focused, and the bridge is drained continuously.
  let pumping = true;
  const pump = () => {
    if (!ws || !pumping) return;
    if (!ws.halted) ws.run(6_000_000);
    drain();
    setTimeout(pump, 25);
  };

  (async () => {
    // Wait until the in-OS server is listening, then dial it over the bridge.
    for (let i = 0; i < 600 && !(ws.shows && ws.shows("LSP-LISTENING")); i++) {
      if (!ws.halted) ws.run(6_000_000);
      await new Promise((r) => setTimeout(r, 25));
    }
    connId = ws.dial_guest(PORT);
    if (connId == null) {
      out.appendLine("holospace: LSP bridge unavailable (no networked devcontainer)");
      return;
    }
    pump();
    await request("initialize", { processId: null, rootUri: "file:///workspace", capabilities: {} });
    notify("initialized", {});
    initialized = true;
    out.appendLine("holospace: language server connected over the substrate bridge (CC-18/CC-33)");
    // A visible signal the language server is live over the bridge (also the
    // deterministic witness for the browser conformance test).
    const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
    status.text = "$(symbol-method) HOLOSPACE-LSP-LIVE";
    status.tooltip = "Language intelligence from a server in the devcontainer OS, over the substrate bridge (CC-18/CC-33)";
    status.show();
    context.subscriptions.push(status);

    const openDoc = (doc) => {
      if (doc.uri.scheme !== "holospace") return;
      notify("textDocument/didOpen", {
        textDocument: { uri: lspUri(doc.uri), languageId: doc.languageId || "plaintext", version: 1, text: doc.getText() },
      });
    };
    vscode.workspace.textDocuments.forEach(openDoc);
    context.subscriptions.push(vscode.workspace.onDidOpenTextDocument(openDoc));
    context.subscriptions.push(
      vscode.workspace.onDidChangeTextDocument((e) => {
        if (e.document.uri.scheme !== "holospace") return;
        notify("textDocument/didChange", {
          textDocument: { uri: lspUri(e.document.uri), version: e.document.version },
          contentChanges: [{ text: e.document.getText() }],
        });
      }),
    );
    context.subscriptions.push(
      vscode.languages.registerHoverProvider(
        { scheme: "holospace" },
        {
          async provideHover(doc, position) {
            if (!initialized) return null;
            const r = await request("textDocument/hover", {
              textDocument: { uri: lspUri(doc.uri) },
              position: { line: position.line, character: position.character },
            });
            const val = r && r.contents && (r.contents.value || (typeof r.contents === "string" ? r.contents : ""));
            return val ? new vscode.Hover(new vscode.MarkdownString(val)) : null;
          },
        },
      ),
    );
  })().catch((e) => out.appendLine("holospace: language client error — " + e));

  context.subscriptions.push(
    new vscode.Disposable(() => {
      pumping = false;
      if (connId != null) ws.guest_close(connId);
    }),
  );
}

// ── holospaces-as-remote: the substrate-native extension host (CC-48; ADR-020) ──
// ADR-020's resolved frontier: the extension host that activates ARBITRARY
// marketplace extensions is "holospaces' OWN, on the hologram substrate ... its
// VS Code + Node API surface backed by the holospace's own primitives" — NOT Node
// booted inside the emulated guest. The substrate's execution surface in the
// browser peer IS the workbench's extension host (the same process the Workspace
// wasm peer runs in, ADR-015's web-model refinement); that host runs on the
// substrate peer with NO Node on the host and NO deployment outside the holospace
// (Law L4). Over it an arbitrary Open VSX extension (CC-19) installs from the open
// gallery and ACTIVATES, its contribution observable in the real workbench — and
// its backends are the holospace's own: the filesystem (the `holospace:`
// FileSystemProvider over virtio-9p, CC-15), the terminal (the OS console, CC-11),
// and language intelligence from a server in the OS over the in-process substrate
// bridge (CC-18/CC-33), wired above. holospaces is the remote, in the tab, on the
// substrate (ADR-020) — the substrate-native ext host replaces the legacy Node
// `vscode-server` exactly as the substrate replaces the cloud VM.
//
// This confirms the substrate-native ext host is LIVE and bound to the holospace,
// then publishes `HOLOSPACE-REMOTE-LIVE` — the deterministic witness signal. It is
// surfaced ONLY when both hold (never inferred, AGENTS.md): (a) the extension host
// is running (this extension's own activate() ran in it — the host executes
// arbitrary extensions' code, the CC-48 capability); and (b) the holospace backs
// it — the workspace FileSystemProvider is registered AND the holospace booted
// (its workspace content is reachable), so the remote's backend is the holospace's
// own content (Law L1), not an empty shell.
function startRemoteExtensionHost(context, out) {
  (async () => {
    await readyPromise;
    if (bootError) {
      out && out.appendLine("holospace: remote ext host not live — the holospace did not boot (" + String(bootError).split("\n")[0] + ")");
      return;
    }
    // (b) The holospace backs the host: the booted holospace's own workspace is
    // reachable (virtio-9p, CC-15). We read the workspace listing the running OS
    // shares — the remote's filesystem backend is the holospace's content, not a
    // stand-in. (Every core now exposes the shared 9p workspace; the terminal
    // fallback below survives a core that has not booted a workspace at all.)
    const fsBacked = has9p() || (ws && typeof ws.terminal === "function");
    if (!fsBacked) {
      out && out.appendLine("holospace: remote ext host not live — no holospace primitive is backing it yet");
      return;
    }
    // (a) The substrate-native ext host is running: this code IS executing in it,
    // and the workbench has already activated this extension's contributions. The
    // host runs arbitrary extensions' code — the CC-48 capability, on the
    // substrate execution surface (ADR-020), with no Node on the host.
    out.appendLine(
      "holospace: holospaces-as-remote is LIVE — the substrate-native extension host runs on the " +
        "substrate execution surface (ADR-020), backed by the holospace's own filesystem (CC-15), " +
        "terminal (CC-11), and language intelligence over the substrate bridge (CC-18/CC-33). " +
        "Arbitrary Open VSX extensions activate here — no Node on the host, no deployment outside the holospace (L4).",
    );
    // The deterministic witness signal, visible in the real workbench: the
    // substrate-native remote ext host is live and bound to the holospace.
    const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
    status.text = "$(remote) HOLOSPACE-REMOTE-LIVE";
    status.tooltip = "The VS Code extension host runs on the hologram substrate (ADR-020), backed by the holospace's own primitives — no Node, no server elsewhere (CC-48)";
    status.show();
    context.subscriptions.push(status);
  })().catch((e) => out && out.appendLine("holospace: remote ext host error — " + e));
}

// CC-48 proper — the substrate-native Node-API extension host. Unlike
// `startRemoteExtensionHost` (which marks the host live once *this* extension runs
// + the holospace backs it), this genuinely loads an ARBITRARY Node-only Open VSX
// extension and runs its `activate()` on the browser peer's own JS engine (native
// exec, in the tab — not the emulated guest, not vscode-web's web host), with the
// Node API surface backed by the holospace's own filesystem (CC-15). It publishes
// HOLOSPACE-NODE-EXTHOST-LIVE only after a real activation — the CC-48 witness
// signal. The subject defaults to a Node-only extension and is overridable via the
// `holospace.cc48Extension` setting (the witness sets it).
// A genuinely Node-only Open VSX extension (package.json `main`, NO `browser`) as
// the default subject — it cannot run in vscode-web's web host, so activating it
// proves the substrate-native Node-API host did the work. Overridable via the
// `holospace.cc48Extension` setting (the witness sets/uses it).
const CC48_DEFAULT_EXT = "editorconfig.editorconfig";

// A thin recording proxy over the real `vscode` API: it forwards every call to the
// genuine workbench API (so contributions actually appear) while recording the
// extension's registrations (commands, status-bar items) — so the host can publish
// PROOF of the extension's own contribution, not merely that activate() returned.
function recordingVscode(real, rec) {
  const wrap = (ns, overrides) => new Proxy(ns, { get: (t, k) => (k in overrides ? overrides[k] : t[k]) });
  const commands = wrap(real.commands, {
    registerCommand: (id, fn, ...r) => { rec.commands.push(id); return real.commands.registerCommand(id, fn, ...r); },
  });
  const window = wrap(real.window, {
    createStatusBarItem: (...a) => { const it = real.window.createStatusBarItem(...a); rec.statusBar.push(it); return it; },
  });
  return new Proxy(real, { get: (t, k) => (k === "commands" ? commands : k === "window" ? window : t[k]) });
}

function holospaceFsAdapter() {
  // A Node `fs`-shaped adapter over the holospace's OWN filesystem (CC-15): every
  // path resolves under the booted workspace, routed through the registered
  // `holospace:` FileSystemProvider (virtio-9p). No host filesystem is touched.
  const root = (vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0])
    ? vscode.workspace.workspaceFolders[0].uri
    : vscode.Uri.parse("holospace:/workspace");
  const uri = (p) => root.with({ path: (root.path.replace(/\/+$/, "") + "/" + String(p).replace(/^\/+/, "")) });
  const fs = vscode.workspace.fs;
  return {
    readFile: async (p) => new Uint8Array(await fs.readFile(uri(p))),
    writeFile: async (p, bytes) => fs.writeFile(uri(p), bytes),
    readdir: async (p) => (await fs.readDirectory(uri(p))).map(([n]) => n),
    mkdir: async (p) => fs.createDirectory(uri(p)),
    rm: async (p) => fs.delete(uri(p), { recursive: true }),
    stat: async (p) => { const s = await fs.stat(uri(p)); return { isFile: () => (s.type & 1) !== 0, isDirectory: () => (s.type & 2) !== 0, size: s.size }; },
    exists: async (p) => { try { await fs.stat(uri(p)); return true; } catch { return false; } },
  };
}

function startNodeExtHost(context, out) {
  // Mirror every step to the output channel AND the page console (`[CC48] …`), so
  // the witness can see the bring-up chain and any failure reason.
  const dbg = (m) => { try { out && out.appendLine("holospace: " + m); } catch (_e) { /* */ } try { console.warn("[CC48] " + m); } catch (_e) { /* */ } };
  (async () => {
    await readyPromise;
    if (bootError) { dbg("CC-48 skipped — holospace did not boot"); return; }
    // Load the substrate-native ext-host runtime (a CommonJS module host + Node API
    // surface, pure-JS/browser-safe), served beside this extension. Try a relative
    // `require` first; fall back to fetching + evaluating the served source (the web
    // ext host does not always resolve relative requires).
    let host = null;
    try { const h = require("./node-exthost.js"); if (h && h.installFromOpenVsx) { host = h; dbg("ext-host runtime loaded via require()"); } } catch (_e) { /* fall through */ }
    if (!host) {
      try {
        const srcUri = vscode.Uri.joinPath(context.extensionUri, "node-exthost.js");
        dbg("loading ext-host runtime via fetch/fs; scheme=" + srcUri.scheme);
        let src;
        if (/^https?:$/.test(srcUri.scheme + ":")) {
          src = await (await fetch(srcUri.toString(true))).text();
        } else {
          src = new TextDecoder().decode(await vscode.workspace.fs.readFile(srcUri));
        }
        const mod = { exports: {} };
        new Function("module", "exports", "globalThis", src)(mod, mod.exports, globalThis);
        host = mod.exports;
        dbg("ext-host runtime loaded via " + srcUri.scheme + " (" + src.length + " bytes)");
      } catch (e) {
        dbg("CC-48 node ext host runtime unavailable — " + String(e && e.stack ? e.stack : e).split("\n").slice(0, 2).join(" | "));
        return;
      }
    }
    if (!host || !host.installFromOpenVsx) { dbg("CC-48 node ext host runtime missing installFromOpenVsx"); return; }
    const extId =
      (vscode.workspace.getConfiguration("holospace").get("cc48Extension")) || CC48_DEFAULT_EXT;
    dbg("bringing up the substrate-native Node-API ext host; installing " + extId + " from Open VSX…");
    try {
      const rec = { commands: [], statusBar: [] };
      const installed = await host.installFromOpenVsx({
        vscode: recordingVscode(vscode, rec),
        extId,
        fsAdapter: holospaceFsAdapter(),
      });
      dbg("installed " + extId + "@" + installed.version + " (" + installed.fileCount + " files); activate() returned");
      // Keep the activated extension's disposables tied to our lifetime.
      if (installed.context && Array.isArray(installed.context.subscriptions)) {
        for (const d of installed.context.subscriptions) if (d && d.dispose) context.subscriptions.push(d);
      }
      const contributed = rec.commands.length + rec.statusBar.length;
      const sample = rec.commands[0] || (rec.statusBar[0] && rec.statusBar[0].text) || "(activate ran)";
      out.appendLine(
        "holospace: HOLOSPACE-NODE-EXTHOST-LIVE — the Node-only extension " + extId + "@" + installed.version +
          " ACTIVATED in the substrate-native (wasm-exec) ext host and contributed " + contributed +
          " item(s) (e.g. " + sample + "), backed by the holospace's own filesystem (CC-15), terminal (CC-11), " +
          "network (CC-16) — no emulated-guest server, no Node on the host, no deployment outside the holospace (CC-48/L4).",
      );
      const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 98);
      status.text = "$(extensions) HOLOSPACE-NODE-EXTHOST-LIVE: " + extId;
      status.tooltip = "A Node-only marketplace extension runs in holospaces' substrate-native extension host (CC-48) — native exec, in the tab, no emulated guest, no host Node";
      status.show();
      context.subscriptions.push(status);
    } catch (e) {
      // Honest: the host is not live until a Node-only extension genuinely activates.
      dbg("CC-48 substrate-native ext host not live yet — " + String(e && e.stack ? e.stack : e).split("\n").slice(0, 3).join(" | "));
    }
  })().catch((e) => dbg("CC-48 node ext host error — " + String(e && e.stack ? e.stack : e).split("\n").slice(0, 3).join(" | ")));
}

function activate(context) {
  base = deriveBase(context.extensionUri);
  // This launch's holospace identity (its κ), carried in the workspace folder
  // authority. All durable per-instance state (the OPFS resume snapshot +
  // scrollback) is namespaced by it, so distinct holospaces never bleed (Law L1).
  holoKey = deriveHoloKey();
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
      // Real language intelligence: connect an LSP client to the language server
      // running in the devcontainer OS over the in-process substrate bridge
      // (ADR-020/CC-33). The editor's hovers + diagnostics come from a server in
      // the OS — the VS Code remote model, in the browser tab, no Node.
      if (bridged) {
        startLanguageClient(context, out);
      }
      // holospaces-as-remote (CC-48; ADR-020): the substrate-native extension host
      // is live on the substrate execution surface (the browser ext host this code
      // runs in), backed by the holospace's own primitives. Surfaces
      // HOLOSPACE-REMOTE-LIVE once the host is confirmed running and holospace-bound
      // — for EVERY booted core, not only the bridged one (the ext host is the same
      // substrate surface regardless of which guest backs the filesystem).
      startRemoteExtensionHost(context, out);
      // CC-48 proper: bring up the substrate-native NODE-API extension host and
      // genuinely install + activate an arbitrary Node-only Open VSX extension in
      // it. Publishes HOLOSPACE-NODE-EXTHOST-LIVE only on a real activation.
      startNodeExtHost(context, out);
      // Persist the running machine to OPFS periodically (CC-30/CC-31), so the
      // next launch resumes from it instead of cold-booting. The extension host
      // is a worker (no `document` visibility events), so a timer is the portable
      // suspend trigger; `saveSnapshot` no-ops while a previous save is in flight.
      // Skipped for the bridged (networked) devcontainer: its snapshot does not
      // yet carry the virtio-net device + live connection state, so it cold-boots
      // each session (a documented frontier; the non-networked resume — CC-30/31 —
      // stays witnessed).
      if (!bridged) {
        const timer = setInterval(saveSnapshot, 120000);
        context.subscriptions.push(new vscode.Disposable(() => clearInterval(timer)));
      }
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
// Exposed for the keying witness (snapshot-keying-test): the pure identity→OPFS
// mapping that keeps distinct holospaces from sharing a slot (CC-31 regression).
module.exports._keying = { sanitizeHoloKey, namesFor };
