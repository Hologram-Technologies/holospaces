.global _start
_start:
  li   t0, 0x123
  csrw mscratch, t0       # CSR 0x340 = t0
  csrr a0, mscratch       # a0 = 0x123 (291)
  csrrwi t1, mscratch, 5  # t1 = old (0x123); mscratch = 5
  add  a0, a0, t1         # 291 + 291 = 582
  csrr t2, mscratch       # t2 = 5
  add  a0, a0, t2         # 582 + 5 = 587
  li   a7, 93
  ecall
