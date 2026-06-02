// CC-14 / CC-20 in the browser — a devcontainer's OCI image is assembled into a
// bootable root filesystem and booted over the emulator's virtio-blk *entirely
// in the browser peer* (Chromium via Playwright), the same wasm code GitHub
// Pages serves. The page fetches the devcontainer's image layer and the kernel
// from the cold-start gateway, assembles the rootfs with the in-crate Layer
// Assembler (wasm), and boots a real Linux that mounts it over /dev/vda — no
// server assembles or boots the OS (Law L1/L4).
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
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("DEVCONTAINER-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("DEVCONTAINER-TEST: pageerror —", e.message)));

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  const r = await page.evaluate(async () => {
    const hs = window.hs;
    const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
    const gunzip = async (b) => {
      const s = new Response(b).body.pipeThrough(new DecompressionStream("gzip"));
      return new Uint8Array(await new Response(s).arrayBuffer());
    };

    // 1) Fetch the devcontainer's OCI image layer + the kernel from the gateway.
    const layer = await bytes("./devcontainer-layer.tar.gz");
    const kernel = await gunzip(await bytes("./devcontainer-kernel.gz"));

    // 2) Assemble the layer into a bootable ext4 rootfs — the Layer Assembler,
    //    in the browser (gunzip + untar + overlay + the in-crate ext4 writer).
    const img = new hs.DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const rootfs = img.assemble();

    // 3) Boot a real Linux that mounts it over the emulator's virtio-blk.
    const ws = hs.Workspace.boot_devcontainer(kernel, rootfs);
    let mounted = false;
    let userspace = false;
    for (let i = 0; i < 2000; i++) {
      const halted = ws.run(5_000_000);
      mounted = mounted || ws.shows("Mounted root (ext4 filesystem)");
      userspace = userspace || ws.shows("USERSPACE-OK");
      if (halted || userspace) break;
    }
    return {
      rootfsLen: rootfs.length,
      wholeBlocks: rootfs.length % 4096 === 0,
      mounted,
      userspace,
      tail: ws.terminal().split("\n").slice(-6).join("\n"),
    };
  });

  check(r.rootfsLen > 0 && r.wholeBlocks, `the OCI layer assembled into an ext4 rootfs in the browser (${r.rootfsLen} bytes)`);
  check(r.mounted, "a real Linux mounted the assembled rootfs over the emulator's virtio-blk — in the browser (CC-14)");
  check(r.userspace, "the devcontainer's userspace ran from the imported rootfs (USERSPACE-OK)");
  if (!r.mounted || !r.userspace) console.error("  console tail:\n" + r.tail);

  console.log(failed ? "DEVCONTAINER-TEST: FAILED" : "DEVCONTAINER-TEST: PASS (devcontainer assembled + booted over virtio-blk in the browser)");
} finally {
  await browser.close();
  server.close();
}

// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
