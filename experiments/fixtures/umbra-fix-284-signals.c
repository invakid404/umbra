/* Small same-binary native/routed signal controls for #284. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sys/socket.h>
#include <time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sched.h>

static volatile sig_atomic_t deliveries;
static const char *handler_path;
static volatile sig_atomic_t io_failed;
static atomic_int rate_ready, rate_stop, rate_failed;
static const char *rate_root;

static void *rate_worker(void *arg) {
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &blocked, NULL);
    char path[4096];
    snprintf(path, sizeof(path), "%s/thread-%ld", rate_root, (long)arg);
    int announced = 0;
    while (!atomic_load(&rate_stop)) {
        int fd = open(path, O_RDONLY);
        char data[3];
        if (fd < 0 || read(fd, data, sizeof(data)) != 3 ||
            memcmp(data, "io\n", 3) || close(fd)) {
            atomic_store(&rate_failed, 1);
            break;
        }
        if (!announced) { atomic_fetch_add(&rate_ready, 1); announced = 1; }
    }
    return NULL;
}

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
    if (!strcmp(argv[1], "mask-verify")) {
        if (argc != 3) return 64;
        sigset_t mask;
        struct sigaction action;
        if (sigprocmask(SIG_SETMASK, NULL, &mask) ||
            sigismember(&mask, SIGUSR2) != atoi(argv[2]) ||
            sigaction(SIGPIPE, NULL, &action) || action.sa_handler != SIG_DFL) return 104;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (sigaction(SIGUSR1, &action, NULL) || kill(getpid(), SIGUSR1) ||
            deliveries != 1) return 105;
        return 0;
    }
    if (!strcmp(argv[1], "rate")) {
        if (argc != 3) return 64;
        rate_root = argv[2];
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (sigaction(SIGUSR1, &action, NULL)) return 100;
        pthread_t workers[2];
        for (long i = 0; i < 2; i++)
            if (pthread_create(&workers[i], NULL, rate_worker, (void *)i)) return 101;
        while (atomic_load(&rate_ready) != 2) {
            if (atomic_load(&rate_failed)) return 102;
            sched_yield();
        }
        printf("READY %d\n", getpid());
        fflush(stdout);
        while (!deliveries) sched_yield();
        atomic_store(&rate_stop, 1);
        for (int i = 0; i < 2; i++) pthread_join(workers[i], NULL);
        if (atomic_load(&rate_failed)) return 103;
        printf("delivered=%d\n", (int)deliveries);
        return 0;
    }
    if (!strcmp(argv[1], "concurrent")) {
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (sigaction(SIGUSR1, &action, NULL)) return 82;
        if (write(1, "READY\n", 6) != 6) return 83;
        char go;
        if (read(0, &go, 1) != 1 || go != 'q') return 84;
        if (kill(getpid(), SIGUSR1) || deliveries != 1) return 85;
        return write(1, "ONE\n", 4) == 4 ? 0 : 86;
    }
    if (!strcmp(argv[1], "pending-exec") || !strcmp(argv[1], "pending-child")) {
        sigset_t set, mask, pending;
        sigemptyset(&set);
        sigaddset(&set, SIGUSR1);
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        action.sa_handler = handled;
        if (!strcmp(argv[1], "pending-exec")) {
            if (sigaction(SIGUSR1, &action, NULL) ||
                sigprocmask(SIG_BLOCK, &set, NULL) || kill(getpid(), SIGUSR1)) return 87;
            pid_t child = fork();
            if (child < 0) return 88;
            if (!child) {
                if (sigpending(&pending) || sigismember(&pending, SIGUSR1)) _exit(89);
                _exit(0);
            }
            int status;
            if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
                WEXITSTATUS(status)) return 90;
            execl(argv[0], argv[0], "pending-child", (char *)NULL);
            return 91;
        }
        if (sigprocmask(SIG_SETMASK, NULL, &mask) ||
            !sigismember(&mask, SIGUSR1) || sigpending(&pending) ||
            !sigismember(&pending, SIGUSR1)) return 92;
        struct sigaction inherited;
        if (sigaction(SIGUSR1, NULL, &inherited) || inherited.sa_handler != SIG_DFL) return 93;
        if (sigaction(SIGUSR1, &action, NULL) ||
            sigprocmask(SIG_UNBLOCK, &set, NULL) || deliveries != 1) return 94;
        return write(1, "pending once\n", 13) == 13 ? 0 : 95;
    }
    if (!strcmp(argv[1], "ignore-exec") || !strcmp(argv[1], "ignore-child")) {
        struct sigaction action;
        memset(&action, 0, sizeof(action));
        sigemptyset(&action.sa_mask);
        if (!strcmp(argv[1], "ignore-exec")) {
            action.sa_handler = SIG_IGN;
            if (sigaction(SIGUSR2, &action, NULL)) return 96;
            execl(argv[0], argv[0], "ignore-child", (char *)NULL);
            return 97;
        }
        if (sigaction(SIGUSR2, NULL, &action) || action.sa_handler != SIG_IGN ||
            kill(getpid(), SIGUSR2)) return 98;
        return write(1, "ignored after exec\n", 19) == 19 ? 0 : 99;
    }
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
    if (argc != 4 || (strcmp(argv[1], "disposition") &&
                     strcmp(argv[1], "timer") && strcmp(argv[1], "inherited-socket"))) return 64;
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
    if (!strcmp(argv[1], "timer")) {
        alarm(1);
        struct timespec remain = { .tv_sec = 1, .tv_nsec = 200000000 };
        while (nanosleep(&remain, &remain) && errno == EINTR) {}
    } else if (signo == SIGPIPE) {
        int pair[2];
        int fd = 1;
        if (strcmp(argv[1], "inherited-socket")) {
            if (pipe(pair) || close(pair[0])) return 68;
            fd = pair[1];
        }
        errno = 0;
        ssize_t count = write(fd, "x", 1);
        if (count != -1 || errno != EPIPE) return 69;
        if (fd != 1) close(fd);
    } else {
        if (kill(getpid(), signo)) return 70;
    }
    dprintf(!strcmp(argv[1], "inherited-socket") ? 2 : 1,
            "continued count=%d\n", (int)deliveries);
    return deliveries == (!strcmp(argv[3], "handler") ? 1 : 0) ? 0 : 71;
}
