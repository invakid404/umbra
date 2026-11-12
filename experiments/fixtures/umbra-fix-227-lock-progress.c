/* FT-4 fallback: sibling progress, NOT proof of a kernel lock wait.
 * A generic waiting-state observation gates B; it cannot distinguish debugger
 * suspension. The no-fcntl-stubs test and ABI controls are separate obligations.
 * helper/target share a pre-mapped host file; neither waits on a traced pipe. */
#include <errno.h>
#include <crt_externs.h>
#include <fcntl.h>
#include <libproc.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <mach/mach_time.h>
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
    fprintf(stderr, "thread A entering fcntl fd=%d\n", lockfd);
    s->attempted = 1;
    int rc = fcntl(lockfd, F_SETLKW, &lock);
    s->result = rc;
    s->returned = 1;
    fprintf(stderr, "thread A returned rc=%d errno=%d\n", rc, errno);
    return NULL;
}
static void *b(void *unused) {
    (void)unused;
    while (!s->allow) {}
    atomic_fetch_add(&s->counter, 1);
    s->request = 1;
    return NULL;
}
static int setup(int argc, char **argv) {
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
    struct stat mapping_stat;
    if (fstat(mapping, &mapping_stat)) { close(mapping); return 25; }
    if (mapping_stat.st_size < (off_t)sizeof(*s)) {
        fprintf(stderr, "shared mapping too short: %lld bytes\n", (long long)mapping_stat.st_size);
        close(mapping);
        return 26;
    }
    s = mmap(NULL, sizeof(*s), PROT_READ | PROT_WRITE, MAP_SHARED, mapping, 0);
    if (s == MAP_FAILED || close(mapping)) return 12;
    lockfd = open(argv[3], helper ? O_RDWR : O_RDONLY);
    if (lockfd < 0) return 13;
    return 0;
}
static int setup_result;
/* Map shared state and open the host lock before supervisor arming. */
__attribute__((constructor)) static void prepare_host_descriptors(void) {
    setup_result = setup(*_NSGetArgc(), *_NSGetArgv());
}
int main(int argc, char **argv) {
    if (setup_result) { fprintf(stderr, "setup result=%d errno=%d\n", setup_result, errno); return setup_result; }
    if (argc != 5) return 10;
    int helper = !strcmp(argv[1], "helper");
    if (helper) {
        struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
        if (fcntl(lockfd, F_SETLK, &lock)) return 14;
        fprintf(stderr, "helper acquired lock pid=%d mapping=%s\n", getpid(), argv[2]);
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
    fprintf(stderr, "target mapped pid=%d\n", getpid());
    // Eight seconds allows helper launch/scheduling after copy-up, while staying
    // below the 12 s provider deadline and 20 s harness watchdog. Read the user
    // clock only every 4096 spins; no trapped syscall enters the ready loop.
    mach_timebase_info_data_t timebase;
    if (mach_timebase_info(&timebase) != KERN_SUCCESS) return 27;
    uint64_t ready_start = mach_absolute_time();
    uint64_t ready_ticks = 8000000000ULL * timebase.denom / timebase.numer;
    unsigned spins = 0;
    while (!s->ready) {
        if ((++spins & 4095) == 0 && mach_absolute_time() - ready_start >= ready_ticks) {
            fprintf(stderr, "target helper never ready\n");
            return 28;
        }
    }
    fprintf(stderr, "target helper ready\n");
    pthread_t first, second;
    if (pthread_create(&first, NULL, a, NULL) || pthread_create(&second, NULL, b, NULL))
        return 16;
    if (pthread_join(first, NULL) || pthread_join(second, NULL)) return 17;
    if (!s->request || !s->released || s->result || s->counter != 1) return 18;
    if (close(lockfd)) return 19;
    int out = open(argv[4], O_CREAT | O_TRUNC | O_WRONLY, 0600);
    const char record[] = "NOT_KERNEL_WAIT_PROOF sibling=1 request=1 released=1 result=0 joined=2\n";
    if (out < 0 || write(out, record, sizeof(record) - 1) != sizeof(record) - 1) return 20;
    return close(out) == 0 ? 0 : 21;
}
