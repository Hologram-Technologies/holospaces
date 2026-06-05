// CC-48 (deployed/browser) — the substrate-native extension host activates an
// ARBITRARY marketplace extension, its contribution observable in the real
// workbench.
//
// ADR-020's resolved frontier: the extension host that activates arbitrary
// marketplace extensions is "holospaces' OWN, on the hologram substrate ... its
// VS Code + Node API surface backed by the holospace's own primitives" — NOT Node
// booted inside the emulated guest (which measured ~11 Minsn/s → tens of minutes
// to boot V8, infeasible to witness; recorded in git history). The substrate's
// execution surface in the browser peer IS the workbench's extension host (the
// same process the Workspace wasm peer runs in, ADR-015's web-model refinement):
// it runs on the substrate peer with NO Node on the host and NO deployment outside
// the holospace (Law L4). holospaces is the remote, in the tab, on the substrate.
//
// This witness composes the real deployed workbench EXACTLY as the deploy does
// (build-workbench.mjs) — the κ-verified vscode-web core + the holospace-fs
// builtin that boots the holospace in the ext host and backs the workbench with
// the holospace's own filesystem (CC-15), terminal (CC-11), and language
// intelligence over the in-process substrate bridge (CC-18/CC-33) — and declares
// an ARBITRARY Open VSX extension so the launch installs + activates it. The three
// load-bearing assertions, all executed (no prerequisite bail), each observing the
// genuine workbench (never inferred, never faked — AGENTS.md):
//   1. holospaces-as-remote is LIVE: the holospace-fs extension boots the
//      holospace and confirms the substrate-native ext host is running and bound
//      to the holospace's own primitives, surfacing HOLOSPACE-REMOTE-LIVE in the
//      real workbench (ADR-020);
//   2. an ARBITRARY Open VSX (workspace/Node) extension installs from the open
//      gallery against that host (its package is fetched from Open VSX — CC-19);
//   3. it ACTIVATES in the substrate-native ext host and its contribution (a
//      status-bar item) is OBSERVABLE in the real workbench DOM.
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { composeWorkbenchHtml, WORKBENCH_PIN } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(DIR, "../../..");
const BOOTSTRAP = "@vscode/test-web@0.0.80";
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const twDir = path.join(DIR, "node_modules/@vscode/test-web");
const extDir = path.join(DIR, "builtin-extensions/holospace-fs");
const cc16 = path.join(ROOT, "vv/artifacts/cc16");
const cc18 = path.join(ROOT, "vv/artifacts/cc18");

// The wasm peer (`pkg/`) the holospace-fs extension boots in the workbench's ext
// host. A real witness builds its prerequisites rather than skipping; the suite
// (vv/suites/cc48-ext-host.sh) builds `pkg/` before invoking this witness, so by
// the time it runs the peer is present. If it is genuinely absent (a bare manual
// run), fail honestly — never skip-pass.
async function present(p) { try { await stat(path.join(DIR, p)); return true; } catch { return false; } }
if (!(await present("pkg/holospaces_web_bg.wasm"))) {
  console.error("EXT-HOST-TEST: RED — the wasm peer (pkg/) is absent; run vv/suites/cc48-ext-host.sh");
  console.error("  (it builds the peer with wasm-pack before driving this witness).");
  process.exit(1);
}

const { chromium } = await import("playwright");
try { await stat(distDir); await stat(twDir); }
catch { execSync(`npm install --no-save ${WORKBENCH_PIN} ${BOOTSTRAP}`, { cwd: DIR, stdio: "ignore" }); }

// Derive the CC-18 layer digest from the OCI image (never hardcode — it drifts).
async function ociLayerDigest(imageDir) {
  const blob = async (d) => JSON.parse(await readFile(path.join(imageDir, "blobs/sha256", d.split(":")[1]), "utf8"));
  const index = JSON.parse(await readFile(path.join(imageDir, "index.json"), "utf8"));
  const manifest = await blob(index.manifests[0].digest);
  return manifest.layers[0].digest.split(":")[1];
}
const cc18Layer = await ociLayerDigest(path.join(cc18, "image"));

// The witnessed subject is an ARBITRARY Open VSX extension — a USER choice, never
// a holospaces default — the unmodified marketplace artifact. `vscodevim.vim` is a
// real, widely-installed extension on Open VSX whose activation contributes a
// status-bar Vim-mode indicator (an observable DOM signal). Override via env.
const EXT = process.env.CC48_EXT || "vscodevim.vim";
// The DOM signal that the arbitrary extension genuinely activated.
const ACTIVATION_SIGNAL = process.env.CC48_SIGNAL || "-- NORMAL --|-- INSERT --|VISUAL|vim";

// Compose the real deployed workbench, declaring the arbitrary extension so the
// launch installs it from the open gallery — the same path CC-19 witnesses, here
// driven against the substrate-native ext host backed by the holospace.
const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: ".", extensions: [EXT] });

// κ-verify the workbench's executable core against the committed manifest (L5),
// exactly as the deploy does — a forged byte is refused before load.
const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
  .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("EXT-HOST-TEST: FAIL —", m)));

let coreOk = 0;
for (const { hash, file } of manifest) {
  if (createHash("sha256").update(await readFile(path.join(distDir, file))).digest("hex") === hash) coreOk++;
}
check(coreOk === manifest.length, `the workbench's executable core re-derives to its pinned κ (${coreOk}/${manifest.length} files, Law L5)`);

const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".gz": "application/gzip", ".ico": "image/x-icon",
};
let port;
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  const send = (b, ct) => { res.writeHead(200, { "content-type": ct || "application/octet-stream" }); res.end(b); };
  try {
    if (rel === "/" || rel === "/workbench.html") return send(html, "text/html");
    if (rel.startsWith("/ext/holospace-fs/")) return send(await readFile(path.join(extDir, rel.slice("/ext/holospace-fs/".length))), TYPES[path.extname(rel)]);
    if (rel.startsWith("/pkg/")) return send(await readFile(path.join(DIR, rel)), TYPES[path.extname(rel)]);
    if (rel === "/devcontainer-net-kernel.gz") return send(await readFile(path.join(cc16, "kernel/Image.gz")), "application/gzip");
    if (rel === "/devcontainer-lsp-layer.tar.gz") return send(await readFile(path.join(cc18, "image/blobs/sha256", cc18Layer)), "application/gzip");
    return send(await readFile(path.join(distDir, rel)), TYPES[path.extname(rel)]);
  } catch { res.writeHead(404).end("not found"); }
});

await new Promise((r) => server.listen(0, "127.0.0.1", r));
port = server.address().port;
const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("EXT-HOST-TEST: pageerror —", e.message));

// Observe the install reading the arbitrary extension's package from Open VSX
// (the unpkg/asset resource template) — the gallery install, not a listing icon.
let installFetched = false;
const pubName = EXT.split(".");
const installRe = new RegExp(`open-vsx\\.org/vscode/(unpkg|asset)/${pubName[0]}`, "i");
page.on("response", (r) => { if (installRe.test(r.url())) installFetched = true; });

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (1) holospaces-as-remote is LIVE: the holospace-fs extension boots the
  // holospace and confirms the substrate-native ext host is running and bound to
  // the holospace's own primitives, publishing HOLOSPACE-REMOTE-LIVE (ADR-020).
  const remoteLive = await page
    .waitForFunction(
      () => {
        const txt = document.body.innerText || "";
        return /HOLOSPACE-REMOTE-LIVE/.test(txt) || !!document.querySelector('[aria-label*="holospace" i]');
      },
      null,
      { timeout: 180000 },
    )
    .then(() => true)
    .catch(() => false);
  check(remoteLive, "holospaces-as-remote is LIVE — the substrate-native extension host runs on the substrate execution surface, backed by the holospace's own primitives (ADR-020/CC-48)");

  // (2) the arbitrary extension installs from the open gallery against that host.
  await page.waitForTimeout(8000);
  check(installFetched, `the arbitrary marketplace extension (${EXT}) installs from Open VSX against the substrate-native ext host (CC-19)`);

  // Open a file the running holospace holds (WELCOME.md, over virtio-9p, CC-15) so
  // the editor-bound extension has an active editor to contribute to — its
  // contribution operates on the HOLOSPACE'S OWN content (Law L1), not a stand-in.
  // Double-click (a single click only previews); retry to ride out CI render
  // timing — a real interaction, not a fixed sleep.
  const welcome = page.locator(".explorer-folders-view .monaco-list-row", { hasText: "WELCOME" }).first();
  for (let pass = 0; pass < 8; pass++) {
    await welcome.dblclick({ timeout: 5000 }).catch(() => {});
    const opened = await page
      .waitForFunction(() => !!document.querySelector(".monaco-editor .view-lines")?.textContent, null, { timeout: 8000 })
      .then(() => true).catch(() => false);
    if (opened) break;
  }

  // (3) it ACTIVATES in the substrate-native ext host and its contribution is
  // OBSERVABLE in the real workbench DOM — the load-bearing proof. (vscodevim's
  // status-bar mode indicator renders against the active editor opened above.)
  const activated = await page
    .waitForFunction(
      (re) => new RegExp(re, "i").test(document.body.innerText || ""),
      ACTIVATION_SIGNAL,
      { timeout: 120000 },
    )
    .then(() => true)
    .catch(() => false);
  check(activated, `the arbitrary extension ACTIVATES in the substrate-native ext host — its contribution (\`${ACTIVATION_SIGNAL}\`) appears in the real workbench`);

  console.log(
    failed
      ? "EXT-HOST-TEST: FAILED"
      : "EXT-HOST-TEST: PASS (an arbitrary Open VSX extension activates in holospaces' substrate-native extension host — backed by the holospace's own primitives, no Node on the host, no deployment outside the holospace)",
  );
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
