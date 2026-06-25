/* PID 1 for the holospaces x86-64 (amd64) Alpine boot witness (CC-45) —
 * freestanding, raw amd64 Linux syscalls (no libc), the rootfs-over-κ-disk
 * realization of CC-44's freestanding initramfs PID-1. Where CC-44 proves the
 * *kernel* reaches userspace, CC-45 proves a real **Alpine** userland mounted
 * over the virtio-blk κ-disk actually RUNS: this init mounts the pseudo-fs,
 * reads /etc/alpine-release straight off the mounted root (so the assertion can
 * only pass if the real Alpine ext4 is the live root), then fork+execve's the
 * stock Alpine `/bin/busybox` (musl-linked — the kernel loads its PT_INTERP
 * /lib/ld-musl-x86_64.so.1) to run a tiny shell script that re-prints the
 * release and `apk --version`, proving musl + busybox + apk-tools execute. Then
 * it powers the machine off via the reboot syscall exactly as CC-44 does
 * (LINUX_REBOOT_CMD_POWER_OFF → native_machine_halt → hlt → Halt::Halted, the
 * clean-stop the witness accepts). amd64 ABI: nr in rax; args rdi,rsi,rdx,r10,
 * r8,r9; `syscall`. */
#define SYS_read    0
#define SYS_write   1
#define SYS_mount  165
#define SYS_reboot 169
#define SYS_openat 257
#define SYS_clone   56
#define SYS_execve  59
#define SYS_wait4   61

static long sys3(long n, long a, long b, long c) {
    register long rax asm("rax") = n, rdi asm("rdi") = a, rsi asm("rsi") = b, rdx asm("rdx") = c;
    asm volatile("syscall" : "+r"(rax) : "r"(rdi), "r"(rsi), "r"(rdx) : "rcx", "r11", "memory");
    return rax;
}
static long sys4(long n, long a, long b, long c, long d) {
    register long rax asm("rax") = n, rdi asm("rdi") = a, rsi asm("rsi") = b, rdx asm("rdx") = c, r10 asm("r10") = d;
    asm volatile("syscall" : "+r"(rax) : "r"(rdi), "r"(rsi), "r"(rdx), "r"(r10) : "rcx", "r11", "memory");
    return rax;
}
static long sys5(long n, long a, long b, long c, long d, long e) {
    register long rax asm("rax") = n, rdi asm("rdi") = a, rsi asm("rsi") = b, rdx asm("rdx") = c,
                  r10 asm("r10") = d, r8 asm("r8") = e;
    asm volatile("syscall" : "+r"(rax) : "r"(rdi),"r"(rsi),"r"(rdx),"r"(r10),"r"(r8) : "rcx","r11","memory");
    return rax;
}
static unsigned slen(const char *s){ unsigned n=0; while(s[n]) n++; return n; }
static void out(const char *s){ sys3(SYS_write, 1, (long)s, slen(s)); }
static void cat(const char *path){
    char buf[256];
    long fd = sys4(SYS_openat, -100, (long)path, 0, 0); /* AT_FDCWD, O_RDONLY */
    if (fd < 0) return;
    long n; while ((n = sys3(SYS_read, fd, (long)buf, sizeof buf)) > 0) sys3(SYS_write, 1, (long)buf, n);
}

void _start(void) {
    out("HOLOSPACES-ALPINE-USERSPACE-OK\n");
    /* The pseudo-filesystems a real userland (and apk) expect. Errors ignored —
     * a re-mount of an already-present node is harmless. */
    sys5(SYS_mount, (long)"proc",     (long)"/proc", (long)"proc",     0, 0);
    sys5(SYS_mount, (long)"sysfs",    (long)"/sys",  (long)"sysfs",    0, 0);
    sys5(SYS_mount, (long)"devtmpfs", (long)"/dev",  (long)"devtmpfs", 0, 0);

    /* Proof #1: the Alpine ext4 really is the mounted root. */
    out("alpine-release: "); cat("/etc/alpine-release");

    /* Proof #2: the stock musl-linked Alpine userland EXECUTES. fork+exec+wait
     * busybox sh; the kernel resolves /lib/ld-musl-x86_64.so.1 (PT_INTERP). */
    static char *argv[] = {
        "/bin/busybox", "sh", "-c",
        "echo ALPINE-USERLAND-RAN; cat /etc/alpine-release; /sbin/apk --version 2>&1 || echo apk-missing",
        0
    };
    static char *envp[] = { "PATH=/usr/sbin:/usr/bin:/sbin:/bin", 0 };
    long pid = sys5(SYS_clone, 17 /*SIGCHLD*/, 0, 0, 0, 0); /* fork() */
    if (pid == 0) {
        sys3(SYS_execve, (long)argv[0], (long)argv, (long)envp);
        out("execve-failed\n");
        sys4(SYS_reboot, (long)0xfee1deadL, (long)0x28121969L, (long)0x4321fedcL, 0);
        for(;;){}
    }
    int status = 0;
    sys4(SYS_wait4, pid, (long)&status, 0, 0);

    /* LINUX_REBOOT magic1=0xfee1dead magic2=672274793 cmd=POWER_OFF (0x4321fedc). */
    sys4(SYS_reboot, (long)0xfee1deadL, (long)0x28121969L, (long)0x4321fedcL, 0);
    for (;;) {}
}
