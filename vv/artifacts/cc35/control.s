// CC-35 A64 integer core — control-flow self-check battery.
//
// Exercises the A64 branch family (B/BL/B.cond/CBZ/CBNZ/TBZ/TBNZ/RET) and the
// NZCV condition codes through real loops and a subroutine call. Computes
// sum(1..=100) = 5050 with a CBNZ countdown, checks it, then validates a
// BL/RET call and a TBZ skip. PASS\n + exit 0 on success, FAIL\n + exit 1
// otherwise. Position-independent.

.text
.global _start
_start:
    // sum(1..=100) via a CBNZ countdown loop.
    mov  x0, #0                    // acc
    mov  x1, #100                  // i
1:  add  x0, x0, x1
    sub  x1, x1, #1
    cbnz x1, 1b
    movz x2, #5050
    cmp  x0, x2
    b.ne fail

    // BL/RET: a subroutine that adds 1.
    mov  x0, #41
    bl   inc
    cmp  x0, #42
    b.ne fail

    // TBNZ on a set bit takes the branch (skips a poison).
    mov  x3, #8                    // bit 3 set
    tbnz x3, #3, 2f
    b    fail                      // not taken would be a bug
2:
    // TBZ on a clear bit takes the branch.
    mov  x4, #0
    tbz  x4, #5, 3f
    b    fail
3:
    // CBZ on zero takes the branch.
    mov  x5, #0
    cbz  x5, pass
    b    fail

inc:
    add  x0, x0, #1
    ret

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
