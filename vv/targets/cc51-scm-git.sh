#!/usr/bin/env bash
#
# CC-51 (TARGET) — The workbench's Source Control view reflects, commits, and
#                  pushes the REAL repository over the holospace's own primitives
#
# OPM process: SD4 Working (the projection drives the operator's repository).
# Today `holospace-fs` registers NO `SourceControl` provider (the `scm` count is
# 0): the editor shares the workspace over 9p (CC-15), git runs in the guest
# (CC-16/CC-20), holospaces is the remote over the bridge (CC-33/CC-34) — but the
# Source Control view is empty. This adds a builtin SCM provider (`holospace-scm`)
# that runs a real Git engine as NATIVE exec on the browser peer (the CC-48
# discipline — heavy in-tab work is native peer exec, NEVER the emulated guest)
# over the holospace's OWN virtio-9p workspace (CC-15) — the SAME `.git` content
# the guest's `git` reads (one content, Law L1). Status, quick-diff vs HEAD,
# stage/unstage, commit, branch, and push/pull over CC-16 all work, and a commit
# made in the UI is a real Git commit in the repository's `.git` — never a server
# outside the holospace (Law L4).
#
# Authority: the VS Code SourceControl (`scm`) API + the Git on-disk object and
#   pack-protocol formats (the authority a commit's bytes re-derive against,
#   Law L5), with isomorphic-git as the spec-conformant Git engine.
# Witnesses:
#   • host       crates/holospaces/tests/cc51_nested_workspace.rs — the host
#     writes a nested `.git/objects/...` tree the guest reads back byte-identically
#     over virtio-9p (CC-15 nested-path parity with the guest's Twalk/Tcreate);
#   • deployed   crates/holospaces-web/web/scm-git-test.mjs — in Chromium the
#     Source Control view shows a real repo's status, an edit shows as modified
#     with an accurate diff vs HEAD, a commit via the SCM input is a real Git
#     commit that re-derives per the object-format spec and appears in the log,
#     and a push reaches a real remote.
#
# GREEN when: a commit made in the SCM UI is a real Git commit (re-derives to the
#   canonical Git object) in the workspace's `.git` the guest shares, status/diffs
#   are accurate vs HEAD, and push/pull round-trip against a real remote.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
EMU="$ROOT/crates/holospaces/src/emulator.rs"
SCM_EXT="$ROOT/crates/holospaces-web/web/builtin-extensions/holospace-scm/extension.js"
HOST_WITNESS="$ROOT/crates/holospaces/tests/cc51_nested_workspace.rs"
WEB_WITNESS="$ROOT/crates/holospaces-web/web/scm-git-test.mjs"

# Liveness probe: the substrate has the nested-path 9p workspace API AND the
# holospace-scm builtin SCM provider exists AND both witnesses are present.
if grep -qE 'fn workspace_write_path' "$EMU" 2>/dev/null \
   && [ -f "$SCM_EXT" ] && [ -f "$HOST_WITNESS" ] && [ -f "$WEB_WITNESS" ]; then
    rc=0
    if command -v cargo >/dev/null 2>&1; then
        cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
            --test cc51_nested_workspace -- --ignored --nocapture || rc=1
    else
        echo "cc51-scm-git: SKIP host witness — cargo absent"
    fi
    if command -v node >/dev/null 2>&1; then
        ( cd "$ROOT/crates/holospaces-web/web" && node scm-git-test.mjs ) || rc=1
    else
        echo "cc51-scm-git: SKIP deployed witness — node absent"
    fi
    exit "$rc"
fi

echo "cc51-scm-git: RED — TARGET not yet live."
echo "  needed: (1) a nested-path 9p workspace API on the host side (CC-15 parity"
echo "              with the guest Twalk/Tcreate) so the .git object tree is the"
echo "              one shared content (Law L1);"
echo "          (2) a builtin SCM provider holospace-scm running a real Git engine"
echo "              as native browser-peer exec over that 9p workspace (CC-48"
echo "              discipline), registering a vscode.scm SourceControl;"
echo "          (3) witnesses cc51_nested_workspace.rs + scm-git-test.mjs."
echo "  spec:   a commit made in the SCM UI is a real Git commit (re-derives to the"
echo "          canonical Git object, Law L5) in the workspace .git; status and"
echo "          diffs are accurate vs HEAD; push/pull round-trip against a remote."
exit 1
