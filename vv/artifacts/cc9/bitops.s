.global _start
_start:
  li t0, 0xF0
  li t1, 0x0F
  or  t0, t0, t1     # 0xFF
  li  t2, 0xAA
  and t0, t0, t2     # 0xAA = 170
  xori t0, t0, 0xFF  # 0x55 = 85
  slli t0, t0, 1     # 0xAA = 170
  srli t0, t0, 1     # 0x55 = 85
  mv  a0, t0         # 85
  li  a7, 93
  ecall
