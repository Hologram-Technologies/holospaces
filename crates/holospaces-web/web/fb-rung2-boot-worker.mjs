// fb-rung2-boot-worker.mjs — RUNG 2 witness: boot the REAL graphical aarch64 kernel
// (CONFIG_DRM_SIMPLEDRM + fbcon) over a per-sector OPFS κ-disk, and watch the kernel's
// own framebuffer console render onto the emulator's simple-framebuffer — which the κ
// render stack then projects. Runs in a Worker (OPFS sync handles are worker-only).
import init, { DevcontainerImage, Aarch64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());

// cheap: any non-zero pixel byte (fbcon draws light text on a black ground → text = non-zero)
const fbNonZero = (fb) => { for (let i = 0; i < fb.length; i += 16) if (fb[i]) return true; return false; };
const fbNonZeroCount = (fb) => { let c = 0; for (let i = 0; i < fb.length; i += 16) if (fb[i]) c++; return c; };

self.onmessage = async (ev) => {
  const post = (m, transfer) => self.postMessage(m, transfer || []);
  // unique per-run file names so a stale handle from a crashed run can never block a retry
  const nonce = (ev && ev.data && ev.data.nonce) || "x";
  const ROOTFS = `gfx-rootfs-${nonce}`, PACK = `gfx-pack-${nonce}`;
  try {
    await init();
    post({ phase: "loading graphical kernel + aarch64 rootfs" });
    const kernel = await gunzip(await bytes("./graphical-aarch64-kernel.gz"));   // 13.8MB DRM/fbcon kernel
    const layer  = await bytes("./alpine-aarch64-layer.tar.gz");                 // stock Alpine aarch64 minirootfs

    post({ phase: "assembling bootable ext4 → OPFS κ-disk" });
    const DISK = 96 * 1024 * 1024;   // small: alpine minirootfs is ~8MB; keeps the up-front κ-ingest + memory low
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rw, DISK);
    rw.close();

    post({ phase: "booting graphical kernel (simple-framebuffer in the devicetree)" });
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const ws = Aarch64Workspace.boot_devcontainer_opfs_streamed_graphical(kernel, rootfsRead, packHandle);

    const dims = ws.framebuffer_dims();
    const [W, H] = [dims[0], dims[1]];
    let drm = false, fbcon = false, halted = false, firstPixels = -1;

    for (let i = 0; i < 9000; i++) {
      halted = ws.run(8_000_000);
      const term = ws.terminal();
      drm   = drm   || /simpledrm|simple-framebuffer|\[\s*drm\s*\]/i.test(term);
      // the canonical fbcon-bound message: "Console: switching to colour frame buffer device 160x50"
      fbcon = fbcon || /switching to .*frame ?buffer device|fb0: .* frame ?buffer|simple-framebuffer.*fb0/i.test(term);
      const fb = ws.framebuffer();
      const nz = fbNonZero(fb);
      if (nz && firstPixels < 0) firstPixels = i;

      const sendFb = nz && (i === firstPixels || i % 60 === 0);
      const msg = { phase: "running", iter: i, drm, fbcon, fbNonZero: nz, pixels: nz ? fbNonZeroCount(fb) : 0,
                    W, H, serialTail: term.split("\n").slice(-18).join("\n") };
      if (sendFb) { const copy = fb.slice(); msg.fb = copy.buffer; post(msg, [copy.buffer]); }
      else if (i % 20 === 0 || nz) post(msg);

      if (halted) break;
      if (nz && fbcon && i > firstPixels + 40) break;   // solid graphical witness — stop
    }

    const term = ws.terminal();
    const fb = ws.framebuffer();
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    const copy = fb.slice();
    post({ phase: "done", done: true, imageLen, W, H, drm, fbcon, firstPixels, halted,
           pixels: fbNonZeroCount(fb),
           serialTail: term.split("\n").slice(-34).join("\n"), fb: copy.buffer }, [copy.buffer]);
  } catch (e) {
    post({ phase: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
