/* Rank-11 single-thread spawn parent. One argv mode per selected leaf:
 *
 *   c11-01                null actions and attributes (the control)
 *   c11-02-actions        initialized empty file actions, null attributes
 *   c11-02-attributes     null file actions, initialized empty attributes
 *   c11-02-both           both initialized and empty
 *   c11-05                routed CLOEXEC victim + routed control + kernel controls
 *   c11-05-premise        kernel dup2 onto a live routed descriptor (apparatus)
 *   c11-09                one inherited routed descriptor across spawn
 *   c11-10                E8's pipe launch, as the same C API sequence
 *   ofd-race              parent and spawned child write one inherited routed
 *                         descriptor concurrently, unordered (#190)
 *   ledger-escape         apparatus: a SETSID child left running on purpose
 *
 * argv: <mode> <root> <absolute helper>. The helper is always a separate
 * image (umbra-userspace-spawn-child). Records go to <root>/records.pre, written
 * and closed before the spawn call, and <root>/records.done after the reap, so
 * a run the supervisor stops at the spawn trap leaves the first and not the
 * second. Each is one exclusive create, never O_APPEND (#161).
 *
 * c11-10 and ledger-escape announce `ledger pid=<n>` on stdout from the PID
 * posix_spawn returned and then wait for one `G` on stdin, so the harness has
 * the child's birth identity before the child is released to do anything. */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

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
static void path_in(char *out, size_t size, const char *root, const char *name) {
    if (snprintf(out, size, "%s/%s", root, name) >= (int)size) exit(71);
}
static void flush(const char *root, const char *name) {
    char path[4096];
    path_in(path, sizeof path, root, name);
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || transfer(fd, records, used) != (ssize_t)used || close(fd) != 0) exit(72);
    used = 0;
}
static void identity(void) {
    note("identity pid=%d ppid=%d pgid=%d sid=%d\n", getpid(), getppid(), getpgrp(), getsid(0));
}
/* Reap exactly this PID; EINTR (SIGCHLD in c11-10) re-enters the wait. */
static int reap(pid_t child, int *status) {
    pid_t got;
    do got = waitpid(child, status, 0);
    while (got < 0 && errno == EINTR);
    note("wait pid=%d result=%d status=%d exited=%d code=%d signaled=%d signal=%d\n", child,
         got, *status, WIFEXITED(*status), WIFEXITED(*status) ? WEXITSTATUS(*status) : -1,
         WIFSIGNALED(*status), WIFSIGNALED(*status) ? WTERMSIG(*status) : -1);
    return got == child && WIFEXITED(*status) && WEXITSTATUS(*status) == 0 ? 0 : -1;
}
static int spawn(pid_t *child, const char *helper, const posix_spawn_file_actions_t *actions,
                 const posix_spawnattr_t *attributes, char *const args[]) {
    errno = 0;
    int result = posix_spawn(child, helper, actions, attributes, args, environ);
    int saved = errno;
    /* posix_spawn reports through its return value; errno is recorded beside
     * it only because the contract asks for both. */
    note("spawn result=%d errno=%d pid=%d\n", result, saved, result ? -1 : *child);
    return result;
}
/* Birth-identity ledger handshake (design-gate R3). */
static int announce(pid_t child) {
    char line[64];
    int n = snprintf(line, sizeof line, "ledger pid=%d\n", child);
    if (transfer(STDOUT_FILENO, line, (size_t)n) != n) return -1;
    char token = 0;
    ssize_t got;
    do got = read(STDIN_FILENO, &token, 1);
    while (got < 0 && errno == EINTR);
    note("ledger pid=%d ack=%c\n", child, got == 1 ? token : '?');
    return got == 1 && token == 'G' ? 0 : -1;
}

/* C11-01 and the three C11-02 leaves: the same child, one pointer shape each. */
static int shape(const char *mode, const char *root, const char *helper) {
    int use_actions = !strcmp(mode, "c11-02-actions") || !strcmp(mode, "c11-02-both");
    int use_attributes = !strcmp(mode, "c11-02-attributes") || !strcmp(mode, "c11-02-both");
    posix_spawn_file_actions_t actions;
    posix_spawnattr_t attributes;
    note("phase=pre-spawn mode=%s actions=%d attributes=%d\n", mode, use_actions, use_attributes);
    identity();
    if (use_actions) {
        int rc = posix_spawn_file_actions_init(&actions);
        note("file-actions-init result=%d\n", rc);
        if (rc) return 5;
    }
    if (use_attributes) {
        int rc = posix_spawnattr_init(&attributes);
        note("spawnattr-init result=%d\n", rc);
        if (rc) return 6;
    }
    flush(root, "records.pre");
    char *args[] = {(char *)helper, "write-child", (char *)root, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, use_actions ? &actions : NULL,
              use_attributes ? &attributes : NULL, args)) {
        flush(root, "records.done");
        return 10;
    }
    int status = 0;
    int failed = reap(child, &status);
    char path[4096];
    path_in(path, sizeof path, root, "parent.txt");
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || transfer(fd, "PARENT", 6) != 6 || close(fd) != 0) return 13;
    if (use_actions && posix_spawn_file_actions_destroy(&actions)) return 14;
    if (use_attributes && posix_spawnattr_destroy(&attributes)) return 15;
    note("phase=done\n");
    flush(root, "records.done");
    return failed ? 11 : 0;
}

/* C11-05, amendment A2. The kernel controls are opened FIRST, as one pipe:
 * pipe(2) gives fildes[0], the read end, the lowest free number, so with the
 * CLOEXEC kernel control below V the child's pipe() puts its READ end there
 * and its WRITE end on V's number -- which is what lets a native write(V)
 * land in the pipe at all. With V first, the read end would take V and a
 * native write(V) would fail EBADF. Routed, V is at or above the descriptor
 * fence wherever it is opened, so this order cannot change a routed reading. */
static int cloexec(const char *root, const char *helper) {
    note("phase=pre-spawn mode=c11-05\n");
    identity();
    int kernel[2];
    if (pipe(kernel) != 0) return 20;
    if (fcntl(kernel[0], F_SETFD, FD_CLOEXEC) != 0) return 21;
    char path[4096];
    path_in(path, sizeof path, root, "victim.txt");
    int victim = open(path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
    if (victim < 0) return 22;
    if (transfer(victim, "VICTIM-SEED", 11) != 11) return 23;
    path_in(path, sizeof path, root, "control.txt");
    int control = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (control < 0) return 24;
    note("descriptors victim=%d control=%d kernel-cloexec=%d kernel-inherited=%d\n", victim,
         control, kernel[0], kernel[1]);
    flush(root, "records.pre");
    char v[16], n[16], kc[16], kn[16];
    snprintf(v, sizeof v, "%d", victim);
    snprintf(n, sizeof n, "%d", control);
    snprintf(kc, sizeof kc, "%d", kernel[0]);
    snprintf(kn, sizeof kn, "%d", kernel[1]);
    char *args[] = {(char *)helper, "c11-05", (char *)root, v, n, kc, kn, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, NULL, NULL, args)) {
        flush(root, "records.done");
        return 10;
    }
    int status = 0;
    int failed = reap(child, &status);
    note("close victim=%d control=%d\n", close(victim), close(control));
    close(kernel[0]);
    close(kernel[1]);
    note("phase=done\n");
    flush(root, "records.done");
    return failed ? 11 : 0;
}

/* The R4 collision premise, amended by A2 to keep the errno: a kernel dup2 of
 * a pipe's write end onto a LIVE routed descriptor, in one process. */
static int premise(const char *root) {
    note("phase=pre-dup2 mode=c11-05-premise\n");
    char path[4096];
    path_in(path, sizeof path, root, "victim.txt");
    int victim = open(path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
    if (victim < 0) return 31;
    struct rlimit limit;
    if (getrlimit(RLIMIT_NOFILE, &limit) != 0) return 32;
    int pair[2];
    if (pipe(pair) != 0) return 33;
    errno = 0;
    int result = dup2(pair[1], victim);
    int saved = errno;
    note("dup2-live-victim victim=%d source=%d result=%d errno=%d rlimit-cur=%llu "
         "rlimit-max=%llu\n",
         victim, pair[1], result, result < 0 ? saved : 0, (unsigned long long)limit.rlim_cur,
         (unsigned long long)limit.rlim_max);
    flush(root, "records.pre");
    if (result != victim) return 34;
    if (transfer(victim, "COLLISION-c11-05", 16) != 16) return 35;
    if (fcntl(pair[0], F_SETFL, O_NONBLOCK) != 0) return 36;
    char got[64];
    ssize_t length = read(pair[0], got, sizeof got);
    note("pipe-read result=%zd bytes=%.*s\n", length, length > 0 ? (int)length : 0, got);
    flush(root, "records.done");
    return 0;
}

/* C11-09: AA from the parent, BBB from the spawned child on the inherited
 * descriptor, C from the parent after the acknowledgment and the reap. */
static int offset(const char *root, const char *helper) {
    note("phase=pre-spawn mode=c11-09\n");
    identity();
    char path[4096];
    path_in(path, sizeof path, root, "out.txt");
    int output = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (output < 0) return 40;
    if (transfer(output, "AA", 2) != 2) return 41;
    int ack[2];
    if (pipe(ack) != 0) return 42;
    if (fcntl(ack[0], F_SETFD, FD_CLOEXEC) != 0) return 43;
    note("descriptors output=%d ack-read=%d ack-write=%d\n", output, ack[0], ack[1]);
    flush(root, "records.pre");
    char o[16], a[16];
    snprintf(o, sizeof o, "%d", output);
    snprintf(a, sizeof a, "%d", ack[1]);
    char *args[] = {(char *)helper, "c11-09", (char *)root, o, a, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, NULL, NULL, args)) {
        flush(root, "records.done");
        return 10;
    }
    close(ack[1]);
    char token = 0;
    ssize_t got;
    do got = read(ack[0], &token, 1);
    while (got < 0 && errno == EINTR);
    note("ack result=%zd token=%c\n", got, got == 1 ? token : '?');
    int status = 0;
    int failed = reap(child, &status);
    errno = 0;
    ssize_t wrote = write(output, "C", 1);
    note("write-parent result=%zd errno=%d\n", wrote, wrote < 0 ? errno : 0);
    note("close output=%d\n", close(output));
    close(ack[0]);
    note("phase=done\n");
    flush(root, "records.done");
    return failed || got != 1 || token != 'K' || wrote != 1 ? 11 : 0;
}

static int sigchld_pipe[2];
static volatile sig_atomic_t sigchld_count;
static void on_sigchld(int number, siginfo_t *info, void *context) {
    (void)number, (void)info, (void)context;
    int saved = errno;
    sigchld_count++;
    (void)write(sigchld_pipe[1], "S", 1);
    errno = saved;
}
/* std::io::pipe's shape: both ends close-on-exec. */
static int cloexec_pipe(int pair[2]) {
    if (pipe(pair) != 0) return -1;
    return fcntl(pair[0], F_SETFD, FD_CLOEXEC) || fcntl(pair[1], F_SETFD, FD_CLOEXEC) ? -1 : 0;
}
/* posix_child.rs child_fd: a CLOEXEC duplicate at or above 3. */
static int child_fd(int fd) {
    int duplicate = fcntl(fd, F_DUPFD_CLOEXEC, 3);
    if (duplicate >= 0) close(fd);
    return duplicate;
}

/* C11-10: pinned E8 (codex 44fe510c utils/pty/src/posix_child.rs:136-226 via
 * pipe.rs:149-182), in single-threaded C: SIGCHLD subscription, three pipes,
 * addchdir_np, adddup2 onto 0/1/2, addinherit_np, SETSID, CLOEXEC_DEFAULT and
 * a SIGPIPE default. The reaper thread and Tokio wrappers are deliberately
 * absent; this qualifies the API sequence only. */
static int e8(const char *root, const char *helper) {
    note("phase=pre-spawn mode=c11-10\n");
    identity();
    if (cloexec_pipe(sigchld_pipe) || fcntl(sigchld_pipe[1], F_SETFL, O_NONBLOCK)) return 50;
    struct sigaction subscribe;
    memset(&subscribe, 0, sizeof subscribe);
    sigemptyset(&subscribe.sa_mask);
    subscribe.sa_sigaction = on_sigchld;
    subscribe.sa_flags = SA_SIGINFO | SA_RESTART;
    if (sigaction(SIGCHLD, &subscribe, NULL)) return 51;
    int in[2], out[2], err[2], inherit[2];
    if (cloexec_pipe(in) || cloexec_pipe(out) || cloexec_pipe(err) || cloexec_pipe(inherit))
        return 52;
    int child_in = child_fd(in[0]), child_out = child_fd(out[1]), child_err = child_fd(err[1]);
    if (child_in < 0 || child_out < 0 || child_err < 0) return 53;
    /* A non-CLOEXEC kernel descriptor that CLOEXEC_DEFAULT must keep out. */
    int decoy = dup(inherit[0]);
    if (decoy < 0) return 54;
    posix_spawn_file_actions_t actions;
    posix_spawnattr_t attributes;
    int rc = posix_spawn_file_actions_init(&actions);
    note("file-actions-init result=%d\n", rc);
    if (rc) return 55;
    rc = posix_spawnattr_init(&attributes);
    note("spawnattr-init result=%d\n", rc);
    if (rc) return 56;
    rc = posix_spawn_file_actions_addchdir_np(&actions, root);
    note("addchdir result=%d path=%s\n", rc, root);
    if (rc) return 57;
    const int sources[] = {child_in, child_out, child_err};
    for (int target = 0; target < 3; target++) {
        rc = posix_spawn_file_actions_adddup2(&actions, sources[target], target);
        note("adddup2 source=%d target=%d result=%d\n", sources[target], target, rc);
        if (rc) return 58;
    }
    rc = posix_spawn_file_actions_addinherit_np(&actions, inherit[1]);
    note("addinherit fd=%d result=%d\n", inherit[1], rc);
    if (rc) return 59;
    sigset_t defaults;
    sigemptyset(&defaults);
    sigaddset(&defaults, SIGPIPE);
    rc = posix_spawnattr_setsigdefault(&attributes, &defaults);
    note("setsigdefault SIGPIPE result=%d\n", rc);
    if (rc) return 60;
    short flags = POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF;
    rc = posix_spawnattr_setflags(&attributes, flags);
    note("setflags flags=0x%x result=%d\n", flags, rc);
    if (rc) return 61;
    note("descriptors stdin-write=%d stdout-read=%d stderr-read=%d inherit-read=%d "
         "inherit-write=%d decoy=%d\n",
         in[1], out[0], err[0], inherit[0], inherit[1], decoy);
    flush(root, "records.pre");
    char inherited[16], decoyed[16];
    snprintf(inherited, sizeof inherited, "%d", inherit[1]);
    snprintf(decoyed, sizeof decoyed, "%d", decoy);
    /* The child's root operand is ".": it resolves against the requested cwd. */
    char *args[] = {(char *)helper, "c11-10", ".", inherited, decoyed, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, &actions, &attributes, args)) {
        flush(root, "records.done");
        return 10;
    }
    if (announce(child)) {
        flush(root, "records.done");
        return 62;
    }
    close(child_in), close(child_out), close(child_err), close(inherit[1]), close(decoy);
    static const char nonce[] = "e8-nonce";
    if (transfer(in[1], nonce, sizeof nonce - 1) != (ssize_t)(sizeof nonce - 1)) return 63;
    close(in[1]);
    /* Drain all three streams to EOF and see SIGCHLD, in any order. */
    struct { int fd; const char *label; char bytes[256]; size_t length; int eof; } stream[] = {
        {out[0], "stdout", {0}, 0, 0}, {err[0], "stderr", {0}, 0, 0},
        {inherit[0], "inherit", {0}, 0, 0}};
    int seen = 0;
    while (!(stream[0].eof && stream[1].eof && stream[2].eof && seen)) {
        struct pollfd wait[4];
        int count = 0;
        for (int i = 0; i < 3; i++)
            if (!stream[i].eof) wait[count++] = (struct pollfd){stream[i].fd, POLLIN, 0};
        if (!seen) wait[count++] = (struct pollfd){sigchld_pipe[0], POLLIN, 0};
        if (poll(wait, (nfds_t)count, -1) < 0) {
            if (errno == EINTR) continue;
            return 64;
        }
        for (int j = 0; j < count; j++) {
            if (!wait[j].revents) continue;
            if (wait[j].fd == sigchld_pipe[0]) {
                char token;
                if (read(sigchld_pipe[0], &token, 1) == 1) seen = 1;
                continue;
            }
            for (int i = 0; i < 3; i++) {
                if (stream[i].fd != wait[j].fd) continue;
                /* A full buffer is more output than any E8 echo produces: a
                 * fixture error, never EOF (a zero-length read would look
                 * like one and leave the child blocked on a full pipe). */
                if (stream[i].length == sizeof stream[i].bytes - 1) return 67;
                ssize_t n = read(stream[i].fd, stream[i].bytes + stream[i].length,
                                 sizeof stream[i].bytes - 1 - stream[i].length);
                if (n < 0 && errno == EINTR) continue;
                if (n < 0) return 65;
                if (n == 0) stream[i].eof = 1;
                stream[i].length += (size_t)n;
            }
        }
    }
    for (int i = 0; i < 3; i++)
        note("%s bytes=%.*s length=%zu eof=%d\n", stream[i].label, (int)stream[i].length,
             stream[i].bytes, stream[i].length, stream[i].eof);
    note("sigchld count=%d\n", (int)sigchld_count);
    int status = 0;
    int failed = reap(child, &status);
    if (posix_spawn_file_actions_destroy(&actions) || posix_spawnattr_destroy(&attributes))
        return 66;
    note("phase=done\n");
    flush(root, "records.done");
    return failed ? 11 : 0;
}

/* Apparatus for the ledger (R3): a child born into its own session, never
 * released and never reaped. Only the harness's identity-checked cleanup can
 * end it; the process-group census cannot see it. */
static int escape(const char *root, const char *helper) {
    posix_spawnattr_t attributes;
    if (posix_spawnattr_init(&attributes)) return 80;
    if (posix_spawnattr_setflags(&attributes, POSIX_SPAWN_SETSID)) return 81;
    char *args[] = {(char *)helper, "hold", (char *)root, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, NULL, &attributes, args)) return 10;
    if (announce(child)) return 82;
    return 0;
}

/* ofd-race (#190): one inherited routed descriptor written by the parent and
 * the spawned child at the same time, with nothing ordering the writes. Each
 * record is ONE write(2), never retried, so a short or failed write is counted
 * rather than completed. With one shared open file description every record
 * lands once: 20 records, 120 bytes. With copied positions both writers start
 * at 0 and overwrite each other. Ten per writer keeps the run inside the
 * userspace store's replay budget (#196). The child reports ready before the
 * parent says go, so both writers are live when writing starts. */
#define RACE_RECORDS 10
static int race(const char *root, const char *helper) {
    note("phase=pre-spawn mode=ofd-race\n");
    identity();
    char path[4096];
    path_in(path, sizeof path, root, "out.txt");
    int output = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (output < 0) return 50;
    int go[2], ready[2];
    if (pipe(go) != 0 || pipe(ready) != 0) return 51;
    if (fcntl(go[1], F_SETFD, FD_CLOEXEC) != 0 || fcntl(ready[0], F_SETFD, FD_CLOEXEC) != 0)
        return 52;
    note("descriptors output=%d go-read=%d go-write=%d ready-read=%d ready-write=%d\n", output,
         go[0], go[1], ready[0], ready[1]);
    flush(root, "records.pre");
    char o[16], g[16], r[16];
    snprintf(o, sizeof o, "%d", output);
    snprintf(g, sizeof g, "%d", go[0]);
    snprintf(r, sizeof r, "%d", ready[1]);
    char *args[] = {(char *)helper, "ofd-race", (char *)root, o, g, r, NULL};
    pid_t child = -1;
    if (spawn(&child, helper, NULL, NULL, args)) {
        flush(root, "records.done");
        return 10;
    }
    close(go[0]);
    close(ready[1]);
    char token = 0;
    ssize_t got;
    do got = read(ready[0], &token, 1);
    while (got < 0 && errno == EINTR);
    note("ready result=%zd token=%c\n", got, got == 1 ? token : '?');
    if (got != 1 || token != 'R') return 54;
    if (transfer(go[1], "G", 1) != 1) return 53;
    int short_writes = 0, first_errno = 0;
    for (int i = 0; i < RACE_RECORDS; i++) {
        char record[8];
        snprintf(record, sizeof record, "P%04d\n", i);
        errno = 0;
        ssize_t n = write(output, record, 6);
        if (n != 6) {
            short_writes++;
            if (!first_errno) first_errno = n < 0 ? errno : -1;
        }
    }
    close(go[1]);
    close(ready[0]);
    note("write-parent records=%d short=%d first-errno=%d\n", RACE_RECORDS, short_writes,
         first_errno);
    int status = 0;
    int failed = reap(child, &status);
    note("close output=%d\n", close(output));
    note("phase=done\n");
    flush(root, "records.done");
    return failed || short_writes ? 11 : 0;
}

int main(int argc, char **argv) {
    if (argc != 4) return 2;
    const char *mode = argv[1], *root = argv[2], *helper = argv[3];
    if (!strcmp(mode, "c11-01") || !strcmp(mode, "c11-02-actions") ||
        !strcmp(mode, "c11-02-attributes") || !strcmp(mode, "c11-02-both"))
        return shape(mode, root, helper);
    if (!strcmp(mode, "c11-05")) return cloexec(root, helper);
    if (!strcmp(mode, "c11-05-premise")) return premise(root);
    if (!strcmp(mode, "c11-09")) return offset(root, helper);
    if (!strcmp(mode, "c11-10")) return e8(root, helper);
    if (!strcmp(mode, "ofd-race")) return race(root, helper);
    if (!strcmp(mode, "ledger-escape")) return escape(root, helper);
    return 2;
}
