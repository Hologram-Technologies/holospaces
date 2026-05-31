/* PID 1 for the holospaces emulator boot witness — freestanding, raw RISC-V
 * Linux syscalls (no libc). Prints a marker + the real /proc/version, then
 * powers the machine off via the reboot syscall (-> SBI SRST -> emulator halt). */
#define SYS_openat 56
#define SYS_read   63
#define SYS_write  64
#define SYS_mount  40
#define SYS_reboot 142

static long sys3(long n,long a,long b,long c){
  register long a7 asm("a7")=n,a0 asm("a0")=a,a1 asm("a1")=b,a2 asm("a2")=c;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2):"memory");
  return a0;
}
static long sys4(long n,long a,long b,long c,long d){
  register long a7 asm("a7")=n,a0 asm("a0")=a,a1 asm("a1")=b,a2 asm("a2")=c,a3 asm("a3")=d;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2),"r"(a3):"memory");
  return a0;
}
static unsigned slen(const char*s){unsigned n=0;while(s[n])n++;return n;}

void _start(void){
  const char *msg="HOLOSPACES-LINUX-USERSPACE-OK\n";
  sys3(SYS_write,1,(long)msg,slen(msg));
  sys4(SYS_mount,(long)"proc",(long)"/proc",(long)"proc",0); /* errors ignored */
  char buf[512];
  long fd=sys4(SYS_openat,-100,(long)"/proc/version",0,0); /* AT_FDCWD, O_RDONLY */
  if(fd>=0){ long n=sys3(SYS_read,fd,(long)buf,sizeof buf); if(n>0) sys3(SYS_write,1,(long)buf,n); }
  sys4(SYS_reboot,(long)0xfee1deadL,(long)0x28121969L,(long)0x4321fedcL,0);
  for(;;){}
}
