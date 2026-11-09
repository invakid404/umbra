#include <sys/mount.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/wait.h>

extern char **environ;

static void require(int ok, const char *phase) {
    if (!ok) {
        fprintf(stderr, "f214 %s errno=%d\n", phase, errno);
        exit(20);
    }
}

static void marker(const char *root, const char *name, pid_t pid) {
    char path[4096], text[32];
    int n = snprintf(path, sizeof(path), "%s/%s", root, name);
    require(n >= 0 && (size_t)n < sizeof(path), "marker path");
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    require(fd >= 0, "marker open");
    n = snprintf(text, sizeof(text), "%d\n", pid);
    require(write(fd, text, (size_t)n) == n && close(fd) == 0, "marker write");
}

int main(int argc, char **argv) {
    require(argc >= 3, "arguments");
    if (strcmp(argv[1], "exec-child") == 0) {
        require(argc == 7, "exec arguments");
        int inherited = atoi(argv[2]), cloexec = atoi(argv[3]);
        struct statfs metadata;
        require(fstatfs(inherited, &metadata) == 0, "exec inherited metadata");
        require(metadata.f_fsid.val[0] == (int32_t)strtol(argv[4], NULL, 10) &&
                metadata.f_fsid.val[1] == (int32_t)strtol(argv[5], NULL, 10), "exec identity");
        require(fstatfs(cloexec, &metadata) == -1 && errno == EBADF, "exec cloexec");
        printf("f214 exec metadata pid=%d\n", getpid());
        marker(argv[6], "records.child", getpid());
        return 0;
    }
    char path[4096], output[4096];
    int length = snprintf(path, sizeof(path), "%s/seed.txt", argv[2]);
    require(length >= 0 && (size_t)length < sizeof(path), "path length");
    length = snprintf(output, sizeof(output), "%s/records.done", argv[2]);
    require(length >= 0 && (size_t)length < sizeof(output), "output length");
    int fd = open(path, O_RDONLY);
    require(fd >= 0, "open");
    struct statfs by_fd, by_path;
    memset(&by_fd, 0, sizeof(by_fd));
    memset(&by_path, 0, sizeof(by_path));
    require(fstatfs(fd, &by_fd) == 0, "fstatfs");
    require(statfs(path, &by_path) == 0, "statfs");
    require(memcmp(&by_fd.f_fsid, &by_path.f_fsid, sizeof(by_fd.f_fsid)) == 0,
            "path descriptor identity");
    require(by_fd.f_type == by_path.f_type &&
            strcmp(by_fd.f_fstypename, by_path.f_fstypename) == 0 &&
            strcmp(by_fd.f_mntonname, by_path.f_mntonname) == 0 &&
            strcmp(by_fd.f_mntfromname, by_path.f_mntfromname) == 0,
            "path descriptor names");
    if (strcmp(argv[1], "errors") == 0) {
        int closed = open(path, O_RDONLY);
        require(closed >= 0 && close(closed) == 0, "closed descriptor setup");
        struct statfs *bad = (struct statfs *)(uintptr_t)1;
        require(fstatfs(closed, bad) == -1 && errno == EBADF, "closed before bad pointer");
        require(fstatfs(fd, bad) == -1 && errno == EFAULT, "valid before bad pointer");
        char missing[4096];
        length = snprintf(missing, sizeof(missing), "%s/missing", argv[2]);
        require(length >= 0 && (size_t)length < sizeof(missing), "missing length");
        require(statfs(missing, bad) == -1 && errno == ENOENT, "missing before bad pointer");
    }
    if (strcmp(argv[1], "fork") == 0) {
        int cloexec = open(path, O_RDONLY | O_CLOEXEC);
        require(cloexec >= 0, "cloexec open");
        char fd_arg[32], clo_arg[32], fsid0[32], fsid1[32];
        snprintf(fd_arg, sizeof(fd_arg), "%d", fd);
        snprintf(clo_arg, sizeof(clo_arg), "%d", cloexec);
        snprintf(fsid0, sizeof(fsid0), "%d", by_fd.f_fsid.val[0]);
        snprintf(fsid1, sizeof(fsid1), "%d", by_fd.f_fsid.val[1]);
        pid_t child = fork();
        require(child >= 0, "fork");
        if (child == 0) {
            struct statfs inherited;
            require(fstatfs(fd, &inherited) == 0, "fork inherited metadata");
            require(memcmp(&inherited.f_fsid, &by_fd.f_fsid, sizeof(inherited.f_fsid)) == 0,
                    "fork identity");
            printf("f214 fork metadata pid=%d\n", getpid());
            marker(argv[2], "records.pre", getpid());
            fflush(stdout);
            char *args[] = {argv[3], "exec-child", fd_arg, clo_arg, fsid0, fsid1, argv[2], NULL};
            execve(argv[3], args, environ);
            require(0, "execve");
        }
        int status;
        require(waitpid(child, &status, 0) == child, "wait child");
        require(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child status");
        printf("f214 child completed pid=%d\n", child);
        marker(argv[2], "records.pid", child);
        require(close(cloexec) == 0, "parent cloexec close");
    }
    if (strcmp(argv[1], "copyup") == 0) {
        int writer = open(path, O_WRONLY);
        require(writer >= 0, "copyup open");
        require(write(writer, "Z", 1) == 1, "copyup write");
        struct statfs after;
        memset(&after, 0, sizeof(after));
        require(fstatfs(writer, &after) == 0, "copyup metadata");
        require(memcmp(&after.f_fsid, &by_fd.f_fsid, sizeof(after.f_fsid)) == 0,
                "copyup identity");
        require(close(writer) == 0, "copyup close");
        by_path = after;
    }
    if (strcmp(argv[1], "unlink") == 0) {
        char renamed[4096];
        length = snprintf(renamed, sizeof(renamed), "%s/renamed.txt", argv[2]);
        require(length >= 0 && (size_t)length < sizeof(renamed), "rename length");
        require(renameat(AT_FDCWD, path, AT_FDCWD, renamed) == 0, "renameat");
        struct statfs after;
        memset(&after, 0, sizeof(after));
        require(statfs(path, &after) == -1 && errno == ENOENT, "old name absent");
        require(statfs(renamed, &after) == 0, "new name present");
        require(fstatfs(fd, &after) == 0, "renamed fd");
        require(memcmp(&after.f_fsid, &by_fd.f_fsid, sizeof(after.f_fsid)) == 0,
                "rename identity");
        require(unlinkat(AT_FDCWD, renamed, 0) == 0, "unlinkat");
        require(statfs(renamed, &after) == -1 && errno == ENOENT, "unlinked name absent");
        memset(&after, 0, sizeof(after));
        require(fstatfs(fd, &after) == 0, "unlinked fd");
        require(memcmp(&after.f_fsid, &by_fd.f_fsid, sizeof(after.f_fsid)) == 0,
                "unlink identity");
        by_path = after;
    }
    int record = open(output, O_CREAT | O_WRONLY | O_TRUNC, 0600);
    require(record >= 0, "record open");
    require(write(record, &by_fd, sizeof(by_fd)) == sizeof(by_fd), "record fd");
    require(write(record, &by_path, sizeof(by_path)) == sizeof(by_path), "record path");
    require(close(record) == 0 && close(fd) == 0, "close");
    return 0;
}
