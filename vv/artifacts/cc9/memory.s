.global _start
_start:
  li   t1, 0x4000     # scratch addr in RAM
  li   t0, 0xDEADBEEF
  sd   t0, 0(t1)
  ld   t2, 0(t1)
  lbu  a0, 0(t1)      # low byte 0xEF = 239
  li   a7, 93
  ecall
