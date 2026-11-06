/* Rank 13: one primitive per invocation. Native data is disposable.
 * Build standalone for C leaves; -DC13_RUST_BRIDGE for the Rust API leaf.
 * Exit 9 means exact EBADF characterization, 0 positive control;
 * 70 is instrumentation failure, 71 unexpected primitive result.
 */
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef C13_RUST_BRIDGE
extern int c13_rust_lock(int, int *, int *);
#endif
static int transfer(int fd, void *buf, size_t size, int writing) {
  size_t at = 0;
  while (at < size) {
    ssize_t n = writing ? write(fd, (char *)buf + at, size-at)
                        : read(fd, (char *)buf + at, size-at);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) return -1;
    at += (size_t)n;
  }
  return 0;
}
static int emit(const char *root, const char *name, const char *bytes) {
  char path[4096];
  int n = snprintf(path, sizeof path, "%s/%s", root, name);
  if (n < 0 || (size_t)n >= sizeof path) return -1;
  int fd = open(path, O_WRONLY|O_CREAT, 0600);
  if (fd < 0) return -1;
  int result = transfer(fd, (void *)bytes, strlen(bytes), 1);
  int closed = close(fd);
  return result || closed ? -1 : 0;
}
static int pipe_round(int *pair, const char *nonce, const char *phase) {
  char sent[128], got[128];
  int n = snprintf(sent, sizeof sent, "C13-PIPE %s %s\n", nonce, phase);
  if (n <= 0 || n >= (int)sizeof sent) return -1;
  if (transfer(pair[1], sent, n, 1) || transfer(pair[0], got, n, 0)) return -1;
  return memcmp(sent, got, n) ? -1 : 0;
}
int c13_main(int argc, const char **argv) {
  if (argc != 5) return 70;
  const char *id=argv[1], *root=argv[2], *mode=argv[3], *nonce=argv[4];
  if (strlen(nonce)!=32 || strspn(nonce,"0123456789abcdef")!=32) return 70;
  int native=!strcmp(mode,"native");
  if (!native && strcmp(mode,"routed")) return 70;
  int which=0;
  for (int i=1;i<=5;i++) { char expected[16]; snprintf(expected,sizeof expected,"C13-%02d",i); if (!strcmp(id,expected)) which=i; }
  if (!which) return 70;
  long page=getpagesize();
  int fd=-1, pair[2]={-1,-1}, error=0, result=0, unlocked=-1, unmapped=-1;
  const char *memory="null";
  {
    char path[4096]; snprintf(path,sizeof path,"%s/seed.txt",root);
    fd=open(path,O_RDWR);
    if (fd<0) return 70;
    unsigned char *seed=malloc(page);
    if (!seed || transfer(fd,seed,page,0)) return 70;
    for (long i=0;i<page;i++) if (seed[i]!=0x53) return 70;
    free(seed);
    if (which==5) { if (close(fd)) return 70; fd=-1; }
    else if (pipe(pair) || pipe_round(pair,nonce,"before")) return 70;
  }
  const char *operation=which==1?"File::try_lock":which==2?"flock":which==3?"F_SETLK":which==4?"mmap_shared":"mmap_anon";
  const char *answer="0";
  if (which==1) {
#ifdef C13_RUST_BRIDGE
    result=c13_rust_lock(fd,&error,&unlocked);
#else
    return 70;
#endif
    answer=result?"os_error":"ok";
  } else if (which==2) {
    result=flock(fd,LOCK_EX|LOCK_NB); if (result) error=errno;
    else unlocked=flock(fd,LOCK_UN);
    answer=result?"-1":"0";
  } else if (which==3) {
    struct flock lock={0}; lock.l_type=F_WRLCK; lock.l_whence=SEEK_SET; lock.l_len=1;
    result=fcntl(fd,F_SETLK,&lock); if (result) error=errno;
    else { lock.l_type=F_UNLCK; unlocked=fcntl(fd,F_SETLK,&lock); }
    answer=result?"-1":"0";
  } else {
    void *map=mmap(NULL,page,PROT_READ|PROT_WRITE,which==4?MAP_SHARED:MAP_ANON|MAP_PRIVATE,fd,0);
    if (map==MAP_FAILED) { result=-1; error=errno; answer="MAP_FAILED"; }
    else {
      answer="mapped";
      if (native || which==5) {
        memcpy(map,"C13!",4); memory=memcmp(map,"C13!",4)?"false":"true";
      }
      unmapped=munmap(map,page);
    }
  }
  char pipes[512]="null", fdtext[32]="null", errtext[32]="null";
  int closed=0;
  if (which!=5) {
    if (pipe_round(pair,nonce,"after")) return 70;
    int wc=close(pair[1]); char b; ssize_t eof;
    do { eof=read(pair[0],&b,1); } while (eof<0 && errno==EINTR);
    int rc=close(pair[0]); closed=close(fd);
    if (wc || rc || eof!=0 || closed) return 70;
    snprintf(pipes,sizeof pipes,"{\"read_fd\":%d,\"write_fd\":%d,\"before\":\"C13-PIPE %s before\\n\",\"after\":\"C13-PIPE %s after\\n\",\"eof\":true,\"writer_close\":0,\"reader_close\":0}",pair[0],pair[1],nonce,nonce);
    snprintf(fdtext,sizeof fdtext,"%d",fd);
  }
  if (result) snprintf(errtext,sizeof errtext,"%d",error);
  int expected_exit=native||which==5?0:9;
  int good=native||which==5 ? result==0 : result==-1 && error==EBADF;
  if (native && which<=3 && unlocked!=0) good=0;
  if ((native && which==4)||which==5) if (unmapped!=0 || strcmp(memory,"true")) good=0;
  char sentinel[160], report[2048];
  snprintf(sentinel,sizeof sentinel,"C13-SENTINEL %s %s %s\n",id,mode,nonce);
  if (emit(root,"c13-sentinel.txt",sentinel)) return 70;
  snprintf(report,sizeof report,"{\"version\":1,\"case\":\"%s\",\"mode\":\"%s\",\"nonce\":\"%s\",\"pid\":%d,\"phase\":\"complete\",\"probe_fd\":%s,\"seed_read_ok\":%s,\"operation\":\"%s\",\"result\":\"%s\",\"error\":%s,\"memory_check\":%s,\"unlock\":%s,\"unmap\":%s,\"probe_close\":%s,\"pipe\":%s,\"sentinel_written\":true,\"intended_exit\":%d}\n",id,mode,nonce,getpid(),fdtext,which==5?"null":"true",operation,answer,errtext,memory,unlocked==0?"0":"null",unmapped==0?"0":"null",which==5?"null":"0",pipes,expected_exit);
  if (emit(root,"c13-report.json",report)) return 70;
  return good?expected_exit:71;
}
#ifndef C13_RUST_BRIDGE
int main(int argc, char **argv) { return c13_main(argc,(const char **)argv); }
#endif
