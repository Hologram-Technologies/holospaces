#!/bin/busybox sh
# CC-37 — the arm64 devcontainer's PID 1. Boots from the κ-disk (virtio-blk)
# rootfs and runs the ecosystem's **stock `linux-arm64` busybox** binary: it
# brings up the pseudo-filesystems, installs busybox's applets, reports the
# guest architecture (`uname -m` → aarch64 — the stock binary executing), reads
# the real /proc/version, runs a real busybox computation, and powers off. A
# deterministic witness that an unmodified `linux-arm64` binary runs on the
# emulator over the shared virtio devices (no riscv64 workaround).
/bin/busybox mkdir -p /proc /sys /dev /bin /sbin /usr/bin /usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null
/bin/busybox --install -s
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
echo "CC37-DEVCONTAINER-UP"
echo "CC37-ARCH:$(uname -m)"
/bin/busybox head -c 60 /proc/version; echo
echo "CC37-BUSYBOX-OK:$(echo holospaces | wc -c)"
/bin/busybox poweroff -f
