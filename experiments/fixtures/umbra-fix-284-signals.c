/* Small same-binary native/routed signal controls for #284. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

static volatile sig_atomic_t deliveries;
static const char *handler_path;
static volatile sig_atomic_t io_failed;

static void handled(int signo) {
    (void)signo;
    ++deliveries;
    if (handler_path) {
        int fd = open(handler_path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        if (fd < 0) { io_failed = 1; return; }
        if (write(fd, "handler\n", 8) != 8) io_failed = 1;
        if (close(fd)) io_failed = 1;
    }
}

int main(int argc, char **argv) {
    if (argc < 2) return 64;
    if (!strcmp(argv[1], "unsafe")) {
        if (argc != 4) return 64;
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (sigaction(SIGUSR1, &action, NULL)) return 76;
        if (!strcmp(argv[3], "native") && kill(getpid(), SIGUSR1)) return 77;
        if (!strcmp(argv[2], "exec")) {
            execl(argv[0], argv[0], "initial", (char *)NULL);
            return 78;
        }
        pid_t child = fork();
        if (child < 0) return 79;
        if (!child) {
            if (!strcmp(argv[2], "park")) {
                char release;
                if (read(0, &release, 1) != 1 || release != 'q') _exit(80);
            }
            _exit(0);
        }
        int status;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
            WEXITSTATUS(status)) return 81;
        printf("unsafe returned count=%d\n", (int)deliveries);
        return 0;
    }
    if (!strcmp(argv[1], "handler-io")) {
        if (argc != 3) return 64;
        handler_path = argv[2];
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (sigaction(SIGUSR1, &action, NULL) || kill(getpid(), SIGUSR1)) return 72;
        if (deliveries != 1 || io_failed) return 73;
        int fd = open(handler_path, O_RDONLY);
        char data[8];
        if (fd < 0 || read(fd, data, sizeof(data)) != 8 ||
            memcmp(data, "handler\n", 8) || close(fd)) return 74;
        return write(1, data, 8) == 8 ? 0 : 75;
    }
    if (!strcmp(argv[1], "initial")) {
        struct sigaction action;
        sigset_t mask;
        if (sigaction(SIGPIPE, NULL, &action) ||
            sigprocmask(SIG_SETMASK, NULL, &mask)) return 65;
        printf("initial pipe=%s blocked=%d\n",
               action.sa_handler == SIG_DFL ? "default" :
               action.sa_handler == SIG_IGN ? "ignored" : "handler",
               sigismember(&mask, SIGPIPE));
        return 0;
    }
    if (argc != 4 || strcmp(argv[1], "disposition")) return 64;
    int signo = atoi(argv[2]);
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    sigemptyset(&action.sa_mask);
    action.sa_handler = !strcmp(argv[3], "default") ? SIG_DFL :
                       !strcmp(argv[3], "ignored") ? SIG_IGN : handled;
    if (sigaction(signo, &action, NULL)) return 66;
    sigset_t unblocked;
    sigemptyset(&unblocked);
    sigaddset(&unblocked, signo);
    if (sigprocmask(SIG_UNBLOCK, &unblocked, NULL)) return 67;
    if (signo == SIGPIPE) {
        int pair[2];
        if (pipe(pair) || close(pair[0])) return 68;
        errno = 0;
        ssize_t count = write(pair[1], "x", 1);
        if (count != -1 || errno != EPIPE) return 69;
        close(pair[1]);
    } else {
        if (kill(getpid(), signo)) return 70;
    }
    printf("continued count=%d\n", (int)deliveries);
    return deliveries == (!strcmp(argv[3], "handler") ? 1 : 0) ? 0 : 71;
}
