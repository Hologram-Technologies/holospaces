// fb-rung2-x64-boot-worker.mjs — x86-64 RUNG 2 witness: boot the REAL graphical amd64 Alpine kernel
// (DRM_SIMPLEDRM + fbcon, no embedded init → root=/dev/vda) over a per-sector OPFS κ-disk, and watch
// the kernel's own framebuffer console render onto the emulator's linear framebuffer (advertised via
// the x86 boot protocol's screen_info) — which the κ render stack then projects. The x64 twin of
// fb-rung2-boot-worker.mjs (aarch64). Runs in a Worker (OPFS sync handles are worker-only).
import init, { DevcontainerImage, X64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());

// cheap: any non-zero pixel byte (fbcon draws light text on a black ground → text = non-zero)
const fbNonZero = (fb) => { for (let i = 0; i < fb.length; i += 16) if (fb[i]) return true; return false; };
const fbNonZeroCount = (fb) => { let c = 0; for (let i = 0; i < fb.length; i += 16) if (fb[i]) c++; return c; };

self.onmessage = async (ev) => {
  const post = (m, transfer) => self.postMessage(m, transfer || []);
  const nonce = (ev && ev.data && ev.data.nonce) || "x";
  const ROOTFS = `gfx-x64-rootfs-${nonce}`, PACK = `gfx-x64-pack-${nonce}`;
  try {
    await init();
    post({ phase: "loading graphical amd64 kernel + x86_64 Alpine rootfs" });
    const kernel = await gunzip(await bytes("./graphical-x64-kernel.gz"));   // DRM_SIMPLEDRM + fbcon, disk-root
    const layer  = await bytes("./alpine-amd64-layer.tar.gz");               // stock Alpine x86_64 minirootfs

    post({ phase: "assembling bootable ext4 → OPFS κ-disk" });
    const DISK = 64 * 1024 * 1024;   // minirootfs is ~12MB unpacked; smaller disk → far faster up-front κ-ingest
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rw, DISK);
    rw.close();

    post({ phase: "booting graphical amd64 kernel (screen_info → simple-framebuffer → simpledrm)", imageLen, kernelLen: kernel.length });
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const t0 = Date.now();
    post({ phase: "ctor-start (ingesting κ-disk + ELF load, synchronous)" });
    const ws = X64Workspace.boot_devcontainer_opfs_streamed_graphical(kernel, rootfsRead, packHandle);
    post({ phase: "ctor-done", ctorMs: Date.now() - t0 });
    // CORRECTNESS-FIRST region-JIT hunt: enable the chaining region JIT in NOTRUST mode — every
    // compiled hot region runs dry and is shadow-compared to the interpreter on EVERY execution,
    // latching the first divergence (the input-dependent codegen bug behind the old "uname: not
    // found"). ev.data.jit selects: "notrust" (hunt), "on" (real fast path), else off (pure interp).
    // Default OFF: the region JIT is a NET SLOWDOWN on real Alpine (short regions; see the project's
    // own verdict in memory). The pure interpreter is the correct + fastest path. "notrust"/"on" are
    // for explicit JIT experiments only (ev.data.jit), never the default.
    const jitMode = (ev && ev.data && ev.data.jit) || "off";
    if (jitMode === "notrust") ws.set_region_jit(true, true);
    else if (jitMode === "on") ws.set_region_jit(true, false);

    const dims = ws.framebuffer_dims();
    const [W, H] = [dims[0], dims[1]];
    let drm = false, fbcon = false, halted = false, firstPixels = -1;
    let mounted = false, userspace = false, firstUser = -1, iterRan = 0;

    const STEP = 8_000_000;
    const tRun0 = performance.now();
    for (let i = 0; i < 20000; i++) {
      halted = ws.run(STEP);
      iterRan = i + 1;
      const term = ws.terminal();
      drm   = drm   || /simpledrm|simple-framebuffer|\[\s*drm\s*\]/i.test(term);
      fbcon = fbcon || /switching to .*frame ?buffer device|fb0: .* frame ?buffer|simple-framebuffer.*fb0/i.test(term);
      mounted   = mounted   || /EXT4-fs .* mounted filesystem|VFS: Mounted root/i.test(term);
      // disk-root /init reaches userland → these markers (or a busybox/login prompt)
      userspace = userspace || /Run \/init as init process|init as init process|Welcome to Alpine|\/ #|workspace#|login:/i.test(term);
      const fb = ws.framebuffer();
      const nz = fbNonZero(fb);
      if (nz && firstPixels < 0) firstPixels = i;
      if (userspace && firstUser < 0) firstUser = i;

      const sendFb = nz && (i === firstPixels || i % 60 === 0 || (firstUser >= 0 && i <= firstUser + 8));
      const msg = { phase: "running", iter: i, drm, fbcon, mounted, userspace, fbNonZero: nz,
                    pixels: nz ? fbNonZeroCount(fb) : 0, W, H, serialTail: term.split("\n").slice(-18).join("\n") };
      if (sendFb) { const copy = fb.slice(); msg.fb = copy.buffer; post(msg, [copy.buffer]); }
      else if (i % 20 === 0 || nz) post(msg);

      if (halted) break;
      if (firstUser >= 0 && i > firstUser + 60) break;   // reached userland + ran a while → witnessed
    }

    const term = ws.terminal();
    const fb = ws.framebuffer();
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    const copy = fb.slice();
    const elapsedSec = (performance.now() - tRun0) / 1000;
    const mips = (iterRan * STEP) / elapsedSec / 1e6;   // approx: budget steps actually run ≈ iters×STEP
    post({ phase: "done", done: true, imageLen, W, H, drm, fbcon, firstPixels, halted, mounted, userspace,
           mips: Math.round(mips * 10) / 10, elapsedSec: Math.round(elapsedSec * 10) / 10,
           jitMode, regionDivergence: ws.region_divergence(),
           pixels: fbNonZeroCount(fb), haltReason: ws.halt_reason(),
           serialTail: term.split("\n").slice(-34).join("\n"), fullSerial: term, fb: copy.buffer }, [copy.buffer]);
  } catch (e) {
    post({ phase: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
