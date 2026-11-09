/* Kernel-descriptor ABI controls and bounded cost readings.
 * argv: host-file record-file. The host file is outside the routed workspace.
 * Fixed preregistered count: 16 calls per timing loop; startup is excluded. */
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static char records[8192];
static size_t used;
static void note(const char *format, ...) {
    va_list args;
    va_start(args, format);
    int n = vsnprintf(records + used, sizeof(records) - used, format, args);
    va_end(args);
    if (n < 0 || (size_t)n >= sizeof(records) - used) exit(10);
    used += (size_t)n;
}
static void record(const char *name, int rc) {
    note("%s result=%d errno=%d\n", name, rc, errno);
}
static unsigned long long now(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) exit(11);
    return (unsigned long long)t.tv_sec * 1000000000ULL + t.tv_nsec;
}
int main(int argc, char **argv) {
    if (argc != 3) return 12;
    int fd = open(argv[1], O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 13;
    errno = 0; record("getfd", fcntl(fd, F_GETFD));
    errno = 0; record("getfl", fcntl(fd, F_GETFL));
    errno = 0; record("setfd-clear", fcntl(fd, F_SETFD, 0));
    errno = 0; record("getfd-clear", fcntl(fd, F_GETFD));
    errno = 0; record("setfd-set", fcntl(fd, F_SETFD, FD_CLOEXEC));
    errno = 0; record("getfd-set", fcntl(fd, F_GETFD));
    errno = 0; record("setfl", fcntl(fd, F_SETFL, O_NONBLOCK));
    errno = 0; record("getfl-nonblock", fcntl(fd, F_GETFL));
    errno = 0;
    int copy = fcntl(fd, F_DUPFD, 100);
    record("dupfd", copy);
    if (copy < 0 || close(copy)) return 14;
    char path[4096] = {0};
    errno = 0; record("getpath", fcntl(fd, F_GETPATH, path));
    note("path=%s\n", path);
    if (strcmp(path, argv[1])) return 15;
    struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
    errno = 0; record("setlk", fcntl(fd, F_SETLK, &lock));
    lock.l_type = F_UNLCK;
    errno = 0; record("unlock", fcntl(fd, F_SETLK, &lock));
    errno = 0; record("unknown", fcntl(fd, 0x7ffffffe, 0));
    unsigned long long start = now();
    for (int i = 0; i < 16; i++) if (fcntl(fd, F_GETFD) != FD_CLOEXEC) return 16;
    unsigned long long elapsed = now() - start;
    note("cost getfd calls=16 elapsed_ns=%llu\n", elapsed);
    struct stat stat;
    start = now();
    for (int i = 0; i < 16; i++) if (fstat(fd, &stat)) return 17;
    elapsed = now() - start;
    note("cost fstat calls=16 elapsed_ns=%llu\n", elapsed);
    if (close(fd)) return 18;
    int out = open(argv[2], O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (out < 0) return 19;
    size_t done = 0;
    while (done < used) {
        ssize_t n = write(out, records + done, used - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 20;
        done += (size_t)n;
    }
    return close(out) == 0 ? 0 : 21;
}
