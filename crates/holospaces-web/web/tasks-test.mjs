// CC-53 (deployed/browser) — tasks.json tasks run in the devcontainer, with
// output, exit status, and problem matchers, witnessed in the real workbench.
//
// holospace-tasks registers a TaskProvider whose CustomExecution Pseudoterminal
// runs each task IN THE GUEST devcontainer (CC-11) over a file-exec channel on
// the holospace's OWN virtio-9p workspace (CC-15) — a guest task-runner agent
// (seeded into the devcontainer /init) runs the command and streams its output +
// exit code back over 9p. This drives the REAL workbench and asserts, each
// observing the genuine task system / an independent guest run, never faked:
//
//   1. the TaskProvider is LIVE in the real workbench (HOLOSPACE-TASKS-LIVE);
//   2. the default build task (Ctrl+Shift+B) RUNS IN THE GUEST and its non-zero
//      EXIT STATUS surfaces — the `build` task (echo + `exit 2`) reports code 2,
//      captured from the guest agent's `<id>.exit` over 9p;
//   3. its problem matcher produces a DIAGNOSTIC in the Problems panel — the
//      task emits `main.rs:2:5: warning: …`, and the contributed matcher turns
//      it into a problem on main.rs;
//   4. the devcontainer's LIFECYCLE commands (CC-22) surface as re-runnable
//      tasks — the seeded devcontainer.json's postCreateCommand runs in the
//      guest and exits 0;
//   5. a BACKGROUND/watch task runs WITHOUT blocking — while the long-running
//      watch task streams in the guest, the build task runs to its exit
//      (concurrent guest execution; the UI + task system stay usable).
//
// The fast core (tasks.json parse, command build, exec protocol) is proven by
// builtin-extensions/holospace-tasks/tasks-core.test.cjs; this proves it wired
// into the real workbench against a real guest.
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { composeWorkbenchHtml, WORKBENCH_PIN, BUILTIN_EXTENSIONS } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(DIR, "../../..");
const BOOTSTRAP = "@vscode/test-web@0.0.80";
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const twDir = path.join(DIR, "node_modules/@vscode/test-web");
// Serve every builtin the composed workbench declares — one shared list.
const extDirs = Object.fromEntries(
  BUILTIN_EXTENSIONS.map((n) => [n, path.join(DIR, "builtin-extensions", n)]),
);
const cc16 = path.join(ROOT, "vv/artifacts/cc16");
const cc18 = path.join(ROOT, "vv/artifacts/cc18");

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("TASKS-TEST: FAIL —", m)));

async function present(p) { try { await stat(path.join(DIR, p)); return true; } catch { return false; } }
if (!(await present("pkg/holospaces_web_bg.wasm"))) {
  console.error("TASKS-TEST: RED — the wasm peer (pkg/) is absent; run vv/suites/cc53-tasks.sh");
  process.exit(1);
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

const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: "." });

const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
  .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });
let coreOk = 0;
for (const { hash, file } of manifest) {
  if (createHash("sha256").update(await readFile(path.join(distDir, file))).digest("hex") === hash) coreOk++;
}
check(coreOk === manifest.length, `the workbench's executable core re-derives to its pinned κ (${coreOk}/${manifest.length} files, Law L5)`);

const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".cjs": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".gz": "application/gzip", ".ico": "image/x-icon",
};
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  if (rel.split("/").includes("..")) { res.writeHead(403).end("forbidden"); return; }
  const send = (b, ct) => { res.writeHead(200, { "content-type": ct || "application/octet-stream" }); res.end(b); };
  try {
    if (rel === "/" || rel === "/workbench.html") return send(html, "text/html");
    for (const [name, d] of Object.entries(extDirs)) {
      const pre = `/ext/${name}/`;
      if (rel.startsWith(pre)) return send(await readFile(path.join(d, rel.slice(pre.length))), TYPES[path.extname(rel)]);
    }
    if (rel.startsWith("/pkg/")) return send(await readFile(path.join(DIR, rel)), TYPES[path.extname(rel)]);
    if (rel === "/devcontainer-net-kernel.gz") return send(await readFile(path.join(cc16, "kernel/Image.gz")), "application/gzip");
    if (rel === "/devcontainer-lsp-layer.tar.gz") return send(await readFile(path.join(cc18, "image/blobs/sha256", cc18Layer)), "application/gzip");
    return send(await readFile(path.join(distDir, rel)), TYPES[path.extname(rel)]);
  } catch { res.writeHead(404).end("not found"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("TASKS-TEST: pageerror —", e.message));
const cclog = [];
page.on("console", (m) => { const t = m.text(); if (t.includes("[CC53]")) { cclog.push(t); console.log("  " + t); } });

// Drive a command-palette command, selecting the row whose label matches by
// tokens (order-independent), then a real click so the exact command runs.
async function runCommand(title) {
  const input = page.locator(".quick-input-widget .input, .quick-input-box input").first();
  for (let attempt = 0; attempt < 3; attempt++) {
    await page.keyboard.press("Control+Shift+P");
    try { await input.waitFor({ state: "visible", timeout: 5000 }); break; }
    catch { if (attempt === 2) throw new Error(`command palette did not open for "${title}"`); }
  }
  await input.fill(`>${title}`);
  await page.waitForTimeout(700);
  const idx = await page.evaluate((t) => {
    const norm = (s) => (s || "").toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
    const want = norm(t);
    const tokens = want.split(" ").filter(Boolean);
    const rows = [...document.querySelectorAll(".quick-input-list .monaco-list-row")];
    // EXACT normalized match first — a token match alone picks the wrong sibling
    // ("Holospace: Run Task" tokens all appear in "Holospace: Run Build Task").
    // The row text may append a keybinding/description, so fall back to prefix,
    // then to the SHORTEST token-matching row (the plain title is the shortest).
    const texts = rows.map((r) => norm(r.innerText));
    let best = texts.findIndex((x) => x === want);
    if (best < 0) best = texts.findIndex((x) => x.startsWith(want));
    if (best < 0) {
      const matches = texts.map((x, i) => ({ x, i })).filter(({ x }) => tokens.every((tok) => x.includes(tok)));
      matches.sort((a, b) => a.x.length - b.x.length);
      best = matches.length ? matches[0].i : -1;
    }
    return { best, texts: texts.slice(0, 8) };
  }, title);
  const { best: idx2, texts } = idx;
  if (idx2 < 0) console.log(`  palette rows for "${title}":`, JSON.stringify(texts));
  if (idx2 >= 0) await page.locator(".quick-input-list .monaco-list-row").nth(idx2).click();
  else await page.keyboard.press("Enter");
  await page.waitForTimeout(700);
}

// Pick an item from the currently-open quick-pick by its visible label.
async function pickQuick(label, timeout = 12000) {
  const input = page.locator(".quick-input-widget .input, .quick-input-box input").first();
  await input.waitFor({ state: "visible", timeout });
  await input.fill(label);
  await page.waitForTimeout(600);
  const idx = await page.evaluate((t) => {
    const norm = (s) => (s || "").toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
    const want = norm(t);
    const tokens = want.split(" ").filter(Boolean);
    const rows = [...document.querySelectorAll(".quick-input-list .monaco-list-row")];
    const texts = rows.map((r) => norm(r.innerText));
    let best = texts.findIndex((x) => x === want);
    if (best < 0) best = texts.findIndex((x) => x.startsWith(want));
    if (best < 0) {
      const matches = texts.map((x, i) => ({ x, i })).filter(({ x }) => tokens.every((tok) => x.includes(tok)));
      matches.sort((a, b) => a.x.length - b.x.length);
      best = matches.length ? matches[0].i : -1;
    }
    return best;
  }, label);
  if (idx >= 0) await page.locator(".quick-input-list .monaco-list-row").nth(idx).click();
  else await page.keyboard.press("Enter");
  await page.waitForTimeout(700);
}

// Run the build task via the extension's command (runs it through the real task
// system — executeTask → the CustomExecution → the guest). A palette command is
// the reliable trigger headless (the Ctrl+Shift+B keybinding is not delivered).
async function runBuildTask() {
  await runCommand("Holospace: Run Build Task");
  await page.waitForTimeout(500);
}

const countLog = (re) => cclog.filter((l) => re.test(l)).length;
const waitCount = (re, n, timeout = 120000) =>
  (async () => {
    const start = Date.now();
    while (Date.now() - start < timeout) {
      if (countLog(re) >= n) return true;
      await page.waitForTimeout(500);
    }
    return false;
  })();
const waitLog = (re, timeout = 120000) => waitCount(re, 1, timeout);

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (1) The TaskProvider is LIVE.
  const live = await page
    .waitForFunction(() => /HOLOSPACE-TASKS-LIVE/.test(document.body.innerText || ""), null, { timeout: 120000 })
    .then(() => true).catch(() => false);
  check(live, "the holospace-tasks TaskProvider is registered + live in the real workbench (registerTaskProvider 0 → a provider)");

  // Wait for the workspace + guest agent to be up: provideTasks finds the seeded
  // tasks.json once 9p is mounted, and the guest agent (in the devcontainer
  // /init) runs the command. Retry running the build task until the guest
  // executes it (boot + seed can take ~1-2 min).
  let ran = false;
  for (let i = 0; i < 18 && !ran; i++) {
    await runBuildTask();
    ran = await waitLog(/HOLOSPACE-TASK-EXIT label=build code=2/, 15000);
    if (!ran) await page.waitForTimeout(4000);
  }
  // (2) A tasks.json task runs IN THE GUEST and its exit status surfaces.
  check(ran, "the tasks.json build task RUNS in the devcontainer and its non-zero EXIT STATUS surfaces (build → code 2, captured from the guest over 9p)");

  // (3) Its problem matcher produced a diagnostic — surface the Problems panel.
  await page.waitForTimeout(1500);
  // Open the Problems panel and look for the diagnostic the matcher produced.
  await runCommand("View: Focus Problems");
  await page.waitForTimeout(1500);
  const diag = await page
    .waitForFunction(() => /TODO found here/.test(document.body.innerText || ""), null, { timeout: 30000 })
    .then(() => true).catch(() => false);
  check(diag, "the task's problem matcher produces a DIAGNOSTIC in the Problems panel (main.rs:2:5: warning: TODO found here)");

  // (4) The devcontainer's LIFECYCLE commands (CC-22) surface as re-runnable
  // tasks: run the seeded devcontainer.json's postCreateCommand through the task
  // system and see it exit 0 in the guest.
  await runCommand("Holospace: Run Task");
  await pickQuick("lifecycle postCreateCommand");
  const lifecycle = await waitLog(/HOLOSPACE-TASK-EXIT label=lifecycle: postCreateCommand code=0/, 45000);
  check(lifecycle, "the devcontainer.json lifecycle command surfaces as a task and RUNS in the guest (lifecycle: postCreateCommand → exit 0, CC-22)");

  // (5) A BACKGROUND/watch task runs WITHOUT blocking: start the long-running
  // watch task; once the guest streams its first output (it is running), run the
  // build task again — its exit must surface WHILE watch is still running (the
  // guest agent executes tasks concurrently; the UI + task system stay usable).
  await runCommand("Holospace: Run Task");
  await pickQuick("watch");
  const watching = await waitLog(/HOLOSPACE-TASK-OUT label=watch/, 45000);
  check(watching, "the background watch task is RUNNING in the guest (first output streamed)");
  const buildExitsBefore = countLog(/HOLOSPACE-TASK-EXIT label=build code=2/);
  await runBuildTask();
  const buildAgain = await waitCount(/HOLOSPACE-TASK-EXIT label=build code=2/, buildExitsBefore + 1, 60000);
  const watchStillRunning = countLog(/HOLOSPACE-TASK-EXIT label=watch/) === 0;
  check(
    buildAgain && watchStillRunning,
    "a background task runs WITHOUT blocking — the build task ran to its exit while watch kept running (concurrent guest execution, non-blocking UI)",
  );

  console.log(
    failed
      ? "TASKS-TEST: FAILED"
      : "TASKS-TEST: PASS (tasks.json tasks run in the devcontainer over the holospace's own primitives — a real guest run with output + exit status captured over 9p, a problem matcher producing a diagnostic, the devcontainer lifecycle commands surfaced as tasks, and a background task running without blocking — no server outside the holospace)",
  );
} catch (e) {
  failed = true;
  console.error("TASKS-TEST: error —", e && e.message);
  try { await page.screenshot({ path: path.join(DIR, "tasks-test-failure.png") }); } catch {}
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
