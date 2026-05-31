.global _start
_start:
  li   t1, 0x4000
  li   t0, 100
  sw   t0, 0(t1)
  li   t2, 23
  amoadd.w  a3, t2, (t1)  # a3 = old (100); mem = 123
  amoswap.w a4, x0, (t1)  # a4 = old (123); mem = 0
  add  a0, a3, a4         # 100 + 123 = 223
  li   a7, 93
  ecall
