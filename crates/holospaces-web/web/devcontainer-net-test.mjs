// CC-16 in the browser — a devcontainer reaches the internet from a tab.
//
// The same wasm peer GitHub Pages serves boots a real Linux on the emulator,
// brings its virtio-net interface up with DHCP against the in-browser userspace
// TCP/IP NAT, and opens a TCP connection whose payload is tunnelled out over a
// WebSocket to the egress relay (there is no raw NIC behind a tab; ADR-014). The
// relay opens the real socket to a host server and pumps bytes back; the guest
// completes an HTTP exchange — NET-CONNECTED / NET-RECV / NET-DONE — entirely in
// the browser peer. The drive loop YIELDS to the event loop between run-chunks so
// the WebSocket delivers the host's bytes (the cooperative analogue of a host
// socket's reads arriving between the NAT's polls natively).
import http from "node:http";
import net from "node:net";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("NET-TEST: FAIL —", m)));

// 1) A real host server the guest's HTTP request must reach (the "internet").
//    A raw socket so the full response arrives in one segment (as the native
//    witness's server does), within the freestanding init's single read().
const target = net.createServer((sock) => {
  sock.on("data", () => {
    sock.end("HTTP/1.0 200 OK\r\nContent-Length: 22\r\n\r\nHELLO-FROM-HOST-SERVER");
  });
});
await new Promise((r) => target.listen(0, "127.0.0.1", r));
const tport = target.address().port;

// 2) The egress relay, port-forwarding the guest's 10.0.2.9:8080 to the host
//    server (the same guestfwd the native StdEgress witness uses).
process.env.REDIRECT = `10.0.2.9:8080=127.0.0.1:${tport}`;
const { startRelay } = await import("./relay.mjs");
const relay = await startRelay();
const rport = relay.address().port;

// 3) Serve the web peer assets.
const assets = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
await new Promise((r) => assets.listen(0, "127.0.0.1", r));
const aport = assets.address().port;

const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("NET-TEST: pageerror —", e.message)));

try {
  await page.goto(`http://127.0.0.1:${aport}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  const relayUrl = `ws://127.0.0.1:${rport}`;
  const r = await page.evaluate(async (relayUrl) => {
    const hs = window.hs;
    const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
    const gunzip = async (b) => {
      const s = new Response(b).body.pipeThrough(new DecompressionStream("gzip"));
      return new Uint8Array(await new Response(s).arrayBuffer());
    };

    // Assemble the devcontainer rootfs + gunzip the net kernel (the same Layer
    // Assembler path as the offline devcontainer, in the browser).
    const layer = await bytes("./devcontainer-net-layer.tar.gz");
    const kernel = await gunzip(await bytes("./devcontainer-net-kernel.gz"));
    const img = new hs.DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const rootfs = img.assemble();

    // Boot networked: virtio-net + the NAT, egress tunnelled to the relay.
    const ws = hs.Workspace.boot_devcontainer_net(kernel, rootfs, relayUrl);
    let ipcfg = false, connected = false, recv = false, done = false;
    for (let i = 0; i < 3000; i++) {
      const halted = ws.run(10_000_000);
      ipcfg = ipcfg || ws.shows("IP-Config: Complete");
      connected = connected || ws.shows("NET-CONNECTED");
      recv = recv || ws.shows("HELLO-FROM-HOST-SERVER");
      done = done || ws.shows("NET-DONE");
      if (halted || done) break;
      // Yield so the WebSocket delivers the relay's frames into the NAT.
      await new Promise((res) => setTimeout(res, 0));
    }
    return { ipcfg, connected, recv, done, tail: ws.terminal().split("\n").slice(-10).join("\n") };
  }, relayUrl);

  check(r.ipcfg, "the OS configured its interface via DHCP over virtio-net — in the browser (IP-Config: Complete)");
  check(r.connected, "the OS opened a TCP connection through the NAT, tunnelled over the WebSocket relay (NET-CONNECTED)");
  check(r.recv, "the OS received the host server's HTTP response through the egress relay (HELLO-FROM-HOST-SERVER)");
  check(r.done, "the network exchange completed in the browser (NET-DONE)");
  if (!r.recv) console.error("  console tail:\n" + r.tail);

  console.log(failed ? "NET-TEST: FAILED" : "NET-TEST: PASS (the devcontainer reached the internet from the browser, tunnelled over the relay)");
} finally {
  await browser.close();
  assets.close();
  relay.close();
  target.close();
}

// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
