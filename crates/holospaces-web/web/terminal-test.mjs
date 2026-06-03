// CC-11 (raw terminal, browser) — the integrated terminal over the devcontainer
// OS console is a *real* terminal in the browser peer: raw keystrokes reach the
// guest (which echoes + line-edits them and handles Ctrl-C), and the output
// streams back as a delta. The same wasm GitHub Pages serves; the host CC-11 raw
// witness proves the substrate behaviour against the qemu oracle, this proves the
// browser Workspace terminal API (feed_input + terminal_delta) end to end.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };
const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("TERMINAL-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("TERMINAL-TEST: pageerror —", e.message)));

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  const r = await page.evaluate(async () => {
    const hs = window.hs;
    const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
    const gunzip = async (b) =>
      new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
    const enc = new TextEncoder();
    const dec = new TextDecoder();

    // The deployed devcontainer base: the CC-22 BusyBox layer (its busybox +
    // setsid/stty applets back the persistent interactive shell), not the
    // init-only CC-14 layer.
    const layer = await bytes("./devcontainer-busybox-layer.tar.gz");
    const kernel = await gunzip(await bytes("./devcontainer-kernel.gz"));
    const img = new hs.DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    // The deployed, *bootable* rootfs — the persistent interactive shell.
    const ws = hs.Workspace.boot_devcontainer(kernel, img.assemble_bootable(128 * 1024 * 1024));

    // Stream output the way the terminal does: accumulate terminal_delta().
    let out = "";
    let everReEmitted = false;
    const pump = (chunks) => {
      for (let i = 0; i < chunks; i++) {
        if (!ws.halted) ws.run(8_000_000);
        const d = ws.terminal_delta();
        out += dec.decode(d);
      }
    };
    // Boot to the interactive shell.
    for (let i = 0; i < 250; i++) {
      pump(1);
      if (out.includes("holospace devcontainer ready")) break;
    }
    const booted = out.includes("holospace devcontainer ready");

    // terminal_delta must never re-emit: after draining, an immediate second call
    // (no run) returns nothing.
    if (ws.terminal_delta().length !== 0) everReEmitted = true;

    // Raw input + guest echo/line-edit: type `echo abZ`, backspace, `c`, Enter.
    const mark = out.length;
    ws.feed_input(enc.encode("echo abZ"));
    pump(25);
    ws.feed_input(enc.encode("\x7f")); // backspace the Z
    pump(12);
    ws.feed_input(enc.encode("c\n"));
    pump(50);
    const afterEcho = out.slice(mark);

    // Ctrl-C interrupts a blocking command.
    const mark2 = out.length;
    ws.feed_input(enc.encode("sleep 999999\n"));
    pump(40);
    ws.feed_input(enc.encode("\x03")); // Ctrl-C
    pump(40);
    ws.feed_input(enc.encode("echo AFTER-CTRLC\n"));
    pump(60);
    const afterCtrlC = out.slice(mark2);

    return { booted, everReEmitted, afterEcho, afterCtrlC };
  });

  check(r.booted, "the deployed devcontainer booted to its interactive shell in the browser");
  check(!r.everReEmitted, "terminal_delta() never re-emits — it streams only newly-produced bytes (O(new), not O(total))");
  check(r.afterEcho.includes("echo abZ"), "raw keystrokes reached the guest and were echoed by its tty (not by JS)");
  check(
    r.afterEcho.split("\n").some((l) => l.trim() === "abc"),
    "the backspace edited the line in the guest, so `echo abc` ran (output `abc`)",
  );
  check(
    r.afterCtrlC.split("\n").some((l) => l.trim() === "AFTER-CTRLC"),
    "Ctrl-C interrupted `sleep 999999` (SIGINT reached the foreground), so the next command ran",
  );

  console.log(failed ? "TERMINAL-TEST: FAILED" : "TERMINAL-TEST: PASS (raw interactive terminal over the devcontainer console, in the browser)");
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
