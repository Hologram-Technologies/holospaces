/* CC-37 — the arm64 devcontainer's PID 1: a stock `linux-arm64` toolchain
 * binary (built by the upstream `aarch64-linux-gnu` GCC), freestanding (raw
 * arm64 syscalls, no libc), so it exercises only the base A64 + system ISA the
 * holospaces emulator implements. It boots from the κ-disk (virtio-blk) rootfs,
 * proves a real `linux-arm64` binary runs in the OS (a real computation + the
 * `uname` syscall reporting `aarch64`), reads the real `/proc/version`, and
 * powers off (`reboot` → PSCI SYSTEM_OFF → the emulator halts). A deterministic
 * witness that an unmodified `linux-arm64` binary runs on the emulator over the
 * shared virtio device — no riscv64 workaround. */
#define SYS_openat 56
#define SYS_read   63
#define SYS_write  64
#define SYS_mount  40
#define SYS_uname  160
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
static void puts_(const char *s) { sys3(SYS_write, 1, (long)s, slen(s)); }

/* struct utsname: 6 fields of 65 bytes (the Linux layout). */
struct utsname { char f[6][65]; };

void _start(void) {
    puts_("CC37-DEVCONTAINER-UP\n");

    /* A real computation, to prove the stock arm64 binary executes its logic. */
    long sum = 0;
    for (long i = 1; i <= 1000; i++) sum += i; /* 500500 */
    char num[8];
    int p = 0;
    char tmp[8];
    int t = 0;
    long v = sum;
    if (v == 0) tmp[t++] = '0';
    while (v > 0) { tmp[t++] = (char)('0' + v % 10); v /= 10; }
    while (t > 0) num[p++] = tmp[--t];
    num[p] = '\n';
    puts_("CC37-COMPUTE:");
    sys3(SYS_write, 1, (long)num, p + 1);

    /* The uname syscall reports the guest architecture (aarch64). */
    struct utsname u;
    if (sys3(SYS_uname, (long)&u, 0, 0) == 0) {
        puts_("CC37-ARCH:");
        puts_(u.f[4]); /* machine */
        puts_("\n");
    }

    /* Read the real /proc/version (the kernel, over the mounted rootfs). */
    sys4(SYS_mount, (long)"proc", (long)"/proc", (long)"proc", 0);
    char buf[256];
    long fd = sys4(SYS_openat, -100, (long)"/proc/version", 0, 0);
    if (fd >= 0) {
        long n = sys3(SYS_read, fd, (long)buf, sizeof buf);
        if (n > 0) sys3(SYS_write, 1, (long)buf, n);
    }

    sys4(SYS_reboot, (long)0xfee1deadL, (long)0x28121969L, (long)0x4321fedcL, 0);
    for (;;) {}
}
