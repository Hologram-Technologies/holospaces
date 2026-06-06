#!/usr/bin/env bash
# Fetch + verify the imported Intel SDM volumes (the x86-64 architectural
# authority). Like the Structurizr import (PROVENANCE.md), the copyrighted PDF is
# NOT redistributed in-tree: it is fetched from Intel's versioned CDN and verified
# against the committed sha256 pin. Run from this directory.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# Volume 3A — System Programming Guide, Part 1 (doc 253668): paging, the local
# APIC + APIC timer, interrupt/exception delivery, the 8259-via-LINT0 virtual-wire
# mode — the privileged behavior the holospaces x86-64 core conforms to.
url_vol3a="https://cdrdv2-public.intel.com/812386/253668-sdm-vol-3a.pdf"
curl -sSL --max-time 180 -o "$here/253668-sdm-vol-3a.pdf" "$url_vol3a"
( cd "$here" && sha256sum -c intel-sdm.sha256 )
echo "Intel SDM Vol 3A verified against the pin."
