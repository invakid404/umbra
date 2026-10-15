/* Rank-11 spawned helper: a separate image from umbra-userspace-spawn, so a
 * spawn always names a different binary than the one running.
 *
 * argv: <mode> <root> [mode operands]. Every record goes to
 * <root>/records.child through one exclusive create, never O_APPEND: a routed
 * O_APPEND open stops the run (#161), which is how the first exploratory draft
 * of this fixture measured nothing. Records are evidence beside the
 * independently read shadow bytes, never the only oracle. */
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

static char records[8192];
static size_t used;
static void note(const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    int n = vsnprintf(records + used, sizeof records - used, format, arguments);
    va_end(arguments);
    if (n < 0 || (size_t)n >= sizeof records - used) exit(70);
    used += (size_t)n;
}
/* Short transfers continue and EINTR retries; anything else is the caller's. */
static ssize_t transfer(int fd, const char *bytes, size_t length) {
    size_t done = 0;
    while (done < length) {
        ssize_t n = write(fd, bytes + done, length - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return n;
        done += (size_t)n;
    }
    return (ssize_t)done;
}
static void identity(void) {
    note("identity pid=%d ppid=%d pgid=%d sid=%d\n", getpid(), getppid(), getpgrp(), getsid(0));
}
static void flush(const char *root) {
    char path[4096];
    if (snprintf(path, sizeof path, "%s/records.child", root) >= (int)sizeof path) exit(71);
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || transfer(fd, records, used) != (ssize_t)used || close(fd) != 0) exit(72);
}
/* Write one payload, recording the direct return and the errno saved at once. */
static ssize_t recorded_write(const char *label, int fd, const char *bytes) {
    errno = 0;
    ssize_t n = write(fd, bytes, strlen(bytes));
    int saved = errno;
    note("%s fd=%d result=%zd errno=%d\n", label, fd, n, n < 0 ? saved : 0);
    return n;
}

/* C11-01/C11-02: one exclusive create, one write, one close. */
static int write_child(const char *root) {
    identity();
    char path[4096];
    if (snprintf(path, sizeof path, "%s/child.txt", root) >= (int)sizeof path) return 3;
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    note("open-child fd=%d errno=%d\n", fd, fd < 0 ? errno : 0);
    if (fd < 0) return 4;
    if (transfer(fd, "CHILD", 5) != 5) return 5;
    if (close(fd) != 0) return 6;
    flush(root);
    return 0;
}

/* C11-05 (amendment A2): no dup2 creates the collision. V and N are the
 * parent's routed CLOEXEC victim and routed control; KC and KN are kernel
 * CLOEXEC and non-CLOEXEC controls. The write through V is issued whether or
 * not pipe() landed on V's number, because its result is the stale-entry
 * oracle either way: natively V is closed at exec, so it reaches whatever the
 * kernel put at that number or fails EBADF; a routed child that still holds a
 * logical entry for V writes into the victim's shadow instead. */
static int cloexec(const char *root, int v, int n, int kc, int kn) {
    identity();
    struct rlimit limit;
    if (getrlimit(RLIMIT_NOFILE, &limit) != 0) return 10;
    note("rlimit-nofile cur=%llu max=%llu\n", (unsigned long long)limit.rlim_cur,
         (unsigned long long)limit.rlim_max);
    const int probes[] = {v, n, kc, kn};
    const char *labels[] = {"victim", "control", "kernel-cloexec", "kernel-inherited"};
    for (int i = 0; i < 4; i++) {
        errno = 0;
        int flags = fcntl(probes[i], F_GETFD);
        note("kernel-view %s fd=%d getfd=%d errno=%d\n", labels[i], probes[i], flags,
             flags < 0 ? errno : 0);
    }
    int pair[2];
    errno = 0;
    int piped = pipe(pair);
    note("pipe result=%d errno=%d read=%d write=%d collision=%d\n", piped, piped ? errno : 0,
         piped ? -1 : pair[0], piped ? -1 : pair[1],
         !piped && (pair[0] == v || pair[1] == v));
    if (piped) return 11;
    recorded_write("write-victim", v, "COLLISION-c11-05");
    recorded_write("write-control", n, "CONTROL-c11-05");
    if (fcntl(pair[0], F_SETFL, O_NONBLOCK) != 0) return 12;
    char got[64];
    errno = 0;
    ssize_t length = read(pair[0], got, sizeof got - 1);
    int saved = errno;
    if (length < 0) note("pipe-read result=-1 errno=%d bytes=\n", saved);
    else note("pipe-read result=%zd errno=0 bytes=%.*s\n", length, (int)length, got);
    /* A2's discriminators: F was never virtual, then V. Both are kernel dup2
     * calls. The third target was never virtual either but sits where umbra's
     * virtual range does (the fence is RLIMIT_NOFILE): if it fails exactly as
     * V does, the V reading is the kernel's limit check, not the logical
     * table. Natively all three are below the limit and succeed. */
    const int targets[] = {100, v, 4200};
    const char *names[] = {"dup2-never-virtual", "dup2-victim", "dup2-never-virtual-above-fence"};
    for (int i = 0; i < 3; i++) {
        errno = 0;
        int result = dup2(pair[1], targets[i]);
        note("%s target=%d result=%d errno=%d\n", names[i], targets[i], result,
             result < 0 ? errno : 0);
        if (result == targets[i] && targets[i] != pair[1] && targets[i] != v) close(targets[i]);
    }
    flush(root);
    return 0;
}

/* C11-09: one inherited descriptor, three bytes, then the acknowledgment. */
static int inherited_write(const char *root, int output, int ack) {
    identity();
    ssize_t n = recorded_write("write-inherited", output, "BBB");
    flush(root);
    if (transfer(ack, "K", 1) != 1) return 20;
    return n == 3 ? 0 : 21;
}

/* C11-10: echo the nonce the parent sends on stdin to stdout, stderr and the
 * one explicitly inherited descriptor, then write a relative file in the
 * working directory the parent's addchdir asked for. Stdin is drained to EOF
 * first, so nothing happens before the parent has released this child. */
static int echo(const char *root, int inherited, int decoy) {
    char nonce[128];
    size_t length = 0;
    for (;;) {
        ssize_t n = read(STDIN_FILENO, nonce + length, sizeof nonce - 1 - length);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return 30;
        if (n == 0) break;
        length += (size_t)n;
        if (length == sizeof nonce - 1) return 31;
    }
    nonce[length] = 0;
    identity();
    note("stdin bytes=%zu nonce=%s\n", length, nonce);
    /* Descriptor census: only 0, 1, 2 and the inherited write end should be
     * open; the decoy is a non-CLOEXEC kernel descriptor CLOEXEC_DEFAULT must
     * close. A number the kernel reports as open is open; this cannot see
     * virtual descriptors, which no kernel call can (#165). */
    note("census");
    for (int fd = 0; fd < 256; fd++)
        if (fcntl(fd, F_GETFD) >= 0) note(" %d", fd);
    note("\ndecoy fd=%d open=%d inherited=%d\n", decoy, fcntl(decoy, F_GETFD) >= 0, inherited);
    char cwd[4096];
    note("cwd %s\n", getcwd(cwd, sizeof cwd) ? cwd : "UNAVAILABLE");
    char line[192];
    int n = snprintf(line, sizeof line, "OUT:%s", nonce);
    if (transfer(STDOUT_FILENO, line, (size_t)n) != n) return 32;
    n = snprintf(line, sizeof line, "ERR:%s", nonce);
    if (transfer(STDERR_FILENO, line, (size_t)n) != n) return 33;
    n = snprintf(line, sizeof line, "INH:%s", nonce);
    if (transfer(inherited, line, (size_t)n) != n) return 34;
    int fd = open("child.txt", O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0) return 35;
    n = snprintf(line, sizeof line, "C11-10-CHILD:%s", nonce);
    if (transfer(fd, line, (size_t)n) != n || close(fd) != 0) return 36;
    flush(root);
    return 0;
}

/* ofd-race (#190): report ready, wait for the parent's go byte, then write 10
 * records on the inherited descriptor with nothing ordering them against the
 * parent's. One write(2) per record, never retried. */
static int race_write(const char *root, int output, int go, int ready) {
    identity();
    if (transfer(ready, "R", 1) != 1) return 23;
    close(ready);
    char token = 0;
    ssize_t got;
    do got = read(go, &token, 1);
    while (got < 0 && errno == EINTR);
    int short_writes = 0, first_errno = 0;
    for (int i = 0; i < 10; i++) {
        char record[8];
        snprintf(record, sizeof record, "C%04d\n", i);
        errno = 0;
        ssize_t n = write(output, record, 6);
        if (n != 6) {
            short_writes++;
            if (!first_errno) first_errno = n < 0 ? errno : -1;
        }
    }
    note("go result=%zd token=%c\n", got, got == 1 ? token : '?');
    note("write-child records=10 short=%d first-errno=%d\n", short_writes, first_errno);
    flush(root);
    return got == 1 && token == 'G' && !short_writes ? 0 : 22;
}

int main(int argc, char **argv) {
    if (argc < 3) return 2;
    const char *mode = argv[1], *root = argv[2];
    if (!strcmp(mode, "write-child") && argc == 3) return write_child(root);
    if (!strcmp(mode, "c11-05") && argc == 7)
        return cloexec(root, atoi(argv[3]), atoi(argv[4]), atoi(argv[5]), atoi(argv[6]));
    if (!strcmp(mode, "c11-09") && argc == 5)
        return inherited_write(root, atoi(argv[3]), atoi(argv[4]));
    if (!strcmp(mode, "c11-10") && argc == 5) return echo(root, atoi(argv[3]), atoi(argv[4]));
    if (!strcmp(mode, "ofd-race") && argc == 6)
        return race_write(root, atoi(argv[3]), atoi(argv[4]), atoi(argv[5]));
    /* Apparatus: a child that never exits on its own. Only a signal ends it. */
    if (!strcmp(mode, "hold") && argc == 3)
        for (;;) pause();
    return 2;
}
