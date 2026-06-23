#!/bin/busybox sh
# CC-45 — the amd64 (x86-64) devcontainer's PID 1: the ecosystem's **stock
# linux-amd64 busybox** (glibc, SSE/AVX ifunc string routines and all). It brings
# up the pseudo-filesystems, installs the busybox applet symlinks, then runs a
# series of **unmodified linux-amd64 applets** over fork+exec and command
# substitution — the proof that arbitrary linux-amd64 programs run in the OS:
#   • `uname -m`          → the guest architecture (x86_64), via a forked applet;
#   • a real shell computation (sum 1..1000 == 500500);
#   • `head` reads the real /proc/version (the running kernel).
# Finally it powers off (busybox `poweroff` → reboot syscall → native_machine_halt
# → `hlt` with interrupts masked → the emulator halts). No riscv64/arm64 workaround,
# no freestanding shim — the stock glibc binary itself, running its SSE memcpy/strlen,
# forking children, and faulting copy-on-write pages, all on the holospaces x86-64
# emulator. The differential oracle is qemu-system-x86_64 on the same kernel + rootfs.
/bin/busybox mkdir -p /proc /sys /dev /bin /sbin /usr/bin /usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null
/bin/busybox --install -s
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
echo "CC45-DEVCONTAINER-UP"
echo "CC45-ARCH:$(uname -m)"
s=0
i=1
while [ "$i" -le 1000 ]; do
	s=$((s + i))
	i=$((i + 1))
done
echo "CC45-COMPUTE:$s"
head -c 60 /proc/version
echo
poweroff -f
