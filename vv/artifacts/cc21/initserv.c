/* CC-21 — a server running INSIDE the devcontainer, reached from outside via a
 * forwarded port (the running-app preview). A freestanding RV64 program (no
 * libc): bind 0.0.0.0:8080, listen, accept one connection over the guest's
 * virtio-net interface (the CC-16 NAT's ingress path), serve an HTTP response,
 * and power off. holospaces forwards a host port to this listener. */
static long ecall(long n,long a,long b,long c,long d,long e){
  register long a7 asm("a7")=n,a0 asm("a0")=a,a1 asm("a1")=b,a2 asm("a2")=c,a3 asm("a3")=d,a4 asm("a4")=e;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2),"r"(a3),"r"(a4):"memory");return a0;
}
#define SYS_close 57
#define SYS_write 64
#define SYS_read 63
#define SYS_socket 198
#define SYS_bind 200
#define SYS_listen 201
#define SYS_accept 202
#define SYS_reboot 142
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void put(const char*s){ecall(SYS_write,1,(long)s,slen(s),0,0);}
void _start(void){
  unsigned char sa[16]={0};
  sa[0]=2;                 /* AF_INET */
  sa[2]=0x1f;sa[3]=0x90;   /* port 8080 BE */
  /* sa[4..8] = 0.0.0.0 (INADDR_ANY) */
  long fd=ecall(SYS_socket,2,1,0,0,0);
  if(fd<0){put("SERVER-SOCKET-FAIL\n");goto off;}
  if(ecall(SYS_bind,fd,(long)sa,16,0,0)<0){put("SERVER-BIND-FAIL\n");goto off;}
  if(ecall(SYS_listen,fd,8,0,0,0)<0){put("SERVER-LISTEN-FAIL\n");goto off;}
  put("SERVER-LISTENING\n");
  long cfd=ecall(SYS_accept,fd,0,0,0,0);
  if(cfd<0){put("SERVER-ACCEPT-FAIL\n");goto off;}
  char buf[256]; ecall(SYS_read,cfd,(long)buf,sizeof buf,0,0); /* the request */
  const char*resp="HTTP/1.0 200 OK\r\nContent-Length: 23\r\n\r\nHELLO-FROM-GUEST-SERVER";
  ecall(SYS_write,cfd,(long)resp,slen(resp),0,0);
  ecall(SYS_close,cfd,0,0,0,0);
  put("SERVER-SERVED\n");
off:
  ecall(SYS_reboot,0xfee1dead,0x28121969,0x4321fedc,0,0);for(;;);
}
