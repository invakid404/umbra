/* Fixture for umbra M0 Gate 2 tracer tests: filesystem mutations through
 * libc, a direct arm64 syscall, and immediate-writing descendants. */
#define _DARWIN_C_SOURCE 1
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#if !defined(__APPLE__) || !defined(__aarch64__)
#error "This fixture requires native arm64 Darwin"
#endif

extern char **environ;
static const int open_flags = O_WRONLY | O_CREAT | O_TRUNC;

static int error_line(const char *operation, int error)
{
    fprintf(stderr, "umbra-test-child: %s: errno=%d (%s)\n",
            operation, error, strerror(error));
    return 1;
}

static int write_all(int fd, const char *data, size_t length)
{
    while (length != 0) {
        ssize_t n = write(fd, data, length);
        if (n < 0) {
            if (errno == EINTR)
                continue;
            return error_line("write", errno);
        }
        if (n == 0)
            return error_line("write made no progress", EIO);
        data += n;
        length -= (size_t)n;
    }
    return 0;
}

/* The open itself never calls libc. Keep this symbol easy to find at -O0.
 * Darwin returns positive errno with carry set; normalize to negative errno
 * inside the asm. A successful descriptor can be zero.
 * ABI reference: apple-oss-distributions/xnu, libsyscall/custom/SYS.h. */
static int raw_open(const char *path)
{
    register long x0 __asm__("x0") = (long)path;
    register long x1 __asm__("x1") = open_flags;
    register long x2 __asm__("x2") = 0644;
    register long x3 __asm__("x3") = 0;
    register long x4 __asm__("x4") = 0;
    register long x5 __asm__("x5") = 0;
    register long x16 __asm__("x16") = SYS_open;
    _Static_assert(SYS_open == 5, "Unexpected Darwin open syscall number");
    __asm__ volatile("svc #0x80\n\t"
                     "cneg x0, x0, cs"
                     : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3),
                       "+r"(x4), "+r"(x5), "+r"(x16)
                     :
                     : "memory", "cc");
    if (x0 < 0) {
        errno = (int)-x0;
        return -1;
    }
    return (int)x0;
}

/* Same Darwin arm64 convention as raw_open, with wait4's four operands:
 * x0 pid selector, x1 status, x2 options, x3 rusage, number in x16. Numbers 7
 * and 400 are passed literally so the tracer sees each one by name.
 * ABI reference: apple-oss-distributions/xnu, libsyscall/custom/SYS.h. */
static long raw_wait4(long number, pid_t pid, int *status, long options,
                      struct rusage *usage)
{
    register long x0 __asm__("x0") = (long)pid;
    register long x1 __asm__("x1") = (long)status;
    register long x2 __asm__("x2") = options;
    register long x3 __asm__("x3") = (long)usage;
    register long x4 __asm__("x4") = 0;
    register long x5 __asm__("x5") = 0;
    register long x16 __asm__("x16") = number;
    __asm__ volatile("svc #0x80\n\t"
                     "cneg x0, x0, cs"
                     : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3),
                       "+r"(x4), "+r"(x5), "+r"(x16)
                     :
                     : "memory", "cc");
    if (x0 < 0) {
        errno = (int)-x0;
        return -1;
    }
    return x0;
}

static int write_case(const char *path, const char *data, size_t length)
{
    int fd = open(path, open_flags, 0644);
    if (fd < 0)
        return error_line("open", errno);
    int result = write_all(fd, data, length);
    if (close(fd) < 0)
        result = error_line("close", errno);
    return result;
}

static int open_write(const char *path, int direct)
{
    if (!direct)
        return write_case(path, "libc\n", 5);
    int fd = raw_open(path);
    if (fd < 0)
        return error_line("svc open", errno);
    int result = write_all(fd, "libc\n", 5);
    if (close(fd) < 0)
        result = error_line("close", errno);
    return result;
}

static int wait_child(pid_t pid, const char *label, int report)
{
    int status;
    while (waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR)
            return error_line("waitpid", errno);
    }
    if (WIFEXITED(status)) {
        int code = WEXITSTATUS(status);
        if (code != 0) {
            fprintf(stderr, "umbra-test-child: %s child exit=%d: "
                    "errno=%d (%s)\n", label, code, ECHILD, strerror(ECHILD));
            return code;
        }
        if (report)
            printf("%s: child exit=0\n", label);
        return 0;
    }
    fprintf(stderr, "umbra-test-child: %s child signal=%d: errno=%d (%s)\n",
            label, WIFSIGNALED(status) ? WTERMSIG(status) : 0,
            ECHILD, strerror(ECHILD));
    return 1;
}

static int fork_write(const char *path)
{
    pid_t pid = fork();
    if (pid == 0) {
        /* No library calls between fork's return and this first mutation. */
        int fd = open(path, open_flags, 0644);
        if (fd < 0)
            _exit(error_line("child open", errno));
        int result = write_all(fd, "fork\n", 5);
        _exit(result); /* Kernel closes the descriptor. */
    }
    if (pid < 0)
        return error_line("fork", errno);
    return wait_child(pid, "fork-write", 1);
}

static int self_path(char *resolved)
{
    char buffer[PATH_MAX];
    uint32_t size = sizeof(buffer);
    if (_NSGetExecutablePath(buffer, &size) != 0)
        return error_line("executable path too long", ENAMETOOLONG);
    if (realpath(buffer, resolved) == NULL)
        return error_line("realpath executable", errno);
    return 0;
}

static int spawn_or_exec(const char *path, int spawn)
{
    char executable[PATH_MAX];
    if (self_path(executable) != 0)
        return 1;
    char *args[] = {executable, "open-libc", (char *)path, NULL};
    pid_t pid;
    if (spawn) {
        int error = posix_spawn(&pid, executable, NULL, NULL, args, environ);
        if (error != 0)
            return error_line("posix_spawn", error);
    } else {
        pid = fork();
        if (pid == 0) {
            execv(executable, args);
            _exit(error_line("execv", errno));
        }
        if (pid < 0)
            return error_line("fork", errno);
    }
    return wait_child(pid, spawn ? "posix-spawn-write" : "exec-write", 1);
}

static int grandchild_write(const char *path)
{
    pid_t pid = fork();
    if (pid == 0) {
        pid_t grandchild = fork();
        if (grandchild == 0) {
            int fd = open(path, open_flags, 0644);
            if (fd < 0)
                _exit(error_line("grandchild open", errno));
            _exit(write_all(fd, "grandchild\n", 11));
        }
        if (grandchild < 0)
            _exit(error_line("grandchild fork", errno));
        /* Reap and relay status before _exit: no polling or orphan race. */
        _exit(wait_child(grandchild, "grandchild-write", 0));
    }
    if (pid < 0)
        return error_line("fork", errno);
    return wait_child(pid, "grandchild-write", 1);
}

static int dup_inherit_write(const char *path)
{
    int fd = open(path, open_flags, 0644);
    if (fd < 0)
        return error_line("parent open", errno);
    pid_t pid = fork();
    if (pid == 0)
        _exit(write_all(fd, "dup\n", 4));
    if (pid < 0) {
        int error = errno;
        close(fd);
        return error_line("fork", error);
    }
    int result = 0;
    if (close(fd) < 0)
        result = error_line("parent close", errno);
    int child_result = wait_child(pid, "dup-inherit-write", 1);
    return result != 0 ? result : child_result;
}

/* Dirfd-relative renames through the namespace.
 *
 * Every path here is a LOGICAL path inside the traced namespace, given by the
 * <root> operand. Nothing at that location exists on the host, so an operand
 * the tracer fails to rewrite lands on a host path that is not there and fails
 * loudly rather than quietly doing the right thing. Untraced, the case runs
 * against the host and needs a writable root, which is why smoke.sh passes a
 * temporary directory.
 *
 * Legs: create through one descriptor by relative name; renameat across two
 * distinct descriptors with both operands relative; read the destination back
 * through the second descriptor; confirm the source name is gone from the
 * first; then renameatx_np with flags zero, whose source is an absolute
 * logical path and whose destination is descriptor-relative. */
static int read_exact(int dirfd, const char *name, const char *expect,
                      size_t length)
{
    char buffer[64];
    int fd = openat(dirfd, name, O_RDONLY);
    if (fd < 0)
        return error_line("openat read", errno);
    ssize_t n = read(fd, buffer, sizeof(buffer));
    int result = 0;
    if (n < 0)
        result = error_line("read", errno);
    else if ((size_t)n != length || memcmp(buffer, expect, length) != 0) {
        fprintf(stderr, "umbra-test-child: dirfd-rename: %s holds %d bytes: "
                "errno=%d (%s)\n", name, (int)n, EINVAL, strerror(EINVAL));
        result = 1;
    }
    if (close(fd) < 0 && result == 0)
        result = error_line("close read", errno);
    return result;
}

static int open_case_dir(const char *root, const char *name, char *out,
                         size_t size)
{
    if ((size_t)snprintf(out, size, "%s/%s", root, name) >= size) {
        errno = ENAMETOOLONG;
        error_line("case directory path", ENAMETOOLONG);
        return -1;
    }
    if (mkdirat(AT_FDCWD, out, 0755) < 0) {
        error_line("mkdirat", errno);
        return -1;
    }
    int fd = open(out, O_RDONLY | O_DIRECTORY);
    if (fd < 0) {
        error_line("open directory", errno);
        return -1;
    }
    return fd;
}

static int dirfd_rename(const char *root)
{
    char first[PATH_MAX], second[PATH_MAX], source[PATH_MAX];
    int a = open_case_dir(root, "da", first, sizeof(first));
    if (a < 0)
        return 1;
    int b = open_case_dir(root, "db", second, sizeof(second));
    if (b < 0) {
        close(a);
        return 1;
    }
    int result = 0;
    int fd = openat(a, "a", open_flags, 0644);
    if (fd < 0)
        result = error_line("openat source", errno);
    else {
        result = write_all(fd, "dirfd\n", 6);
        if (close(fd) < 0 && result == 0)
            result = error_line("close source", errno);
    }
    if (result == 0 && renameat(a, "a", b, "b") < 0)
        result = error_line("renameat", errno);
    if (result == 0)
        result = read_exact(b, "b", "dirfd\n", 6);
    if (result == 0) {
        int gone = openat(a, "a", O_RDONLY);
        if (gone >= 0) {
            close(gone);
            result = error_line("source survived renameat", EEXIST);
        } else if (errno != ENOENT)
            result = error_line("source lookup after renameat", errno);
    }
    if (result == 0
        && (size_t)snprintf(source, sizeof(source), "%s/db/b", root)
               >= sizeof(source))
        result = error_line("source path", ENAMETOOLONG);
    if (result == 0 && renameatx_np(AT_FDCWD, source, b, "c", 0) < 0)
        result = error_line("renameatx_np", errno);
    if (result == 0)
        result = read_exact(b, "c", "dirfd\n", 6);
    if (close(a) < 0 && result == 0)
        result = error_line("close da", errno);
    if (close(b) < 0 && result == 0)
        result = error_line("close db", errno);
    if (result != 0)
        return result;
    fprintf(stderr, "CAPTURED dirfd-rename\n");
    return 0;
}

/* Logical symlinks: creation, exact readlink bytes, traversal, no-follow
 * metadata and a bounded link loop.
 *
 * The <root> operand is a logical namespace path. Under the tracer the
 * overlay stores a symlink as a placeholder object plus its target bytes in
 * control metadata, never as a filesystem symlink, so every check below is
 * about what the tracee can observe rather than what is on disk. Target bytes
 * stay literal: a relative target is compared byte for byte, including a
 * non-ASCII byte, and readlink must never append a NUL or over-report. */
/* The literal-bytes target is deliberately not valid UTF-8, and deliberately
 * dangling: a symlink target is opaque bytes, while APFS rejects the same byte
 * in a file *name*, so traversal uses a separate plain name. */
static const char relative_target[] = "data-\xff";
static const char traverse_target[] = "data";
static const char sentinel = 0x5a;

static int check_readlink(const char *what, ssize_t got, const char *buffer,
                          const char *expect, size_t length)
{
    if (got < 0)
        return error_line(what, errno);
    if ((size_t)got != length || memcmp(buffer, expect, length) != 0) {
        fprintf(stderr, "umbra-test-child: symlink-cycle: %s returned %d "
                "bytes: errno=%d (%s)\n", what, (int)got, EINVAL,
                strerror(EINVAL));
        return 1;
    }
    /* readlink reports a count and never terminates the buffer. */
    if (buffer[length] != sentinel) {
        fprintf(stderr, "umbra-test-child: symlink-cycle: %s wrote past its "
                "%zu bytes: errno=%d (%s)\n", what, length, EINVAL,
                strerror(EINVAL));
        return 1;
    }
    return 0;
}

static int symlink_cycle(const char *root)
{
    char dir[PATH_MAX], link[PATH_MAX], absolute[PATH_MAX], loop[PATH_MAX];
    char buffer[PATH_MAX];
    int d = open_case_dir(root, "sl", dir, sizeof(dir));
    if (d < 0)
        return 1;
    int result = 0;
    if ((size_t)snprintf(link, sizeof(link), "%s/rel", dir) >= sizeof(link)
        || (size_t)snprintf(absolute, sizeof(absolute), "%s/%s", dir,
                            traverse_target)
               >= sizeof(absolute))
        result = error_line("case path", ENAMETOOLONG);
    /* symlink(2): relative target, link name anchored at the cwd. */
    if (result == 0 && symlink(relative_target, link) < 0)
        result = error_line("symlink", errno);
    if (result == 0) {
        memset(buffer, sentinel, sizeof(buffer));
        result = check_readlink("readlink", readlink(link, buffer,
                                                     sizeof(buffer) - 1),
                                buffer, relative_target,
                                strlen(relative_target));
    }
    /* A short buffer truncates to exactly that many bytes. */
    if (result == 0) {
        memset(buffer, sentinel, sizeof(buffer));
        result = check_readlink("readlink truncated",
                                readlink(link, buffer, 3), buffer,
                                relative_target, 3);
    }
    /* symlinkat(2)/readlinkat(2) with an absolute target, through a dirfd. */
    if (result == 0 && symlinkat(absolute, d, "abs") < 0)
        result = error_line("symlinkat", errno);
    if (result == 0) {
        memset(buffer, sentinel, sizeof(buffer));
        result = check_readlink("readlinkat",
                                readlinkat(d, "abs", buffer,
                                           sizeof(buffer) - 1),
                                buffer, absolute, strlen(absolute));
    }
    /* Traverse: create the named object, then reach it through a relative and
     * an absolute link. */
    if (result == 0 && symlinkat(traverse_target, d, "good") < 0)
        result = error_line("symlinkat good", errno);
    if (result == 0) {
        int fd = openat(d, traverse_target, open_flags, 0644);
        if (fd < 0)
            result = error_line("openat target", errno);
        else {
            result = write_all(fd, "symlink\n", 8);
            if (close(fd) < 0 && result == 0)
                result = error_line("close target", errno);
        }
    }
    if (result == 0)
        result = read_exact(d, "good", "symlink\n", 8);
    if (result == 0)
        result = read_exact(d, "abs", "symlink\n", 8);
    /* A no-follow stat must see a link, never the stored placeholder. */
    if (result == 0) {
        struct stat st;
        memset(&st, 0, sizeof(st));
        if (fstatat(AT_FDCWD, link, &st, AT_SYMLINK_NOFOLLOW) < 0)
            result = error_line("fstatat no-follow", errno);
        else if (!S_ISLNK(st.st_mode)
                 || st.st_size != (off_t)strlen(relative_target)) {
            fprintf(stderr, "umbra-test-child: symlink-cycle: no-follow stat "
                    "mode %#o size %lld: errno=%d (%s)\n", st.st_mode,
                    (long long)st.st_size, EINVAL, strerror(EINVAL));
            result = 1;
        }
    }
    /* A two-link loop must exhaust the expansion bound as ELOOP, promptly. */
    if (result == 0) {
        if ((size_t)snprintf(loop, sizeof(loop), "%s/loop1", dir)
            >= sizeof(loop))
            result = error_line("loop path", ENAMETOOLONG);
        else if (symlinkat("loop2", d, "loop1") < 0)
            result = error_line("symlinkat loop1", errno);
        else if (symlinkat("loop1", d, "loop2") < 0)
            result = error_line("symlinkat loop2", errno);
        else {
            int fd = open(loop, O_RDONLY);
            if (fd >= 0) {
                close(fd);
                result = error_line("link loop resolved", EEXIST);
            } else if (errno != ELOOP)
                result = error_line("link loop errno", errno);
        }
    }
    if (close(d) < 0 && result == 0)
        result = error_line("close sl", errno);
    if (result != 0)
        return result;
    fprintf(stderr, "CAPTURED symlink-cycle\n");
    return 0;
}

/* WNOHANG polling through every wait entry point the tracer owns.
 *
 * WNOHANG is mask value 1 — the least significant bit, not 1 << 1 — per
 * sys/wait.h. Under the tracer a native wait reports ECHILD, because attaching
 * the debugger transiently reparents children, so a zero return here is
 * evidence that the tracer virtualized the call rather than letting it through.
 *
 * The child is held on a pipe instead of sleeping: it cannot exit until this
 * process releases it, so a poll that wrongly blocks deadlocks and fails
 * against the session deadline rather than passing on lucky timing.
 *
 * All five entry points are exercised. The two libSystem stubs are resolved
 * exactly as native.rs::install resolves them, so the calls land on the
 * installed breakpoints; the two raw svc #0x80 forms name syscall numbers 7
 * and 400 explicitly, which a public wait4 call alone cannot demonstrate. */
typedef pid_t (*wait4_fn)(pid_t, int *, int, struct rusage *);

static wait4_fn wait4_stub;
static wait4_fn wait4_nocancel_stub;

/* A poll that reaps nothing must leave both output buffers alone. */
static const int status_sentinel = 0x5a5a5a5a;

static pid_t poll_public(pid_t pid, int *status, int options,
                         struct rusage *usage)
{
    (void)usage; /* The documented shape takes a null rusage. */
    return wait4(pid, status, options, NULL);
}

static pid_t poll_stub(pid_t pid, int *status, int options,
                       struct rusage *usage)
{
    return wait4_stub(pid, status, options, usage);
}

static pid_t poll_nocancel(pid_t pid, int *status, int options,
                           struct rusage *usage)
{
    return wait4_nocancel_stub(pid, status, options, usage);
}

static pid_t poll_raw_wait4(pid_t pid, int *status, int options,
                            struct rusage *usage)
{
    _Static_assert(SYS_wait4 == 7, "Unexpected Darwin wait4 syscall number");
    return (pid_t)raw_wait4(SYS_wait4, pid, status, options, usage);
}

static pid_t poll_raw_nocancel(pid_t pid, int *status, int options,
                               struct rusage *usage)
{
    _Static_assert(SYS_wait4_nocancel == 400,
                   "Unexpected Darwin wait4_nocancel syscall number");
    return (pid_t)raw_wait4(SYS_wait4_nocancel, pid, status, options, usage);
}

/* Arm64 leaves the upper 32 bits of a register holding an `int` argument
 * unspecified, and the kernel's argument munger truncates them: this poll is a
 * legal WNOHANG caller. Anything that reads all 64 bits of x2 sees reserved
 * option bits the callee never receives. */
static pid_t poll_raw_high_bits(pid_t pid, int *status, int options,
                                struct rusage *usage)
{
    long garbage = (long)0xffffffff00000000UL | (unsigned int)options;
    return (pid_t)raw_wait4(SYS_wait4, pid, status, garbage, usage);
}

static int poll_once(const char *label, wait4_fn call)
{
    int status = status_sentinel;
    struct rusage usage, reference;
    memset(&usage, 0x5a, sizeof(usage));
    reference = usage;
    _Static_assert(WNOHANG == 1, "WNOHANG is mask 1, not 1 << 1");
    errno = 0;
    long result = call(-1, &status, WNOHANG, &usage);
    if (result != 0) {
        fprintf(stderr, "umbra-test-child: wnohang-wait: %s returned %ld: "
                "errno=%d (%s)\n", label, result, errno, strerror(errno));
        return 1;
    }
    if (status != status_sentinel) {
        fprintf(stderr, "umbra-test-child: wnohang-wait: %s wrote status "
                "%#x: errno=%d (%s)\n", label, status, EINVAL,
                strerror(EINVAL));
        return 1;
    }
    if (memcmp(&usage, &reference, sizeof(usage)) != 0) {
        fprintf(stderr, "umbra-test-child: wnohang-wait: %s wrote the rusage "
                "buffer: errno=%d (%s)\n", label, EINVAL, strerror(EINVAL));
        return 1;
    }
    return 0;
}

static int wnohang_wait(const char *path)
{
    static const struct {
        const char *label;
        wait4_fn call;
    } entries[] = {
        {"wait4", poll_public},
        {"__wait4", poll_stub},
        {"__wait4_nocancel", poll_nocancel},
        {"svc x16=7 (SYS_wait4)", poll_raw_wait4},
        {"svc x16=400 (SYS_wait4_nocancel)", poll_raw_nocancel},
        {"svc x16=7, unspecified high bits in x2", poll_raw_high_bits},
    };
    wait4_stub = (wait4_fn)dlsym(RTLD_DEFAULT, "__wait4");
    wait4_nocancel_stub = (wait4_fn)dlsym(RTLD_DEFAULT, "__wait4_nocancel");
    if (wait4_stub == NULL || wait4_nocancel_stub == NULL)
        return error_line("dlsym __wait4/__wait4_nocancel", ENOSYS);
    int release[2];
    if (pipe(release) < 0)
        return error_line("pipe", errno);
    pid_t pid = fork();
    if (pid == 0) {
        char byte;
        close(release[1]);
        while (read(release[0], &byte, 1) < 0) {
            if (errno != EINTR)
                _exit(error_line("child read", errno));
        }
        _exit(0);
    }
    if (pid < 0) {
        int error = errno;
        close(release[0]);
        close(release[1]);
        return error_line("fork", error);
    }
    close(release[0]);
    int result = 0;
    for (size_t i = 0; result == 0 && i < sizeof(entries) / sizeof(*entries);
         i++)
        result = poll_once(entries[i].label, entries[i].call);
    if (result == 0)
        result = write_all(release[1], "x", 1);
    if (close(release[1]) < 0 && result == 0)
        result = error_line("close release", errno);
    if (result != 0) {
        /* Never leave the held child behind on a failure path. */
        kill(pid, SIGKILL);
        while (waitpid(pid, NULL, 0) < 0 && errno == EINTR)
            continue;
        return result;
    }
    /* The blocking path must still reap the released child. */
    if (wait_child(pid, "wnohang-wait", 0) != 0)
        return 1;
    /* With the child reaped the tracer has no live child to virtualize, so it
     * must hand the poll back to the kernel: a repeated reap reports ECHILD. */
    int status = status_sentinel;
    errno = 0;
    if (wait4(-1, &status, WNOHANG, NULL) != -1 || errno != ECHILD)
        return error_line("repeated reap did not report ECHILD", errno);
    if (write_case(path, "wnohang\n", 8) != 0)
        return 1;
    fprintf(stderr, "CAPTURED wnohang-wait\n");
    return 0;
}

/* Launch-time argv[0] rewrite. The tracer executes a signed twin of this
 * binary, so the running image is never the vendor path the caller named. The
 * contract is that argv[0] still reads as that vendor path: the caller's
 * argv[0] is replaced, while argv[1..] and the launched image are not. Capture
 * only when argv[0] is byte-identical to the expected vendor path and differs
 * from the image actually running, so neither a passed-through caller argv[0]
 * nor a rewrite to the twin can be mistaken for the contract. Comparison is
 * exact bytes; realpath is used only to widen the negative twin check. */
static int argv0_check(const char *argv0, const char *expected, const char *path)
{
    char running[PATH_MAX];
    uint32_t size = sizeof(running);
    if (_NSGetExecutablePath(running, &size) != 0)
        return error_line("executable path too long", ENAMETOOLONG);
    char resolved[PATH_MAX];
    if (self_path(resolved) != 0)
        return 1;
    if (strcmp(argv0, expected) != 0) {
        fprintf(stderr, "umbra-test-child: argv0-check: argv[0] is \"%s\", "
                "expected vendor path \"%s\": errno=%d (%s)\n",
                argv0, expected, EINVAL, strerror(EINVAL));
        return 1;
    }
    if (strcmp(argv0, running) == 0 || strcmp(argv0, resolved) == 0) {
        fprintf(stderr, "umbra-test-child: argv0-check: argv[0] \"%s\" is the "
                "running image, not a distinct vendor path: errno=%d (%s)\n",
                argv0, EINVAL, strerror(EINVAL));
        return 1;
    }
    int result = write_case(path, "argv0\n", 6);
    if (result != 0)
        return result;
    fprintf(stderr, "CAPTURED argv0-check\n");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 3)
        return error_line("usage: umbra-test-child <subcommand> <path> "
                          "[operand]", EINVAL);
    const char *command = argv[1];
    const char *path = argv[2];
    /* Cases spelled as options take their own operands; bare-name cases keep
     * the original exactly-one-path shape. */
    if (strcmp(command, "--symlink-cycle") == 0) {
        if (argc != 3)
            return error_line("usage: umbra-test-child --symlink-cycle <root>",
                              EINVAL);
        return symlink_cycle(path);
    }
    if (strcmp(command, "--dirfd-rename") == 0) {
        if (argc != 3)
            return error_line("usage: umbra-test-child --dirfd-rename <root>",
                              EINVAL);
        return dirfd_rename(path);
    }
    if (strcmp(command, "--wnohang-wait") == 0) {
        if (argc != 3)
            return error_line("usage: umbra-test-child --wnohang-wait <path>",
                              EINVAL);
        return wnohang_wait(path);
    }
    if (strcmp(command, "--argv0-check") == 0) {
        if (argc != 4)
            return error_line("usage: umbra-test-child --argv0-check <path> "
                              "<vendor-argv0>", EINVAL);
        return argv0_check(argv[0], argv[3], path);
    }
    if (argc != 3)
        return error_line("usage: umbra-test-child <subcommand> <path>", EINVAL);
    if (strcmp(command, "open-libc") == 0)
        return open_write(path, 0);
    if (strcmp(command, "open-svc") == 0)
        return open_write(path, 1);
    if (strcmp(command, "fork-write") == 0)
        return fork_write(path);
    if (strcmp(command, "posix-spawn-write") == 0)
        return spawn_or_exec(path, 1);
    if (strcmp(command, "exec-write") == 0)
        return spawn_or_exec(path, 0);
    if (strcmp(command, "grandchild-write") == 0)
        return grandchild_write(path);
    if (strcmp(command, "dup-inherit-write") == 0)
        return dup_inherit_write(path);
    return error_line("unknown subcommand", EINVAL);
}
