.global _start
_start:
  li   t0, 0x7FFFFFFF
  addiw t0, t0, 1     # 32-bit overflow -> sign-extended 0xFFFFFFFF_80000000
  srai t0, t0, 63     # arithmetic >> => all ones = -1
  addi t0, t0, 101    # -1 + 101 = 100
  mv   a0, t0         # 100
  li   a7, 93
  ecall
