import init, { DevcontainerImage, X64Workspace } from "./pkg/holospaces_web.js";
const gunzip = async (b) => new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
self.onmessage = async () => {
  try {
    self.postMessage({ stage: "init" }); await init();
    self.postMessage({ stage: "fetch-kernel" });
    const kernel = await gunzip(await bytes("./x64-diskroot-kernel.gz"));
    const layer = await bytes("./alpine-amd64-layer.tar.gz");
    self.postMessage({ stage: "assemble (64MB disk)" });
    const DISK = 64 * 1024 * 1024;                         // 64MB instead of 256MB — OOM test
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle("sm-rootfs", { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    img.assembleBootableIntoOpfs(rw, DISK); rw.close();
    self.postMessage({ stage: "boot" });
    const rr = await (await root.getFileHandle("sm-rootfs")).createSyncAccessHandle();
    const pk = await (await root.getFileHandle("sm-pack", { create: true })).createSyncAccessHandle(); pk.truncate(0);
    const ws = X64Workspace.boot_devcontainer_opfs_streamed(kernel, rr, pk);
    let mounted = false, userspace = false;
    for (let i = 0; i < 9000; i++) {
      const halted = ws.run(4_000_000);
      const t = ws.terminal();
      mounted = mounted || t.includes("EXT4-fs");
      userspace = userspace || t.includes("workspace#") || t.includes("devcontainer ready") || t.includes("USERSPACE-OK");
      if (i % 200 === 0) self.postMessage({ stage: "run", i, mounted, userspace, tail: t.split("\n").slice(-3).join(" | ") });
      if (halted || userspace) break;
    }
    self.postMessage({ ok: true, mounted, userspace, tail: ws.terminal().split("\n").slice(-12).join("\n") });
  } catch (e) { self.postMessage({ ok: false, error: String(e && e.stack || e) }); }
};
