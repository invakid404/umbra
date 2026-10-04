/*
 * The descriptor fixture: six ways to ask a routed namespace to operate on a
 * descriptor it already handed out, plus one that it answers.
 *
 * Half of a pair. `umbra-userspace-descriptorstd.rs` issues four of the same
 * shapes through `std::fs::File`, and the comparison *is* the point -- see "WHY
 * A PAIR" below. Neither file is worth reading without the other.
 *
 * WHAT THESE CASES MEASURE
 * ------------------------
 * A routed `open` succeeds and returns a descriptor above the 4096 fence -- it
 * is umbra's, not the kernel's. `read` and `write` on that descriptor are
 * served. Six other descriptor-relative calls are **not**: each has no row in
 * `abi::TRACED_STUBS`, so no breakpoint covers it, the call goes to the kernel
 * bare, and the kernel has never heard of the number. The tracee gets
 * `EBADF`(9) -- a plain POSIX answer, delivered to the tracee, with the run
 * surviving and recording its terminal completion.
 *
 *   lseek      `lseek(fd, 0, SEEK_END)` -- 199. The one a reader expects to
 *              work, because a descriptor without a position is not a file
 *              descriptor in any sense a program recognises.
 *   dupfd      `fcntl(fd, F_DUPFD, 0)` -- 92, command 0. The aliasing shape.
 *   ftruncate  `ftruncate(fd, 2)` -- 201. The destructive one, and the only
 *              case here that would change the target's bytes if it worked.
 *   fsync      `fsync(fd)` -- 95. The durability shape, and the one whose
 *              refusal a program is least able to compensate for.
 *   pread      `pread(fd, buf, 4, 0)` -- 153. Positional read; `read` on the
 *              same descriptor is served, so the pair isolates *positional*.
 *   pwrite     `pwrite(fd, "X", 1, 0)` -- 154. Positional write, same logic.
 *   fstat      `fstat(fd, &info)` -- 339. **The served-member control.** It
 *              SUCCEEDS on the very descriptor the six are refused on.
 *
 * WHY THE CONTROL IS THE MOST IMPORTANT CASE IN THE FILE
 * ------------------------------------------------------
 * Without it, six `EBADF`s are consistent with a much duller and much less
 * actionable claim: that descriptor-relative calls simply do not work on a
 * routed descriptor, perhaps because the open did not really route. `fstat` is
 * descriptor-relative, is issued on the same descriptor at the same point in
 * the same run, and is **answered**. That makes the six refusals a statement
 * about `TRACED_STUBS` *membership* -- one call site per row, and these six
 * have no row -- rather than about descriptors. It is also the non-vacuity
 * proof for the whole slice, and a stronger one than a mutation probe because
 * it runs in the **unmutated** job.
 *
 * WHY A PAIR, AND WHY NOT AN ARGV MODE IN AN EXISTING FIXTURE
 * -----------------------------------------------------------
 * `umbra-userspace-stdio.c` + `umbra-userspace-buffered.rs` and then
 * `umbra-userspace-append.c` + `umbra-userspace-appendstd.rs` are the
 * precedent: two files, one comparison, documented together. Here the pair is
 * *more* load-bearing than it was for append, not less, and the reason is a
 * measurement rather than a symmetry: `File::try_clone` is
 * `fcntl(F_DUPFD_CLOEXEC)` -- command **67**, not the `F_DUPFD` command 0 this
 * file issues -- and `File::sync_all` is `fcntl(F_FULLFSYNC)`, command 51, not
 * the `fsync`(95) this file issues. So the two halves provably do **not**
 * present the same syscall for two of the four shared shapes, which is the
 * opposite of the append pair, where both halves provably set the same flag
 * bit. A one-language fixture would have reported the wrong call.
 *
 * Extending an existing fixture was declined, following the ratified precedent
 * in `userspace_run.rs::project_listing_fixture` and `umbra-userspace-append.c`
 * before it. `umbra-userspace-edges.c` is the file a reader would reach for --
 * it already argv-dispatches and already has a descriptor in hand -- but
 * nineteen harness call sites hang off it (`routed_edge(` 19 occurrences = 1
 * definition + 18 uses, one of them inside `routed_exec_edge`; `routed_exec_edge(`
 * 3 = 1 definition + 2 uses; 17 + 2 = 19), its exit block is already 70-87, and
 * shipped fixtures are the suite's controls and are not edited to carry new
 * claims.
 *
 * ONE BINARY WITH ARGV MODES, NOT SEVEN BINARIES. Seven would need seven
 * `fixture_binary` names, seven `UMBRA_*_PATH` variables and seven launchers to
 * express one disposition that measured identically six times out of six.
 * `umbra-userspace-edges.c` is the tree's own demonstration that one
 * argv-dispatched binary scales to nineteen call sites.
 *
 * THE FENCE GUARD, AND WHY EVERY CASE HAS ONE
 * --------------------------------------------
 * umbra hands out descriptor numbers at or above 4096; the kernel's own start
 * at 0 and stay small. A case whose `open` was *not* routed would get a small
 * number, and every assertion below it would then be a statement about an
 * ordinary host file -- an `lseek` that works, an `fsync` that works, and a
 * green test asserting nothing about umbra at all. 145 is that check, and it
 * comes before the primitive rather than after, so no finding can be read off
 * an unrouted descriptor.
 *
 * TWO SENTINELS, NOT ONE
 * -----------------------
 * `<path>.<case>live` before the primitive and `<path>.<case>after` after it,
 * both with the case name as payload, exactly as `umbra-userspace-edges.c`'s
 * `sentinel` writes it and for its reasons: a sentinel the case itself operates
 * on lets two assertions pass by reading what the other wrote, and a payload
 * naming the case catches a cross-wired read-back.
 *
 * Two rather than one because this disposition is a **survivable** errno, which
 * is what separates this slice from the append pair. There, the refusal stops
 * the run, so a post-sentinel could never be written and `missingparent`'s
 * rule puts the sentinel before. Here both are reachable and both are needed:
 * the first proves the run was alive and routing right up to the refusal, and
 * the second proves it still was afterwards. Together they are the slate's
 * boundedness invariant asserted on **bytes read back through the client**
 * rather than inferred from an exit code -- this suite's standing discipline,
 * where an exit code is never allowed to stand in for a byte.
 *
 * The post-sentinel is also read back *here*, in-process, before the fixture
 * exits: written, closed, reopened, compared. That is what 148 is. The harness
 * reads the same object independently over NFSv4, so the claim is made twice
 * through two different paths, and a disagreement between them would be a
 * finding in its own right.
 *
 * NO DIAGNOSTIC OUTPUT, DELIBERATELY. `umbra-userspace-toy.c:21-27`'s reason: a
 * traced tracee inherits the platform provider's standard output and providers
 * are spawned with it closed, so nothing printed here is readable by any
 * harness. The verdict travels in the exit status, and unlike the append pair's
 * two refused cases the verdict here is this program's own -- the tracee is
 * resumed and does report one.
 *
 * EXIT CODES
 * ----------
 * Distinct per failure so a run that fails is diagnosable from its status
 * alone. Kept clear of `umbra-userspace-edges.c`'s 70-87,
 * `umbra-userspace-rustio.rs`'s 90-107, `umbra-userspace-buffered.rs`'s
 * 110-114, `umbra-userspace-stdio.c`'s 120-126 and 129, and the append pair's
 * 130-134 -- the rule every one of those headers states. **140 is the first
 * free decade above the allocated range**, which is the unit they all allocate
 * in: the blocks ascend 70s, 90s-100s, 110s, 120s, 130s, and now 140s. The
 * qualifier is load-bearing twice over. 10-69 is entirely unused, so 140 is
 * emphatically not the first free decade; and 135-139 *is* free, but a block
 * starting mid-decade would be the first here to do so, which
 * `umbra-userspace-append.c`'s header explicitly declines as a precedent. 149
 * is left free: the block is allocated to the codes the cases can actually
 * produce and no spare is pre-spent.
 *
 * Shared code-for-code with `umbra-userspace-descriptorstd.rs`, which is the
 * pair's whole point: a status means the same thing whichever language produced
 * it. One asymmetry, stated rather than left to be noticed -- **147 is
 * reachable from this half only.** `std` exposes no `fcntl`, and the Rust half
 * takes no dependency and uses no `unsafe`, as every other Rust fixture here
 * does not. Its header records the same thing. This is the shape
 * `umbra-userspace-appendstd.rs` already has, where the truncation arm of 132
 * is reachable from the C half alone because `OsString` grows; the code's
 * *meaning* is shared even where its reachability is not.
 *
 *   errno an errno, returned verbatim. **`9` = `EBADF` is the only value these
 *         cases can produce**, and it is the measured disposition: any other
 *         errno is reported as 146 instead, below, because the boundedness
 *         steps that follow assume the `EBADF` answer and must not run on a
 *         different one. No errno can reach this file's own block, since none
 *         is >= 140 -- `sys/errno.h` on this SDK puts `ELAST` at 107.
 *
 *         It does overlap the 2-9 block shared by `umbra-userspace-toy.c`,
 *         `umbra-userspace-listing.c` and `umbra-userspace-listing-project.c`.
 *         That is a human-diagnosis nicety rather than a correctness problem,
 *         for the reason `umbra-userspace-append.c`'s header gives at length:
 *         the harness always knows which fixture it launched.
 *   140   wrong argument count, or an unknown case name
 *   141   THE PRIMITIVE SUCCEEDED. Two readings, and which one applies depends
 *         on the case rather than on the code. For `fstat` it is the
 *         **expected** status: the control exists to succeed, and the day it
 *         stops is the day the membership claim has changed. For the other six
 *         it is the non-green-wash alarm -- what fires the day that primitive
 *         is admitted, which is when its case is supposed to go red. See "WHEN
 *         THIS GOES RED" below.
 *   142   setup failed -- a path could not be built or the target could not be
 *         prepared or inspected, or the routed open of it failed, so the case
 *         never reached the primitive it exists to issue
 *   143   the primitive failed but reported **no** errno, so there is no POSIX
 *         answer to return. Mirrors `umbra-userspace-edges.c`'s 81 and the
 *         append pair's 133: a zero errno after a failed call must never read
 *         as success.
 *   144   a liveness sentinel's own routed write failed, so the case has
 *         nothing to say about what came before or after it. Mirrors edges.c's
 *         87 and the append pair's 134, and covers **both** sentinels here.
 *   145   the routed open returned a descriptor **below the 4096 fence**, so
 *         the open was not routed and nothing else in the case would mean
 *         anything. See "THE FENCE GUARD" above.
 *   146   the primitive failed with an errno that is **not** `EBADF` -- a
 *         disposition change short of success, and the one the two candidate
 *         remedies for an unimplementable primitive would produce (`ENOTSUP`,
 *         `ENOSYS`). Collapsed to one code deliberately: 147 and 148 below
 *         assume the `EBADF` answer, so a different errno must stop the case
 *         here rather than flow into claims that no longer apply.
 *   147   an unrelated **kernel** descriptor's state changed across the
 *         refusal. Half of the boundedness invariant: a refusal that perturbed
 *         a descriptor it was never given would be unbounded even while
 *         returning the right errno. `fcntl(2, F_GETFD)` before and after --
 *         standard error, which is an **open kernel descriptor** the tracee
 *         inherits and never routes, so its flags read back and it is exactly
 *         the kind of bystander a bounded refusal must leave alone. It is below
 *         the 4096 fence, so no routed call can reach it. (The closed-stdout
 *         claim this family's headers carry is about fd 1, and is deliberately
 *         not extended to fd 2 here: were fd 2 closed, `prepare` would return
 *         142 and every case in the file would fail rather than measure
 *         anything.)
 *   148   the post-refusal routed write and its read-back disagreed, so the run
 *         stopped routing correctly at the refusal even though it survived it.
 *         The other half of the boundedness invariant, and the half an exit
 *         code alone could not carry.
 *
 * WHEN THIS GOES RED
 * ------------------
 * Deliberately, and one case at a time. These cases characterize refusals that
 * are *supposed* to be lifted, and they are written so that lifting one fails
 * it loudly instead of passing it vacuously: on the day a primitive is
 * admitted, its case exits 141 and its harness assertion fails on
 * `child_exit() == 9`. What to write in its place is in each test's doc comment
 * in `userspace_run.rs`, and the replacement is the **positive invariant** for
 * that primitive rather than a looser version of the characterization --
 * shared offsets for an alias, an unchanged offset for positional I/O, exact
 * truncation, an honest sync result. None of what is recorded here is an
 * acceptable permanent behaviour and none of these cases should be relaxed to
 * keep it green.
 *
 * The control is the exception and inverts: `fstat` exiting anything but 141 is
 * a regression, not progress, and its case says so.
 *
 * No case is `#[ignore]`d on the harness side, for the reason
 * `userspace_run.rs`'s header gives: no step of the job passes
 * `--include-ignored`, so an ignored case reports green without ever running,
 * which is the skip-as-pass shape the suite exists to catch.
 *
 * Build: clang -arch arm64 -O1 umbra-userspace-descriptor.c -o umbra-userspace-descriptor
 */

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/*
 * Build `<path><suffix>` into `out`, refusing to truncate.
 *
 * `umbra-userspace-edges.c`'s `derive` and the append pair's, for their reason:
 * a silently truncated path is a case that asserts against the wrong object,
 * which looks exactly like a pass when the object it should have named is
 * absent.
 */
static int derive(char *out, size_t size, const char *path, const char *suffix) {
    int written = snprintf(out, size, "%s%s", path, suffix);
    if (written <= 0 || (size_t)written >= size) {
        return 142;
    }
    return 0;
}

/*
 * Build `<dirname(path)>/<name>` into `out`, refusing to truncate.
 *
 * The harness hands every fixture one destination inside the workspace and each
 * derives the siblings it needs from it -- `derive` covers the suffix case, and
 * this covers the one `derive` cannot: naming a *different* file in the same
 * directory. `seed.txt` is that file, and it matters that it is the harness's
 * own pre-seeded nonempty object rather than anything this fixture created: the
 * routed open of it **copies the base file up**, so every case here runs
 * against a shadow object with real content, which is what makes `ftruncate`'s
 * and `pwrite`'s refusals observable as "the bytes are still there".
 */
static int sibling(char *out, size_t size, const char *path, const char *name) {
    const char *slash = strrchr(path, '/');
    if (slash == NULL) {
        return 142;
    }
    int written = snprintf(out, size, "%.*s/%s", (int)(slash - path), path, name);
    if (written <= 0 || (size_t)written >= size) {
        return 142;
    }
    return 0;
}

/* One routed create-and-write, used only by the sentinels below. */
static int routed_write(const char *path, const char *bytes, size_t n) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return 144;
    }
    if (write(fd, bytes, n) != (ssize_t)n) {
        (void)close(fd);
        return 144;
    }
    if (close(fd) < 0) {
        return 144;
    }
    return 0;
}

/*
 * Write one of the two liveness sentinels, at `<path>.<case><when>`.
 *
 * Collapsed to 144 rather than propagating the errno, for the reason
 * `umbra-userspace-edges.c` gives its 87: a sentinel is a known-good operation
 * that every case crosses, so its failure is a broken run rather than this
 * case's finding, and a distinct code is the diagnosis.
 */
static int sentinel(const char *path, const char *case_name, const char *when) {
    char derived[1024];
    char suffix[64];
    int written = snprintf(suffix, sizeof suffix, ".%s%s", case_name, when);
    if (written <= 0 || (size_t)written >= sizeof suffix) {
        return 142;
    }
    int failure = derive(derived, sizeof derived, path, suffix);
    if (failure) {
        return failure;
    }
    return routed_write(derived, case_name, strlen(case_name));
}

/*
 * Write the post-refusal sentinel and read it straight back, through routing.
 *
 * The write alone would prove only that an `open` and a `write` still returned
 * zero after the refusal. Reading the bytes back on a fresh descriptor is what
 * proves they *landed* -- the distinction `umbra-userspace-stdio.c` exists for,
 * where a discarded `fclose` leaves the object existing, empty, at exit 0. A
 * post-refusal write that silently lost its payload is exactly the shape that
 * would make the rest of this file's boundedness claim false while passing it.
 */
static int survived(const char *path, const char *case_name) {
    int failure = sentinel(path, case_name, "after");
    if (failure) {
        return failure;
    }
    char derived[1024];
    char suffix[64];
    int written = snprintf(suffix, sizeof suffix, ".%safter", case_name);
    if (written <= 0 || (size_t)written >= sizeof suffix) {
        return 142;
    }
    failure = derive(derived, sizeof derived, path, suffix);
    if (failure) {
        return failure;
    }
    int fd = open(derived, O_RDONLY);
    if (fd < 0) {
        return 148;
    }
    char back[64];
    size_t n = strlen(case_name);
    ssize_t got = read(fd, back, sizeof back);
    (void)close(fd);
    if (got != (ssize_t)n || memcmp(back, case_name, n) != 0) {
        return 148;
    }
    return 0;
}

/*
 * Report what a primitive that was expected to be refused actually did, and
 * check the two halves of the boundedness invariant when it was refused.
 *
 * Shared by all seven cases, because the reporting is the delicate part and
 * must not drift between them: a *successful* primitive is 141 -- the alarm for
 * six of the cases and the expected status for the control -- a failure with an
 * errno other than `EBADF` is a disposition change worth its own code, and a
 * failure without an errno must never read as success.
 *
 * `result` is the primitive's own return value and `answer` the `errno` it
 * left, captured by the caller immediately after the call and before anything
 * else -- including before this function is entered, since a function call is
 * not guaranteed to preserve `errno`.
 */
static int report(const char *path, const char *case_name, long result, int answer,
                  int fence_before) {
    if (result >= 0) {
        return 141;
    }
    if (answer == 0) {
        return 143;
    }
    if (answer != EBADF) {
        return 146;
    }
    if (fcntl(2, F_GETFD) != fence_before) {
        return 147;
    }
    int failure = survived(path, case_name);
    if (failure) {
        return failure;
    }
    return answer;
}

/*
 * Everything the seven cases share: the unrelated kernel descriptor's state,
 * the pre-refusal sentinel, the target, and the routed open behind the fence.
 *
 * Kept in one function rather than repeated seven times because a case that
 * differed here by accident would be measuring something other than its
 * primitive, and the whole slice turns on all seven reaching the identical
 * state before the one call that distinguishes them.
 */
static int prepare(const char *path, const char *case_name, int *fence_before, int *fd) {
    *fence_before = fcntl(2, F_GETFD);
    if (*fence_before < 0) {
        return 142;
    }
    int failure = sentinel(path, case_name, "live");
    if (failure) {
        return failure;
    }
    char target[1024];
    failure = sibling(target, sizeof target, path, "seed.txt");
    if (failure) {
        return failure;
    }
    /*
     * The target is stat'd first, and that is setup rather than ceremony: the
     * destructive cases mean nothing unless the file they could damage actually
     * has content, so "exists and is not empty" is checked rather than assumed.
     * A zero-length `seed.txt` would make `ftruncate`'s and `pwrite`'s
     * refusals unobservable. `umbra-userspace-append.c::case_bare_existing`
     * checks the same thing for the same reason.
     */
    struct stat info;
    if (stat(target, &info) != 0 || info.st_size == 0) {
        return 142;
    }
    *fd = open(target, O_RDWR);
    if (*fd < 0) {
        return 142;
    }
    if (*fd < 4096) {
        (void)close(*fd);
        return 145;
    }
    return 0;
}

/*
 * One case: prepare, issue EXACTLY ONE primitive, report.
 *
 * The `which` dispatch is inside the measured region deliberately -- a
 * `strcmp` is not a syscall, so no routed call separates the fence guard from
 * the primitive, and the primitive is the first thing the routed descriptor
 * sees after the open that produced it.
 */
static int run_case(const char *path, const char *case_name) {
    int fence_before = -1;
    int fd = -1;
    int failure = prepare(path, case_name, &fence_before, &fd);
    if (failure) {
        return failure;
    }

    char buffer[8];
    struct stat info;
    long result = -1;
    errno = 0;
    if (strcmp(case_name, "lseek") == 0) {
        result = (long)lseek(fd, 0, SEEK_END);
    } else if (strcmp(case_name, "dupfd") == 0) {
        result = (long)fcntl(fd, F_DUPFD, 0);
    } else if (strcmp(case_name, "ftruncate") == 0) {
        result = (long)ftruncate(fd, 2);
    } else if (strcmp(case_name, "fsync") == 0) {
        result = (long)fsync(fd);
    } else if (strcmp(case_name, "pread") == 0) {
        result = (long)pread(fd, buffer, 4, 0);
    } else if (strcmp(case_name, "pwrite") == 0) {
        result = (long)pwrite(fd, "X", 1, 0);
    } else if (strcmp(case_name, "fstat") == 0) {
        result = (long)fstat(fd, &info);
    } else {
        (void)close(fd);
        return 140;
    }
    int answer = errno;

    int verdict = report(path, case_name, result, answer, fence_before);
    /*
     * The refused descriptor is closed last, and its result is deliberately
     * discarded. `close` has a `TRACED_STUBS` row and was measured to succeed
     * on a descriptor every one of these primitives was just refused on -- but
     * naming that would cost an exit code, and the block is allocated to the
     * claims the cases are read for. Discarding it is also what the harness
     * needs: a verdict already reached must not be overwritten by a later call.
     */
    (void)close(fd);
    return verdict;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 140;
    }
    const char *case_name = argv[1];
    if (strcmp(case_name, "lseek") != 0 && strcmp(case_name, "dupfd") != 0 &&
        strcmp(case_name, "ftruncate") != 0 && strcmp(case_name, "fsync") != 0 &&
        strcmp(case_name, "pread") != 0 && strcmp(case_name, "pwrite") != 0 &&
        strcmp(case_name, "fstat") != 0) {
        return 140;
    }
    return run_case(argv[2], case_name);
}
