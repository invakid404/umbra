/* #274: descendants keep inherited descriptions after another slot retires. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/event.h>
#include <unistd.h>

static void require(int ok, int code) { if (!ok) { fprintf(stderr, "f274 pid=%d failure=%d errno=%d\n", getpid(), code, errno); _exit(code); } }
static void put(int fd, const char *s) { require(write(fd, s, strlen(s)) == (ssize_t)strlen(s), 80); }
static void mark(const char *name, const char *text) {
    int fd = open(name, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    require(fd >= 0, 81); put(fd, text); require(close(fd) == 0, 82);
}
static void gate(const char *name) {
    fprintf(stderr, "f274 pid=%d gate=%s\n", getpid(), name);
    for (int n = 0; n < 10000; ++n) {
        int fd = open(name, O_RDONLY);
        if (fd >= 0) { close(fd); fprintf(stderr, "f274 pid=%d gate-ready\n", getpid()); return; }
        require(errno == ENOENT, 83);
        usleep(1000);
    }
    require(0, 84);
}
static void waited(pid_t pid, int code) {
    fprintf(stderr, "f274 pid=%d wait=%d expected=%d\n", getpid(), pid, code);
    int status = 0; pid_t got;
    do { got = waitpid(pid, &status, 0); } while (got < 0 && errno == EINTR);
    fprintf(stderr, "f274 pid=%d waited=%d raw=%d\n", getpid(), got, status);
    require(got == pid && WIFEXITED(status) && WEXITSTATUS(status) == code, 85);
}
int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "writer")) {
        int fd = atoi(argv[2]); put(fd, "B"); return 17;
    }
    require(argc == 4, 87);
    char mode[64];
    require(strlen(argv[1]) < sizeof(mode), 108); strcpy(mode, argv[1]);
    char *suffix = strchr(mode, ":"[0]);
    int routed = suffix != NULL;
    if (suffix) { require(!strcmp(suffix, ":routed"), 109); *suffix = 0; }
    char file[4096], ready[4096], done[4096], record[4096];
    snprintf(file, sizeof(file), "%s/file", argv[2]);
    snprintf(ready, sizeof(ready), "%s/ready", argv[2]);
    snprintf(done, sizeof(done), "%s/done", argv[2]);
    snprintf(record, sizeof(record), "%s/records", argv[2]);
    int fd = open(file, O_CREAT | O_TRUNC | O_RDWR, 0600); require(fd >= 0, 88); put(fd, "A");
    if (!strcmp(mode, "sibling")) {
        pid_t a = fork(); require(a >= 0, 89);
        if (!a) { gate(ready); put(fd, "C"); _exit(23); }
        pid_t b = fork(); require(b >= 0, 91);
        if (!b) {
            char number[32]; snprintf(number, sizeof(number), "%d", fd);
            execl(argv[3], argv[3], "writer", number, (char *)NULL); _exit(92);
        }
        waited(b, 17); usleep(300000); mark(ready, "retired\n");
        waited(a, 23); put(fd, "D");
        mark(record, "exec=17 sibling=23 bytes=ABCD\n");
    } else {
        require(!strcmp(mode, "parent") || !strcmp(mode, "parent-nonzero") || !strcmp(mode, "parent-signal"), 94);
        int identities[2]; require(pipe(identities) == 0, 100);
        pid_t p = fork(); require(p >= 0, 95);
        if (!p) {
            pid_t g = fork(); require(g >= 0, 96);
            if (!g) {
                close(identities[0]); close(identities[1]);
                gate(ready); put(fd, "B"); mark(done, "descendant\n");
                if (!strcmp(mode, "parent-signal")) raise(SIGKILL);
                _exit(!strcmp(mode, "parent-nonzero") ? 7 : 0);
            }
            close(identities[0]);
            require(write(identities[1], &g, sizeof(g)) == sizeof(g), 101);
            close(identities[1]);
            _exit(19);
        }
        close(identities[1]);
        pid_t descendant;
        require(read(identities[0], &descendant, sizeof(descendant)) == sizeof(descendant), 102);
        close(identities[0]);
        char identity_path[4096], identity[32];
        snprintf(identity_path, sizeof(identity_path), "%s/descendant.pid", argv[2]);
        snprintf(identity, sizeof(identity), "%d\n", descendant);
        mark(identity_path, identity);
        // Native control observes the kernel status. The routed harness checks
        // this PID's debugger exit packet: Seatbelt denies NOTE_EXITSTATUS and
        // ptrace can suppress outside observers' exit events for orphans.
        int queue = -1;
        struct kevent change, event;
        if (!routed) {
            queue = kqueue(); require(queue >= 0, 103);
            EV_SET(&change, descendant, EVFILT_PROC, EV_ADD | EV_ONESHOT,
                   NOTE_EXIT | NOTE_EXITSTATUS, 0, NULL);
            require(kevent(queue, &change, 1, NULL, 0, NULL) == 0, 104);
        }
        waited(p, 19); usleep(300000); mark(ready, "parent-retired\n"); gate(done);
        if (!routed) {
        struct timespec bound = {10, 0};
        int count = kevent(queue, NULL, 0, &event, 1, &bound);
        require(count == 1 && !(event.flags & EV_ERROR)
                && event.ident == (uintptr_t)descendant && event.filter == EVFILT_PROC
                && (event.fflags & (NOTE_EXIT | NOTE_EXITSTATUS)) == (NOTE_EXIT | NOTE_EXITSTATUS), 105);
        int status = (int)event.data;
        fprintf(stderr, "f274 descendant=%d exit-status=%d\n", descendant, status);
        require(WIFEXITED(status) && WEXITSTATUS(status) == 0, 106);
        require(close(queue) == 0, 107);
        }
        put(fd, "C");
        mark(record, "parent=19 bytes=ABC\n");
    }
    require(close(fd) == 0, 99); return 0;
}
