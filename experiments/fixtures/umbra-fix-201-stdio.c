/* Small stdio payload fixture. All filesystem observations belong to the
 * caller's inherited descriptors; this program does not reopen their paths. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int copy(int input, int output) {
    char data[128];
    ssize_t n;
    while ((n = read(input, data, sizeof data)) > 0) {
        ssize_t offset = 0;
        while (offset < n) {
            ssize_t written = write(output, data + offset, (size_t)(n - offset));
            if (written < 0 && errno == EINTR) continue;
            if (written <= 0) return 22;
            offset += written;
        }
    }
    return n < 0 ? 21 : 0;
}

int main(int argc, char **argv) {
    if (argc < 2) return 64;
    if (!strcmp(argv[1], "copy")) return copy(0, 1);
    if (!strcmp(argv[1], "write")) {
        const char bytes[] = "f201-output\n";
        return write(1, bytes, sizeof bytes - 1) == sizeof bytes - 1 ? 0 : 23;
    }
    if (!strcmp(argv[1], "pty-read")) {
        int fd = argc > 2 ? atoi(argv[2]) : 0;
        dprintf(2, "ISO READY mode=f201 pid=%d ppid=%d pgid=%d sid=%d\n",
                getpid(), getppid(), getpgrp(), getsid(0));
        char data[6];
        ssize_t n;
        do { n = read(fd, data, sizeof data); } while (n < 0 && errno == EINTR);
        if (n != sizeof data || memcmp(data, "hello\n", sizeof data)) return 24;
        dprintf(2, "F201 input matched exactly six bytes\n");
        return write(1, "f201-read-ok\n", 13) == 13 ? 0 : 25;
    }
    if (!strcmp(argv[1], "sigpipe")) {
        int ignored = argc > 2 && !strcmp(argv[2], "ignore");
        signal(SIGPIPE, ignored ? SIG_IGN : SIG_DFL);
        errno = 0;
        ssize_t n = write(1, "x", 1);
        return ignored && n == -1 && errno == EPIPE ? 0 : 26;
    }
    if (!strcmp(argv[1], "close-output")) {
        /* Parent's control process must stay alive while the reader sees EOF.
         * The caller releases the fixture by sending exactly one stdin byte. */
        if (close(1)) return 27;
        char release;
        return read(0, &release, 1) == 1 && release == 'q' ? 0 : 28;
    }
    if (!strcmp(argv[1], "sentinel")) {
        if (argc != 3) return 64;
        errno = 0;
        return fcntl(atoi(argv[2]), F_GETFD) == -1 && errno == EBADF ? 0 : 29;
    }
    if (!strcmp(argv[1], "fork-exec")) {
        pid_t child = fork();
        if (child < 0) return 30;
        if (!child) {
            execl(argv[0], argv[0], "copy", (char *)NULL);
            _exit(31);
        }
        int status;
        if (waitpid(child, &status, 0) != child) return 32;
        return WIFEXITED(status) ? WEXITSTATUS(status) : 33;
    }
    return 64;
}
