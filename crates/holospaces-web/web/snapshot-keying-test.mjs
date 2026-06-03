// CC-31 (multi-holospace isolation) — a deterministic guard that the browser
// peer keys each holospace's durable resume state by its IDENTITY (κ), never a
// single global OPFS slot.
//
// THE REGRESSION THIS CATCHES: the resume snapshot (CC-31) was first written to
// a fixed filename ("holospace-devcontainer.snapshot.*"). OPFS is per-origin and
// shared by every workbench tab, so every holospace read and wrote the *same*
// slot — launching one holospace resumed another's machine (its files, its idle
// shell), and deleting/renaming left remnants that bled into the next launch.
// Identity is what-not-where (Law L1): the fix keys every artifact by the κ
// carried in the workspace folder authority. This witness fails if the keying
// ever collapses two distinct holospaces onto one slot, or if the Platform
// Manager's cleanup mapping drifts from the workbench's (so a delete would fail
// to clear what the workbench wrote).
import { createRequire } from "node:module";
import Module from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";

const DIR = path.dirname(fileURLToPath(import.meta.url));
let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("KEYING-TEST: FAIL —", m)));

// Load the real extension with a minimal `vscode` stub (it only destructures a
// few symbols at load; the keying helpers we test are pure and vscode-free).
const realLoad = Module._load;
Module._load = function (request, parent, isMain) {
  if (request === "vscode") return {};
  return realLoad.apply(this, arguments);
};
const require = createRequire(import.meta.url);
const ext = require(path.join(DIR, "builtin-extensions/holospace-fs/extension.js"));
Module._load = realLoad;

const { sanitizeHoloKey, namesFor } = ext._keying || {};
check(typeof sanitizeHoloKey === "function" && typeof namesFor === "function", "the extension exposes its identity→OPFS keying helpers");

// The Platform Manager's cleanup mapping (index.html `holoKeyOf`) — kept here
// verbatim as the contract; if the two drift, delete/terminate would not clear
// what the workbench persisted.
const managerHoloKeyOf = (k) => String(k).replace(/[^A-Za-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "default";

// Two genuinely distinct holospace identities (real κ-shaped strings).
const KA = "sha256:9d180ad71797ac0df935fc4b5647fc1b22bbff5e7fd5b44c15643d384a2cda72";
const KB = "sha256:4b41108a8c4417bc25e8bbe4377f81c0e809eda2330848f6c65b1d7a050776db";

const a = namesFor(sanitizeHoloKey(KA));
const b = namesFor(sanitizeHoloKey(KB));

// 1) Distinct holospaces → fully disjoint OPFS slots (the anti-bleed property).
const aFiles = [a.snapshot, a.kappa, a.scrollback];
const bFiles = [b.snapshot, b.kappa, b.scrollback];
check(
  aFiles.every((f) => !bFiles.includes(f)),
  "distinct holospaces map to DISJOINT OPFS files — no shared/global slot (no cross-holospace bleed)",
);
// 2) The identity is actually in the filename (not a constant that happens to differ).
check(a.snapshot.includes(sanitizeHoloKey(KA)) && b.snapshot.includes(sanitizeHoloKey(KB)), "each holospace's filename embeds its sanitized κ");
// 3) Same identity → same slot (a holospace resumes its OWN state across sessions).
check(a.snapshot === namesFor(sanitizeHoloKey(KA)).snapshot, "the same identity is stable — a holospace resumes its own slot");
// 4) Empty identity (the single-holospace demo) is the lone "default" slot, and a
//    real κ never collides with it.
check(sanitizeHoloKey("") === "default" && sanitizeHoloKey(KA) !== "default", "an empty identity is the single 'default' slot; a real κ is never 'default'");
// 5) The Manager's cleanup mapping AGREES with the workbench's — so remove/terminate
//    clears exactly the files the workbench wrote.
check(managerHoloKeyOf(KA) === sanitizeHoloKey(KA) && managerHoloKeyOf(KB) === sanitizeHoloKey(KB), "the Manager's delete-cleanup key matches the workbench's key (cleanup clears the right slot)");
// 6) Filenames are OPFS-safe (no path separators that could escape the namespace).
check([...aFiles, ...bFiles].every((f) => !/[/\\]/.test(f)), "the keyed filenames are flat OPFS-safe names (no separators)");

// 7) The build-workbench wiring threads the per-launch `?id=<κ>` into the
//    workspace folder authority — the carrier the extension keys on. Run the real
//    injected config script (the deployed artifact's) in a sandbox for two launch
//    URLs and assert the resulting folderUri.authority differs accordingly. This
//    closes the loop: Manager `?id` → folder authority → `deriveHoloKey` → slot.
import vm from "node:vm";
const { runtimeConfig } = await import("./build-workbench.mjs");
function injectedAuthority(search) {
  const html = runtimeConfig([]);
  const code = html.replace(/^<script>/, "").replace(/<\/script>$/, "");
  let captured = null;
  const sandbox = {
    window: { location: { pathname: "/holospaces/workbench.html", search, protocol: "https:", host: "x.github.io" } },
    URLSearchParams,
    JSON,
    document: {
      getElementById: () => ({ setAttribute: (_k, v) => (captured = JSON.parse(v)) }),
    },
  };
  vm.runInNewContext(code, sandbox);
  return captured && captured.folderUri && captured.folderUri.authority;
}
const authA = injectedAuthority(`?id=${encodeURIComponent(KA)}`);
const authB = injectedAuthority(`?id=${encodeURIComponent(KB)}`);
const authNone = injectedAuthority("");
check(authA === KA && authB === KB, "build-workbench threads ?id=<κ> into the workspace folder authority (per-launch identity)");
check(authA !== authB, "two launches with different ?id get different folder authorities (the carrier the extension keys on)");
check(authNone === "", "no ?id (single-holospace demo) → empty authority → the 'default' slot");
// And end-to-end: the authority the wiring produces sanitizes to the SAME slot the
// extension would compute — Manager launch → distinct OPFS files.
check(
  namesFor(sanitizeHoloKey(authA)).snapshot !== namesFor(sanitizeHoloKey(authB)).snapshot,
  "end-to-end: two Manager launches resolve to DISJOINT OPFS snapshot files",
);

console.log(failed ? "KEYING-TEST: FAILED" : "KEYING-TEST: PASS (each holospace's resume state is keyed by its identity — distinct holospaces never bleed)");
process.exit(failed ? 1 : 0);
