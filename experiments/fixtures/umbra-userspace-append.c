/*
 * The append fixture: three ways to ask a routed namespace for `O_APPEND`,
 * told apart by whether the target exists and whether the open may create it.
 *
 * Half of a pair. `umbra-userspace-appendstd.rs` issues the same three shapes
 * through Rust's `OpenOptions`, and the comparison *is* the point -- see "WHY A
 * PAIR" below. Neither file is worth reading without the other.
 *
 * WHAT THESE CASES MEASURE -- issue #161
 * --------------------------------------
 * https://github.com/invakid404/umbra/issues/161 -- "overlay: O_APPEND is
 * deliberately refused with `Err(UnsupportedCapability)`, ending the run".
 *
 * A routed open carrying `O_APPEND` is refused by the namespace as unsupported:
 * there is no atomic append-at-end storage operation behind the routed
 * namespace, and stat-then-write is wrong for a second writer. An unsupported
 * resolution is **propagated** rather than answered to the tracee, so the run
 * ends without its terminal completion record and the tracee never receives an
 * answer at all. That is the disposition this fixture characterizes, and it is
 * a *loud* failure rather than a silent wrong answer -- which is what separates
 * it from the `#127` trap that `umbra-userspace-stdio.c` characterizes.
 *
 * THE ORDERING THESE CASES EXIST TO PIN
 * -------------------------------------
 * The refusal is not the first thing a routed open decides. `Overlay::resolve`
 * answers an **absent** target first -- a target that does not resolve and an
 * open that may not create it is `ENOENT`, decided well before the `O_APPEND`
 * test is ever reached. So `O_APPEND` on an absent path never reaches the
 * append check at all: it is ordinary name resolution, and the tracee gets a
 * plain errno while the run survives.
 *
 * That makes `bare-absent` an **ordering control** rather than an append
 * measurement, and it is the reason this file carries three cases instead of
 * two. A future change that moved the append test above the absent test would
 * silently convert `bare-absent` from a tracee `ENOENT` into a run stop, and
 * nothing else in the suite would notice. The control is the positive half that
 * `umbra-userspace-edges.c`'s header records probe D as having lacked.
 *
 *   bare-existing  `O_WRONLY | O_APPEND` on the harness's pre-seeded
 *                  `seed.txt`, which exists and is **not empty**. Reaches the
 *                  append check. This is the shape that matters most: it is one
 *                  of only two here where the target has content to append to,
 *                  so it is the only kind where the future positive invariant
 *                  -- *each successful write extends the existing content* --
 *                  is even expressible.
 *
 *   bare-absent    `O_WRONLY | O_APPEND` on a sibling that is not there, and no
 *                  `O_CREAT`. Does **not** reach the append check; the ordering
 *                  control described above. Returns the errno verbatim.
 *
 *   create-absent  `O_CREAT | O_WRONLY | O_APPEND` on a sibling that is not
 *                  there. Reaches the append check, because `O_CREAT` is what
 *                  carries it past the absent test. This is the shape a real
 *                  append-mode persistence layer issues -- open-for-append,
 *                  creating if needed -- and the half of the invariant it pins
 *                  is that the refused open creates **nothing**.
 *
 * WHY A PAIR, AND WHY NOT AN ARGV MODE IN AN EXISTING FIXTURE
 * -----------------------------------------------------------
 * `umbra-userspace-stdio.c` and `umbra-userspace-buffered.rs` are the
 * precedent: two files, one comparison, documented together. C-append against
 * Rust-append is the identical construction -- same operation, two call paths,
 * and whether the two languages reach the same refusal is the question.
 *
 * Extending an existing fixture was considered and declined, following the
 * ratified precedent in `userspace_run.rs::project_listing_fixture`, which
 * declined exactly this trade one slice earlier. `umbra-userspace-edges.c`
 * already issues the only other `O_APPEND` in the tree, in
 * `case_rollbackchild` -- but that case's *subject* is run-scoping, and its
 * append open is merely the instrument that stops the run; folding
 * append characterization into it would entangle two claims, and nineteen
 * harness call sites hang off that file. `umbra-userspace-rustio.rs`'s header
 * argues at length that its legs are transcriptions of codex's own exec-server
 * calls, which these are not.
 *
 * WHAT `case_rollbackchild` ALREADY COVERS, AND WHAT IT DOES NOT
 * --------------------------------------------------------------
 * It issues `O_CREAT | O_WRONLY | O_APPEND` -- the same flag word as
 * `create-absent` -- and its harness test asserts the run-stop disposition. So
 * the *disposition* of that one flag combination already ships, asserted. What
 * it asserts nothing about is **creation or truncation**: whether the refused
 * open left an object behind, and whether a refused append left the base file's
 * bytes alone. Those are what this fixture's cases are read for, and they are
 * net-new coverage rather than a second copy.
 *
 * THE LIVENESS SENTINEL, AND WHY ITS PLACEMENT DIFFERS BY CASE
 * -------------------------------------------------------------
 * `<path>.<case>live`, payload the case name, exactly as
 * `umbra-userspace-edges.c`'s `sentinel` writes it and for the same reason: a
 * sentinel the case itself operates on lets two assertions pass by reading what
 * the other wrote, and a payload naming the case catches a cross-wired
 * read-back.
 *
 * For `bare-existing` and `create-absent` it is written **before** the refusal,
 * per the rule `exclcollide` and `mkdirexists` establish: a case whose refusal
 * stops the run cannot put its sentinel after, because the process is never
 * resumed to write it. It is what gives the harness a positive datum proving
 * the run was alive and routing right up to the refusal.
 *
 * For `bare-absent` the refusal is non-fatal -- an ordinary tracee errno -- so
 * the sentinel goes **after**, per `missingparent`, where it additionally
 * proves the run kept routing past the refused call.
 *
 * NO DIAGNOSTIC OUTPUT, DELIBERATELY. `umbra-userspace-toy.c:21-27`'s reason: a
 * traced tracee inherits the platform provider's standard output and providers
 * are spawned with it closed, so nothing printed here is readable by any
 * harness. The verdict travels in the exit status, and for the three cases that
 * reach the append check the verdict is the *run's* rather than this program's
 * -- read from the journal, exactly as `case_rollbackchild`'s header states.
 *
 * EXIT CODES
 * ----------
 * Distinct per failure so a run that fails is diagnosable from its status
 * alone. Kept clear of `umbra-userspace-edges.c`'s 70-87,
 * `umbra-userspace-rustio.rs`'s 90-107, `umbra-userspace-buffered.rs`'s 110-114
 * and `umbra-userspace-stdio.c`'s 120-129, so a status is never ambiguous
 * between fixtures -- the rule those four headers each state. 130 is the first
 * free *decade above the allocated range*, which is the unit every fixture
 * above allocates in: the blocks ascend 70s, 90s-100s, 110s, 120s, and now
 * 130s. The qualifier is load-bearing -- 10-69 is entirely unused, so 130 is
 * emphatically not the first free decade, it is the first free one that
 * continues the ascent. 88-89, 108-109 and 115-119 are free too, but a block
 * that starts mid-decade would be the first here to do so. Shared with
 * `umbra-userspace-appendstd.rs`, which is the pair's whole point, and
 * code-for-code identical with it so a status means the same thing whichever
 * language produced it.
 *
 * One correction to that survey, recorded because these two headers are now the
 * most recent one: `umbra-userspace-buffered.rs` records edges.c's block as
 * 70-86, but edges.c does define 87 (its liveness-sentinel code, mirrored by
 * 134 below), so 70-87 above is the accurate range. buffered.rs is a shipped
 * fixture and is deliberately left untouched; the note is here so the next
 * reader takes the wider range.
 *
 *   errno an errno, returned verbatim. The expected outcome for `bare-absent`
 *         alone (2 = ENOENT). From one of the other two it means the refusal
 *         was answered to the tracee instead of stopping the run -- a real
 *         disposition change, and the status says which errno.
 *
 *         **The bound is 106, not 64.** `sys/errno.h` on this SDK puts `ELAST`
 *         at 107, and real values reach 106 -- `EOVERFLOW` 84, `EOWNERDEAD`
 *         105, `EQFULL` 106. So a verbatim errno can land anywhere in 0-106,
 *         which overlaps *three* of the blocks above, not one: the 2-9 range
 *         shared by `umbra-userspace-toy.c`, `umbra-userspace-listing.c` and
 *         `umbra-userspace-listing-project.c`; `umbra-userspace-edges.c`'s
 *         70-87; and `umbra-userspace-rustio.rs`'s 90-107, where a status in
 *         90-106 is ambiguous between an errno from this pair and a step code
 *         from `rustio`. Stated rather than glossed, because the cross-fixture
 *         rule is weaker than a reader of the block list would assume.
 *
 *         **None of that is a defect, and the reasons belong together.**
 *         `ENOENT` (2) is the only errno these cases can produce -- every other
 *         disposition is a run stop, which returns no status at all -- so the
 *         2-9 overlap is the only one ever reached, and the 70-87 and 90-106
 *         ones are theoretical. No errno can reach this file's own block, since
 *         none is >= 130. And the harness always knows which fixture it
 *         launched, so no status is ever read without that context. The
 *         ambiguity is a human-diagnosis nicety, not a correctness problem.
 *   130   wrong argument count, or an unknown case name
 *   131   AN OPEN THAT WAS EXPECTED TO BE REFUSED SUCCEEDED. The non-green-wash
 *         alarm: it is what fires the day append is admitted, which is when
 *         these cases are supposed to go red. See "WHEN THIS GOES RED" below.
 *   132   setup failed -- the target could not be prepared or inspected, so the
 *         case never reached the open it exists to issue
 *   133   the refused open failed but reported **no** errno, so there is no
 *         POSIX answer to return. Mirrors `umbra-userspace-edges.c`'s 81: a
 *         zero errno after a failed call must never read as success.
 *   134   the liveness sentinel's own routed write failed, so the case has
 *         nothing to say about what came after it. Mirrors edges.c's 87.
 *
 * WHEN THIS GOES RED
 * ------------------
 * Deliberately. These cases characterize a refusal that is *supposed* to be
 * lifted -- #161 carries two candidate remedies -- and they are written so that
 * lifting it fails them loudly instead of passing them vacuously. On the day a
 * routed `O_APPEND` is admitted, `bare-existing` and `create-absent` exit 131
 * and the harness's run-stop assertions fail. What to change is in each test's
 * doc comment in `userspace_run.rs`, and the short version is: the
 * characterization is replaced by the positive invariant -- for `bare-existing`,
 * that the appended bytes **extend** `seed\n` rather than replacing it.
 *
 * Neither case is `#[ignore]`d on the harness side, for the reason
 * `userspace_run.rs`'s header gives: no step of the job passes
 * `--include-ignored`, so an ignored case reports green without ever running,
 * which is the skip-as-pass shape the suite exists to catch.
 *
 * Build: clang -arch arm64 -O1 umbra-userspace-append.c -o umbra-userspace-append
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
 * `umbra-userspace-edges.c`'s `derive`, for its reason: a silently truncated
 * path is a case that asserts against the wrong object, which looks exactly
 * like a pass when the object it should have named is absent.
 */
static int derive(char *out, size_t size, const char *path, const char *suffix) {
    int written = snprintf(out, size, "%s%s", path, suffix);
    if (written <= 0 || (size_t)written >= size) {
        return 132;
    }
    return 0;
}

/*
 * Build `<dirname(path)>/<name>` into `out`, refusing to truncate.
 *
 * The harness hands every fixture one destination inside the workspace and
 * each derives the siblings it needs from it -- `derive` covers the suffix
 * case, and this covers the one `derive` cannot: naming a *different* file in
 * the same directory. `seed.txt` is that file, and it is the harness's own
 * pre-seeded nonempty object rather than anything this fixture creates, which
 * is what makes `bare-existing` an append against content the run did not
 * write. `umbra-userspace-rustio.rs` reaches it as `parent.join("seed.txt")`.
 */
static int sibling(char *out, size_t size, const char *path, const char *name) {
    const char *slash = strrchr(path, '/');
    if (slash == NULL) {
        return 132;
    }
    int written = snprintf(out, size, "%.*s/%s", (int)(slash - path), path, name);
    if (written <= 0 || (size_t)written >= size) {
        return 132;
    }
    return 0;
}

/* One routed create-and-write, used only by the sentinel below. */
static int routed_write(const char *path, const char *bytes, size_t n) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return 134;
    }
    if (write(fd, bytes, n) != (ssize_t)n) {
        (void)close(fd);
        return 134;
    }
    if (close(fd) < 0) {
        return 134;
    }
    return 0;
}

/*
 * Write the liveness sentinel for one case, at `<path>.<case>live`.
 *
 * Collapsed to 134 rather than propagating the errno, for the reason
 * `umbra-userspace-edges.c` gives its 87: the sentinel is a known-good
 * operation that every other case crosses, so its failure is a broken run
 * rather than this case's finding, and a distinct code is the diagnosis.
 */
static int sentinel(const char *path, const char *case_name) {
    char derived[1024];
    char suffix[64];
    int written = snprintf(suffix, sizeof suffix, ".%slive", case_name);
    if (written <= 0 || (size_t)written >= sizeof suffix) {
        return 132;
    }
    int failure = derive(derived, sizeof derived, path, suffix);
    if (failure) {
        return failure;
    }
    return routed_write(derived, case_name, strlen(case_name));
}

/*
 * Issue one open that is expected to be refused, and report what came back.
 *
 * Shared by the two cases that reach the append check, because the reporting is
 * the delicate part and must not drift between them: a *successful* open is the
 * alarm, a failure with an errno is a disposition change worth naming by errno,
 * and a failure without one must never read as success.
 *
 * In a tree whose refusal still fires this function **does not return** -- the
 * run is stopped inside the open and the process is never resumed. Every path
 * below is therefore a finding.
 */
static int refused_open(const char *path, int flags) {
    errno = 0;
    int fd = open(path, flags, 0644);
    if (fd >= 0) {
        (void)close(fd);
        return 131;
    }
    if (errno == 0) {
        return 133;
    }
    return errno;
}

/*
 * `O_WRONLY | O_APPEND` against the harness's pre-seeded, nonempty `seed.txt`.
 *
 * The target is stat'd first, and that is setup rather than ceremony: this case
 * means nothing unless the file it appends to actually has content, so "exists
 * and is not empty" is checked rather than assumed. A zero-length `seed.txt`
 * would make the no-truncation half of the invariant unobservable.
 */
static int case_bare_existing(const char *path) {
    char target[1024];
    int failure = sibling(target, sizeof target, path, "seed.txt");
    if (failure) {
        return failure;
    }
    struct stat info;
    if (stat(target, &info) != 0 || info.st_size == 0) {
        return 132;
    }
    failure = sentinel(path, "bare-existing");
    if (failure) {
        return failure;
    }
    return refused_open(target, O_WRONLY | O_APPEND);
}

/*
 * `O_WRONLY | O_APPEND` against a sibling that is not there -- the ordering
 * control. No `O_CREAT`, so name resolution answers before the append check is
 * reached and the tracee gets an ordinary errno.
 *
 * The sentinel comes **after**, per `missingparent`: the refusal here is
 * non-fatal, so the run is still alive to write it, and that it lands is the
 * other half of the claim -- the run kept routing past a refused call.
 */
static int case_bare_absent(const char *path) {
    char target[1024];
    int failure = derive(target, sizeof target, path, ".ab");
    if (failure) {
        return failure;
    }
    errno = 0;
    int fd = open(target, O_WRONLY | O_APPEND);
    if (fd >= 0) {
        (void)close(fd);
        return 131;
    }
    int answer = errno;
    failure = sentinel(path, "bare-absent");
    if (failure) {
        return failure;
    }
    if (answer == 0) {
        return 133;
    }
    return answer;
}

/*
 * `O_CREAT | O_WRONLY | O_APPEND` against a sibling that is not there.
 *
 * `O_CREAT` is what carries this past the absent test and into the append
 * check, which is the whole difference from `bare-absent` above. The target is
 * deliberately *not* pre-created: what the harness reads this case for is that
 * the refused open created nothing.
 */
static int case_create_absent(const char *path) {
    char target[1024];
    int failure = derive(target, sizeof target, path, ".ap");
    if (failure) {
        return failure;
    }
    failure = sentinel(path, "create-absent");
    if (failure) {
        return failure;
    }
    return refused_open(target, O_CREAT | O_WRONLY | O_APPEND);
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 130;
    }
    if (strcmp(argv[1], "bare-existing") == 0) {
        return case_bare_existing(argv[2]);
    }
    if (strcmp(argv[1], "bare-absent") == 0) {
        return case_bare_absent(argv[2]);
    }
    if (strcmp(argv[1], "create-absent") == 0) {
        return case_create_absent(argv[2]);
    }
    return 130;
}
