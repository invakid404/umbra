/* #274: serial children with externally observable census milestones.
 * Host-created files start and release the completed root.
 * No timeout or lifetime policy is imposed on the supervised session.
 */
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int gate(const char *path) {
    while (access(path, F_OK) != 0) {
        if (errno != ENOENT)
            return 1;
        usleep(10000);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "child") == 0)
        return 0;
    if (argc != 3) return 14;
    setvbuf(stderr, NULL, _IOLBF, 0);
    fprintf(stderr, "ready pid=%d\n", getpid());
    if (gate(argv[1]))
        return 10;
    for (int i = 0; i < 200; ++i) {
        pid_t child;
        char *args[] = {argv[0], "child", NULL};
        int error = posix_spawn(&child, argv[0], NULL, NULL, args, environ);
        if (error) {
            fprintf(stderr, "spawn[%d]: %d\n", i, error);
            return 11;
        }
        int status = 0;
        pid_t waited;
        do {
            waited = waitpid(child, &status, 0);
        } while (waited < 0 && errno == EINTR);
        if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            fprintf(stderr, "wait[%d]: pid=%d status=%d errno=%d\n",
                    i, waited, status, errno);
            return 12;
        }
    }
    fputs("completed=200\n", stderr);
    return gate(argv[2]) ? 13 : 0;
}
