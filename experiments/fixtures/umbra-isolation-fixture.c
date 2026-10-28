/* umbra-isolation-fixture: the #193 run-isolation fixture.
 *
 * One binary, several roles, selected by argv[1]. Built by
 * crates/umbra-cli/tests/isolation.rs with clang at test time. Every report
 * line starts with "ISO " and goes to stderr unless stated otherwise.
 *
 * Tracee modes (run UNDER umbra as the supervised program; stdout is
 * /dev/null there, so reports go to fd 2):
 *   park          READY; fork a child (CHILD line) that sleeps up to 120 s;
 *                 the root waitpid()s it (umbra parks the root).
 *   sleep         READY; sleep up to 120 s; exit 0.
 *   exit <n>      READY; sleep 300 ms; exit n.
 *   setsid        READY; setsid(); SETSID rc/errno line; exit 0.
 *   desc <pgid>   READY; a child setpgid(0,0) then setpgid(0,<pgid>), one
 *                 DESC line each; the root waits for it; exit 0.
 *   tree          READY; child and grandchild (CHILD/GRANDCHILD lines) sleep
 *                 up to 120 s; root and child waitpid() (parked).
 *   ttyread       READY; open("/dev/tty"); poll 8 s; read; TTYREAD line.
 *   ttywrite      READY; write a payload line to fd 2; exit 0 if whole.
 *   wait <secs>   handlers for INT TSTP QUIT HUP CONT WINCH TTIN TTOU TERM;
 *                 READY; sleep; WAIT-DONE line with counts and the window.
 *   ioctl         READY; tcgetattr/TIOCGWINSZ/tcgetpgrp on fd 2; IOCTL line.
 *
 * Harness roles (run by the test, never under umbra):
 *   sentinel <records> <stop-file> <max-s>
 *                 a passive member of the launching group: records every
 *                 HUP CONT INT TERM QUIT TSTP TTIN TTOU WINCH it receives
 *                 ("sig=<n> si_pid=<p>" lines), never stops itself.
 *   jobc <pgid> <out> <stop-file> <max-s>
 *                 setsid(), then sample pg_jobc of <pgid> every 500 us from a
 *                 sysctl KERN_PROC_PGRP listing, reading only the entries
 *                 whose own e_pgid is <pgid> (see jobc_record; #209). Lines:
 *                 "jobc=<n> n=<n>" (a reading, on change), "moved ..." (an
 *                 entry filled in another group, never a reading), "none n=<n>"
 *                 (no own entry, on change), "DISAGREE ..." and "ERR ..."
 *                 (instrument errors); "MIN <n>" over the readings at the
 *                 end, or "MIN none" when the run produced no valid reading.
 *   jobc-select <pgid>
 *                 one listing from stdin ("pid pgid jobc" per line, kp[0]
 *                 first), classified exactly as the jobc mode classifies a
 *                 sysctl listing; stdout gets the lines jobc would write.
 *   launch <dir> <fresh|group> -- <argv...>
 *                 fresh: setsid() first. Fork the sentinel, then the program
 *                 (stdin and stdout /dev/null, stderr inherited); LAUNCH line;
 *                 wait for the program; <dir>/umbra.status gets its raw status;
 *                 then wait for <dir>/stop (max 120 s) and the sentinel.
 *   procs         stdout: one line per process,
 *                 "pid ppid pgid sid p_stat start_us comm" (sid -1 if unknown).
 *   kill-session <sid> <before-us>
 *                 SIGKILL every process whose session is <sid> and that started
 *                 before <before-us>; stdout: "KILLED <pid> <comm>" per kill.
 *                 Session-scoped ownership (L39): a process that started before
 *                 an instant when <sid> was known to be this run's session and
 *                 is in it now was in it then (sid changes only by setsid,
 *                 which cannot yield <sid>; setpgid cannot cross sessions).
 *   pty <case> <dir> -- <argv...>
 *                 the terminal harness (see pty_main): a pty, a mini
 *                 job-control shell on its slave, <argv> as the foreground job.
 *                 stdout: one "ISO PTY ..." summary line. When the tracee is
 *                 ready, <dir>/debugserver.txt gets the argv of every
 *                 debugserver in the pty's session.
 *   home          stdout: this user's home directory from the password
 *                 database (where umbra writes isolation records).
 *
 * In launch, an argv element "@G0" is replaced by the launcher's group id.
 */
#include <errno.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <poll.h>
#include <pwd.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>
#include <util.h>

extern char **environ;

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if ((size_t)n >= sizeof buf) n = sizeof buf - 1;
    (void)write(2, buf, (size_t)n);
}

static long long now_us(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (long long)tv.tv_sec * 1000000LL + tv.tv_usec;
}

static int exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static void sleep_upto(int seconds) {
    for (int i = 0; i < seconds * 10; i++) usleep(100000);
}

static void ready(const char *mode) {
    say("ISO READY mode=%s pid=%d ppid=%d pgid=%d sid=%d\n", mode, getpid(), getppid(), getpgrp(),
        getsid(0));
}

/* ------------------------------------------------------------------ tracee */

static volatile sig_atomic_t got[64];
static void on_count(int sig) {
    if (sig > 0 && sig < 64) got[sig]++;
}

static int tracee(int argc, char **argv) {
    const char *mode = argv[1];
    if (!strcmp(mode, "park")) {
        ready(mode);
        pid_t child = fork();
        if (child < 0) return 2;
        if (child == 0) {
            say("ISO CHILD pid=%d ppid=%d pgid=%d sid=%d\n", getpid(), getppid(), getpgrp(), getsid(0));
            sleep_upto(120);
            _exit(0);
        }
        int st;
        waitpid(child, &st, 0);
        return 0;
    }
    if (!strcmp(mode, "sleep")) {
        ready(mode);
        sleep_upto(120);
        return 0;
    }
    if (!strcmp(mode, "exit")) {
        int code = argc > 2 ? atoi(argv[2]) : 0;
        ready(mode);
        usleep(300000);
        return code;
    }
    if (!strcmp(mode, "setsid")) {
        ready(mode);
        errno = 0;
        pid_t r = setsid();
        int e = r < 0 ? errno : 0;
        say("ISO SETSID rc=%d errno=%d pid=%d sid=%d\n", r, e, getpid(), getsid(0));
        return 0;
    }
    if (!strcmp(mode, "desc")) {
        pid_t target = argc > 2 ? (pid_t)atoi(argv[2]) : 0;
        ready(mode);
        pid_t c = fork();
        if (c == 0) {
            int a = setpgid(0, 0);
            int ae = a < 0 ? errno : 0;
            say("ISO DESC step=new rc=%d errno=%d pid=%d pgid=%d\n", a, ae, getpid(), getpgrp());
            int b = setpgid(0, target);
            int be = b < 0 ? errno : 0;
            say("ISO DESC step=join target=%d rc=%d errno=%d pid=%d pgid=%d\n", target, b, be, getpid(),
                getpgrp());
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        return 0;
    }
    if (!strcmp(mode, "tree")) {
        ready(mode);
        pid_t c = fork();
        if (c == 0) {
            pid_t g = fork();
            if (g == 0) {
                say("ISO GRANDCHILD pid=%d ppid=%d sid=%d\n", getpid(), getppid(), getsid(0));
                sleep_upto(120);
                _exit(0);
            }
            say("ISO CHILD pid=%d ppid=%d sid=%d\n", getpid(), getppid(), getsid(0));
            int st;
            waitpid(g, &st, 0);
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        return 0;
    }
    if (!strcmp(mode, "ttyread")) {
        ready(mode);
        int fd = open("/dev/tty", O_RDONLY);
        int oe = fd < 0 ? errno : 0;
        int pr = -2;
        ssize_t n = -2;
        if (fd >= 0) {
            struct pollfd p = {.fd = fd, .events = POLLIN};
            pr = poll(&p, 1, 8000);
            char data[64];
            if (pr > 0) n = read(fd, data, sizeof data);
        }
        say("ISO TTYREAD fd=%d errno=%d poll=%d read=%zd\n", fd, oe, pr, n);
        return 0;
    }
    if (!strcmp(mode, "ttywrite")) {
        ready(mode);
        static const char payload[] = "ISO TTYWRITE-PAYLOAD 0123456789\n";
        ssize_t n = write(2, payload, sizeof payload - 1);
        if (n == (ssize_t)(sizeof payload - 1)) return 0;
        int e = errno;
        return 10 + (e > 110 ? 110 : e);
    }
    if (!strcmp(mode, "wait")) {
        int secs = argc > 2 ? atoi(argv[2]) : 10;
        int sigs[] = {SIGINT, SIGTSTP, SIGQUIT, SIGHUP, SIGCONT, SIGWINCH, SIGTTIN, SIGTTOU, SIGTERM};
        for (size_t i = 0; i < sizeof sigs / sizeof sigs[0]; i++) {
            struct sigaction sa;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = on_count;
            sigaction(sigs[i], &sa, NULL);
        }
        ready(mode);
        sleep_upto(secs);
        struct winsize ws;
        int wr = ioctl(2, TIOCGWINSZ, &ws);
        say("ISO WAIT-DONE winch=%d int=%d tstp=%d quit=%d hup=%d cont=%d rows=%d cols=%d\n",
            got[SIGWINCH], got[SIGINT], got[SIGTSTP], got[SIGQUIT], got[SIGHUP], got[SIGCONT],
            wr == 0 ? ws.ws_row : -1, wr == 0 ? ws.ws_col : -1);
        return 0;
    }
    if (!strcmp(mode, "ioctl")) {
        ready(mode);
        struct termios t;
        int a = tcgetattr(2, &t);
        int ae = a < 0 ? errno : 0;
        struct winsize ws;
        int b = ioctl(2, TIOCGWINSZ, &ws);
        int be = b < 0 ? errno : 0;
        pid_t c = tcgetpgrp(2);
        int ce = c < 0 ? errno : 0;
        say("ISO IOCTL tcgetattr=%d/%d winsz=%d/%d tcgetpgrp=%d/%d isatty2=%d\n", a, ae, b, be, c, ce,
            isatty(2));
        return 0;
    }
    say("ISO UNKNOWN-MODE %s\n", mode);
    return 2;
}

/* ---------------------------------------------------------------- sentinel */

static int sentinel_fd = -1;

static char *put(char *p, const char *s) {
    while (*s) *p++ = *s++;
    return p;
}
static char *num(char *p, long long v) {
    char t[24];
    int n = 0;
    unsigned long long u = v < 0 ? (unsigned long long)(-v) : (unsigned long long)v;
    do {
        t[n++] = (char)('0' + u % 10);
        u /= 10;
    } while (u);
    if (v < 0) *p++ = '-';
    while (n) *p++ = t[--n];
    return p;
}

static void on_sentinel(int sig, siginfo_t *info, void *ctx) {
    (void)ctx;
    char buf[128], *p = buf;
    p = put(p, "sig=");
    p = num(p, sig);
    p = put(p, " si_pid=");
    p = num(p, info ? info->si_pid : -1);
    *p++ = '\n';
    (void)write(sentinel_fd, buf, (size_t)(p - buf));
}

static int sentinel(char **argv) {
    sentinel_fd = open(argv[2], O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (sentinel_fd < 0) return 3;
    int sigs[] = {SIGHUP, SIGCONT, SIGINT, SIGTERM, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
    for (size_t i = 0; i < sizeof sigs / sizeof sigs[0]; i++) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_sigaction = on_sentinel;
        sa.sa_flags = SA_SIGINFO | SA_RESTART;
        sigaction(sigs[i], &sa, NULL);
    }
    dprintf(sentinel_fd, "START pid=%d ppid=%d pgid=%d sid=%d\n", getpid(), getppid(), getpgrp(), getsid(0));
    int max = atoi(argv[4]);
    for (int i = 0; i < max * 20 && !exists(argv[3]); i++) usleep(50000);
    dprintf(sentinel_fd, "STOP\n");
    return 0;
}

/* -------------------------------------------------------------------- jobc */

/* G0's listing: every process whose group was <pgid> when the kernel filtered
 * it. Returns the entry count (into *kp, which the caller frees), or -1 with
 * errno set. */
static int jobc_listing(int pgid, struct kinfo_proc **kp) {
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PGRP, pgid};
    size_t len = 0;
    *kp = NULL;
    if (sysctl(mib, 4, NULL, &len, NULL, 0) < 0) return -1;
    len += 8 * sizeof(struct kinfo_proc);
    *kp = malloc(len);
    if (!*kp) return -1;
    if (sysctl(mib, 4, *kp, &len, NULL, 0) < 0) return -1;
    return (int)(len / sizeof(struct kinfo_proc));
}

struct jobc_state {
    int last, lastn, min, none;
};

/* Classify one listing and write its lines (#209). xnu admits an entry on its
 * group at filter time but fills e_pgid and e_jobc later, from the group it is
 * in then (xnu-12377.121.6 kern_sysctl.c:736-743, :1083-1087; kern_proc.c
 * :4264-4277). A process spawned with POSIX_SPAWN_SETSID is listed in G0 until
 * its setsid, so it can be filled with its new group's pg_jobc of 0: listed
 * first as the newest process, it was the old reading (kp[0].e_jobc). Only
 * entries whose own e_pgid is <pgid> are read; any other entry is logged as
 * "moved" and never read. The own entries share G0's pgrp, but the listing is
 * not atomic: entries that disagree fail conservatively ("DISAGREE"), never
 * a reading. */
static void jobc_record(FILE *out, long long t, int pgid, const struct kinfo_proc *kp, int n,
                        struct jobc_state *st) {
    int own = 0, jobc = 0, disagree = 0;
    for (int i = 0; i < n; i++) {
        int g = kp[i].kp_eproc.e_pgid, j = kp[i].kp_eproc.e_jobc;
        if (g != pgid) {
            fprintf(out, "%lld moved pid=%d pgid=%d ej=%d n=%d\n", t, kp[i].kp_proc.p_pid, g, j, n);
            continue;
        }
        if (own && j != jobc) disagree = 1;
        if (!own) jobc = j;
        own++;
    }
    if (disagree) {
        fprintf(out, "%lld DISAGREE n=%d own=", t, n);
        for (int i = 0, first = 1; i < n; i++)
            if (kp[i].kp_eproc.e_pgid == pgid) {
                fprintf(out, "%s%d:%d", first ? "" : ",", kp[i].kp_proc.p_pid, kp[i].kp_eproc.e_jobc);
                first = 0;
            }
        fprintf(out, "\n");
        st->lastn = -1;
        return;
    }
    if (!own) {
        if (!st->none || n != st->lastn) fprintf(out, "%lld none n=%d\n", t, n);
        st->none = 1;
        st->lastn = n;
        return;
    }
    if (jobc < st->min) st->min = jobc;
    if (st->none || jobc != st->last || n != st->lastn) fprintf(out, "%lld jobc=%d n=%d\n", t, jobc, n);
    st->none = 0;
    st->last = jobc;
    st->lastn = n;
}

static int jobc_main(char **argv) {
    /* Its own session: never a member of the group it watches. */
    if (setsid() < 0) return 4;
    int pgid = atoi(argv[2]);
    FILE *out = fopen(argv[3], "w");
    if (!out) return 3;
    setvbuf(out, NULL, _IOLBF, 0);
    long long end = now_us() + atoll(argv[5]) * 1000000LL;
    struct jobc_state st = {-1000, -1, 1 << 30, 0};
    fprintf(out, "START pid=%d pgid=%d\n", getpid(), pgid);
    while (now_us() < end && !exists(argv[4])) {
        struct kinfo_proc *kp;
        int n = jobc_listing(pgid, &kp);
        if (n < 0) {
            int e = errno;
            fprintf(out, "%lld ERR errno=%d\n", now_us(), e);
            st.lastn = -1;
        } else {
            jobc_record(out, now_us(), pgid, kp, n, &st);
        }
        free(kp);
        usleep(500);
    }
    /* "MIN none": the run produced no valid reading of G0's own entries. */
    if (st.min == 1 << 30)
        fprintf(out, "MIN none\n");
    else
        fprintf(out, "MIN %d\n", st.min);
    return 0;
}

/* jobc-select: classify one listing read from stdin, as jobc_main would. */
static int jobc_select(char **argv) {
    int pgid = atoi(argv[2]), n = 0;
    struct kinfo_proc kp[64];
    memset(kp, 0, sizeof kp);
    int pid, g, j;
    while (n < 64 && scanf("%d %d %d", &pid, &g, &j) == 3) {
        kp[n].kp_proc.p_pid = pid;
        kp[n].kp_eproc.e_pgid = g;
        kp[n].kp_eproc.e_jobc = (short)j;
        n++;
    }
    struct jobc_state st = {-1000, -1, 1 << 30, 0};
    jobc_record(stdout, 0, pgid, kp, n, &st);
    return 0;
}

/* ------------------------------------------------------------------ launch */

static int launch(int argc, char **argv) {
    if (argc < 6 || strcmp(argv[4], "--")) return 2;
    const char *dir = argv[2];
    if (!strcmp(argv[3], "fresh") && setsid() < 0) {
        say("ISO LAUNCH-SETSID errno=%d\n", errno);
        return 4;
    }
    char rec[1024], stop[1024], status[1024], self[1024];
    snprintf(rec, sizeof rec, "%s/sentinel.log", dir);
    snprintf(stop, sizeof stop, "%s/stop", dir);
    snprintf(status, sizeof status, "%s/umbra.status", dir);
    uint32_t size = sizeof self;
    if (_NSGetExecutablePath(self, &size) != 0) return 5;
    pid_t s = fork();
    if (s == 0) {
        char *args[] = {self, "sentinel", rec, stop, "120", NULL};
        execv(self, args);
        _exit(127);
    }
    /* The sentinel records before the program starts. */
    for (int i = 0; i < 200 && !exists(rec); i++) usleep(5000);
    /* "@G0" in the program's argv is this launcher's group. */
    char g0[16];
    snprintf(g0, sizeof g0, "%d", getpgrp());
    for (int i = 5; i < argc; i++)
        if (!strcmp(argv[i], "@G0")) argv[i] = g0;
    pid_t u = fork();
    if (u == 0) {
        int null = open("/dev/null", O_RDWR);
        dup2(null, 0);
        dup2(null, 1);
        execv(argv[5], &argv[5]);
        _exit(127);
    }
    say("ISO LAUNCH pid=%d pgid=%d sid=%d sentinel=%d umbra=%d\n", getpid(), getpgrp(), getsid(0), s, u);
    int st = 0;
    while (waitpid(u, &st, 0) < 0 && errno == EINTR) {
    }
    FILE *f = fopen(status, "w");
    if (f) {
        fprintf(f, "%d\n", st);
        fclose(f);
    }
    say("ISO UMBRA-EXIT status=%d\n", st);
    for (int i = 0; i < 2400 && !exists(stop); i++) usleep(50000);
    waitpid(s, &st, 0);
    return 0;
}

/* ------------------------------------------------------------- proc table */

struct row {
    pid_t pid, ppid, pgid, sid;
    int stat;
    long long start;
    char comm[MAXCOMLEN + 1];
};

static struct row *table(size_t *count) {
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_ALL, 0};
    size_t len = 0;
    if (sysctl(mib, 4, NULL, &len, NULL, 0) < 0) return NULL;
    len += 64 * sizeof(struct kinfo_proc);
    struct kinfo_proc *kp = malloc(len);
    if (!kp) return NULL;
    if (sysctl(mib, 4, kp, &len, NULL, 0) < 0) {
        free(kp);
        return NULL;
    }
    size_t n = len / sizeof(struct kinfo_proc);
    struct row *rows = calloc(n ? n : 1, sizeof *rows);
    if (!rows) {
        free(kp);
        return NULL;
    }
    for (size_t i = 0; i < n; i++) {
        struct row *r = &rows[i];
        r->pid = kp[i].kp_proc.p_pid;
        r->ppid = kp[i].kp_eproc.e_ppid;
        r->pgid = kp[i].kp_eproc.e_pgid;
        r->sid = getsid(r->pid);
        r->stat = kp[i].kp_proc.p_stat;
        r->start = (long long)kp[i].kp_proc.p_starttime.tv_sec * 1000000LL + kp[i].kp_proc.p_starttime.tv_usec;
        memcpy(r->comm, kp[i].kp_proc.p_comm, MAXCOMLEN);
        r->comm[MAXCOMLEN] = 0;
        for (char *c = r->comm; *c; c++)
            if (*c == ' ') *c = '_';
    }
    free(kp);
    *count = n;
    return rows;
}

static int procs(void) {
    size_t n = 0;
    struct row *rows = table(&n);
    if (!rows) return 3;
    for (size_t i = 0; i < n; i++)
        printf("%d %d %d %d %d %lld %s\n", rows[i].pid, rows[i].ppid, rows[i].pgid, rows[i].sid, rows[i].stat,
               rows[i].start, rows[i].comm[0] ? rows[i].comm : "-");
    free(rows);
    return 0;
}

/* Kill every process in session `sid` that started before `before`.
 * Returns the number killed; prints each kill to `out`. */
static int kill_session(pid_t sid, long long before, FILE *out, pid_t spare) {
    if (sid <= 1) return 0;
    size_t n = 0;
    struct row *rows = table(&n);
    if (!rows) return 0;
    int killed = 0;
    for (size_t i = 0; i < n; i++) {
        struct row *r = &rows[i];
        if (r->sid != sid || r->pid == spare || r->start >= before || r->stat == 5 /* SZOMB */) continue;
        /* Re-check right before the signal. */
        if (getsid(r->pid) != sid) continue;
        if (kill(r->pid, SIGKILL) == 0) {
            if (out) fprintf(out, "KILLED %d %s\n", r->pid, r->comm);
            killed++;
        }
    }
    free(rows);
    return killed;
}

/* Write the argv of every debugserver in session `sid` to `path`, one line
 * each (read-only; `ps` prints the argv of a process this user owns). */
static void debugserver_args(pid_t sid, const char *path) {
    FILE *out = fopen(path, "w");
    if (!out) return;
    size_t n = 0;
    struct row *rows = table(&n);
    for (size_t i = 0; rows && i < n; i++) {
        if (rows[i].sid != sid || strcmp(rows[i].comm, "debugserver")) continue;
        char cmd[64], line[4096];
        snprintf(cmd, sizeof cmd, "/bin/ps -o args= -p %d", rows[i].pid);
        FILE *ps = popen(cmd, "r");
        if (!ps) continue;
        while (fgets(line, sizeof line, ps)) fputs(line, out);
        pclose(ps);
    }
    free(rows);
    fclose(out);
}

/* --------------------------------------------------------------------- pty */

/* The terminal harness, ported from the #193 design harness (b193-tty.py).
 *
 *   O (this process)  holds the pty master; types the case's input
 *   M (mini-shell)    new session, the slave is its controlling terminal;
 *                     ignores TTOU/TTIN/TSTP like a shell
 *   J (the job)       forked by M, own group, the foreground group; execs argv
 *
 * M waits for J with WUNTRACED. On a stop it takes the terminal back, waits
 * 1 s, then does `fg`. On SIGHUP it forwards HUP and CONT to J's group (as zsh
 * does), waits up to 3 s for J, and exits. When J ends, M records tcgetpgrp()
 * before reclaiming the terminal. M writes <dir>/mshell.txt; it never writes
 * to the terminal. O keeps M unreaped (the session id stays M's) while it
 * kills what is left of the session, then reaps it.
 */
static volatile sig_atomic_t m_hup;
static void on_m_hup(int sig) {
    (void)sig;
    m_hup = 1;
}

static void mshell(const char *dir, const char *tcase, char **cmd) {
    char path[1024];
    snprintf(path, sizeof path, "%s/mshell.txt", dir);
    FILE *rec = fopen(path, "w");
    if (!rec) _exit(3);
    setvbuf(rec, NULL, _IOLBF, 0);
    signal(SIGTTOU, SIG_IGN);
    signal(SIGTTIN, SIG_IGN);
    signal(SIGTSTP, SIG_IGN);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_m_hup;
    sigaction(SIGHUP, &sa, NULL);
    struct termios t;
    tcgetattr(0, &t);
    if (!strcmp(tcase, "T3"))
        t.c_lflag |= TOSTOP;
    else
        t.c_lflag &= ~(tcflag_t)TOSTOP;
    tcsetattr(0, TCSANOW, &t);
    pid_t j = fork();
    if (j == 0) {
        setpgid(0, 0);
        tcsetpgrp(0, getpid());
        signal(SIGTTOU, SIG_DFL);
        signal(SIGTTIN, SIG_DFL);
        signal(SIGTSTP, SIG_DFL);
        signal(SIGHUP, SIG_DFL);
        execve(cmd[0], cmd, environ);
        _exit(127);
    }
    setpgid(j, j);
    tcsetpgrp(0, j);
    fprintf(rec, "job=%d shell_pgid=%d\n", j, getpgrp());
    long long deadline = now_us() + 30000000LL, hup_until = 0;
    for (;;) {
        if (now_us() > deadline) {
            fprintf(rec, "deadline\n");
            break;
        }
        if (m_hup && !hup_until) {
            fprintf(rec, "sighup\n");
            killpg(j, SIGHUP);
            killpg(j, SIGCONT);
            hup_until = now_us() + 3000000LL;
        }
        if (hup_until && now_us() > hup_until) {
            fprintf(rec, "hup-timeout\n");
            break;
        }
        int st = 0;
        pid_t r = waitpid(j, &st, WNOHANG | WUNTRACED);
        if (r < 0 && errno == EINTR) continue;
        if (r < 0) {
            fprintf(rec, "echild\n");
            break;
        }
        if (r == 0) {
            usleep(20000);
            continue;
        }
        if (WIFSTOPPED(st)) {
            fprintf(rec, "stopped=%d\n", WSTOPSIG(st));
            tcsetpgrp(0, getpgrp());
            sleep(1);
            tcsetpgrp(0, j);
            killpg(j, SIGCONT);
            continue;
        }
        pid_t owner = tcgetpgrp(0);
        if (WIFEXITED(st))
            fprintf(rec, "ended=exit:%d owner_is_job=%d\n", WEXITSTATUS(st), owner == j);
        else
            fprintf(rec, "ended=signal:%d owner_is_job=%d\n", WTERMSIG(st), owner == j);
        tcsetpgrp(0, getpgrp());
        break;
    }
    fclose(rec);
    _exit(0);
}

static int pty_main(int argc, char **argv) {
    if (argc < 6 || strcmp(argv[4], "--")) return 2;
    const char *tcase = argv[2], *dir = argv[3];
    char **cmd = &argv[5];
    struct winsize ws = {.ws_row = 24, .ws_col = 80};
    int master = -1;
    pid_t m = forkpty(&master, NULL, NULL, &ws);
    if (m < 0) return 4;
    if (m == 0) mshell(dir, tcase, cmd);

    char tpath[1024];
    snprintf(tpath, sizeof tpath, "%s/transcript.log", dir);
    FILE *transcript = fopen(tpath, "w");
    char buf[65536];
    size_t have = 0;
    long long ready_at = 0, end = now_us() + 35000000LL;
    int induced = 0, closed = 0, timed_out = 1;
    siginfo_t info;
    while (now_us() < end) {
        if (!closed) {
            struct pollfd p = {.fd = master, .events = POLLIN};
            if (poll(&p, 1, 50) > 0) {
                ssize_t n = read(master, buf + have, sizeof buf - 1 - have);
                if (n <= 0) {
                    closed = 1;
                } else {
                    if (transcript) fwrite(buf + have, 1, (size_t)n, transcript);
                    have += (size_t)n;
                    buf[have] = 0;
                    if (have > sizeof buf - 4096) {
                        memmove(buf, buf + 32768, have - 32768);
                        have -= 32768;
                        buf[have] = 0;
                    }
                    if (!ready_at && strstr(buf, "ISO READY")) {
                        ready_at = now_us();
                        char dpath[1024];
                        snprintf(dpath, sizeof dpath, "%s/debugserver.txt", dir);
                        debugserver_args(m, dpath);
                    }
                }
            }
        } else {
            usleep(10000);
        }
        if (ready_at && !induced && now_us() >= ready_at + 500000) {
            induced = 1;
            if (!strcmp(tcase, "T1"))
                (void)write(master, "hello\n", 6);
            else if (!strcmp(tcase, "T4"))
                (void)write(master, "\x03", 1);
            else if (!strcmp(tcase, "T5"))
                (void)write(master, "\x1a", 1);
            else if (!strcmp(tcase, "T6"))
                (void)write(master, "\x1c", 1);
            else if (!strcmp(tcase, "T7")) {
                struct winsize big = {.ws_row = 40, .ws_col = 100};
                ioctl(master, TIOCSWINSZ, &big);
            } else if (!strcmp(tcase, "T8")) {
                close(master);
                closed = 1;
            }
        }
        memset(&info, 0, sizeof info);
        if (waitid(P_PID, (id_t)m, &info, WEXITED | WNOHANG | WNOWAIT) == 0 && info.si_pid == m) {
            timed_out = 0;
            break;
        }
    }
    if (timed_out) kill(m, SIGKILL);
    if (!closed) {
        /* Drain what is left. */
        long long t = now_us() + 300000;
        while (now_us() < t) {
            struct pollfd p = {.fd = master, .events = POLLIN};
            if (poll(&p, 1, 50) <= 0) continue;
            ssize_t n = read(master, buf, sizeof buf - 1);
            if (n <= 0) break;
            if (transcript) fwrite(buf, 1, (size_t)n, transcript);
        }
    }
    if (transcript) fclose(transcript);
    sleep(1);
    /* M is unreaped, so its session id cannot be reused: everything in it is
     * this harness's. Count survivors (live, non-zombie) and kill them. */
    char kpath[1024];
    snprintf(kpath, sizeof kpath, "%s/killed.log", dir);
    FILE *klog = fopen(kpath, "w");
    int killed = kill_session(m, now_us() + 1, klog, m);
    if (klog) fclose(klog);
    int st;
    waitpid(m, &st, 0);
    printf("ISO PTY case=%s timed_out=%d ready=%d survivors=%d\n", tcase, timed_out, ready_at != 0, killed);
    return 0;
}

/* -------------------------------------------------------------------- main */

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    const char *mode = argv[1];
    if (!strcmp(mode, "sentinel") && argc == 5) return sentinel(argv);
    if (!strcmp(mode, "jobc") && argc == 6) return jobc_main(argv);
    if (!strcmp(mode, "jobc-select") && argc == 3) return jobc_select(argv);
    if (!strcmp(mode, "launch")) return launch(argc, argv);
    if (!strcmp(mode, "procs")) return procs();
    if (!strcmp(mode, "kill-session") && argc == 4) {
        int n = kill_session((pid_t)atoi(argv[2]), atoll(argv[3]), stdout, 0);
        return n >= 0 ? 0 : 1;
    }
    if (!strcmp(mode, "pty")) return pty_main(argc, argv);
    if (!strcmp(mode, "home")) {
        struct passwd *pw = getpwuid(getuid());
        if (!pw) return 3;
        printf("%s\n", pw->pw_dir);
        return 0;
    }
    return tracee(argc, argv);
}
