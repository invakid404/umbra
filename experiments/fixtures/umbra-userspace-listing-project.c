// Enumerate a project-shaped directory twice, over one reused descriptor
// number, and write everything it read into a file the test can read back.
//
// **The sibling of `umbra-userspace-listing.c`, not a replacement for it.** That
// one is the control: three flat ASCII names, one enumeration, one
// `getattrlistbulk` reply carrying entries. It stays byte-identical, because it
// is the one thing in this area that must not move. This file is the case the
// control cannot express: a directory whose records do not fit one reply, read
// twice, the second time on the descriptor number the first one gave back.
//
// **What it guards.** `Engine::commit` evicts a closed descriptor's residual
// directory page (`umbra-overlay/src/engine.rs:4033-4041`), and that eviction is
// what makes a second enumeration re-read the directory instead of being served
// the empty remainder the first one left behind. The eviction is live on master
// and **nothing exercised it** -- every shipped fixture reads each directory
// once, which `crates/umbra-storage-nfs-userspace/README.md:688` records as the
// reason an earlier defect in this area went unseen. Two passes is the smallest
// shape that can tell the difference, and this is it.
//
// Everything in the control's header comment applies here unchanged and is not
// repeated: why a fixture writes names into the routed workspace instead of
// asserting on `/bin/ls`'s output, why `fts` is the consumer that matters, and
// why `FTS_NOCHDIR` is deliberately not set. What follows is only what differs.
//
// **Two passes, and the output descriptor is opened before the first of them.**
// This is load-bearing and is the one thing in this fixture that is easy to get
// subtly wrong. umbra allocates a virtual descriptor by scanning upward for the
// lowest number absent from the run's descriptor set
// (`umbra-supervisor/src/events.rs:964-1010`), so closing a directory and
// reopening it reuses the number on its own -- no `dup2`, no `RLIMIT_NOFILE`
// discipline, and `dup2` would not work anyway (`FsOp::Dup` is decoded
// nowhere, so it reaches the kernel and gets `EBADF`). What the reuse *does*
// require is that this program hold the **same** descriptors when each pass
// begins. The control opens its output file after `fts_open`; doing that here
// would leave pass 2 starting from a different free-slot state, the allocator
// would hand `fts` a different number, the residual-page cache key
// `(task, generation, fd, object)` would not collide, and the second pass would
// quietly stop testing eviction while still passing.
//
// So: the output file is opened once, before pass 1, and held across both.
//
// **`probe-fd=<n>` is how the reuse becomes observable.** `fts` keeps its
// directory descriptor private, so this program cannot read it. Immediately
// before each `fts_open` it instead opens the directory itself, records the
// number it was given, and closes it again. The test asserts the two recorded
// numbers are equal. Stated precisely, because the inference matters: that
// proves *the allocator's free-slot state was identical at the start of both
// passes*. Combined with lowest-free allocation, which is a property of the
// allocator's source and not of this measurement, it follows that `fts`
// received the same number both times. It is evidence plus a source-verified
// rule, not a direct observation of `fts`'s internal descriptor.
//
// **Both passes enumerate the same tree, and this fixture writes nothing into
// it.** The discriminator is not a new name appearing in pass 2; it is pass 2
// reporting the tree *at all*. With the eviction in place, the second
// `fts_open` finds no cached page under its key and goes back through
// `Overlay::merged`, so it reports all 239 names. With the eviction suppressed,
// the key collides with what pass 1 left -- a completed enumeration's **empty
// remainder** -- and `resolve_directory` prefers that cache over `merged`, so
// the first reply carries zero entries, which means end-of-directory to `fts`.
// Pass 2 then reports **nothing** and the program still exits 0. 239 against 0
// is as distinguishable as a pair of listings gets.
//
// **Nothing is created in the enumerated directory, deliberately.** An earlier
// draft of this fixture created two files there between the passes, to prove the
// base-plus-shadow merge at the same time. Measured: that write *materialises
// the directory into the shadow*, and a routed `stat` by path on a shadow object
// is refused `ENOTSUP` at `engine.rs:3020-3023` -- a limitation the comment
// above it already defers. `fts` stats a root entry before walking it, so pass 2
// got `FTS_NS` and reported nothing for a reason that had nothing to do with the
// cache. The merge claim therefore belongs to a different arc, and this one
// keeps its output file at the workspace root so the enumerated directory is
// never materialised at all.
//
// **The subdirectories are seeded but never entered.** `fts_set(FTS_SKIP)` is
// kept exactly as the control has it. Recursive traversal is explicitly not
// claimed and not tested by this slice (`README.md:688`), and this file must not
// change that. The two directories exist for their *records*: a directory entry
// omits `ATTR_FILE_LINKCOUNT`, so its record is 4 bytes shorter than a file
// record of the same name length, and that branch of the encoder is otherwise
// unexercised end to end.
//
// **The output is buffered, and that is a measured requirement rather than a
// tidiness.** The control emits one `write` per name, which is fine for three
// names. Measured here, at 239 entries over two passes: a run that issues one
// routed `write` per name dies partway through the second pass with
// `StorageUnavailable during execute.write_at: replay: replay buffer
// exhausted: 64 records`. The in-process replay ledger a live provider binds
// (`crates/umbra-storage-nfs-userspace/src/fake.rs`, `max_records: 64`) admits
// an intent per mutation and applies backpressure before dispatch; records are
// retired explicitly, not as a side effect of settling, so 482 distinct writes
// in one session exhaust the budget. Nothing in the shipped suite had ever
// approached it, because the control makes three.
//
// So lines accumulate in a buffer and are flushed once per pass -- two writes
// instead of 482, and the same bytes in the same order. The flush loops on
// partial writes rather than treating a short one as failure, because a routed
// transfer is allowed to be short (this is the behaviour
// `a_routed_transfer_past_the_backend_bound_is_short_rather_than_fatal`
// pins); the control's `emit` can insist on a whole write only because its
// lines are tiny.
//
// Exit codes extend the control's, same numbers for the same meanings:
//   2  usage
//   3  fts_open failed
//   4  fts_read reported an error on an entry
//   5  the output file could not be created
//   6  a write to the output file was short or failed
//   7  fts_read ended with errno set
//   8  fts_children failed on a directory it could not read -- distinct from
//      the same call returning NULL for an empty directory, which is not an
//      error and which this fixture must not report as one
//   9  the descriptor probe could not open the directory

#include <errno.h>
#include <fcntl.h>
#include <fts.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/// One pass's worth of lines, flushed in a single `write`. Sized for a pass
/// with headroom to spare: 239 entries whose names are at most 100 bytes come to
/// about 24 KiB, well inside this buffer, and a flush happens whenever the next
/// line would not fit -- so a tree that outgrew it would simply cost one extra
/// write rather than failing.
#define BUFFER_BYTES 65536

static char buffer[BUFFER_BYTES];
static size_t buffered;

/// Write the buffer out, looping over partial writes.
static int flush_buffer(int fd) {
    size_t offset = 0;
    while (offset < buffered) {
        ssize_t written = write(fd, buffer + offset, buffered - offset);
        if (written <= 0) {
            return -1;
        }
        offset += (size_t)written;
    }
    buffered = 0;
    return 0;
}

/// Append one `"%s\n"` line, flushing first if it would not fit.
static int emit(int fd, const char *name) {
    char line[1024];
    int n = snprintf(line, sizeof line, "%s\n", name);
    if (n <= 0 || (size_t)n >= sizeof line) {
        return -1;
    }
    if (buffered + (size_t)n > sizeof buffer && flush_buffer(fd) != 0) {
        return -1;
    }
    if (buffered + (size_t)n > sizeof buffer) {
        return -1;
    }
    memcpy(buffer + buffered, line, (size_t)n);
    buffered += (size_t)n;
    return 0;
}

/// Record the descriptor number the directory would be opened on, then give it
/// back. See the header: this is the observable that stands in for `fts`'s own
/// private descriptor.
///
/// The number goes into the buffer, not straight to the descriptor, so the
/// recorded value and the names that follow it stay in one ordered stream. That
/// changes nothing about what is being observed: a buffered append issues no
/// syscall, and the flush writes to a descriptor that was already open, so this
/// program's descriptor holdings are the same either way.
static int emit_probe_fd(int out, const char *dir) {
    int probe = open(dir, O_RDONLY);
    if (probe < 0) {
        fprintf(stderr, "probe open %s: %s\n", dir, strerror(errno));
        return 9;
    }
    char line[64];
    int n = snprintf(line, sizeof line, "probe-fd=%d", probe);
    // Closed before anything else, so the next thing to allocate a descriptor
    // is `fts_open` itself and it is handed the number just recorded.
    close(probe);
    if (n <= 0 || (size_t)n >= sizeof line) {
        return 6;
    }
    if (emit(out, line) != 0) {
        fprintf(stderr, "write %s: %s (errno=%d)\n", dir, strerror(errno), errno);
        return 6;
    }
    return 0;
}

/// One complete enumeration of `dir`, names appended to `out`.
///
/// Byte-for-byte the control's loop, including the `errno`-clearing discipline
/// around `fts_children` and the reason for it: that call returns NULL both for
/// an empty directory and for one it could not read, and treating them alike
/// would let a failed read produce an empty listing and exit 0.
static int enumerate(int out, char *dir) {
    char *paths[] = {dir, NULL};
    // `FTS_PHYSICAL | FTS_NOSTAT` with `FTS_NAMEONLY` below: the flags plain
    // `/bin/ls` passes, which is what decides the attribute set `fts` asks the
    // kernel for (`common=0x8200000b file=0x00000001`, the narrow set umbra
    // serves). The control's header has the measurement.
    FTS *tree = fts_open(paths, FTS_PHYSICAL | FTS_NOSTAT, NULL);
    if (tree == NULL) {
        fprintf(stderr, "fts_open: %s\n", strerror(errno));
        return 3;
    }
    FTSENT *entry;
    errno = 0;
    while ((entry = fts_read(tree)) != NULL) {
        if (entry->fts_info == FTS_ERR || entry->fts_info == FTS_DNR) {
            fprintf(stderr, "fts_read %s: %s\n", entry->fts_name, strerror(entry->fts_errno));
            return 4;
        }
        if (entry->fts_info != FTS_D) {
            continue;
        }
        errno = 0;
        FTSENT *children = fts_children(tree, FTS_NAMEONLY);
        if (children == NULL && errno != 0) {
            fprintf(stderr, "fts_children %s: %s (errno=%d)\n", entry->fts_path, strerror(errno),
                    errno);
            return 8;
        }
        for (FTSENT *child = children; child != NULL; child = child->fts_link) {
            if (emit(out, child->fts_name) != 0) {
                fprintf(stderr, "write: %s (errno=%d)\n", strerror(errno), errno);
                return 6;
            }
        }
        // One directory, never its subdirectories -- see the header.
        fts_set(tree, entry, FTS_SKIP);
    }
    if (errno != 0) {
        fprintf(stderr, "fts_read: %s\n", strerror(errno));
        return 7;
    }
    fts_close(tree);
    // One routed write per pass. See the header: one per *name* exhausts the
    // provider's 64-record replay budget partway through pass 2.
    if (flush_buffer(out) != 0) {
        fprintf(stderr, "flush: %s (errno=%d)\n", strerror(errno), errno);
        return 6;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <directory> <output>\n", argv[0]);
        return 2;
    }
    // **Before pass 1, and held across both.** See the header: equal descriptor
    // holdings at the start of each pass is what makes the number reuse, and
    // the reuse is what makes the cache key collide.
    int out = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (out < 0) {
        fprintf(stderr, "open %s: %s\n", argv[2], strerror(errno));
        return 5;
    }

    int failure = emit_probe_fd(out, argv[1]);
    if (failure != 0) {
        return failure;
    }
    failure = enumerate(out, argv[1]);
    if (failure != 0) {
        return failure;
    }

    // Pass 2, on the descriptor number pass 1 gave back. Nothing has been
    // created, moved or written in between: the only difference between the two
    // passes is that the first one has happened.
    failure = emit_probe_fd(out, argv[1]);
    if (failure != 0) {
        return failure;
    }
    failure = enumerate(out, argv[1]);
    if (failure != 0) {
        return failure;
    }

    close(out);
    return 0;
}
