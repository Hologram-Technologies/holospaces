// fb-rung3-boot-worker.mjs — RUNG 3 witness: a real Wayland compositor (cage) drawing a
// GUI session onto the emulator's simple-framebuffer. Boots the graphical aarch64 kernel
// with a CUSTOM /init that launches `cage -- foot` over wlroots' DRM backend + pixman
// software renderer (no GPU), seat opened directly as PID 1 (LIBSEAT_BACKEND=builtin).
import init, { DevcontainerImage, Aarch64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const fbNonZero = (fb) => { let c = 0; for (let i = 0; i < fb.length; i += 16) if (fb[i]) c++; return c; };

// PID 1: mount, set the wlroots/seat env for a GPU-less DRM session, launch cage → foot.
// cage's stderr streams to /dev/console (the serial), so wlroots/cage logs reach the witness.
const INIT = `#!/bin/busybox sh
/bin/busybox mkdir -p /proc /sys /dev /tmp /run /root
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null
/bin/busybox mount -t tmpfs tmpfs /run 2>/dev/null
/bin/busybox mount -t tmpfs tmpfs /tmp 2>/dev/null
/bin/busybox --install -s 2>/dev/null
export PATH=/bin:/sbin:/usr/bin:/usr/sbin HOME=/root TERM=linux
export XDG_RUNTIME_DIR=/run
export LIBSEAT_BACKEND=builtin
export WLR_RENDERER=pixman
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_BACKENDS=drm
export WLR_DRM_NO_ATOMIC=1
/bin/busybox echo HOLO-RUNG3-PRECAGE > /dev/console
/bin/busybox ls -l /dev/dri > /dev/console 2>&1
cage -- foot -e /bin/busybox sh -c 'echo HOLO GRAPHICAL SESSION; uname -a; while true; do sleep 2; done' > /dev/console 2>&1
/bin/busybox echo HOLO-RUNG3-CAGE-EXIT > /dev/console
exec /bin/busybox sh
`;

self.onmessage = async (ev) => {
  const post = (m, transfer) => self.postMessage(m, transfer || []);
  const nonce = (ev && ev.data && ev.data.nonce) || "x";
  const ROOTFS = `r3-rootfs-${nonce}`, PACK = `r3-pack-${nonce}`;
  try {
    await init();
    post({ phase: "loading graphical kernel + cage rootfs" });
    const kernel = await gunzip(await bytes("./graphical-aarch64-kernel.gz"));
    const layer  = await bytes("./alpine-aarch64-cage-layer.tar.gz");

    post({ phase: "assembling bootable ext4 (custom cage init) → OPFS κ-disk" });
    const DISK = 384 * 1024 * 1024;   // cage + wlroots + mesa + foot + fonts need room
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableWithInitIntoOpfs(rw, DISK, new TextEncoder().encode(INIT));
    rw.close();

    post({ phase: "booting — kernel, then cage on the framebuffer" });
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const ws = Aarch64Workspace.boot_devcontainer_opfs_streamed_graphical(kernel, rootfsRead, packHandle);

    const dims = ws.framebuffer_dims();
    const [W, H] = [dims[0], dims[1]];
    let dri = false, wlr = false, cage = false, exited = false, halted = false, firstSession = -1;

    for (let i = 0; i < 12000; i++) {
      halted = ws.run(8_000_000);
      const term = ws.terminal();
      dri  = dri  || /\/dev\/dri|card0|renderD/i.test(term);
      wlr  = wlr  || /wlr|wlroots|backend\/drm|\[drm\]|DRM master|Cage/i.test(term);
      cage = cage || /HOLO GRAPHICAL SESSION|wl_display|Wayland|foot/i.test(term);
      exited = exited || /HOLO-RUNG3-CAGE-EXIT/.test(term);
      const fb = ws.framebuffer();
      const px = fbNonZero(fb);
      if (cage && firstSession < 0) firstSession = i;

      const sendFb = px > 0 && (i % 50 === 0 || (cage && firstSession >= 0 && i <= firstSession + 5));
      const msg = { phase: "running", iter: i, dri, wlr, cage, exited, pixels: px, W, H,
                    serialTail: term.split("\n").slice(-22).join("\n") };
      if (sendFb) { const c = fb.slice(); msg.fb = c.buffer; post(msg, [c.buffer]); }
      else if (i % 20 === 0) post(msg);

      if (halted) break;
      if (cage && firstSession >= 0 && i > firstSession + 80) break;  // session ran a while — witnessed
    }

    const term = ws.terminal();
    const fb = ws.framebuffer();
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    const c = fb.slice();
    post({ phase: "done", done: true, imageLen, W, H, dri, wlr, cage, exited, halted,
           pixels: fbNonZero(fb), serialTail: term.split("\n").slice(-40).join("\n"), fb: c.buffer }, [c.buffer]);
  } catch (e) {
    post({ phase: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
