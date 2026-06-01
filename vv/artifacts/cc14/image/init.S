.section .text
.globl _start
_start:
    li a0, 1
    lla a1, msg
    li a2, 13
    li a7, 64          # write
    ecall
    li a0, 0xfee1dead  # LINUX_REBOOT_MAGIC1
    li a1, 0x28121969  # LINUX_REBOOT_MAGIC2
    li a2, 0x4321fedc  # LINUX_REBOOT_CMD_POWER_OFF
    li a3, 0
    li a7, 142         # reboot
    ecall
    li a0, 0
    li a7, 93          # exit
    ecall
1:  j 1b
.section .rodata
msg: .ascii "USERSPACE-OK\n"
