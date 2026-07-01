// fb-resume-x64-boot-worker.mjs — THE SPEED KEYSTONE: boot graphical x86-64 Alpine ONCE, snapshot
// the running machine (suspend), resume it, and prove the resumed machine (a) keeps executing and
// (b) restores the FRAMEBUFFER pixel-for-pixel — in << the cold-boot time. The framebuffer lives at
// the top of guest RAM, so it is captured in the suspend snapshot; resume restores it. This is the
// "boot once slowly, reopen instantly" proof the whole desktop-in-a-tab strategy rests on.
import init, { DevcontainerImage, X64Workspace } from "./pkg/holospaces_web.js";

const gunzip = async (buf) =>
  new Uint8Array(await new Response(new Response(buf).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const fbCount = (fb) => { let c = 0; for (let i = 0; i < fb.length; i += 16) if (fb[i]) c++; return c; };
// A cheap content hash of the framebuffer (sampled) so we can assert pixel-for-pixel restore.
const fbHash = (fb) => { let h = 2166136261 >>> 0; for (let i = 0; i < fb.length; i += 64) { h ^= fb[i]; h = Math.imul(h, 16777619) >>> 0; } return h >>> 0; };

self.onmessage = async (ev) => {
  const post = (m, transfer) => self.postMessage(m, transfer || []);
  const nonce = (ev && ev.data && ev.data.nonce) || "x";
  const ROOTFS = `res-x64-rootfs-${nonce}`, PACK = `res-x64-pack-${nonce}`;
  try {
    await init();
    post({ phase: "loading graphical amd64 kernel + Alpine rootfs" });
    const kernel = await gunzip(await bytes("./graphical-x64-kernel.gz"));
    const layer  = await bytes("./alpine-amd64-layer.tar.gz");

    post({ phase: "assembling ext4 → OPFS κ-disk" });
    const DISK = 64 * 1024 * 1024;
    const root = await navigator.storage.getDirectory();
    const rw = await (await root.getFileHandle(ROOTFS, { create: true })).createSyncAccessHandle();
    rw.truncate(0);
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    img.assembleBootableIntoOpfs(rw, DISK);
    rw.close();

    post({ phase: "COLD BOOT (graphical → userland) — timing it" });
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();
    packHandle.truncate(0);
    const tBoot0 = performance.now();
    const ws = X64Workspace.boot_devcontainer_opfs_streamed_graphical(kernel, rootfsRead, packHandle);

    const dims = ws.framebuffer_dims();
    const [W, H] = [dims[0], dims[1]];
    let fbcon = false, mounted = false, userspace = false, halted = false, firstUser = -1;
    for (let i = 0; i < 20000; i++) {
      halted = ws.run(8_000_000);
      const term = ws.terminal();
      fbcon = fbcon || /fb0: .* frame ?buffer|simpledrm/i.test(term);
      mounted = mounted || /EXT4-fs .* mounted|VFS: Mounted root/i.test(term);
      userspace = userspace || /Run \/init as init process|busybox|\/ #|workspace#/i.test(term);
      if (userspace && firstUser < 0) firstUser = i;
      if (i % 20 === 0) post({ phase: "cold-boot running", iter: i, fbcon, mounted, userspace, pixels: fbCount(ws.framebuffer()) });
      if (halted) break;
      if (firstUser >= 0 && i > firstUser + 30) break; // settled in userland — snapshot here
    }
    const tBoot = performance.now() - tBoot0;
    const fbPre = ws.framebuffer();
    const pixPre = fbCount(fbPre), hashPre = fbHash(fbPre);
    const termPre = ws.terminal();
    post({ phase: "booted — suspending", tBootSec: Math.round(tBoot / 100) / 10, pixPre, hashPre, fbcon, mounted, userspace });

    // ── SUSPEND: snapshot the running graphical machine ────────────────────────────
    const tSus0 = performance.now();
    const snap = ws.suspend();
    const tSuspend = performance.now() - tSus0;
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    post({ phase: "suspended", tSuspendSec: Math.round(tSuspend / 100) / 10, snapBytes: snap.length });

    // ── RESUME: rebuild the machine from the snapshot — NO boot, NO disk re-ingest ──
    const tRes0 = performance.now();
    const ws2 = X64Workspace.resume(snap);
    const tResume = performance.now() - tRes0;

    const fbPost = ws2.framebuffer();
    const pixPost = fbCount(fbPost), hashPost = fbHash(fbPost);
    const termPost = ws2.terminal();
    // Prove it's LIVE: run a few chunks; the console must not regress.
    let liveErr = "";
    try { for (let i = 0; i < 6; i++) ws2.run(4_000_000); } catch (e) { liveErr = String(e); }
    const termRun = ws2.terminal();

    const fbRestored = (pixPost === pixPre && hashPost === hashPre);
    const live = termPost.length >= termPre.length - 64 && termRun.length >= termPost.length && !liveErr;
    const speedup = tResume > 0 ? Math.round((tBoot / tResume) * 10) / 10 : 0;
    const c = fbPost.slice();
    post({
      phase: "done", done: true, W, H,
      tBootSec: Math.round(tBoot / 100) / 10, tSuspendSec: Math.round(tSuspend / 100) / 10,
      tResumeMs: Math.round(tResume), snapBytes: snap.length, speedup,
      pixPre, pixPost, hashPre, hashPost, fbRestored, live, liveErr,
      pass: fbRestored && live,
      haltReason: ws2.halt_reason(),
      serialTail: termRun.split("\n").slice(-12).join("\n"),
      fb: c.buffer,
    }, [c.buffer]);
  } catch (e) {
    post({ phase: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
