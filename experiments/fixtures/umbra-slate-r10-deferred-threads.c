/* Rank-10 deferred leaves. argv: case, seeded input, output.
 * Records use inherited stderr. C10-13 releases its child after poll/join.
 * No sleeps establish ordering. The test launcher owns the deadline. */
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int report_fd = 2;
static unsigned long long tid(void) {
    uint64_t t; assert(pthread_threadid_np(NULL, &t) == 0); return t;
}
static void bytes(int fd, const char *p, size_t n) {
    while (n) { ssize_t r = write(fd, p, n); if (r < 0 && errno == EINTR) continue;
        assert(r > 0); p += r; n -= (size_t)r; }
}
static void note(const char *fmt, ...) {
    char b[512]; va_list ap; va_start(ap, fmt);
    int n = vsnprintf(b, sizeof b, fmt, ap); va_end(ap);
    assert(n >= 0 && (size_t)n < sizeof b); bytes(report_fd, b, (size_t)n);
}
static char token(int fd) {
    char c; ssize_t n; do n = read(fd, &c, 1); while (n < 0 && errno == EINTR);
    assert(n == 1); return c;
}
static void transfer(int fd, const char *payload) {
    size_t done = 0, len = strlen(payload);
    while (done < len) {
        errno = 0; ssize_t n = write(fd, payload + done, len - done); int e = errno;
        note("write tid=%llu fd=%d result=%zd errno=%d\n", tid(), fd, n, e);
        if (n < 0 && e == EINTR) continue;
        assert(n > 0); done += (size_t)n;
    }
}
static int opened(const char *p, int flags) {
    errno = 0; int fd = open(p, flags, 0600); int e = errno;
    note("open tid=%llu fd=%d errno=%d\n", tid(), fd, e); assert(fd >= 0); return fd;
}
static void closed(int fd) {
    errno = 0; int r = close(fd); int e = errno;
    note("close tid=%llu fd=%d result=%d errno=%d\n", tid(), fd, r, e); assert(r == 0);
}
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int phase, mode, shared;
static unsigned long long helper_tid;
static int parked, leaving;
static const char *input;
static void publish(int p) {
    assert(pthread_mutex_lock(&mu) == 0); phase = p;
    assert(pthread_cond_broadcast(&cv) == 0); assert(pthread_mutex_unlock(&mu) == 0);
}
static void await(int p) {
    assert(pthread_mutex_lock(&mu) == 0);
    while (phase < p) assert(pthread_cond_wait(&cv, &mu) == 0);
    assert(pthread_mutex_unlock(&mu) == 0);
}
static void *helper(void *unused) {
    (void)unused;
    int fd = -1;
    if (mode == 7) fd = opened(input, O_WRONLY);
    if (mode == 13) {
        assert(pthread_mutex_lock(&mu) == 0);
        helper_tid = tid(); parked = 1; phase = 1;
        note("helper-ready tid=%llu parked=1 leaving=0\n", helper_tid);
        assert(pthread_cond_broadcast(&cv) == 0);
        while (phase < 2) assert(pthread_cond_wait(&cv, &mu) == 0);
        leaving = 1; parked = 0;
        note("helper-leaving tid=%llu parked=0 leaving=1\n", helper_tid);
        assert(pthread_mutex_unlock(&mu) == 0);
    } else { note("helper-ready tid=%llu\n", tid()); publish(1); await(2); }
    if (mode == 7) { transfer(fd, "B"); closed(fd); }
    if (mode == 8) closed(shared);
    publish(3); return NULL;
}
static pthread_t start(void) {
    pthread_t h; assert(pthread_create(&h, NULL, helper, NULL) == 0); await(1); return h;
}
static void finish(pthread_t h) {
    publish(2); assert(pthread_join(h, NULL) == 0); note("helper-joined tid=%llu\n", tid());
}
int main(int argc, char **argv) {
    assert(argc == 4); mode = atoi(argv[1]); input = argv[2];

    note("case=%d main_tid=%llu\n", mode, tid());
    if (mode == 7) {
        shared = opened(input, O_WRONLY); pthread_t h = start();
        transfer(shared, "AA"); finish(h); closed(shared);
    } else if (mode == 8) {
        shared = opened(input, O_RDONLY); pthread_t h = start(); finish(h);
        char b; errno = 0; ssize_t r = read(shared, &b, 1); int e = errno;
        note("stale-read tid=%llu fd=%d result=%zd errno=%d\n", tid(), shared, r, e);
        assert(r == -1 && e == EBADF);
        int fresh = opened(argv[3], O_WRONLY | O_CREAT | O_EXCL);
        note("reuse old=%d fresh=%d observed=%d\n", shared, fresh, shared == fresh);
        transfer(fresh, "FRESH"); closed(fresh);
    } else if (mode == 11) {
        pthread_t h = start(); finish(h);
        pid_t child = fork(); assert(child >= 0);
        if (!child) {
            int fd = open(argv[3], O_WRONLY | O_CREAT | O_EXCL, 0600);
            if (fd < 0) _exit(71);
            const char *s = "CHILD"; size_t left = 5;
            while (left) { ssize_t n = write(fd, s, left);
                if (n < 0 && errno == EINTR) continue;
                if (n <= 0) _exit(72); s += n; left -= (size_t)n; }
            if (close(fd)) _exit(73); _exit(0);
        }
        int st = -1; pid_t r = waitpid(child, &st, 0);
        note("reap child=%d result=%d status=%d\n", child, r, st);
        assert(r == child && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    } else if (mode == 13) {
        int ready[2], go[2]; assert(pipe(ready) == 0 && pipe(go) == 0);
        pid_t child = fork(); assert(child >= 0);
        if (!child) {
            close(0); close(ready[0]); close(go[1]);
            if (write(ready[1], "R", 1) != 1) _exit(81); close(ready[1]);
            char c; ssize_t n; do n = read(go[0], &c, 1); while (n < 0 && errno == EINTR);
            close(go[0]);
            if (n != 1 || c != 'G') _exit(82);
            static const char sentinel[] = "c10-13 child-post-release=CHILD-G\n";
            if (write(2, sentinel, sizeof sentinel - 1) != sizeof sentinel - 1) _exit(83);
            _exit(0);
        }
        close(ready[1]); close(go[0]); assert(token(ready[0]) == 'R'); close(ready[0]);
        note("c10-13 ready child=%d\n", child);
        pthread_t h = start(); int st = 0x5a5a5a5a;
        assert(pthread_mutex_lock(&mu) == 0);
        assert(parked == 1 && leaving == 0 && phase == 1);
        note("c10-13 pre-poll helper_tid=%llu parked=%d leaving=%d phase=%d\n", helper_tid, parked, leaving, phase);
        assert(pthread_mutex_unlock(&mu) == 0);
        errno = 0; pid_t polled = waitpid(child, &st, WNOHANG); int e = errno;
        note("c10-13 poll child=%d result=%d errno=%d status=%d\n", child, polled, e, st);
        finish(h); note("c10-13 helper-joined\n");
        note("c10-13 release=G caller_tid=%llu\n", tid());
        bytes(go[1], "G", 1); close(go[1]);
        int final = -1; pid_t r = waitpid(child, &final, 0);
        note("reap child=%d result=%d status=%d\n", child, r, final);
        assert(polled == 0 && st == 0x5a5a5a5a);
        assert(r == child && WIFEXITED(final) && WEXITSTATUS(final) == 0);
    } else return 2;
    note("complete case=%d\n", mode); return 0;
}
