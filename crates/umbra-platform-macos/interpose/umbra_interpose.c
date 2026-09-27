/*
 * umbra's userspace-routing interposer.
 *
 * WHAT THIS IS
 * ------------
 * A dylib umbra loads into the tracee before its first instruction, via
 * DYLD_INSERT_LIBRARIES on the supervised launch. It replaces libc's `open`,
 * `read`, `write` and `close` with functions that hand the operation to umbra,
 * which services it through the overlay and the run's storage provider. It
 * exists so a run can be backed by storage with no kernel-visible path at all --
 * a userspace NFS client -- where rewriting a syscall's path operand has nothing
 * to rewrite the operand to.
 *
 * It is not a security boundary and does not claim to be one. See LIMITS.
 *
 * HOW A REQUEST REACHES UMBRA
 * ---------------------------
 * One instruction: `svc #0x80` with UMBRA_TRAP_NUMBER in x16. umbra plants a
 * breakpoint on that instruction -- and only on instructions inside this image,
 * because that is the only extra image it scans for them -- so the trap is
 * honoured at these call sites and nowhere else. Arguments go in x0..x3 exactly
 * as a syscall's would, and umbra answers the way the kernel would: x0 carries
 * the result, and the carry flag distinguishes an errno from a return value.
 * Bytes for a read are written straight into the caller's buffer by umbra, while
 * the thread is stopped at the `svc`, before it steps past it.
 *
 * There is deliberately no socket, no shared memory and no file. A descriptor
 * would have to be kept out of the tracee's own descriptor space, hidden from
 * this file's own `close`, and reasoned about across fork and exec; a shared
 * region would have to be found by umbra; and either one would need a thread in
 * the supervisor to service it, because the supervisor's event loop is otherwise
 * blocked waiting for the very tracee that is waiting for it. One trap
 * instruction has none of those problems, costs no descriptor, and is already
 * exactly what the tracer knows how to intercept -- the request arrives as an
 * ordinary syscall entry and is answered by the ordinary resolve / prepare /
 * emulate / observe / commit sequence, on the thread that owns the namespace.
 *
 * ARMING, AND WHY THIS LIBRARY CANNOT ARM ITSELF
 * ----------------------------------------------
 * Nothing here decides to route. `umbra_control.magic` starts zero and is
 * written by **umbra**, into this library's `__DATA,__umbra_arm` section,
 * through the debugger port, while the tracee is stopped -- and only *after*
 * umbra has planted breakpoints on the `svc` sites below.
 *
 * That order is the whole point, and an earlier version got it wrong. umbra can
 * only learn this library's load address once dyld has mapped it, which means
 * running the tracee to its entry point, which means every library initializer
 * has already run. A version of this file that armed itself from its own
 * constructor was therefore live, with no breakpoint on its trap, for the entire
 * initializer window: any program with a file-touching `__attribute__
 * ((constructor))`, ObjC `+load`, or its own inserted dylib turned its first
 * `open` into an `svc` the kernel does not know, took SIGSYS, and died before
 * `main` with an undecoded debugger packet for a diagnosis.
 *
 * So the rule is mechanical rather than aspirational: **this library routes from
 * the instruction that arms it until the library is disarmed or the process
 * exits.** Before arming -- dyld's own image loading and every library
 * initializer -- every call here passes straight through to libc and reaches the
 * host, covered by the Seatbelt profile umbra installed before the target's
 * first instruction and by nothing else. That window is strictly larger than a
 * rewrite-backed run's, because such a run plants its breakpoints before any
 * initializer runs and a routed one cannot; see `Session::wait_for_image`.
 *
 * "or disarmed" is not a hedge. A tracee can find this section through
 * `getsectiondata` and zero it, which ends the window early. Measured, the
 * consequence is fail-closed and self-inflicted: after disarming, a `read` on a
 * virtual descriptor it already holds gets EBADF from the kernel -- the number is
 * not a kernel object -- and a fresh `open` is still routed, because umbra
 * breakpoints libc's `__open` stub independently of this library. So the tracee
 * loses its own descriptors and gains nothing. It is recorded because the
 * sentence used to say "and process exit", which was simply not true.
 *
 * Do not reintroduce a constructor that sets `umbra_control.magic`, and do not
 * take the value from the environment: arming has to happen after breakpoints
 * exist, and only umbra knows when that is.
 *
 * IF UMBRA IS NOT THERE
 * ---------------------
 * Nothing arms the control block, so every interposed function passes through to
 * libc and this library is inert. It is not a fallback layer and it does not
 * become one: without a supervisor there is no routing to fall back *from*.
 *
 * The trap itself, executed anyway (by hand, or by a build that armed the block
 * some other way), is refused by the kernel with **SIGSYS** -- measured. Under a
 * supervisor the same instruction is breakpointed and never reaches the kernel;
 * a supervisor that *resumed* one instead of answering it would get Darwin's
 * `nosys`, which returns ENOSYS and posts SIGSYS, which is why umbra answers
 * every trap it intercepts and never resumes one.
 *
 * DORMANCY
 * --------
 * DYLD_INSERT_LIBRARIES applies to every image in the exec chain, and umbra's
 * supervised launch runs `sandbox-exec` first, which then execs the target. So
 * this library is loaded twice: once into the installer, once into the target.
 * It must route in the target only, and it does, for free: umbra arms only the
 * image it meant to route, so the installer's copy is never armed and its file
 * operations reach libc exactly as they did before.
 *
 * VIRTUAL DESCRIPTORS
 * -------------------
 * umbra allocates the descriptor numbers, not this file, and they are always at
 * or above `umbra_control.floor` -- the RLIMIT_NOFILE ceiling umbra set on the
 * tracee before exec, and written into this library beside the arming magic. The kernel never allocates a number at or above its
 * own RLIMIT_NOFILE, so the two ranges cannot collide for the lifetime of the
 * process or any descendant, and the tracee cannot move the boundary because the
 * hard limit was lowered too. `read`, `write` and `close` route on exactly that
 * test: below the floor is a kernel descriptor and goes to libc unchanged --
 * which is what keeps writes to inherited stdout and stderr working -- and at or
 * above it is umbra's.
 *
 * LIMITS -- read these before assuming an operation is covered
 * ------------------------------------------------------------
 * 1. Only `open`, `read`, `write` and `close` are interposed here, and "not
 *    interposed" is not the same as "not routed". umbra still breakpoints the
 *    libc syscall stubs it always did, so a **path** operation this file does
 *    not replace -- `openat` is the one that matters -- is routed through the
 *    tracer instead, and works. Measured: `openat(AT_FDCWD, ...)` returns a
 *    virtual descriptor and writes through it.
 *
 *    `fstat` used to be named in the refused list below and is not any more.
 *    It is still not interposed here; it is routed through the tracer, on the
 *    `fstat`(339) and `__fstat`(189) libc stubs. That is the distinction the
 *    paragraph above draws, reaching a descriptor-relative call for the first
 *    time rather than a path one -- so the list lost a member, not its meaning.
 *    Why the removal is described instead of simply made: a doc comment still
 *    naming `fstat` as refused would be a false invariant in the one place a
 *    reader has nothing but the comment to check it against.
 *
 *    Routing it needs the descriptor fence *more* than interposing it would,
 *    not less: every `fstat` in the process reaches a breakpoint, including
 *    libsystem's own on kernel descriptors, so the supervisor applies the same
 *    floor test this file applies to `read`/`write`/`close` before the call is
 *    resolved at all. Below the floor it passes through to the kernel
 *    untouched. Measured: XPC bundle resolution `fstat`s fd 3 during startup
 *    and `cat` `fstat`s fd 1.
 *
 *    What is genuinely refused is the rest of the **descriptor-relative** set:
 *    `lseek`, `dup`, `dup2`, `fcntl`, `ftruncate`, `fsync`, `pread`, `pwrite`,
 *    `readv`, `writev`, `mmap`, the directory-reading calls entry 6 below does
 *    not claim, and `openat` with a virtual dirfd. A virtual descriptor is not
 *    a kernel object, so these reach the kernel, which does not know the
 *    number, and receive EBADF. That is a
 *    refusal, not a wrong answer, and it is why the descriptor fence above is
 *    load-bearing rather than tidy.
 *
 *    The list lost a member, and it also changed **which of the remaining
 *    members a program can reach** -- which is a separate consequence and is
 *    not implied by the first. A program that previously bailed at the refused
 *    `fstat` now gets an answer and proceeds to whatever it calls next, so a
 *    refusal one call further on becomes reachable for the first time.
 *    Measured, on `/bin/cat`: its locale open is routed, so the descriptor is
 *    virtual; `_Read_RuneMagi` `fstat`s it -- previously refused, now answered
 *    -- and then `mmap`s it, which it never used to reach. With that `mmap`
 *    refused EBADF, `cat` still exits 0 with correct bytes and still exits 1
 *    with its own `No such file or directory` on an absent operand, so the
 *    mapping refusal stays fail-closed and the utility does not regress. The
 *    general shape is what matters: routing one member of a refused set moves
 *    the failure point rather than removing it, and the next member has to be
 *    checked rather than assumed unreached.
 * 2. A program that reaches the syscall layer directly -- its own `svc`, or a
 *    libc entry point that is not interposed -- is not routed by this file at
 *    all. Nothing here can prevent that, and nothing here claims to: what stops
 *    such a call writing the host is the kernel-enforced Seatbelt profile umbra
 *    installs before the target runs, never this library. That is unchanged by
 *    the calls the tracer routes -- `mkdir`(136), `fstat`(339)/`__fstat`(189),
 *    `setattrlistat`(524) (which is how `utimensat` reaches the kernel), and
 *    the four directory-read calls of entry 6 below, one of which (`close`) this
 *    file also interposes. Each is breakpointed at its libc stub, so a program that
 *    issues the `svc` itself bypasses every one of them exactly as it bypasses
 *    `open`, and Seatbelt is what refuses it.
 * 3. Writable shared file mappings are unreachable in principle. A store to a
 *    resident page is not a call of any kind.
 * 4. Unlinking or renaming a file that is open is not representable: umbra maps
 *    a descriptor to a logical *path*, so an unlinked open object has no name
 *    left to route to.
 * 5. Calls made before umbra arms this library are not routed -- dyld's image
 *    loading and every library initializer. See ARMING above and the note over
 *    `umbra_open`.
 * 6. Directory reads on a virtual descriptor are **routed**, and it takes four
 *    calls: `getattrlistbulk`(461), which is what `ls` actually uses,
 *    `fchdir`(13), and both `close`(6) and `__close_nocancel`(399). All four
 *    are in `TRACED_STUBS`; three of them are *only* there, and `close`(6) is
 *    **both breakpointed and interposed by this file**. That pair is not a
 *    double route and it is the newly interesting fact here: `umbra_close`
 *    traps for a descriptor above the fence and calls the real `close` below
 *    it, and *that* call lands on the breakpoint and is passed straight back
 *    out by the supervisor's floor test. The two layers cover different
 *    callers -- this file rebinds the executable's call sites, and the
 *    breakpoint catches the intra-library calls `DYLD_INTERPOSE` cannot reach,
 *    which is how `fts` releases its dirfd (through `__close_nocancel`, which
 *    this file does not interpose at all). A routed `open` of a directory is no
 *    longer refused, and `ls` lists the shadow's merged entries and exits 0 --
 *    measured end to end against a live NFSv4 fixture.
 *
 *    **This entry previously attributed the dirfd `close` refusal to "the
 *    tracer layer", and that named the wrong layer for the wrong symbol.**
 *    `close`(6) was interposed and *only* interposed -- it is one of the four
 *    functions below -- and was never in `TRACED_STUBS` (it is now, which is
 *    what the paragraph above is about); what fts actually closes the dirfd with is
 *    `__close_nocancel`(399), which was neither interposed nor breakpointed.
 *    Both are in `TRACED_STUBS` now, and that is the honest statement of where
 *    they are handled. The rest of that entry described a state master could no
 *    longer reach either: it claimed `ls` lists names through fts's `readdir`
 *    fallback and exits 1 at every errno, dying on SIGTRAP with the dirfd close
 *    refused, when in fact the run *stopped* at the routed directory `open`
 *    long before `getattrlistbulk` was reached.
 *
 *    What stays unclaimed is the rest of the directory surface:
 *    `__getdirentries64`(344) and `getdirentries`(196) are reachable only
 *    through the fallback a *failed* `getattrlistbulk` triggers, which serving
 *    461 means never entering; `opendir`/`readdir`/`closedir` are that
 *    fallback's entry points; and `ls -l` needs `listxattr`(240) plus a wider
 *    `getattrlistbulk` attribute set, which is refused by name because the
 *    bitmap is the reply layout. That refusal is `ENOTSUP` answered **to the
 *    tracee**, and it is evaluated *after* the supervisor's descriptor floor
 *    test, never before: the kernel serves the wider set perfectly well, so a
 *    descriptor umbra does not own must reach it untouched. Refusing earlier
 *    regressed `ls -l` on the rewrite-backed registries from exit 0 to a
 *    stopped run. `ls -l`'s `fstat` on a virtual *dirfd* is now
 *    supplied -- routing `fstat` did not use to reach it, because a routed
 *    `open` of a directory was refused before a dirfd was ever issued, and that
 *    is no longer true -- but `listxattr` is not, so `-l` stays unclaimed.
 * 7. `touch -t` and `-r` take the same path as plain `touch` and are expected
 *    to work, but nothing tests them, so they are claimed as unverified rather
 *    than as supported.
 * 8. `touch` on a file that **already exists** is served on the
 *    `nfs-userspace` registry only. It reaches the kernel as
 *    `setattrlistat`(524), which umbra routes on every registry -- but only
 *    `umbra-storage-nfs-userspace` can apply a timestamp, mapping it onto
 *    NFSv4's `FATTR4_TIME_ACCESS_SET`/`FATTR4_TIME_MODIFY_SET`.
 *    `umbra-storage-local`, `umbra-storage-nfs` and `umbra-storage-tar` refuse
 *    a timestamp update outright, each in its own `check_update`.
 *
 *    So on `--local-dev` and on kernel-`nfs`, `touch <existing>` is refused to
 *    the tracee with `ENOTSUP` and **the run survives** -- measured: exit 1,
 *    `touch: <path>: Operation not supported`, host untouched, nothing
 *    journalled. That is the point of the refusal rather than an accident of
 *    it: the check is `STORAGE_TIMESTAMP_FIDELITY_V1` at `resolve`, before any
 *    journal record exists, because reaching the backend's refusal from inside
 *    `prepare` would flush an intent for a change that never happened and stop
 *    the run. `touch` on a file that does not exist creates it on every
 *    registry; only the existing-file case is conditional.
 *
 *    Recorded here rather than only in the capability's own doc comment
 *    because this is the list a reader consults to learn what does not work,
 *    and a per-registry limitation on the canonical case of a shipped utility
 *    belongs in it. Implementing `utimensat(2)` in the three refusing backends
 *    and advertising the name is what removes this item.
 *
 * Adding an interposed function here is not sufficient to support it. umbra must
 * be able to represent the operation too, and the refusals above stay honest
 * only while both halves agree.
 */

#include <errno.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

/*
 * The reserved trap number, in x16.
 *
 * Far above every Darwin BSD syscall number and not a Mach trap (those are
 * negative), so it cannot be mistaken for a real call in either direction: the
 * kernel refuses it outright (SIGSYS), and umbra's decoder reaches its trap arm
 * only for this exact value. Changing it is a wire-format change shared with
 * `umbra-platform-macos/src/abi.rs`, which carries the same constant and a test
 * asserting the two agree.
 */
#define UMBRA_TRAP_NUMBER 0x554d4252 /* 'UMBR' */

/* Operation codes, in x0. Mirrored by `abi.rs`. */
#define UMBRA_OP_OPEN 1
#define UMBRA_OP_READ 2
#define UMBRA_OP_WRITE 3
#define UMBRA_OP_CLOSE 4

/*
 * What umbra writes to arm this library. See ARMING.
 *
 * It lives in its own section so umbra can find it by name while it is already
 * walking this image's load commands to plant breakpoints, rather than parsing a
 * symbol table. `used` keeps the linker from dropping it; `volatile` keeps the
 * compiler from caching a pre-arming read of `magic` across a call, because the
 * write comes from outside this process's own instruction stream.
 *
 * The layout is a wire format: two little-endian `uint64_t`, `magic` then
 * `floor`, mirrored by `interpose::ARM_MAGIC` and `Session::arm_interposer` in
 * `native.rs`, which writes both in one 16-byte poke. Adding a field means
 * changing both sides.
 */
struct umbra_control {
    volatile uint64_t magic;
    volatile uint64_t floor;
};

__attribute__((used)) static struct umbra_control umbra_control
    __attribute__((section("__DATA,__umbra_arm"))) = {0, 0};

/*
 * Written by umbra, never here. `'UMBRARM1'` big-endian as a 64-bit literal, so
 * a partially-written or zeroed block cannot read as armed.
 */
#define UMBRA_ARM_MAGIC 0x554d425241524d31ULL

/* Whether umbra has armed this library. See ARMING. */
static int umbra_active(void) { return umbra_control.magic == UMBRA_ARM_MAGIC; }

/*
 * Calling the real libc function: by name, deliberately.
 *
 * dyld does not apply an interpose table to calls made from the interposing
 * image itself. Measured on this host: a replacement that calls `open` by name
 * creates the file once per call, with no recursion.
 *
 * `dlsym(RTLD_NEXT, ...)` was tried first, on the reasoning that it states the
 * intent rather than relying on dyld's policy. It **deadlocks**, and why is
 * worth recording. An interposed function here can run *before* this library's
 * constructor: dyld calls `open` while loading images, and those calls already
 * reach these replacements. The resolution therefore had to happen lazily inside
 * the wrapper -- which puts a `dlsym` call inside an `open` call dyld is making
 * while it holds its own loader lock, and the process hangs before reaching
 * `main`. Do not reintroduce it.
 */
extern int open(const char *, int, ...);
extern ssize_t read(int, void *, size_t);
extern ssize_t write(int, const void *, size_t);
extern int close(int);

/*
 * Hand one operation to umbra.
 *
 * `failed` receives the carry flag: umbra sets it exactly when x0 holds an errno
 * rather than a result, which is the convention a Darwin arm64 syscall return
 * already uses -- so `abi::emulate_result` needs no special case for this trap.
 * Unsupervised, control does not come back here at all: see IF UMBRA IS NOT
 * THERE.
 *
 * "memory" is in the clobber list because umbra writes the caller's read buffer
 * while the thread is stopped at this instruction; without it the compiler is
 * free to keep a stale copy of those bytes.
 */
static long umbra_trap(long op, long a1, long a2, long a3, int *failed) {
    register long x16 __asm__("x16") = UMBRA_TRAP_NUMBER;
    register long x0 __asm__("x0") = op;
    register long x1 __asm__("x1") = a1;
    register long x2 __asm__("x2") = a2;
    register long x3 __asm__("x3") = a3;
    register long carry __asm__("x4");
    __asm__ __volatile__("svc #0x80\n\t"
                         "cset x4, cs"
                         : "+r"(x0), "=r"(carry)
                         : "r"(x16), "r"(x1), "r"(x2), "r"(x3)
                         : "cc", "memory");
    *failed = (int)carry;
    return x0;
}

/* Translate a trap answer into the POSIX return convention. */
static long umbra_finish(long value, int failed) {
    if (failed) {
        errno = (int)value;
        return -1;
    }
    return value;
}

/*
 * Whether this descriptor is one umbra issued. See VIRTUAL DESCRIPTORS.
 *
 * **This predicate has a twin in Rust and they must agree.** `fstat` is routed
 * through the tracer rather than through this file -- libsystem calls its own
 * entry point, so there is nothing here to interpose -- and every `fstat` in
 * the process therefore reaches a breakpoint, including libsystem's own on
 * kernel descriptors. The supervisor applies this same test before resolving
 * one, in `Supervisor::syscall_entry`, against the same constant: umbra's
 * `descriptor_floor` is `DESCRIPTOR_FENCE`, which is also the `RLIMIT_NOFILE`
 * written into `umbra_control.floor` below. So the *value* cannot drift -- there
 * is one constant and one assignment in the whole tree -- but the *comparison*
 * is written twice, in two languages.
 *
 * `abi.rs`'s `the_interposers_descriptor_test_is_the_one_the_supervisor_applies`
 * pins this function's text so a change here fails a test that names the Rust
 * twin. Change one, change both.
 */
static int umbra_owns(int fd) {
    return umbra_active() && fd >= 0 &&
           (uint64_t)fd >= umbra_control.floor;
}

/*
 * `open` routes unconditionally when active, for every path.
 *
 * Not "paths that look like they belong to the run": the overlay *is* the
 * tracee's namespace, so which layer answers a path -- the run's shadow or the
 * read-only host base -- is a question only umbra can answer, and answering it
 * here would be a second resolver free to disagree with the real one. A path the
 * run's shadow does not cover is still resolved by umbra, which reads it from the
 * base and hands the bytes back.
 *
 * The exception is timing rather than policy: an `open` that runs before umbra
 * arms this library passes through. Those are dyld's own image loads and the
 * program's library initializers, which reached the host before this library
 * existed and still do, under the same Seatbelt profile. See ARMING.
 */
static int umbra_open(const char *path, int flags, ...) {
    mode_t mode = 0;
    if (flags & 0x0200 /* O_CREAT */) {
        va_list args;
        va_start(args, flags);
        mode = (mode_t)va_arg(args, int);
        va_end(args);
    }
    if (!umbra_active()) {
        return open(path, flags, mode);
    }
    int failed = 0;
    long value =
        umbra_trap(UMBRA_OP_OPEN, (long)path, (long)flags, (long)mode, &failed);
    return (int)umbra_finish(value, failed);
}

static ssize_t umbra_read(int fd, void *buffer, size_t count) {
    if (!umbra_owns(fd)) {
        return read(fd, buffer, count);
    }
    int failed = 0;
    long value =
        umbra_trap(UMBRA_OP_READ, (long)fd, (long)buffer, (long)count, &failed);
    return (ssize_t)umbra_finish(value, failed);
}

static ssize_t umbra_write(int fd, const void *buffer, size_t count) {
    if (!umbra_owns(fd)) {
        return write(fd, buffer, count);
    }
    int failed = 0;
    long value =
        umbra_trap(UMBRA_OP_WRITE, (long)fd, (long)buffer, (long)count, &failed);
    return (ssize_t)umbra_finish(value, failed);
}

static int umbra_close(int fd) {
    if (!umbra_owns(fd)) {
        return close(fd);
    }
    int failed = 0;
    long value = umbra_trap(UMBRA_OP_CLOSE, (long)fd, 0, 0, &failed);
    return (int)umbra_finish(value, failed);
}

struct umbra_interpose_entry {
    const void *replacement;
    const void *original;
};

__attribute__((used)) static struct umbra_interpose_entry umbra_interposed[]
    __attribute__((section("__DATA,__interpose"))) = {
        {(const void *)umbra_open, (const void *)open},
        {(const void *)umbra_read, (const void *)read},
        {(const void *)umbra_write, (const void *)write},
        {(const void *)umbra_close, (const void *)close},
};
