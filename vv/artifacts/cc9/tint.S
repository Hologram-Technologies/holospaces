.section .text
.global _start
_start:
  la   t0, handler
  csrw mtvec, t0           # trap vector (direct)
  li   t0, 0x02004000      # CLINT mtimecmp
  li   t1, 50
  sd   t1, 0(t0)           # mtimecmp = 50 (arm the timer)
  li   t0, 0x80
  csrs mie, t0             # MTIE (machine timer interrupt enable)
  li   t0, 0x8
  csrs mstatus, t0         # MIE (global)
  li   s0, 0               # not-yet-fired marker
spin:
  j    spin                # wait for the timer interrupt
.align 2
handler:
  csrr t2, mcause          # expect interrupt (bit63) | 7 (machine timer)
  li   t3, 0x8000000000000007
  bne  t2, t3, fail
  li   t0, 1
  la   t1, tohost
  sd   t0, 0(t1)           # HTIF exit(0) — pass
1: j 1b
fail:
  li   t0, 3
  la   t1, tohost
  sd   t0, 0(t1)           # HTIF exit(1) — fail
1: j 1b
.section .tohost,"aw",@progbits
.align 6
.global tohost
tohost: .dword 0
