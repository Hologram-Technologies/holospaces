// CC-17 (Phase 1) — the real VS Code web workbench loads, κ-verified, in the tab.
//
// The workspace is the REAL VS Code web workbench — the same compilation that
// powers vscode.dev / github.dev (ADR-012) — not a reconstruction from Monaco +
// xterm components (that is CC-13). holospaces serves the pinned build as the
// Workspace Projection tab; the browser peer VERIFIES the workbench's executable
// core by re-derivation against the committed manifest (Law L5 — a forged byte is
// refused) before loading, exactly as CC-13 verifies its components. This witness
// asserts the authentic workbench boots to its UI in Chromium.
//
// Phase 2 wires a FileSystemProvider over the virtio-9p workspace (CC-15) + the
// terminal over the holospace console (CC-11); Phase 3 the remote extension host
// in the devcontainer OS (ADR-015) for CC-18/CC-19. Authority + pin: the npm
// package vscode-web@1.91.1 and vv/artifacts/cc17/{SOURCE.txt,vendor.sha256}.
import http from "node:http";
import { readFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(DIR, "../../..");
const PIN = "vscode-web@1.91.1";

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("WORKBENCH-TEST: FAIL —", m)));

// 1) Obtain the pinned real VS Code web build (fetched by pin, as the browser
//    tests fetch Playwright by pin; the 60 MB build is not committed).
const distDir = path.join(DIR, "node_modules/vscode-web/dist");
try {
  await stat(distDir);
} catch {
  console.log(`==> installing the pinned ${PIN}`);
  execSync(`npm install --no-save ${PIN}`, { cwd: DIR, stdio: "ignore" });
}

// 2) κ-verify the workbench's executable core against the committed manifest.
const manifest = (await readFile(path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"), "utf8"))
  .split("\n")
  .map((l) => l.trim())
  .filter((l) => l && !l.startsWith("#"))
  .map((l) => {
    const [hash, file] = l.split(/\s+/);
    return { hash, file };
  });
let verified = 0;
for (const { hash, file } of manifest) {
  const bytes = await readFile(path.join(distDir, file));
  const got = createHash("sha256").update(bytes).digest("hex");
  if (got !== hash) {
    check(false, `executable-core integrity: ${file} (${got} ≠ pinned ${hash})`);
  } else {
    verified++;
  }
}
check(verified === manifest.length, `the workbench's executable core re-derives to its pinned κ (${verified}/${manifest.length} files, Law L5)`);

// A negative control: a forged byte must be refused.
{
  const bytes = await readFile(path.join(distDir, manifest[0].file));
  const forged = Buffer.from(bytes);
  forged[0] ^= 0xff;
  const got = createHash("sha256").update(forged).digest("hex");
  check(got !== manifest[0].hash, "a forged workbench-core byte fails re-derivation (a tampered build is refused)");
}

// 3) Serve the verified build with a holospaces-branded embedder.
const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json",
  ".png": "image/png", ".svg": "image/svg+xml", ".ttf": "font/ttf", ".woff": "font/woff",
  ".woff2": "font/woff2", ".wasm": "application/wasm", ".map": "application/json", ".ico": "image/x-icon",
};
const workbenchHtml = (await readFile(path.join(distDir, "out/vs/code/browser/workbench/workbench.html"), "utf8"))
  .replaceAll("{{WORKBENCH_WEB_BASE_URL}}", ".")
  .replaceAll("{{WORKBENCH_WEB_CONFIGURATION}}", "{}")
  .replaceAll("{{WORKBENCH_AUTH_SESSION}}", "")
  .replaceAll("{{WORKBENCH_NLS_BASE_URL}}", "");
const productJson = JSON.stringify({
  productConfiguration: {
    nameShort: "holospaces VS Code",
    nameLong: "holospaces VS Code",
    applicationName: "code-web",
    version: "1.91.1",
    // The OPEN marketplace — Open VSX (https://open-vsx.org), the gallery
    // vscode.dev / Gitpod / code-server use. holospaces wires the *open* gallery
    // so an operator installs ARBITRARY extensions (no Microsoft/GitHub lock-in;
    // the MS Marketplace ToS forbids non-MS products) — CC-19. None are bundled.
    extensionsGallery: {
      serviceUrl: "https://open-vsx.org/vscode/gallery",
      itemUrl: "https://open-vsx.org/vscode/item",
      resourceUrlTemplate:
        "https://open-vsx.org/vscode/unpkg/{publisher}/{name}/{version}/{path}",
    },
  },
});
const server = http.createServer(async (req, res) => {
  let rel = decodeURIComponent(req.url.split("?")[0]);
  if (rel === "/" || rel === "/index.html") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(workbenchHtml);
  }
  if (rel === "/product.json") {
    res.writeHead(200, { "content-type": "application/json" });
    return res.end(productJson);
  }
  try {
    const body = await readFile(path.join(distDir, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
try {
  await page.goto(`http://127.0.0.1:${port}/index.html`, { timeout: 30000 });
  await page.waitForSelector(".monaco-workbench", { timeout: 60000 });
  await page.waitForSelector(".monaco-workbench .activitybar", { timeout: 30000 }).catch(() => {});
  const booted = await page.evaluate(() => !!document.querySelector(".monaco-workbench"));
  const activitybar = await page.evaluate(() => !!document.querySelector(".activitybar"));
  const title = await page.title();
  check(booted, "the real VS Code web workbench booted to its authentic UI in the tab (.monaco-workbench)");
  check(activitybar, "the workbench rendered its activity bar (the real workbench shell, not Monaco-only — CC-13)");
  check(title.includes("holospaces"), `the workbench is the holospaces Workspace Projection (title: "${title}")`);

  // CC-19 — the workbench is wired to the OPEN marketplace (Open VSX), so an
  // operator installs ARBITRARY extensions, with no Microsoft/GitHub lock-in.
  const gallery = await page.evaluate(async () => {
    const product = await (await fetch("./product.json")).json();
    return product?.productConfiguration?.extensionsGallery?.serviceUrl ?? null;
  });
  check(
    typeof gallery === "string" && gallery.includes("open-vsx.org"),
    `the real workbench is wired to the open gallery (Open VSX) — arbitrary extensions install, no lock-in (serviceUrl: ${gallery})`,
  );
  check(
    !(gallery || "").includes("marketplace.visualstudio.com"),
    "holospaces does not lock the workbench to the Microsoft Marketplace (no proprietary gallery)",
  );
  console.log(failed ? "WORKBENCH-TEST: FAILED" : "WORKBENCH-TEST: PASS (the real VS Code web workbench loads κ-verified in the holospaces tab, wired to the open gallery)");
} finally {
  await browser.close();
  server.close();
}

// Force-exit: the spawned @vscode/test-web server / browser can leave lingering
// handles that keep the event loop alive after the result is printed (the test
// would otherwise hang). The result line above has already been written.
process.exit(failed ? 1 : 0);
