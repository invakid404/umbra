// Read a directory the way `/bin/ls` reads one, and write the names somewhere
// the test can read them back.
//
// **Why this exists rather than asserting on `/bin/ls`'s own output.** A traced
// tracee inherits the *platform provider's* standard output, and the provider is
// spawned with `Stdio::null()` (`umbra-core/src/provider/transport.rs`), so
// nothing any traced program prints can reach a test harness. `ls` itself is
// therefore observable only by its exit status -- and an exit status cannot tell
// a correct listing from an empty one, which is the whole reason the design gate
// ratified asserting on entries instead.
//
// So this writes the names it read into a file *inside the routed workspace*,
// where the test reads them back through the userspace NFSv4 client with nothing
// mounted. That is the "names read back through the client" the gate asked for.
//
// **It uses `fts`, deliberately, and that is the point of the fixture.** `ls`
// issues no directory syscall of its own: every one comes from `fts` inside
// `libsystem_c.dylib`, which is why interposing cannot reach them and why umbra
// has to breakpoint the libc stubs. Calling `getattrlistbulk` directly here
// would test umbra's encoder against umbra's own idea of the format. Going
// through `fts` tests it against Apple's consumer of that format -- the same
// code, in the same library, that `/bin/ls` runs.
//
// `FTS_NOCHDIR` is deliberately *not* set: `fts` then uses its
// `open(".")`/`fchdir` save-and-restore idiom, which is what puts `fchdir`(13)
// on a virtual descriptor and is half of what this proves.
//
// Exit codes are distinct so a failure says which step failed rather than only
// that one did:
//   2  usage
//   3  fts_open failed
//   4  fts_read reported an error on an entry
//   5  the output file could not be created
//   6  a write to the output file was short or failed
//   7  fts_read ended with errno set
//   8  fts_children failed on a directory it could not read -- distinct from
//      the same call returning NULL for an empty directory, which is not an
//      error and which this fixture must not report as one

#include <errno.h>
#include <fcntl.h>
#include <fts.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int emit(int fd, const char *name) {
    char line[1024];
    int n = snprintf(line, sizeof line, "%s\n", name);
    if (n <= 0 || (size_t)n >= sizeof line) {
        return -1;
    }
    ssize_t written = write(fd, line, (size_t)n);
    return written == (ssize_t)n ? 0 : -1;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <directory> <output>\n", argv[0]);
        return 2;
    }
    char *paths[] = {argv[1], NULL};
    // **`FTS_PHYSICAL | FTS_NOSTAT` is exactly what plain `/bin/ls` passes**,
    // and matching it is load-bearing rather than tidy. `FTS_PHYSICAL` is what
    // `ls` uses without `-L`: the final component is not followed, so a listing
    // never silently reports a link's target. `FTS_NOSTAT` is what `ls` sets
    // when it needs no metadata -- no `-l`, no sorting by time -- and it decides
    // *which attributes `fts` asks the kernel for*.
    //
    // Measured: without it, `fts_children` requests
    // `common=0x82079e0b file=0x0000022d` -- creation, modification, change and
    // access times, owner, group, access mask, flags, allocation and data
    // lengths, device type -- where plain `ls` requests
    // `common=0x8200000b file=0x00000001`. umbra serves the narrow set and
    // refuses the wide one by name, because the attribute bitmap *is* the reply
    // layout; the wide set is `ls -l` territory and is not claimed.
    FTS *tree = fts_open(paths, FTS_PHYSICAL | FTS_NOSTAT, NULL);
    if (tree == NULL) {
        fprintf(stderr, "fts_open: %s\n", strerror(errno));
        return 3;
    }
    int out = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (out < 0) {
        fprintf(stderr, "open %s: %s\n", argv[2], strerror(errno));
        return 5;
    }
    FTSENT *entry;
    errno = 0;
    while ((entry = fts_read(tree)) != NULL) {
        if (entry->fts_info == FTS_ERR || entry->fts_info == FTS_DNR) {
            fprintf(stderr, "fts_read %s: %s\n", entry->fts_name,
                    strerror(entry->fts_errno));
            return 4;
        }
        if (entry->fts_info != FTS_D) {
            continue;
        }
        // `FTS_NAMEONLY` for the same reason as `FTS_NOSTAT` above: it is what
        // says only the names are wanted, so `fts` keeps to the narrow request.
        //
        // **`NULL` here means two different things and the difference is the
        // whole point of this fixture.** `fts_children` returns `NULL` both for
        // a directory that is genuinely empty (`errno` untouched) and for a
        // directory it could not read (`errno` set). Treating them alike would
        // let a *failed* directory read produce an empty listing and exit 0 --
        // which is precisely the disposition this fixture exists to detect, and
        // which the slice's own tests cannot tell from a correct empty listing.
        //
        // So `errno` is cleared immediately before the call and read
        // immediately after, before any other libc call can overwrite it. The
        // trailing `fts_read` check at the bottom cannot serve here: a later
        // `fts_read` resets `errno`, so an error raised in this loop would be
        // gone by the time that check runs.
        errno = 0;
        FTSENT *children = fts_children(tree, FTS_NAMEONLY);
        if (children == NULL && errno != 0) {
            fprintf(stderr, "fts_children %s: %s (errno=%d)\n", entry->fts_path,
                    strerror(errno), errno);
            return 8;
        }
        for (FTSENT *child = children; child != NULL; child = child->fts_link) {
            if (emit(out, child->fts_name) != 0) {
                fprintf(stderr, "write %s: %s (errno=%d)\n", argv[2], strerror(errno),
                        errno);
                return 6;
            }
        }
        // One directory, never its subdirectories: `ls -R` is out of scope and
        // descending would need a working-directory model umbra does not claim.
        fts_set(tree, entry, FTS_SKIP);
    }
    if (errno != 0) {
        fprintf(stderr, "fts_read: %s\n", strerror(errno));
        return 7;
    }
    fts_close(tree);
    close(out);
    return 0;
}
