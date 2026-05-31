#!/usr/bin/env bash
# Regenerate the CC-11 interactive kernel from its pinned sources. Requires
# gcc-riscv64-linux-gnu, cpio, and the kernel build deps. See SOURCE.txt.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; cd "$work"
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux
mkdir -p root/dev root/proc
riscv64-linux-gnu-gcc -O2 -static -nostdlib -ffreestanding -march=rv64gc -mabi=lp64d \
  "$here/shell.c" -o root/init
cp "$here/initramfs.list" initramfs.list
sed -i "s#/tmp/cc11/initramfs/init#$work/root/init#" initramfs.list
cp "$here/../cc9/linux/kernel.config" linux/.config
( cd linux
  export ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu-
  ./scripts/config --set-str INITRAMFS_SOURCE "$work/initramfs.list"
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc11
  make olddefconfig
  make -j"$(nproc)" Image )
gzip -9 -c linux/arch/riscv/boot/Image > Image.gz
echo "rebuilt Image.gz in $work"
