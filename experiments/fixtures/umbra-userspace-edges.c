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
 *   umbra-userspace-edges forkexec <path>
 *       `fork`, then have the **child `exec` a different binary** -- the helper
 *       named by UMBRA_EDGE_HELPER -- which writes `execchild` to `<path>.exec`
 *       through routing. The parent waits and then writes through routing
 *       itself. Exits 0 when both halves worked.
 *
 *       This is the regression test for the exec half of the interposer's
 *       lifecycle, and it is a *different image* on purpose. `exec` resets the
 *       address space, so the interposer arrives in the new image mapped and
 *       **inert**: dyld re-loads it (DYLD_INSERT_LIBRARIES rides in `envp` and
 *       survives the exec) and its `__DATA,__umbra_arm` control block comes back
 *       zeroed. umbra has to re-arm it. It used to re-arm only when the exec'd
 *       image was the one image named at launch, so a child that exec'd anything
 *       else ran with an inert interposer, silently and with no diagnostic at
 *       all.
 *
 *       What that cost is narrower than "unmediated", and the narrow version is
 *       the measured one. Only the *interposer* was un-armed: the tracer
 *       re-plants every breakpointed libc stub after an exec regardless, so this
 *       case's `open` was still routed and still came back a **virtual
 *       descriptor**. `write` and `close` reach umbra through the interposer
 *       alone, so they went to libc holding a number the kernel does not own and
 *       were answered **EBADF**. The object was therefore **created and left
 *       empty** in the store -- not absent, and nothing touched the host.
 *
 *       So the Rust side asserts the **bytes** through the NFSv4 client, not the
 *       name: broken and fixed both leave an entry here, and only its contents
 *       tell them apart.
 *
 *   umbra-userspace-edges execwrite <path>
 *       The helper half of `forkexec`, and of nothing else: create `<path>`
 *       through routing, write `execchild`, close. Exits 0 on success. It is a
 *       case of this same program so the fixture stays one file; the Rust driver
 *       copies the binary to a **different basename** before naming it in
 *       UMBRA_EDGE_HELPER, which is what makes it a different image to umbra --
 *       a twin is cached at `<sha256 of contents>/<basename>`, so the copy
 *       resigns to a path the launch target's does not equal.
 *
 *   umbra-userspace-edges chdirchild <path>
 *       `mkdir <path>.d` through routing, then `fork` and have the child `exec`
 *       the helper in `chdirwrite` mode: it `chdir`s into `<path>.d` and writes
 *       `chdirchild` to the **relative** name `leaf.txt`. Exits 0 when that
 *       worked.
 *
 *       The working directory is what relative resolution anchors against, and
 *       umbra keeps its own logical copy in `ProcessContext::cwd`. An
 *       unintercepted `chdir`(2) reaches the kernel, moves the *host* working
 *       directory and leaves the logical one behind, so the relative `open` that
 *       follows resolves against the launch directory instead. The failure is
 *       therefore a **wrong entry name** rather than a missing file, and that is
 *       what the Rust side asserts in both directions: `<path>.d/leaf.txt` holds
 *       the bytes, and `leaf.txt` beside the workspace root does not exist.
 *
 *   umbra-userspace-edges chdirwrite <path>
 *       The helper half of `chdirchild`: `chdir(<path>.d)`, then create
 *       `leaf.txt` -- **relative**, which is the whole point -- write
 *       `chdirchild`, close. Exits 0 on success.
 *
 *   umbra-userspace-edges grandchild <path>
 *       `fork`, and the child `fork`s again. The **grandchild** does a whole
 *       routed create/write/close of `<path>.grand`; the child writes
 *       `<path>.mid` after reaping it; the parent writes `<path>` after reaping
 *       the child. Exits 0 when all three worked.
 *
 *       Two generations, because one generation is what `fork` already covers.
 *       Each process is attached from its own parent's syscall return, so a
 *       grandchild is reached only if that discovery composes -- the child must
 *       itself be mediated closely enough for its own `fork` breakpoint to fire.
 *
 *   umbra-userspace-edges rollbackchild <path>
 *       `fork`, let the child write `<path>.child` through routing and exit 0,
 *       reap it, and then have the **parent** issue an operation the routed
 *       namespace refuses outright: an `O_APPEND` open, which has no atomic
 *       append-at-end storage operation behind it. That refusal stops the run.
 *
 *       The case therefore never returns; its verdict is the run's, not this
 *       program's, and the Rust side reads it from the journal. What is being
 *       asserted is that the child's write belongs to the **parent's run** --
 *       one shadow, one journal, no per-process transaction that could commit
 *       independently of it -- so a run that never reaches its terminal
 *       `RunCompleted` record covers the child's writes as much as the parent's.
 *
 *   umbra-userspace-edges failedexec <path>
 *       `exec` something that **cannot be exec'd** -- the image named by
 *       UMBRA_EDGE_NOEXEC -- so the call returns instead of replacing this
 *       image, then `fork` and have the child do a routed write. Exits 0 when
 *       the child's write worked and the parent's did too.
 *
 *       The shape needed here is narrow, and both halves of it matter. umbra
 *       resigns every image a tracee execs and rewrites the operand to name the
 *       resigned twin, so the file must be something `codesign` accepts *with
 *       umbra's entitlements* -- which rules out a dylib: it signs, but
 *       `codesign -d --entitlements` does not report them back and umbra's own
 *       `verify` refuses it before the exec is ever reached. What does work is
 *       an ordinary arm64 executable with its **execute bits cleared**: the
 *       content is what `codesign` cares about, so signing and verification
 *       both pass, and `fs::copy` preserves the mode into the twin -- so the
 *       rewritten exec names an unexecutable file too. Measured: `execv` on it
 *       fails EACCES (13).
 *
 *       So umbra resigns a *new* image and records it, while the tracee never
 *       leaves the one it already had.
 *
 *       The `fork` afterwards is what turns that into a dead run. umbra points
 *       the child's interposer requirement at the image it believes the parent
 *       runs; if that is the unexecutable twin it matches nothing, so no trap
 *       site is breakpointed -- while the control block the child inherited
 *       through the `fork` is **armed**. The child's first routed call then
 *       issues `svc #0x80` with a trap number no breakpoint covers and the run
 *       dies on an undecoded SIGSYS, which is the #116 defect shape.
 *
 *       The child writes `failedexec` to `<path>.child` and the parent writes to
 *       `<path>`, and the Rust side reads both back through the NFSv4 client:
 *       the run surviving is necessary but not sufficient, because a run that
 *       survived while the child routed nothing would still exit 0 here.
 *
 *   umbra-userspace-edges chdirshapes <path>
 *       The three `chdir` operand shapes `chdirchild` does not reach, in one
 *       case because each is a single call and none needs a second process:
 *
 *         * `chdir` to a name that does not resolve -- must be **ENOENT to the
 *           tracee**, not a stopped run. A routed run answers its own ENOENT;
 *           resuming the trap instead would reach Darwin's `nosys` and the
 *           tracee would see ENOSYS (78).
 *         * `chdir` to a name that resolves to a **regular file** -- must be
 *           ENOTDIR (20), again to the tracee.
 *         * a **relative** operand, which is the one that exercises the
 *           `DirRef::Cwd` anchoring branch: the name is resolved against the
 *           working directory umbra currently holds *before* becoming the new
 *           one. Two of them in a row, so the second can only resolve if the
 *           first actually moved the anchor.
 *
 *       Exits 0 when all three behaved. The relative half then writes
 *       `chdirshapes` to `leaf.txt`, and the Rust side reads it back at the
 *       **entry name** two directories deep, which is the only place it can be
 *       if both relative moves anchored where they should have.
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
 *   83  a case that needs the exec helper was run without `UMBRA_EDGE_HELPER`
 *       naming it. A hard failure rather than a skip, for 80's reason: a
 *       `forkexec` that quietly exec'd nothing would report success while
 *       proving nothing about the exec'd image at all.
 *   84  a path this case has to build did not fit its buffer.
 *   85  a case that needs the unexecutable image was run without
 *       `UMBRA_EDGE_NOEXEC` naming it. A hard failure rather than a skip, for
 *       83's reason.
 *   86  the `exec` this case requires to **fail** unexpectedly succeeded, so the
 *       state it exists to reach was never entered and it must not report a
 *       pass. A successful exec never returns, so reaching the line after it is
 *       itself the diagnosis.
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

/*
 * Build `<path><suffix>` into `out`, refusing to truncate.
 *
 * Shared by every case below rather than repeated: a silently truncated path is
 * a case that asserts against the wrong object, which looks exactly like a pass
 * when the object it should have named is absent.
 */
static int derive(char *out, size_t size, const char *path, const char *suffix) {
    int written = snprintf(out, size, "%s%s", path, suffix);
    if (written <= 0 || (size_t)written >= size) {
        return 84;
    }
    return 0;
}

/* The helper binary this run's driver copied to a basename of its own. */
static const char *helper(void) { return getenv("UMBRA_EDGE_HELPER"); }

/*
 * Reap one child and report its verdict as this case's own.
 *
 * `_exit` codes travel up unchanged, so a child that failed names its own cause
 * rather than being flattened into a generic "the child failed".
 */
static int reap(pid_t pid) {
    int status = 0;
    if (waitpid(pid, &status, 0) != pid) {
        return 72;
    }
    if (!WIFEXITED(status)) {
        return 79;
    }
    return WEXITSTATUS(status);
}

/* Create `path` through routing, write `bytes`, close. Used by both helper
 * modes, which are the halves that run in an exec'd image. */
static int routed_write(const char *path, const char *bytes, size_t length) {
    errno = 0;
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return errno ? errno : 76;
    }
    errno = 0;
    if (write(fd, bytes, length) != (ssize_t)length) {
        int failure = errno ? errno : 76;
        (void)close(fd);
        return failure;
    }
    errno = 0;
    if (close(fd) < 0) {
        return errno ? errno : 76;
    }
    return 0;
}

/* The helper's `forkexec` half, and the entire content of the exec'd image's
 * `main`: one routed create/write/close. Small on purpose -- everything this
 * case is measuring happens before the first instruction of it runs. */
static int case_execwrite(const char *path) {
    return routed_write(path, "execchild", 9);
}

/*
 * The three `chdir` shapes `chdirchild` does not reach.
 *
 * All in one case and in one process: each is a single call whose whole answer
 * is its errno, so a process apiece would be ceremony rather than isolation.
 */
static int case_chdirshapes(const char *path) {
    char outer[1024];
    char inner[1024];
    int failure = derive(outer, sizeof outer, path, ".outer");
    if (failure) {
        return failure;
    }
    failure = derive(inner, sizeof inner, path, ".outer/inner");
    if (failure) {
        return failure;
    }

    /* 1. A name that does not resolve. ENOENT, and the run lives. */
    char absent[1024];
    failure = derive(absent, sizeof absent, path, ".nowhere");
    if (failure) {
        return failure;
    }
    errno = 0;
    if (chdir(absent) == 0) {
        return 71;
    }
    if (errno != ENOENT) {
        return errno ? errno : 81;
    }

    /* 2. A name that resolves to a regular file. ENOTDIR, and the run lives. */
    failure = routed_write(path, "file", 4);
    if (failure) {
        return failure;
    }
    errno = 0;
    if (chdir(path) == 0) {
        return 71;
    }
    if (errno != ENOTDIR) {
        return errno ? errno : 81;
    }

    /* 3. Two relative moves. The second can only resolve if the first moved the
     * anchor, which is the property the absolute-operand case cannot show. */
    if (mkdir(outer, 0755) != 0 || mkdir(inner, 0755) != 0) {
        return errno ? errno : 76;
    }
    char *slash = strrchr(outer, '/');
    if (!slash) {
        return 84;
    }
    errno = 0;
    if (chdir(slash + 1) != 0) {
        return errno ? errno : 76;
    }
    errno = 0;
    if (chdir("inner") != 0) {
        return errno ? errno : 76;
    }
    return routed_write("leaf.txt", "chdirshapes", 11);
}

/*
 * A *failed* exec must not move umbra's idea of which image this process runs.
 *
 * The regression test for R1. The image named by UMBRA_EDGE_NOEXEC is an
 * ordinary arm64 executable with its execute bits cleared, so `execv` on it
 * fails **EACCES (13)** and leaves this process running the image it already
 * had -- and umbra must agree, because the `fork` below hands its answer to the
 * child. See this file's `failedexec` header entry for why that shape and not a
 * dylib: a dylib fails `execv` with ENOEXEC, but umbra's resign refuses it
 * earlier still, so the exec operand is never rewritten and nothing is
 * exercised.
 */
static int case_failedexec(const char *path) {
    const char *image = getenv("UMBRA_EDGE_NOEXEC");
    if (!image) {
        return 85;
    }
    char child_path[1024];
    int failure = derive(child_path, sizeof child_path, path, ".child");
    if (failure) {
        return failure;
    }

    char *const argv[] = {(char *)image, NULL};
    errno = 0;
    execv(image, argv);
    if (errno == 0) {
        /* A successful exec does not return, so arriving here with no errno
         * means the call did something this case cannot reason about. */
        return 86;
    }

    /* The exec failed, as required. Now fork: this is where a stale image note
     * becomes a child with an armed control block and no breakpoints. */
    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        _exit(routed_write(child_path, "failedexec", 10));
    }
    failure = reap(pid);
    if (failure) {
        return failure;
    }
    return routed_write(path, "parent", 6);
}

/*
 * fork, exec a *different* binary, and require the exec'd image to route.
 *
 * The child `exec`s rather than returning, so nothing of this image's state
 * survives into it: the address space is replaced, and with it the interposer's
 * control block. Whether the exec'd image routes is therefore entirely umbra's
 * to re-establish, which is what this measures.
 */
static int case_forkexec(const char *path) {
    const char *program = helper();
    if (!program) {
        return 83;
    }
    char child_path[1024];
    int failure = derive(child_path, sizeof child_path, path, ".exec");
    if (failure) {
        return failure;
    }

    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        char *const argv[] = {(char *)program, (char *)"execwrite", child_path, NULL};
        errno = 0;
        execv(program, argv);
        /* Only reached when the exec itself failed; a successful one never
         * returns, so this cannot mask the exec'd image's own verdict. */
        _exit(errno ? errno : 76);
    }
    failure = reap(pid);
    if (failure) {
        return failure;
    }
    /* And the parent still routes after a child exec'd out from under it. */
    return routed_write(path, "parent", 6);
}

/*
 * fork, exec, `chdir`, and write through a **relative** name.
 *
 * The directory is created through routing first, so it exists in the run's
 * shadow rather than on the host -- there is nothing on the host for an
 * unrouted `chdir` to land in by luck.
 */
static int case_chdirchild(const char *path) {
    const char *program = helper();
    if (!program) {
        return 83;
    }
    char directory[1024];
    int failure = derive(directory, sizeof directory, path, ".d");
    if (failure) {
        return failure;
    }
    errno = 0;
    if (mkdir(directory, 0755) != 0) {
        return errno ? errno : 76;
    }
    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        char *const argv[] = {(char *)program, (char *)"chdirwrite", (char *)path, NULL};
        errno = 0;
        execv(program, argv);
        _exit(errno ? errno : 76);
    }
    return reap(pid);
}

/* The helper's `chdirchild` half: move, then write a name with no directory in
 * it at all, so the object's location is decided entirely by the working
 * directory umbra thinks this process has. */
static int case_chdirwrite(const char *path) {
    char directory[1024];
    int failure = derive(directory, sizeof directory, path, ".d");
    if (failure) {
        return failure;
    }
    errno = 0;
    if (chdir(directory) != 0) {
        return errno ? errno : 76;
    }
    return routed_write("leaf.txt", "chdirchild", 10);
}

/*
 * Two generations. The grandchild is the one this case is named for; the other
 * two writes are what prove the intermediate process stayed mediated across its
 * own fork rather than merely surviving it.
 */
static int case_grandchild(const char *path) {
    char middle[1024];
    char grand[1024];
    int failure = derive(middle, sizeof middle, path, ".mid");
    if (failure) {
        return failure;
    }
    failure = derive(grand, sizeof grand, path, ".grand");
    if (failure) {
        return failure;
    }

    pid_t child = fork();
    if (child < 0) {
        return 72;
    }
    if (child == 0) {
        pid_t descendant = fork();
        if (descendant < 0) {
            _exit(72);
        }
        if (descendant == 0) {
            _exit(routed_write(grand, "grandchild", 10));
        }
        int inner = reap(descendant);
        if (inner) {
            _exit(inner);
        }
        _exit(routed_write(middle, "middle", 6));
    }
    failure = reap(child);
    if (failure) {
        return failure;
    }
    return routed_write(path, "parent", 6);
}

/*
 * The child writes; then the parent stops the run.
 *
 * `O_APPEND` on a routed open is refused by the namespace as unsupported --
 * there is no atomic append-at-end storage operation, and stat-then-write is
 * wrong for a second writer -- and an unsupported resolution is propagated
 * rather than answered to the tracee, so the run ends without its terminal
 * completion record. This function does not return in a working tree; the
 * `return 71` below is the refusal failing to fire.
 */
static int case_rollbackchild(const char *path) {
    char child_path[1024];
    int failure = derive(child_path, sizeof child_path, path, ".child");
    if (failure) {
        return failure;
    }
    pid_t pid = fork();
    if (pid < 0) {
        return 72;
    }
    if (pid == 0) {
        _exit(routed_write(child_path, "rollbackchild", 13));
    }
    failure = reap(pid);
    if (failure) {
        return failure;
    }
    int fd = open(path, O_CREAT | O_WRONLY | O_APPEND, 0644);
    if (fd >= 0) {
        (void)close(fd);
    }
    return 71;
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
    if (strcmp(argv[1], "forkexec") == 0) {
        return case_forkexec(argv[2]);
    }
    if (strcmp(argv[1], "failedexec") == 0) {
        return case_failedexec(argv[2]);
    }
    if (strcmp(argv[1], "execwrite") == 0) {
        return case_execwrite(argv[2]);
    }
    if (strcmp(argv[1], "chdirchild") == 0) {
        return case_chdirchild(argv[2]);
    }
    if (strcmp(argv[1], "chdirwrite") == 0) {
        return case_chdirwrite(argv[2]);
    }
    if (strcmp(argv[1], "chdirshapes") == 0) {
        return case_chdirshapes(argv[2]);
    }
    if (strcmp(argv[1], "grandchild") == 0) {
        return case_grandchild(argv[2]);
    }
    if (strcmp(argv[1], "rollbackchild") == 0) {
        return case_rollbackchild(argv[2]);
    }
    if (strcmp(argv[1], "ctor") == 0) {
        /* Set by the constructor before `main` was entered. */
        return umbra_ctor_outcome;
    }
    return 70;
}
