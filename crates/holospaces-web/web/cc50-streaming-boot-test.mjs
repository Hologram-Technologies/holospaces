// CC-50 — the *streamed-into-OPFS* rootfs BOOTS via the shipped browser path.
//
// The CC-50 streaming Rootfs Assembly writes only the non-zero 4 KiB blocks of a
// bootable ext4 image straight into an OPFS file (sparse — the free space is a
// hole), so peak wasm heap tracks the image's content, not its declared disk
// size ("the KappaStore IS the memory, RAM is a cache"). The host witness proves
// that streamed file is byte-identical to the dense image and structurally clean;
// this witness proves the harder, deployed claim the reviewer asked for: the
// streamed-into-OPFS image **actually boots** through the SAME code GitHub Pages
// ships — no byte-identity substitute.
//
// The shipped flow (holospace-fs extension host worker):
//   1. assemble the bootable rootfs straight into an OPFS file — the streaming
//      sparse serializer `holospaces::assembly::stream_ext4_image_bootable`, the
//      EXACT primitive `DevcontainerProvision.assembleIntoOpfs` uses (here driven
//      via `DevcontainerImage.assembleBootableIntoOpfs` so the witness is
//      hermetic — no registry pull — but the assembly path is identical);
//   2. open a sync access handle on that OPFS file + a second handle for the
//      κ-store pack — both worker-only, which is where the extension host runs;
//   3. boot with `Workspace.boot_devcontainer_routed_opfs_streamed(kernel,
//      rootfsHandle, diskHandle)` — the shipped CC-42 path that pages the disk
//      sector-by-sector from the streamed OPFS file into the OPFS-backed κ-store,
//      so neither provisioning nor boot ever holds the whole image in RAM.
// The witness drives all of that inside a real dedicated Worker (sync access
// handles are worker-only) in Chromium, then asserts the guest reaches a
// userspace marker.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css", ".mjs": "text/javascript" };
const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  if (rel === WORKER_PATH) {
    res.writeHead(200, { "content-type": "text/javascript" });
    res.end(WORKER_SRC);
    return;
  }
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("CC50-STREAMING-BOOT-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("CC50-STREAMING-BOOT-TEST: pageerror —", e.message)));
page.on("console", (m) => { if (m.type() === "error") console.error("  [page console]", m.text()); });

// The worker source, served from the http origin (so the relative wasm import
// resolves) at /cc50-boot-worker.mjs. A module worker so it can `import` the
// wasm-pack ESM the page serves; the whole streamed-assembly → OPFS → paged-boot
// flow runs here because `createSyncAccessHandle()` is dedicated-worker-only (the
// same context the deployed extension host runs in).
const WORKER_PATH = "/cc50-boot-worker.mjs";
const WORKER_SRC = `
self.addEventListener("error", (e) => { try { self.postMessage({ ok: false, error: "worker error event: " + (e.message || "") + " @ " + (e.filename || "") + ":" + (e.lineno || "") }); } catch {} });
self.addEventListener("unhandledrejection", (e) => { try { self.postMessage({ ok: false, error: "unhandledrejection: " + String(e.reason && e.reason.stack ? e.reason.stack : e.reason) }); } catch {} });

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const gunzip = async (b) =>
  new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());

self.onmessage = async () => {
  try {
    const mod = await import("./pkg/holospaces_web.js");
    const { default: init, DevcontainerImage, Workspace } = mod;
    await init();

    // 1) The deployed devcontainer base layer (CC-22 BusyBox) + the networked
    //    RISC-V kernel — the exact fixtures the deploy boots over the shipped
    //    streamed paged-κ-disk path.
    const layer = await bytes("./devcontainer-busybox-layer.tar.gz");
    const kernel = await gunzip(await bytes("./devcontainer-net-kernel.gz"));

    // 2) Fresh OPFS files for this run: the provisioned rootfs (streamed into)
    //    and the κ-store pack the boot pages sectors into.
    const root = await navigator.storage.getDirectory();
    const ROOTFS = "cc50-stream-rootfs.img";
    const PACK = "cc50-stream-disk.pack";
    for (const n of [ROOTFS, PACK]) { try { await root.removeEntry(n); } catch {} }
    const rootfsFile = await root.getFileHandle(ROOTFS, { create: true });
    const rootfsHandle = await rootfsFile.createSyncAccessHandle();

    // 3) STREAM the bootable rootfs straight into the OPFS file — sparse, no
    //    dense Vec. Declare a disk far larger than the image's content so the
    //    sparseness is real: a 512 MiB disk over a few-MiB BusyBox layer.
    const DISK_BYTES = 512 * 1024 * 1024;
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rootfsHandle, DISK_BYTES);

    // The file the streamer produced is sparse: its allocated bytes are far less
    // than the declared disk. (getSize reports the logical size = the full
    // image; the sparseness is in the assembly heap, asserted host-side. Here we
    // confirm the file spans the whole image so trailing free space reads zero.)
    const onDiskLen = rootfsHandle.getSize();

    // The boot reads the rootfs sector-by-sector via a *read* sync handle; close
    // and reopen so the boot path owns it exactly as the extension host does.
    rootfsHandle.close();
    const rootfsReadFile = await root.getFileHandle(ROOTFS);
    const rootfsRead = await rootfsReadFile.createSyncAccessHandle();
    const packFile = await root.getFileHandle(PACK, { create: true });
    const packHandle = await packFile.createSyncAccessHandle();

    // 4) BOOT the streamed-into-OPFS image via the SHIPPED path: page the disk
    //    sector-by-sector from the OPFS file into the OPFS-backed κ-store.
    const ws = Workspace.boot_devcontainer_routed_opfs_streamed(kernel, rootfsRead, packHandle);

    let booted = false;
    let mounted = false;
    for (let i = 0; i < 4000; i++) {
      const halted = ws.run(8_000_000);
      mounted = mounted || ws.shows("Mounted root (ext4 filesystem)") || ws.shows("EXT4-fs");
      booted = booted || ws.shows("holospace devcontainer ready");
      if (halted || booted) break;
    }
    const tail = ws.terminal().split("\\n").slice(-12).join("\\n");

    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}

    self.postMessage({ ok: true, imageLen, onDiskLen, diskBytes: DISK_BYTES, booted, mounted, tail });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
`;

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  const r = await page.evaluate(async (workerPath) => {
    // Same-origin module worker (served by the test http server) so its relative
    // import of ./pkg/holospaces_web.js resolves against the origin — exactly as
    // the deployed extension host worker loads the wasm peer.
    const w = new Worker(workerPath, { type: "module" });
    const result = await new Promise((resolve) => {
      w.onmessage = (e) => resolve(e.data);
      w.onerror = (e) => resolve({ ok: false, error: "worker error: " + (e.message || e.filename + ":" + e.lineno) });
      w.postMessage("go");
    });
    w.terminate();
    return result;
  }, WORKER_PATH);

  if (!r.ok) {
    failed = true;
    console.error("CC50-STREAMING-BOOT-TEST: worker failed —", r.error);
  } else {
    check(
      r.imageLen >= r.diskBytes && r.onDiskLen === r.imageLen,
      `the bootable rootfs streamed into the OPFS file (${r.imageLen} bytes for a ${r.diskBytes}-byte disk, sparse)`,
    );
    check(r.mounted, "a real Linux mounted the streamed-into-OPFS rootfs over the emulator's virtio-blk — in the browser");
    check(
      r.booted,
      "the streamed-into-OPFS image BOOTED to userspace via the shipped paged-κ-disk path (holospace devcontainer ready)",
    );
    if (!r.booted) console.error("  console tail:\n" + r.tail);
  }

  console.log(
    failed
      ? "CC50-STREAMING-BOOT-TEST: FAILED"
      : "CC50-STREAMING-BOOT-TEST: PASS (the streamed-into-OPFS rootfs booted to userspace via the shipped browser path)",
  );
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
