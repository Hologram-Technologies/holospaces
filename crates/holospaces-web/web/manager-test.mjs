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

  console.log(failed ? "MANAGER-TEST: FAILED" : "MANAGER-TEST: PASS (browser peer + Platform Manager)");
} finally {
  await browser.close();
  server.close();
}

process.exitCode = failed ? 1 : 0;
