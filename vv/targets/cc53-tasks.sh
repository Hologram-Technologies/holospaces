#!/usr/bin/env bash
#
# CC-53 (TARGET) — The workbench runs `tasks.json` tasks in the devcontainer
#                  (output, exit status, problem matchers, background tasks)
#
# OPM process: SD4 Working. `holospace-fs` registers no task provider, and the
# web workbench DISABLES shell/process task execution in a virtual workspace (the
# `virtualWorkspace` context gates `shellExecutionSupported`/`processExecutionSupported`
# off), so `tasks.json` is dead. This adds a builtin `holospace-tasks` that parses
# `.vscode/tasks.json` (over 9p, CC-15) and registers a TaskProvider whose tasks
# run as CustomExecution: a Pseudoterminal runs each task's command IN THE GUEST
# devcontainer (CC-11) over a file-exec channel on the 9p workspace — a tiny guest
# task-runner agent (a /bin/sh loop seeded into the devcontainer /init) runs the
# command and streams its output + exit code back over 9p (CC-15/CC-11;
# image-agnostic — only sh + the share), never a server outside the holospace
# (Law L4). Output + exit surface in the task terminal; the task's problem
# matchers populate the Problems panel; background tasks run non-blocking.
#
# Authority: the VS Code Task provider API + the tasks.json schema, with the
#   task command's real execution in the guest (its exit code + output) as the
#   authority a witnessed run re-derives against.
# Witnesses:
#   • core     builtin-extensions/holospace-tasks/tasks-core.test.cjs — the
#     tasks.json (JSONC) parse + the file-exec request/stream/exit protocol,
#     verified deterministically under Node;
#   • deployed crates/holospaces-web/web/tasks-test.mjs — in Chromium Run Task
#     runs a declared task in the guest, its output + non-zero exit status
#     surface, and a problem-matcher task creates a diagnostic in the Problems
#     panel.
#
# GREEN when: a tasks.json task runs in the devcontainer (its output + exit
#   status surface), a problem matcher produces a diagnostic, and a background
#   task runs without blocking — witnessed in the real deployed workbench.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TASKS_EXT="$ROOT/crates/holospaces-web/web/builtin-extensions/holospace-tasks/extension.js"
WEB_WITNESS="$ROOT/crates/holospaces-web/web/tasks-test.mjs"

# Liveness probe: the holospace-tasks builtin + the guest task-runner agent (in
# the devcontainer init) + the witness all exist.
if [ -f "$TASKS_EXT" ] && [ -f "$WEB_WITNESS" ] \
   && grep -q "hs-tasks" "$ROOT/crates/holospaces/src/machine.rs" 2>/dev/null; then
    command -v node >/dev/null 2>&1 || { echo "cc53-tasks: SKIP — node absent"; exit 127; }
    ( cd "$ROOT/crates/holospaces-web/web/builtin-extensions/holospace-tasks" && node tasks-core.test.cjs ) || exit 1
    command -v wasm-pack >/dev/null 2>&1 || { echo "cc53-tasks: SKIP deployed witness — wasm-pack absent"; exit 127; }
    if [ ! -f "$ROOT/crates/holospaces-web/web/pkg/holospaces_web_bg.wasm" ]; then
        "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
    fi
    ( cd "$ROOT/crates/holospaces-web/web" && node tasks-test.mjs ) || exit 1
    exit 0
fi

echo "cc53-tasks: RED — TARGET not yet live."
echo "  needed: a guest task-runner agent in the devcontainer /init (machine.rs),"
echo "          and a builtin holospace-tasks that parses .vscode/tasks.json (CC-15)"
echo "          and registers a TaskProvider whose CustomExecution Pseudoterminal"
echo "          runs each task in the guest over the 9p file-exec channel,"
echo "          capturing output + exit; problem matchers → Problems; background OK."
echo "  spec:   a tasks.json task runs in the devcontainer (output + exit surface),"
echo "          a problem matcher creates a diagnostic, a background task is"
echo "          non-blocking — witnessed in the real workbench (tasks-test.mjs)."
exit 1
