/*
 * The buffered-output fixture: three ways to put the same bytes in the same
 * routed destination, told apart only by what the program does with the
 * result.
 *
 * WHY ONE ARGV-DISPATCHED BINARY AND NOT THREE
 * --------------------------------------------
 * The rule this experiment is built around is "persist the same small
 * generated-file payload in every case, and inspect it independently through
 * the NFS client". One image makes the first half of that a single `PAYLOAD`
 * constant every case reads; three images would make it three copies, three
 * places to drift, and a convention nothing checks. The three shapes differ in
 * exactly one dimension -- how the write's result is handled -- and that
 * dimension is the axis under test, so splitting them across images would
 * separate the very things whose comparability is the whole point. The
 * precedent is `umbra-userspace-edges.c`, which dispatches eighteen cases
 * (sixteen of them launched by the harness) the same way.
 *
 * WHAT THE THREE CASES MEASURE -- issue #127
 * ------------------------------------------
 * umbra routes a tracee's filesystem calls two ways: breakpoints on the libc
 * stubs listed in `abi::TRACED_STUBS`, and `DYLD_INTERPOSE` over the
 * executable's own call sites. A routed descriptor's number is >= 4096, which
 * is not a number the kernel has a descriptor for, so a call that escapes both
 * mechanisms and reaches the kernel directly fails with `EBADF`.
 *
 * Measured on this host with lldb, stdio's output path is
 *
 *     fflush / fclose -> __sflush -> _swrite -> __swrite -> __write_nocancel(397)
 *
 * and 397 escapes both: it is absent from `TRACED_STUBS`, and interposition
 * cannot rebind a call made from inside the dyld shared cache. Its neighbours
 * do not escape -- `fopen`'s `__open_nocancel`(398) is breakpointed, `fclose`'s
 * `__close_nocancel`(399) is breakpointed, and the `fstat`(339) that
 * `__smakebuf` uses to size the buffer is breakpointed -- so the object is
 * created, sized and released perfectly cleanly, and holds nothing.
 *
 * Note which symbol that is. #127's own "Cause" section says the call is public
 * `write`(4); measured, `write`(4) is never entered on this path. A remedy that
 * routed only `write` would leave #127 exactly as live as it is now.
 *
 *   raw      open(2) + write(2) + close(2). The positive control: these are the
 *            executable's own call sites, so interposition reaches them, and
 *            the export ends up holding every byte. It is what proves the other
 *            two cases' empty object is the defect and not a read-back that
 *            never works.
 *
 *   checked  fopen + fprintf + fflush + fclose with every result consulted.
 *            The failure is observable here: `fflush` returns -1 with `errno`
 *            EBADF and `ferror` set, and this case exits naming that arm.
 *
 *   ignored  the same calls, with `fclose`'s result discarded -- which is what
 *            a great many programs do. Measured: `fprintf` returns the full
 *            byte count, `ferror` stays clear, the process exits 0, and the
 *            export holds an object that *exists* with *zero bytes* in it.
 *            Exit 0 over an empty file, with nothing anywhere reporting a
 *            problem, is the trap this fixture exists to make executable.
 *
 * THE PAYLOAD MUST STAY SMALL, AND 4095 BYTES IS THE MEASURED BOUND
 * -----------------------------------------------------------------
 * stdio sizes its buffer from the descriptor's `st_blksize`, which this backend
 * reports as 4096. Payload sweep on a routed descriptor: at 100 and at 4095
 * bytes `fprintf` returns the full count with `ferror` clear and the export
 * holds 0 bytes -- silent. At 4096 and above the buffer spills during the
 * `fprintf` itself, so `fprintf` returns -1 and any program that checks
 * printf's result catches it there.
 *
 * So the defect is silent *iff* the payload fits the buffer. A payload at or
 * above 4096 would stop exercising #127's silent shape and would turn
 * `ignored` into a case that reports its own failure. `PAYLOAD` is 20 bytes and
 * must never reach 4096.
 *
 * NO DIAGNOSTIC OUTPUT, DELIBERATELY
 * ----------------------------------
 * `umbra-userspace-toy.c:21-27`'s reason, and here it is doubly binding: a
 * traced tracee inherits the platform provider's standard output, and providers
 * are spawned with it closed, so nothing a fixture prints can be read by any
 * harness. The verdict travels in the exit status alone; the bytes are read out
 * of the export by the harness, through the NFSv4 client, with nothing mounted.
 *
 * WHAT THIS FIXTURE DOES NOT CLAIM
 * --------------------------------
 * It does not claim a measured set of syscalls for its `raw` case. The two
 * buffered cases link stdio, so the image contains stdio symbols whichever case
 * runs, and `umbra-userspace-toy.c:25-27`'s `nm -u` argument -- "exactly
 * `_open`, `_read`, `_write`, `_close`" -- is simply not available here. That is
 * a real cost of one binary over three and it is paid knowingly;
 * `umbra-userspace-rustio.rs` argues the general case at length. What this
 * fixture proves is behavioural: the exit codes below, plus the bytes and sizes
 * the harness reads back out of the export.
 *
 * EXIT CODES
 * ----------
 * Distinct per failure so a run that fails is diagnosable from its status
 * alone, with no debugger and no log. Kept clear of
 * `umbra-userspace-edges.c`'s 70-86 and `umbra-userspace-rustio.rs`'s 90-107,
 * so a status is never ambiguous between fixtures. 127 and 128 are skipped: no
 * shell is anywhere in the launch path, but those two carry shell meanings
 * ("command not found", "invalid exit argument") and a reader should not have
 * to rule that out.
 *
 *   120  wrong argument count, or an unknown case name
 *   121  raw: create/open for writing failed
 *   122  raw: short or failed write
 *   123  raw: close after writing failed
 *   124  checked/ignored: fopen for writing failed
 *   125  checked/ignored: fprintf wrote short, or set the stream's error flag
 *   126  checked: fflush reported failure   <- the arm that fires under #127
 *   129  checked: fclose reported failure, after a flush that had succeeded
 *
 * Build: clang -arch arm64 -O1 umbra-userspace-stdio.c -o umbra-userspace-stdio
 */

#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* The bytes every case persists.
 *
 * One constant, read by all three cases, is what makes "the same payload in
 * every case" a property of the program rather than a convention. Must match
 * `userspace_run.rs`'s `PAYLOAD`, which is what the harness compares the
 * export against, and must stay below 4096 bytes -- see the header. */
static const char PAYLOAD[] = "umbra-userspace-nfs\n";

/* The positive control: the executable's own open/write/close. */
static int case_raw(const char *path) {
    const size_t n = sizeof PAYLOAD - 1;

    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return 121;
    }
    if (write(fd, PAYLOAD, n) != (ssize_t)n) {
        return 122;
    }
    if (close(fd) < 0) {
        return 123;
    }
    return 0;
}

/* The two buffered cases, which differ in one decision and share every call.
 *
 * `check_close` is what the case names mean. With it, every result is
 * consulted. Without it, `fclose`'s result is thrown away exactly as an
 * ordinary program throws it away -- and nothing else is: the `fopen` and the
 * `fprintf` are checked in both cases, which is the point. A program can be as
 * careful as this one is about its writes and still be told they succeeded. */
static int case_buffered(const char *path, int check_close) {
    const int n = (int)(sizeof PAYLOAD - 1);

    FILE *stream = fopen(path, "w");
    if (stream == NULL) {
        return 124;
    }
    /* "%s" rather than the payload as the format string, so no byte of the
     * payload is ever read as a conversion. */
    if (fprintf(stream, "%s", PAYLOAD) != n || ferror(stream)) {
        fclose(stream);
        return 125;
    }
    if (!check_close) {
        /* The entire `ignored` case, in one line. The result is discarded on
         * purpose, and the cast says so to the compiler and the reader both. */
        (void)fclose(stream);
        return 0;
    }
    /* `fflush` as its own statement rather than leaning on the flush inside
     * `fclose`: measured, it is the call that reports first on a routed
     * descriptor (-1, `errno` EBADF, `ferror` set), and reaching it separately
     * is what lets the exit status name which arm failed. `fclose` would also
     * return -1, but on this path it is never the first to. */
    if (fflush(stream) != 0 || ferror(stream)) {
        fclose(stream);
        return 126;
    }
    if (fclose(stream) != 0) {
        return 129;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 120;
    }
    if (strcmp(argv[1], "raw") == 0) {
        return case_raw(argv[2]);
    }
    if (strcmp(argv[1], "checked") == 0) {
        return case_buffered(argv[2], 1);
    }
    if (strcmp(argv[1], "ignored") == 0) {
        return case_buffered(argv[2], 0);
    }
    return 120;
}
