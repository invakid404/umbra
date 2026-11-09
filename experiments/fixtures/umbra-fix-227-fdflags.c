/* Public fcntl descriptor queries. The harness runs native controls first.
 * "refusals" intentionally has different native and routed expectations.
 * Inheritance and threaded ordering are separate cases in the harness. */
#include <errno.h>
#include <stdarg.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/wait.h>

extern int __close_nocancel(int);

static char records[4096], record_path[4096];
static size_t used;
static void note(const char *format, ...) {
    va_list args; va_start(args, format);
    int n = vsnprintf(records + used, sizeof(records) - used, format, args);
    va_end(args);
    if (n < 0 || (size_t)n >= sizeof(records) - used) _exit(70);
    used += (size_t)n;
}
static void flush_records(void) {
    int out = open(record_path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (out < 0) _exit(71);
    size_t done = 0;
    while (done < used) {
        ssize_t n = write(out, records + done, used - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) _exit(72);
        done += (size_t)n;
    }
    if (close(out)) _exit(73);
}

static void result(const char *name, int value) {
    int error = value < 0 ? errno : 0;
    note("%s result=%d errno=%d\n", name, value, error);
}

static _Atomic int phase;
static int shared_fd;
static void *close_thread(void *unused) {
    (void)unused;
    while (atomic_load(&phase) != 1) {}
    int rc = close(shared_fd);
    atomic_store(&phase, rc == 0 ? 2 : 3);
    return NULL;
}

static int exec_child(int fd, int swept) {
    char bytes[3];
    if (fcntl(fd, F_GETFD) != 0) return 31;
    errno = 0;
    if (fcntl(swept, F_GETFD) != -1 || errno != EBADF) return 32;
    errno = 0;
    if (write(swept, "BAD", 3) != -1 || errno != EBADF) return 33;
    if (read(fd, bytes, 3) != 3 || memcmp(bytes, "ABC", 3)) return 34;
    if (write(fd, "D", 1) != 1) return 35;
    return close(fd) == 0 ? 0 : 36;
}

int main(int argc, char **argv) {
    if (argc == 4 && !strcmp(argv[1], "exec-child"))
        return exec_child(atoi(argv[2]), atoi(argv[3]));
    if (argc != 3) return 10;
    if (snprintf(record_path, sizeof(record_path), "%s.records", argv[2]) >= (int)sizeof(record_path)) return 74;
    if (atexit(flush_records)) return 75;
    int fd = open(argv[2], O_CREAT | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) return 11;
    if (!strcmp(argv[1], "flags")) {
        result("cloexec", fcntl(fd, F_GETFD));
        if (close(fd)) return 12;
        fd = open(argv[2], O_RDWR);
        if (fd < 0) return 13;
        result("inherited", fcntl(fd, F_GETFD));
    } else if (!strcmp(argv[1], "closed")) {
        if (close(fd)) return 14;
        result("closed", fcntl(fd, F_GETFD));
        result("unissued", fcntl(1000000, F_GETFD));
        return 0;
    } else if (!strcmp(argv[1], "refusals")) {
        result("getfl", fcntl(fd, F_GETFL));
        result("setfd", fcntl(fd, F_SETFD, FD_CLOEXEC));
        int copy = fcntl(fd, F_DUPFD, 0);
        result("dupfd", copy);
        if (copy >= 0 && close(copy)) return 15;
        copy = fcntl(fd, F_DUPFD_CLOEXEC, 0);
        result("dupfd-cloexec", copy);
        if (copy >= 0 && close(copy)) return 16;
    } else if (!strcmp(argv[1], "close")) {
        int original = fd;
        result("close", close(fd));
        result("double-close", close(fd));
        fd = open(argv[2], O_RDWR);
        if (fd < 0) return 17;
        note("reuse-plain equal=%d\n", fd == original);
        result("close-nocancel", __close_nocancel(fd));
        result("double-nocancel", __close_nocancel(fd));
        fd = open(argv[2], O_RDWR);
        if (fd < 0) return 18;
        note("reuse-nocancel equal=%d\n", fd == original);
    } else if (!strcmp(argv[1], "thread")) {
        pthread_t thread;
        shared_fd = fd;
        if (pthread_create(&thread, NULL, close_thread, NULL)) return 40;
        result("thread-live", fcntl(fd, F_GETFD));
        atomic_store(&phase, 1);
        while (atomic_load(&phase) == 1) {}
        if (pthread_join(thread, NULL) || atomic_load(&phase) != 2) return 41;
        result("thread-closed", fcntl(fd, F_GETFD));
        return 0;
    } else if (!strcmp(argv[1], "fork") || !strcmp(argv[1], "exec")) {
        if (write(fd, "ABC", 3) != 3) return 50;
        int live = open(argv[2], O_RDWR);
        if (live < 0) return 51;
        int ready[2], release[2];
        if (pipe(ready) || pipe(release)) return 52;
        char live_arg[24], swept_arg[24];
        snprintf(live_arg, sizeof(live_arg), "%d", live);
        snprintf(swept_arg, sizeof(swept_arg), "%d", fd);
        pid_t child = fork();
        if (child < 0) return 53;
        if (child == 0) {
            close(ready[0]); close(release[1]);
            char token;
            if (write(ready[1], "R", 1) != 1 || read(release[0], &token, 1) != 1)
                _exit(54);
            close(ready[1]); close(release[0]);
            if (!strcmp(argv[1], "exec")) {
                execl(argv[0], argv[0], "exec-child", live_arg, swept_arg, NULL);
                _exit(55);
            }
            if (fcntl(fd, F_GETFD) != FD_CLOEXEC || close(fd)) _exit(56);
            char bytes[3];
            if (read(live, bytes, 3) != 3 || memcmp(bytes, "ABC", 3)) _exit(57);
            if (write(live, "D", 1) != 1 || close(live)) _exit(58);
            _exit(0);
        }
        close(ready[1]); close(release[0]);
        char token;
        if (read(ready[0], &token, 1) != 1 || token != 'R') return 59;
        if (write(release[1], "G", 1) != 1) return 60;
        close(ready[0]); close(release[1]);
        int status;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
            return 61;
        if (fcntl(fd, F_GETFD) != FD_CLOEXEC || fcntl(live, F_GETFD) != 0) return 62;
        if (write(live, "E", 1) != 1) return 63;
        char tail[2];
        if (read(fd, tail, 2) != 2 || memcmp(tail, "DE", 2)) return 64;
        if (close(live)) return 65;
        int verify = open(argv[2], O_RDONLY);
        char all[5];
        if (verify < 0 || read(verify, all, 5) != 5 || memcmp(all, "ABCDE", 5)) return 66;
        if (close(verify)) return 67;
        note("%s child=0 parent-cloexec=1 parent-live=0 bytes=ABCDE\n", argv[1]);
    } else {
        return 19;
    }
    return close(fd) == 0 ? 0 : 20;
}
