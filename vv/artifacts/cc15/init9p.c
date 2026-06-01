/* freestanding: raw riscv64 syscalls, no libc */
static long ecall(long n,long a,long b,long c,long d,long e){
  register long a7 asm("a7")=n, a0 asm("a0")=a, a1 asm("a1")=b, a2 asm("a2")=c, a3 asm("a3")=d, a4 asm("a4")=e;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2),"r"(a3),"r"(a4):"memory");
  return a0;
}
#define SYS_openat 56
#define SYS_close 57
#define SYS_read 63
#define SYS_write 64
#define SYS_mount 40
#define SYS_mkdirat 34
#define SYS_reboot 142
#define AT_FDCWD -100
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void puts_(const char*s){ecall(SYS_write,1,(long)s,slen(s),0,0);}
void _start(void){
  ecall(SYS_mkdirat,AT_FDCWD,(long)"/mnt",0755,0,0);
  long r=ecall(SYS_mount,(long)"hsworkspace",(long)"/mnt",(long)"9p",0,(long)"trans=virtio,version=9p2000.L");
  if(r<0){puts_("9P-MOUNT-FAILED\n");goto off;}
  puts_("9P-MOUNTED\n");
  /* read the file holospaces placed on the share */
  long fd=ecall(SYS_openat,AT_FDCWD,(long)"/mnt/from-holospaces.txt",0/*O_RDONLY*/,0,0);
  if(fd>=0){char b[128];long n=ecall(SYS_read,fd,(long)b,sizeof b,0,0);if(n>0){puts_("READ:");ecall(SYS_write,1,(long)b,n,0,0);}ecall(SYS_close,fd,0,0,0,0);}
  else puts_("9P-READ-OPEN-FAILED\n");
  /* write a file back for holospaces to read */
  fd=ecall(SYS_openat,AT_FDCWD,(long)"/mnt/from-guest.txt",0x41/*O_WRONLY|O_CREAT*/,0644,0);
  if(fd>=0){const char*m="GUEST-WROTE-THIS\n";ecall(SYS_write,fd,(long)m,slen(m),0,0);ecall(SYS_close,fd,0,0,0,0);puts_("9P-WROTE\n");}
  else puts_("9P-WRITE-OPEN-FAILED\n");
  puts_("9P-DONE\n");
off:
  ecall(SYS_reboot,0xfee1dead,0x28121969,0x4321fedc,0,0);
  for(;;);
}
