// CC-18 (deployed, browser) + CC-33/ADR-020 — the deployed wasm peer gives the
// workbench real language intelligence from a language server running in the
// devcontainer OS, reached over the in-process substrate bridge. The host
// `cc18_lsp_bridge` witness proves the substrate behaviour against the lsp-types
// spec authority; this proves the *browser* Workspace bridge API
// (boot_devcontainer_bridged + dial_guest/guest_send/guest_recv) carries a full
// LSP session to the in-OS server, end to end, with no Node.
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
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("LSP-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("LSP-TEST: pageerror —", e.message)));

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

    // The deployed bridged devcontainer: the CC-16 net kernel + the CC-18 layer
    // (BusyBox + lsp-demo), booted with the in-process loopback bridge.
    const layer = await bytes("./devcontainer-lsp-layer.tar.gz");
    const kernel = await gunzip(await bytes("./devcontainer-net-kernel.gz"));
    const img = new hs.DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const ws = hs.Workspace.boot_devcontainer_bridged(kernel, img.assemble_bootable(128 * 1024 * 1024));

    // Boot until the in-OS language server is listening.
    let listening = false;
    for (let i = 0; i < 800; i++) {
      if (!ws.halted) ws.run(8_000_000);
      if (ws.shows("LSP-LISTENING")) {
        listening = true;
        break;
      }
    }
    if (!listening) return { listening: false, console: ws.terminal().slice(-400) };

    // Dial the server over the in-process bridge and drive a full LSP session.
    const id = ws.dial_guest(7000);
    for (let i = 0; i < 30; i++) ws.run(2_000_000);

    const frame = (msg) => {
      const body = enc.encode(JSON.stringify(msg));
      const header = enc.encode(`Content-Length: ${body.length}\r\n\r\n`);
      const f = new Uint8Array(header.length + body.length);
      f.set(header, 0);
      f.set(body, header.length);
      return f;
    };
    const DOC = "file:///workspace/main.rs";
    const SRC = "fn greet(name) {\n  // TODO: greet\n  return greet(name)\n}\n";
    const td = { uri: DOC };
    for (const m of [
      { jsonrpc: "2.0", id: 1, method: "initialize", params: { processId: null, capabilities: {} } },
      { jsonrpc: "2.0", method: "initialized", params: {} },
      { jsonrpc: "2.0", method: "textDocument/didOpen", params: { textDocument: { uri: DOC, languageId: "rust", version: 1, text: SRC } } },
      { jsonrpc: "2.0", id: 2, method: "textDocument/hover", params: { textDocument: td, position: { line: 0, character: 4 } } },
      { jsonrpc: "2.0", id: 3, method: "textDocument/completion", params: { textDocument: td, position: { line: 2, character: 2 } } },
      { jsonrpc: "2.0", id: 4, method: "textDocument/definition", params: { textDocument: td, position: { line: 2, character: 10 } } },
    ]) {
      ws.guest_send(id, frame(m));
    }

    // Drain + parse Content-Length frames from the server's replies.
    const findSep = (buf) => {
      for (let i = 0; i + 4 <= buf.length; i++)
        if (buf[i] === 13 && buf[i + 1] === 10 && buf[i + 2] === 13 && buf[i + 3] === 10) return i;
      return -1;
    };
    let inbuf = new Uint8Array(0);
    const msgs = [];
    for (let i = 0; i < 800; i++) {
      ws.run(2_000_000);
      const got = ws.guest_recv(id);
      if (got.length) {
        const merged = new Uint8Array(inbuf.length + got.length);
        merged.set(inbuf, 0);
        merged.set(got, inbuf.length);
        inbuf = merged;
        for (;;) {
          const hdr = findSep(inbuf);
          if (hdr < 0) break;
          const header = dec.decode(inbuf.subarray(0, hdr));
          const m = /Content-Length:\s*(\d+)/i.exec(header);
          if (!m) break;
          const len = parseInt(m[1], 10);
          const start = hdr + 4;
          if (inbuf.length < start + len) break;
          try {
            msgs.push(JSON.parse(dec.decode(inbuf.subarray(start, start + len))));
          } catch {}
          inbuf = inbuf.slice(start + len);
        }
      }
      const has = (id) => msgs.some((v) => v.id === id);
      if (has(1) && has(2) && has(3) && has(4)) break;
    }

    const byId = (id) => msgs.find((v) => v.id === id);
    const diag = msgs.find((v) => v.method === "textDocument/publishDiagnostics");
    return {
      listening: true,
      caps: !!(byId(1) && byId(1).result && byId(1).result.capabilities && byId(1).result.capabilities.hoverProvider),
      hover: (byId(2) && byId(2).result && byId(2).result.contents && byId(2).result.contents.value) || "",
      completionHasGreet: !!(byId(3) && byId(3).result && JSON.stringify(byId(3).result).includes("greet")),
      definitionLine0: !!(byId(4) && byId(4).result && byId(4).result.range && byId(4).result.range.start.line === 0),
      diagLine1: !!(diag && (diag.params.diagnostics || []).some((d) => d.range.start.line === 1)),
    };
  });

  check(r.listening, "the in-OS language server boots and listens (LSP-LISTENING) in the browser peer");
  if (r.listening) {
    check(r.caps, "initialize over the bridge advertises hover/completion/definition (LSP capabilities)");
    check(r.hover.includes("greet"), `hover over the bridge names the symbol \`greet\` (got: ${JSON.stringify(r.hover).slice(0, 80)})`);
    check(r.completionHasGreet, "completion over the bridge offers the document's identifiers");
    check(r.definitionLine0, "go-to-definition over the bridge points at the definition (line 0)");
    check(r.diagLine1, "the server's TODO diagnostic (line 1) arrives over the bridge");
  }
  console.log(failed ? "LSP-TEST: FAILED" : "LSP-TEST: PASS (the deployed wasm carries a full LSP session to the in-OS server over the substrate bridge — real language intelligence, no Node)");
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
