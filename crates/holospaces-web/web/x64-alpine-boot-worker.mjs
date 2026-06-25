// x64-alpine-boot-worker.mjs — witness REAL Alpine (x86-64) booting to an interactive shell
// over a per-sector OPFS κ-disk, IN THE BROWSER, via the x64 disk-root path
// (DevcontainerImage.assembleBootableIntoOpfs → X64Workspace.boot_devcontainer_opfs_streamed).
// Mirrors the proven aarch64 Alpine worker; uses an x86_64 disk-root kernel (no embedded
// initramfs hijack) so the kernel mounts /dev/vda and runs the injected /init → busybox shell.
import init, { DevcontainerImage, X64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());

self.onmessage = async () => {
  try {
    await init();
    const kernel = await gunzip(await bytes("./x64-diskroot-kernel.gz")); // x86_64 kernel, root=/dev/vda (no embedded init)
    const layer = await bytes("./alpine-amd64-layer.tar.gz");             // stock Alpine 3.21 x86_64 minirootfs

    // 1) assemble the Alpine layer → bootable ext4 STRAIGHT INTO an OPFS file (sparse), + inject /init
    const DISK = 256 * 1024 * 1024;
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle("alpine-x64-rootfs", { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rw, DISK);
    rw.close();

    // 2) BOOT over the paged-κ-disk: sectors page from the OPFS rootfs into the OPFS-backed κ-store
    const rootfsRead = await (await root.getFileHandle("alpine-x64-rootfs")).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle("alpine-x64-pack", { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const ws = X64Workspace.boot_devcontainer_opfs_streamed(kernel, rootfsRead, packHandle);

    const shows = (s) => ws.terminal().includes(s);
    let mounted = false, userspace = false, halted = false;
    for (let i = 0; i < 9000; i++) {
      halted = ws.run(8_000_000);
      mounted = mounted || shows("EXT4-fs") || shows("Mounted root (ext4 filesystem)");
      // the injected DEVCONTAINER_INIT prints this + drops to an interactive busybox shell prompt
      userspace = userspace || shows("devcontainer ready") || shows("workspace#");
      if (halted || userspace) break;
    }

    // 3) PROVE it's really Alpine x86-64 + interactive: type into the live shell
    const type = (s) => ws.feed_input(new TextEncoder().encode(s));
    let alpineRelease = false, arch = false;
    if (userspace) {
      type("cat /etc/alpine-release; uname -m\n");
      for (let i = 0; i < 1200 && !alpineRelease; i++) { ws.run(8_000_000); alpineRelease = /3\.2\d\.\d/.test(ws.terminal()); }
      arch = ws.terminal().includes("x86_64");
    }
    const tail = ws.terminal().split("\n").slice(-24).join("\n");
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    self.postMessage({ ok: true, imageLen, diskBytes: DISK, mounted, userspace, alpineRelease, arch, tail });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
