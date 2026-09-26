/*
 * The userspace-routing end-to-end fixture.
 *
 * create -> write -> close -> reopen -> read -> compare, and nothing else.
 *
 * WHY THIS PROGRAM AND NOT `umbra-test-child`
 * -------------------------------------------
 * It is *self-verifying*. Success is "the tracee exited zero", so the end-to-end
 * assertion does not have to inspect the store out of band to know the read came
 * back. An out-of-band check alone cannot tell a routed operation from an
 * unrouted one; this program's own comparison can, because the bytes it compares
 * are the ones it read.
 *
 * The read-back costs almost nothing and buys two more things: a second `open`,
 * which exercises descriptor allocation and release twice, and a failure mode
 * that is distinguishable from the write side's. The two mutation probes rely on
 * exactly that -- breaking read routing makes the *compare* reject (exit 8), and
 * breaking write routing makes the *read-back* come up short (exit 7) -- so
 * neither probe can pass for the other's reason.
 *
 * NO STDIO, DELIBERATELY
 * ----------------------
 * `fopen`/`fwrite` would pull in `fstat` for buffer sizing, and possibly `mmap`.
 * Neither is routed by umbra's interposer, so using stdio would put operations in
 * the fixture that the slice does not claim and does not need. Measured on the
 * compiled binary: `nm -u` lists exactly `_open`, `_read`, `_write`, `_close`
 * plus the stack-protector symbols, which are not calls of any kind.
 *
 * EXIT CODES
 * ----------
 * Distinct per failure so a run that fails is diagnosable from its status alone,
 * with no debugger and no log. 0 is success; every other value names one step.
 *
 *   2  wrong argument count
 *   3  create/open for writing failed
 *   4  short or failed write
 *   5  close after writing failed
 *   6  reopen for reading failed
 *   7  short or failed read-back        <- mutation probe B lands here
 *   8  the bytes read back differ       <- mutation probe A lands here
 *   9  close after reading failed
 *
 * Build: clang -arch arm64 -O1 umbra-userspace-toy.c -o umbra-userspace-toy
 */

#include <fcntl.h>
#include <stddef.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        return 2;
    }
    static const char msg[] = "umbra-userspace-nfs\n";
    const size_t n = sizeof msg - 1;

    int fd = open(argv[1], O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return 3;
    }
    if (write(fd, msg, n) != (ssize_t)n) {
        return 4;
    }
    if (close(fd) < 0) {
        return 5;
    }

    char buf[64];
    fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        return 6;
    }
    if (read(fd, buf, sizeof buf) != (ssize_t)n) {
        return 7;
    }
    for (size_t i = 0; i < n; i++) {
        if (buf[i] != msg[i]) {
            return 8;
        }
    }
    if (close(fd) < 0) {
        return 9;
    }
    return 0;
}
