#!/usr/bin/env bash
# Regenerate the CC-9 Linux boot artifacts from their pinned sources. Requires
# gcc-riscv64-linux-gnu, device-tree-compiler, cpio, and the kernel build deps
# (bc bison flex libssl-dev libelf-dev). See SOURCE.txt for the pins.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; cd "$work"
rev="$(grep -m1 'Commit' "$here/SOURCE.txt" | awk '{print $3}')"
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux
( cd linux && [ "$(git rev-parse HEAD)" = "$rev" ] || echo "WARN: tag v6.6 != pinned $rev" )
# init + initramfs
mkdir -p root/dev root/proc
riscv64-linux-gnu-gcc -O2 -static -nostdlib -ffreestanding -march=rv64gc -mabi=lp64d \
  "$here/init.c" -o root/init
cp "$here/initramfs.list" initramfs.list
sed -i "s#/tmp/linuxboot/initramfs/init#$work/root/init#" initramfs.list
# dtb
dtc -I dts -O dtb -o holospaces.dtb "$here/holospaces.dts"
# kernel
cp "$here/kernel.config" linux/.config
( cd linux
  export ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu-
  ./scripts/config --set-str INITRAMFS_SOURCE "$work/initramfs.list"
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc9
  make olddefconfig
  make -j"$(nproc)" Image )
gzip -9 -c linux/arch/riscv/boot/Image > Image.gz
echo "rebuilt in $work: Image.gz $(stat -c%s Image.gz) bytes"
