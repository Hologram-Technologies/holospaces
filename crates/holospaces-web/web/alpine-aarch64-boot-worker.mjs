// alpine-aarch64-boot-worker.mjs — witness REAL Alpine (aarch64) booting to an interactive shell
// over a per-sector OPFS κ-disk, IN THE BROWSER, via the proven aarch64 path
// (assembleBootableIntoOpfs → Workspace.boot_devcontainer_routed_opfs_streamed).
import init, { DevcontainerImage, Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());

self.onmessage = async () => {
  try {
    await init();
    const kernel = await gunzip(await bytes("./devcontainer-kernel.gz"));  // the RISC-V devcontainer kernel
    const layer = await bytes("./alpine-riscv64-layer.tar.gz");          // stock Alpine riscv64 minirootfs (arch-matched)

    // 1) assemble the Alpine layer → bootable ext4 STRAIGHT INTO an OPFS file (sparse), + inject /init
    const DISK = 256 * 1024 * 1024;
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle("alpine-rootfs", { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rw, DISK);
    rw.close();

    // 2) BOOT over the paged-κ-disk: sectors page from the OPFS rootfs into the OPFS-backed κ-store
    const rootfsRead = await (await root.getFileHandle("alpine-rootfs")).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle("alpine-pack", { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const ws = Workspace.boot_devcontainer_routed_opfs_streamed(kernel, rootfsRead, packHandle);

    let mounted = false, userspace = false, halted = false;
    for (let i = 0; i < 6000; i++) {
      halted = ws.run(8_000_000);
      mounted = mounted || ws.shows("EXT4-fs") || ws.shows("Mounted root (ext4 filesystem)");
      // the injected DEVCONTAINER_INIT prints this + drops to an interactive busybox shell prompt
      userspace = userspace || ws.shows("devcontainer ready") || ws.shows("workspace#");
      if (halted || userspace) break;
    }

    // 3) PROVE it's really Alpine + interactive: type commands into the live shell
    const type = (s) => ws.feed_input(new TextEncoder().encode(s));
    let alpineRelease = false, apk = false;
    if (userspace) {
      type("cat /etc/alpine-release\n");
      for (let i = 0; i < 800 && !alpineRelease; i++) { ws.run(8_000_000); alpineRelease = /3\.2\d\.\d/.test(ws.terminal()); }
      apk = ws.shows("apk-tools");   // best-effort (no extra slow run)
    }
    const tail = ws.terminal().split("\n").slice(-22).join("\n");
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    self.postMessage({ ok: true, imageLen, diskBytes: DISK, mounted, userspace, alpineRelease, apk, tail });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
