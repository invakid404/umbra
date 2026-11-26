// Startup-cost observer. The calling process waits for this observer; the
// observer excludes itself from the product process/descriptor inventory.
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/proc_info.h>
#include <time.h>
#include <unistd.h>

struct member {
    pid_t pid;
    struct proc_bsdinfo info;
    int observer;
};

static int below(struct member *members, int count, int index, pid_t root) {
    for (int hop = 0; hop <= count; hop++) {
        if (members[index].pid == root) return 1;
        pid_t parent = (pid_t)members[index].info.pbi_ppid;
        if (parent <= 1) return 0;
        int found = -1;
        for (int i = 0; i < count; i++)
            if (members[i].pid == parent) { found = i; break; }
        if (found < 0) return 0;
        index = found;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 8) return 2;
    // The caller publishes this file only after spawn returned and all of the
    // observer's temporary launch descriptors and the gate writer have closed.
    struct timespec began, current;
    if (clock_gettime(CLOCK_UPTIME_RAW, &began)) return 3;
    while (access(argv[7], F_OK)) {
        if (errno != ENOENT || clock_gettime(CLOCK_UPTIME_RAW, &current)
            || current.tv_sec - began.tv_sec >= 5) return 3;
        usleep(1000);
    }
    const char *directory = argv[1], *event = argv[2], *keep = argv[5];
    pid_t actor = (pid_t)strtol(argv[3], NULL, 10);
    pid_t child = (pid_t)strtol(argv[4], NULL, 10);
    for (const char *p = event; *p; p++)
        if (!isalnum((unsigned char)*p) && *p != '-' && *p != '_') return 2;
    for (const char *p = keep; *p; p++)
        if (!isdigit((unsigned char)*p) && *p != ',') return 2;
    struct proc_bsdinfo actor_info;
    if (proc_pidinfo(actor, PROC_PIDTBSDINFO, 0, &actor_info,
                     sizeof(actor_info)) != sizeof(actor_info)) return 3;
    char root_path[4096], log_path[4096], observers_path[4096];
    int root_length = snprintf(root_path, sizeof(root_path), "%s/root", directory);
    int log_length = snprintf(log_path, sizeof(log_path), "%s/events.jsonl", directory);
    int observers_length = snprintf(observers_path, sizeof(observers_path), "%s/observers", directory);
    if (root_length < 0 || root_length >= (int)sizeof(root_path)
        || log_length < 0 || log_length >= (int)sizeof(log_path)
        || observers_length < 0 || observers_length >= (int)sizeof(observers_path))
        return 2;
    // Retain observer birth identities so a concurrently collected observer
    // cannot be mistaken for a product zombie after proc_pidpath stops working.
    struct proc_bsdinfo observer_info;
    if (proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &observer_info,
                     sizeof(observer_info)) != sizeof(observer_info)) return 3;
    FILE *observers = fopen(observers_path, "a");
    if (!observers || flock(fileno(observers), LOCK_EX)) return 4;
    if (fprintf(observers, "%d %llu %llu\n", getpid(),
                (unsigned long long)observer_info.pbi_start_tvsec,
                (unsigned long long)observer_info.pbi_start_tvusec) < 0
        || fflush(observers) || fclose(observers)) return 4;
    int root_fd = open(root_path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (root_fd >= 0) {
        if (dprintf(root_fd, "%d %llu %llu\n", actor,
                    (unsigned long long)actor_info.pbi_start_tvsec,
                    (unsigned long long)actor_info.pbi_start_tvusec) < 0) return 4;
        if (close(root_fd)) return 4;
    } else if (errno != EEXIST) return 4;
    FILE *root_file = fopen(root_path, "r");
    int root;
    unsigned long long seconds, micros;
    if (!root_file || fscanf(root_file, "%d %llu %llu", &root, &seconds, &micros) != 3)
        return 4;
    fclose(root_file);
    int capacity = proc_listallpids(NULL, 0) + 256;
    if (capacity <= 256) return 3;
    pid_t *pids = calloc((size_t)capacity, sizeof(*pids));
    struct member *members = calloc((size_t)capacity, sizeof(*members));
    if (!pids || !members) return 4;
    int listed = proc_listallpids(pids, capacity * (int)sizeof(*pids));
    if (listed <= 0 || listed >= capacity) return 3;
    char observer_path[PROC_PIDPATHINFO_MAXSIZE];
    if (proc_pidpath(getpid(), observer_path, sizeof(observer_path)) <= 0) return 3;
    int count = 0, root_seen = 0;
    for (int i = 0; i < listed; i++) {
        if (pids[i] <= 0) continue;
        struct proc_bsdinfo info;
        if (proc_pidinfo(pids[i], PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info))
            continue;
        char path[PROC_PIDPATHINFO_MAXSIZE];
        int observer = proc_pidpath(pids[i], path, sizeof(path)) > 0
            && strcmp(path, observer_path) == 0;
        members[count++] = (struct member){pids[i], info, observer};
        if (pids[i] == root && info.pbi_start_tvsec == seconds
            && info.pbi_start_tvusec == micros) root_seen = 1;
    }
    if (!root_seen) return 3;
    observers = fopen(observers_path, "r");
    if (!observers || flock(fileno(observers), LOCK_SH)) return 4;
    int observer_pid, parsed;
    unsigned long long observer_seconds, observer_micros;
    while ((parsed = fscanf(observers, "%d %llu %llu", &observer_pid,
                             &observer_seconds, &observer_micros)) == 3) {
        for (int i = 0; i < count; i++)
            if (members[i].pid == observer_pid
                && members[i].info.pbi_start_tvsec == observer_seconds
                && members[i].info.pbi_start_tvusec == observer_micros)
                members[i].observer = 1;
    }
    if (parsed != EOF || ferror(observers) || fclose(observers)) return 4;
    FILE *log = fopen(log_path, "a");
    if (!log || flock(fileno(log), LOCK_EX)) return 4;
    struct timespec now;
    if (clock_gettime(CLOCK_UPTIME_RAW, &now)) return 3;
    fprintf(log, "{\"event\":\"%s\",\"actor\":%d,\"child\":%d,"
                 "\"status\":%s,\"boot_nanos\":%llu,\"keep\":[%s],\"processes\":[",
            event, actor, child, argv[6],
            (unsigned long long)now.tv_sec * 1000000000ULL + now.tv_nsec, keep);
    int first = 1;
    for (int i = 0; i < count; i++) {
        if (members[i].observer || !below(members, count, i, root)) continue;
        int bytes = proc_pidinfo(members[i].pid, PROC_PIDLISTFDS, 0, NULL, 0);
        if (bytes < 0) return 3;
        int size = bytes + 128 * (int)sizeof(struct proc_fdinfo);
        struct proc_fdinfo *fds = calloc(1, (size_t)size);
        if (!fds) return 4;
        int read = proc_pidinfo(members[i].pid, PROC_PIDLISTFDS, 0, fds, size);
        if (read < 0 || read >= size || read % sizeof(*fds)) return 3;
        fprintf(log, "%s{\"pid\":%d,\"parent\":%u,\"state\":%u,\"birth\":[%llu,%llu],\"fds\":[",
                first ? "" : ",", members[i].pid, members[i].info.pbi_ppid,
                members[i].info.pbi_status,
                (unsigned long long)members[i].info.pbi_start_tvsec,
                (unsigned long long)members[i].info.pbi_start_tvusec);
        first = 0;
        for (int j = 0; j < read / (int)sizeof(*fds); j++)
            fprintf(log, "%s{\"fd\":%d,\"kind\":%u}", j ? "," : "",
                    fds[j].proc_fd, fds[j].proc_fdtype);
        fputs("]}", log);
        free(fds);
    }
    fputs("],\"excluded_observers\":[", log);
    first = 1;
    for (int i = 0; i < count; i++) {
        if (!members[i].observer || !below(members, count, i, root)) continue;
        fprintf(log, "%s{\"pid\":%d,\"birth\":[%llu,%llu]}", first ? "" : ",",
                members[i].pid,
                (unsigned long long)members[i].info.pbi_start_tvsec,
                (unsigned long long)members[i].info.pbi_start_tvusec);
        first = 0;
    }
    fputs("]", log);
    if (strcmp(event, "driver-after-collection") == 0) {
        fputs(",\"host_processes\":[", log);
        for (int i = 0; i < count; i++)
            fprintf(log, "%s{\"pid\":%d,\"parent\":%u,\"state\":%u,\"birth\":[%llu,%llu]}",
                    i ? "," : "", members[i].pid, members[i].info.pbi_ppid,
                    members[i].info.pbi_status,
                    (unsigned long long)members[i].info.pbi_start_tvsec,
                    (unsigned long long)members[i].info.pbi_start_tvusec);
        fputs("]", log);
    }
    fputs("}\n", log);
    if (fflush(log) || flock(fileno(log), LOCK_UN) || fclose(log)) return 4;
    free(members);
    free(pids);
    return 0;
}
