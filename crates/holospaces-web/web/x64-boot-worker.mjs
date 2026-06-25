// x64-boot-worker.mjs — witness the holospaces x86-64 core booting a real Linux kernel
// (CC-44, embedded initramfs) over a STREAMED OPFS κ-disk, IN THE BROWSER. The host analogue is
// the passing CC-44 streamed-κ-disk test ("the deployed X64Workspace path").
import init, { X64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());

self.onmessage = async () => {
  try {
    await init();

    // 1) the CC-44 x86-64 kernel (gunzipped ELF) — embedded initramfs PID 1
    const kernel = await gunzip(new Uint8Array(await (await fetch("./x64-kernel.gz")).arrayBuffer()));

    // 2) an 8 MiB κ-disk in OPFS (probed by the kernel's virtio-blk; not the root)
    const DISK = 8 * 1024 * 1024;
    const root = await navigator.storage.getDirectory();
    const rfh = await root.getFileHandle("x64-rootfs", { create: true });
    const rw = await rfh.createSyncAccessHandle();
    const pattern = new Uint8Array(DISK);
    for (let i = 0; i < DISK; i++) pattern[i] = (i * 131 + 7) & 255;
    rw.truncate(0); rw.write(pattern, { at: 0 }); rw.flush(); rw.close();
    const rootfsRead = await (await root.getFileHandle("x64-rootfs")).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle("x64-pack", { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);

    // 3) BOOT the x64 core over the streamed OPFS κ-disk (CC-44 initramfs posture)
    const ws = X64Workspace.boot_linux_initramfs_streamed(kernel, rootfsRead, packHandle);

    let handed = false, userspace = false, halted = false;
    for (let i = 0; i < 7000; i++) {
      halted = ws.run(8_000_000);
      const t = ws.terminal();
      handed = handed || t.includes("Run /init as init process");
      userspace = userspace || t.includes("HOLOSPACES-LINUX-USERSPACE-OK");
      if (halted || userspace) break;
    }
    const tail = ws.terminal().split("\n").slice(-16).join("\n");
    const excTrace = (ws.exc_trace ? ws.exc_trace() : "").split("\n").slice(-80).join("\n");
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    self.postMessage({ ok: true, handed, userspace, halted, tail, excTrace });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
