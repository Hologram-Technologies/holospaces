/* PID 1 for the holospaces x86-64 (amd64) emulator boot witness (CC-44) —
 * freestanding, raw amd64 Linux syscalls (no libc). The x86-64 realization of
 * the CC-36 arm64 init.c. Prints a marker + the real /proc/version, then powers
 * the machine off via the reboot syscall (LINUX_REBOOT_CMD_POWER_OFF). On a
 * platform with no ACPI/PM (the holospaces x86-64 core) the kernel's power-off
 * falls through to native_machine_halt → stop_this_cpu → `hlt` with interrupts
 * masked → the emulator halts (Halt::Halted), the clean-stop signal the witness
 * accepts. The amd64 syscall ABI: the number in rax, arguments
 * rdi,rsi,rdx,r10,r8,r9, the call via `syscall`. */
#define SYS_read    0
#define SYS_write   1
#define SYS_mount  165
#define SYS_reboot 169
#define SYS_openat 257

static long sys3(long n, long a, long b, long c) {
    register long rax asm("rax") = n;
    register long rdi asm("rdi") = a;
    register long rsi asm("rsi") = b;
    register long rdx asm("rdx") = c;
    asm volatile("syscall"
                 : "+r"(rax)
                 : "r"(rdi), "r"(rsi), "r"(rdx)
                 : "rcx", "r11", "memory");
    return rax;
}
static long sys4(long n, long a, long b, long c, long d) {
    register long rax asm("rax") = n;
    register long rdi asm("rdi") = a;
    register long rsi asm("rsi") = b;
    register long rdx asm("rdx") = c;
    register long r10 asm("r10") = d;
    asm volatile("syscall"
                 : "+r"(rax)
                 : "r"(rdi), "r"(rsi), "r"(rdx), "r"(r10)
                 : "rcx", "r11", "memory");
    return rax;
}
static unsigned slen(const char *s) { unsigned n = 0; while (s[n]) n++; return n; }

void _start(void) {
    const char *msg = "HOLOSPACES-LINUX-USERSPACE-OK\n";
    sys3(SYS_write, 1, (long)msg, slen(msg));
    sys4(SYS_mount, (long)"proc", (long)"/proc", (long)"proc", 0); /* errors ignored */
    char buf[512];
    long fd = sys4(SYS_openat, -100, (long)"/proc/version", 0, 0); /* AT_FDCWD, O_RDONLY */
    if (fd >= 0) {
        long n = sys3(SYS_read, fd, (long)buf, sizeof buf);
        if (n > 0) sys3(SYS_write, 1, (long)buf, n);
    }
    /* LINUX_REBOOT: magic1=0xfee1dead, magic2=672274793, cmd=POWER_OFF. */
    sys4(SYS_reboot, (long)0xfee1deadL, (long)0x28121969L, (long)0x4321fedcL, 0);
    for (;;) {}
}
