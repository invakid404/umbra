/* D6: exactly five programs. Each emits one atomic JSON record to the
 * harness-owned stdout stream. The root reports four actual parent waits.
 * cc -std=c11 -Wall -Wextra -Werror tree5.c -o tree5
 */
#include <errno.h>
#include <libproc.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
extern char **environ;

static unsigned long long now_ns(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_UPTIME_RAW, &t)) return 0;
    return (unsigned long long)t.tv_sec * 1000000000ULL + t.tv_nsec;
}
static int identity(char *buffer, size_t size, int root) {
    struct proc_bsdinfo info;
    memset(&info, 0, sizeof info);
    if (proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof info) != sizeof info) return -1;
    return snprintf(buffer, size, "{\"tree5\":true,\"root\":%s,\"pid\":%d,\"parent\":%d,\"birth\":[%llu,%llu]",
        root ? "true" : "false", getpid(), getppid(),
        (unsigned long long)info.pbi_start_tvsec, (unsigned long long)info.pbi_start_tvusec);
}
static int emit(char *buffer, int length) {
    /* One write, comfortably below PIPE_BUF, including when stdio is piped.
     * A partial or failed write is a failed workload, never a partial receipt.
     */
    return length > 0 && write(STDOUT_FILENO, buffer, (size_t)length) == length ? 0 : 1;
}
int main(int argc, char **argv) {
    char record[1024];
    int root = argc == 1;
    if (!root && (argc != 2 || strcmp(argv[1], "child"))) return 2;
    int used = identity(record, sizeof record, root);
    if (used < 0 || (size_t)used >= sizeof record) return 3;
    if (!root) {
        int n = snprintf(record + used, sizeof record - used, "}\n");
        return n != 2 ? 4 : emit(record, used + n);
    }
    pid_t children[4];
    int count = 0, failed = 0;
    unsigned long long start = now_ns();
    if (!start) return 5;
    for (int i = 0; i < 4; i++) {
        char *args[] = {argv[0], "child", NULL};
        if (posix_spawn(&children[count], argv[0], NULL, NULL, args, environ)) { failed = 1; break; }
        count++;
    }
    used += snprintf(record + used, sizeof record - used, ",\"collected\":[");
    for (int i = 0; i < count; i++) {
        int status = 0, forced = 0;
        pid_t got;
        for (;;) {
            got = waitpid(children[i], &status, WNOHANG);
            if (got == children[i] || (got < 0 && errno != EINTR)) break;
            unsigned long long now = now_ns();
            if (!now || now >= start + 60000000000ULL) {
                if (!forced) { kill(children[i], SIGKILL); forced = 1; failed = 1; }
                if (!now || now >= start + 61000000000ULL) { got = -1; break; }
            }
            struct timespec pause = {0, 1000000};
            nanosleep(&pause, NULL);
        }
        if (got != children[i] || !WIFEXITED(status) || WEXITSTATUS(status)) failed = 1;
        int n = snprintf(record + used, sizeof record - used,
            "%s{\"pid\":%d,\"waited\":%d,\"status\":%d}",
            i ? "," : "", children[i], got, status);
        if (n < 0 || (size_t)n >= sizeof record - used) return 6;
        used += n;
    }
    int n = snprintf(record + used, sizeof record - used, "]}\n");
    if (n != 3) return 7;
    return emit(record, used + n) || failed || count != 4;
}
