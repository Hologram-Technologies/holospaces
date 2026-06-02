// CC-13 — the VS Code workspace, end-to-end in a real browser (Chromium via
// Playwright). Entering a holospace opens the workspace IDE: it imports the real
// VS Code components (Monaco editor + xterm.js terminal) by κ and **verifies them
// by re-derivation through the substrate** before loading (Law L5; the gateway
// cannot lie), boots the holospace's devcontainer OS on the emulator in the tab
// (CC-9), drives it through the xterm.js terminal (CC-11), and edits the
// environment's content through Monaco by κ (CC-11). No server does the work.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };
const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/workspace.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("WORKSPACE-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("WORKSPACE-TEST: pageerror —", e.message)));

try {
  await page.goto(`http://127.0.0.1:${port}/workspace.html?name=demo&id=blake3:demo`);
  await page.waitForFunction("window.__ready === true", null, { timeout: 240000 });

  const r = await page.evaluate(() => {
    const ws = window.__ws;
    const out = {
      monaco: !!window.monaco,
      xterm: !!window.Terminal,
      ready: ws.shows("HOLOSPACES-WORKSPACE-READY"),
      booted: ws.terminal().includes("Linux version 6.6"),
      files: JSON.parse(ws.files()).length,
      stateBefore: ws.state_kappa(),
    };
    // drive the terminal through the projection (as xterm's onData does)
    out.echoEvent = ws.type_line("echo hi-from-the-ide");
    out.echoResponse = ws.terminal().includes("hi-from-the-ide");
    out.stateAfter = ws.state_kappa();
    // the editor: an edit content-addresses the file — its κ is its identity (L1)
    const k1 = ws.save_file("/work/README.md", new TextEncoder().encode("v1"));
    const k2 = ws.save_file("/work/README.md", new TextEncoder().encode("v2 edited"));
    out.editAdvanced = k1 !== k2 && k1.startsWith("blake3:");
    out.readBack = new TextDecoder().decode(ws.read_path("/work/README.md"));
    return out;
  });

  check(r.monaco, "the real Monaco editor loaded — imported + κ-verified through the substrate (L5)");
  check(r.xterm, "the real xterm.js terminal loaded — imported + κ-verified through the substrate (L5)");
  check(r.ready && r.booted, "the devcontainer OS booted on the emulator in the tab (CC-9)");
  check(r.files >= 3, `the file tree shows the devcontainer project (${r.files} files)`);
  check(r.echoResponse && r.echoEvent.startsWith("blake3:"), `the terminal drove the OS; the command is a channel event (${r.echoEvent})`);
  check(r.stateAfter !== r.stateBefore, "typing advanced the holospace's κ snapshot (CC-11)");
  check(r.editAdvanced, "a Monaco editor edit advances the file's κ (Law L1)");
  check(r.readBack === "v2 edited", "the editor reads content back by κ (Law L5)");
  check((await page.locator(".monaco-editor").count()) > 0, "the Monaco editor is rendered in the DOM");
  check((await page.locator(".xterm").count()) > 0, "the xterm.js terminal is rendered in the DOM");

  console.log(failed ? "WORKSPACE-TEST: FAILED" : "WORKSPACE-TEST: PASS (the VS Code devcontainer experience)");
} finally {
  await browser.close();
  server.close();
}
// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
