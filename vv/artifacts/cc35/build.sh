#!/usr/bin/env bash
#
# Reproducibly assemble the CC-35 A64 integer batteries from their `.s` sources
# into (a) flat `.bin` images — the holospaces emulator witness loads these at
# its reset PC — and (b) static `_start` ELF executables for the
# `qemu-aarch64` (linux-user) differential oracle. Both run the *same* machine
# code; the only difference is the loader. The `.bin` images are committed; the
# ELFs are rebuilt on demand by the V&V suite when a linker + qemu are present.
#
# Toolchain: the LLVM cross-assembler (`llvm-mc`/`llvm-objcopy`, always able to
# target aarch64) is preferred; the GNU `aarch64-linux-gnu-*` binutils are used
# when present. No host architecture assumption — A64 is assembled cross.
set -euo pipefail
cd "$(dirname "$0")"

BATTERIES="arith memory control simd"

asm_bin() {
    local s="$1.s" obj="$1.o" bin="$1.bin"
    if command -v llvm-mc >/dev/null 2>&1 && command -v llvm-objcopy >/dev/null 2>&1; then
        llvm-mc -triple=aarch64-linux-gnu -filetype=obj "$s" -o "$obj"
        llvm-objcopy -O binary --only-section=.text "$obj" "$bin"
    elif command -v aarch64-linux-gnu-as >/dev/null 2>&1; then
        aarch64-linux-gnu-as "$s" -o "$obj"
        aarch64-linux-gnu-objcopy -O binary --only-section=.text "$obj" "$bin"
    else
        echo "build.sh: no aarch64 assembler (llvm-mc or aarch64-linux-gnu-as)" >&2
        return 127
    fi
    rm -f "$obj"
    echo "  assembled $bin ($(wc -c <"$bin") bytes)"
}

asm_elf() {
    # A static, no-libc ELF entered at _start — for the qemu-aarch64 oracle.
    local s="$1.s" obj="$1.elf.o" elf="$1.elf"
    if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
        aarch64-linux-gnu-gcc -nostdlib -static -Wl,-e,_start "$s" -o "$elf"
    elif command -v clang >/dev/null 2>&1 && command -v ld.lld >/dev/null 2>&1; then
        clang --target=aarch64-linux-gnu -c "$s" -o "$obj"
        ld.lld -e _start -o "$elf" "$obj"
        rm -f "$obj"
    else
        return 1
    fi
    echo "  linked $elf"
}

echo "CC-35: assembling A64 batteries"
for b in $BATTERIES; do
    asm_bin "$b"
done

if [ "${WITH_ELF:-0}" = 1 ]; then
    for b in $BATTERIES; do asm_elf "$b" || echo "  (no linker — skipped $b.elf)"; done
fi

# Refresh the checksum manifest of the committed images.
sha256sum $(for b in $BATTERIES; do echo "$b.bin"; done) >cc35.sha256
echo "CC-35: done"
