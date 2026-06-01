// Compose the real VS Code web workbench as holospaces' OWN served content
// (CC-17 Phase 3, ADR-012/ADR-015). holospaces is the substrate gateway: it
// serves Microsoft's κ-verified executable core BYTE-IDENTICAL (Law L5) and
// composes it with (a) VS Code's supported web-embedding bootstrap (`create()`,
// from `@vscode/test-web`), and (b) the `holospace-fs` builtin extension that
// boots the holospace in the extension-host worker and binds the workbench to
// its virtio-9p workspace (CC-15) + console (CC-11). No server: the result is
// static content the deploy serves and the witness checks.
//
// Used two ways:
//   • as a module — `composeWorkbenchHtml(...)` (the CC-17 Phase 3 witness);
//   • as a script — `node build-workbench.mjs <siteDir>` assembles the workbench
//     into the deploy's `_site` (the Pages build, see .github/workflows/pages.yml).
import { readFile, writeFile, cp, mkdir, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

export const WORKBENCH_PIN = "vscode-web@1.91.1";
const BOOTSTRAP_PIN = "@vscode/test-web@0.0.80";

// The runtime config-builder: it fills the workbench's web-configuration meta
// from `window.location`, so the same HTML works at any origin/path (a user site
// at the root, or a project site under `/<repo>/`). It opens the holospace
// workspace folder, registers `holospace-fs` as a builtin (located beside this
// page), and wires the OPEN gallery (Open VSX) — arbitrary extensions, no lock-in.
const RUNTIME_CONFIG = `<script>(function(){var loc=window.location;var dir=loc.pathname.replace(/\\/[^/]*$/,"");var cfg={folderUri:{"$mid":1,scheme:"holospace",authority:"",path:"/workspace"},additionalBuiltinExtensions:[{scheme:loc.protocol.replace(":",""),authority:loc.host,path:dir+"/ext/holospace-fs"}],productConfiguration:{nameShort:"holospaces VS Code",nameLong:"holospaces VS Code",applicationName:"code-web",version:"1.91.1",extensionsGallery:{serviceUrl:"https://open-vsx.org/vscode/gallery",itemUrl:"https://open-vsx.org/vscode/item",resourceUrlTemplate:"https://open-vsx.org/vscode/unpkg/{publisher}/{name}/{version}/{path}"}}};document.getElementById("vscode-workbench-web-configuration").setAttribute("data-settings",JSON.stringify(cfg));})();</script>`;

/**
 * Compose the workbench HTML. `baseUrl` is where the vscode-web dist is served
 * relative to the page (`.` when the dist is at the page's directory, `./workbench`
 * when it is in a `workbench/` subdirectory).
 */
export async function composeWorkbenchHtml({ distDir, twDir, baseUrl }) {
  const tpl = await readFile(path.join(distDir, "out/vs/code/browser/workbench/workbench.html"), "utf8");
  // The supported web-embedding bootstrap — it reads the web-configuration meta
  // and calls the workbench's `create()` (the dist's stock `workbench.js` is the
  // server bootstrap, which does not wire `additionalBuiltinExtensions`).
  let twMain = await readFile(path.join(twDir, "out/browser/amd/main.js"), "utf8");
  twMain = twMain.replace("./workbench.api", "vs/workbench/workbench.web.main") +
    '\nrequire(["vscode-web-browser-main"], function() { });';

  let html = tpl
    .replaceAll("{{WORKBENCH_WEB_BASE_URL}}", baseUrl)
    .replaceAll("{{WORKBENCH_WEB_CONFIGURATION}}", "{}")
    .replaceAll("{{WORKBENCH_AUTH_SESSION}}", "")
    .replaceAll("{{WORKBENCH_NLS_BASE_URL}}", "");
  // Fill the config at runtime (after the meta, before the bootstrap).
  html = html.replace(
    '<meta id="vscode-workbench-web-configuration" data-settings="{}">',
    '<meta id="vscode-workbench-web-configuration" data-settings="{}">\n' + RUNTIME_CONFIG,
  );
  // Swap the dist's server bootstrap for the supported web `create()` bootstrap.
  html = html.replace(/<script src="[^"]*workbench\/workbench\.js"><\/script>/, `<script>${twMain}</script>`);
  return html;
}

/** κ-verify the workbench's executable core against the committed manifest (L5). */
async function verifyCore(distDir, manifestPath) {
  const manifest = (await readFile(manifestPath, "utf8"))
    .split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"))
    .map((l) => { const [hash, file] = l.split(/\s+/); return { hash, file }; });
  for (const { hash, file } of manifest) {
    const got = createHash("sha256").update(await readFile(path.join(distDir, file))).digest("hex");
    if (got !== hash) throw new Error(`workbench core integrity failed: ${file} (${got} ≠ ${hash})`);
  }
  return manifest.length;
}

// ── Deploy build: assemble the workbench into the site directory ─────────────
async function main(siteDir) {
  const DIR = path.dirname(fileURLToPath(import.meta.url));
  const ROOT = path.resolve(DIR, "../../..");
  const distDir = path.join(DIR, "node_modules/vscode-web/dist");
  const twDir = path.join(DIR, "node_modules/@vscode/test-web");
  try { await stat(distDir); await stat(twDir); }
  catch { execSync(`npm install --no-save ${WORKBENCH_PIN} ${BOOTSTRAP_PIN}`, { cwd: DIR, stdio: "inherit" }); }

  const n = await verifyCore(distDir, path.join(ROOT, "vv/artifacts/cc17/vendor.sha256"));
  console.log(`build-workbench: κ-verified the workbench core (${n} files, Law L5)`);

  await mkdir(path.join(siteDir, "workbench"), { recursive: true });
  await cp(distDir, path.join(siteDir, "workbench"), { recursive: true });
  await cp(path.join(DIR, "builtin-extensions/holospace-fs"), path.join(siteDir, "ext/holospace-fs"), { recursive: true });
  const html = await composeWorkbenchHtml({ distDir, twDir, baseUrl: "./workbench" });
  await writeFile(path.join(siteDir, "workbench.html"), html);
  console.log(`build-workbench: composed ${path.join(siteDir, "workbench.html")} (real workbench + holospace-fs + Open VSX)`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const siteDir = process.argv[2];
  if (!siteDir) { console.error("usage: node build-workbench.mjs <siteDir>"); process.exit(2); }
  main(siteDir).catch((e) => { console.error(e); process.exit(1); });
}
