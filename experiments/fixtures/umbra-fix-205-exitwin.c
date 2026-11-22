/*
 * #205 native control (no umbra code): how the kernel hands a debugger-held
 * program back when its debugger process ends.
 *
 * Only the orderings that leave nothing behind are built in. The orderings
 * that leak a launchd-parented zombie (a pending exit, or a breakpoint hold,
 * when the debugger process ends) were measured once during the #205 design
 * and are deliberately absent: each run of one leaves a zombie that only a
 * reboot clears.
 *
 * Roles, matching umbra's topology:
 *   M  original parent (umbra's platform provider role); forks C, then T.
 *   C  test program; thread A takes the attach stop, thread B waits on a pipe
 *      and calls _exit when told to.
 *   T  debugger process: task_for_pid(C), sets C's task exception port to its
 *      own port and calls ptrace(PT_ATTACHEXC, C), as debugserver does, then
 *      receives the attach-stop exception and keeps it unanswered, so C's
 *      thread A is held.
 *
 * Modes:
 *   exit-held-release   B exits while A is held; T answers; T exits.
 *   kill-held-release   M sends SIGKILL to C while held; T answers; T exits.
 *   term-held-release   M calls task_terminate(C) while held; T answers; T exits.
 *   held-end            nothing asks C to end; T exits without answering
 *                       (a plain stop with nothing pending).
 *
 * The run's last line is `RESULT mode=<mode> C=<pid> <outcome>`, where outcome
 * is `collected by original parent M: status=<n>` when the program came back
 * to its original parent, and anything else otherwise. Build it with the
 * get-task-allow entitlement (umbra-fix-205-exitwin.entitlements).
 */
#include <errno.h>
#include <libproc.h>
#include <mach/mach.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/ptrace.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static long long t0;
static long long now_ms(void)
{
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (long long)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}
static const char *who = "M";
#define SAY(...)                                                   \
    do {                                                           \
        printf("[%s +%lldms] ", who, now_ms() - t0);               \
        printf(__VA_ARGS__);                                       \
        printf("\n");                                              \
        fflush(stdout);                                            \
    } while (0)

struct state {
    int found, status, ppid, traced, inexit;
};
static struct state read_state(pid_t pid)
{
    struct state s = {0};
    struct proc_bsdinfo b;
    memset(&b, 0, sizeof b);
    int n = proc_pidinfo(pid, PROC_PIDTBSDINFO, 1, &b, sizeof b);
    if (n == (int)sizeof b) {
        s.found = 1;
        s.status = (int)b.pbi_status;
        s.ppid = (int)b.pbi_ppid;
        s.traced = (b.pbi_flags & PROC_FLAG_TRACED) != 0;
        s.inexit = (b.pbi_flags & PROC_FLAG_INEXIT) != 0;
    } else {
        s.status = errno;
    }
    return s;
}
static void print_state(const char *label, pid_t pid)
{
    struct state s = read_state(pid);
    if (s.found)
        SAY("%s C=%d p_stat=%d ppid=%d traced=%d inexit=%d", label, pid, s.status, s.ppid,
            s.traced, s.inexit);
    else
        SAY("%s C=%d lookup failed errno=%d", label, pid, s.status);
}
static void watch(pid_t pid, int ms)
{
    struct state last = {-1, -1, -1, -1, -1};
    long long end = now_ms() + ms;
    while (now_ms() < end) {
        struct state s = read_state(pid);
        if (memcmp(&s, &last, sizeof s)) {
            print_state("watch", pid);
            last = s;
        }
        usleep(2000);
    }
}
static void put(int fd, char c) { (void)!write(fd, &c, 1); }
static char get(int fd, int ms)
{
    struct pollfd p = {.fd = fd, .events = POLLIN};
    char c = 0;
    if (poll(&p, 1, ms) == 1 && read(fd, &c, 1) == 1)
        return c;
    return 0;
}

static int go_pipe[2]; /* T -> C's thread B: end now */

static void *thread_b(void *arg)
{
    (void)arg;
    char c = get(go_pipe[0], 15000);
    _exit(c ? 0 : 9);
}

static void run_c(int ready_fd)
{
    who = "C";
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    pthread_t b;
    pthread_create(&b, NULL, thread_b, NULL); /* B inherits the full mask */
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL); /* A takes every signal */
    put(ready_fd, 'r');
    close(ready_fd);
    for (;;)
        pause();
}

/* Reply KERN_SUCCESS to the held exception request. */
static void answer(mach_msg_header_t *request)
{
    struct {
        mach_msg_header_t header;
        NDR_record_t ndr;
        kern_return_t ret;
    } reply;
    memset(&reply, 0, sizeof reply);
    reply.header.msgh_bits = MACH_MSGH_BITS(MACH_MSGH_BITS_REMOTE(request->msgh_bits), 0);
    reply.header.msgh_remote_port = request->msgh_remote_port;
    reply.header.msgh_size = sizeof reply;
    reply.header.msgh_id = request->msgh_id + 100;
    reply.ndr = NDR_record;
    reply.ret = KERN_SUCCESS;
    kern_return_t kr = mach_msg(&reply.header, MACH_SEND_MSG, sizeof reply, 0, MACH_PORT_NULL,
                                MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
    SAY("answered the held exception: mach_msg=%d", kr);
}

static int run_t(const char *mode, pid_t c, int to_m, int from_m)
{
    who = "T";
    alarm(20);
    mach_port_t task = MACH_PORT_NULL;
    kern_return_t kr = task_for_pid(mach_task_self(), c, &task);
    SAY("task_for_pid=%d", kr);
    if (kr != KERN_SUCCESS)
        return 71;
    mach_port_t exc = MACH_PORT_NULL;
    kr = mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &exc);
    if (kr == KERN_SUCCESS)
        kr = mach_port_insert_right(mach_task_self(), exc, exc, MACH_MSG_TYPE_MAKE_SEND);
    if (kr == KERN_SUCCESS)
        kr = task_set_exception_ports(task, EXC_MASK_SOFTWARE | EXC_MASK_BREAKPOINT, exc,
                                      (exception_behavior_t)(EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES),
                                      THREAD_STATE_NONE);
    SAY("exception port set: %d", kr);
    if (kr != KERN_SUCCESS)
        return 72;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    int rc = ptrace(PT_ATTACHEXC, c, 0, 0);
#pragma clang diagnostic pop
    SAY("PT_ATTACHEXC=%d errno=%d", rc, rc ? errno : 0);
    if (rc)
        return 73;
    union {
        mach_msg_header_t header;
        char bytes[2048];
    } msg;
    memset(&msg, 0, sizeof msg);
    kr = mach_msg(&msg.header, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, sizeof msg, exc, 5000,
                  MACH_PORT_NULL);
    SAY("held exception received: mach_msg=%d id=%d", kr, msg.header.msgh_id);
    if (kr != KERN_SUCCESS)
        return 74;
    print_state("held", c);
    int releases = strcmp(mode, "held-end") != 0;
    put(to_m, 'h');
    if (!strcmp(mode, "exit-held-release")) {
        SAY("telling C's thread B to _exit(0)");
        put(go_pipe[1], 'x');
    } else if (strcmp(mode, "held-end")) {
        char a = get(from_m, 5000);
        SAY("M acted: %c", a ? a : '?');
    }
    watch(c, 500);
    if (releases) {
        answer(&msg.header);
        watch(c, 500);
        SAY("debugger process exits after answering");
    } else {
        SAY("debugger process exits with the exception unanswered");
    }
    return 0;
}

static void *terminate_thread(void *arg)
{
    mach_port_t task = (mach_port_t)(uintptr_t)arg;
    long long at = now_ms() - t0;
    kern_return_t kr = task_terminate(task);
    SAY("task_terminate(C) called at +%lldms returned %d", at, kr);
    return NULL;
}

int main(int argc, char **argv)
{
    t0 = now_ms();
    if (argc != 2) {
        fprintf(stderr, "usage: %s <mode>\n", argv[0]);
        return 64;
    }
    const char *mode = argv[1];
    if (strcmp(mode, "exit-held-release") && strcmp(mode, "kill-held-release") &&
        strcmp(mode, "term-held-release") && strcmp(mode, "held-end")) {
        fprintf(stderr, "unknown mode %s (only the orderings that leave nothing behind exist)\n",
                mode);
        return 64;
    }
    alarm(25);
    int ready[2], t_to_m[2], m_to_t[2];
    if (pipe(go_pipe) || pipe(ready) || pipe(t_to_m) || pipe(m_to_t))
        return 65;
    pid_t c_pid = fork();
    if (c_pid < 0)
        return 66;
    if (c_pid == 0) {
        close(go_pipe[1]);
        close(ready[0]);
        close(t_to_m[0]);
        close(t_to_m[1]);
        close(m_to_t[0]);
        close(m_to_t[1]);
        run_c(ready[1]);
    }
    close(ready[1]);
    if (!get(ready[0], 5000)) {
        kill(c_pid, SIGKILL);
        waitpid(c_pid, NULL, 0);
        return 67;
    }
    pid_t t_pid = fork();
    if (t_pid < 0) {
        kill(c_pid, SIGKILL);
        waitpid(c_pid, NULL, 0);
        return 68;
    }
    if (t_pid == 0) {
        close(go_pipe[0]);
        close(t_to_m[0]);
        close(m_to_t[1]);
        _exit(run_t(mode, c_pid, t_to_m[1], m_to_t[0]));
    }
    close(go_pipe[0]);
    close(go_pipe[1]);
    close(t_to_m[1]);
    close(m_to_t[0]);
    SAY("mode=%s M=%d C=%d T=%d", mode, getpid(), c_pid, t_pid);
    if (!get(t_to_m[0], 8000)) {
        SAY("FAILED: debugger process never held C");
        /* No exit is pending and the hold is a plain stop: ending T first
         * hands C back to M (the held-end ordering), so nothing leaks. */
        kill(t_pid, SIGKILL);
        waitpid(t_pid, NULL, 0);
        kill(c_pid, SIGKILL);
        waitpid(c_pid, NULL, 0);
        return 69;
    }
    if (!strcmp(mode, "kill-held-release")) {
        SAY("kill(C, SIGKILL)=%d", kill(c_pid, SIGKILL));
        put(m_to_t[1], 'k');
    } else if (!strcmp(mode, "term-held-release")) {
        mach_port_t ctask = MACH_PORT_NULL;
        kern_return_t kr = task_for_pid(mach_task_self(), c_pid, &ctask);
        SAY("M task_for_pid=%d", kr);
        pthread_t th;
        pthread_create(&th, NULL, terminate_thread, (void *)(uintptr_t)ctask);
        pthread_detach(th);
        usleep(50000);
        put(m_to_t[1], 't');
    }
    int st = 0;
    pid_t w = waitpid(t_pid, &st, 0);
    SAY("debugger process reaped: waitpid=%d status=%d", w, st);
    print_state("after debugger exit", c_pid);
    long long end = now_ms() + 2000;
    for (;;) {
        pid_t r = waitpid(c_pid, &st, WNOHANG);
        if (r == c_pid) {
            SAY("RESULT mode=%s C=%d collected by original parent M: status=%d", mode, c_pid, st);
            return 0;
        }
        if (r < 0) {
            int e = errno;
            struct state s = read_state(c_pid);
            SAY("RESULT mode=%s C=%d not M's child (waitpid errno=%d): found=%d p_stat=%d "
                "ppid=%d traced=%d inexit=%d",
                mode, c_pid, e, s.found, s.status, s.ppid, s.traced, s.inexit);
            return 1;
        }
        if (now_ms() > end) {
            print_state("RESULT still M's child, not collectable", c_pid);
            kill(c_pid, SIGKILL);
            usleep(200000);
            waitpid(c_pid, &st, WNOHANG);
            return 1;
        }
        usleep(5000);
    }
}
