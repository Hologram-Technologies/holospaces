set -euo pipefail
# CC-46 — arm64 real-kernel devbus-parity fixtures (reproduce).
#
# The kernel and the stock linux-arm64 busybox are IDENTICAL to CC-37's (same
# torvalds/linux v6.6 config — it already enables VIRTIO + VIRTIO_MMIO +
# VIRTIO_BLK + EXT4 + VIRTIO_NET + NET_9P + NET_9P_VIRTIO + 9P_FS + IP_PNP_DHCP,
# exactly the devbus complement CC-46 exercises), so this recipe extends CC-37:
# it rebuilds the kernel + busybox the same way and copies the result here. Only
# the /init differs — it mounts the 9p workspace, fetches over the NAT, and
# serves a bridge listener (the CC-15/CC-16/CC-33 round-trips).
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CC46="$ROOT/vv/artifacts/cc46"
CC37="$ROOT/vv/artifacts/cc37"

echo "== [1/2] kernel + stock linux-arm64 busybox (identical to CC-37) =="
# Rebuild CC-37's artifacts (clones v6.6, builds the kernel + busybox, packs the
# layer) into the CC-37 tree, then mirror them here. Keeping a single kernel +
# busybox source avoids drift; the devbus config is already in CC-37's recipe.
bash "$CC37/build.sh"
mkdir -p "$CC46/linux" "$CC46/rootfs"
cp "$CC37/linux/Image.gz"        "$CC46/linux/Image.gz"
cp "$CC37/linux/kernel.config"   "$CC46/linux/kernel.config"
cp "$CC37/rootfs/busybox"        "$CC46/rootfs/busybox"
cp "$CC37/rootfs/layer.tar.gz"   "$CC46/rootfs/layer.tar.gz"
echo "kernel: $(stat -c%s "$CC46/linux/Image.gz") bytes; busybox: $(stat -c%s "$CC46/rootfs/busybox") bytes"

echo "== [2/2] checksums =="
( cd "$CC46" && sha256sum linux/Image.gz linux/kernel.config rootfs/busybox rootfs/layer.tar.gz init.sh > cc46.sha256 )
echo "DONE"
