static long ecall(long n,long a,long b,long c,long d,long e){
  register long a7 asm("a7")=n,a0 asm("a0")=a,a1 asm("a1")=b,a2 asm("a2")=c,a3 asm("a3")=d,a4 asm("a4")=e;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2),"r"(a3),"r"(a4):"memory");return a0;
}
#define SYS_write 64
#define SYS_socket 198
#define SYS_connect 203
#define SYS_read 63
#define SYS_reboot 142
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void put(const char*s){ecall(SYS_write,1,(long)s,slen(s),0,0);}
void _start(void){
  /* sockaddr_in: family(2 LE) port(2 BE) addr(4 BE) pad(8) — connect to 10.0.2.9:8080 */
  unsigned char sa[16]={0};
  sa[0]=2;            /* AF_INET (LE u16 = 2) */
  sa[2]=0x1f;sa[3]=0x90; /* port 8080 BE */
  sa[4]=10;sa[5]=0;sa[6]=2;sa[7]=9; /* 10.0.2.9 */
  long fd=ecall(SYS_socket,2,1,0,0,0); /* AF_INET, SOCK_STREAM */
  if(fd<0){put("NET-SOCKET-FAILED\n");goto off;}
  long r=ecall(SYS_connect,fd,(long)sa,16,0,0);
  if(r<0){put("NET-CONNECT-FAILED\n");goto off;}
  put("NET-CONNECTED\n");
  const char*req="GET / HTTP/1.0\r\nHost: h\r\n\r\n";
  ecall(SYS_write,fd,(long)req,slen(req),0,0);
  char buf[256];long n=ecall(SYS_read,fd,(long)buf,sizeof buf,0,0);
  if(n>0){put("NET-RECV:");ecall(SYS_write,1,(long)buf,n,0,0);put("\n");}
  else put("NET-RECV-EMPTY\n");
  put("NET-DONE\n");
off:
  ecall(SYS_reboot,0xfee1dead,0x28121969,0x4321fedc,0,0);for(;;);
}
