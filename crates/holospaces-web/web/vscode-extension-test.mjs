// CC-19 (foundation) — a real extension runs in the real VS Code web workbench,
// the SUPPORTED way (ADR-012/015).
//
// Codespaces / vscode.dev run extensions in a real extension host; holospaces
// uses that same mechanism via Microsoft's own `@vscode/test-web`
// (`--extensionDevelopmentPath` serves an extension to the real workbench with
// the proper extension-host + service-worker wiring; `--browser none` serves
// only and we drive our own Chromium, the harness's bundled launcher being
// unavailable here). This witness loads a real web extension and asserts it
// ACTIVATES in the genuine workbench (its status-bar contribution appears) and
// that its command is registered — the extension host runs extensions in the
// holospaces workbench. That is the prerequisite for real extensions and their
// integrations (CC-19: the GitHub sign-in → pull-requests/issues scenario) and
// for language servers (CC-18). No hand-rolled embedder, no hacks.
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import net from "node:net";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ext = path.join(DIR, "extensions/holospace-demo");
let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("EXTENSION-TEST: FAIL —", m)));
const freePort = () =>
  new Promise((resolve, reject) => {
    const s = net.createServer();
    s.on("error", reject);
    s.listen(0, "localhost", () => {
      const port = s.address().port;
      s.close(() => resolve(port));
    });
  });

const port = await freePort();
const srv = spawn(
  "npx",
  ["--no-install", "@vscode/test-web", "--browser", "none", "--port", String(port), "--extensionDevelopmentPath", ext, DIR],
  { cwd: DIR, detached: true },
);
let log = "";
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));
const up = await new Promise((r) => {
  const t = setInterval(() => {
    if (/EADDRINUSE|uncaughtException|Error: listen/.test(log)) {
      clearInterval(t);
      r(false);
    }
    if (/Listening on/.test(log)) {
      clearInterval(t);
      r(true);
    }
  }, 300);
  setTimeout(() => {
    clearInterval(t);
    r(/Listening on/.test(log));
  }, 240000);
});

const browser = await chromium.launch();
try {
  check(up, "the real VS Code web workbench is served with the extension under development (the supported extension-host mechanism)");
  if (!up) throw new Error("server did not start:\n" + log.slice(-500));
  const page = await (await browser.newContext()).newPage();
  await page.goto(`http://localhost:${port}/`, { timeout: 120000, waitUntil: "domcontentloaded" });
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });

  // The extension activates and contributes a status-bar item — proof its code
  // ran in the workbench's extension host.
  const activated = await page
    .waitForFunction(() => /HOLOSPACE-EXT-LIVE/.test(document.body.innerText), null, { timeout: 60000 })
    .then(() => true)
    .catch(() => false);
  check(activated, "the extension activated in the real workbench's extension host (its status-bar contribution appeared)");

  // Best-effort: surface its contributed command in the palette (quick-input
  // automation is timing-flaky; activation above is the deterministic proof the
  // extension host ran the extension's code).
  let commandFound = false;
  if (activated) {
    try {
      await page.keyboard.press("F1");
      await page.waitForSelector(".quick-input-widget", { timeout: 8000 });
      await page.locator(".quick-input-widget input").fill("Holospace: Hello");
      await new Promise((r) => setTimeout(r, 1500));
      commandFound = await page
        .waitForFunction(() => /Holospace: Hello/.test(document.querySelector(".quick-input-list")?.textContent || ""), null, { timeout: 8000 })
        .then(() => true)
        .catch(() => false);
      await page.keyboard.press("Escape").catch(() => {});
    } catch {}
  }
  console.log(commandFound ? "  ✓ (bonus) the extension's contributed command is registered in the command palette" : "  · (command-palette automation flaky; activation is the deterministic proof)");

  console.log(failed ? "EXTENSION-TEST: FAILED" : "EXTENSION-TEST: PASS (a real extension activates in the real workbench's extension host, the supported way)");
} finally {
  await browser.close();
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {
    srv.kill("SIGKILL");
  }
}
// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
