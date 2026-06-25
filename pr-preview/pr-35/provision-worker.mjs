// Provisioning worker — assembles a holospace's REAL OCI image into a *sparse*
// OPFS rootfs, the memory-bounded way GitHub Pages must use.
//
// Why a worker: the streaming sparse assembler (`DevcontainerProvision.assembleIntoOpfs`,
// CC-50) writes only the non-zero 4 KiB blocks straight into an OPFS file through a
// `FileSystemSyncAccessHandle` — and sync access handles are **dedicated-worker
// only**. The Manager tab (a window) therefore cannot call it; before this it fell
// back to the DENSE `assemble(1 GiB)`, materializing a whole gigabyte `Vec` in the
// tab, which OOM-traps for a real base image (Debian/`buildpack-deps`) → the
// provision fails → nothing boots. This worker removes that ceiling: peak heap
// tracks the image's *content*, not its declared disk size ("the KappaStore IS the
// memory, RAM is a cache", Laws L3/L4), so an **arbitrary** image provisions.
//
// The router extension's CORS-free fetch (`chrome.runtime.connect`) is window-only,
// so the window relays each layer fetch: the worker asks ({type:"fetch"}), the
// window pulls it through the router and posts the bytes back ({type:"fetchResult"},
// transferred), the worker `deliver`s them. The pull re-derives every blob (Law L5).
import init, { DevcontainerProvision } from "./pkg/holospaces_web.js";

let resolveFetch = null;

self.addEventListener("error", (e) => {
  try { self.postMessage({ type: "error", error: "worker error: " + (e.message || "") + " @ " + (e.filename || "") + ":" + (e.lineno || "") }); } catch {}
});
self.addEventListener("unhandledrejection", (e) => {
  try { self.postMessage({ type: "error", error: "unhandledrejection: " + String(e.reason && e.reason.stack ? e.reason.stack : e.reason) }); } catch {}
});

self.onmessage = async (e) => {
  const msg = e.data;
  if (msg && msg.type === "fetchResult") {
    const r = resolveFetch;
    resolveFetch = null;
    if (r) r(msg);
    return;
  }
  if (!msg || msg.type !== "provision") return;
  try {
    await init();
    const { image, arch, kappa, diskBytes } = msg;

    // Pull the image manifest + layers, relaying each fetch to the window (which
    // owns the router extension transport).
    const prov = new DevcontainerProvision(image, arch);
    let steps = 0;
    while (!prov.isDone()) {
      const url = prov.nextUrl();
      if (!url) break;
      const accept = prov.nextAccept();
      const bearer = prov.nextBearer();
      const resp = await new Promise((resolve) => {
        resolveFetch = resolve;
        self.postMessage({ type: "fetch", url, accept, bearer });
      });
      if (!resp || !resp.ok) {
        throw new Error("the router could not fetch " + url + " — is the extension enabled for this site?");
      }
      prov.deliver(resp.status, resp.contentType || "", new Uint8Array(resp.body));
      if (++steps > 300) throw new Error("the image pull did not converge");
    }

    // A fresh OPFS file for the provisioned rootfs, streamed sparse into a sync
    // access handle (worker-only). holospace-fs reads `provisioned/<kappa>` at boot.
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle("provisioned", { create: true });
    // Replace any prior staging so a re-provision is clean.
    try { await dir.removeEntry(kappa); } catch {}
    try { await dir.removeEntry(`${kappa}.occ`); } catch {}
    const fh = await dir.getFileHandle(kappa, { create: true });
    const handle = await fh.createSyncAccessHandle();
    // The occupancy sidecar — the ascending indices of the blocks the assembler
    // writes, so the boot pages an arbitrarily large declared disk O(content) (only
    // the occupied blocks are read, never the holes). Co-located with the rootfs.
    const occFh = await dir.getFileHandle(`${kappa}.occ`, { create: true });
    const occHandle = await occFh.createSyncAccessHandle();
    let imageLen;
    try {
      // Safety net for re-provisioning: the assembler writes only the non-zero blocks
      // and truncates to the final length, so if removeEntry failed above and the file
      // already exists, stale non-zero blocks from the old image (in regions the new
      // image leaves sparse) would survive and corrupt the rootfs. Zero the file first
      // so only this image's content is present. assembleIntoOpfsTracked likewise
      // overwrites the occupancy sidecar.
      handle.truncate(0);
      imageLen = prov.assembleIntoOpfsTracked(handle, occHandle, diskBytes);
    } finally {
      try { handle.close(); } catch {}
      try { occHandle.close(); } catch {}
    }
    self.postMessage({ type: "done", imageLen });
  } catch (err) {
    self.postMessage({ type: "error", error: String(err && err.stack ? err.stack : err) });
  }
};
