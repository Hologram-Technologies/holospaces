// holospace-scm — Source Control (Git) for a holospace (CC-51).
//
// A real VS Code `SourceControl` provider whose engine is a real Git
// implementation (isomorphic-git, vendored + κ-pinned) running as NATIVE exec on
// the browser peer's own JS engine — the CC-48 discipline (heavy in-tab work is
// native peer exec, never the emulated guest) — over the holospace's OWN
// virtio-9p workspace (CC-15), reached through the `holospace://` FileSystemProvider
// that holospace-fs registers. Because `.git` is the ONE shared content (Law L1),
// this is the *same* repository the guest's `git` reads: a commit made here is a
// real Git commit in the guest's `git log`. No server outside the holospace (L4).
//
// Capabilities (Codespaces/Gitpod SCM parity): working-tree status
// (modified/added/deleted/untracked), quick-diff gutter + the diff editor vs HEAD,
// stage/unstage, commit, the branch indicator + switch/create, and push/pull over
// the browser peer's network with the operator's auth (CC-24). It degrades
// honestly: no holospace workspace (a core without 9p yet) → no provider, never a
// fake one.
"use strict";
const vscode = require("vscode");

// The vendored Git engine (UMD bundles), κ-pinned (Law L5 at the import boundary).
const GIT_PINS = {
  "vendor/isomorphic-git/index.umd.min.js":
    "4377c9fd608ecea01782ae1bd3bf7cb15121b7c6069af046a6431b2561e682e7",
  "vendor/isomorphic-git/http-web.umd.js":
    "5c10c8754d36b19c5c0bbfc3d087c0d92b4a1fe97b46ab0c5b88f063f11ccdc6",
};

let git = null; // the isomorphic-git API
let gitHttp = null; // its web smart-http client (push/pull)
let out = null;

function log(msg) {
  // A console line the witness keys on for the host's bring-up diagnostics.
  console.log("[CC51] " + msg);
  if (out) out.appendLine("holospace-scm: " + msg);
}

const enc = new TextEncoder();
const dec = new TextDecoder();
const toHex = (buf) =>
  Array.from(new Uint8Array(buf)).map((b) => b.toString(16).padStart(2, "0")).join("");

// Evaluate a UMD bundle into its CommonJS export object, no bundler, no global
// pollution — the same dependency-free module evaluation the substrate-native ext
// host (CC-48) uses. The bundle is κ-verified (re-derived to its pinned sha256)
// before it runs (Law L5) — an imported external artifact, content-addressed.
async function loadUmd(extBase, rel) {
  const res = await fetch(`${extBase}/${rel}`);
  if (!res.ok) throw new Error(`fetch ${rel}: ${res.status}`);
  const bytes = new Uint8Array(await res.arrayBuffer());
  const got = toHex(await crypto.subtle.digest("SHA-256", bytes));
  if (got !== GIT_PINS[rel]) {
    throw new Error(`${rel} failed κ-verification (got ${got}, want ${GIT_PINS[rel]})`);
  }
  const module = { exports: {} };
  // The UMD factory takes the CommonJS `exports` when `module` is present.
  new Function("module", "exports", dec.decode(bytes))(module, module.exports);
  return module.exports;
}

async function loadGit(extBase) {
  if (git) return;
  git = await loadUmd(extBase, "vendor/isomorphic-git/index.umd.min.js");
  const http = await loadUmd(extBase, "vendor/isomorphic-git/http-web.umd.js");
  gitHttp = http.default || http;
  log(`Git engine loaded (isomorphic-git; ${Object.keys(git).length} ops), κ-verified`);
}

// ── The Git filesystem, backed by the holospace's own workspace ──────────────
// isomorphic-git operates over a Node-`fs`-shaped adapter; this one routes every
// read and write through `vscode.workspace.fs` to the `holospace://` provider —
// i.e. over virtio-9p into the running holospace (CC-15). The `.git` object tree
// the Git engine writes is the same content the guest's git reads (Law L1).
function makeGitFs(rootUri) {
  const uriOf = (p) => {
    const rel = String(p).replace(/^\/+/, "");
    return rel ? vscode.Uri.joinPath(rootUri, ...rel.split("/")) : rootUri;
  };
  // Translate a VS Code FileSystemError into the POSIX `err.code` isomorphic-git
  // branches on (it checks `err.code === "ENOENT"` etc.), so missing files read
  // as ENOENT instead of an opaque throw.
  const posix = (e, code) => {
    const err = new Error((code || "EIO") + ": " + (e && e.message ? e.message : String(e)));
    err.code = code || "EIO";
    return err;
  };
  const isMissing = (e) => e && (e.code === "FileNotFound" || /entry not found|ENOENT/i.test(e.message || ""));

  const promises = {
    async readFile(p, opts) {
      let bytes;
      try {
        bytes = await vscode.workspace.fs.readFile(uriOf(p));
      } catch (e) {
        throw isMissing(e) ? posix(e, "ENOENT") : posix(e, "EIO");
      }
      const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      const encoding = typeof opts === "string" ? opts : opts && opts.encoding;
      return encoding ? dec.decode(u8) : u8;
    },
    async writeFile(p, data) {
      const u8 = typeof data === "string" ? enc.encode(data) : data instanceof Uint8Array ? data : new Uint8Array(data);
      await vscode.workspace.fs.writeFile(uriOf(p), u8);
    },
    async unlink(p) {
      try {
        await vscode.workspace.fs.delete(uriOf(p), { recursive: false });
      } catch (e) {
        throw isMissing(e) ? posix(e, "ENOENT") : posix(e, "EIO");
      }
    },
    async readdir(p) {
      let entries;
      try {
        entries = await vscode.workspace.fs.readDirectory(uriOf(p));
      } catch (e) {
        throw isMissing(e) ? posix(e, "ENOENT") : posix(e, "ENOTDIR");
      }
      return entries.map(([name]) => name);
    },
    async mkdir(p) {
      await vscode.workspace.fs.createDirectory(uriOf(p));
    },
    async rmdir(p) {
      try {
        await vscode.workspace.fs.delete(uriOf(p), { recursive: false });
      } catch (e) {
        throw isMissing(e) ? posix(e, "ENOENT") : posix(e, "ENOTEMPTY");
      }
    },
    async stat(p) {
      let s;
      try {
        s = await vscode.workspace.fs.stat(uriOf(p));
      } catch (e) {
        throw isMissing(e) ? posix(e, "ENOENT") : posix(e, "EIO");
      }
      const isDir = (s.type & vscode.FileType.Directory) !== 0;
      const isSym = (s.type & vscode.FileType.SymbolicLink) !== 0;
      const mode = isDir ? 0o40000 : 0o100644;
      return {
        type: isDir ? "dir" : "file",
        mode,
        size: s.size,
        ino: 0,
        mtimeMs: s.mtime || 0,
        ctimeMs: s.ctime || 0,
        uid: 0,
        gid: 0,
        dev: 1,
        isFile: () => !isDir && !isSym,
        isDirectory: () => isDir,
        isSymbolicLink: () => isSym,
      };
    },
    async lstat(p) {
      return promises.stat(p);
    },
    async rename(from, to) {
      await vscode.workspace.fs.rename(uriOf(from), uriOf(to), { overwrite: true });
    },
    async readlink(p) {
      throw posix(new Error("symlinks unsupported"), "ENOENT");
    },
    async symlink() {
      throw posix(new Error("symlinks unsupported"), "ENOSYS");
    },
    async chmod() {
      /* mode bits are not represented on the 9p share; no-op */
    },
  };
  return { promises };
}

// ── Status mapping ──────────────────────────────────────────────────────────
// isomorphic-git's statusMatrix row is [filepath, headStatus, workdirStatus,
// stageStatus] with 0/1/2/3 codes. A file has an UNSTAGED change when workdir !=
// stage, and a STAGED change when stage != head.
function unstagedLetter(head, workdir, stage) {
  if (workdir === 0) return "D"; // present in index/head, gone from the working tree
  if (head === 0 && stage === 0) return "U"; // untracked
  return "M";
}
function stagedLetter(head, workdir, stage) {
  if (head === 0) return "A"; // newly added to the index
  if (stage === 0) return "D"; // deletion staged
  return "M";
}
const LETTER_TIP = { M: "Modified", A: "Added", D: "Deleted", U: "Untracked" };
const LETTER_COLOR = {
  M: "gitDecoration.modifiedResourceForeground",
  A: "gitDecoration.addedResourceForeground",
  D: "gitDecoration.deletedResourceForeground",
  U: "gitDecoration.untrackedResourceForeground",
};

function activate(context) {
  out = vscode.window.createOutputChannel("Holospace SCM");
  context.subscriptions.push(out);
  // The deploy serves this extension's assets at its own URI; the vendored Git
  // engine sits beside this file.
  const extBase = context.extensionUri.toString().replace(/\/+$/, "");

  // The holospace workspace folder (the `holospace://…/workspace` root the
  // workbench opened). Without it — a core that has no 9p workspace yet — there
  // is no repository surface, so we register no provider (honest, ADR-015).
  const folder = (vscode.workspace.workspaceFolders || []).find((f) => f.uri.scheme === "holospace");
  if (!folder) {
    log("no holospace workspace folder — Source Control not available on this core");
    return;
  }
  const rootUri = folder.uri;
  const dir = "/"; // the Git engine's working dir == the share root; gitdir == /.git
  const fs = makeGitFs(rootUri);

  // A status-bar marker the deployed UI shows and the conformance witness keys on
  // (the deterministic, always-in-the-DOM signal — like CC-18's HOLOSPACE-LSP-LIVE).
  const marker = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  context.subscriptions.push(marker);
  const setMarker = (text, tip) => {
    marker.text = text;
    marker.tooltip = tip || text;
    marker.show();
  };

  const scm = vscode.scm.createSourceControl("holospace-git", "Git (holospace)", rootUri);
  context.subscriptions.push(scm);
  const indexGroup = scm.createResourceGroup("index", "Staged Changes");
  const workingGroup = scm.createResourceGroup("working", "Changes");
  indexGroup.hideWhenEmpty = true;
  scm.inputBox.placeholder = "Message (Ctrl+Enter to commit)";
  scm.acceptInputCommand = { command: "holospaceGit.commit", title: "Commit" };
  // The quick-diff base: VS Code asks for the "original" of a working file and
  // renders the gutter change bars + the inline diff against it.
  scm.quickDiffProvider = {
    provideOriginalResource(uri) {
      if (uri.scheme !== "holospace") return undefined;
      return uri.with({ scheme: "holospace-git-head" });
    },
  };

  // The HEAD version of a tracked file — the diff base. Read the blob at HEAD
  // from the real object store (the Git engine resolves HEAD → tree → blob); an
  // untracked/added file has no HEAD version, so it diffs against empty.
  const relOf = (uri) => uri.path.replace(/^\/+/, "").replace(/^workspace\/?/, "");
  const headProvider = {
    async provideTextDocumentContent(uri) {
      try {
        const oid = await git.resolveRef({ fs, dir, ref: "HEAD" });
        const { blob } = await git.readBlob({ fs, dir, oid, filepath: relOf(uri) });
        return dec.decode(blob);
      } catch {
        return ""; // no HEAD yet, or the path is not in HEAD (added)
      }
    },
  };
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider("holospace-git-head", headProvider),
  );

  // File decorations — the M/A/D/U badges + colours on changed files, in the SCM
  // view and the explorer (the same surface the built-in Git uses).
  const decorations = new Map(); // uri.toString() -> { badge, color, tooltip, strikeThrough }
  const decoEmitter = new vscode.EventEmitter();
  const decoProvider = {
    onDidChangeFileDecorations: decoEmitter.event,
    provideFileDecoration(uri) {
      const d = decorations.get(uri.toString());
      if (!d) return undefined;
      return {
        badge: d.badge,
        color: new vscode.ThemeColor(d.color),
        tooltip: d.tooltip,
        propagate: false,
      };
    },
  };
  context.subscriptions.push(vscode.window.registerFileDecorationProvider(decoProvider));

  const resourceState = (uri, letter) => ({
    resourceUri: uri,
    decorations: {
      tooltip: LETTER_TIP[letter],
      strikeThrough: letter === "D",
      faded: false,
    },
    command: {
      command: "holospaceGit.openChange",
      title: "Open Changes",
      arguments: [uri],
    },
  });

  let branch = null;
  let isRepo = false;

  async function detectRepo() {
    try {
      await vscode.workspace.fs.stat(vscode.Uri.joinPath(rootUri, ".git"));
      isRepo = true;
    } catch {
      isRepo = false;
    }
    return isRepo;
  }

  async function updateBranch() {
    try {
      branch = (await git.currentBranch({ fs, dir, fullname: false })) || "(no branch)";
    } catch {
      branch = "(no branch)";
    }
    scm.statusBarCommands = [
      { command: "holospaceGit.checkout", title: `$(git-branch) ${branch}`, tooltip: "Switch branch" },
    ];
  }

  let refreshing = false;
  let refreshQueued = false;
  async function refresh() {
    if (refreshing) {
      refreshQueued = true;
      return;
    }
    refreshing = true;
    try {
      if (!(await detectRepo())) {
        workingGroup.resourceStates = [];
        indexGroup.resourceStates = [];
        scm.count = 0;
        decorations.clear();
        decoEmitter.fire(undefined);
        setMarker("$(source-control) HOLOSPACE-SCM-NOREPO", "No Git repository — Initialize Repository to start");
        return;
      }
      await updateBranch();
      const matrix = await git.statusMatrix({ fs, dir });
      const working = [];
      const staged = [];
      const nextDeco = new Map();
      for (const [filepath, head, workdir, stage] of matrix) {
        const uri = vscode.Uri.joinPath(rootUri, ...filepath.split("/"));
        if (workdir !== stage) {
          const letter = unstagedLetter(head, workdir, stage);
          working.push(resourceState(uri, letter));
          nextDeco.set(uri.toString(), {
            badge: letter,
            color: LETTER_COLOR[letter],
            tooltip: LETTER_TIP[letter],
          });
        }
        if (stage !== head) {
          const letter = stagedLetter(head, workdir, stage);
          staged.push(resourceState(uri, letter));
          // a staged-only change still wants a badge if not already set
          if (!nextDeco.has(uri.toString())) {
            nextDeco.set(uri.toString(), {
              badge: letter,
              color: LETTER_COLOR[letter],
              tooltip: LETTER_TIP[letter],
            });
          }
        }
      }
      workingGroup.resourceStates = working;
      indexGroup.resourceStates = staged;
      scm.count = working.length + staged.length;
      decorations.clear();
      for (const [k, v] of nextDeco) decorations.set(k, v);
      decoEmitter.fire(undefined);
      setMarker(
        `$(source-control) HOLOSPACE-SCM-LIVE branch=${branch} staged=${staged.length} changes=${working.length}`,
        "Source Control over the holospace's own virtio-9p workspace (CC-51)",
      );
    } catch (e) {
      log("refresh error: " + (e && e.message));
    } finally {
      refreshing = false;
      if (refreshQueued) {
        refreshQueued = false;
        refresh();
      }
    }
  }

  // The commit author — the operator's identity (CC-24): the repo's configured
  // user, else a holospace default so a first commit always succeeds.
  async function author() {
    let name = "holospace operator";
    let email = "operator@holospace.local";
    try {
      name = (await git.getConfig({ fs, dir, path: "user.name" })) || name;
      email = (await git.getConfig({ fs, dir, path: "user.email" })) || email;
    } catch {
      /* unconfigured — use the default */
    }
    return { name, email };
  }

  // Auth for push/pull (CC-24): the operator's stored credential, else a prompt.
  async function onAuth(url) {
    const stored = await context.secrets.get("holospaceGit.token");
    if (stored) return { username: stored, password: "x-oauth-basic" };
    const token = await vscode.window.showInputBox({
      prompt: `Git credential for ${url} (a token / password)`,
      password: true,
      ignoreFocusOut: true,
    });
    if (token) {
      await context.secrets.store("holospaceGit.token", token);
      return { username: token, password: "x-oauth-basic" };
    }
    return { cancel: true };
  }

  const relOfState = (arg) => {
    // A command argument is either a resource state (from the inline menu) or a Uri.
    const uri = arg && arg.resourceUri ? arg.resourceUri : arg;
    return uri ? relOf(uri) : null;
  };

  const cmd = (id, fn) =>
    context.subscriptions.push(vscode.commands.registerCommand(id, (...a) => Promise.resolve(fn(...a)).catch((e) => {
      log(id + " failed: " + (e && e.message));
      vscode.window.showErrorMessage(`holospace Git: ${e && e.message ? e.message : e}`);
    })));

  cmd("holospaceGit.refresh", () => refresh());

  cmd("holospaceGit.init", async () => {
    await git.init({ fs, dir, defaultBranch: "main" });
    log("initialized an empty Git repository (default branch main)");
    await refresh();
  });

  async function stagePath(filepath) {
    // Staging a deletion is a removal from the index; an add/modify is `add`.
    let exists = true;
    try {
      await vscode.workspace.fs.stat(vscode.Uri.joinPath(rootUri, ...filepath.split("/")));
    } catch {
      exists = false;
    }
    if (exists) await git.add({ fs, dir, filepath });
    else await git.remove({ fs, dir, filepath });
  }

  cmd("holospaceGit.stage", async (arg) => {
    const fp = relOfState(arg);
    if (fp) await stagePath(fp);
    await refresh();
  });
  cmd("holospaceGit.unstage", async (arg) => {
    const fp = relOfState(arg);
    if (fp) await git.resetIndex({ fs, dir, filepath: fp });
    await refresh();
  });
  cmd("holospaceGit.stageAll", async () => {
    for (const s of workingGroup.resourceStates) await stagePath(relOf(s.resourceUri));
    await refresh();
  });
  cmd("holospaceGit.unstageAll", async () => {
    for (const s of indexGroup.resourceStates) await git.resetIndex({ fs, dir, filepath: relOf(s.resourceUri) });
    await refresh();
  });

  cmd("holospaceGit.openChange", async (uri) => {
    if (!uri) return;
    const head = uri.with({ scheme: "holospace-git-head" });
    const name = relOf(uri).split("/").pop();
    await vscode.commands.executeCommand("vscode.diff", head, uri, `${name} (Working Tree)`);
  });

  cmd("holospaceGit.commit", async () => {
    if (!(await detectRepo())) {
      vscode.window.showWarningMessage("holospace Git: no repository — Initialize Repository first.");
      return;
    }
    const message = (scm.inputBox.value || "").trim();
    if (!message) {
      vscode.window.showWarningMessage("holospace Git: a commit message is required.");
      return;
    }
    // Smart commit: if nothing is staged but the working tree has changes, stage
    // them all (the built-in Git's default), so a commit is never silently empty.
    if (indexGroup.resourceStates.length === 0) {
      for (const s of workingGroup.resourceStates) await stagePath(relOf(s.resourceUri));
    }
    const sha = await git.commit({ fs, dir, message, author: await author() });
    scm.inputBox.value = "";
    log(`commit ${sha} — "${message.split("\n")[0]}"`);

    // Verify-by-re-derivation (Law L5) against the Git object-format authority:
    // read the commit object's bytes back from the store and recompute its κ
    // (sha1 of "commit <len>\0<content>"); it MUST equal the oid the engine
    // returned, and HEAD must point at it. Then publish the witnessed marker.
    const ok = await verifyCommit(sha, message);
    if (ok) {
      setMarker(`$(check) HOLOSPACE-SCM-COMMIT=${sha}`, `Committed ${sha} (re-derived, Law L5)`);
      log(`HOLOSPACE-SCM-VERIFIED=${sha} (object re-derives to its κ; HEAD points at it)`);
    }
    await refresh();
  });

  // Re-derive a commit oid from its stored object bytes (the Git object-format
  // authority): the loose object is zlib-deflated `commit <len>\0<content>`; its
  // sha1 IS the oid. We read it back through the Git engine's `readObject` (raw),
  // recompute the hash, and confirm HEAD resolves to it — independent of the
  // value the commit() call returned.
  async function verifyCommit(sha, message) {
    try {
      const head = await git.resolveRef({ fs, dir, ref: "HEAD" });
      if (head !== sha) {
        log(`verify FAILED: HEAD (${head}) != commit (${sha})`);
        return false;
      }
      const { commit } = await git.readCommit({ fs, dir, oid: sha });
      if (commit.message.trim() !== message.trim()) {
        log("verify FAILED: stored commit message mismatch");
        return false;
      }
      // Re-derive the oid from the canonical commit bytes (Law L5).
      const rederived = await rederiveCommitOid(commit);
      if (rederived !== sha) {
        log(`verify FAILED: re-derived oid ${rederived} != ${sha}`);
        return false;
      }
      return true;
    } catch (e) {
      log("verify error: " + (e && e.message));
      return false;
    }
  }

  // Serialize a commit to its canonical Git object form and hash it — the
  // content-address re-derivation (sha1("commit "+len+"\0"+content)).
  async function rederiveCommitOid(commit) {
    const lines = [];
    lines.push(`tree ${commit.tree}`);
    for (const p of commit.parent || []) lines.push(`parent ${p}`);
    const a = commit.author;
    lines.push(`author ${a.name} <${a.email}> ${a.timestamp} ${tzOffset(a.timezoneOffset)}`);
    const c = commit.committer;
    lines.push(`committer ${c.name} <${c.email}> ${c.timestamp} ${tzOffset(c.timezoneOffset)}`);
    if (commit.gpgsig) lines.push("gpgsig " + commit.gpgsig);
    const content = lines.join("\n") + "\n\n" + commit.message;
    const body = enc.encode(content);
    const header = enc.encode(`commit ${body.length}\0`);
    const full = new Uint8Array(header.length + body.length);
    full.set(header, 0);
    full.set(body, header.length);
    return toHex(await crypto.subtle.digest("SHA-1", full));
  }
  function tzOffset(minutes) {
    // isomorphic-git stores the offset in minutes (positive == west of UTC, the
    // negative of the `+HHMM` shown); reproduce Git's `±HHMM`.
    const sign = minutes <= 0 ? "+" : "-";
    const abs = Math.abs(minutes);
    const hh = String(Math.floor(abs / 60)).padStart(2, "0");
    const mm = String(abs % 60).padStart(2, "0");
    return `${sign}${hh}${mm}`;
  }

  cmd("holospaceGit.push", async () => {
    let remotes = await git.listRemotes({ fs, dir });
    if (!remotes.length) {
      // No remote yet — prompt for one (real git's "push with no upstream" flow),
      // wire it as `origin`, then push to it.
      const url = await vscode.window.showInputBox({
        prompt: "No remote configured — URL to push to",
        ignoreFocusOut: true,
      });
      if (!url) return;
      await git.addRemote({ fs, dir, remote: "origin", url, force: true });
      log(`added remote origin → ${url}`);
      remotes = await git.listRemotes({ fs, dir });
    }
    const ref = (await git.currentBranch({ fs, dir })) || "main";
    const remote = remotes[0].remote;
    log(`push ${ref} → ${remote} (${remotes[0].url})`);
    const result = await git.push({ fs, http: gitHttp, dir, remote, ref, onAuth, corsProxy: undefined });
    if (result && result.ok) {
      setMarker(`$(cloud-upload) HOLOSPACE-SCM-PUSH=${remote}/${ref}`, "Pushed to the remote (smart-http pack-protocol)");
      log(`HOLOSPACE-SCM-PUSH=${remote}/${ref} ok`);
      vscode.window.showInformationMessage(`holospace Git: pushed ${ref} to ${remote}.`);
    } else {
      throw new Error("push was rejected: " + JSON.stringify(result && result.error));
    }
    await refresh();
  });

  cmd("holospaceGit.pull", async () => {
    const remotes = await git.listRemotes({ fs, dir });
    if (!remotes.length) {
      vscode.window.showWarningMessage("holospace Git: no remote configured.");
      return;
    }
    const ref = (await git.currentBranch({ fs, dir })) || "main";
    log(`pull ${ref} ← ${remotes[0].remote}`);
    await git.pull({ fs, http: gitHttp, dir, ref, singleBranch: true, author: await author(), onAuth });
    setMarker(`$(cloud-download) HOLOSPACE-SCM-PULL=${remotes[0].remote}/${ref}`, "Pulled from the remote");
    await refresh();
  });

  cmd("holospaceGit.checkout", async () => {
    const branches = await git.listBranches({ fs, dir });
    const pick = await vscode.window.showQuickPick(branches, { placeHolder: "Select a branch to checkout" });
    if (!pick) return;
    await git.checkout({ fs, dir, ref: pick });
    await refresh();
  });

  cmd("holospaceGit.createBranch", async () => {
    const name = await vscode.window.showInputBox({ prompt: "New branch name" });
    if (!name) return;
    await git.branch({ fs, dir, ref: name, checkout: true });
    log(`created and checked out branch ${name}`);
    await refresh();
  });

  cmd("holospaceGit.addRemote", async (nameArg, urlArg) => {
    const remote = nameArg || (await vscode.window.showInputBox({ prompt: "Remote name", value: "origin" }));
    if (!remote) return;
    const url = urlArg || (await vscode.window.showInputBox({ prompt: `Remote URL for ${remote}` }));
    if (!url) return;
    await git.addRemote({ fs, dir, remote, url, force: true });
    log(`added remote ${remote} → ${url}`);
    setMarker(`$(repo) HOLOSPACE-SCM-REMOTE=${remote}`, `Remote ${remote} → ${url}`);
  });

  // Watch the WORKING TREE and refresh on change — but NOT the `.git` object
  // store. The broad `holospace:/**/*` glob also fires for `.git/**`, and the
  // FileSystemProvider emits a change for every write the Git engine makes there
  // (a commit/push writes hundreds of objects via `vscode.workspace.fs`), so an
  // unfiltered watcher would refresh-storm during our own operations. We instead
  // ignore `.git/**` here and watch the few git STATE files (HEAD / index /
  // refs) separately, so a ref change still refreshes the view without the
  // object-write storm.
  let debounce = null;
  const queueRefresh = () => {
    if (debounce) clearTimeout(debounce);
    debounce = setTimeout(() => refresh(), 400);
  };
  const isGitInternal = (uri) => /(?:^|\/)\.git\//.test(uri.path);
  const treeWatcher = vscode.workspace.createFileSystemWatcher("holospace:/**/*");
  context.subscriptions.push(treeWatcher);
  const onTreeChange = (uri) => { if (!isGitInternal(uri)) queueRefresh(); };
  treeWatcher.onDidChange(onTreeChange);
  treeWatcher.onDidCreate(onTreeChange);
  treeWatcher.onDidDelete(onTreeChange);
  const gitStateWatcher = vscode.workspace.createFileSystemWatcher("holospace:/**/.git/{HEAD,index,refs/**}");
  context.subscriptions.push(gitStateWatcher);
  gitStateWatcher.onDidChange(queueRefresh);
  gitStateWatcher.onDidCreate(queueRefresh);
  gitStateWatcher.onDidDelete(queueRefresh);
  context.subscriptions.push(vscode.workspace.onDidSaveTextDocument(queueRefresh));

  // Bring-up: load the κ-verified engine, then reflect the real repository.
  setMarker("$(sync~spin) holospace Source Control — starting…");
  (async () => {
    await loadGit(extBase);
    // The workspace files arrive over 9p once the holospace boots; the
    // FileSystemProvider awaits that readiness, so the first refresh blocks until
    // the real tree is present (a few retries cover a slow boot).
    for (let i = 0; i < 60; i++) {
      await refresh();
      if (isRepo || (await listedSomething())) break;
      await new Promise((r) => setTimeout(r, 1000));
    }
    log("Source Control ready");
  })().catch((e) => {
    log("startup error — " + (e && e.message));
    setMarker("$(error) holospace Source Control failed", String(e && e.message));
  });

  async function listedSomething() {
    try {
      const entries = await vscode.workspace.fs.readDirectory(rootUri);
      return entries.length > 0;
    } catch {
      return false;
    }
  }
}

function deactivate() {}

module.exports = { activate, deactivate };
