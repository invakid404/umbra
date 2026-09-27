/*
 * The userspace-routing *edge* fixture: what a routed run does when an operation
 * leaves the happy path.
 *
 * `umbra-userspace-toy.c` proves the mechanism works. This proves the mechanism
 * answers correctly when the program does something ordinary and wrong -- names
 * a file that is not there, hands a bad pointer to `read`, asks for more bytes
 * than the backend takes in one call -- or something ordinary and awkward, like
 * touching a file from a library initializer. Every one of those killed the run
 * before the round-1 review; each case here is the regression test for one.
 *
 * Each case reports through the exit status, so the assertion needs no output
 * channel and no debugger:
 *
 *   umbra-userspace-edges notfound <path>
 *       `open` a path that does not exist. Exits with **errno**: POSIX says
 *       ENOENT (2). It used to exit 78 (ENOSYS), because the supervisor resumed
 *       the routing trap and Darwin's `nosys` answered it.
 *
 *   umbra-userspace-edges efault <path>
 *       Create the file, reopen it for reading, then `read(fd, NULL, 4)`. Exits
 *       with **errno**: Darwin says EFAULT (14). It used to kill the run.
 *
 *   umbra-userspace-edges bigio <path>
 *       One `write` of UMBRA_EDGE_BIG bytes, then one `read` of the same, both
 *       larger than any single transfer the backend accepts. A **short** count
 *       is the correct answer and is what this requires; the remainder is
 *       finished in a loop. Exits 0 when the bytes read back match. It used to
 *       kill the run with "I/O exceeds size or offset bounds".
 *
 *   umbra-userspace-edges statat <path>
 *       Create the file through routing, close it, then `fstatat(AT_FDCWD, path,
 *       &st, 0)` on it. Exits with **errno**: a routed run cannot answer a path
 *       operation against an object it created, because there is no kernel path
 *       to rewrite the operand to, so it refuses with ENOTSUP (45 on Darwin).
 *       Before round 2 it killed the whole run with an internal error, where the
 *       same program on `local` exits 0 -- and `create a file then stat it` is
 *       what `cp`, `install` and both the Rust and Go standard libraries do.
 *
 *   umbra-userspace-edges accessat <path>
 *       The same shape through `faccessat(AT_FDCWD, path, R_OK, 0)`, because the
 *       two reach the refusal by different arms of the decoder.
 *
 *   umbra-userspace-edges statbase <path>
 *       The control for the two above: `fstatat` a file the run never touched
 *       (`/etc/hosts`). It resolves through the read-only base, which *does*
 *       have a host path, so it is rewritten and answered exactly as before.
 *       Exits 0. This is what makes the ENOTSUP arm a refusal of the cases it
 *       cannot answer rather than a blanket one.
 *
 *   umbra-userspace-edges trunc <path>
 *       Write ten bytes, close, reopen with O_TRUNC and write two, then read the
 *       file back. Exits 0 when exactly those two bytes come back. This is the
 *       only case that reaches `StorageOperation::Truncate` through the
 *       userspace client: the round-1 review found that arm working but pinned
 *       by nothing, because the plain toy's only O_TRUNC open creates an empty
 *       object and the arm skips a zero-length one.
 *
 *   umbra-userspace-edges fork <path>
 *       `fork`, then let the **child** do a whole routed create/write/close/
 *       reopen/read/compare on a path of its own, while the parent waits and
 *       then writes through routing itself. Exits 0 when both halves worked.
 *
 *       A forked child inherits its parent's address space, so it starts with
 *       the interposer mapped and **already armed** -- its routing traps are
 *       live from its first instruction. Before round 4 umbra re-ran the
 *       load-address search on it, which resumes the tracee to `main`: a
 *       forked child is long past `main` and never reaches it again, so it ran
 *       *unmediated* until its first routed call took SIGSYS and ended the run.
 *       This case is the regression test for both halves -- that the child is
 *       mediated at all, and that it is mediated from its first instruction.
 *
 *   umbra-userspace-edges forkfd <path>
 *       The other half of `fork`: the parent opens a routed file and writes
 *       `seed`, forks, and the **child** writes `child` to the *inherited*
 *       descriptor. Exits 0 when the child's write succeeded.
 *
 *       A routed descriptor is virtual -- a number above the fence, never a
 *       kernel descriptor -- so nothing about it is the kernel's to inherit. It
 *       works because umbra's own descriptor table is inherited across a fork,
 *       and it is inherited *with its offset*: the object holds `seedchild`
 *       afterwards, which the Rust side asserts through the client. The plausible
 *       guess -- that the child's table is empty and the write answers `EBADF` --
 *       is what this case was written to check, and it is wrong.
 *
 *   umbra-userspace-edges ctor <path>
 *       Does nothing in `main`. The point is the constructor below, which opens,
 *       reads and closes a file when UMBRA_EDGE_CTOR is set in the environment --
 *       from the window between dyld mapping umbra's interposer and umbra arming
 *       it. Exits 0 **only when all three of those calls succeeded**.
 *
 *       A constructor cannot return a status, so it records one in
 *       `umbra_ctor_outcome` and this case returns it. That indirection is the
 *       whole point rather than plumbing: the calls used to be issued with their
 *       results discarded and the case returned 0 unconditionally, so B1's
 *       regression test -- "a file-touching initializer no longer kills the
 *       launch" -- passed whether or not the initializer's own `open` worked.
 *       It proved the launch survived and nothing else. Now a pass means the
 *       initializer really did read a file from inside that window.
 *
 * Exit codes below 64 are errno values; the codes above are this program's own,
 * kept clear of them:
 *
 *   70  wrong argument count / unknown case
 *   71  a call that was expected to fail succeeded
 *   72  setup failed (could not create or reopen the file)
 *   73  short-I/O loop made no progress, or ran past the buffer
 *   74  the bytes read back differ from the bytes written
 *   75  out of memory
 *   76  the forked child's own call failed for a reason with no errno to report
 *   77  the forked child read back bytes it did not write
 *   79  the forked child did not exit normally (it was killed by a signal)
 *   80  the `ctor` case's constructor never ran, so the case would have proved
 *       nothing (its `UMBRA_EDGE_CTOR` guard was not set)
 *   81  a call failed but reported no `errno`, so there is no POSIX answer to
 *       return in its place. Every `return errno` below falls back to this
 *       rather than to 0: a zero `errno` after a failed call would otherwise be
 *       indistinguishable from success, and `statbase` and `ctor` both *expect*
 *       0, so the failure would have been reported as a pass.
 *   82  the first transfer of an over-bound I/O was **not short** -- it moved
 *       every byte asked for. `bigio` exists to prove the backend clamps such a
 *       transfer and the caller finishes it in a loop; a complete first call
 *       means there was no clamp to observe, so the case has nothing to say and
 *       must not report success.
 *
 * Build: clang -arch arm64 -O1 umbra-userspace-edges.c -o umbra-userspace-edges
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

/* Comfortably past `LibnfsRawTransport`'s 1 048 572-byte per-call bound, and
 * past the contract's own 1 MiB ceiling, so one call cannot satisfy it. */
#define UMBRA_EDGE_BIG (2u * 1024u * 1024u)

/*
 * A file-touching library initializer, which is the whole of case `ctor`.
 *
 * Guarded on an environment variable so the other cases do not depend on this
 * one: if arming ever regresses, `ctor` fails on its own rather than taking
 * every case with it.
 *
 * `/etc/hosts` is read-only, present on every macOS host, and outside the run's
 * shadow -- so this exercises the window without depending on anything the run
 * created.
 */
static int umbra_ctor_outcome = 80;

__attribute__((constructor)) static void umbra_edge_ctor(void) {
    if (!getenv("UMBRA_EDGE_CTOR")) {
        /* Leaves 80 rather than 0, so the `ctor` case cannot pass on a run that
         * never armed the guard -- which is the same silent pass as ignoring a
         * failure, one step earlier. */
        return;
    }
    errno = 0;
    int fd = open("/etc/hosts", O_RDONLY);
    if (fd < 0) {
        umbra_ctor_outcome = errno ? errno : 81;
        return;
    }
    char byte;
    errno = 0;
    ssize_t got = read(fd, &byte, 1);
    if (got != 1) {
        /* Keep the first cause: the close below can only overwrite a diagnosis
         * that is already more specific than anything it could add. */
        umbra_ctor_outcome = errno ? errno : 81;
        (void)close(fd);
        return;
    }
    errno = 0;
    if (close(fd) != 0) {
        umbra_ctor_outcome = errno ? errno : 81;
        return;
    }
    umbra_ctor_outcome = 0;
}

/*
 * Write every byte, requiring the first call to answer **short** -- neither a
 * failure nor a complete transfer.
 *
 * Both rejections are the point, and only one of them used to be checked. This
 * helper and its read twin are used by `bigio` alone, whose whole claim is that a
 * transfer past the backend's per-call bound is clamped rather than refused; a
 * first call that moved all `total` bytes means nothing clamped it, so the case
 * would have reported success while proving the opposite of what it names. That
 * makes these helpers **routing-specific**: run `bigio` against an ordinary host
 * filesystem, which completes 2 MiB in one call, and it now correctly fails with
 * 82 rather than passing for the wrong reason.
 */
static int write_all(int fd, const char *data, size_t total) {
    ssize_t first = write(fd, data, total);
    if (first <= 0) {
        return 73;
    }
    if ((size_t)first >= total) {
        return 82;
    }
    size_t done = (size_t)first;
    while (done < total) {
        ssize_t count = write(fd, data + done, total - done);
        if (count <= 0) {
            return 73;
        }
        done += (size_t)count;
    }
    return 0;
}

/* Read every byte, on the same terms as `write_all` above: the first call must
 * be short, so a backend that stopped clamping is caught rather than tolerated. */
static int read_all(int fd, char *out, size_t total) {
    ssize_t first = read(fd, out, total);
    if (first <= 0) {
        return 73;
    }
    if ((size_t)first >= total) {
        return 82;
    }
    size_t done = (size_t)first;
    while (done < total) {
        ssize_t count = read(fd, out + done, total - done);
        if (count <= 0) {
            return 73;
        }
        done += (size_t)count;
    }
    return 0;
}

static int case_notfound(const char *path) {
    errno = 0;
    int fd = open(path, O_RDONLY);
    if (fd >= 0) {
        /* Already failing; a close error cannot make this worse or clearer. */
        (void)close(fd);
        return 71;
    }
    return errno ? errno : 81;
}

static int case_efault(const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "seed", 4) != 4 || close(fd) < 0) {
        return 72;
    }
    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return 72;
    }
    /* The bad pointer. An ordinary program bug; Darwin answers EFAULT. */
    errno = 0;
    if (read(fd, (void *)0, 4) >= 0) {
        (void)close(fd);
        return 71;
    }
    int failure = errno ? errno : 81;
    /* Checked, not discarded. This close is on the *expected* path, so swallowing
     * it would let the case report EFAULT while the routed descriptor failed to
     * close -- the one outcome that looks exactly like a pass. */
    errno = 0;
    if (close(fd) != 0) {
        return errno ? errno : 81;
    }
    return failure;
}

/* Create through routing, close, then ask a path question about the result. */
static int case_pathop(const char *path, int access_instead) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "seed", 4) != 4 || close(fd) < 0) {
        return 72;
    }
    int outcome;
    if (access_instead) {
        outcome = faccessat(AT_FDCWD, path, R_OK, 0);
    } else {
        struct stat info;
        outcome = fstatat(AT_FDCWD, path, &info, 0);
    }
    if (outcome == 0) {
        /* Answered rather than refused. The routed slice does not claim this,
         * so a success here means the limit moved and this case is stale. */
        return 71;
    }
    return errno ? errno : 81;
}

/*
 * The control: a path operation on an object this run never created.
 *
 * This case *expects* 0, so it is the one where `return errno` was genuinely
 * dangerous rather than merely imprecise: a failed `fstatat` that left `errno`
 * at 0 would have exited 0 and the assertion would have read the failure as the
 * success it was looking for.
 */
static int case_statbase(void) {
    struct stat info;
    /* Zeroed first so the checks below cannot be satisfied by whatever happened
     * to be on the stack: an implementation that returned 0 without filling the
     * buffer is exactly the "answered" this case must not accept. */
    memset(&info, 0, sizeof info);
    errno = 0;
    if (fstatat(AT_FDCWD, "/etc/hosts", &info, 0) != 0) {
        return errno ? errno : 81;
    }
    /* `/etc/hosts` is a non-empty regular file on every macOS host, so a real
     * answer says so. Without this, a return of 0 alone was the whole assertion
     * and a stub that answered 0 and wrote nothing would have passed. */
    if (!S_ISREG(info.st_mode) || info.st_size <= 0) {
        return 74;
    }
    return 0;
}

static int case_trunc(const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "0123456789", 10) != 10 || close(fd) < 0) {
        return 72;
    }
    /* The object is ten bytes now, so this O_TRUNC has something to shorten. */
    fd = open(path, O_WRONLY | O_TRUNC);
    if (fd < 0 || write(fd, "ab", 2) != 2 || close(fd) < 0) {
        return 72;
    }
    char back[16];
    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return 72;
    }
    errno = 0;
    ssize_t count = read(fd, back, sizeof back);
    if (close(fd) < 0) {
        return 72;
    }
    if (count < 0) {
        /* A failed read, not a short one: 73 below means "the truncate did not
         * happen", and reporting that for an I/O error would be a wrong answer
         * rather than a missing one. */
        return errno ? errno : 81;
    }
    if (count != 2) {
        /* Nine or ten bytes here means the truncate did not happen. */
        return 73;
    }
    if (back[0] != 'a' || back[1] != 'b') {
        return 74;
    }
    return 0;
}

/*
 * fork, and route in both processes.
 *
 * The child works on `<path>.child` rather than `<path>`: the two are separate
 * objects in the run's shadow, so neither half can pass by reading what the
 * other wrote. The child uses `_exit` so it cannot flush an inherited stdio
 * buffer on the way out and route something this case did not ask for.
 */
static int case_fork(const char *path) {
    char child_path[1024];
    int written = snprintf(child_path, sizeof child_path, "%s.child", path);
    if (written <= 0 || (size_t)written >= sizeof child_path) {
        return 72;
    }

    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        errno = 0;
        int fd = open(child_path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd < 0 || write(fd, "fork", 4) != 4 || close(fd) < 0) {
            _exit(errno ? errno : 76);
        }
        char back[8];
        errno = 0;
        fd = open(child_path, O_RDONLY);
        if (fd < 0) {
            _exit(errno ? errno : 76);
        }
        errno = 0;
        ssize_t got = read(fd, back, sizeof back);
        if (close(fd) < 0) {
            _exit(errno ? errno : 76);
        }
        if (got != 4 || memcmp(back, "fork", 4) != 0) {
            _exit(77);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(pid, &status, 0) != pid) {
        return 72;
    }
    if (!WIFEXITED(status)) {
        return 79;
    }
    if (WEXITSTATUS(status) != 0) {
        return WEXITSTATUS(status);
    }
    /* And the parent still routes after the fork and the wait. */
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "parent", 6) != 6 || close(fd) < 0) {
        return 72;
    }
    return 0;
}

/*
 * The inherited-descriptor half: a routed fd **does** survive a fork, offset and
 * all, so the child's write continues the parent's in the store. umbra's own
 * descriptor table is what is inherited -- the number is virtual, so there is
 * nothing here for the kernel to inherit -- and the proof is the order of the
 * bytes: `seed` then `child`, which only holds if the offset came across too.
 */
static int case_forkfd(const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "seed", 4) != 4) {
        return 72;
    }
    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        /* The descriptor is the parent's, and routed: the number is above the
         * fence, so the interposer traps on it. Whether umbra can still answer
         * it is the question this case measures. */
        errno = 0;
        if (write(fd, "child", 5) != 5) {
            /* Cleared first: a *short* write sets no errno, and a leftover value
             * from an earlier successful call would name an unrelated cause. */
            _exit(errno ? errno : 76);
        }
        _exit(0);
    }
    int status = 0;
    if (waitpid(pid, &status, 0) != pid || close(fd) < 0) {
        return 72;
    }
    if (!WIFEXITED(status)) {
        return 79;
    }
    return WEXITSTATUS(status);
}

static int case_bigio(const char *path) {
    char *out = malloc(UMBRA_EDGE_BIG);
    char *back = malloc(UMBRA_EDGE_BIG);
    if (!out || !back) {
        return 75;
    }
    for (unsigned i = 0; i < UMBRA_EDGE_BIG; i++) {
        out[i] = (char)(i * 31u + 7u);
    }
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return 72;
    }
    int failure = write_all(fd, out, UMBRA_EDGE_BIG);
    if (close(fd) < 0 || failure) {
        return failure ? failure : 72;
    }
    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return 72;
    }
    failure = read_all(fd, back, UMBRA_EDGE_BIG);
    if (close(fd) < 0 || failure) {
        return failure ? failure : 72;
    }
    if (memcmp(out, back, UMBRA_EDGE_BIG) != 0) {
        return 74;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 70;
    }
    if (strcmp(argv[1], "notfound") == 0) {
        return case_notfound(argv[2]);
    }
    if (strcmp(argv[1], "efault") == 0) {
        return case_efault(argv[2]);
    }
    if (strcmp(argv[1], "bigio") == 0) {
        return case_bigio(argv[2]);
    }
    if (strcmp(argv[1], "trunc") == 0) {
        return case_trunc(argv[2]);
    }
    if (strcmp(argv[1], "statat") == 0) {
        return case_pathop(argv[2], 0);
    }
    if (strcmp(argv[1], "accessat") == 0) {
        return case_pathop(argv[2], 1);
    }
    if (strcmp(argv[1], "statbase") == 0) {
        return case_statbase();
    }
    if (strcmp(argv[1], "fork") == 0) {
        return case_fork(argv[2]);
    }
    if (strcmp(argv[1], "forkfd") == 0) {
        return case_forkfd(argv[2]);
    }
    if (strcmp(argv[1], "ctor") == 0) {
        /* Set by the constructor before `main` was entered. */
        return umbra_ctor_outcome;
    }
    return 70;
}
