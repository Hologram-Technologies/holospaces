// CC-31 — resume a devcontainer from a persisted κ snapshot over OPFS, in the
// browser (Chromium via Playwright), the same wasm code GitHub Pages serves.
//
// The substrate primitive (Emulator::snapshot / restore) is proven byte-for-byte
// host-side against the qemu differential oracle (CC-30). This witnesses the
// *browser* half: a running devcontainer is suspended to a κ snapshot, gzipped
// (most of guest RAM is zero), and persisted to the Origin Private File System;
// then — across a real page RELOAD (a brand-new runtime; only OPFS survives) —
// the snapshot is read back, *verified by re-derivation* (Law L5; ADR-019, OPFS
// is durable but untrusted), and the machine resumes with its workspace files
// intact and still executing — no kernel fetch, no rootfs assembly, no cold boot.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };
const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("RESUME-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("RESUME-TEST: pageerror —", e.message)));

const WITNESS = "resume-witness.txt";
const CONTENT = "this file and the running machine survived a reload";

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  // ── Session 1: boot, write a workspace file, run a bit, suspend → OPFS ──────
  const s1 = await page.evaluate(
    async ([witness, content]) => {
      const hs = window.hs;
      const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
      const gunzip = async (b) =>
        new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
      const gzip = async (b) =>
        new Uint8Array(await new Response(new Response(b).body.pipeThrough(new CompressionStream("gzip"))).arrayBuffer());

      const layer = await bytes("./devcontainer-layer.tar.gz");
      const kernel = await gunzip(await bytes("./devcontainer-kernel.gz"));
      const img = new hs.DevcontainerImage();
      img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
      const ws = hs.Workspace.boot_devcontainer(kernel, img.assemble());
      ws.ws_write(witness, new TextEncoder().encode(content));
      // Run a bit so the snapshot captures a genuinely *running* machine (real
      // registers, page tables, devices) partway through its boot, not a
      // freshly-constructed one — and short of userspace, so the resume has a
      // boot to finish.
      for (let i = 0; i < 12; i++) if (ws.run(2_000_000) || ws.shows("USERSPACE-OK")) break;
      const reachedUserspace = ws.shows("USERSPACE-OK");

      const snapshot = ws.suspend();
      const kappa = hs.kappa(snapshot);
      const gz = await gzip(snapshot);
      const root = await navigator.storage.getDirectory();
      const fh = await root.getFileHandle("snap.gz", { create: true });
      const w = await fh.createWritable();
      await w.write(gz);
      await w.close();
      const kh = await root.getFileHandle("snap.kappa", { create: true });
      const kw = await kh.createWritable();
      await kw.write(kappa);
      await kw.close();
      return {
        before: new TextDecoder().decode(ws.ws_read(witness)),
        snapshotLen: snapshot.length,
        gzLen: gz.length,
        reachedUserspace,
      };
    },
    [WITNESS, CONTENT],
  );

  check(s1.before === CONTENT, "session 1: the workspace file is present before suspend");
  check(!s1.reachedUserspace, "session 1 suspends the machine mid-boot (short of userspace), so the resume has a boot to finish");
  check(
    s1.snapshotLen > 0 && s1.gzLen > 0 && s1.gzLen < s1.snapshotLen,
    `the κ snapshot gzips small (${s1.snapshotLen} → ${s1.gzLen} bytes — mostly-zero RAM)`,
  );

  // ── RELOAD: a brand-new runtime; only OPFS survives ─────────────────────────
  await page.reload();
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  // ── Session 2: read OPFS, verify (L5), resume, confirm intact + still live ──
  const s2 = await page.evaluate(
    async ([witness]) => {
      const hs = window.hs;
      const gunzip = async (b) =>
        new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
      const root = await navigator.storage.getDirectory();
      const gz = new Uint8Array(await (await (await root.getFileHandle("snap.gz")).getFile()).arrayBuffer());
      const recordedKappa = await (await (await root.getFileHandle("snap.kappa")).getFile()).text();
      const snapshot = await gunzip(gz);

      const verified = hs.kappa(snapshot) === recordedKappa; // Law L5 / ADR-019
      let file = null;
      let stillRunning = false;
      if (verified) {
        const ws = hs.Workspace.resume_devcontainer(snapshot);
        file = new TextDecoder().decode(ws.ws_read(witness));
        // The resumed machine is *live* and *continues the very boot* it was
        // suspended partway through: run it on and it reaches userspace. (The
        // console output buffer is not part of the snapshot — a projection of the
        // past, not future-affecting state — so liveness is the boot *advancing*,
        // not the old scrollback.)
        for (let i = 0; i < 200; i++) {
          if (ws.run(2_000_000) || ws.shows("USERSPACE-OK")) break;
        }
        stillRunning = ws.shows("USERSPACE-OK");
      }
      // A corrupted snapshot must NOT re-derive to the recorded κ.
      const tampered = snapshot.slice();
      tampered[tampered.length - 1] ^= 0xff;
      const tamperRejected = hs.kappa(tampered) !== recordedKappa;
      return { verified, file, stillRunning, tamperRejected };
    },
    [WITNESS],
  );

  check(s2.verified, "after reload: the persisted snapshot re-derives to its recorded κ (Law L5; ADR-019)");
  check(s2.file === CONTENT, "the workspace file is intact after resume across a page reload (CC-30/CC-31)");
  check(s2.stillRunning, "the resumed machine is live — it continues executing, it was not re-booted");
  check(s2.tamperRejected, "a tampered snapshot is refused (it does not re-derive to the recorded κ)");

  console.log(
    failed
      ? "RESUME-TEST: FAILED"
      : "RESUME-TEST: PASS (devcontainer resumed from a κ snapshot over OPFS, workspace intact, across a reload)",
  );
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
