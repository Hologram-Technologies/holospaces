#!/bin/sh
# Dev Container Feature install script. Receives the declared options as
# environment variables (uppercased option ids), per the Dev Container Features
# spec. Runs in the devcontainer OS during the build phase, before the lifecycle.
set -e
echo "FEATURE-INSTALLED:${VERSION}"
mkdir -p /usr/local/holospace-demo
echo "${VERSION}" > /usr/local/holospace-demo/version
echo "feature holospace-demo ${VERSION} installed" > /usr/local/holospace-demo/README
