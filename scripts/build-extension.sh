#!/usr/bin/env bash
#
# Build the holospaces egress extension into the **upload zip** for the Chrome
# Developer Console. The artifact contains ONLY the files Chrome loads at runtime
# — the page-side connector, the test, the build tooling, and the docs are NOT in
# it — so the package is exactly what the store expects, minimal and reviewable.
#
# Output: crates/holospaces-web/extension/dist/holospaces-egress-extension-v<ver>.zip
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT="$ROOT/crates/holospaces-web/extension"
DIST="$EXT/dist"

# The runtime files Chrome loads — and ONLY these.
RUNTIME=(manifest.json background.js content.js icons/icon16.png icons/icon48.png icons/icon128.png)

# Validate the manifest (MV3, minimal, referenced files declared) and read the version.
VERSION=$(python3 - "$EXT/manifest.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
assert m["manifest_version"] == 3, "must be Manifest V3"
assert m["background"]["service_worker"] == "background.js", "service worker must be background.js"
assert set(m["icons"].keys()) == {"16", "48", "128"}, "icons 16/48/128 required"
# Minimal-permission posture: a fast store review.
assert "host_permissions" not in m, "no host permissions (egress opens sockets, never fetch())"
assert not m.get("permissions"), "no broad permissions"
assert len(m.get("name", "")) <= 75 and len(m.get("description", "")) <= 132, "store name/description limits"
print(m["version"])
PY
)

# Every runtime file must exist (icons are committed source assets).
for f in "${RUNTIME[@]}"; do
    [ -f "$EXT/$f" ] || { echo "build-extension: missing runtime file $f" >&2; exit 1; }
done

mkdir -p "$DIST"
ZIP="$DIST/holospaces-egress-extension-v$VERSION.zip"
rm -f "$ZIP"
# -X strips extra file attributes for a reproducible, lean archive.
( cd "$EXT" && zip -q -X "$ZIP" "${RUNTIME[@]}" )

# Validate the artifact: it must contain EXACTLY the runtime files — no dev,
# test, page, or build files leaked in.
CONTENTS=$(unzip -Z1 "$ZIP" | sort | tr '\n' ' ')
EXPECTED=$(printf '%s\n' "${RUNTIME[@]}" | sort | tr '\n' ' ')
if [ "$CONTENTS" != "$EXPECTED" ]; then
    echo "build-extension: zip contents are not exactly the runtime files" >&2
    echo "  got:      $CONTENTS" >&2
    echo "  expected: $EXPECTED" >&2
    exit 1
fi

echo "build-extension: $ZIP"
echo "  ready to upload to the Chrome Developer Console (one zip, $(du -h "$ZIP" | cut -f1))."
