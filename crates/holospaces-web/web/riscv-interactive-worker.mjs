import init, { DevcontainerImage, Workspace } from "./pkg/holospaces_web.js";
const gunzip = async (b) => new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
let ws = null;
async function boot() {
  postMessage({ kind: "stage", s: "init" }); await init();
  postMessage({ kind: "stage", s: "fetch riscv kernel + alpine-riscv64 layer" });
  const kernel = await gunzip(await bytes("./devcontainer-kernel.gz"));
  const layer  = await bytes("./alpine-riscv64-layer.tar.gz");
  postMessage({ kind: "stage", s: "assemble_bootable" });
  const img = new DevcontainerImage();
  img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
  const rootfs = img.assemble_bootable(256 * 1024 * 1024);   // in-memory bootable, injected /init
  postMessage({ kind: "stage", s: "boot_devcontainer" });
  ws = Workspace.boot_devcontainer(kernel, rootfs);
  postMessage({ kind: "booted" });
  let last = "";
  const tick = () => { let halted = false;
    try { for (let k = 0; k < 24 && !halted; k++) halted = ws.run(2_000_000); }
    catch (e) { postMessage({ kind: "error", error: String(e && e.stack || e) }); return; }
    const t = ws.terminal(); if (t !== last) { last = t; postMessage({ kind: "term", text: t }); }
    if (halted) { postMessage({ kind: "halted" }); return; } setTimeout(tick, 0); };
  tick();
}
onmessage = (e) => { const m = e.data || {}; if (m.kind === "boot") boot(); else if (m.kind === "input" && ws) ws.feed_input(new TextEncoder().encode(m.data)); };
