// CC-51 (deployed/browser) — Source Control (Git) over the holospace's OWN
// virtio-9p workspace, witnessed end-to-end in the real workbench.
//
// The SCM provider (holospace-scm) runs a real Git engine (isomorphic-git,
// κ-pinned) as NATIVE exec on the browser peer (the CC-48 discipline) over the
// holospace's own workspace (CC-15) — the SAME `.git` content the guest's git
// reads (Law L1). This drives the REAL workbench and asserts, each observing the
// genuine UI / an independent oracle, never faked:
//
//   1. the SourceControl provider is LIVE in the real workbench (the `scm` count
//      goes from 0 to a registered "Git (holospace)" provider — the status-bar
//      marker the extension publishes only after activate()+refresh());
//   2. it reflects the REAL repository state — after Initialize Repository the
//      workspace's files appear as working-tree changes (status over 9p);
//   3. a commit made through the SCM input is a REAL Git commit — its oid
//      re-derives to the canonical Git object (Law L5, the object-format
//      authority) and HEAD points at it (the extension publishes the verified
//      marker only after that check);
//   4. PUSH reaches a real remote — a `git http-backend` bare repo (real git as
//      the independent oracle) receives the commit; `git -C <bare> log` shows the
//      SAME sha + message the in-browser commit produced.
//
// Hermetic: the remote is a local bare repo served same-origin; no third party.
import http from "node:http";
import { readFile, stat, mkdtemp, rm } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync, spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import os from "node:os";
import path from "node:path";
import { composeWorkbenchHtml, WORKBENCH_PIN, BUILTIN_EXTENSIONS } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(DIR, "../../..");
const BOOTSTRAP = "@vscode/test-web@0.0.80";
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const twDir = path.join(DIR, "node_modules/@vscode/test-web");
// Serve EVERY builtin the composed workbench declares (BUILTIN_EXTENSIONS) —
// a declared-but-unserved builtin 404s and destabilizes the extension host.
const extDir = (name) => path.join(DIR, "builtin-extensions", name);
const cc16 = path.join(ROOT, "vv/artifacts/cc16");
const cc18 = path.join(ROOT, "vv/artifacts/cc18");

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("SCM-GIT-TEST: FAIL —", m)));

async function present(p) { try { await stat(path.join(DIR, p)); return true; } catch { return false; } }
if (!(await present("pkg/holospaces_web_bg.wasm"))) {
  console.error("SCM-GIT-TEST: RED — the wasm peer (pkg/) is absent; run vv/suites/cc51-scm-git.sh");
  console.error("  (it builds the peer with wasm-pack before driving this witness).");
  process.exit(1);
}
// `git` is the independent oracle for the push assertion.
try { execSync("git --version", { stdio: "ignore" }); }
catch { console.error("SCM-GIT-TEST: SKIP — git (the push oracle) is absent"); process.exit(127); }

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

// κ-verify the workbench's executable core against the committed manifest (L5).
const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
  .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });
let coreOk = 0;
for (const { hash, file } of manifest) {
  if (createHash("sha256").update(await readFile(path.join(distDir, file))).digest("hex") === hash) coreOk++;
}
check(coreOk === manifest.length, `the workbench's executable core re-derives to its pinned κ (${coreOk}/${manifest.length} files, Law L5)`);

// ── A real git remote: a bare repo served over `git http-backend` (smart-http) ──
const remoteRoot = await mkdtemp(path.join(os.tmpdir(), "scm-remote-"));
execSync(`git init --bare -b main "${path.join(remoteRoot, "repo.git")}"`, { stdio: "ignore" });
execSync(`git -C "${path.join(remoteRoot, "repo.git")}" config http.receivepack true`, { stdio: "ignore" });
const GIT_BACKEND = execSync("git --exec-path").toString().trim() + "/git-http-backend";

// Serve git-http-backend as a CGI under /git/* on the SAME origin as the page (no CORS).
function serveGit(req, res, rel) {
  const m = rel.match(/^\/git(\/.*)$/);
  if (!m) return false;
  const url = new URL(req.url, "http://x");
  const env = {
    ...process.env,
    GIT_PROJECT_ROOT: remoteRoot,
    GIT_HTTP_EXPORT_ALL: "1",
    PATH_INFO: m[1],
    REQUEST_METHOD: req.method,
    QUERY_STRING: url.search.replace(/^\?/, ""),
    CONTENT_TYPE: req.headers["content-type"] || "",
    REMOTE_USER: "witness",
  };
  const cgi = spawn(GIT_BACKEND, { env });
  const chunks = [];
  cgi.stdout.on("data", (d) => chunks.push(d));
  cgi.stdout.on("end", () => {
    const buf = Buffer.concat(chunks);
    const sep = buf.indexOf("\r\n\r\n");
    const headerBlk = buf.slice(0, sep).toString();
    const body = buf.slice(sep + 4);
    let status = 200;
    for (const line of headerBlk.split("\r\n")) {
      const sm = line.match(/^Status:\s*(\d+)/i);
      if (sm) status = parseInt(sm[1], 10);
      else { const i = line.indexOf(":"); if (i > 0) res.setHeader(line.slice(0, i), line.slice(i + 1).trim()); }
    }
    res.writeHead(status);
    res.end(body);
  });
  req.pipe(cgi.stdin);
  return true;
}

const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".gz": "application/gzip", ".ico": "image/x-icon",
};
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  if (rel.startsWith("/git/")) { if (serveGit(req, res, rel)) return; }
  const send = (b, ct) => { res.writeHead(200, { "content-type": ct || "application/octet-stream" }); res.end(b); };
  try {
    if (rel === "/" || rel === "/workbench.html") return send(html, "text/html");
    for (const name of BUILTIN_EXTENSIONS) {
      const pre = `/ext/${name}/`;
      if (rel.startsWith(pre)) return send(await readFile(path.join(extDir(name), rel.slice(pre.length))), TYPES[path.extname(rel)]);
    }
    if (rel.startsWith("/pkg/")) return send(await readFile(path.join(DIR, rel)), TYPES[path.extname(rel)]);
    if (rel === "/devcontainer-net-kernel.gz") return send(await readFile(path.join(cc16, "kernel/Image.gz")), "application/gzip");
    if (rel === "/devcontainer-lsp-layer.tar.gz") return send(await readFile(path.join(cc18, "image/blobs/sha256", cc18Layer)), "application/gzip");
    return send(await readFile(path.join(distDir, rel)), TYPES[path.extname(rel)]);
  } catch { res.writeHead(404).end("not found"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const remoteUrl = `http://127.0.0.1:${port}/git/repo.git`;

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("SCM-GIT-TEST: pageerror —", e.message));
const cclog = [];
const errlog = [];
page.on("pageerror", (e) => errlog.push("pageerror: " + e.message));
page.on("requestfailed", (r) => { const u = r.url(); if (!u.startsWith("data:")) errlog.push("reqfail: " + u.slice(0, 120)); });
page.on("console", (m) => {
  const t = m.text();
  if (t.includes("[CC51]") || t.includes("HOLOSPACE-SCM")) { cclog.push(t); console.log("  " + t); }
  if (/error|failed|exception/i.test(t)) errlog.push(t);
});

// Drive a command through the real command palette (the user path). Selects the
// matching row by its visible label (deterministic — not the MRU-highlighted
// row Enter would pick), so the exact command runs every time.
async function runCommand(title) {
  const input = page.locator(".quick-input-widget .input, .quick-input-box input").first();
  for (let attempt = 0; attempt < 3; attempt++) {
    await page.keyboard.press("Control+Shift+P");
    try {
      await input.waitFor({ state: "visible", timeout: 5000 });
      break;
    } catch {
      if (attempt === 2) throw new Error(`command palette did not open for "${title}"`);
    }
  }
  await input.fill(`>${title}`);
  await page.waitForTimeout(700);
  // Find the row whose label matches by TOKENS (order-independent: the palette
  // renders the title before its category, so a "holospace Git: Push" substring
  // never matches — but the tokens holospace/git/push all appear). Click it with
  // a real mouse event so the exact command runs (Enter would pick the
  // MRU-highlighted row, not necessarily this one).
  const idx = await page.evaluate((t) => {
    const norm = (s) => (s || "").toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
    const tokens = norm(t).split(" ").filter(Boolean);
    const rows = [...document.querySelectorAll(".quick-input-list .monaco-list-row")];
    return rows.findIndex((r) => { const x = norm(r.innerText); return tokens.every((tok) => x.includes(tok)); });
  }, title);
  if (idx >= 0) await page.locator(".quick-input-list .monaco-list-row").nth(idx).click();
  else await page.keyboard.press("Enter");
  await page.waitForTimeout(700);
}

// Provide the next quick-input answer (a showInputBox / showQuickPick prompt the
// command opens), waiting for the widget and typing the value.
async function answerInput(value) {
  // The text input (class `.input`) — NOT the widget's hidden "toggle all"
  // checkbox, which a bare `input` selector would match first.
  const qi = page.locator(".quick-input-widget input.input, .quick-input-box input.input").first();
  await qi.waitFor({ state: "visible", timeout: 12000 });
  await page.waitForTimeout(200);
  await qi.fill(value);
  await page.waitForTimeout(200);
  await page.keyboard.press("Enter");
  await page.waitForTimeout(500);
}
const bodyHas = (re) =>
  page.waitForFunction((src) => new RegExp(src).test(document.body.innerText || ""), re.source, { timeout: 120000 })
    .then(() => true).catch(() => false);

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (1) The SourceControl provider is LIVE in the real workbench (no repo yet).
  const live = await bodyHas(/HOLOSPACE-SCM-(NOREPO|LIVE)/);
  check(live, "the holospace-scm SourceControl provider is registered + live in the real workbench (scm count 0 → a provider)");

  // (2) Initialize a repository; the seeded workspace files appear as changes.
  await runCommand("holospace Git: Initialize Repository");
  const reflectsState = await page
    .waitForFunction(() => /HOLOSPACE-SCM-LIVE[^\n]*changes=([1-9]\d*)/.test(document.body.innerText || ""), null, { timeout: 120000 })
    .then(() => true).catch(() => false);
  check(reflectsState, "the SCM view reflects the REAL repository state — the workspace's files are working-tree changes over 9p");

  // (3) Commit through the SCM input → a REAL Git commit, re-derived (Law L5).
  await runCommand("View: Show Source Control");
  const scmInput = page.locator(".scm-editor textarea, .scm-editor-container textarea, .scm-editor .inputarea").first();
  await scmInput.waitFor({ timeout: 15000 });
  await scmInput.focus();
  await page.keyboard.type("witness: the first commit");
  await page.waitForTimeout(300);
  await runCommand("holospace Git: Commit");
  const committed = await page
    .waitForFunction(() => /HOLOSPACE-SCM-COMMIT=[0-9a-f]{40}/.test(document.body.innerText || ""), null, { timeout: 120000 })
    .then(() => true).catch(() => false);
  check(committed, "a commit made through the SCM input is published as a real Git commit (40-hex oid)");
  const verified = cclog.find((l) => /HOLOSPACE-SCM-VERIFIED=[0-9a-f]{40}/.test(l));
  check(!!verified, "the commit oid RE-DERIVES to the canonical Git object and HEAD points at it (Law L5, the object-format authority)");
  const sha = (verified && verified.match(/[0-9a-f]{40}/)[0]) || null;

  // (4) Push to the real bare remote; `git -C <bare> log` is the independent
  // oracle. Push with no upstream prompts for the URL (real git's flow) — a
  // single input box.
  await runCommand("holospace Git: Push");
  await answerInput(remoteUrl);
  const pushed = await page
    .waitForFunction(() => /HOLOSPACE-SCM-PUSH=origin\/main/.test(document.body.innerText || ""), null, { timeout: 120000 })
    .then(() => true).catch(() => false);
  check(pushed, "PUSH over the smart-http pack-protocol reports success");

  let remoteSha = "";
  let remoteMsg = "";
  try {
    remoteSha = execSync(`git -C "${path.join(remoteRoot, "repo.git")}" rev-parse refs/heads/main`).toString().trim();
    remoteMsg = execSync(`git -C "${path.join(remoteRoot, "repo.git")}" log -1 --format=%s main`).toString().trim();
  } catch (e) { /* ref absent — push did not land */ }
  check(
    remoteSha && sha && remoteSha === sha,
    `the bare remote RECEIVED the commit — independent oracle \`git log\` shows the same sha (${remoteSha || "absent"} ${remoteSha === sha ? "==" : "!="} ${sha})`,
  );
  check(remoteMsg === "witness: the first commit", `the pushed commit's message round-tripped to the remote ("${remoteMsg}")`);

  console.log(
    failed
      ? "SCM-GIT-TEST: FAILED"
      : "SCM-GIT-TEST: PASS (Source Control over the holospace's own virtio-9p workspace — a real Git engine on the browser peer; status, a Law-L5-verified commit, and a push received by a real git remote, no server outside the holospace)",
  );
} catch (e) {
  failed = true;
  console.error("SCM-GIT-TEST: error —", e && e.message);
  try { await page.screenshot({ path: path.join(DIR, "scm-test-failure.png") }); } catch {}
  if (errlog.length) console.error("  page errors:\n   " + errlog.slice(-12).join("\n   "));
} finally {
  await browser.close();
  server.close();
  await rm(remoteRoot, { recursive: true, force: true }).catch(() => {});
}
process.exit(failed ? 1 : 0);
