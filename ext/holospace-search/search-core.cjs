// holospace-search — the search engine core (CC-52).
//
// Pure, browser-safe, dependency-free: the caller injects the async filesystem
// (a root-relative `list`/`read` backed by the holospace's virtio-9p workspace,
// CC-15) and the `ignore` factory (vendored, κ-pinned), so the SAME code runs in
// the web extension host (fetched + evaluated, like the SCM engine) AND under
// Node for the fast unit witness. No `require`, no globals beyond standard ES.
//
// It implements what a FileSearchProvider / TextSearchProvider needs: a streaming
// tree walk that honours VS Code exclude/include globs and `.gitignore`
// (`useIgnoreFiles`), and a text matcher faithful to a TextSearchQuery
// (pattern / isRegExp / isCaseSensitive / isWordMatch / isMultiline) whose
// reported ranges re-derive against the file's actual content (Law L5).
"use strict";

// ── VS Code glob → RegExp ─────────────────────────────────────────────────────
// Supports the search-glob subset: `**` (any number of path segments, incl.
// none), `*` (any run within a segment), `?` (one char within a segment),
// `{a,b,c}` alternation, and `[...]` character classes. Matching is against a
// POSIX, root-relative path (no leading slash).
// Translate a glob to a regex BODY (no anchors, no any-depth prefix). `{a,b}`
// alternatives are translated through this same body function, so an alternative
// NEVER inherits the no-slash "anywhere" prefix `globToRegExp` adds — otherwise
// `**/*.{js,ts}` would let `js`/`ts` match across a `/` (e.g. `src/a.x/js`).
function globBody(glob) {
  let re = "";
  let i = 0;
  const n = glob.length;
  while (i < n) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        // `**` — zero or more segments. Consume an optional following `/`.
        i += 2;
        if (glob[i] === "/") i++;
        re += "(?:[^/]*(?:/|$))*";
      } else {
        re += "[^/]*";
        i++;
      }
    } else if (c === "?") {
      re += "[^/]";
      i++;
    } else if (c === "{") {
      // `{a,b,c}` → `(?:a|b|c)` (commas split alternatives; each a glob body).
      const end = glob.indexOf("}", i);
      if (end < 0) { re += "\\{"; i++; continue; }
      const parts = glob.slice(i + 1, end).split(",").map(globBody);
      re += "(?:" + parts.join("|") + ")";
      i = end + 1;
    } else if (c === "[") {
      const end = glob.indexOf("]", i);
      if (end < 0) { re += "\\["; i++; continue; }
      re += glob.slice(i, end + 1);
      i = end + 1;
    } else if ("\\^$.|+()".includes(c)) {
      re += "\\" + c;
      i++;
    } else {
      re += c;
      i++;
    }
  }
  return re;
}

// A glob with no `/` (e.g. `*.log`, `node_modules`) matches a basename at any
// depth (the no-slash "anywhere" prefix); a glob with a `/` is anchored to the
// root-relative path.
function globToRegExp(glob) {
  const body = globBody(glob);
  const full = glob.includes("/") ? body : "(?:.*/)?" + body;
  return new RegExp("^" + full + "$");
}

// Compile a list of globs into a single predicate `(relPath) => boolean` (true if
// any glob matches). An empty list yields a predicate that is always false.
function compileGlobs(globs) {
  const res = (globs || []).filter(Boolean).map(globToRegExp);
  return (relPath) => res.some((r) => r.test(relPath));
}

// Build a `.gitignore` matcher from the injected `ignore` factory and the root
// `.gitignore` text (the standard location VS Code's `useIgnoreFiles` honours).
// Returns `{ ignores(relPath) }`, or null when there are no rules.
function buildIgnore(ignoreFactory, gitignoreText) {
  if (!ignoreFactory || !gitignoreText) return null;
  const lines = gitignoreText.split(/\r?\n/).filter((l) => l && !l.startsWith("#"));
  if (!lines.length) return null;
  const ig = ignoreFactory();
  ig.add(lines);
  return { ignores: (relPath) => { try { return ig.ignores(relPath); } catch { return false; } } };
}

// ── The streaming tree walk ───────────────────────────────────────────────────
// `list(relDir)` → Promise<[{ name, dir }]> over the workspace (CC-15); excluded
// directories are pruned (never descended), so a large `node_modules` costs
// nothing. `onFile(relPath)` is AWAITED for each surviving file AS it is found —
// so a caller that reads + matches + reports inside it streams results while the
// walk is still running (the text provider does exactly this), instead of after
// a full enumeration. Honours excludes/includes/gitignore, `maxResults` (a bound
// on files handed to `onFile`), and cancellation. Returns `{ limitHit }`.
async function walk({ list, isExcluded, isIncluded, ig, maxResults, isCancelled }, onFile) {
  let found = 0;
  let limitHit = false;
  const max = maxResults && maxResults > 0 ? maxResults : Infinity;
  const queue = [""]; // BFS over directories so results stream broadly, not depth-first
  while (queue.length) {
    if (isCancelled && isCancelled()) break;
    const dir = queue.shift();
    let entries;
    try {
      entries = await list(dir);
    } catch {
      continue; // a directory that vanished / is unreadable — skip, never throw
    }
    for (const { name, dir: isDir } of entries) {
      if (limitHit || (isCancelled && isCancelled())) break;
      const rel = dir ? `${dir}/${name}` : name;
      if (isExcluded && isExcluded(rel)) continue;
      if (ig && ig.ignores(rel)) continue;
      if (isDir) {
        queue.push(rel);
      } else {
        if (isIncluded && !isIncluded(rel)) continue;
        await onFile(rel);
        found++;
        if (found >= max) { limitHit = true; break; }
      }
    }
    if (limitHit) break;
  }
  return { limitHit };
}

// ── Text matching ─────────────────────────────────────────────────────────────
// Compile a TextSearchQuery into a global RegExp over the file content. A literal
// query is escaped; word-match wraps `\b`; case sensitivity + multiline follow
// the query. Matching against the whole content (not line-by-line) lets a single
// matcher serve both single- and multi-line queries, and the match index maps
// back to a (line, column) that re-derives against the file (Law L5).
function compileQuery(query) {
  let src = query.pattern;
  if (!query.isRegExp) src = src.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  if (query.isWordMatch) src = `\\b(?:${src})\\b`;
  let flags = "g";
  if (!query.isCaseSensitive) flags += "i";
  if (query.isMultiline) flags += "m";
  // `s` (dotAll) only when the user explicitly writes a multiline regex; keep `.`
  // line-bounded otherwise so a plain `.*` does not swallow the file.
  return new RegExp(src, flags);
}

// Search `content` for `query`; return `[{ line, startCol, endCol, lineText }]`
// (0-based line/column), each a real occurrence at that position. `maxPerFile`
// bounds pathological matches. Empty patterns yield nothing.
function searchContent(content, query, maxPerFile = 10000) {
  if (!query.pattern) return [];
  let re;
  try { re = compileQuery(query); } catch { return []; }
  // Precompute line start offsets for O(log n) index → line mapping.
  const lineStarts = [0];
  for (let i = 0; i < content.length; i++) if (content[i] === "\n") lineStarts.push(i + 1);
  const lineOf = (idx) => {
    let lo = 0, hi = lineStarts.length - 1;
    while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (lineStarts[mid] <= idx) lo = mid; else hi = mid - 1; }
    return lo;
  };
  const endOfLine = (line) => (line + 1 < lineStarts.length ? lineStarts[line + 1] - 1 : content.length);
  const out = [];
  let m;
  let guard = 0;
  while ((m = re.exec(content)) !== null) {
    if (++guard > maxPerFile) break;
    const start = m.index;
    const end = m.index + m[0].length;
    const startLine = lineOf(start);
    const endLine = lineOf(end);
    const startCol = start - lineStarts[startLine];
    const endCol = end - lineStarts[endLine];
    const lineText = content.slice(lineStarts[startLine], endOfLine(startLine));
    out.push({ line: startLine, startCol, endLine, endCol, lineText });
    if (m.index === re.lastIndex) re.lastIndex++; // zero-width guard
  }
  return out;
}

const api = { globToRegExp, compileGlobs, buildIgnore, walk, compileQuery, searchContent };
// Dual export: `require`-able under Node (the unit witness) and assignable when
// fetched + evaluated in the browser ext host (`module.exports`).
if (typeof module !== "undefined" && module.exports) module.exports = api;
