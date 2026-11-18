/* Small same-binary native/routed signal controls for #284. */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t deliveries;

static void handled(int signo) {
    (void)signo;
    ++deliveries;
}

int main(int argc, char **argv) {
    if (argc < 2) return 64;
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
