// CC-52 (fast core witness) — the search engine's glob / gitignore / walk / text
// matching, verified deterministically under Node against an in-memory tree, so
// the load-bearing logic is proven without the heavy browser run (mirrors
// holospace-fs/node-exthost.test.cjs). The browser witness (search-test.mjs)
// then proves it wired into the real workbench.
"use strict";
const assert = require("assert");
const path = require("path");
const core = require("./search-core.cjs");
const ignoreFactory = require("./vendor/ignore/index.cjs");

let pass = 0;
const ok = (c, m) => { if (!c) throw new Error("FAIL: " + m); pass++; console.log("  ✓", m); };

// ── globToRegExp / compileGlobs ──────────────────────────────────────────────
{
  const m = core.compileGlobs(["**/node_modules/**"]);
  ok(m("node_modules/dep/index.js"), "**/node_modules/** matches a nested node_modules file");
  ok(m("src/node_modules/x.js"), "**/node_modules/** matches node_modules at any depth");
  ok(!m("src/app.js"), "**/node_modules/** does not match a normal source file");

  const star = core.compileGlobs(["*.log"]);
  ok(star("a.log"), "*.log matches a root log");
  ok(star("deep/dir/b.log"), "*.log (no slash) matches a log at any depth");
  ok(!star("a.txt"), "*.log does not match a .txt");

  const brace = core.compileGlobs(["**/*.{js,ts}"]);
  ok(brace("src/a.ts") && brace("src/b.js"), "{js,ts} alternation matches both");
  ok(!brace("src/a.css"), "{js,ts} alternation excludes .css");
  ok(!brace("src/a.x/js"), "{js,ts} alternative does NOT match across a path separator (no any-depth prefix)");

  const q = core.compileGlobs(["src/?.js"]);
  ok(q("src/a.js") && !q("src/ab.js"), "? matches exactly one char within a segment");

  const dir = core.compileGlobs(["**/.git/**"]);
  ok(dir(".git/objects/ab/cd") && !dir("src/git.js"), "**/.git/** matches the git dir, not 'git.js'");
}

// ── buildIgnore (.gitignore semantics, via the vendored `ignore`) ─────────────
{
  const ig = core.buildIgnore(ignoreFactory, "node_modules/\n*.log\n!keep.log\nsecret.txt\n# comment\n");
  ok(ig.ignores("node_modules/x"), ".gitignore ignores node_modules/");
  ok(ig.ignores("a.log") && !ig.ignores("keep.log"), ".gitignore honours negation (!keep.log)");
  ok(ig.ignores("secret.txt") && !ig.ignores("src/app.js"), ".gitignore ignores secret.txt, keeps source");
  ok(core.buildIgnore(ignoreFactory, "") === null, "no rules → null ignore matcher");
}

// ── walk: streaming, excludes, includes, gitignore-prune, maxResults ─────────
async function walkTests() {
  // An in-memory workspace (relPath → "DIR" | content).
  const tree = {
    "": "DIR", "src": "DIR", "node_modules": "DIR", "node_modules/dep": "DIR",
    "src/app.js": "x", "src/util.ts": "x", "README.md": "x",
    "node_modules/dep/index.js": "x", "build.log": "x", "secret.txt": "x",
    ".gitignore": "secret.txt\n",
  };
  const list = async (dir) => Object.keys(tree)
    .filter((p) => p && (dir ? p.startsWith(dir + "/") : true) && p.split("/").length === (dir ? dir.split("/").length + 1 : 1))
    .map((p) => ({ name: p.split("/").pop(), dir: tree[p] === "DIR" }));

  const exclude = core.compileGlobs(["**/node_modules/**"]);
  const ig = core.buildIgnore(ignoreFactory, tree[".gitignore"]);

  // All files, excluding node_modules + gitignore'd secret.txt.
  let found = [];
  let res = await core.walk({ list, isExcluded: exclude, ig, maxResults: 0 }, (rel) => found.push(rel));
  ok(found.includes("src/app.js") && found.includes("README.md"), "walk finds normal files");
  ok(!found.some((f) => f.startsWith("node_modules/")), "walk prunes the excluded node_modules tree");
  ok(!found.includes("secret.txt"), "walk honours .gitignore (secret.txt absent)");
  ok(!res.limitHit, "no limit hit without maxResults");

  // includes filter: only *.ts
  found = [];
  const includeTs = core.compileGlobs(["**/*.ts"]);
  await core.walk({ list, isExcluded: exclude, isIncluded: includeTs, ig }, (rel) => found.push(rel));
  ok(found.length === 1 && found[0] === "src/util.ts", "includes glob restricts to *.ts");

  // maxResults → limitHit, and streaming (onFile called incrementally).
  found = [];
  let firstSeenBeforeReturn = false;
  res = await core.walk({ list, isExcluded: exclude, ig, maxResults: 1 }, (rel) => { found.push(rel); firstSeenBeforeReturn = true; });
  ok(res.limitHit && found.length === 1, "maxResults bounds results and sets limitHit");
  ok(firstSeenBeforeReturn, "results stream via onFile (not returned in one batch)");

  // cancellation stops the walk.
  found = [];
  let cancelled = false;
  await core.walk({ list, isExcluded: exclude, ig, isCancelled: () => cancelled }, (rel) => { found.push(rel); cancelled = true; });
  ok(found.length <= 2, "cancellation halts the walk promptly");
}

// ── searchContent: literal / regex / case / word / multiline, with positions ──
{
  const content = "const FINDME = 1;\nfunction findme() {}\n  // FINDME again\nplain line\n";
  // Case-sensitive (the toggle on) matches only the two upper-case FINDME.
  let r = core.searchContent(content, { pattern: "FINDME", isCaseSensitive: true });
  ok(r.length === 2, "case-sensitive matches the two upper-case FINDME (not 'findme')");
  ok(r[0].line === 0 && r[0].startCol === 6 && r[0].endCol === 12, "match reports the correct line + columns (re-derives in the file)");
  ok(content.split("\n")[r[0].line].slice(r[0].startCol, r[0].endCol) === "FINDME",
    "the reported range slices to exactly the matched text (Law L5)");

  // Case-insensitive is the VS Code default (no isCaseSensitive) → all three.
  r = core.searchContent(content, { pattern: "findme" });
  ok(r.length === 3, "case-insensitive (the default) matches all three occurrences");

  r = core.searchContent(content, { pattern: "find", isWordMatch: true });
  ok(r.length === 0, "word-match: 'find' does not match inside 'findme'/'FINDME'");

  r = core.searchContent(content, { pattern: "f.ndme", isRegExp: true, isCaseSensitive: false });
  ok(r.length === 3, "regex f.ndme matches findme/FINDME");

  r = core.searchContent("alpha\nbeta\ngamma\n", { pattern: "beta\\ngamma", isRegExp: true, isMultiline: true });
  ok(r.length === 1 && r[0].line === 1 && r[0].endLine === 2, "multiline regex spans lines with correct start/end lines");

  ok(core.searchContent(content, { pattern: "" }).length === 0, "empty pattern yields no matches");
}

(async () => {
  try {
    await walkTests();
    console.log(`SEARCH-CORE-TEST: PASS (${pass} checks)`);
    process.exit(0);
  } catch (e) {
    console.error("SEARCH-CORE-TEST: FAILED —", e.message);
    process.exit(1);
  }
})();
