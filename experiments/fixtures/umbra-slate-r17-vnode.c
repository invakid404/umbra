/* C17-03: one thread, receipt before mutation, raw result channel. */
#include <sys/event.h>
#include <sys/time.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <assert.h>
static int report_fd;
static unsigned seq;
static const char *nonce;
static unsigned long long micros(void) {
    struct timespec t; assert(clock_gettime(CLOCK_MONOTONIC, &t)==0);
    return (unsigned long long)t.tv_sec*1000000 + t.tv_nsec/1000;
}
static void raw(int fd, const char *s, size_t n) {
    while(n) { ssize_t k=write(fd,s,n); if(k<0 && errno==EINTR) continue;
        assert(k>0); s+=k; n-=(size_t)k; }
}
static void record(const char *phase,const char *data) {
    char b[8192],header[32];
    int n=snprintf(b,sizeof b,"{\"version\":1,\"seq\":%u,\"nonce\":\"%s\",\"pid\":%d,\"us\":%llu,\"phase\":\"%s\",\"data\":%s}",seq++,nonce,getpid(),micros(),phase,data);
    assert(n>0 && (size_t)n<sizeof b);
    int h=snprintf(header,sizeof header,"%d:",n); raw(report_fd,header,h); raw(report_fd,b,n); raw(report_fd,"\n",1);
}
int main(int argc,char **argv) {
    assert(argc==4); nonce=argv[3];
    for(const char *p=nonce;*p;p++) assert((*p>='0'&&*p<='9')||(*p>='a'&&*p<='f'));
    char path[4096],work[4096],buf[8192];
    snprintf(path,sizeof path,"%s/evidence/result",argv[2]);
    report_fd=open(path,O_CREAT|O_EXCL|O_WRONLY,0600); assert(report_fd>=0);
    record("BEGIN","\"vnode\""); record("channel","\"C17-CHANNEL\"");
    snprintf(work,sizeof work,"%s/work",argv[2]);
    snprintf(path,sizeof path,"%s/work/seed",argv[2]);
    int fd=open(path,O_RDONLY); assert(fd>=0);
    char seed[8]={0}; assert(read(fd,seed,8)==4); assert(memcmp(seed,"SEED",4)==0);
    record("initial-read","\"SEED\"");
    int queue=kqueue(); assert(queue>=0); record("queue","true");
    struct kevent change,receipt,event;
    EV_SET(&change,fd,EVFILT_VNODE,EV_ADD|EV_CLEAR|EV_RECEIPT,NOTE_WRITE,0,NULL);
    memset(&receipt,0,sizeof receipt); errno=0;
    int n=kevent(queue,&change,1,&receipt,1,NULL); int saved=errno;
    snprintf(buf,sizeof buf,"{\"return\":%d,\"errno\":%d,\"flags\":%u,\"data\":%lld}",n,saved,receipt.flags,(long long)receipt.data);
    record("registration",buf);
    int admitted=n==1 && (receipt.flags&EV_ERROR) && receipt.data==0;
    if(admitted) {
        record("READY","true"); int writer=open(path,O_WRONLY|O_TRUNC); assert(writer>=0);
        raw(writer,nonce,strlen(nonce)); assert(close(writer)==0); record("mutation","true");
        record("observation-start","true"); struct timespec timeout={5,0};
        memset(&event,0,sizeof event); errno=0;
        n=kevent(queue,NULL,0,&event,1,&timeout); saved=errno;
        snprintf(buf,sizeof buf,"{\"return\":%d,\"errno\":%d,\"filter\":%d,\"fflags\":%u,\"ident\":%llu,\"expected_fd\":%d}",n,saved,event.filter,event.fflags,(unsigned long long)event.ident,fd);
        record("event",buf); record("observation-end","true");
    } else { record("mutation","\"NOT_APPLICABLE\""); record("observation","\"NOT_APPLICABLE\""); }
    assert(close(fd)==0); fd=open(path,O_RDONLY); assert(fd>=0);
    char bytes[256]={0}; n=(int)read(fd,bytes,sizeof bytes-1); assert(n>=0); assert(close(fd)==0);
    snprintf(buf,sizeof buf,"\"%s\"",bytes); record("read",buf);
    DIR *dir=opendir(work); assert(dir); unsigned count=0; struct dirent *entry;
    errno=0; while((entry=readdir(dir))) { if(strcmp(entry->d_name,".")&&strcmp(entry->d_name,"..")) { assert(strcmp(entry->d_name,"seed")==0); count++; } errno=0; }
    assert(errno==0); assert(closedir(dir)==0); assert(count==1); record("list","[\"seed\"]");
    assert(close(queue)==0); record("teardown","true");
    snprintf(buf,sizeof buf,"{\"records\":%u}",seq+1); record("COMPLETE",buf);
    return close(report_fd)==0 ? 0 : 9;
}
