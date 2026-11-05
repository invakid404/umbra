/* Rank 12: one focal namespace operation per invocation. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char src_bytes[] = "C12 complete source\n";
static const char dst_bytes[] = "C12 original destination, deliberately longer\n";
static int put(const char *path, const char *bytes, size_t size) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0) return -1;
    size_t pos = 0;
    while (pos < size) {
        ssize_t n = write(fd, bytes + pos, size - pos);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { close(fd); return -1; }
        pos += (size_t)n;
    }
    return close(fd);
}
static int contents(int fd, const char *want, size_t size) {
    char buf[256]; size_t pos = 0;
    for (;;) {
        ssize_t n = read(fd, buf + pos, sizeof(buf) - pos);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return -1;
        if (n == 0) break;
        pos += (size_t)n;
        if (pos == sizeof(buf)) return -1;
    }
    return pos == size && memcmp(buf, want, size) == 0 ? 0 : -1;
}
int main(int argc, char **argv) {
    if (argc != 3) return 64;
    int mode = atoi(argv[1]);
    if (mode < 1 || mode > 4) return 64;
    char src[4096], dst[4096], phase[4096];
    if (snprintf(src, sizeof(src), "%s/src", argv[2]) >= (int)sizeof(src) ||
        snprintf(dst, sizeof(dst), "%s/dst", argv[2]) >= (int)sizeof(dst) ||
        snprintf(phase, sizeof(phase), "%s/ready", argv[2]) >= (int)sizeof(phase)) return 65;
    if (put(src, src_bytes, sizeof(src_bytes)-1) ||
        put(dst, dst_bytes, sizeof(dst_bytes)-1)) return 66;
    int old = -1;
    if (mode >= 3) {
        old = open(mode == 3 ? dst : src, O_RDONLY);
        if (old < 0) return 67;
    }
    if (put(phase, "ready\n", 6)) return 68;
    int result = (mode == 1 || mode == 3)
        ? renameat(AT_FDCWD, src, AT_FDCWD, dst)
        : unlinkat(AT_FDCWD, src, 0);
    int saved_errno = errno;
    fprintf(stderr, "C12 mode=%d result=%d errno=%d pid=%d\n",
            mode, result, result < 0 ? saved_errno : 0, getpid());
    if (result < 0) { if (old >= 0) close(old); return 69; }
    if (old >= 0) {
        const char *want = mode == 3 ? dst_bytes : src_bytes;
        size_t len = mode == 3 ? sizeof(dst_bytes)-1 : sizeof(src_bytes)-1;
        int good = contents(old, want, len);
        if (close(old) || good) return 70;
    }
    int published = open(dst, O_RDONLY);
    if (published < 0) return 71;
    const char *want = (mode == 1 || mode == 3) ? src_bytes : dst_bytes;
    size_t len = (mode == 1 || mode == 3) ? sizeof(src_bytes)-1 : sizeof(dst_bytes)-1;
    int good = contents(published, want, len);
    if (close(published) || good) return 72;
    /* The independent harness checks missing SRC without relying on a routed
       missing-open error, which is a different operation's characterization. */
    return 0;
}
