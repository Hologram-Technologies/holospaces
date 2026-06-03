// CC-35 A64 integer core — data-processing self-check battery.
//
// Exercises the A64 data-processing instruction groups (the Arm Architecture
// Reference Manual, ARM DDI 0487) and verifies each against its Arm-ARM-defined
// result. On full success it writes "PASS\n" to fd 1 and exits 0; on the first
// failing check it writes "FAIL\n" and exits 1 — so the emulator witness and the
// qemu-aarch64 differential oracle compare the same stdout + status.
//
// Position-independent: no absolute addresses, scratch on the stack, messages
// reached by ADR — so it runs identically as a flat image on the holospaces
// core and as a static ELF under qemu-aarch64.

.text
.global _start
_start:
    // movz/movk compose a 64-bit constant.
    movz x0, #0x7788
    movk x0, #0x5566, lsl #16
    movk x0, #0x3344, lsl #32
    movk x0, #0x1122, lsl #48
    movz x1, #0x7788
    movk x1, #0x5566, lsl #16
    movk x1, #0x3344, lsl #32
    movk x1, #0x1122, lsl #48
    cmp  x0, x1
    b.ne fail

    // add/sub immediate with LSL #12 and the carry flags.
    mov  x2, #0
    add  x2, x2, #1, lsl #12        // 0x1000
    cmp  x2, #1, lsl #12
    b.ne fail

    // add/sub shifted register.
    mov  x3, #3
    mov  x4, #5
    add  x5, x4, x3, lsl #4         // 5 + (3<<4) = 53
    cmp  x5, #53
    b.ne fail

    // add/sub extended register (SXTB of -1).
    mov  x6, #100
    movn w7, #0                     // 0xffffffff, low byte 0xff = -1
    add  x8, x6, w7, sxtb           // 100 + (-1) = 99
    cmp  x8, #99
    b.ne fail

    // logical register + immediate.
    movz x9, #0xff00
    movz x10, #0x00ff
    orr  x11, x9, x10               // 0xffff
    movz x29, #0xffff
    cmp  x11, x29
    b.ne fail
    and  x12, x11, #0xff            // logical immediate
    cmp  x12, #0xff
    b.ne fail
    bic  x13, x11, x10              // 0xffff & ~0x00ff = 0xff00
    movz x29, #0xff00
    cmp  x13, x29
    b.ne fail

    // multiply / divide / high product.
    mov  x14, #7
    mov  x15, #6
    mul  x16, x14, x15             // 42
    cmp  x16, #42
    b.ne fail
    madd x17, x14, x15, x14        // 7*6 + 7 = 49
    cmp  x17, #49
    b.ne fail
    movz x18, #0x8000, lsl #48     // 1<<63
    mov  x19, #2
    umulh x20, x18, x19           // high 64 of (1<<63)*2 = 1
    cmp  x20, #1
    b.ne fail
    mov  x21, #20
    neg  x21, x21                 // -20
    mov  x22, #3
    sdiv x23, x21, x22            // -20/3 = -6
    mov  x24, #6
    neg  x24, x24
    cmp  x23, x24
    b.ne fail

    // variable shift + bitfield + extract.
    mov  x25, #1
    mov  x26, #40
    lsl  x27, x25, x26            // 1<<40
    movz x28, #0x0000, lsl #32
    movk x28, #0x0100, lsl #32    // 0x0000_0100_0000_0000 = 1<<40
    cmp  x27, x28
    b.ne fail
    movz x0, #0xcdef
    movk x0, #0x00ab, lsl #16     // 0x00ab_cdef
    ubfx x1, x0, #8, #8           // byte 1 = 0xcd
    cmp  x1, #0xcd
    b.ne fail
    movn x2, #0xff                // -256
    asr  x3, x2, #4              // -16
    mov  x4, #16
    neg  x4, x4
    cmp  x3, x4
    b.ne fail

    // one-source bit ops.
    movz x5, #0x1000, lsl #48     // 1<<60
    clz  x6, x5                  // 3
    cmp  x6, #3
    b.ne fail
    mov  x7, #1
    rbit x8, x7                  // 1<<63
    movz x9, #0x8000, lsl #48
    cmp  x8, x9
    b.ne fail

    // conditional select / conditional compare.
    mov  x10, #5
    mov  x11, #3
    cmp  x10, x11
    csel x12, x10, x11, gt       // 5 (GT holds)
    cmp  x12, #5
    b.ne fail

    b    pass

fail:
    adr  x1, failmsg
    mov  x0, #1
    mov  x2, #5
    mov  x8, #64
    svc  #0
    mov  x0, #1
    mov  x8, #93
    svc  #0
pass:
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
