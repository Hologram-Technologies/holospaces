.global _start
_start:
  li t0, 0
  li t1, 1
  li t2, 11
1:
  bge t1, t2, 2f
  add t0, t0, t1
  addi t1, t1, 1
  j 1b
2:
  mv a0, t0          # 1+..+10 = 55
  li a7, 93
  ecall
