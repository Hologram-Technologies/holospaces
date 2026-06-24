/* A freestanding program the guest's in-image toolchain (tcc) compiles and the
   guest then runs — the build-in-guest proof (CC-45). No libc/headers: it makes
   the write + exit syscalls directly, so the compile depends only on tcc itself. */
static long sys_write(long fd, const char *b, long n) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(1), "D"(fd), "S"(b), "d"(n)
                     : "rcx", "r11", "memory");
    return r;
}
static void sys_exit(long c) {
    __asm__ volatile("syscall" : : "a"(60), "D"(c));
}
void _start(void) {
    const char m[] = "CC45-BUILT-IN-GUEST:42\n";
    sys_write(1, m, sizeof(m) - 1);
    sys_exit(0);
}
