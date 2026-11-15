/* #274: serial children with externally observable census milestones.
 * stdin supplies one byte to start and one to release the completed root.
 * No timeout or lifetime policy is imposed on the supervised session.
 */
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int gate(void) {
    char byte;
    ssize_t n;
    do {
        n = read(STDIN_FILENO, &byte, 1);
    } while (n < 0 && errno == EINTR);
    return n == 1 ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "child") == 0)
        return 0;
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("ready pid=%d\n", getpid());
    if (gate())
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
    puts("completed=200");
    return gate() ? 13 : 0;
}
