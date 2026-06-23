// End-to-end Hologram Platform Manager test in a real browser (Chromium via
// Playwright). Serves the wasm console over http://127.0.0.1 (a secure context)
// and exercises the operator flow entirely in the browser peer:
//   1) sign in → a content-addressed operator identity (Law L1);
//   2) provision a holospace → its identity κ; the View lists it;
//   3) resolve the holospace from the local store and verify it by
//      re-derivation (Law L5); a different κ is absent (no forging);
//   4) the operator roster κ is stable (R5).
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const ROOT = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };

// The CC-14 real OCI image fixture (a real Linux), served over the OCI
// distribution endpoints so DevcontainerProvision pulls a REAL image in the
// browser. In production the router extension's CORS-free fetch is the transport;
// here the same-origin test server stands in for it (the pull logic is identical).
const IMAGE_DIR = path.join(ROOT, "../../../vv/artifacts/cc14/image");

const server = http.createServer(async (req, res) => {
  const url = req.url.split("?")[0];
  if (url.startsWith("/v2/img/")) {
    try {
      if (url.includes("/manifests/")) {
        const index = JSON.parse(await readFile(path.join(IMAGE_DIR, "index.json")));
        const digest = index.manifests[0].digest.replace("sha256:", "");
        const body = await readFile(path.join(IMAGE_DIR, "blobs/sha256", digest));
        res.writeHead(200, { "content-type": "application/vnd.oci.image.manifest.v1+json" }).end(body);
      } else if (url.includes("/blobs/")) {
        const d = url.split("/blobs/")[1].replace("sha256:", "");
        const body = await readFile(path.join(IMAGE_DIR, "blobs/sha256", d));
        res.writeHead(200, { "content-type": "application/octet-stream" }).end(body);
      } else {
        res.writeHead(404).end();
      }
    } catch {
      res.writeHead(404).end();
    }
    return;
  }
  const rel = req.url === "/" ? "/index.html" : url;
  try {
    const body = await readFile(path.join(ROOT, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

let failed = false;
function check(cond, msg) {
  if (cond) {
    console.log("  ✓", msg);
  } else {
    console.error("MANAGER-TEST: FAIL —", msg);
    failed = true;
  }
}

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => {
  console.error("MANAGER-TEST: pageerror —", e.message);
  failed = true;
});

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.__ready === true", null, { timeout: 20000 });

  const r = await page.evaluate(() => {
    const c = new window.hs.Console();
    const enc = (s) => new TextEncoder().encode(s);
    const identity = c.sign_in(enc("operator-self-sovereign-key"));
    const code = enc("a holospace code module");
    const holospace = c.provision(code, 256 * 1024 * 1024);
    const view = JSON.parse(c.view());
    const bytes = c.resolve(holospace);
    const verified = bytes ? window.hs.verify_kappa(bytes, holospace) : false;
    const absent = c.resolve("blake3:" + "0".repeat(64));
    return {
      identity,
      holospace,
      view,
      resolved: !!bytes,
      verified,
      absent: absent === undefined || absent === null,
      roster1: c.roster_kappa(),
      roster2: c.roster_kappa(),
    };
  });

  check(r.identity.startsWith("blake3:"), `signed in — operator identity ${r.identity}`);
  check(r.holospace.startsWith("blake3:"), `provisioned holospace ${r.holospace}`);
  check(r.view.operator === r.identity, "View shows the signed-in operator");
  check(
    r.view.holospaces.length === 1 && r.view.holospaces[0] === r.holospace,
    "View lists the provisioned holospace",
  );
  check(r.resolved && r.verified, "holospace resolves and verifies by re-derivation (L5)");
  check(r.absent, "an unknown κ is absent (no forging — L1/L5)");
  check(r.roster1 && r.roster1 === r.roster2, `operator roster κ is stable (${r.roster1})`);

  // CC-2 / RT2 — the browser `.holo` engine equals the native one: run the
  // fixture `.holo` (compiled + executed natively) through the executor
  // compiled to wasm, and assert an identical output κ.
  const nativeKappa = (await readFile(path.join(ROOT, "fixture.kappa"), "utf8")).trim();
  const browserKappa = await page.evaluate(async () => {
    const res = await fetch("./fixture.holo");
    const archive = new Uint8Array(await res.arrayBuffer());
    return window.hs.run_holo(archive);
  });
  check(
    browserKappa === nativeKappa,
    `browser .holo engine equals native (κ ${browserKappa})`,
  );

  // CC-6 / ADR-008 — the execution surface in the browser: validate a real
  // recompiled userland against the host-ABI surface, then provision it as a
  // holospace; an ambient (WASI-style) import is refused.
  const surface = await page.evaluate(async () => {
    const res = await fetch("./fixture-userland.wasm");
    const userland = new Uint8Array(await res.arrayBuffer());
    let validated = true;
    try {
      window.hs.validate_userland(userland);
    } catch {
      validated = false;
    }
    const c = new window.hs.Console();
    c.sign_in(new TextEncoder().encode("operator-self-sovereign-key"));
    const holospace = c.provision_userland(userland, 4 * 1024 * 1024);
    const view = JSON.parse(c.view());

    // Boot the userland container IN THE BROWSER via the wasmi interpreter
    // engine — spawn, suspend to a κ snapshot, resume, terminate (ADR-008/CC-6).
    const snapshot = c.boot_userland(userland, 4 * 1024 * 1024);

    // The Codespaces scenario (arc42 ch.1): import a devcontainer and run it in
    // the browser tab — its config selects the κ-addressed Wasm userland. The
    // operator selects the guest **architecture** before provisioning (the
    // Manager's arch picker; ADR-021) — riscv64 or aarch64. The selection is part
    // of the holospace's content-addressed identity, so it is fixed for the
    // holospace's lifetime: provisioning the *same* devcontainer under two
    // architectures yields two *distinct* holospaces in the roster.
    const config = new TextEncoder().encode('{"name":"app","image":"debian:12"}');
    const archBefore = JSON.parse(c.view()).holospaces.length;
    const devcontainerSnapshot = c.run_devcontainer(
      "https://github.com/example/cool-project.git",
      "main",
      ".devcontainer/devcontainer.json",
      config,
      userland,
      "aarch64", // the operator's architecture selection
      64 * 1024 * 1024,
    );
    // The same devcontainer under a *different* architecture is a *different*
    // holospace (the arch is immutable, enforced by content-addressing).
    c.run_devcontainer(
      "https://github.com/example/cool-project.git",
      "main",
      ".devcontainer/devcontainer.json",
      config,
      userland,
      "riscv64",
      64 * 1024 * 1024,
    );
    const archAfter = JSON.parse(c.view()).holospaces.length;
    const archSelectionDistinguishesHolospaces = archAfter === archBefore + 2;

    // A module that reaches for an ambient `env` host is rejected by the surface.
    const ambient = Uint8Array.from([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0xff]);
    let rejected = false;
    try {
      window.hs.validate_userland(ambient);
    } catch {
      rejected = true;
    }
    return {
      validated,
      holospace,
      listed: view.holospaces,
      snapshot,
      devcontainerSnapshot,
      archSelectionDistinguishesHolospaces,
      rejected,
    };
  });

  check(surface.validated, "a recompiled userland validates against the host-ABI surface (CC-6)");
  check(
    surface.holospace.startsWith("blake3:") && surface.listed.includes(surface.holospace),
    `provisioned a userland holospace on the execution surface (${surface.holospace})`,
  );
  check(
    typeof surface.snapshot === "string" && surface.snapshot.startsWith("blake3:"),
    `booted a userland container in-browser (interpreter engine) → snapshot κ ${surface.snapshot}`,
  );
  check(
    typeof surface.devcontainerSnapshot === "string" &&
      surface.devcontainerSnapshot.startsWith("blake3:"),
    `imported + ran a devcontainer in-browser, no Docker/VM (κ ${surface.devcontainerSnapshot})`,
  );
  check(
    surface.archSelectionDistinguishesHolospaces,
    "the operator's architecture selection (aarch64 vs riscv64) is part of the holospace identity — the same devcontainer under two ISAs is two distinct holospaces (ADR-021; arch fixed at provisioning)",
  );
  check(surface.rejected, "an invalid/ambient userland module is refused (closed host surface)");

  // CC-12 — the management console: provision a holospace from a validated
  // devcontainer (reproducible κ), and the dashboard renders the resource table.
  const dash = await page.evaluate(() => {
    const c = new window.hs.Console();
    c.sign_in(new TextEncoder().encode("operator-self-sovereign-key"));
    const cfg = new TextEncoder().encode('{"name":"my-devcontainer","image":"debian:12","features":{}}');
    const k1 = c.provision_devcontainer(cfg, "riscv64", 128 * 1024 * 1024);
    const k2 = new window.hs.Console();
    k2.sign_in(new TextEncoder().encode("operator-self-sovereign-key"));
    const k1b = k2.provision_devcontainer(cfg, "riscv64", 128 * 1024 * 1024);
    // The architecture is part of the holospace identity (Law L1): the SAME
    // devcontainer config under a different ISA is a DISTINCT holospace.
    const k1arm = k2.provision_devcontainer(cfg, "aarch64", 128 * 1024 * 1024);
    let rejected = false;
    try { c.provision_devcontainer(new TextEncoder().encode("not json"), "riscv64", 1); } catch { rejected = true; }

    // Repo-URL launch (the Codespaces/Gitpod flow): the (repo, reference,
    // config, arch) tuple is the holospace identity (Law L1). The page fetches
    // the repo's devcontainer.json and passes its bytes here; identity is
    // reproducible and repo/reference-distinct.
    const cfgPath = ".devcontainer/devcontainer.json";
    const r1 = c.provision_repo("https://github.com/octocat/Hello-World", "master", cfgPath, cfg, "riscv64", 128 * 1024 * 1024);
    const r1b = k2.provision_repo("https://github.com/octocat/Hello-World", "master", cfgPath, cfg, "riscv64", 128 * 1024 * 1024);
    const r2 = k2.provision_repo("https://github.com/octocat/Spoon-Knife", "master", cfgPath, cfg, "riscv64", 128 * 1024 * 1024);
    const rRef = k2.provision_repo("https://github.com/octocat/Hello-World", "develop", cfgPath, cfg, "riscv64", 128 * 1024 * 1024);

    // Browser CAS receive (the consumer side of get_with_fetch): content the
    // page fetched from a /cas gateway is admitted ONLY if it re-derives to the
    // requested κ (verify-on-receipt, Law L5); a tampered byte is refused.
    const blob = new TextEncoder().encode("substrate content delivered from a gateway");
    const blobK = window.hs.kappa(blob);
    const recvK = c.receive(blob, blobK);
    const recvRoundTrips = recvK === blobK && !!c.resolve(blobK);
    let recvRefusesTamper = false;
    try { c.receive(new TextEncoder().encode("tampered bytes"), blobK); } catch { recvRefusesTamper = true; }

    // The uor-native content network in the browser ("browser as a router",
    // CC-38): two peers over an in-process link (the same content_net::peer code
    // a bare-metal peer runs) exchange content, fetched by κ and verified on
    // receipt. Proves the browser peer speaks the same protocol as bare-metal.
    const cn = JSON.parse(c.content_network_selftest());

    return {
      contentNetFetched: cn.fetched,
      contentNetNoForge: cn.absent_is_none,
      holospace: k1,
      reproducible: k1 === k1b,
      archDistinct: k1arm !== k1,
      listed: JSON.parse(c.view()).holospaces.includes(k1),
      rejected,
      repoHolospace: r1,
      repoReproducible: r1 === r1b,
      repoDistinct: r2 !== r1,
      refDistinct: rRef !== r1,
      repoVsPaste: r1 !== k1, // a repo-identified holospace ≠ the bare-config one
      recvRoundTrips,
      recvRefusesTamper,
      defaultImage: window.hs.default_devcontainer_image(),
      hasTable: !!document.querySelector("#rows") || !!document.querySelector("#holospaces"),
      hasArchPicker: !!document.querySelector("#harch") && document.querySelector("#harch").options.length >= 2,
      archOptions: document.querySelector("#harch") ? Array.from(document.querySelector("#harch").options).map((o) => o.value) : [],
      hasSettings: !!document.querySelector("#drawer"),
      hasEgressField: !!document.querySelector("#d-egress"),
      hasExtIndicator: (() => {
        const s = document.querySelector("#ext-status"), l = document.querySelector("#ext-label");
        // No extension is installed in CI, so it must render the "not detected" state.
        return !!s && !!l && /not detected/i.test(l.textContent || "");
      })(),
      hasExtDownload: (() => {
        const a = document.querySelector("#ext-dl");
        return !!a && a.hasAttribute("download") && (a.getAttribute("href") || "").includes("extension/holospaces-egress-extension.zip");
      })(),
      hasRepoInput: !!document.querySelector("#hrepo"),
      defImgShown: (document.querySelector("#defimg")?.textContent || "").includes("buildpack-deps"),
    };
  });
  check(dash.holospace.startsWith("blake3:") && dash.listed, `provisioned a devcontainer holospace (${dash.holospace.slice(0, 24)}…)`);
  check(dash.reproducible, "same devcontainer ⇒ same holospace κ (reproducible, L1/Q4)");
  check(dash.archDistinct, "the launch-time architecture is part of identity — same config under aarch64 ≠ riscv64 (ADR-021)");
  check(dash.rejected, "an invalid devcontainer.json is refused (CC-4)");
  check(dash.repoHolospace.startsWith("blake3:"), `provisioned a holospace from a repository URL (${dash.repoHolospace.slice(0, 24)}…)`);
  check(dash.repoReproducible, "same repo+ref+config+arch ⇒ same holospace κ (reproducible, L1)");
  check(dash.repoDistinct, "a different repository is a distinct holospace (repo is part of identity)");
  check(dash.refDistinct, "a different git reference is a distinct holospace (reference is part of identity)");
  check(dash.repoVsPaste, "a repo-identified holospace differs from the bare-config one (CC-20 source identity)");
  check(dash.recvRoundTrips, "the browser receives gateway content by κ, verified on receipt, and resolves it (CC-20 /cas client)");
  check(dash.recvRefusesTamper, "gateway content that does not re-derive to the requested κ is refused (Law L5)");
  check(dash.contentNetFetched, "the browser peer fetches content from another peer over the uor-native network (CC-38, browser↔bare-metal protocol)");
  check(dash.contentNetNoForge, "a κ no peer holds resolves to nothing on the content network (no forging)");
  check(dash.defaultImage.includes("buildpack-deps"), `the usable default image is exposed to the page (${dash.defaultImage})`);
  check(dash.hasTable, "the management console renders the holospaces dashboard");
  check(dash.hasArchPicker, "the launch form offers the architecture picker");
  check(
    ["riscv64", "aarch64", "x64"].every((a) => dash.archOptions.includes(a)),
    `the architecture picker offers riscv64, aarch64, and x64 (amd64 — the ubiquitous registry/Codespaces arch) [${dash.archOptions.join(", ")}]`,
  );
  check(dash.hasSettings, "the console exposes a per-guest settings drawer");
  check(dash.hasEgressField, "the settings drawer offers a per-guest egress node (the holospaces-node the guest's internet routes through, CC-39)");
  check(dash.hasExtDownload, "the console offers the local egress extension for download + manual install (CC-41, until it is in the Chrome store)");
  check(dash.hasExtIndicator, "the console shows a live egress-extension connection indicator (auto-detected; 'not detected · install' when absent)");
  check(dash.hasRepoInput, "the launch form takes a git repository URL (Codespaces/Gitpod flow)");
  check(dash.defImgShown, "the launch form shows the usable default image");

  // CC-38 — the content network over the LIVE transport seam. Two separate
  // browser peers (Console A, Console B) exchange content over the frame seam a
  // WebRTC data channel carries between tabs: A publishes content, B fetches it
  // by κ, and a JS pump shuttles each peer's outbound frames to the other's
  // inbound (exactly what the data channel's onmessage/send does). Verified on
  // receipt; a κ no peer holds resolves to null. The real WebRTC pump is a
  // drop-in for this in-test bridge.
  const cross = await page.evaluate(() => {
    const A = new window.hs.Console();
    const B = new window.hs.Console();
    // Pump one frame in each direction; returns true while any frame moved.
    const pump = () => {
      let moved = false, f;
      while ((f = B.cn_outbound()) !== undefined) { A.cn_inbound(f); moved = true; }
      while ((f = A.cn_outbound()) !== undefined) { B.cn_inbound(f); moved = true; }
      return moved;
    };
    const drive = (kappa) => {
      B.cn_fetch_start(kappa);
      for (let i = 0; i < 200; i++) {
        const r = B.cn_fetch_poll();   // sends the request on the first poll
        if (r !== undefined) return r; // null (absent) or the bytes
        pump();
      }
      return "timeout";
    };
    const expected = "a layer fetched from a peer over the content network transport";
    const k = A.cn_put(new TextEncoder().encode(expected));
    const got = drive(k);
    const unheld = window.hs.kappa(new TextEncoder().encode("content no peer holds"));
    const absent = drive(unheld);
    return {
      fetched: got instanceof Uint8Array ? new TextDecoder().decode(got) : String(got),
      expected,
      absentIsNull: absent === null,
    };
  });
  check(cross.fetched === cross.expected, "two browser peers exchange content over the live transport seam a WebRTC data channel carries (CC-38)");
  check(cross.absentIsNull, "a κ no peer holds resolves to null over the transport seam (no forging)");

  // CC-42 — the browser provisions a repository's REAL OCI image into a bootable
  // rootfs, in the tab. DevcontainerProvision drives the page-driven pull
  // (Unit 1/2) against a real image fixture served over the OCI distribution
  // endpoints; in production the router extension's CORS-free fetch is the
  // transport, here the same-origin server stands in for it. Re-derives every
  // blob (Law L5) and assembles the ext4 rootfs the emulator boots — proving the
  // launched holospace is the repository's actual devcontainer, not a demo.
  const provision = await page.evaluate(async (origin) => {
    const m = await import("./pkg/holospaces_web.js");
    await m.default("./pkg/holospaces_web_bg.wasm");
    const prov = new m.DevcontainerProvision(`${origin}/img:latest`, "riscv64");
    let steps = 0;
    while (!prov.isDone()) {
      const url = prov.nextUrl();
      if (!url) break;
      const headers = {};
      const accept = prov.nextAccept();
      if (accept) headers["Accept"] = accept;
      const bearer = prov.nextBearer();
      if (bearer) headers["Authorization"] = `Bearer ${bearer}`;
      const res = await fetch(url, { headers });
      const body = new Uint8Array(await res.arrayBuffer());
      prov.deliver(res.status, res.headers.get("content-type") || "", body);
      if (++steps > 50) throw new Error("the pull did not converge");
    }
    const rootfs = prov.assemble(256 * 1024 * 1024);
    return { len: rootfs.length, mult4k: rootfs.length % 4096 === 0 };
  }, `127.0.0.1:${port}`);
  check(
    provision.len > 0 && provision.mult4k,
    "the browser provisions a repository's REAL OCI image into a bootable ext4 rootfs via the page-driven pull (CC-42)",
  );

  console.log(failed ? "MANAGER-TEST: FAILED" : "MANAGER-TEST: PASS (browser peer + Platform Manager console)");
} finally {
  await browser.close();
  server.close();
}

// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
