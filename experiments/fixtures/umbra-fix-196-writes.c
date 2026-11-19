/* Purpose-built replay-retirement workload. No retries of a short write:
 * every requested write must complete, and the harness checks every byte. */
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;

static int records(int fd, int count, size_t size) {
    char *bytes = malloc(size);
    if (!bytes) return 20;
    for (int i = 0; i < count; i++) {
        memset(bytes, 'a' + i % 26, size);
        if (write(fd, bytes, size) != (ssize_t)size) {
            fprintf(stderr, "write index=%d errno=%d\n", i, errno);
            free(bytes);
            return 21;
        }
    }
    free(bytes);
    return 0;
}
static int finish(int fd) {
    if (write(fd, "END\n", 4) != 4) return 22;
    return close(fd) == 0 ? 0 : 23;
}
int main(int argc, char **argv) {
    if (argc >= 4 && (!strcmp(argv[1], "child") || !strcmp(argv[1], "exec-child"))) {
        int fd = atoi(argv[3]);
        if (!strcmp(argv[1], "exec-child")) {
            if (argc != 5) return 24;
            errno = 0;
            if (fcntl(atoi(argv[4]), F_GETFD) != -1 || errno != EBADF) return 25;
        }
        int result = records(fd, 80, 6);
        if (result) return result;
        return !strcmp(argv[1], "exec-child") ? finish(fd) : (close(fd) == 0 ? 0 : 26);
    }
    if (argc != 4) return 2;
    char path[4096];
    if (snprintf(path, sizeof path, "%s/out.txt", argv[2]) >= (int)sizeof path) return 3;
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || write(fd, "BEGIN\n", 6) != 6) return 4;
    int result;
    if (!strcmp(argv[1], "many")) result = records(fd, 400, 6);
    else if (!strcmp(argv[1], "payload")) result = records(fd, 8, 786432);
    else if (!strcmp(argv[1], "fork") || !strcmp(argv[1], "spawn")) {
        pid_t child;
        if (!strcmp(argv[1], "fork")) {
            child = fork();
            if (child == 0) {
                int rc = records(fd, 80, 6);
                if (close(fd) != 0 && !rc) rc = 26;
                _exit(rc);
            }
        } else {
            char descriptor[16];
            snprintf(descriptor, sizeof descriptor, "%d", fd);
            char *args[] = {argv[0], "child", argv[2], descriptor, NULL};
            int rc = posix_spawn(&child, argv[0], NULL, NULL, args, environ);
            if (rc) return 27;
        }
        if (child < 0) return 28;
        int status = 0;
        pid_t waited;
        do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
        if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 29;
        result = 0;
    } else if (!strcmp(argv[1], "exec")) {
        int control = open(path, O_RDONLY | O_CLOEXEC);
        if (control < 0) return 30;
        char descriptor[16], closed[16];
        snprintf(descriptor, sizeof descriptor, "%d", fd);
        snprintf(closed, sizeof closed, "%d", control);
        char *args[] = {argv[0], "exec-child", argv[2], descriptor, closed, NULL};
        execve(argv[0], args, environ);
        return 31;
    } else return 32;
    return result ? result : finish(fd);
}
