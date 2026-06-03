/* PID 1 for the holospaces AArch64 emulator boot witness (CC-36) — freestanding,
 * raw arm64 Linux syscalls (no libc). Prints a marker + the real /proc/version,
 * then powers the machine off via the reboot syscall (-> PSCI SYSTEM_OFF over an
 * SMC -> the emulator's PSCI handler halts). The arm64 syscall ABI: the number
 * in x8, arguments x0..x5, the call via `svc #0`. */
#define SYS_openat 56
#define SYS_read   63
#define SYS_write  64
#define SYS_mount  40
#define SYS_reboot 142

static long sys3(long n, long a, long b, long c) {
    register long x8 asm("x8") = n;
    register long x0 asm("x0") = a, x1 asm("x1") = b, x2 asm("x2") = c;
    asm volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}
static long sys4(long n, long a, long b, long c, long d) {
    register long x8 asm("x8") = n;
    register long x0 asm("x0") = a, x1 asm("x1") = b, x2 asm("x2") = c, x3 asm("x3") = d;
    asm volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
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
    sys4(SYS_reboot, (long)0xfee1deadL, (long)0x28121969L, (long)0x4321fedcL, 0);
    for (;;) {}
}
