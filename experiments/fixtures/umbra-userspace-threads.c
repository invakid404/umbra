/* Rank-10 fixtures. c10-01 through c10-06; later modes are deliberately absent.
 * Records are buffered on the originating thread before the record writer runs.
 * A record is evidence, never the sole oracle for the output bytes. */
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

struct rendezvous { pthread_mutex_t mutex; pthread_cond_t changed; unsigned phase; };
static void checked(int rc) { if (rc != 0) exit(90); }
static void rendezvous_init(struct rendezvous *r) {
    checked(pthread_mutex_init(&r->mutex, NULL));
    checked(pthread_cond_init(&r->changed, NULL)); r->phase = 0;
}
static void rendezvous_publish(struct rendezvous *r, unsigned phase) {
    checked(pthread_mutex_lock(&r->mutex)); r->phase = phase;
    checked(pthread_cond_broadcast(&r->changed)); checked(pthread_mutex_unlock(&r->mutex));
}
static void rendezvous_wait(struct rendezvous *r, unsigned phase) {
    checked(pthread_mutex_lock(&r->mutex));
    while (r->phase < phase) checked(pthread_cond_wait(&r->changed, &r->mutex));
    checked(pthread_mutex_unlock(&r->mutex));
}
static void rendezvous_destroy(struct rendezvous *r) {
    checked(pthread_cond_destroy(&r->changed)); checked(pthread_mutex_destroy(&r->mutex));
}
/* The helper cannot unlock this mutex after publishing readiness except by
 * entering cond_wait with phase==0. Main's subsequent acquisition witnesses
 * the atomic release in that wait, not an already-satisfied predicate.
 * Spurious returns are counted and re-enter the predicate loop. This proves
 * a blocking API handoff; it does not claim a duration of kernel sleep. */
struct helper_state {
    struct rendezvous gate;
    uint64_t tid;
    unsigned ready, wait_calls, wait_returns, finished;
};
static void *keep_alive(void *value) {
    struct helper_state *s = value;
    checked(pthread_mutex_lock(&s->gate.mutex));
    checked(pthread_threadid_np(NULL, &s->tid));
    s->ready = 1;
    checked(pthread_cond_broadcast(&s->gate.changed));
    while (s->gate.phase == 0) {
        s->wait_calls++;
        checked(pthread_cond_wait(&s->gate.changed, &s->gate.mutex));
        s->wait_returns++;
    }
    s->finished = 1;
    checked(pthread_mutex_unlock(&s->gate.mutex));
    return NULL;
}
struct record { const char *op; long result; int error; uint64_t tid; int fd; unsigned round; };
static _Thread_local struct record records[64];
static _Thread_local unsigned count, current_round;
static _Thread_local int current_fd;
static void capture(const char *op, long result, int error) {
    if (count == 64) exit(91);
    uint64_t caller; checked(pthread_threadid_np(NULL, &caller));
    records[count++] = (struct record){op, result, error, caller, current_fd, current_round};
}
static void transfer(int fd, char *bytes, size_t length, int writing) {
    current_fd = fd;
    size_t offset = 0;
    while (offset < length) {
        errno = 0;
        ssize_t n = writing ? write(fd, bytes + offset, length - offset)
                            : read(fd, bytes + offset, length - offset);
        int saved = errno;
        capture(writing ? "write" : "read", n, saved);
        if (n < 0 && saved == EINTR) continue;
        if (n <= 0) exit(92);
        offset += (size_t)n;
    }
}
static void close_recorded(int fd) {
    current_fd = fd;
    errno = 0; int rc = close(fd); int saved = errno;
    capture("close", rc, saved); if (rc != 0) exit(93);
}
/* Helper-owned storage: main never performs the measured calls or populates
 * their result slots. Main holds the release mutex through its first wait,
 * ensuring helper I/O starts only after main has entered cond_wait. */
struct helper_io {
    struct rendezvous gate;
    const char *input_path, *output_path;
    uint64_t tid;
    struct record calls[64];
    unsigned count, entered, done, main_waits;
    int result;
};
static void *helper_io_run(void *value) {
    struct helper_io *s = value;
    checked(pthread_mutex_lock(&s->gate.mutex));
    checked(pthread_threadid_np(NULL, &s->tid));
    if (s->main_waits == 0) exit(13);
    s->entered = 1;
    checked(pthread_mutex_unlock(&s->gate.mutex));
    errno = 0; int fd = open(s->input_path, O_RDONLY); int saved = errno;
    capture("open-input", fd, saved); if (fd < 0) exit(3);
    char input[6]; transfer(fd, input, sizeof input, 0); close_recorded(fd);
    if (memcmp(input, "ABCDEF", 6) != 0) exit(4);
    errno = 0; fd = open(s->output_path, O_CREAT | O_EXCL | O_WRONLY, 0600); saved = errno;
    capture("open-output", fd, saved); if (fd < 0) exit(5);
    char payload[] = "HELPER"; transfer(fd, payload, 6, 1); close_recorded(fd);
    /* Only this caller writes its thread-local slots; copy before
     * publication so main validates a helper-owned snapshot after joining. */
    s->count = count; memcpy(s->calls, records, count * sizeof records[0]);
    s->result = 0;
    checked(pthread_mutex_lock(&s->gate.mutex)); s->done = 1;
    checked(pthread_cond_broadcast(&s->gate.changed));
    checked(pthread_mutex_unlock(&s->gate.mutex));
    return NULL;
}

/* The two-party generation barrier is reused before each competing open and
 * write. A phase turn counter separately orders alternating and shared-fd I/O. */
struct team {
    struct rendezvous gate;
    unsigned arrived, generation, mode;
    const char *output;
    int fd;
    uint64_t tids[2];
    struct record log[2][64]; unsigned counts[2];
};
static void team_barrier(struct team *s) {
    checked(pthread_mutex_lock(&s->gate.mutex));
    unsigned generation = s->generation;
    if (++s->arrived == 2) {
        s->arrived = 0; s->generation++;
        checked(pthread_cond_broadcast(&s->gate.changed));
    } else {
        while (generation == s->generation)
            checked(pthread_cond_wait(&s->gate.changed, &s->gate.mutex));
    }
    checked(pthread_mutex_unlock(&s->gate.mutex));
}
static int team_open(const char *path) {
    errno = 0; int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600); int saved = errno;
    current_fd = fd; capture("open-output", fd, saved); if (fd < 0) exit(5);
    return fd;
}
static void team_actor(struct team *s, unsigned role) {
    checked(pthread_threadid_np(NULL, &s->tids[role]));
    team_barrier(s); /* both ready before any round */
    if (s->mode == 6) {
        if (role == 0) {
            current_round = 0; s->fd = team_open(s->output);
            char a[] = "AA"; transfer(s->fd, a, 2, 1);
            rendezvous_publish(&s->gate, 1); /* publishes fd and initial write */
            rendezvous_wait(&s->gate, 2);
            current_round = 2; char c[] = "C"; transfer(s->fd, c, 1, 1);
            close_recorded(s->fd);
        } else {
            rendezvous_wait(&s->gate, 1);
            current_round = 1; char b[] = "BBB"; transfer(s->fd, b, 3, 1);
            rendezvous_publish(&s->gate, 2);
        }
    } else for (unsigned round = 0; round < 4; ++round) {
        current_round = round;
        if (s->mode == 4) rendezvous_wait(&s->gate, round * 2 + role);
        else team_barrier(s); /* before open */
        char path[4096], payload[32];
        int n = snprintf(path, sizeof path, "%s.%c%02u", s->output, role ? 'H' : 'M', round);
        if (n < 0 || (size_t)n >= sizeof path) exit(16);
        if (s->mode == 4) n = snprintf(payload, sizeof payload, "%c%02u", role ? 'H' : 'M', round);
        else n = snprintf(payload, sizeof payload, "%s%02u", role ? "HELPER" : "MAIN", round);
        if (n < 0 || (size_t)n >= sizeof payload) exit(17);
        int fd = team_open(path);
        if (s->mode == 5) team_barrier(s); /* before write */
        transfer(fd, payload, (size_t)n, 1); close_recorded(fd);
        if (s->mode == 4) rendezvous_publish(&s->gate, round * 2 + role + 1);
    }
    s->counts[role] = count; memcpy(s->log[role], records, count * sizeof records[0]);
}
static void *team_worker(void *value) { team_actor(value, 1); return NULL; }
static int team_run(unsigned mode, const char *output, const char *report_path) {
    struct team s = {0}; s.mode = mode; s.output = output;
    rendezvous_init(&s.gate);
    pthread_t worker; checked(pthread_create(&worker, NULL, team_worker, &s));
    team_actor(&s, 0);
    void *result; checked(pthread_join(worker, &result));
    if (result || s.tids[0] == s.tids[1]) return 18;
    rendezvous_destroy(&s.gate);
    char report[16384];
    size_t used = (size_t)snprintf(report, sizeof report,
        "phase=complete main_tid=%llu helper_tid=%llu mode=%u barrier_generations=%u turn=%u joined=1\n",
        (unsigned long long)s.tids[0], (unsigned long long)s.tids[1], mode, s.generation, s.gate.phase);
    for (unsigned role = 0; role < 2; ++role) for (unsigned i = 0; i < s.counts[role]; ++i) {
        struct record *r = &s.log[role][i];
        int n = snprintf(report + used, sizeof report - used,
            "%s %ld tid=%llu fd=%d round=%u role=%c errno=%d\n",
            r->op, r->result, (unsigned long long)r->tid, r->fd, r->round, role ? 'H' : 'M', r->error);
        if (n < 0 || (size_t)n >= sizeof report - used) return 6;
        used += (size_t)n;
    }
    int fd = open(report_path, O_CREAT | O_EXCL | O_WRONLY, 0600); if (fd < 0) return 7;
    size_t offset = 0;
    while (offset < used) {
        ssize_t n = write(fd, report + offset, used - offset);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 8;
        offset += (size_t)n;
    }
    return close(fd) == 0 ? 0 : 9;
}

static void marker_write(const char *path, char *bytes, size_t length) {
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600); if (fd < 0) exit(70);
    size_t offset = 0;
    while (offset < length) {
        ssize_t n=write(fd,bytes+offset,length-offset);
        if (n<0 && errno==EINTR) continue;
        if (n<=0) exit(71);
        offset+=(size_t)n;
    }
    if (close(fd)!=0) exit(72);
}
static int fork_helper(const char *child_path, const char *ready_path) {
    struct helper_state helper={0}; pthread_t worker;
    uint64_t tid; checked(pthread_threadid_np(NULL,&tid));
    rendezvous_init(&helper.gate);
    checked(pthread_create(&worker,NULL,keep_alive,&helper));
    checked(pthread_mutex_lock(&helper.gate.mutex));
    while (!helper.ready) checked(pthread_cond_wait(&helper.gate.changed,&helper.gate.mutex));
    if (!helper.wait_calls || helper.finished || helper.tid==tid) return 73;
    char ready[256]; int length=snprintf(ready,sizeof ready,
        "phase=pre-fork main_tid=%llu helper_tid=%llu ready=1 waits=%u finished=0\n",
        (unsigned long long)tid,(unsigned long long)helper.tid,helper.wait_calls);
    checked(pthread_mutex_unlock(&helper.gate.mutex));
    if (length<0 || (size_t)length>=sizeof ready) return 74;
    marker_write(ready_path,ready,(size_t)length); /* closed BEFORE fork */
    errno=0; pid_t child=fork(); int fork_errno=errno;
    if (child<0) return 75;
    if (child==0) {
        /* Async-signal-safe calls only after multithreaded fork. No inherited
         * routed descriptor: open the unique child destination here. */
        int fd=open(child_path,O_CREAT|O_EXCL|O_WRONLY,0600); if(fd<0) _exit(76);
        const char payload[]="C10-10-CHILD-WRITE-ATTEMPT\n";
        size_t offset=0;
        while(offset<sizeof payload-1) {
            ssize_t n=write(fd,payload+offset,sizeof payload-1-offset); int saved=errno;
            if(n<0 && saved==EINTR) continue;
            if(n<=0) _exit(77);
            offset+=(size_t)n;
        }
        if(close(fd)!=0) _exit(78);
        _exit(0);
    }
    rendezvous_publish(&helper.gate,1);
    void *result; checked(pthread_join(worker,&result)); if(result || !helper.finished) return 79;
    int status; errno=0; pid_t reaped=waitpid(child,&status,0); int wait_errno=errno;
    if(reaped!=child || !WIFEXITED(status) || WEXITSTATUS(status)!=0) return 80;
    rendezvous_destroy(&helper.gate);
    char done_path[4096], done[256];
    int n=snprintf(done_path,sizeof done_path,"%s.done",ready_path);
    if(n<0 || (size_t)n>=sizeof done_path) return 81;
    n=snprintf(done,sizeof done,"phase=post-fork tid=%llu fork_return=%d fork_errno=%d reaped=%d wait_errno=%d child_exit=0 joined=1\n",(unsigned long long)tid,child,fork_errno,reaped,wait_errno);
    if(n<0 || (size_t)n>=sizeof done) return 82;
    marker_write(done_path,done,(size_t)n);
    return 0;
}
int main(int argc, char **argv) {
    /* Harness timer qualification only, not a threaded compatibility case. */
    if (argc == 2 && strcmp(argv[1], "hang-census") == 0) { for (;;) pause(); }
    if (argc == 2 && strcmp(argv[1], "hang") == 0) {
        /* Unsupervised harness-only child: group kill must reach both PIDs.
         * The byte handshake establishes readiness without a timing sleep. */
        int ready[2]; if (pipe(ready) != 0) return 94;
        pid_t child = fork(); if (child < 0) return 95;
        if (child == 0) {
            if (close(ready[0]) != 0) _exit(96);
            if (write(ready[1], "R", 1) != 1) _exit(97);
            if (close(ready[1]) != 0) _exit(98);
            for (;;) pause();
        }
        if (close(ready[1]) != 0) return 96;
        char byte; if (read(ready[0], &byte, 1) != 1 || byte != 'R') return 97;
        if (close(ready[0]) != 0) return 98;
        for (;;) pause();
    }
    if (argc != 5) return 2;
    if (strcmp(argv[1], "c10-10") == 0) return fork_helper(argv[3], argv[4]);
    if (strcmp(argv[1], "c10-04") == 0) return team_run(4, argv[3], argv[4]);
    if (strcmp(argv[1], "c10-05") == 0) return team_run(5, argv[3], argv[4]);
    if (strcmp(argv[1], "c10-06") == 0) return team_run(6, argv[3], argv[4]);
    int with_helper = strcmp(argv[1], "c10-02") == 0;
    int helper_owns_io = strcmp(argv[1], "c10-03") == 0;
    if (!with_helper && !helper_owns_io && strcmp(argv[1], "c10-01") != 0) return 2;
    struct rendezvous r; rendezvous_init(&r);
    /* Single-thread plumbing check, NOT a qualification of blocking handoffs. */
    rendezvous_publish(&r, 1); rendezvous_wait(&r, 1); rendezvous_destroy(&r);
    uint64_t tid; checked(pthread_threadid_np(NULL, &tid));
    struct helper_state helper = {0}; pthread_t worker;
    unsigned waits_before_io = 0, finished_before_release = 0;
    if (with_helper) {
        rendezvous_init(&helper.gate);
        checked(pthread_create(&worker, NULL, keep_alive, &helper));
        checked(pthread_mutex_lock(&helper.gate.mutex));
        while (!helper.ready) checked(pthread_cond_wait(&helper.gate.changed, &helper.gate.mutex));
        waits_before_io = helper.wait_calls;
        if (waits_before_io == 0 || helper.finished || helper.gate.phase != 0 || helper.tid == tid) return 10;
        checked(pthread_mutex_unlock(&helper.gate.mutex));
    }
    struct helper_io work = {0};
    if (helper_owns_io) {
        work.input_path = argv[2]; work.output_path = argv[3]; work.result = -1;
        rendezvous_init(&work.gate);
        checked(pthread_mutex_lock(&work.gate.mutex));
        checked(pthread_create(&worker, NULL, helper_io_run, &work));
        while (!work.done) {
            work.main_waits++;
            checked(pthread_cond_wait(&work.gate.changed, &work.gate.mutex));
        }
        checked(pthread_mutex_unlock(&work.gate.mutex));
        void *result; checked(pthread_join(worker, &result));
        if (result != NULL || work.result != 0 || !work.entered || work.tid == tid) return 14;
        for (unsigned i = 0; i < work.count; ++i) if (work.calls[i].tid != work.tid) return 15;
        count = work.count; memcpy(records, work.calls, count * sizeof records[0]);
        rendezvous_destroy(&work.gate);
    } else {
    errno = 0; int fd = open(argv[2], O_RDONLY); int saved = errno;
    capture("open-input", fd, saved); if (fd < 0) return 3;
    char input[6]; transfer(fd, input, sizeof input, 0); close_recorded(fd);
    if (memcmp(input, "ABCDEF", 6) != 0) return 4;
    errno = 0; fd = open(argv[3], O_CREAT | O_EXCL | O_WRONLY, 0600); saved = errno;
    capture("open-output", fd, saved); if (fd < 0) return 5;
    char payload[] = "MAIN"; transfer(fd, payload, 4, 1); close_recorded(fd);
    }
    if (with_helper) {
        checked(pthread_mutex_lock(&helper.gate.mutex));
        finished_before_release = helper.finished;
        if (!helper.ready || helper.finished || helper.gate.phase != 0) return 11;
        helper.gate.phase = 1;
        checked(pthread_cond_broadcast(&helper.gate.changed));
        checked(pthread_mutex_unlock(&helper.gate.mutex));
        void *result;
        checked(pthread_join(worker, &result));
        if (result != NULL || !helper.finished || helper.wait_returns == 0) return 12;
        rendezvous_destroy(&helper.gate);
    }
    /* The record writer is separate from the measured transfers. Its failures
     * fail the fixture; native and NFS output readback remain independent. */
    char report[4096]; size_t used = (size_t)snprintf(report, sizeof report,
        "phase=complete tid=%llu input=ABCDEF output=%s length=%u helper_tid=%llu ready=%u waits_before_io=%u wait_calls=%u wait_returns=%u finished_before_release=%u finished_after_join=%u io_tid=%llu helper_entered=%u helper_done=%u main_waits=%u\n",
        (unsigned long long)tid, helper_owns_io ? "HELPER" : "MAIN", helper_owns_io ? 6 : 4,
        (unsigned long long)helper.tid, helper.ready,
        waits_before_io, helper.wait_calls, helper.wait_returns,
        finished_before_release, helper.finished, (unsigned long long)work.tid, work.entered, work.done, work.main_waits);
    for (unsigned i = 0; i < count; ++i) {
        int n = snprintf(report + used, sizeof report - used, "%s %ld tid=%llu errno=%d\n",
                         records[i].op, records[i].result, (unsigned long long)records[i].tid, records[i].error);
        if (n < 0 || (size_t)n >= sizeof report - used) return 6;
        used += (size_t)n;
    }
    int fd = open(argv[4], O_CREAT | O_EXCL | O_WRONLY, 0600); if (fd < 0) return 7;
    size_t offset = 0;
    while (offset < used) {
        ssize_t n = write(fd, report + offset, used - offset);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 8;
        offset += (size_t)n;
    }
    return close(fd) == 0 ? 0 : 9;
}
