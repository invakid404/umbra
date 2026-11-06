/* C15-01: identical setup, only the exec pathname changes. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <limits.h>

static int put(const char *path, const char *bytes) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0) return 21;
    size_t left = strlen(bytes);
    while (left) {
        ssize_t n = write(fd, bytes, left);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { close(fd); return 22; }
        bytes += n; left -= (size_t)n;
    }
    return close(fd) == 0 ? 0 : 23;
}
int main(int argc, char **argv) {
#ifdef C15_HELPER
    if (argc != 2) return 24;
    return put(argv[1], "C15-01 intended helper\n");
#else
    if (argc != 4 || (strcmp(argv[1], "direct") && strcmp(argv[1], "alias"))) return 25;
    char dir[PATH_MAX], alias[PATH_MAX], ready[PATH_MAX], output[PATH_MAX];
    if (snprintf(dir, sizeof dir, "%s/helpers", argv[2]) >= (int)sizeof dir ||
        snprintf(alias, sizeof alias, "%s/helpers/helper", argv[2]) >= (int)sizeof alias ||
        snprintf(ready, sizeof ready, "%s/ready", argv[2]) >= (int)sizeof ready ||
        snprintf(output, sizeof output, "%s/out.txt", argv[2]) >= (int)sizeof output) return 26;
    if (mkdir(dir, 0700)) return 27;
    if (symlink(argv[3], alias)) return 28;
    int error = put(ready, "C15-01 alias created; exec next\n");
    if (error) return error;
    char *args[] = {argv[3], output, NULL};
    execv(strcmp(argv[1], "alias") == 0 ? alias : argv[3], args);
    int saved = errno;
    dprintf(STDERR_FILENO, "C15-01 exec returned errno=%d\n", saved);
    return 29;
#endif
}
