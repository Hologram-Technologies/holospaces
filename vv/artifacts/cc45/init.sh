#!/bin/busybox sh
# CC-45 — the amd64 (x86-64) devcontainer's PID 1: the ecosystem's **stock
# linux-amd64 busybox** (glibc, SSE2 ifunc string routines and all). It brings up
# /proc and then runs a series of **unmodified linux-amd64 applets** over fork+exec
# and command substitution — the proof that arbitrary linux-amd64 programs run in
# the OS:
#   • `uname -m`          → the guest architecture (x86_64), via a forked applet;
#   • a real shell computation (sum 1..1000 == 500500);
#   • `head` reads the real /proc/version (the running kernel).
# Finally it powers off (busybox `poweroff` → reboot syscall → native_machine_halt
# → `hlt` with interrupts masked → the emulator halts). No riscv64/arm64 workaround,
# no freestanding shim — the stock glibc binary itself, running its SSE2 memcpy/
# strlen, forking children, and faulting copy-on-write pages, all on the holospaces
# x86-64 emulator. (Applets are invoked as `/bin/busybox <applet>` rather than via
# the `--install` symlink farm so the witness exercises real fork+exec of the stock
# binary without the ~400-applet install grind — the same markers, faster.)
/bin/busybox mkdir -p /proc /sys /dev
/bin/busybox mount -t proc proc /proc
echo "CC45-DEVCONTAINER-UP"
echo "CC45-ARCH:$(/bin/busybox uname -m)"
s=0
i=1
while [ "$i" -le 1000 ]; do
	s=$((s + i))
	i=$((i + 1))
done
echo "CC45-COMPUTE:$s"
/bin/busybox head -c 60 /proc/version
echo
/bin/busybox poweroff -f
