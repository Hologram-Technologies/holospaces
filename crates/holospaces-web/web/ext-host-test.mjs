// CC-48 (deployed/browser) — the SUBSTRATE-NATIVE extension host activates an
// arbitrary Node-only marketplace extension, its contribution observable in the
// real workbench.
//
// EXECUTION SURFACE (corrected v2 — the hologram-substrate-native way): the
// extension host runs as NATIVE hologram exec on the browser peer's own wasm
// execution surface — a JS/Node-API runtime compiled to wasm32 (CpuBackend),
// backed by the holospace's OWN filesystem (CC-15), terminal (CC-11), and network
// (CC-16), reached over the in-process CC-33 bridge (CC-34). It is NOT
// openvscode-server inside the emulated x86-64 guest (the "interpreter wall":
// ~tens of MIPS, un-JIT-able under unsafe_code=forbid), and it is NOT vscode-web's
// WEB extension host (that is CC-19, already live). This mirrors the downstream
// in-browser-inference discipline: heavy in-tab work runs as native wasm exec,
// residency via the tiered MemKappaStore->OpfsKappaStore store, never the emulator.
//
// The load-bearing assertions, each observing the genuine workbench (never faked):
//   1. the witnessed subject is genuinely NODE-ONLY — its Open VSX package.json
//      has a `main` and NO `browser` entrypoint, so it CANNOT run in vscode-web's
//      web ext host; activating it proves the substrate-native Node-API host did it;
//   2. the substrate-native ext host is LIVE — the holospace's wasm-exec Node-API
//      runtime is up and reached over the bridge, publishing HOLOSPACE-NODE-EXTHOST-LIVE
//      in the real workbench;
//   3. the Node-only extension installs from Open VSX and ACTIVATES in that host,
//      its contribution OBSERVABLE in the real workbench DOM.
//
// Until the substrate-native Node-API ext-host runtime is built (S1), assertions
// 2-3 are EXPECTED RED — honest, never skip-passed (AGENTS.md). It never uses the
// additionalBuiltinExtensions web path (the rejected relabel).
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

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("EXT-HOST-TEST: FAIL —", m)));

// The wasm peer (`pkg/`) carries the substrate-native ext-host runtime the
// holospace-fs builtin loads. A real witness builds its prerequisites; the suite
// builds `pkg/` before invoking this witness. If genuinely absent, fail honestly.
async function present(p) { try { await stat(path.join(DIR, p)); return true; } catch { return false; } }
if (!(await present("pkg/holospaces_web_bg.wasm"))) {
  console.error("EXT-HOST-TEST: RED — the wasm peer (pkg/) is absent; run vv/targets/cc48-ext-host.sh");
  console.error("  (it builds the peer with wasm-pack before driving this witness).");
  process.exit(1);
}

// ── (1) The subject MUST be a Node-only extension (package.json `main`, NO
// `browser`) — verified against Open VSX, so it cannot run in the web ext host and
// its activation proves the substrate-native Node-API host did the work. A USER
// choice; override via CC48_EXT. The default is a stock Node-only Open VSX
// extension. `isNodeOnly` gates acceptance: a `browser`-entrypoint subject is
// rejected as a CC-19 relabel, not a CC-48 witness.
// `editorconfig.editorconfig` is a genuinely Node-only Open VSX extension (it has
// a `main`, no `browser` entrypoint — verified below against the live registry) and
// matches the host's default subject, so both install the same one. Override via
// CC48_EXT (and the `holospace.cc48Extension` setting) to witness another subject.
const EXT = process.env.CC48_EXT || "editorconfig.editorconfig";
const [pub, name] = EXT.split(".");

async function openVsxManifest(pubId, extName) {
  // Open VSX: /api/{pub}/{name}/latest → metadata incl. files.manifest (the
  // package.json URL). Fetch the manifest and read `main`/`browser`.
  const meta = await fetch(`https://open-vsx.org/api/${pubId}/${extName}/latest`).then((r) => r.json());
  const manifestUrl = meta?.files?.manifest;
  if (!manifestUrl) return null;
  return await fetch(manifestUrl).then((r) => r.json());
}

let isNodeOnly = false;
try {
  const pkg = await openVsxManifest(pub, name);
  // Node-only = has a `main` (Node entrypoint) and NO `browser` (web entrypoint).
  isNodeOnly = !!pkg && typeof pkg.main === "string" && pkg.main.length > 0 && pkg.browser == null;
  check(
    isNodeOnly,
    `the subject ${EXT} is Node-only on Open VSX (package.json has \`main\`, no \`browser\` entrypoint) — it cannot run in the web ext host`,
  );
} catch (e) {
  check(false, `could not verify the subject ${EXT} is Node-only on Open VSX: ${e}`);
}

const { chromium } = await import("playwright");
try { await stat(distDir); await stat(twDir); }
catch { execSync(`npm install --no-save ${WORKBENCH_PIN} ${BOOTSTRAP}`, { cwd: DIR, stdio: "ignore" }); }

async function ociLayerDigest(imageDir) {
  const blob = async (d) => JSON.parse(await readFile(path.join(imageDir, "blobs/sha256", d.split(":")[1]), "utf8"));
  const index = JSON.parse(await readFile(path.join(imageDir, "index.json"), "utf8"));
  const manifest = await blob(index.manifests[0].digest);
  return manifest.layers[0].digest.split(":")[1];
}
const cc18Layer = await ociLayerDigest(path.join(cc18, "image"));

// Compose the real deployed workbench. The substrate-native ext host (loaded by
// the holospace-fs builtin) installs + hosts the Node-only extension — we do NOT
// declare it as a vscode-web builtin (no additionalBuiltinExtensions web path).
// The subject κ is passed to the holospace-fs builtin via the page so it installs
// it into the substrate-native host.
const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: "." });

// κ-verify the workbench's executable core against the committed manifest (L5).
const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
  .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });
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

// The substrate-native ext host installs the subject — observe its package fetched
// from Open VSX (the gallery install, not a listing icon).
let installFetched = false;
const installRe = new RegExp(`open-vsx\\.org/.*/${pub}`, "i");
page.on("response", (r) => { if (installRe.test(r.url())) installFetched = true; });

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html?cc48ext=${encodeURIComponent(EXT)}`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (2) The substrate-native ext host is LIVE — the holospace's wasm-exec Node-API
  // runtime is up and reached over the bridge, publishing HOLOSPACE-NODE-EXTHOST-LIVE.
  // RED until the runtime is built (S1).
  const hostLive = await page
    .waitForFunction(() => /HOLOSPACE-NODE-EXTHOST-LIVE/.test(document.body.innerText || ""), null, { timeout: 180000 })
    .then(() => true).catch(() => false);
  check(hostLive, "the substrate-native (wasm-exec) Node-API extension host is LIVE over the bridge (HOLOSPACE-NODE-EXTHOST-LIVE), backed by CC-15/CC-11/CC-16 — not the emulated guest, not the web host");

  // (3) the Node-only extension installs from Open VSX and ACTIVATES in that host,
  // its contribution OBSERVABLE in the real workbench DOM — the load-bearing proof.
  // The host publishes the marker (a status-bar item, always in the DOM) NAMING the
  // subject ONLY after its `activate()` returns, so the marker naming `EXT` proves
  // THIS Node-only extension genuinely activated in the substrate-native host.
  await page.waitForTimeout(8000);
  check(installFetched, `the Node-only extension (${EXT}) installs from Open VSX into the substrate-native ext host`);
  const activated = await page
    .waitForFunction(
      (id) => new RegExp("HOLOSPACE-NODE-EXTHOST-LIVE[^\\n]*" + id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "i").test(document.body.innerText || ""),
      EXT,
      { timeout: 120000 },
    )
    .then(() => true).catch(() => false);
  check(activated, `the Node-only extension ${EXT} ACTIVATES in the substrate-native ext host — the host's marker naming it (published only after its activate() returns) appears in the real workbench`);

  console.log(
    failed
      ? "EXT-HOST-TEST: FAILED"
      : "EXT-HOST-TEST: PASS (a Node-only Open VSX extension activates in holospaces' substrate-native (wasm-exec) extension host — backed by the holospace's own primitives, no emulated-guest server, no Node on the host, no deployment outside the holospace)",
  );
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
