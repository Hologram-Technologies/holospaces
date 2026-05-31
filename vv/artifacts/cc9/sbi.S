.section .text
.global _start
_start:
  la   t0, s_entry
  csrw mepc, t0
  li   t0, (3<<11)
  csrc mstatus, t0        # clear MPP
  li   t0, (1<<11)
  csrs mstatus, t0        # MPP = S (01)
  mret                    # drop to supervisor mode at s_entry
s_entry:
  li   a7, 1              # SBI legacy console_putchar
  li   a0, 0x4F           # 'O'
  ecall
  li   a7, 1
  li   a0, 0x4B           # 'K'
  ecall
  li   a7, 1
  li   a0, 0x0A           # '\n'
  ecall
  li   a7, 0x53525354     # SBI SRST (system reset)
  li   a6, 0              # system_reset
  li   a0, 0              # shutdown
  ecall
1: j 1b
