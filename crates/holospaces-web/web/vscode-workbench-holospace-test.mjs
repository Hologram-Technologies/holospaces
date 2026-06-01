// CC-17 (Phase 3) — the real VS Code web workbench, bound to the running
// holospace, served STATICALLY (no server).
//
// This is the architecture's Workspace Projection (ADR-012): the REAL VS Code
// web workbench (the build that powers vscode.dev / github.dev), not the
// Monaco/xterm fallback (CC-13). Per ADR-015 the browser peer uses VS Code's
// WEB extension-host model, with the holospace primitives as the backends.
//
// The substrate-native realization: holospaces is the gateway that COMPOSES and
// serves the workbench. It serves Microsoft's κ-verified executable core
// BYTE-IDENTICAL (Law L5 — re-derived against the CC-17 manifest before load),
// and composes it as its OWN content with (a) VS Code's supported web-embedding
// bootstrap (`create()`), and (b) a builtin extension `holospace-fs` that boots
// the holospace IN the extension-host worker (the browser is a first-class
// compute substrate) and exposes its `virtio-9p` workspace as a FileSystemProvider
// (CC-15) and its console as the integrated terminal (CC-11). No server, no
// control plane (Laws L1/L3/L4) — the whole thing is static content holospaces
// composes and serves, the github.dev web model with the remote replaced by a
// holospace on the substrate.
//
// This witness composes the workbench exactly as the deploy does (see
// build-workbench.mjs), boots it in headless Chromium, and asserts the real
// workbench loads κ-verified, the LIVE holospace workspace mounts into the
// editor (a file the booted holospace holds is read back through the editor over
// the 9p share), and the workbench is wired to the open gallery (Open VSX).
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

import { composeWorkbenchHtml, WORKBENCH_PIN } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(DIR, "../../..");
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const extDir = path.join(DIR, "builtin-extensions/holospace-fs");
const cc14 = path.join(ROOT, "vv/artifacts/cc14");
const cc14Layer = "a5d38ca0c6059f07fa4cd14937697b61b93696a766dad68c1a2a7b5c54df8e01";

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("HOLOSPACE-WORKBENCH: FAIL —", m)));

// 1) Obtain the pinned real VS Code web build + the supported web bootstrap.
try {
  await stat(distDir);
} catch {
  console.log(`==> installing the pinned ${WORKBENCH_PIN}`);
  execSync(`npm install --no-save ${WORKBENCH_PIN} @vscode/test-web@0.0.80`, { cwd: DIR, stdio: "ignore" });
}

// 2) κ-verify the workbench's executable core against the committed manifest
//    (Law L5 — a forged byte is refused before load), exactly as CC-17 Phase 1.
const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
  .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });
let verified = 0;
for (const { hash, file } of manifest) {
  const bytes = await readFile(path.join(distDir, file));
  if (createHash("sha256").update(bytes).digest("hex") === hash) verified++;
  else check(false, `executable-core integrity: ${file}`);
}
check(verified === manifest.length, `the workbench's executable core re-derives to its pinned κ (${verified}/${manifest.length} files, Law L5)`);

// 3) Serve the composed workbench + the holospace assets, statically.
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff", ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".ico": "image/x-icon", ".gz": "application/gzip" };
let port;
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  const send = (b, ct) => { res.writeHead(200, { "content-type": ct || "application/octet-stream" }); res.end(b); };
  try {
    if (rel === "/" || rel.startsWith("/?")) {
      return send(await composeWorkbenchHtml({ distDir, twDir: path.join(DIR, "node_modules/@vscode/test-web"), baseUrl: ".", origin: `http://127.0.0.1:${port}` }), "text/html");
    }
    if (rel.startsWith("/ext/holospace-fs")) {
      let sub = rel.slice("/ext/holospace-fs".length); if (sub === "" || sub === "/") sub = "/package.json";
      return send(await readFile(path.join(extDir, sub)), TYPES[path.extname(sub)]);
    }
    if (rel.startsWith("/pkg/")) return send(await readFile(path.join(DIR, rel)), TYPES[path.extname(rel)]);
    if (rel === "/devcontainer-kernel.gz") return send(await readFile(path.join(cc14, "kernel/Image.gz")), "application/gzip");
    if (rel === "/devcontainer-layer.tar.gz") return send(await readFile(path.join(cc14, "image/blobs/sha256", cc14Layer)), "application/gzip");
    return send(await readFile(path.join(distDir, rel)), TYPES[path.extname(rel)]);
  } catch { res.writeHead(404).end("null"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
port = server.address().port;

const browser = await chromium.launch();
try {
  const page = await (await browser.newContext()).newPage();
  await page.goto(`http://127.0.0.1:${port}/`, { timeout: 30000 });

  // The real workbench loads (the genuine vscode-web, not Monaco-only — CC-13).
  const loaded = await page.waitForSelector(".monaco-workbench .activitybar", { timeout: 60000 }).then(() => true).catch(() => false);
  check(loaded, "the real VS Code web workbench loaded (activity bar — the real workbench shell, not Monaco)");

  // The holospace-fs builtin extension booted the devcontainer in the extension
  // host and mounted its virtio-9p workspace (CC-15): the file the booted
  // holospace holds appears in the editor's explorer (its readDirectory reached
  // the real editor over the live workspace).
  const mounted = await page.waitForFunction(
    () => [...document.querySelectorAll(".explorer-folders-view .monaco-list-row")].some((r) => /WELCOME/.test(r.textContent || "")),
    null, { timeout: 180000 },
  ).then(() => true).catch(() => false);
  check(mounted, "the running holospace's virtio-9p workspace mounted into the real workbench (CC-15 — a file the holospace holds is in the editor's file tree)");

  // The editor reads that file's content over the live workspace (FileSystemProvider.readFile).
  let editorText = "";
  if (mounted) {
    try {
      await page.click(".explorer-folders-view .monaco-list-row:has-text('WELCOME')", { timeout: 5000 });
      await page.waitForTimeout(3000);
      editorText = await page.evaluate(() => document.querySelector(".monaco-editor .view-lines")?.innerText || "");
    } catch { /* timing-flaky; the mount above is the deterministic fact */ }
  }
  check(/holospace/i.test(editorText), `the editor reads the file's content over the live workspace (read by κ; CC-11/CC-15): "${editorText.replace(/\s+/g, " ").slice(0, 60)}"`);

  // The workbench is wired to the open gallery (Open VSX) — arbitrary extensions, no lock-in (CC-19).
  const gallery = await page.evaluate(() => {
    const el = document.getElementById("vscode-workbench-web-configuration");
    try { return JSON.parse(el.getAttribute("data-settings"))?.productConfiguration?.extensionsGallery?.serviceUrl ?? null; } catch { return null; }
  });
  check(typeof gallery === "string" && gallery.includes("open-vsx.org"), `the workbench is wired to the open gallery (Open VSX), not the MS Marketplace (serviceUrl: ${gallery})`);

  console.log(failed ? "HOLOSPACE-WORKBENCH: FAILED" : "HOLOSPACE-WORKBENCH: PASS (the real VS Code workbench, κ-verified, over the running holospace — served statically, no server)");
} finally {
  await browser.close();
  server.close();
}
process.exitCode = failed ? 1 : 0;
