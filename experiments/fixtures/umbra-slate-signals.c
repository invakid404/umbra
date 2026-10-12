/* Rank-11 test-only signal observer, built as a dylib and injected with
 * DYLD_INSERT_LIBRARIES into a native fixture, or into `umbra` (and from there
 * the providers it starts, which inherit its environment). No production
 * executable links this file. It exists to give #186 a sender for a fatal
 * signal, so it records and then lets the signal do what it would have done.
 *
 * Output is one file per process, `<UMBRA_SLATE_SIGNAL_DIR>/<pid>.sig`, opened
 * in the constructor (normal context) before any handler exists. Each record
 * is one fixed 104-byte write; the harness decodes them. The handler uses
 * only getpid, getppid, getpgrp, clock_gettime, memcpy, memset, write and
 * raise, all on the POSIX.1-2008 TC2 async-signal-safe list. Darwin's
 * clock_gettime is listed there, not independently audited here.
 *
 * Fatal semantics are preserved, never replaced: a signal is observed only if
 * its disposition is SIG_DFL at load (anything else is recorded as declined,
 * never chained); the handler is SA_RESETHAND, so the default is back before
 * it runs, and it re-raises the same signal after recording. The re-raise is
 * synthetic and takes the default action without reaching this handler, so it
 * produces no record of its own and never stands in for a sender.
 *
 * What it cannot see: a process whose environment drops DYLD_INSERT_LIBRARIES
 * (debugserver, the routed tracee, SIP-protected tools), SIGKILL/SIGSTOP, and a
 * handler a program installs over this one later. The destructor reports which
 * handlers were still this file's at a normal exit; no destructor record means
 * that was not observed, not that the handler survived. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

enum { RECORD_INSTALLED = 0, RECORD_DELIVERED = 1, RECORD_DECLINED = 2, RECORD_INTACT = 3 };

struct slate_signal_record {
    uint32_t magic;   /* 'SLAT' */
    uint16_t version; /* 2 */
    uint16_t size;    /* sizeof(struct slate_signal_record) */
    uint32_t kind;
    uint32_t sequence;
    int32_t recipient, parent, group;
    int32_t signal_number, code, sender;
    uint32_t sender_uid;
    uint32_t mask;    /* installed / declined / intact signal bits */
    uint32_t dropped; /* earlier records whose write was short or failed */
    uint32_t reserved;
    int64_t seconds, nanoseconds; /* CLOCK_REALTIME */
    char name[32];
};
_Static_assert(sizeof(struct slate_signal_record) == 104, "record layout is a wire format");

static const int observed[] = {SIGHUP, SIGINT, SIGQUIT, SIGPIPE, SIGTERM};
static int output = -1;
static char program[32];
static volatile sig_atomic_t sequence, dropped;

static void emit(uint32_t kind, int number, const siginfo_t *info, uint32_t mask) {
    struct slate_signal_record record;
    memset(&record, 0, sizeof record);
    record.magic = 0x534c4154;
    record.version = 2;
    record.size = sizeof record;
    record.kind = kind;
    record.sequence = (uint32_t)sequence++;
    record.recipient = getpid();
    record.parent = getppid();
    record.group = getpgrp();
    record.signal_number = number;
    if (info) {
        record.code = info->si_code;
        record.sender = info->si_pid;
        record.sender_uid = info->si_uid;
    }
    record.mask = mask;
    record.dropped = (uint32_t)dropped;
    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now) == 0) {
        record.seconds = now.tv_sec;
        record.nanoseconds = now.tv_nsec;
    }
    memcpy(record.name, program, sizeof record.name);
    /* One attempt. A short or failed write is counted, never retried, and the
     * harness treats a missing delivery record as a coverage gap. */
    if (write(output, &record, sizeof record) != (ssize_t)sizeof record) dropped++;
}

static void observe(int number, siginfo_t *info, void *context) {
    (void)context;
    int saved = errno;
    emit(RECORD_DELIVERED, number, info, 0);
    errno = saved;
    (void)raise(number);
}

__attribute__((constructor)) static void install_observer(void) {
    const char *directory = getenv("UMBRA_SLATE_SIGNAL_DIR");
    if (!directory) return;
    const char *name = getprogname();
    strncpy(program, name ? name : "?", sizeof program - 1);
    char path[1024];
    int n = snprintf(path, sizeof path, "%s/%d.sig", directory, getpid());
    if (n < 0 || (size_t)n >= sizeof path) return;
    output = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
    if (output < 0) return;
    /* Out of the fixtures' numbering: the lowest free number is theirs to
     * measure (C11-05 and the C11-10 census depend on it). */
    int relocated = fcntl(output, F_DUPFD_CLOEXEC, 900);
    if (relocated >= 0) {
        close(output);
        output = relocated;
    }
    uint32_t installed = 0, declined = 0;
    for (size_t i = 0; i < sizeof observed / sizeof observed[0]; i++) {
        struct sigaction previous;
        if (sigaction(observed[i], NULL, &previous) != 0 || previous.sa_handler != SIG_DFL) {
            declined |= 1u << observed[i];
            continue;
        }
        struct sigaction action;
        memset(&action, 0, sizeof action);
        sigemptyset(&action.sa_mask);
        action.sa_sigaction = observe;
        action.sa_flags = SA_SIGINFO | SA_RESETHAND;
        if (sigaction(observed[i], &action, NULL) == 0) installed |= 1u << observed[i];
        else declined |= 1u << observed[i];
    }
    emit(RECORD_INSTALLED, 0, NULL, installed);
    if (declined) emit(RECORD_DECLINED, 0, NULL, declined);
}

__attribute__((destructor)) static void attest_observer(void) {
    if (output < 0) return;
    uint32_t intact = 0;
    for (size_t i = 0; i < sizeof observed / sizeof observed[0]; i++) {
        struct sigaction current;
        if (sigaction(observed[i], NULL, &current) == 0 && (current.sa_flags & SA_SIGINFO) &&
            current.sa_sigaction == observe)
            intact |= 1u << observed[i];
    }
    emit(RECORD_INTACT, 0, NULL, intact);
}
