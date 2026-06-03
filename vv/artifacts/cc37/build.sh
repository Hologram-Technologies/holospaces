set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CC37="$ROOT/vv/artifacts/cc37"
work="$(mktemp -d)"; cd "$work"
echo "== [1/3] cc37 arm64 kernel (no initramfs, virtio-blk root) =="
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux >/dev/null 2>&1
( cd linux
  export ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
  make defconfig >/dev/null
  ./scripts/config --set-str INITRAMFS_SOURCE "" \
      --enable VIRTIO --enable VIRTIO_MMIO --enable VIRTIO_BLK --enable EXT4_FS \
      --enable VIRTIO_NET --enable NET_9P --enable NET_9P_VIRTIO --enable 9P_FS \
      --enable IP_PNP --enable IP_PNP_DHCP --disable CMDLINE_FORCE
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc37
  make olddefconfig >/dev/null
  make -j"$(nproc)" Image >/dev/null 2>&1 )
gzip -9 -c linux/arch/arm64/boot/Image > "$CC37/linux/Image.gz"
cp linux/.config "$CC37/linux/kernel.config"
echo "kernel: $(stat -c%s "$CC37/linux/Image.gz") bytes"

echo "== [2/3] static busybox arm64 (the stock binary) =="
BBVER=1.36.1
wget -q "https://busybox.net/downloads/busybox-$BBVER.tar.bz2"
tar xf "busybox-$BBVER.tar.bz2"
( cd "busybox-$BBVER"
  make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig >/dev/null
  sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
  # Disprefer features needing extra libs to keep the static link clean.
  sed -i 's/^CONFIG_TC=y/# CONFIG_TC is not set/' .config || true
  yes "" | make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- oldconfig >/dev/null 2>&1
  make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j"$(nproc)" >/dev/null 2>&1
  aarch64-linux-gnu-strip busybox )
cp "busybox-$BBVER/busybox" "$CC37/rootfs/busybox"
echo "busybox: $(stat -c%s "$CC37/rootfs/busybox") bytes  ($(file -b "$CC37/rootfs/busybox" | cut -d, -f1-2))"

echo "== [3/3] OCI image layer (tar+gzip of /bin/busybox) =="
ldir="$(mktemp -d)"; mkdir -p "$ldir/bin"
cp "$CC37/rootfs/busybox" "$ldir/bin/busybox"
( cd "$ldir" && tar --numeric-owner --owner=0 --group=0 --mtime='2026-05-31 00:00:00 UTC' --sort=name -cf - bin ) | gzip -9 -n > "$CC37/rootfs/layer.tar.gz"
( cd "$CC37" && sha256sum linux/Image.gz linux/kernel.config rootfs/busybox rootfs/layer.tar.gz > cc37.sha256 )
echo "DONE"
