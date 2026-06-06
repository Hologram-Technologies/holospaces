#!/usr/bin/env bash
# Regenerate the CC-44 amd64 (x86-64) Linux boot artifact from its pinned
# sources. The x86-64 realization of the CC-36 arm64 recipe
# (vv/artifacts/cc36/linux/build.sh). Requires the x86_64 kernel build deps
# (gcc, bc bison flex libssl-dev libelf-dev) and cpio. See SOURCE.txt for the
# pins.
#
# The config is `make x86_64_defconfig` (the amd64 default) with the freestanding
# initramfs spliced in + virtio-mmio/virtio-blk/9p/ext4 — robust and reproducible
# rather than a hand-trimmed config. Two outputs are committed:
#   • vmlinux.gz  — the gzip-compressed, *uncompressed* ELF kernel. The
#                   holospaces x86-64 core loads its PT_LOAD segments and enters
#                   `startup_64` directly (the 64-bit boot protocol, paging +
#                   GDT + boot_params set up by the loader) — no real-mode setup,
#                   no in-guest decompressor. This is the image the emulator
#                   witness boots.
#   • bzImage     — the standard self-decompressing kernel, the image
#                   qemu-system-x86_64 -kernel boots for the differential oracle.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; cd "$work"
rev="$(grep -m1 'Commit' "$here/SOURCE.txt" | awk '{print $3}')"
git clone --depth 1 --branch v6.6 https://github.com/torvalds/linux.git linux
( cd linux && [ "$(git rev-parse HEAD)" = "$rev" ] || echo "WARN: tag v6.6 != pinned $rev" )

# init + initramfs (a freestanding static amd64 PID 1, raw syscalls, no libc).
mkdir -p root/dev root/proc
gcc -O2 -static -nostdlib -ffreestanding -no-pie \
    "$here/init.c" -o root/init
cp "$here/initramfs.list" initramfs.list
sed -i "s#/tmp/linuxboot/initramfs/init#$work/root/init#" initramfs.list

# kernel
( cd linux
  export ARCH=x86_64
  make x86_64_defconfig
  # The CC-44 boot witness: a freestanding initramfs PID-1, a virtio-mmio
  # blk/9p/net stack (the shared device bus), ext4, and a deterministic build.
  ./scripts/config \
      --set-str INITRAMFS_SOURCE "$work/initramfs.list" \
      --enable  BLK_DEV_INITRD \
      --enable  VIRTIO --enable VIRTIO_MMIO \
      --enable  VIRTIO_MMIO_CMDLINE_DEVICES --enable VIRTIO_BLK \
      --enable  VIRTIO_NET --enable NET_9P --enable NET_9P_VIRTIO --enable 9P_FS \
      --enable  EXT4_FS --enable IP_PNP --enable IP_PNP_DHCP \
      --enable  SERIAL_8250 --enable SERIAL_8250_CONSOLE
  # The holospaces x86-64 core implements *4-level* paging (no LA57/5-level
  # walk; CPUID.7.0:ECX[16] LA57 is not advertised, CR4.LA57 stays off). A
  # 4-level kernel is a completely standard x86-64 Linux — disabling 5-level
  # makes the kernel and the emulator AGREE on the paging mode, so the
  # vmemmap/page-table arithmetic (vmalloc_to_page, __text_poke's alias) is
  # computed for 4 levels and matches the MMU. KASLR stays ON (it is correct
  # under 4-level paging too).
  ./scripts/config --disable FTRACE --disable FUNCTION_TRACER
  ./scripts/config --disable X86_5LEVEL
  echo 0 > .version
  export KBUILD_BUILD_TIMESTAMP="Fri 31 May 2026 00:00:00 UTC" \
         KBUILD_BUILD_USER=holospaces KBUILD_BUILD_HOST=cc44
  make olddefconfig
  make -j"$(nproc)" bzImage )

gzip -9 -c linux/vmlinux                  > "$here/vmlinux.gz"
cp linux/arch/x86/boot/bzImage             "$here/bzImage"
cp linux/.config                           "$here/kernel.config"
( cd "$here" && sha256sum vmlinux.gz bzImage init.c initramfs.list kernel.config > linux.sha256 )
echo "rebuilt: $here/vmlinux.gz $(stat -c%s "$here/vmlinux.gz") bytes; $here/bzImage $(stat -c%s "$here/bzImage") bytes"
