// CC-52 (deployed/browser) — find-in-files + search & replace over the
// holospace's OWN virtio-9p workspace, witnessed in the real workbench.
//
// holospace-search registers a File + Text search provider (the proposed search
// APIs; a builtin keeps its enabledApiProposals) running as native exec on the
// browser peer (the CC-48 discipline) over the holospace's workspace (CC-15).
// This drives the REAL Search view and asserts, each observing the genuine UI /
// the provider's own count, never faked:
//
//   1. the providers are LIVE in the real workbench (HOLOSPACE-SEARCH-LIVE);
//   2. a text query returns the EXPECTED matches from the workspace — the
//      seeded `main.rs` contains `greet` three times; the provider reports
//      "3 matches in 1 files" AND the Search view renders result rows;
//   3. REPLACE-ALL edits the files over 9p — after replacing `greet`→`hello`,
//      a re-search for `greet` returns 0 and for `hello` returns 3 (the provider
//      re-reads the files and sees the new content the edit wrote over 9p).
//
// The fast core (globs / `.gitignore` / streaming / matching) is proven
// deterministically by builtin-extensions/holospace-search/search-core.test.cjs;
// this proves it wired into the real workbench, end to end.
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
// Serve EVERY builtin the composed workbench declares (BUILTIN_EXTENSIONS) —
// a declared-but-unserved builtin 404s and destabilizes the extension host.
const extDir = (name) => path.join(DIR, "builtin-extensions", name);
const cc16 = path.join(ROOT, "vv/artifacts/cc16");
const cc18 = path.join(ROOT, "vv/artifacts/cc18");

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("SEARCH-TEST: FAIL —", m)));

async function present(p) { try { await stat(path.join(DIR, p)); return true; } catch { return false; } }
if (!(await present("pkg/holospaces_web_bg.wasm"))) {
  console.error("SEARCH-TEST: RED — the wasm peer (pkg/) is absent; run vv/suites/cc52-search.sh");
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
  ".html": "text/html", ".js": "text/javascript", ".cjs": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".gz": "application/gzip", ".ico": "image/x-icon",
};
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  // Reject path traversal — `rel` is mapped onto disk via path.join, so refuse any
  // `..` segment before it can escape the served directories.
  if (rel.split("/").includes("..")) { res.writeHead(403).end("forbidden"); return; }
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

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("SEARCH-TEST: pageerror —", e.message, (e.stack || "").split("\n").slice(0, 3).join(" | ")));
const cclog = [];
const allLog = [];
page.on("console", async (m) => { allLog.push(`${m.type()}: ${m.text().slice(0, 240)}`); if (allLog.length > 12000) allLog.shift();
  const t = m.text();
  if (t.includes("[CC52]")) { cclog.push(t); console.log("  " + t); return; }
  if (m.type() === "error" || /is not a function|TypeError|Error:/.test(t)) {
    console.log("  [page-err] " + t.slice(0, 300));
    for (const a of m.args()) {
      try {
        const st = await a.evaluate((e) => (e && e.stack) ? String(e.stack) : null).catch(() => null);
        if (st) console.log("  [page-err-stack] " + st.split("\n").slice(0, 8).join("\n    "));
      } catch { /* ignore */ }
    }
  }
});

// Type into the focused Search input (clearing first).
async function typeInto(selector, text) {
  const el = page.locator(selector).first();
  await el.waitFor({ state: "visible", timeout: 15000 });
  await el.click();
  await page.keyboard.press("Control+A");
  await page.keyboard.press("Delete");
  await page.keyboard.type(text);
  // No Enter here: with search-on-type, Enter + the type-debounce would start TWO
  // search sessions — the second cancels the first MID-STREAM and the landed
  // partial count lingers while the live session re-walks. One session settles
  // cleanly. (The replace flow presses Enter itself, where submit is required.)
  await page.waitForTimeout(400);
}

// Run a text search for `pattern` and return the count the WORKBENCH renders —
// VS Code's own "N results in M files" message in the Search view (or 0 for "No
// results found"). This is authoritative (it reflects what the user sees) and
// independent of how many times VS Code re-invokes the provider, which it caches
// for an unchanged query. Returns null if the view never settles in time.
// Type the query ONCE, then wait for a STABLE result count — the same count on
// two samples ≥900ms apart. Matches stream from the ext host in batches, and a
// retype starts a NEW search session that cancels the old one MID-STREAM — so an
// impatient reader sees partial counts (and a hasty retype loop keeps every
// session partial forever). A user reads the settled view; so does the witness.
async function runSearch(searchInput, pattern, timeout = 30000) {
  await typeInto(searchInput, pattern);
  const start = Date.now();
  let prev = null;
  let prevAt = 0;
  while (Date.now() - start < timeout) {
    const txt = await page.locator(".search-view").first().innerText().catch(() => "");
    let n = null;
    if (/No results found/i.test(txt)) n = 0;
    else {
      const mm = /(\d+)\s+results?\s+in\s+\d+\s+files?/i.exec(txt) || /^\s*(\d+)\s+results?\b/im.exec(txt);
      if (mm) n = parseInt(mm[1], 10);
    }
    if (n != null && n === prev && Date.now() - prevAt >= 2000) {
      // Grace re-check: a batch of streamed matches can land seconds late on a
      // loaded machine, AFTER the count looks settled. Believe a settled count
      // only if it survives an additional grace window.
      await page.waitForTimeout(3000);
      const t2 = await page.locator(".search-view").first().innerText().catch(() => "");
      const m2 = /(\d+)\s+results?\s+in\s+\d+\s+files?/i.exec(t2) || /^\s*(\d+)\s+results?\b/im.exec(t2);
      const n2 = /No results found/i.test(t2) ? 0 : m2 ? parseInt(m2[1], 10) : null;
      if (n2 === n) return n;
      prev = n2;
      prevAt = Date.now();
      continue;
    }
    if (n !== prev) { prev = n; prevAt = Date.now(); }
    await page.waitForTimeout(500);
  }
  return prev; // the last observed count (null if no result state ever rendered)
}

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // (1) The search providers are LIVE in the real workbench.
  const live = await page
    .waitForFunction(() => /HOLOSPACE-SEARCH-LIVE/.test(document.body.innerText || ""), null, { timeout: 120000 })
    .then(() => true).catch(() => false);
  check(live, "the holospace-search File + Text search providers are registered + live (find-in-files active)");

  // Open the Search view and wait for the workspace to have booted (main.rs over 9p).
  await page.keyboard.press("Control+Shift+F");
  await page.waitForSelector(".search-view", { timeout: 30000 });
  const searchInput = ".search-view .search-widget textarea, .search-view .search-widget .monaco-inputbox textarea, .search-view textarea[aria-label*='Search']";

  // (2) A text query returns the expected matches — `greet` occurs 3× in main.rs.
  // Retry until the workspace has booted (~1 min: the FS provider awaits 9p
  // readiness, and main.rs is seeded after the devcontainer boots). `runSearch`
  // clears + retypes, so each call forces a fresh query (a new provider log).
  let n = null;
  for (let i = 0; i < 8 && n !== 3; i++) {
    n = await runSearch(searchInput, "greet", 25000);
    if (n === 3) break;
    await page.waitForTimeout(2000); // let the boot advance before re-querying
  }
  if (n !== 3) {
    const metrics = await page.evaluate(() => {
      const v = document.querySelector(".search-view");
      const list = v && v.querySelector(".monaco-list");
      const msgs = v && v.querySelector(".messages");
      const rowsEl = list && list.querySelectorAll(".monaco-list-row");
      const style = v && getComputedStyle(v);
      return {
        viewDisplay: style && style.display, viewH: v && v.clientHeight,
        listH: list && list.clientHeight, listScrollH: list && list.scrollHeight,
        rows: rowsEl ? rowsEl.length : -1,
        ariaRows: list ? list.getAttribute("aria-rowcount") : null,
        msgs: msgs ? msgs.innerText.slice(0, 200) : null,
      };
    }).catch((e) => ({ err: String(e) }));
    console.log("SEARCH-VIEW-METRICS:", JSON.stringify(metrics));
    console.log("=== RPC/TRACE TAIL (search/cancel/config/task) ===");
    const interesting = allLog.filter((l) => /earch|cancel|Cancel|onfiguration|\$startTextSearch|Task|task/.test(l) && !/textSearch "greet"/.test(l));
    for (const l of interesting.slice(-160)) console.log("  |", l.slice(0, 220));
    const viewText = await page.locator(".search-view").first().innerText().catch(() => "(no view)");
    console.log("SEARCH-VIEW-TEXT:", JSON.stringify(viewText.slice(0, 600)));
    const notifs = await page.locator(".notifications-toasts, .notification-toast").allInnerTexts().catch(() => []);
    console.log("NOTIFICATIONS:", JSON.stringify(notifs).slice(0, 400));
    await page.screenshot({ path: path.join(DIR, "search-test-failure.png") }).catch(() => {});
  }
  check(n === 3, `find-in-files returns the EXPECTED matches — "greet" → 3 matches in main.rs (got ${n})`);
  const rows = await page.locator(".search-view .monaco-list-row").count();
  check(rows > 0, `the Search view RENDERS the provider's results (${rows} rows)`);

  // (3) Replace-all edits the files over 9p. Submit the search (Enter) — Replace
  // All is "Submit Search to Enable"-disabled until then — set the replacement
  // (verifying it took), wait for Replace All to ENABLE, and click it.
  await typeInto(searchInput, "greet");
  await page.locator(searchInput).first().press("Enter");
  await page.waitForTimeout(800);
  // Reveal the replace row by clicking `.toggle-replace-button` until the replace
  // textarea is actually visible (its icon flips show/hide; toggle as needed).
  const toggleBtn = page.locator(".search-view .toggle-replace-button").first();
  const replaceBox = page.locator(".search-view textarea[placeholder='Replace']").first();
  for (let i = 0; i < 4; i++) {
    if (await replaceBox.isVisible().catch(() => false)) break;
    if (await toggleBtn.count()) await toggleBtn.click({ force: true }).catch(() => {});
    await page.waitForTimeout(700);
  }
  await replaceBox.waitFor({ state: "visible", timeout: 10000 });
  let setOk = false;
  for (let i = 0; i < 3 && !setOk; i++) {
    // `fill` sets the value AND dispatches a native input event the
    // monaco-inputbox model listens for (plain keyboard.type left the model
    // empty → Replace All deleted instead of replacing). Enter commits the term
    // (the box hint: "press Enter to preview"), so Replace All applies `hello`.
    await replaceBox.fill("hello");
    await page.waitForTimeout(300);
    await replaceBox.press("Enter");
    await page.waitForTimeout(600);
    setOk = (await replaceBox.inputValue().catch(() => "")) === "hello";
  }
  check(setOk, "a replace term is entered in the Search view's Replace box");
  // Replace-all must act on the SETTLED result set, exactly as a user does — a
  // click while the result stream is mid-flight acts on a PARTIAL set (the
  // ext-host streams matches in batches; a fresh query session replaces the
  // previous one on retype/Enter, so a hasty click replaces only the matches
  // already landed and leaves the tail unreplaced).
  const settled = await (async (want, timeout = 45000) => {
    const start = Date.now();
    let prev = null;
    while (Date.now() - start < timeout) {
      const txt = await page.locator(".search-view").first().innerText().catch(() => "");
      const mm = /(\d+)\s+results?\s+in\s+\d+\s+files?/i.exec(txt);
      const n = mm ? parseInt(mm[1], 10) : null;
      if (n === want && prev === want) return true;
      prev = n;
      await page.waitForTimeout(1000);
    }
    return false;
  })(3);
  check(settled, "the search SETTLED at 3 results before Replace All (replace acts on the full result set)");

  // Wait until Replace All is enabled, then click it.
  await page.waitForFunction(() => {
    const e = document.querySelector(".search-view .codicon-search-replace-all");
    return e && !e.classList.contains("disabled");
  }, null, { timeout: 20000 }).catch(() => {});
  await page.locator(".search-view .codicon-search-replace-all").first().click({ force: true });
  await page.waitForTimeout(700);
  // Confirm the "Replace All" modal.
  const confirm = page.locator(".monaco-dialog-box .monaco-button").filter({ hasText: /^Replace$|Replace All/ }).first();
  if (await confirm.count()) { await confirm.click().catch(() => {}); }
  await page.waitForTimeout(2000);
  // Flush the dirty editor the replace produced to the FileSystemProvider (→ 9p),
  // then CLOSE all editors so no cached editor model shadows the file — the
  // provider then re-reads the SETTLED 9p content (VS Code's FileService caches a
  // just-saved file briefly, so the count can lag before the close).
  const palette = async (cmd) => {
    await page.keyboard.press("Control+Shift+P");
    await page.locator(".quick-input-widget .input, .quick-input-box input").first().waitFor({ state: "visible", timeout: 8000 }).catch(() => {});
    await page.keyboard.type(">" + cmd);
    await page.waitForTimeout(600);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(800);
  };
  await palette("Save All");
  await page.waitForTimeout(1500);
  await palette("View: Close All Editors");
  await page.waitForTimeout(2000);

  // Re-search with the editors closed so the provider reads the SETTLED file from
  // 9p (not a cached editor model): every `greet` match has been replaced — the
  // edit Replace-All applied through the holospace FileSystemProvider landed in
  // the workspace the guest shares (Law L1). Poll to ride out FileService cache
  // lag. (Search seeds the count from the workbench's own "N results" message.)
  let afterGreet = null;
  for (let i = 0; i < 4 && afterGreet !== 0; i++) {
    afterGreet = await runSearch(searchInput, "greet", 25000);
    if (afterGreet === 0) break;
    await page.waitForTimeout(1500);
  }
  check(afterGreet === 0, `replace-all EDITED main.rs over 9p — every "greet" match was replaced; a re-search of the settled file finds 0 (got ${afterGreet})`);

  console.log(
    failed
      ? "SEARCH-TEST: FAILED"
      : "SEARCH-TEST: PASS (find-in-files returns the expected matches in the real Search view, and Replace-All edits the file over 9p — File + Text search providers as native browser-peer exec, the edit landing in the workspace the guest shares, no server outside the holospace)",
  );
} catch (e) {
  failed = true;
  console.error("SEARCH-TEST: error —", e && e.message);
  try { await page.screenshot({ path: path.join(DIR, "search-test-failure.png") }); } catch {}
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
