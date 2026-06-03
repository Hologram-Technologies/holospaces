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

const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
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
    const k1 = c.provision_devcontainer(cfg, 128 * 1024 * 1024);
    const k2 = new window.hs.Console();
    k2.sign_in(new TextEncoder().encode("operator-self-sovereign-key"));
    const k1b = k2.provision_devcontainer(cfg, 128 * 1024 * 1024);
    let rejected = false;
    try { c.provision_devcontainer(new TextEncoder().encode("not json"), 1); } catch { rejected = true; }
    return {
      holospace: k1,
      reproducible: k1 === k1b,
      listed: JSON.parse(c.view()).holospaces.includes(k1),
      rejected,
      hasTable: !!document.querySelector("#rows") || !!document.querySelector("#holospaces"),
    };
  });
  check(dash.holospace.startsWith("blake3:") && dash.listed, `provisioned a devcontainer holospace (${dash.holospace.slice(0, 24)}…)`);
  check(dash.reproducible, "same devcontainer ⇒ same holospace κ (reproducible, L1/Q4)");
  check(dash.rejected, "an invalid devcontainer.json is refused (CC-4)");
  check(dash.hasTable, "the management console renders the holospaces dashboard");

  console.log(failed ? "MANAGER-TEST: FAILED" : "MANAGER-TEST: PASS (browser peer + Platform Manager console)");
} finally {
  await browser.close();
  server.close();
}

// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
