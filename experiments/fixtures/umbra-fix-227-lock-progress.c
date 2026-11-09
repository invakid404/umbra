/* FT-4 fallback: sibling progress, NOT proof of a kernel lock wait.
 * A generic waiting-state observation gates B; it cannot distinguish debugger
 * suspension. The no-fcntl-stubs test and ABI controls are separate obligations.
 * helper/target share a pre-mapped host file; neither waits on a traced pipe. */
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/proc_info.h>
#include <time.h>
#include <unistd.h>

struct shared {
    _Atomic int ready, attempted, allow, request, released, returned, result;
    _Atomic int pid;
    _Atomic uint64_t tid;
    _Atomic unsigned counter;
};
static struct shared *s;
static int lockfd;
static void *a(void *unused) {
    (void)unused;
    uint64_t tid;
    pthread_threadid_np(NULL, &tid);
    s->tid = tid;
    s->pid = getpid();
    struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
    s->attempted = 1;
    int rc = fcntl(lockfd, F_SETLKW, &lock);
    s->result = rc;
    s->returned = 1;
    return NULL;
}
static void *b(void *unused) {
    (void)unused;
    while (!s->allow) {}
    atomic_fetch_add(&s->counter, 1);
    s->request = 1;
    return NULL;
}
int main(int argc, char **argv) {
    if (argc != 5) return 10;
    int helper = !strcmp(argv[1], "helper");
    char shared_path[4096];
    const char *mapping_path = argv[2];
    if (!helper) {
        struct timespec delay = {.tv_nsec = 10000000};
        int found = 0;
        for (int i = 0; i < 1000; i++) {
            int locator = open(argv[2], O_RDONLY);
            if (locator >= 0) {
                ssize_t n = read(locator, shared_path, sizeof(shared_path) - 1);
                close(locator);
                if (n <= 0) return 22;
                shared_path[n] = 0;
                found = 1;
                break;
            }
            if (errno != ENOENT) return 23;
            nanosleep(&delay, NULL);
        }
        if (!found) return 24;
        mapping_path = shared_path;
    }
    int mapping = open(mapping_path, O_RDWR);
    if (mapping < 0) return 11;
    s = mmap(NULL, sizeof(*s), PROT_READ | PROT_WRITE, MAP_SHARED, mapping, 0);
    if (s == MAP_FAILED || close(mapping)) return 12;
    lockfd = open(argv[3], helper ? O_RDWR : O_RDONLY);
    if (lockfd < 0) return 13;
    if (helper) {
        struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
        if (fcntl(lockfd, F_SETLK, &lock)) return 14;
        s->ready = 1;
        struct timespec delay = {.tv_nsec = 10000000};
        int observed = 0;
        for (int i = 0; i < 3000; i++) {
            if (s->attempted && !s->allow) {
                struct proc_threadinfo info = {0};
                int n = proc_pidinfo(s->pid, PROC_PIDTHREADID64INFO, s->tid, &info, sizeof(info));
                if (n == sizeof(info) && info.pth_run_state == TH_STATE_WAITING) {
                    fprintf(stderr, "NOT_KERNEL_WAIT_PROOF pid=%d tid=%llu state=%d flags=%d\n",
                            s->pid, (unsigned long long)s->tid, info.pth_run_state, info.pth_flags);
                    observed = 1;
                    s->allow = 1;
                }
            }
            if (s->request) break;
            nanosleep(&delay, NULL);
        }
        int valid = observed && s->request && s->counter == 1 && !s->returned;
        // Publish release permission before unlocking, so A may immediately run.
        s->released = 1;
        lock.l_type = F_UNLCK;
        int rc = fcntl(lockfd, F_SETLK, &lock);
        fprintf(stderr, "helper observed=%d request=%d counter=%u valid=%d unlock=%d\n",
                observed, s->request, s->counter, valid, rc);
        return valid && rc == 0 && close(lockfd) == 0 ? 0 : 15;
    }
    while (!s->ready) {}
    pthread_t first, second;
    if (pthread_create(&first, NULL, a, NULL) || pthread_create(&second, NULL, b, NULL))
        return 16;
    if (pthread_join(first, NULL) || pthread_join(second, NULL)) return 17;
    if (!s->request || !s->released || s->result || s->counter != 1) return 18;
    if (close(lockfd)) return 19;
    int out = open(argv[4], O_CREAT | O_EXCL | O_WRONLY, 0600);
    const char record[] = "NOT_KERNEL_WAIT_PROOF sibling=1 request=1 released=1 result=0 joined=2\n";
    if (out < 0 || write(out, record, sizeof(record) - 1) != sizeof(record) - 1) return 20;
    return close(out) == 0 ? 0 : 21;
}
