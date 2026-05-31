/* PID 1 for the holospaces workspace witness (CC-11) — a tiny real shell over a
 * real terminal. Freestanding, raw RISC-V Linux syscalls (no libc). It reads
 * lines from the console (the terminal-input channel a workspace projection
 * drives) and answers: `echo <text>` prints the text; `version` prints
 * /proc/version; `exit` (or end of input) powers the machine off via reboot(2).
 * Deterministic given the input, so its output is a differential oracle. */
#define SYS_openat 56
#define SYS_read   63
#define SYS_write  64
#define SYS_mount  40
#define SYS_ioctl  29
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
/* Put the console tty in non-echoing line mode so only the program's own output
 * appears (the terminal echo would otherwise interleave non-deterministically). */
#define TCGETS 0x5401
#define TCSETS 0x5402
#define ECHO_BITS 0x78  /* ECHO|ECHOE|ECHOK|ECHONL */
#define ICANON 0x2
static void raw_console(void){
  unsigned char t[64];
  for(int i=0;i<64;i++) t[i]=0;
  if(sys3(SYS_ioctl,0,TCGETS,(long)t)!=0) return;
  /* c_lflag is the 4th 32-bit field (offset 12). Clear ECHO* (keep ICANON for
   * line buffering — no echo is what makes the output deterministic). */
  unsigned lflag = t[12]|(t[13]<<8)|(t[14]<<16)|((unsigned)t[15]<<24);
  lflag &= ~((unsigned)ECHO_BITS);
  t[12]=lflag&0xff; t[13]=(lflag>>8)&0xff; t[14]=(lflag>>16)&0xff; t[15]=(lflag>>24)&0xff;
  sys3(SYS_ioctl,0,TCSETS,(long)t);
}
static void puts_(const char*s){ sys3(SYS_write,1,(long)s,slen(s)); }
static void putb_(const char*s,long n){ sys3(SYS_write,1,(long)s,n); }
static int streq(const char*a,const char*b){ while(*a&&*a==*b){a++;b++;} return *a==*b; }

int _start(void){
  sys4(SYS_mount,(long)"proc",(long)"/proc",(long)"proc",0);
  raw_console();
  puts_("HOLOSPACES-WORKSPACE-READY\n");
  char line[256];
  for(;;){
    puts_("$ ");
    /* read one line (until '\n' or EOF) */
    long n=0;
    for(;;){
      char c; long r=sys3(SYS_read,0,(long)&c,1);
      if(r<=0){ puts_("\n"); goto done; }      /* EOF -> power off */
      if(c=='\n') break;
      if(n<(long)sizeof line-1) line[n++]=c;
    }
    line[n]=0;
    if(streq(line,"exit")) break;
    if(streq(line,"version")){
      char buf[512];
      long fd=sys4(SYS_openat,-100,(long)"/proc/version",0,0);
      if(fd>=0){ long m=sys3(SYS_read,fd,(long)buf,sizeof buf); if(m>0) putb_(buf,m); }
      continue;
    }
    /* echo <text> */
    if(n>=4 && line[0]=='e'&&line[1]=='c'&&line[2]=='h'&&line[3]=='o'){
      long i=4; if(line[i]==' ') i++;
      putb_(line+i,n-i); puts_("\n"); continue;
    }
    puts_("sh: "); putb_(line,n); puts_(": not found\n");
  }
done:
  puts_("HOLOSPACES-WORKSPACE-DONE\n");
  sys4(SYS_reboot,(long)0xfee1deadL,(long)0x28121969L,(long)0x4321fedcL,0);
  for(;;){}
}
