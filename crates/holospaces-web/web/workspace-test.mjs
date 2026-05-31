// End-to-end workspace test in a real browser (Chromium via Playwright): the
// browser peer boots a real RISC-V Linux kernel on the system emulator IN THE
// TAB (CC-9), then the workspace projection drives it (CC-11) — a terminal whose
// commands are canonical events advancing the holospace's κ snapshot, and an
// editor that addresses content by κ. No server does the work; the browser is
// the machine (Law L1).
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const ROOT = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };

const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(ROOT, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (cond, msg) =>
  cond ? console.log("  ✓", msg) : ((failed = true), console.error("WORKSPACE-TEST: FAIL —", msg));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("WORKSPACE-TEST: pageerror —", e.message)));

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.__ready === true", null, { timeout: 20000 });

  // Boot a real Linux kernel and drive the terminal — entirely in page context.
  // Bounded by an instruction budget; yields so the event loop stays alive.
  const r = await page.evaluate(async () => {
    const out = {};
    async function gunzip(url) {
      const res = await fetch(url);
      const stream = res.body.pipeThrough(new DecompressionStream("gzip"));
      return new Uint8Array(await new Response(stream).arrayBuffer());
    }
    const kernel = await gunzip("./workspace-kernel.gz");
    const dtb = new Uint8Array(await (await fetch("./workspace.dtb")).arrayBuffer());
    out.kernelBytes = kernel.length;

    const ws = window.hs.Workspace.boot(kernel, dtb, 128 * 1024 * 1024, 0x80000000, 0x87000000);
    const READY = "HOLOSPACES-WORKSPACE-READY";
    let steps = 0;
    while (!ws.shows(READY) && !ws.halted && steps < 6_000_000_000) {
      ws.run(20_000_000);
      steps += 20_000_000;
      await new Promise((r) => setTimeout(r, 0)); // keep the tab alive
    }
    out.ready = ws.shows(READY);
    out.bootedLinux = ws.terminal().includes("Linux version 6.6");
    out.stateAfterBoot = ws.state_kappa();

    // Drive the terminal: a real command → a real response, and a canonical event.
    const echoEvent = ws.type_line("echo hello-from-the-browser");
    out.echoEvent = echoEvent;
    out.echoResponse = ws.terminal().includes("hello-from-the-browser");
    const versionEvent = ws.type_line("version");
    out.versionResponse = ws.terminal().includes("Linux version 6.6");
    out.stateAfterInput = ws.state_kappa();
    out.events = ws.channel();

    // The editor: an edit content-addresses the file (its κ is its identity, L1).
    const k1 = ws.save_file("/work/readme.txt", new TextEncoder().encode("hello"));
    const k2 = ws.save_file("/work/readme.txt", new TextEncoder().encode("hello, edited"));
    out.editK1 = k1;
    out.editK2 = k2;
    out.readBack = new TextDecoder().decode(ws.open_file(k1));

    // Power off the machine from userspace.
    ws.type_line("exit");
    out.halted = ws.halted;
    return out;
  });

  check(r.ready, "a real Linux kernel booted to a ready terminal in the browser (CC-9)");
  check(r.bootedLinux, "the terminal shows the real kernel banner (Linux 6.6, RISC-V)");
  check(r.echoResponse, `the terminal ran a real command (echo → response); event κ ${r.echoEvent}`);
  check(r.versionResponse, "a 'version' command printed the real /proc/version");
  check(
    typeof r.echoEvent === "string" && r.echoEvent.startsWith("blake3:"),
    "each command is published as a canonical event on the channel (L1/L2)",
  );
  check(r.stateAfterInput !== r.stateAfterBoot, "typing advanced the holospace's κ snapshot (CC-11)");
  check(r.editK1 !== r.editK2 && r.editK1.startsWith("blake3:"), "an editor edit advances the file's κ (L1)");
  check(r.readBack === "hello", "the editor reads content back by κ (L5)");
  check(r.halted, "the OS powered off from userspace (SBI system reset)");

  console.log(failed ? "WORKSPACE-TEST: FAILED" : "WORKSPACE-TEST: PASS (a real OS + workspace in the browser)");
} finally {
  await browser.close();
  server.close();
}

process.exitCode = failed ? 1 : 0;
