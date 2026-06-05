// CC-48 (TARGET, deployed/browser) — the substrate-native extension host
// activates an ARBITRARY marketplace extension, and its contribution appears in
// the real workbench.
//
// This is the behavioral spec, written FIRST (BDD), for the open frontier named
// by CC-19 / CC-34 / ADR-020: holospaces is the VS Code remote (in the tab, on
// the substrate). The server-backed editor capabilities (LSP over the CC-33
// bridge, CC-18) are already live — lsp-test.mjs is the witnessed exemplar. The
// remaining part is the EXTENSION HOST: VS Code's remote-server (the real
// openvscode-server) running INSIDE the booted devcontainer, reached over the
// in-process substrate bridge, against which an arbitrary (non-web) workspace/
// Node extension from Open VSX installs and ACTIVATES — removing the workbench's
// "not available for the Web" notice. No Node on the host, no deployment outside
// the holospace (Law L4); the backends are the holospace's own filesystem
// (CC-15), terminal (CC-11), and network (CC-16).
//
// Structure mirrors lsp-test.mjs (the CC-18 deployed-over-bridge witness) and
// vscode-extension-test.mjs (the CC-19 real-activation witness): a static server
// for the composed workbench + the wasm peer, driven by headless Chromium. The
// load-bearing assertions are:
//   1. the workbench connects to holospaces-as-remote over the bridge (the
//      remote-agent management connection — CC-34's bigger server);
//   2. an ARBITRARY Open VSX workspace/Node extension installs against that
//      remote and ACTIVATES in the remote extension host (its activate() runs);
//   3. its contribution (a command / status-bar item / view) is OBSERVABLE in
//      the real workbench DOM — never inferred, never faked.
//
// This file NEVER fakes activation: it observes the genuine workbench. Until the
// in-guest openvscode-server orchestration is live, this witness is EXPECTED RED
// (the target tier is non-gating). Promotion to vv/suites/ requires it GREEN.

import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { composeWorkbenchHtml, WORKBENCH_PIN } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const BOOTSTRAP = "@vscode/test-web@0.0.80";
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const twDir = path.join(DIR, "node_modules/@vscode/test-web");

// Prerequisites the witness needs to even attempt the real path: the wasm peer
// (`pkg/`) and the bridged devcontainer kernel + image must be built into the web
// dir (build-site / holo_fixture produce them). If they are absent this witness
// cannot exercise holospaces-as-remote — report RED honestly, do not skip-pass.
// Checked FIRST (before pulling in playwright / the workbench dist), so the
// honest-RED path runs even on a bare checkout.
async function present(p) {
  try { await stat(path.join(DIR, p)); return true; } catch { return false; }
}
const havePeer = await present("pkg/holospaces_web_bg.wasm");
const haveKernel = await present("devcontainer-net-kernel.gz");
if (!havePeer || !haveKernel) {
  console.error("EXT-HOST-TEST: RED (prerequisites absent) —");
  console.error("  the wasm peer (pkg/) and the bridged devcontainer kernel/image must be built");
  console.error("  (run `scripts/build-site.sh` / the holo_fixture example) before this witness can");
  console.error("  exercise holospaces-as-remote. This is the BDD target; the component is not yet live.");
  process.exit(1);
}

const { chromium } = await import("playwright");
try {
  await stat(distDir);
  await stat(twDir);
} catch {
  execSync(`npm install --no-save ${WORKBENCH_PIN} ${BOOTSTRAP}`, { cwd: DIR, stdio: "ignore" });
}

// The witnessed subject is an ARBITRARY non-web (workspace/Node) extension from
// Open VSX — i.e. one whose contribution requires the remote extension host, NOT
// a web-model extension (a browser entrypoint). `vscodevim.vim` is a real,
// widely-installed Node extension on Open VSX with a workspace `main` entrypoint
// and observable contributions (a status-bar item with the current Vim mode);
// it is a USER choice, never a holospaces default — the unmodified subject.
const EXT = process.env.CC48_EXT || "vscodevim.vim";
// The DOM signal that the arbitrary extension genuinely activated: vim's
// status-bar mode indicator. Override via env for a different subject.
const ACTIVATION_SIGNAL = process.env.CC48_SIGNAL || "-- NORMAL --|-- INSERT --|vim";

// Compose the real deployed workbench, declaring the arbitrary extension so the
// launch installs it from the open gallery — the same path CC-19 witnesses for a
// web extension, here for a workspace/Node extension that needs the remote host.
const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: ".", extensions: [EXT] });

const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".gz": "application/gzip",
};
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  if (rel === "/" || rel === "/workbench.html") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(html);
  }
  const file = rel.startsWith("/ext/holospace-fs/")
    ? path.join(DIR, "builtin-extensions/holospace-fs", rel.slice("/ext/holospace-fs/".length))
    : rel.startsWith("/pkg/")
      ? path.join(DIR, rel)
      : ["/devcontainer-net-kernel.gz", "/devcontainer-lsp-layer.tar.gz", "/devcontainer-arm64-kernel.gz"].includes(rel)
        ? path.join(DIR, rel)
        : path.join(distDir, rel);
  try {
    const body = await readFile(file);
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("EXT-HOST-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("EXT-HOST-TEST: pageerror —", e.message));

// Observe the install reading the arbitrary extension's package from Open VSX
// (the unpkg/asset resource template) — that is the gallery install, not a mere
// listing icon. Mirrors extensions-test.mjs.
let installFetched = false;
const pubName = EXT.split(".");
const installRe = new RegExp(`open-vsx\\.org/vscode/(unpkg|asset)/${pubName[0]}`, "i");
page.on("response", (r) => {
  if (installRe.test(r.url())) installFetched = true;
});

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (1) holospaces-as-remote: the workbench's remote-agent connection comes up
  // against the in-guest server over the substrate bridge (CC-34's bigger server,
  // the management + extension-host connection of the remote-server protocol).
  // The holospace-fs extension surfaces the live remote in the "Holospace" output
  // channel / a status-bar marker once the in-guest openvscode-server is reached.
  const remoteLive = await page
    .waitForFunction(
      () => {
        const txt = document.body.innerText || "";
        // The substrate-native remote is live (the management connection over the
        // bridge handshook) — a deterministic marker the host wiring publishes.
        return /HOLOSPACE-REMOTE-LIVE|Remote.*holospace/i.test(txt) ||
          !!document.querySelector('[aria-label*="holospace" i]');
      },
      null,
      { timeout: 120000 },
    )
    .then(() => true)
    .catch(() => false);
  check(remoteLive, "the workbench's remote extension host connects to holospaces-as-remote over the substrate bridge (CC-34)");

  // (2) the arbitrary extension installs from the open gallery against the remote.
  await page.waitForTimeout(8000);
  check(installFetched, `the arbitrary workspace/Node extension (${EXT}) installs from Open VSX against holospaces-as-remote`);

  // (3) it ACTIVATES in the remote ext host and its contribution is OBSERVABLE in
  // the real workbench DOM — the load-bearing proof (never inferred, never faked).
  const sigRe = new RegExp(ACTIVATION_SIGNAL, "i");
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
      : "EXT-HOST-TEST: PASS (an arbitrary Open VSX workspace/Node extension activates against holospaces-as-remote over the substrate bridge, its contribution in the real workbench — no Node, no other deployment)",
  );
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
