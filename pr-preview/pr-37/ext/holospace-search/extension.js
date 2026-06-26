// holospace-search — find-in-files + search & replace for a holospace (CC-52).
//
// Registers a FileSearchProvider + a TextSearchProvider for the `holospace://`
// scheme (the `fileSearchProvider` / `textSearchProvider` proposed APIs — a
// BUILTIN extension keeps its self-declared `enabledApiProposals`, so no
// product.json/CLI gate is needed). Both run as NATIVE exec on the browser peer
// (the CC-48 discipline, never the emulated guest), walking + reading the
// holospace's OWN virtio-9p workspace (CC-15) through `vscode.workspace.fs` — the
// same content the guest sees (Law L1). The engine (the tree walk + glob /
// `.gitignore` matching + text matching) is the shared, unit-witnessed
// `search-core.cjs`; `.gitignore` semantics come from the vendored, κ-pinned
// `ignore`. Replace-all is VS Code's own Search-view edit, applied through the
// holospace FileSystemProvider's `writeFile` (the edits land over 9p) — so an
// accurate `TextSearchMatch.ranges` is all this provider must supply.
//
// The web workbench has NO fallback search for a virtual scheme, so without these
// providers find-in-files is silently empty; with them it is real.
"use strict";
const vscode = require("vscode");

const SCHEME = "holospace";
// The vendored Git-ignore engine, κ-pinned (Law L5 at the import boundary).
const IGNORE_PIN = "aa6a35d736ce7c3555f5419d1e7bfc1caba034b4eb52661fc2a7c990402dfef7";

let core = null; // the search engine (search-core.cjs)
let ignoreFactory = null; // the `ignore` factory (vendored)
let out = null;

const dec = new TextDecoder();
const toHex = (buf) =>
  Array.from(new Uint8Array(buf)).map((b) => b.toString(16).padStart(2, "0")).join("");

function log(msg) {
  console.log("[CC52] " + msg);
  if (out) out.appendLine("holospace-search: " + msg);
}

// Evaluate a CommonJS module fetched from the extension's own served location
// (no bundler, no relative-require dependency) — the same dependency-free module
// evaluation the SCM engine uses. `pin`, when given, is the sha256 the bytes must
// re-derive to before they run (Law L5, for the vendored artifact).
async function loadCjs(url, pin) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`fetch ${url}: ${res.status}`);
  const bytes = new Uint8Array(await res.arrayBuffer());
  if (pin) {
    const got = toHex(await crypto.subtle.digest("SHA-256", bytes));
    if (got !== pin) throw new Error(`${url} failed κ-verification (got ${got}, want ${pin})`);
  }
  const module = { exports: {} };
  new Function("module", "exports", dec.decode(bytes))(module, module.exports);
  return module.exports;
}

async function loadEngine(extBase) {
  if (core) return;
  core = await loadCjs(`${extBase}/search-core.cjs`);
  ignoreFactory = await loadCjs(`${extBase}/vendor/ignore/index.cjs`, IGNORE_PIN);
  log("search engine loaded (walker + glob + .gitignore + text matcher), ignore κ-verified");
}

// Translate a VS Code FileType bitmask to "is a directory".
function isDirType(type) {
  return (type & vscode.FileType.Directory) !== 0 && (type & vscode.FileType.SymbolicLink) === 0;
}

// A root-relative filesystem over the holospace workspace folder (CC-15), the
// shape `search-core` walks: `list(relDir)` and `read(relFile)`.
function makeFs(folderUri) {
  const uriOf = (rel) => (rel ? vscode.Uri.joinPath(folderUri, ...rel.split("/")) : folderUri);
  return {
    async list(relDir) {
      const entries = await vscode.workspace.fs.readDirectory(uriOf(relDir));
      return entries.map(([name, type]) => ({ name, dir: isDirType(type) }));
    },
    async read(relFile) {
      const bytes = await vscode.workspace.fs.readFile(uriOf(relFile));
      const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      return u8;
    },
  };
}

// Read the workspace root `.gitignore` (the location VS Code's `useIgnoreFiles`
// honours), or "" if absent.
async function readGitignore(folderUri) {
  try {
    const bytes = await vscode.workspace.fs.readFile(vscode.Uri.joinPath(folderUri, ".gitignore"));
    return dec.decode(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
  } catch {
    return "";
  }
}

// Build the walk parameters shared by both providers from the search options.
async function walkParams(fs, folderUri, options) {
  const isExcluded = core.compileGlobs(options.excludes || []);
  const includeGlobs = (options.includes || []).filter(Boolean);
  const isIncluded = includeGlobs.length ? core.compileGlobs(includeGlobs) : null;
  const ig = options.useIgnoreFiles ? core.buildIgnore(ignoreFactory, await readGitignore(folderUri)) : null;
  return { list: fs.list, isExcluded, isIncluded, ig };
}

// Decode bytes to text for matching, skipping obvious binaries (a NUL in the
// first chunk) and oversized files.
function decodeTextOrNull(u8, maxBytes) {
  if (maxBytes && u8.length > maxBytes) return null;
  const probe = u8.subarray(0, Math.min(u8.length, 8192));
  for (let i = 0; i < probe.length; i++) if (probe[i] === 0) return null;
  return dec.decode(u8);
}

function activate(context) {
  out = vscode.window.createOutputChannel("Holospace Search");
  context.subscriptions.push(out);
  const extBase = context.extensionUri.toString().replace(/\/+$/, "");

  const marker = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 40);
  context.subscriptions.push(marker);

  const fileProvider = {
    async provideFileSearchResults(query, options, token) {
      await loadEngine(extBase);
      const fs = makeFs(options.folder);
      const params = await walkParams(fs, options.folder, options);
      // The quick-open filter: keep files whose path subsequence-matches the typed
      // pattern (VS Code re-scores; we just supply candidates).
      const pat = (query.pattern || "").toLowerCase();
      const matchesPat = (rel) => {
        if (!pat) return true;
        const s = rel.toLowerCase();
        let i = 0;
        for (let j = 0; j < s.length && i < pat.length; j++) if (s[j] === pat[i]) i++;
        return i === pat.length;
      };
      const uris = [];
      await core.walk(
        { ...params, maxResults: options.maxResults, isCancelled: () => token.isCancellationRequested },
        (rel) => { if (matchesPat(rel)) uris.push(vscode.Uri.joinPath(options.folder, ...rel.split("/"))); },
      );
      log(`fileSearch "${query.pattern || ""}" → ${uris.length} files`);
      return uris;
    },
  };

  const textProvider = {
    async provideTextSearchResults(query, options, progress, token) {
      await loadEngine(extBase);
      const fs = makeFs(options.folder);
      const params = await walkParams(fs, options.folder, options);
      const maxResults = options.maxResults && options.maxResults > 0 ? options.maxResults : Infinity;
      let total = 0;
      let limitHit = false;
      let files = 0;

      // Read + match + report INSIDE the walk, so matches stream to the Search
      // view as files are discovered (non-blocking on a large tree) rather than
      // after a full enumeration. The walk awaits this callback per file.
      await core.walk(
        { ...params, maxResults: 0, isCancelled: () => token.isCancellationRequested || limitHit },
        async (rel) => {
          if (limitHit || token.isCancellationRequested) return;
          let u8;
          try { u8 = await fs.read(rel); } catch { return; }
          const text = decodeTextOrNull(u8, options.maxFileSize);
          if (text == null) return;
          const matches = core.searchContent(text, query);
          if (!matches.length) return;
          files++;
          const uri = vscode.Uri.joinPath(options.folder, ...rel.split("/"));
          for (const m of matches) {
            if (limitHit) break;
            // The document range (used for navigation AND replace-all) and a
            // single-line preview with the match highlighted within it.
            const range = new vscode.Range(m.line, m.startCol, m.endLine, m.endCol);
            const previewMatch =
              m.line === m.endLine
                ? new vscode.Range(0, m.startCol, 0, m.endCol)
                : new vscode.Range(0, m.startCol, 0, m.lineText.length);
            progress.report({ uri, ranges: range, preview: { text: m.lineText, matches: previewMatch } });
            if (++total >= maxResults) limitHit = true;
          }
        },
      );
      log(`textSearch "${query.pattern}" → ${total} matches in ${files} files${limitHit ? " (limit hit)" : ""}`);
      return { limitHit };
    },
  };

  (async () => {
    await loadEngine(extBase);
    // Register only when the workbench exposes the proposed search APIs (a builtin
    // keeps its enabledApiProposals, so this is present) — degrade honestly if not.
    if (typeof vscode.workspace.registerFileSearchProvider !== "function" ||
        typeof vscode.workspace.registerTextSearchProvider !== "function") {
      log("search providers unavailable (proposed API not enabled) — find-in-files inactive");
      marker.text = "$(search) holospace search unavailable";
      marker.show();
      return;
    }
    context.subscriptions.push(vscode.workspace.registerFileSearchProvider(SCHEME, fileProvider));
    context.subscriptions.push(vscode.workspace.registerTextSearchProvider(SCHEME, textProvider));
    marker.text = "$(search) HOLOSPACE-SEARCH-LIVE";
    marker.tooltip = "Find-in-files over the holospace's virtio-9p workspace (CC-52)";
    marker.show();
    context.subscriptions.push(marker);
    log("File + Text search providers registered for holospace:// (find-in-files live)");
  })().catch((e) => log("startup error — " + (e && e.message)));
}

function deactivate() {}

module.exports = { activate, deactivate };
