#!/usr/bin/env bash
#
# CC-53 (LIVE) — The workbench runs `tasks.json` tasks in the devcontainer
#                (output, exit status, problem matchers, background tasks)
#
# OPM process: SD4 Working. The builtin `holospace-tasks` parses `.vscode/tasks.json`
# (over 9p, CC-15) and registers a TaskProvider whose tasks run as CustomExecution:
# a Pseudoterminal runs each task's command IN THE GUEST devcontainer (CC-11) over
# a file-exec channel on the 9p workspace — a tiny guest task-runner agent (a
# /bin/sh loop seeded into the devcontainer /init) runs the command and streams its
# output + exit code back over 9p (image-agnostic — only sh + the share). The web
# workbench DISABLES shell/process task execution in a virtual workspace, so this
# CustomExecution provider is the only path tasks.json runs here. No server outside
# the holospace (Law L4).
#
# Authority: the VS Code Task provider API + the tasks.json schema, with the task
#   command's real execution in the guest (its exit code + output) as the authority.
# Witnesses:
#   • core     builtin-extensions/holospace-tasks/tasks-core.test.cjs — the
#     tasks.json (JSONC) parse + command build + the file-exec request/stream/exit
#     protocol, verified deterministically under Node;
#   • deployed crates/holospaces-web/web/tasks-test.mjs — in Chromium Run Task
#     runs a declared task in the guest, its output + non-zero exit status surface,
#     and a problem-matcher task creates a diagnostic in the Problems panel.
#
# GREEN when both pass: the tasks engine is correct (core) AND tasks.json tasks
#   run in the devcontainer with output/exit + problem matchers in the real
#   deployed workbench (deployed).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"
TASKS="$WEB/builtin-extensions/holospace-tasks"

command -v node >/dev/null 2>&1 || { echo "cc53-tasks: SKIP — node absent"; exit 127; }

# (1) Core witness — the tasks engine, fast + deterministic (no browser).
( cd "$TASKS" && node tasks-core.test.cjs ) || exit 1

# (2) Deployed witness — tasks.json tasks run in the devcontainer (Chromium). The
# guest task-runner agent the witness drives is compiled into the wasm peer's
# devcontainer /init (machine.rs), so the peer must be built from current source.
command -v wasm-pack >/dev/null 2>&1 || { echo "cc53-tasks: SKIP deployed witness — wasm-pack absent"; exit 127; }
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
    "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
fi
( cd "$WEB" && node tasks-test.mjs ) || exit 1
exit 0
