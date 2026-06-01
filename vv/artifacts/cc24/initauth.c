/* CC-24 — the devcontainer authenticates with GitHub over the holospaces
 * network, the way `gh auth login` does: the OAuth 2.0 Device Authorization
 * Grant (RFC 8628). A freestanding RV64 program (no libc, raw syscalls): it
 * POSTs to the device-code endpoint, then polls the token endpoint until it
 * gets an access token — all over the guest's virtio-net interface (the CC-16
 * userspace NAT + tunnelled egress). holospaces carries the bytes content-blind;
 * the token lives here, in the devcontainer, never in holospaces. */
static long ecall(long n,long a,long b,long c,long d,long e){
  register long a7 asm("a7")=n,a0 asm("a0")=a,a1 asm("a1")=b,a2 asm("a2")=c,a3 asm("a3")=d,a4 asm("a4")=e;
  asm volatile("ecall":"+r"(a0):"r"(a7),"r"(a1),"r"(a2),"r"(a3),"r"(a4):"memory");return a0;
}
#define SYS_close 57
#define SYS_write 64
#define SYS_read 63
#define SYS_socket 198
#define SYS_connect 203
#define SYS_reboot 142
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void put(const char*s){ecall(SYS_write,1,(long)s,slen(s),0,0);}
static void putn(const char*s,int n){ecall(SYS_write,1,(long)s,n,0,0);}
static int seq(const char*a,const char*b,int n){for(int i=0;i<n;i++)if(a[i]!=b[i])return 0;return 1;}
/* find substring `pat` in [buf,buf+n); return index or -1 */
static int find(const char*buf,int n,const char*pat){
  int pl=slen(pat); for(int i=0;i+pl<=n;i++)if(seq(buf+i,pat,pl))return i; return -1;
}
/* extract the JSON string value of "key":"<value>" into out; return length */
static int extract(const char*buf,int n,const char*key,char*out,int omax){
  int i=find(buf,n,key); if(i<0)return -1; i+=slen(key);
  /* skip `":"` */ while(i<n&&(buf[i]==':'||buf[i]=='"'||buf[i]==' '))i++;
  int j=0; while(i<n&&buf[i]!='"'&&j<omax-1)out[j++]=buf[i++]; out[j]=0; return j;
}
/* one HTTP/1.0 POST to 10.0.2.9:80; returns bytes read into buf */
static int post(const char*path,const char*body,char*buf,int buflen){
  unsigned char sa[16]={0}; sa[0]=2; sa[2]=0;sa[3]=80; sa[4]=10;sa[5]=0;sa[6]=2;sa[7]=9;
  long fd=ecall(SYS_socket,2,1,0,0,0); if(fd<0)return -1;
  if(ecall(SYS_connect,fd,(long)sa,16,0,0)<0){ecall(SYS_close,fd,0,0,0,0);return -1;}
  char req[512]; int p=0;
  const char*h1="POST "; for(const char*c=h1;*c;c++)req[p++]=*c;
  for(const char*c=path;*c;c++)req[p++]=*c;
  const char*h2=" HTTP/1.0\r\nHost: github.com\r\nAccept: application/json\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: ";
  for(const char*c=h2;*c;c++)req[p++]=*c;
  int bl=slen(body); /* itoa */ char num[12]; int ni=0; int t=bl; if(t==0)num[ni++]='0'; char tmp[12]; int ti=0; while(t){tmp[ti++]='0'+t%10;t/=10;} while(ti)num[ni++]=tmp[--ti]; num[ni]=0;
  for(int k=0;k<ni;k++)req[p++]=num[k];
  const char*h3="\r\n\r\n"; for(const char*c=h3;*c;c++)req[p++]=*c;
  for(const char*c=body;*c;c++)req[p++]=*c;
  ecall(SYS_write,fd,(long)req,p,0,0);
  int n=ecall(SYS_read,fd,(long)buf,buflen,0,0);
  ecall(SYS_close,fd,0,0,0,0);
  return n>0?n:0;
}
void _start(void){
  char buf[1024]; char devcode[128]; char token[128];
  /* 1) device authorization request */
  int n=post("/login/device/code","client_id=holospaces-cli&scope=repo",buf,sizeof buf);
  if(n<=0){put("AUTH-NET-FAIL\n");goto off;}
  if(extract(buf,n,"device_code",devcode,sizeof devcode)<0){put("AUTH-NO-DEVICECODE\n");goto off;}
  put("DEVICE-CODE-OK\n");
  /* the user_code the user would enter at the verification_uri */
  char ucode[64]; if(extract(buf,n,"user_code",ucode,sizeof ucode)>0){put("USER-CODE:");put(ucode);put("\n");}
  /* 2) poll the token endpoint (RFC 8628 §3.4) */
  char body[256]; int bp=0; const char*b1="client_id=holospaces-cli&grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code=";
  for(const char*c=b1;*c;c++)body[bp++]=*c; for(int k=0;devcode[k];k++)body[bp++]=devcode[k]; body[bp]=0;
  for(int attempt=0;attempt<6;attempt++){
    n=post("/login/oauth/access_token",body,buf,sizeof buf);
    if(n>0&&extract(buf,n,"access_token",token,sizeof token)>0){put("AUTH-OK:");put(token);put("\n");goto off;}
    if(n>0&&find(buf,n,"authorization_pending")>=0){put("POLL-PENDING\n");continue;}
    put("AUTH-POLL-FAIL\n");goto off;
  }
  put("AUTH-TIMEOUT\n");
off:
  ecall(SYS_reboot,0xfee1dead,0x28121969,0x4321fedc,0,0);for(;;);
}
