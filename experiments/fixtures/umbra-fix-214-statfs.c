#include <sys/mount.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static void require(int ok, const char *phase) {
    if (!ok) {
        fprintf(stderr, "f214 %s errno=%d\n", phase, errno);
        exit(20);
    }
}

int main(int argc, char **argv) {
    require(argc >= 3, "arguments");
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
