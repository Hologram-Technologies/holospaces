// CC-35 A64 integer core — load/store self-check battery.
//
// Exercises the A64 load/store family (LDR/STR unsigned-offset + pre/post-index
// + register-offset, the sign/zero-extension variants, LDP/STP) against its
// Arm-ARM-defined result. Scratch lives on the stack (writable on both the
// holospaces core and qemu-aarch64), so the battery is position-independent.
// PASS\n + exit 0 on success, FAIL\n + exit 1 on the first failure.

.text
.global _start
_start:
    sub  sp, sp, #128              // a 128-byte scratch frame

    // STR/LDR dword round trip.
    movz x0, #0xbeef
    movk x0, #0xdead, lsl #16      // 0xdead_beef
    str  x0, [sp, #0]
    ldr  x1, [sp, #0]
    cmp  x0, x1
    b.ne fail

    // STRB then LDRSB sign-extends 0xff -> -1.
    movn x2, #0                    // 0xffff_ffff_ffff_ffff
    strb w2, [sp, #16]
    ldrsb x3, [sp, #16]
    mov  x4, #1
    neg  x4, x4                    // -1
    cmp  x3, x4
    b.ne fail

    // STRH then LDRH zero-extends.
    movz x5, #0xabcd
    strh w5, [sp, #24]
    ldrh w6, [sp, #24]
    movz x29, #0xabcd
    cmp  x6, x29
    b.ne fail

    // Pre-index + post-index writeback.
    add  x7, sp, #40
    movz x8, #0x1234
    str  x8, [x7, #8]!            // pre-index: x7 += 8, store there
    ldr  x9, [x7]
    cmp  x8, x9
    b.ne fail
    movz x10, #0x5678
    str  x10, [x7], #8           // post-index: store at x7, then x7 += 8
    sub  x11, x7, #8
    ldr  x12, [x11]
    cmp  x10, x12
    b.ne fail

    // Register-offset addressing with LSL.
    add  x13, sp, #64
    mov  x14, #2
    movz x15, #0x9999
    str  x15, [x13, x14, lsl #3] // [x13 + 16]
    ldr  x16, [x13, #16]
    cmp  x15, x16
    b.ne fail

    // STP/LDP pair.
    add  x17, sp, #96
    movz x18, #0x1111
    movz x19, #0x2222
    stp  x18, x19, [x17]
    ldp  x20, x21, [x17]
    cmp  x18, x20
    b.ne fail
    cmp  x19, x21
    b.ne fail

    b    pass

fail:
    add  sp, sp, #128
    adr  x1, failmsg
    mov  x0, #1
    mov  x2, #5
    mov  x8, #64
    svc  #0
    mov  x0, #1
    mov  x8, #93
    svc  #0
pass:
    add  sp, sp, #128
    adr  x1, passmsg
    mov  x0, #1
    mov  x2, #5
    mov  x8, #64
    svc  #0
    mov  x0, #0
    mov  x8, #93
    svc  #0

passmsg:
    .ascii "PASS\n"
failmsg:
    .ascii "FAIL\n"
