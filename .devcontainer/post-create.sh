#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

git_user_name="${HOLOSPACES_GIT_USER_NAME:-${HOST_GIT_AUTHOR_NAME:-${HOST_GIT_COMMITTER_NAME:-}}}"
git_user_email="${HOLOSPACES_GIT_USER_EMAIL:-${HOST_GIT_AUTHOR_EMAIL:-${HOST_GIT_COMMITTER_EMAIL:-}}}"

if [ -n "$git_user_name" ]; then
    git config --global user.name "$git_user_name"
fi

if [ -n "$git_user_email" ]; then
    git config --global user.email "$git_user_email"
fi

if ! command -v elan >/dev/null 2>&1; then
    curl https://elan.lean-lang.org/elan-init.sh -sSf \
        | sh -s -- -y --default-toolchain none
fi

if command -v docker >/dev/null 2>&1; then
    for _ in $(seq 1 60); do
        if docker info >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
fi

docs/scripts/install-tools.sh
