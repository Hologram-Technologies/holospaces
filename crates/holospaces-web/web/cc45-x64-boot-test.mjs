// CC-45 — a provisioned **amd64 (x86-64)** devcontainer BOOTS in the browser via
// the shipped X64Workspace paged-κ-disk path.
//
// The deployed x86-64 analogue of the CC-50 streaming boot: a stock linux/amd64
// devcontainer layer is assembled — sparse, straight into an OPFS file — by the
// SAME streaming serializer DevcontainerProvision.assembleIntoOpfs uses, then BOOTED
// on the x86-64 system core via the shipped `X64Workspace.boot_devcontainer_opfs_
// streamed(kernel, rootfsHandle, diskHandle)` path (the κ-disk paged sector-by-sector
// from OPFS). x86-64 has no device tree, so the κ-disk is discovered from the kernel
// command line (`virtio_mmio.device=…`) the X64Workspace sets — the regression guard
// for "the deployed amd64 boot can't find its disk". Runs inside a real dedicated
// Worker (sync access handles are worker-only — the deployed extension-host context)
// in Chromium, then asserts the guest reaches the deployed userspace marker.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css", ".mjs": "text/javascript", ".gz": "application/gzip" };

const WORKER_PATH = "/cc45-x64-boot-worker.mjs";
const WORKER_SRC = `
self.addEventListener("error", (e) => { try { self.postMessage({ ok: false, error: "worker error: " + (e.message||"") + " @ " + (e.filename||"") + ":" + (e.lineno||"") }); } catch {} });
self.addEventListener("unhandledrejection", (e) => { try { self.postMessage({ ok: false, error: "unhandledrejection: " + String(e.reason && e.reason.stack ? e.reason.stack : e.reason) }); } catch {} });

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const gunzip = async (b) =>
  new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());

self.onmessage = async () => {
  try {
    const mod = await import("./pkg/holospaces_web.js");
    const { default: init, DevcontainerImage, X64Workspace } = mod;
    await init();

    // The stock linux/amd64 busybox layer + the amd64 Linux kernel — the CC-45
    // fixtures, the same bytes the run-stage witness boots.
    const layer = await bytes("./cc45-x64-layer.tar.gz");
    const kernel = await gunzip(await bytes("./cc45-x64-kernel.gz"));

    const root = await navigator.storage.getDirectory();
    const ROOTFS = "cc45-x64-rootfs.img";
    const OCC = "cc45-x64-occ.idx";
    const PACK = "cc45-x64-disk.pack";
    for (const n of [ROOTFS, OCC, PACK]) { try { await root.removeEntry(n); } catch {} }
    const rootfsHandle = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    const occHandle = await (await root.getFileHandle(OCC, { create: true })).createSyncAccessHandle();

    // An **8 GiB** declared disk — a real build-capable size — assembled SPARSE into
    // OPFS, with the rootfs OCCUPANCY (the ascending indices of the blocks actually
    // written) recorded into the sidecar. The free space is a hole; the occupancy is
    // a few-thousand-entry list, not 8 GiB.
    const DISK_BYTES = 8 * 1024 * 1024 * 1024;
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfsTracked(rootfsHandle, occHandle, DISK_BYTES);
    // COMPACT staging: the rootfs file holds only the packed content blocks (a few
    // MiB), NOT the 8 GiB declared disk — a sparse 8 GiB file would blow the origin's
    // OPFS quota. The sidecar = 8-byte image_len header + 8 bytes per occupied block.
    const rootfsBytes = rootfsHandle.getSize();
    const occBytes = occHandle.getSize();
    const occupiedBlocks = (occBytes - 8) / 8;

    rootfsHandle.close();
    occHandle.close();
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const occRead = await (await root.getFileHandle(OCC)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();

    // BOOT the 8 GiB disk **O(content)** on the x86-64 core: only the occupied blocks
    // are paged from OPFS. An O(disk) boot would read 16.7M sectors and never finish
    // in a browser — that this completes IS the proof it pages by content.
    const ws = X64Workspace.bootDevcontainerOpfsStreamedOccupancy(kernel, rootfsRead, occRead, packHandle);

    let booted = false, mounted = false, ranInit = false, panicked = false;
    // The deployed marker lands by ~500M cycles natively; 400 × 8M = 3.2B is ample
    // headroom (disk size adds no boot cycles — proven natively), and fails fast.
    for (let i = 0; i < 400; i++) {
      const halted = ws.run(8_000_000);
      const t = ws.terminal();
      mounted = mounted || t.includes("mounted filesystem") || t.includes("Mounted root");
      ranInit = ranInit || t.includes("Run /init");
      panicked = panicked || t.includes("Kernel panic");
      booted = booted || t.includes("holospace devcontainer ready");
      if (halted || booted || panicked) break;
    }
    const full = ws.terminal();
    const tail = full.slice(-1400);
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    self.postMessage({ ok: true, imageLen, rootfsBytes, occBytes, occupiedBlocks, diskBytes: DISK_BYTES, booted, mounted, ranInit, panicked, termLen: full.length, tail });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
`;

const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  if (rel === WORKER_PATH) { res.writeHead(200, { "content-type": "text/javascript" }); res.end(WORKER_SRC); return; }
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch { res.writeHead(404).end("not found"); }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("CC45-X64-BOOT-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("CC45-X64-BOOT-TEST: pageerror —", e.message)));
page.on("console", (m) => { if (m.type() === "error") console.error("  [page console]", m.text()); });

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });
  const r = await page.evaluate(async (workerPath) => {
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
    console.error("CC45-X64-BOOT-TEST: worker failed —", r.error);
  } else {
    const totalBlocks = r.diskBytes / 4096;
    check(r.imageLen >= r.diskBytes,
      `the guest sees the full ${(r.diskBytes / 1024 / 1024 / 1024)} GiB declared disk (image_len ${r.imageLen})`);
    check(r.rootfsBytes < 64 * 1024 * 1024 && r.rootfsBytes === r.occupiedBlocks * 4096,
      `staged O(content): the OPFS rootfs is ${(r.rootfsBytes / 1024 / 1024).toFixed(1)} MiB (${r.occupiedBlocks} packed blocks), NOT the 8 GiB disk — so it fits the OPFS quota`);
    check(r.occupiedBlocks > 0 && r.occupiedBlocks < totalBlocks / 8,
      `occupancy ≪ disk: ${r.occupiedBlocks} occupied blocks of the ${totalBlocks} a 8 GiB disk would have`);
    check(r.mounted, "a real amd64 Linux mounted the compact occupancy-paged rootfs over virtio-blk — in the browser");
    check(r.booted, "the amd64 devcontainer BOOTED to userspace O(content) from an 8 GiB occupancy-paged disk on the x86-64 core");
    if (!r.booted) {
      console.error(`  [diag] rootfsBytes=${r.rootfsBytes} termLen=${r.termLen} mounted=${r.mounted} ranInit=${r.ranInit} panicked=${r.panicked}`);
      console.error("  console tail:\n" + r.tail);
    }
  }
  console.log(failed
    ? "CC45-X64-BOOT-TEST: FAILED"
    : "CC45-X64-BOOT-TEST: PASS (an amd64 devcontainer booted in the browser via the shipped X64Workspace path)");
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
