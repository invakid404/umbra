/* Fixture for umbra M0 Gate 2 tracer tests: filesystem mutations through
 * libc, a direct arm64 syscall, and immediate-writing descendants. */
#define _DARWIN_C_SOURCE 1
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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

static int open_write(const char *path, int direct)
{
    int fd = direct ? raw_open(path) : open(path, open_flags, 0644);
    if (fd < 0)
        return error_line(direct ? "svc open" : "open", errno);
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

int main(int argc, char **argv)
{
    if (argc != 3)
        return error_line("usage: umbra-test-child <subcommand> <path>", EINVAL);
    const char *command = argv[1];
    const char *path = argv[2];
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
