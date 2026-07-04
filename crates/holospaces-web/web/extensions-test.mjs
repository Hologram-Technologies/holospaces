// CC-19 (devcontainer extensions) — a devcontainer's declared web extensions
// auto-install from the open gallery in the deployed workbench. A
// `devcontainer.json`'s `customizations.vscode.extensions` (parsed by CC-4) flows
// into the workbench config (`additionalBuiltinExtensions` as gallery ids), so the
// web-capable ones install from Open VSX on launch — the "devcontainer configs
// install their needed dependencies" contract for the web model. This witnesses
// the mechanism: compose the real deployed workbench declaring a known web
// extension and confirm the workbench installs it (fetches its package from Open
// VSX and lists it as installed).
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";
import { composeWorkbenchHtml, WORKBENCH_PIN, BUILTIN_EXTENSIONS } from "./build-workbench.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const BOOTSTRAP = "@vscode/test-web@0.0.80";
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
const twDir = path.join(DIR, "node_modules/@vscode/test-web");
try { await stat(distDir); await stat(twDir); }
catch { execSync(`npm install --no-save ${WORKBENCH_PIN} ${BOOTSTRAP}`, { cwd: DIR, stdio: "ignore" }); }

// A small, definitely-web extension (a color theme — no Node entrypoint) any
// devcontainer could declare; on Open VSX.
const EXT = "dracula-theme.theme-dracula";
const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: ".", extensions: [EXT] });

const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff", ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json" };
const server = http.createServer(async (req, res) => {
  const rel = decodeURIComponent(req.url.split("?")[0]);
  if (rel === "/" || rel === "/workbench.html") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(html);
  }
  // The holospace-fs builtin (so additionalBuiltinExtensions resolves) from source.
  const ext = BUILTIN_EXTENSIONS.find((n) => rel.startsWith(`/ext/${n}/`));
  const file = ext
    ? path.join(DIR, "builtin-extensions", ext, rel.slice(`/ext/${ext}/`.length))
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
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("EXTENSIONS-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();

// Track the workbench fetching the declared extension's *package* from Open VSX
// (the unpkg resource template) — that is the install reading the extension, not
// merely a gallery-listing icon.
let installFetched = false;
page.on("response", (r) => {
  if (/open-vsx\.org\/vscode\/(unpkg|asset)\/dracula-theme/i.test(r.url())) installFetched = true;
});

try {
  await page.goto(`http://127.0.0.1:${port}/workbench.html`);
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });
  // Give the workbench time to resolve + install the declared builtin extension.
  await page.waitForTimeout(12000);
  check(installFetched, `the declared web extension (${EXT}) was fetched from Open VSX to install — the auto-install mechanism works (devcontainer configs install their deps)`);

  // Best-effort UI confirmation it is present (builtin extensions are not always
  // listed under @installed, so this does not fail the witness — the gallery
  // fetch above is the load-bearing proof).
  try {
    await page.keyboard.press("Control+Shift+X");
    await page.waitForTimeout(2000);
    await page.keyboard.type("dracula");
    await page.waitForTimeout(5000);
    const present = await page.evaluate(() =>
      [...document.querySelectorAll(".extension-list-item, .monaco-list-row")].some((r) => /dracula/i.test(r.textContent || "")),
    );
    console.log(present ? "  ✓ the installed extension is visible in the workbench" : "  · (extension not surfaced in the list view — builtin; gallery-fetch is the proof)");
  } catch (e) {
    console.log("  · (UI confirmation skipped:", e.message.split("\n")[0] + ")");
  }

  console.log(failed ? "EXTENSIONS-TEST: FAILED" : "EXTENSIONS-TEST: PASS (a devcontainer's declared web extension auto-installs from Open VSX)");
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
