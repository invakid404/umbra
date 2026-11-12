#include <unistd.h>
#include <sys/wait.h>
int main(void) {
    usleep(200000);
    pid_t children[4];
    for (unsigned i = 0; i < 4; ++i) {
        children[i] = fork();
        if (children[i] < 0) return 2;
        if (!children[i]) { usleep(200000); execl("/bin/sleep", "sleep", "0.2", (char *)0); _exit(3); }
    }
    for (unsigned i = 0; i < 4; ++i) {
        int status;
        if (waitpid(children[i], &status, 0) != children[i]) return 4;
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 5;
    }
    return 0;
}
