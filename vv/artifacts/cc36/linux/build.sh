#!/usr/bin/env bash
# Regenerate the CC-36 arm64 Linux boot artifact from its pinned sources. Mirrors
# the CC-9 RISC-V recipe (vv/artifacts/cc9/linux/build.sh) for AArch64. Requires
# gcc-aarch64-linux-gnu, cpio, and the kernel build deps (bc bison flex
# libssl-dev libelf-dev). See SOURCE.txt for the pins.
#
# The config is `make defconfig` (the arm64 default that boots the `virt`
# machine) with the freestanding initramfs spliced in — robust and reproducible,
# rather than a hand-trimmed config. The output Image is the flat arm64 kernel
# (a `MZ`/arm64 boot header) the emulator loads at its text_offset.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; cd "$work"
rev="$(grep -m1 'Commit' "$here/SOURCE.txt" | awk '{print $3}')"
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux
( cd linux && [ "$(git rev-parse HEAD)" = "$rev" ] || echo "WARN: tag v6.6 != pinned $rev" )

# init + initramfs
mkdir -p root/dev root/proc
aarch64-linux-gnu-gcc -O2 -static -nostdlib -ffreestanding \
    "$here/init.c" -o root/init
cp "$here/initramfs.list" initramfs.list
sed -i "s#/tmp/linuxboot/initramfs/init#$work/root/init#" initramfs.list

# kernel
( cd linux
  export ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
  make defconfig
  # The CC-36 boot witness: a freestanding initramfs PID-1, a virtio-blk/9p/net
  # root option (shared with CC-37), ext4, and a deterministic build.
  ./scripts/config \
      --set-str INITRAMFS_SOURCE "$work/initramfs.list" \
      --enable  BLK_DEV_INITRD \
      --enable  VIRTIO --enable VIRTIO_MMIO --enable VIRTIO_BLK \
      --enable  VIRTIO_NET --enable NET_9P --enable NET_9P_VIRTIO --enable 9P_FS \
      --enable  EXT4_FS --enable IP_PNP --enable IP_PNP_DHCP
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" \
         KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc36
  make olddefconfig
  make -j"$(nproc)" Image )

gzip -9 -c linux/arch/arm64/boot/Image > "$here/Image.gz"
cp linux/.config "$here/kernel.config"
( cd "$here" && sha256sum Image.gz init.c initramfs.list kernel.config > linux.sha256 )
echo "rebuilt: $here/Image.gz $(stat -c%s "$here/Image.gz") bytes"
