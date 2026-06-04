#!/usr/bin/env bash
#
# install-tools.sh
#
# Installs/downloads pinned external tools and writes resolved version
# strings to tools/versions.txt for later verification by build.sh Stage 1.
#
# Pin literals:
#   STRUCTURIZR_VERSION   the structurizr.war release tag (the Playwright
#                         variant is required for V3's SVG export)
#   CMARK_GFM_TAG         git tag in github/cmark-gfm to build from source
#   JDK_MAJOR             required major version of the JDK
#   RUBY_MAJOR            required major version of Ruby
#   DOCKER_MAJOR_MIN      minimum Docker Engine major version
#
# Idempotent: re-running re-validates the existing installs.

set -euo pipefail

readonly STRUCTURIZR_VERSION="2026.04.19"
readonly PANDOC_VERSION="3.9.0.2"
readonly CMARK_GFM_TAG="0.29.0.gfm.13"
readonly JDK_MAJOR="21"
readonly RUBY_MAJOR="3"
readonly DOCKER_MAJOR_MIN="20"

# Structurizr 2026.04.19 bundles Playwright Java 1.58.0. Use the matching
# Node package for browser/dependency provisioning so the cached Chromium
# revision is the one the Java exporter asks for.
readonly PLAYWRIGHT_NPM="playwright@1.58.0"

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly TOOLS_DIR="$REPO_ROOT/tools"
readonly VERSIONS_FILE="$TOOLS_DIR/versions.txt"
readonly CHECKSUMS_FILE="$TOOLS_DIR/checksums.txt"
readonly STRUCTURIZR_WAR="$TOOLS_DIR/structurizr.war"
readonly STRUCTURIZR_URL="https://download.structurizr.com/structurizr-${STRUCTURIZR_VERSION}-playwright.war"
case "$(dpkg --print-architecture)" in
    amd64) readonly PANDOC_ARCH="amd64" ;;
    arm64) readonly PANDOC_ARCH="arm64" ;;
    *) printf 'install-tools: ERROR: unsupported Pandoc package architecture: %s\n' "$(dpkg --print-architecture)" >&2; exit 1 ;;
esac
readonly PANDOC_DEB="$TOOLS_DIR/pandoc-${PANDOC_ARCH}.deb"
readonly PANDOC_URL="https://github.com/jgm/pandoc/releases/download/${PANDOC_VERSION}/pandoc-${PANDOC_VERSION}-1-${PANDOC_ARCH}.deb"
readonly PANDOC_PREFIX="$TOOLS_DIR/pandoc-prefix"
readonly CMARK_SRC_DIR="$TOOLS_DIR/cmark-gfm-src"
readonly CMARK_PREFIX="$TOOLS_DIR/cmark-prefix"
readonly TOOL_BIN_DIR="$TOOLS_DIR/bin"
readonly CMARK_BIN_DIR="$TOOL_BIN_DIR"
readonly CMARK_BIN="$CMARK_BIN_DIR/cmark-gfm"
readonly PANDOC_BIN="$TOOL_BIN_DIR/pandoc"
readonly PLAYWRIGHT_BROWSERS_DIR="$TOOLS_DIR/playwright-browsers"

err()  { printf 'install-tools: ERROR: %s\n' "$*" >&2; }
info() { printf 'install-tools: %s\n' "$*"; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || { err "required command not found: $1"; exit 1; }
}

mkdir -p "$TOOLS_DIR" "$TOOL_BIN_DIR"

# ---- 1. Verify host prerequisites ---------------------------------------

info "verifying host prerequisites"
require_cmd java
require_cmd ruby
require_cmd bundle
require_cmd docker
require_cmd cmake
require_cmd cc
require_cmd git
require_cmd curl
require_cmd sha256sum
require_cmd dpkg-deb
require_cmd sudo
require_cmd apt-get
require_cmd npx

JAVA_VERSION_LINE=$(java --version 2>&1 | head -1)
JAVA_MAJOR=$(printf '%s' "$JAVA_VERSION_LINE" | awk '{print $2}' | cut -d. -f1)
[ "$JAVA_MAJOR" = "$JDK_MAJOR" ] || {
    err "JDK major version pin is $JDK_MAJOR; java reports '$JAVA_VERSION_LINE'"
    exit 1
}

RUBY_VERSION_LINE=$(ruby --version 2>&1)
RUBY_MAJOR_ACTUAL=$(printf '%s' "$RUBY_VERSION_LINE" | awk '{print $2}' | cut -d. -f1)
[ "$RUBY_MAJOR_ACTUAL" = "$RUBY_MAJOR" ] || {
    err "Ruby major version pin is $RUBY_MAJOR; ruby reports '$RUBY_VERSION_LINE'"
    exit 1
}

DOCKER_VERSION_LINE=$(docker --version 2>&1)
DOCKER_MAJOR_ACTUAL=$(printf '%s' "$DOCKER_VERSION_LINE" | awk '{print $3}' | cut -d. -f1)
if [ "$DOCKER_MAJOR_ACTUAL" -lt "$DOCKER_MAJOR_MIN" ]; then
    err "Docker major version >= $DOCKER_MAJOR_MIN required; docker reports '$DOCKER_VERSION_LINE'"
    exit 1
fi
docker compose version >/dev/null 2>&1 || { err "docker compose v2 not available"; exit 1; }
DOCKER_COMPOSE_VERSION=$(docker compose version --short 2>&1)

# ---- 2. Bundle install (asciidoctor, github-markup, commonmarker) -------

info "installing bundled gems"
# Pin gems to vendor/bundle so a fresh clone doesn't pollute system gems
# and to keep the install reproducible from the committed Gemfile.lock.
( cd "$REPO_ROOT" \
    && bundle config set --local path vendor/bundle \
    && bundle install --quiet )

# ---- 3. Download structurizr.war and verify checksum --------------------

# Download a single binary if missing or if its line in checksums.txt no
# longer matches; verify the WHOLE checksums.txt at the end as the gate.
download_if_needed() {
    local url="$1" dest="$2" label="$3" approx_size="$4"
    local basename
    basename=$(basename "$dest")
    if [ -f "$dest" ] \
       && ( cd "$TOOLS_DIR" && sha256sum --quiet --check --ignore-missing checksums.txt ) >/dev/null 2>&1 \
       && grep -q "[[:space:]]$basename\$" "$CHECKSUMS_FILE" \
       && ( cd "$TOOLS_DIR" && grep "[[:space:]]$basename\$" checksums.txt | sha256sum --quiet --check ) >/dev/null 2>&1; then
        info "$label present and matches pinned checksum"
        return
    fi
    info "downloading $label ($approx_size)"
    # Bounded + retried so a slow/stalled mirror fails fast (and recovers from a
    # transient blip) instead of hanging the CI job indefinitely (no timeout =
    # a wedged connection stalls the whole V&V run for hours).
    curl --fail --silent --show-error --location \
        --connect-timeout 30 --max-time 1800 \
        --retry 3 --retry-delay 5 --retry-all-errors \
        --output "$dest" "$url"
    ( cd "$TOOLS_DIR" && grep "[[:space:]]$basename\$" checksums.txt | sha256sum --quiet --check ) || {
        err "$basename SHA-256 mismatch against tools/checksums.txt"
        rm -f "$dest"
        exit 1
    }
}

download_if_needed "$STRUCTURIZR_URL" "$STRUCTURIZR_WAR" "structurizr.war $STRUCTURIZR_VERSION" "~430 MB"
download_if_needed "$PANDOC_URL"      "$PANDOC_DEB"      "pandoc.deb $PANDOC_VERSION"        "~34 MB"
# `java -jar structurizr.war version` emits SLF4J log lines interleaved with
# the version. Extract only the structurizr application version line.
STRUCTURIZR_REPORTED=$(java -jar "$STRUCTURIZR_WAR" version 2>&1 \
    | grep -oE 'structurizr: [^[:space:]]+' \
    | head -1 \
    | sed 's/^structurizr: //' \
    | tr -d '\r')
[ -n "$STRUCTURIZR_REPORTED" ] || {
    err "could not extract version from 'java -jar structurizr.war version' output"
    exit 1
}

# ---- 3b. Extract pandoc.deb to a self-contained prefix -----------------
# Avoids dpkg -i (no system pollution, no sudo) by extracting the deb's
# data archive to tools/pandoc-prefix/ and symlinking the binary into
# tools/bin/. Same model as cmark-gfm.

PANDOC_OK=no
if [ -x "$PANDOC_BIN" ]; then
    if EXISTING_VER=$( "$PANDOC_BIN" --version 2>/dev/null | head -1 | awk '{print $2}' ); then
        if [ "$EXISTING_VER" = "$PANDOC_VERSION" ]; then
            info "pandoc $EXISTING_VER present"
            PANDOC_OK=yes
        else
            info "rebuilding pandoc: have $EXISTING_VER, want $PANDOC_VERSION"
        fi
    fi
fi
if [ "$PANDOC_OK" = "no" ]; then
    info "extracting pandoc $PANDOC_VERSION to $PANDOC_PREFIX"
    rm -rf "$PANDOC_PREFIX"
    mkdir -p "$PANDOC_PREFIX"
    dpkg-deb -x "$PANDOC_DEB" "$PANDOC_PREFIX"
    ln -sfn "$PANDOC_PREFIX/usr/bin/pandoc" "$PANDOC_BIN"
fi
PANDOC_REPORTED=$( "$PANDOC_BIN" --version 2>&1 | head -1 )

# ---- 4. Build cmark-gfm from source at pinned tag -----------------------

# Probe existing install: try `--version`. If it errors out (e.g., missing
# shared libraries from a stale build), treat as "not present" and rebuild.
CMARK_OK=no
if [ -x "$CMARK_BIN" ]; then
    if EXISTING_OUT=$( "$CMARK_BIN" --version 2>/dev/null ); then
        EXISTING_TAG=$(printf '%s' "$EXISTING_OUT" | head -1 | awk '{print $2}')
        if [ "$EXISTING_TAG" = "$CMARK_GFM_TAG" ]; then
            info "cmark-gfm $EXISTING_TAG present"
            CMARK_OK=yes
        else
            info "rebuilding cmark-gfm: have $EXISTING_TAG, want $CMARK_GFM_TAG"
        fi
    else
        info "rebuilding cmark-gfm: existing binary failed to run"
    fi
fi
if [ "$CMARK_OK" = "no" ]; then
    info "building cmark-gfm $CMARK_GFM_TAG from source"
    rm -rf "$CMARK_SRC_DIR" "$CMARK_PREFIX"
    rm -f "$CMARK_BIN"
    git clone --quiet --depth 1 --branch "$CMARK_GFM_TAG" \
        https://github.com/github/cmark-gfm.git "$CMARK_SRC_DIR"
    # RPATH '$ORIGIN/../lib' makes the installed binary find its libs in
    # cmark-prefix/lib relative to its own location, so the binary works
    # without any LD_LIBRARY_PATH setup.
    ( cd "$CMARK_SRC_DIR" \
        && mkdir -p build \
        && cd build \
        && cmake -DCMAKE_BUILD_TYPE=Release \
                 -DCMAKE_INSTALL_PREFIX="$CMARK_PREFIX" \
                 -DCMAKE_INSTALL_RPATH='$ORIGIN/../lib' \
                 -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
                 -DCMARK_TESTS=OFF \
                 -DCMARK_STATIC=OFF \
                 -DCMARK_LIB_FUZZER=OFF .. >/dev/null \
        && make --silent --jobs="$(nproc)" >/dev/null \
        && make --silent install >/dev/null )
    ln -sfn "$CMARK_PREFIX/bin/cmark-gfm" "$CMARK_BIN"
fi
CMARK_REPORTED=$( "$CMARK_BIN" --version 2>&1 | head -1 )

# ---- 5. Verify submodules -----------------------------------------------

info "ensuring arc42-generator submodule is initialized"
( cd "$REPO_ROOT" && git submodule update --init --recursive vendor/arc42-generator )

[ -d "$REPO_ROOT/vendor/arc42-generator/.git" ] \
    || [ -f "$REPO_ROOT/vendor/arc42-generator/.git" ] \
    || { err "vendor/arc42-generator submodule not initialized; run: git submodule update --init --recursive"; exit 1; }

# arc42-generator's own arc42-template submodule pin is the version it shipped
# with at the time of arc42-generator's own pin, and is too old for the
# current Templates.groovy layout expectations. We override it locally to a
# SHA we choose, recorded in tools/arc42-template-pin.txt — this gives V1
# and V2 a consistent authority that we control deterministically.
readonly ARC42_TEMPLATE_PIN_FILE="$TOOLS_DIR/arc42-template-pin.txt"
[ -f "$ARC42_TEMPLATE_PIN_FILE" ] || { err "$ARC42_TEMPLATE_PIN_FILE missing"; exit 1; }
ARC42_TEMPLATE_PIN=$(tr -d '[:space:]' < "$ARC42_TEMPLATE_PIN_FILE")
[ -n "$ARC42_TEMPLATE_PIN" ] || { err "$ARC42_TEMPLATE_PIN_FILE is empty"; exit 1; }

ARC42_TEMPLATE_DIR="$REPO_ROOT/vendor/arc42-generator/arc42-template"
[ -d "$ARC42_TEMPLATE_DIR" ] \
    || { err "$ARC42_TEMPLATE_DIR missing; run: git submodule update --init --recursive"; exit 1; }
CURRENT=$( cd "$ARC42_TEMPLATE_DIR" && git rev-parse HEAD )
if [ "$CURRENT" != "$ARC42_TEMPLATE_PIN" ]; then
    info "checking out arc42-template at pinned SHA $ARC42_TEMPLATE_PIN"
    # Retry around git index.lock contention. A leftover lock is almost
    # always stale — an earlier provisioning run (e.g. an interrupted
    # devcontainer rebuild) that died mid-checkout — and it persists in
    # .git/modules across rebuilds, so without clearing it every later run
    # fails here. Give any genuinely in-flight git op a moment to release the
    # lock, then clear the stale lock and retry.
    checkout_ok=
    for attempt in 1 2 3; do
        if ( cd "$ARC42_TEMPLATE_DIR" && git fetch --quiet origin && git checkout --quiet "$ARC42_TEMPLATE_PIN" ); then
            checkout_ok=1
            break
        fi
        info "arc42-template checkout failed (attempt $attempt); clearing any stale git lock and retrying"
        sleep 1
        ( cd "$ARC42_TEMPLATE_DIR" && rm -f "$(git rev-parse --git-path index.lock)" )
    done
    [ -n "$checkout_ok" ] \
        || { err "could not check out arc42-template at $ARC42_TEMPLATE_PIN (persistent git lock contention or fetch failure)"; exit 1; }
fi
[ -f "$ARC42_TEMPLATE_DIR/EN/adoc/01_introduction_and_goals.adoc" ] \
    || { err "$ARC42_TEMPLATE_DIR/EN/adoc/ layout not found at pinned SHA — pin may be too old"; exit 1; }

# ---- 6. Install Playwright's Chromium system libraries -----------------
# structurizr.war's SVG exporter launches a headless Chromium via its bundled
# Playwright *Java*, which downloads the browser binary itself at export time
# (docs/scripts/v3-structurizr.sh sets PLAYWRIGHT_BROWSERS_PATH for it). All we
# provision here are the system libraries that Chromium links against.
#
# We deliberately do NOT pre-download the browser binary with the Node CLI:
# `playwright install chromium` fetches Chromium to 100% and then wedges in CI
# on its post-download step (the ffmpeg fetch + unpack), hanging the V&V job for
# hours — the regression a recent devcontainer/tooling change introduced by
# adding that pre-download. The Java exporter fetches just the browser at
# runtime, as it always has, with no such hang.
#
# Install the pinned Playwright CLI once via a plain local `npm install` (npx
# -y playwright@VER re-resolves/re-fetches per call and itself wedges as the
# runner user), skipping the npm postinstall's implicit browser download, and
# run only `install-deps`.
PW_CLI_DIR="$TOOLS_DIR/playwright-cli"
info "installing the pinned Playwright CLI ($PLAYWRIGHT_NPM) into tools/playwright-cli"
mkdir -p "$PW_CLI_DIR"
( cd "$PW_CLI_DIR" \
    && { [ -f package.json ] || npm init -y >/dev/null 2>&1; } \
    && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 \
       npm install --no-fund --no-audit "$PLAYWRIGHT_NPM" >/dev/null 2>&1 ) \
    || { err "npm install $PLAYWRIGHT_NPM failed"; exit 1; }
PW_BIN="$PW_CLI_DIR/node_modules/.bin/playwright"
[ -x "$PW_BIN" ] || { err "Playwright CLI not found at $PW_BIN after install"; exit 1; }

info "ensuring Playwright Chromium system libraries are installed ($PW_BIN install-deps chromium)"
sudo env "PATH=$PATH" timeout 600 "$PW_BIN" install-deps chromium >/dev/null 2>&1 \
    || { err "playwright install-deps chromium failed"; exit 1; }

# ---- 6b. Install Graphviz for tools/opl-to-svg.rb ------------------------
# Graphviz's `dot` is the renderer behind tools/opl-to-svg.rb, the
# bootstrap helper that produces an OPM-style SVG from an OPL file.
# It is a developer aid, not a build dependency: opd.svg is committed
# as a source artifact, so day-to-day `build.sh` runs do not invoke
# graphviz. We install it here so `opl-to-svg.rb` is available
# immediately after install-tools.sh.

if ! command -v dot >/dev/null 2>&1; then
    info "installing graphviz (sudo apt-get install -y graphviz)"
    # Bound the dpkg-lock wait (a runner's unattended-upgrades can hold it) so
    # this fails fast rather than blocking the V&V job indefinitely.
    sudo apt-get -o DPkg::Lock::Timeout=300 update -qq >/dev/null 2>&1 || true
    sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300 install -y graphviz >/dev/null 2>&1 \
        || { err "apt-get install graphviz failed"; exit 1; }
fi
GRAPHVIZ_REPORTED=$(dot -V 2>&1 | head -1)

# ---- 7. Write tools/versions.txt ----------------------------------------

info "writing $VERSIONS_FILE"
{
    printf 'java\t%s\n'              "$JAVA_VERSION_LINE"
    printf 'ruby\t%s\n'              "$RUBY_VERSION_LINE"
    printf 'bundler\t%s\n'           "$(bundle --version 2>&1)"
    printf 'docker\t%s\n'            "$DOCKER_VERSION_LINE"
    printf 'docker-compose\t%s\n'    "$DOCKER_COMPOSE_VERSION"
    printf 'structurizr.war\t%s\n'   "$STRUCTURIZR_VERSION"
    printf 'structurizr-reported\t%s\n' "$STRUCTURIZR_REPORTED"
    printf 'cmark-gfm\t%s\n'         "$CMARK_GFM_TAG"
    printf 'cmark-gfm-reported\t%s\n' "$CMARK_REPORTED"
    printf 'pandoc\t%s\n'            "$PANDOC_VERSION"
    printf 'pandoc-reported\t%s\n'   "$PANDOC_REPORTED"
    printf 'arc42-template\t%s\n'    "$ARC42_TEMPLATE_PIN"
    printf 'graphviz-reported\t%s\n' "$GRAPHVIZ_REPORTED"
} > "$VERSIONS_FILE"

info "done"
