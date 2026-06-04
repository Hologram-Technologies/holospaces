// CC-35 A64 Advanced SIMD (NEON) + scalar floating-point self-check battery.
//
// Exercises the SIMD&FP execution unit (the Arm Architecture Reference Manual,
// ARM DDI 0487, C7) and verifies each result against its Arm-ARM-defined value:
// the lane-broadcast + element moves (DUP/UMOV), the bitwise + arithmetic
// three-same forms (AND/ORR/EOR/ADD), compare-to-zero (CMEQ), the modified
// immediate (MOVI), the shifts (SHL/USHR), EXT, the scalar FP data-processing
// (FADD/FSUB/FMUL/FDIV, single + double), the general<->SIMD FMOV, the
// int<->fp conversions (SCVTF/FCVTZS), and FCMP. On full success it writes
// "PASS\n" + exits 0; on the first failing check "FAIL\n" + exits 1 — so the
// holospaces core witness and the qemu-aarch64 differential compare the same
// stdout + status.
//
// Position-independent (scratch in registers, messages reached by ADR) so it
// runs identically as a flat image on the core and as a static ELF under
// qemu-aarch64.

.text
.global _start
_start:
    // ---- DUP (general) + ADD (vector) + UMOV ----
    mov  w0, #5
    dup  v0.16b, w0
    mov  w1, #3
    dup  v1.16b, w1
    add  v2.16b, v0.16b, v1.16b      // each lane 5 + 3 = 8
    umov w2, v2.b[7]
    cmp  w2, #8
    b.ne fail

    // ---- MOVI + AND / ORR / EOR (three-same logical) ----
    movi v3.16b, #0xf0
    movi v4.16b, #0x3c
    and  v5.16b, v3.16b, v4.16b      // 0x30
    umov w2, v5.b[0]
    cmp  w2, #0x30
    b.ne fail
    orr  v6.16b, v3.16b, v4.16b      // 0xfc
    umov w2, v6.b[3]
    cmp  w2, #0xfc
    b.ne fail
    eor  v7.16b, v3.16b, v4.16b      // 0xcc
    umov w2, v7.b[1]
    cmp  w2, #0xcc
    b.ne fail

    // ---- CMEQ #0 (compare-to-zero) ----
    movi v8.16b, #0
    cmeq v9.16b, v8.16b, #0          // every lane 0xff
    umov w2, v9.b[5]
    cmp  w2, #0xff
    b.ne fail

    // ---- shifts: SHL (.4s), USHR (.16b) ----
    mov  w0, #1
    dup  v10.4s, w0
    shl  v11.4s, v10.4s, #4          // 1 << 4 = 16
    umov w2, v11.s[0]
    cmp  w2, #16
    b.ne fail
    mov  w0, #0x80
    dup  v12.16b, w0
    ushr v13.16b, v12.16b, #3        // 0x80 >> 3 = 0x10
    umov w2, v13.b[0]
    cmp  w2, #0x10
    b.ne fail

    // ---- EXT (byte-granular concatenation) ----
    movi v14.16b, #0xaa
    movi v15.16b, #0xbb
    ext  v16.16b, v14.16b, v15.16b, #8   // low 8 bytes 0xaa, high 8 bytes 0xbb
    umov w2, v16.b[0]
    cmp  w2, #0xaa
    b.ne fail
    umov w2, v16.b[8]
    cmp  w2, #0xbb
    b.ne fail

    // ---- scalar FP (double): FADD / FSUB / FMUL / FDIV + general<->SIMD FMOV ----
    movz x0, #0x4000, lsl #48        // 2.0 = 0x4000_0000_0000_0000
    fmov d0, x0
    movz x1, #0x4008, lsl #48        // 3.0 = 0x4008_0000_0000_0000
    fmov d1, x1
    fadd d2, d0, d1                  // 5.0 = 0x4014_...
    fmov x2, d2
    movz x3, #0x4014, lsl #48
    cmp  x2, x3
    b.ne fail
    fsub d3, d1, d0                  // 1.0 = 0x3ff0_...
    fmov x2, d3
    movz x3, #0x3ff0, lsl #48
    cmp  x2, x3
    b.ne fail
    fmul d4, d0, d1                  // 6.0 = 0x4018_...
    fmov x2, d4
    movz x3, #0x4018, lsl #48
    cmp  x2, x3
    b.ne fail
    fdiv d5, d1, d0                  // 1.5 = 0x3ff8_...
    fmov x2, d5
    movz x3, #0x3ff8, lsl #48
    cmp  x2, x3
    b.ne fail

    // ---- conversions: SCVTF (int->fp), FCVTZS (fp->int, toward zero) ----
    mov  x0, #7
    scvtf d6, x0                     // 7.0 = 0x401c_...
    fmov x2, d6
    movz x3, #0x401c, lsl #48
    cmp  x2, x3
    b.ne fail
    fcvtzs x4, d6                    // 7
    cmp  x4, #7
    b.ne fail
    fcvtzs x5, d5                    // 1.5 -> 1 (truncate toward zero)
    cmp  x5, #1
    b.ne fail

    // ---- FCMP + condition branches ----
    fcmp d0, d1                      // 2.0 < 3.0 -> LT (N=1,V=0)
    b.ge fail                        // GE (N==V) is false
    fcmp d1, d0                      // 3.0 > 2.0 -> GT
    b.le fail                        // LE is false

    // ---- scalar FP (single precision): FMOV s, FADD s ----
    movz w0, #0x4000, lsl #16        // 2.0f = 0x4000_0000
    fmov s7, w0
    movz w1, #0x4040, lsl #16        // 3.0f = 0x4040_0000
    fmov s8, w1
    fadd s9, s7, s8                  // 5.0f = 0x40a0_0000
    fmov w2, s9
    movz w3, #0x40a0, lsl #16
    cmp  w2, w3
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
