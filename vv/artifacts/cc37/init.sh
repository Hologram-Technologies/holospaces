#!/bin/busybox sh
# CC-37 — the arm64 devcontainer's PID 1: the ecosystem's **stock linux-arm64
# busybox** (glibc, Advanced-SIMD ifunc string routines and all). It brings up
# the pseudo-filesystems, installs the busybox applet symlinks, and then runs a
# series of **unmodified linux-arm64 applets** over fork+exec and command
# substitution — the proof that arbitrary linux-arm64 programs run in the OS:
#   • `uname -m`          → the guest architecture (aarch64), via a forked applet;
#   • a real shell computation (sum 1..1000 == 500500);
#   • `head` reads the real /proc/version (the running kernel).
# Finally it powers off (busybox `poweroff` → reboot syscall → PSCI SYSTEM_OFF →
# the emulator halts). No riscv64 workaround, no freestanding shim — the stock
# glibc binary itself, running its NEON memcpy/strlen, forking children, and
# faulting copy-on-write pages, all on the holospaces emulator.
/bin/busybox mkdir -p /proc /sys /dev /bin /sbin /usr/bin /usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null
/bin/busybox --install -s
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
echo "CC37-DEVCONTAINER-UP"
echo "CC37-ARCH:$(uname -m)"
s=0
i=1
while [ "$i" -le 1000 ]; do
	s=$((s + i))
	i=$((i + 1))
done
echo "CC37-COMPUTE:$s"
head -c 60 /proc/version
echo
poweroff -f
