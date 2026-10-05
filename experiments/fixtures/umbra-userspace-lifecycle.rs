// The userspace-routing *process-lifecycle* fixture: what a routed run does when
// the tracee forks, when a bare forked child moves its own working directory,
// and when a child `exec`s with an environment it rebuilt from nothing.
//
// Every other fixture in this directory is one process. This one is two, and the
// second one is the measurement: `fork` is where umbra's per-process logical
// state is DUPLICATED rather than shared, and duplication is invisible to any
// case that only ever asks one process a question. The three cases below each
// ask what survives the split, and they disagree with each other on purpose --
// one of them pins a divergence from POSIX and the other two pin invariants that
// hold.
//
// MEASURED DISPOSITION AT THE PIN, and the reason each case exists. Measured
// against live NFS-Ganesha through `umbra-storage-nfs-userspace`, not predicted:
//
//   umbra-userspace-lifecycle sharedofd <path>
//       THE CASE THIS FIXTURE WAS WRITTEN FOR, and the one that PINS A
//       DIVERGENCE rather than verifying an invariant. It is GREEN at this
//       pin, and green is the correct disposition: it asserts the divergent
//       bytes a routed run actually produces and excludes the POSIX ones. It
//       turns RED the day shared-offset routing lands, and that is the day to
//       DELETE it rather than to repair it. One descriptor, three writes, two
//       processes:
//
//         (a) the parent opens `<path>` and writes `seed`
//         (b) the parent forks
//         (c) the child writes `child` on the INHERITED descriptor and exits
//         (d) the parent reads the handshake byte, then reaps
//         (e) the parent writes `parent` on the SAME descriptor
//
//       POSIX requires (a), (c) and (e) to share ONE Open File Description, so
//       the three writes would append in sequence and `<path>` would hold
//       `seedchildparent` -- 15 bytes. MEASURED: it holds `seedparent`, 10
//       bytes. The parent's third write starts at the offset its FIRST write
//       left, as though the child's write never happened, and overwrites the
//       child's bytes in place.
//
//       THE MECHANISM IS IN PRODUCTION SOURCE AND IS NOT A ROUNDING ERROR.
//       `ProcessContext::fds` is a `BTreeMap<TracedFd, FdState>` held BY VALUE
//       (`umbra-core/src/lib.rs`), `FdState::offset` is a plain `u64` with no
//       indirection of any kind, and `track_process` in
//       `umbra-supervisor/src/events.rs` seeds a forked child by CLONING the
//       parent's `ProcessContext` -- `.map(|p| p.context.clone())`. A clone of
//       a `u64` is a copy, so the two processes leave with two offsets where
//       POSIX gives them one. There is no Open File Description object anywhere
//       in the shadow model to share, which is why this is a model-level gap
//       and not a missing `&mut`.
//
//       WHY THE THIRD WRITE IS THE WHOLE POINT. The shipped
//       `a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store`
//       already measures (a) then (c) and asserts `seedchild`, and that
//       sequence CANNOT distinguish the two models: a copied offset and a
//       shared one both place the child's write at 4. Only a write by the
//       PARENT after the child has advanced the position tells them apart, and
//       nothing before this fixture issued one.
//
//       WHY THE DISCRIMINATOR IS BYTES AND NEVER `lseek`. The obvious probe --
//       ask the descriptor where it is -- is unavailable here, and that is a
//       measurement rather than a preference. `lseek`(199) is in NEITHER
//       `abi::TRACED_STUBS` NOR the interposer's replaced set, so it is
//       kernel-bare: it reaches a kernel that has never heard of a descriptor
//       above the `RLIMIT_NOFILE` fence umbra allocates above, and answers
//       `EBADF` -- #165's shape, the kernel's refusal rather than umbra's. A
//       fixture that measured its own offsets would therefore measure nothing.
//       The verdict is the bytes the harness reads back out of the export
//       THROUGH THE NFSv4 CLIENT, which shares no resolution code with the run.
//
//       WHY THE PIPE HANDSHAKE IS HERE WHEN `wait4` WOULD ORDER IT ANYWAY.
//       `wait4` does order (e) after the child's exit, so the handshake is not
//       load-bearing for ordering. It is here because it makes the ordering
//       LOCAL to the write sequence instead of a consequence of process
//       teardown: the parent blocks until the child says its routed write
//       returned, so a measurement of `seedparent` cannot be explained by the
//       child's write having been lost, dropped or still in flight. `pipe`(42)
//       is untraced and kernel-backed -- it is a kernel descriptor below the
//       fence, so the interposer passes it straight to libc -- which is what
//       lets it carry a signal that routing cannot colour.
//
//   umbra-userspace-lifecycle cwdsplit <path>
//       The same duplication, asked of a field where copying is the CORRECT
//       answer -- so this case ships GREEN and is the control that stops the
//       fixture from being a one-sided story about cloning. A BARE fork, no
//       `exec` anywhere: the child `chdir`s into `<path>.d` and writes the
//       RELATIVE name `leaf.txt`, and the parent then writes the SAME relative
//       name from a working directory it never moved.
//
//       MEASURED: `<path>.d/leaf.txt` holds `child` and `leaf.txt` beside the
//       workspace root holds `parent`. Two different objects from one spelling,
//       which is only possible if the child's `chdir` moved the child's logical
//       anchor and left the parent's alone.
//
//       BARE FORK IS THE POINT, and it is what separates this case from the
//       shipped `chdirchild`, whose child `exec`s a helper. An `exec` re-arms
//       the interposer and rebuilds the address space, so a pass there is
//       consistent with the cwd being re-derived at `exec` time rather than
//       cloned at `fork` time. With no `exec` in the sequence, `ProcessContext`
//       cloning is the only mechanism left that can produce the split.
//
//       The negative half is what makes it discriminating: a stale shared
//       anchor would put BOTH writes in one place, so `<path>.d/leaf.txt` would
//       hold `parent` and the workspace-root name would not exist at all. The
//       harness asserts both names, not just the one that is supposed to be
//       there.
//
//   umbra-userspace-lifecycle execenv <path>
//       The PRESERVE ARM of the rebuilt-environment question: a forked child
//       `execve`s a different image with an environment it built from nothing
//       -- a replaced `argv[0]`, an explicit `PATH`, a marker of its own, and
//       the interposer variable deliberately carried across -- and the exec'd
//       image's own routed write still reaches the store.
//
//       MEASURED: `<path>.exec` holds `execenv`, written by the helper after
//       the rewrite, so mediation survives an environment that retains nothing
//       the launch put there except the one variable umbra needs.
//
//       WHAT "THE ONE VARIABLE" IS, MEASURED RATHER THAN ASSUMED. umbra injects
//       EXACTLY ONE variable into a supervised launch -- `DYLD_INSERT_LIBRARIES`
//       -- and `native.rs` says so in as many words ("One variable, and only
//       dyld's"), because the interposer's image and its descriptor floor used
//       to ride in two more and now are written into the library's own `__DATA`
//       through the debugger port instead. So the preserved set is a set of one,
//       and this case rebuilds everything else.
//
//       WHAT THIS CASE DOES NOT MEASURE. It does not strip the variable. The
//       strip arm is a separate slice and is deliberately not built here; what
//       a stripped environment does is already recorded, measured, in
//       `a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image`
//       -- the tracer re-plants every `TRACED_STUBS` breakpoint after an exec
//       unconditionally, so the child's `open` still routes and still returns a
//       VIRTUAL descriptor, while `write` and `close` reach umbra through the
//       interposer and nowhere else and so reach the kernel with a number it
//       does not own, which answers `EBADF`. The object is created and left
//       EMPTY and nothing reaches the host. That is fail-closed, and it is the
//       reason this fixture has no strip case to add.
//
// NO STDIO, DELIBERATELY, for `umbra-userspace-toy.c:21-27`'s reason: the verdict
// travels in the exit status alone, plus the bytes the harness reads back out of
// the export. There is no `println!`, no `eprintln!`, and no
// `unwrap`/`expect`/`assert` anywhere below -- a panic would write a message and
// the message is the thing this fixture must not have. It matters more in a
// forked fixture than in a single-process one: a panic in the CHILD would write
// its message and then unwind through the parent's buffers, so the one stream a
// reader uses to tell umbra's diagnostics from the tracee's would carry two
// processes' output interleaved.
//
// EVERY WRITE IS RAW `write`(2). None of the staged writes goes through Rust's
// buffered I/O, and that is mechanism rather than idiom: a `BufWriter` coalesces
// (a) and (e) into one transfer at drop time, which is precisely the two writes
// whose SEPARATE placement is the measurement. `umbra-userspace-buffered.rs`
// exists to characterize what buffering costs a routed run; this fixture cannot
// afford it. The same reasoning is why the sentinels and the read-backs below go
// through `open`/`write`/`read`/`close` directly instead of `std::fs`.
//
// EVERY CHILD AND GRANDCHILD IS REAPED BEFORE THE PARENT EXITS, deliberately and
// with no exceptions. There is no orphan anywhere in this file, in any case, on
// any failure path: each case's parent `wait4`s the one child it forked before it
// returns, including when it is about to return a failure code. A fixture that
// left a stray child behind would hand the run's reaper a process to collect
// after the tracee it was launched for is gone, and the whole point of a
// lifecycle fixture is to be the well-formed shape rather than the stress case.
//
// TWO SENTINELS PER CASE, because a run that ENDS produces no exit code and
// silence has to be told apart from "nothing ever ran":
//
//   <leaf>.<case>live   written BEFORE the case's sequence starts. Its presence
//                       in the shadow proves this run was routing when it
//                       reached whatever happened next.
//   <leaf>.<case>done   written ONLY after the case's full sequence succeeded.
//                       Measured PRESENT for all three cases at this pin --
//                       none of them ends the run, which is what makes
//                       `sharedofd`'s assertion a statement about BYTES rather
//                       than about a refusal.
//
// Exit codes are this program's own, and the slice 170-178 was verified free by
// stripping comments and string literals from all fourteen sibling fixtures and
// from `userspace_run.rs` and enumerating every integer literal in 10..=255 --
// `toy`/`listing`/`listing-project`/`test-child` hold 0-9, `edges` 70-87,
// `rustio` 90-107, `buffered` 110-114, `stdio` 120-129, `append` pair 130-134,
// `descriptor` pair 140-148, `nofollow` 150-158, `metadata` 160-165. The free
// range in 120..=200 is 127, 128, 135-139, 149, 159 and 166-200. `edges.c` also
// passes raw errnos through, and Darwin's `ELAST` is 107 on this host -- measured
// from `<sys/errno.h>`, not recalled -- so that passthrough cannot reach this
// slice either. Every code names one step:
//
//  170  wrong argument count, an unknown case name, or a <path> with no parent
//       directory to build beside
//  171  setup failed: the `<path>.d` mkdir, the pre-sequence sentinel, the
//       initial routed open, the `pipe`, or -- for `execenv` -- the helper path
//       missing from this process's own environment. NOT the interposer
//       variable: `case_execenv` tolerates its absence by design, because the
//       unrouted host control runs with no inserted library at all, and the
//       harness's byte assertion is what tells the preserve arm from the strip
//       arm. Only `UMBRA_LIFECYCLE_HELPER` has a `None` arm that returns 171.
//  172  stage (a): the parent's first write failed
//  173  stage (b): `fork` failed
//  174  stage (c): the child's work failed -- its write on the inherited
//       descriptor, or its `chdir` and relative write
//  175  stage (d): the handshake read returned no byte, `wait4` failed, it
//       reaped a pid other than the child, or the child did not exit normally
//  176  the write that COMPLETES the case failed: stage (e)'s post-reap write
//       for `sharedofd` and `cwdsplit`, and the exec'd helper's own routed
//       write for `execenv`
//  177  `execve` RETURNED in the rebuilt-environment child, so the exec never
//       took and nothing was measured about the new image
//  178  NON-VACUITY: a stage refused AND an object that the refused stage must
//       not have produced exists anyway
//
// THIS ALLOCATION WAS REVISED FROM THE ONE THE DESIGN GATE'S AUDIT PROPOSED, and
// the note matters because the audit's table is archived and a later slice may
// read it rather than this file. Three differences, all inside the ratified
// 170-178 slice:
//
//   * 176 was "stage (e) the parent's post-reap write failed". Generalized to
//     "the write that completes the case", because `execenv` has no stage (e)
//     of its own -- the write that completes it is issued by the exec'd helper,
//     in a different process and a different image. One meaning covers both,
//     and the alternative was spending a tenth code to say what the argv
//     already says.
//   * 175 also covers the handshake read returning no byte. The audit's table
//     named only `wait4`, written before D2 ratified the handshake back in.
//     Both belong to the same stage -- (d) is where the parent synchronises
//     with a child it is about to reap -- and an EOF on the pipe and a failed
//     `wait4` are the same event seen from two sides.
//   * The helper binary SHARES this slice rather than taking one of its own,
//     following the `append` and `descriptor` pairs: it uses 170 for its own
//     argv contract and 176 for its routed write, and the harness knows which
//     program produced a code because it knows which one it launched.
//
// WHERE THIS FILE AND THE ARCHIVED TABLE DISAGREE, THIS FILE IS AUTHORITATIVE:
// it is the program that produces the codes.
//
// 178 is the one that makes the rest non-vacuous, and it is per-case rather than
// global because "an object a refused stage must not have produced" is a
// different object in each: `<path>` still holding exactly `seed` when (b) or
// (c) refused, `<path>.d/leaf.txt` absent when `cwdsplit`'s child refused, and
// `<path>.exec` absent when `execve` returned. A refusal that mutated the store
// anyway is a far larger defect than the disposition this fixture pins, and no
// check that only read the exit status would catch it. Unreached at this pin,
// and here for the fix.
//
// This file is not a workspace member -- `experiments/` is outside `Cargo.toml`'s
// member list -- so `cargo fmt` and `clippy` never see it and it is hand-
// formatted. Build: `rustc --edition 2021 -O <this file> -o <bin>`. Its exec'd
// half is `umbra-userspace-envwriter.rs`, built separately and named to this
// program through `UMBRA_LIFECYCLE_HELPER`.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

extern "C" {
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buffer: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buffer: *const u8, count: usize) -> isize;
    fn mkdir(path: *const u8, mode: u32) -> i32;
    fn chdir(path: *const u8) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn fork() -> i32;
    fn wait4(pid: i32, status: *mut i32, options: i32, usage: *mut u8) -> i32;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i32;
    fn _exit(code: i32) -> !;
    fn __error() -> *mut i32;
}

/// `errno`, read through libc's own accessor rather than a global.
fn errno() -> i32 {
    unsafe { *__error() }
}

/// Leave the process WITHOUT running a destructor, atexit handler or flush.
///
/// `_exit`(1) rather than `std::process::exit` on every path a forked child
/// takes, and the distinction is load-bearing rather than tidy: the child
/// inherited the parent's buffers along with its address space, so an exit that
/// flushed them would write the parent's pending bytes a second time, from the
/// wrong process, on a descriptor whose offset is the thing being measured.
fn leave(code: i32) -> ! {
    unsafe { _exit(code) }
}

// Flag values measured on this host (`<sys/fcntl.h>`, Darwin arm64) rather than
// recalled. `O_CLOEXEC` is on every descriptor this fixture opens for its own
// bookkeeping and deliberately NOT on the one `sharedofd` hands across the fork:
// that descriptor's inheritance IS the case.
const O_RDONLY: i32 = 0x0000_0000;
const O_WRONLY: i32 = 0x0000_0001;
const O_CREAT: i32 = 0x0000_0200;
const O_TRUNC: i32 = 0x0000_0400;
const O_CLOEXEC: i32 = 0x0100_0000;

/// The mode every object this fixture creates is given.
const MODE: u32 = 0o644;

/// Stage (a): what the parent writes before it forks.
const SEED: &[u8] = b"seed";
/// Stage (c): what the child writes on the descriptor it inherited.
const CHILD: &[u8] = b"child";
/// Stage (e): what the parent writes after it has reaped the child.
const PARENT: &[u8] = b"parent";

/// The relative name both halves of `cwdsplit` write, from two anchors.
const LEAF: &str = "leaf.txt";

/// The environment variable naming the image `execenv`'s child `exec`s.
///
/// Read from this process's own environment BEFORE the rebuild, which is the
/// only time it is readable: the rebuilt environment the child hands `execve`
/// does not carry it.
const HELPER_VARIABLE: &str = "UMBRA_LIFECYCLE_HELPER";

/// The one variable umbra injects into a supervised launch, and so the one the
/// rebuilt environment has to carry across.
///
/// `native.rs` writes exactly this and nothing else ("One variable, and only
/// dyld's"); the interposer's image and descriptor floor are written into its
/// `__DATA` through the debugger port instead of riding in the environment.
const INSERT_VARIABLE: &str = "DYLD_INSERT_LIBRARIES";

/// A NUL-terminated copy of a path, for the `extern "C"` calls above.
fn cstr(path: &Path) -> Vec<u8> {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// A NUL-terminated copy of one path component.
fn cname(name: &str) -> Vec<u8> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// One sentinel beside the destination leaf, named by suffix.
///
/// The harness reads these back out of the export by the same suffix, so the
/// spelling here is load-bearing: `<destination><suffix>`, with no separator of
/// its own beyond the one the suffix carries.
fn sentinel(destination: &Path, suffix: &str) -> PathBuf {
    let mut bytes = destination.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(suffix.as_bytes());
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// Every byte of `bytes` through raw `write`(2), looping over short transfers.
///
/// A short write is not a failure and must not be reported as one: the loop is
/// what keeps "the write failed" meaning the errno rather than the transfer
/// size. Each call is still one raw `write` at the descriptor's current
/// position, which is what the offset measurement needs.
fn write_all(fd: i32, bytes: &[u8]) -> bool {
    let mut written = 0usize;
    while written < bytes.len() {
        let transferred =
            unsafe { write(fd, bytes[written..].as_ptr(), bytes.len() - written) };
        if transferred <= 0 {
            return false;
        }
        written += transferred as usize;
    }
    true
}

/// Create (or truncate) `path` and write `bytes` into it, through raw calls.
fn write_file(path: &Path, bytes: &[u8]) -> bool {
    let name = cstr(path);
    let fd = unsafe { open(name.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, MODE) };
    if fd < 0 {
        return false;
    }
    let wrote = write_all(fd, bytes);
    let closed = unsafe { close(fd) } == 0;
    wrote && closed
}

/// Create (or truncate) the relative name `name` in the caller's working
/// directory and write `bytes` into it.
///
/// Relative on purpose and in both halves of `cwdsplit`: the anchor is the
/// measurement, so the name must carry no directory of its own.
fn write_relative(name: &str, bytes: &[u8]) -> bool {
    let leaf = cname(name);
    let fd = unsafe { open(leaf.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, MODE) };
    if fd < 0 {
        return false;
    }
    let wrote = write_all(fd, bytes);
    let closed = unsafe { close(fd) } == 0;
    wrote && closed
}

/// Read `path` back through raw calls, or `None` if it does not open.
///
/// `None` means "no bytes to compare", never "the object is as it should be":
/// every caller below treats a read error and a byte mismatch differently,
/// because a refused sequence can leave a name that does not resolve and
/// reporting that as a mutation would be a wrong answer that looks like a right
/// one.
fn read_file(path: &Path) -> Option<Vec<u8>> {
    let name = cstr(path);
    let fd = unsafe { open(name.as_ptr(), O_RDONLY | O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let transferred = unsafe { read(fd, chunk.as_mut_ptr(), chunk.len()) };
        if transferred < 0 {
            unsafe { close(fd) };
            return None;
        }
        if transferred == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..transferred as usize]);
    }
    unsafe { close(fd) };
    Some(bytes)
}

/// Whether `path` resolves at all, which is what 178 asks about two of the three
/// cases.
fn present(path: &Path) -> bool {
    read_file(path).is_some()
}

/// The exit code a reaped child reported, or `None` if it did not exit normally.
///
/// Hand-decoded rather than taken from a crate: `WIFEXITED` is `(status &
/// 0x7f) == 0` and `WEXITSTATUS` is the next byte up, and this fixture declares
/// its calls rather than linking `libc`.
fn exited(status: i32) -> Option<i32> {
    if status & 0x7f == 0 {
        Some((status >> 8) & 0xff)
    } else {
        None
    }
}

/// Reap exactly `child` and report what it said.
///
/// Called on EVERY path out of a case that forked, success or failure, which is
/// the no-orphans rule the header states. `options` is 0 -- a blocking wait --
/// because a fixture that polled could return while its child was still alive.
fn reap(child: i32) -> Result<i32, i32> {
    let mut status: i32 = 0;
    let reaped = unsafe { wait4(child, &mut status, 0, std::ptr::null_mut()) };
    if reaped != child {
        return Err(175);
    }
    match exited(status) {
        Some(code) => Ok(code),
        None => Err(175),
    }
}

/// Case (I): one descriptor, three writes, two processes.
///
/// The parent's stage (e) write is the measurement and the bytes the harness
/// reads back are the verdict; see THE CASE THIS FIXTURE WAS WRITTEN FOR above.
fn case_sharedofd(destination: &Path) -> i32 {
    if !write_file(&sentinel(destination, ".sharedofdlive"), b"sharedofd") {
        return 171;
    }

    // NO `O_CLOEXEC`: this is the descriptor the fork hands across, and a
    // close-on-exec flag would be the wrong statement about it even though
    // nothing here execs.
    let name = cstr(destination);
    let fd = unsafe { open(name.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, MODE) };
    if fd < 0 {
        return 171;
    }

    // The handshake pipe. `pipe`(42) is untraced and below the descriptor
    // fence, so these two numbers are the kernel's and carry a signal routing
    // cannot colour.
    let mut channel = [-1i32; 2];
    if unsafe { pipe(channel.as_mut_ptr()) } != 0 {
        unsafe { close(fd) };
        return 171;
    }

    // Stage (a).
    if !write_all(fd, SEED) {
        unsafe { close(fd) };
        unsafe { close(channel[0]) };
        unsafe { close(channel[1]) };
        return 172;
    }

    // Stage (b).
    let child = unsafe { fork() };
    if child < 0 {
        unsafe { close(fd) };
        unsafe { close(channel[0]) };
        unsafe { close(channel[1]) };
        return if read_file(destination).as_deref() == Some(SEED) {
            173
        } else {
            178
        };
    }

    if child == 0 {
        // Stage (c), in the child. The read end is not ours; closing it keeps
        // the parent's EOF meaningful if this process dies before signalling.
        unsafe { close(channel[0]) };
        if !write_all(fd, CHILD) {
            leave(174);
        }
        // The signal is sent only AFTER the routed write returned, which is the
        // whole value of the handshake: the parent cannot proceed on a write
        // that is still in flight.
        let token = [1u8; 1];
        if !write_all(channel[1], &token) {
            leave(174);
        }
        unsafe { close(channel[1]) };
        leave(0);
    }

    // Stage (d), in the parent. The write end must go first or the read below
    // would never see EOF when the child dies without signalling.
    unsafe { close(channel[1]) };
    let mut token = [0u8; 1];
    let signalled = unsafe { read(channel[0], token.as_mut_ptr(), token.len()) } == 1;
    unsafe { close(channel[0]) };

    // Reaped on every path, signalled or not: the child exists and this process
    // is not allowed to leave it.
    let reaped = reap(child);
    if !signalled {
        unsafe { close(fd) };
        return 175;
    }
    match reaped {
        Ok(0) => {}
        Ok(code) => {
            unsafe { close(fd) };
            // The child's own code travels out unchanged when it is one of
            // ours, so the stage that failed is named by the status rather than
            // flattened into "the child failed".
            return if code == 174 && read_file(destination).as_deref() != Some(SEED) {
                178
            } else {
                code
            };
        }
        Err(code) => {
            unsafe { close(fd) };
            return code;
        }
    }

    // Stage (e). The measurement: this write goes at whatever offset THIS
    // process's descriptor state holds, which is the question.
    if !write_all(fd, PARENT) {
        unsafe { close(fd) };
        return 176;
    }
    if unsafe { close(fd) } != 0 {
        return 176;
    }

    if !write_file(&sentinel(destination, ".sharedofddone"), b"sharedofd") {
        return 171;
    }
    0
}

/// Case (V): a bare forked child moves its own working directory.
///
/// No `exec` anywhere, which is what isolates `ProcessContext` cloning from
/// `exec` re-arming; see BARE FORK IS THE POINT above.
fn case_cwdsplit(destination: &Path) -> i32 {
    if !write_file(&sentinel(destination, ".cwdsplitlive"), b"cwdsplit") {
        return 171;
    }
    let moved = sentinel(destination, ".d");
    if unsafe { mkdir(cstr(&moved).as_ptr(), 0o755) } != 0 {
        return 171;
    }
    let inner = moved.join(LEAF);

    // Stage (b): the bare fork.
    let child = unsafe { fork() };
    if child < 0 {
        return if present(&inner) { 178 } else { 173 };
    }

    if child == 0 {
        // Stage (c), in the child: move, then write the RELATIVE name.
        if unsafe { chdir(cstr(&moved).as_ptr()) } != 0 {
            leave(174);
        }
        if !write_relative(LEAF, CHILD) {
            leave(174);
        }
        leave(0);
    }

    // Stage (d).
    match reap(child) {
        Ok(0) => {}
        Ok(code) => {
            return if code == 174 && present(&inner) { 178 } else { code };
        }
        Err(code) => return code,
    }

    // Stage (e): the same relative spelling, from a working directory this
    // process never moved. Where it lands is the measurement, and the harness
    // reads both candidate names.
    if !write_relative(LEAF, PARENT) {
        return 176;
    }

    if !write_file(&sentinel(destination, ".cwdsplitdone"), b"cwdsplit") {
        return 171;
    }
    0
}

/// Case (VI): a forked child `exec`s with an environment it rebuilt from
/// nothing, keeping only the variable mediation needs.
///
/// The preserve arm, and the only arm; see WHAT THIS CASE DOES NOT MEASURE.
fn case_execenv(destination: &Path) -> i32 {
    if !write_file(&sentinel(destination, ".execenvlive"), b"execenv") {
        return 171;
    }
    // Both of these are read HERE, in the parent, before any rebuild: the
    // rebuilt environment the child hands `execve` carries neither the helper
    // name nor anything else the launch put there.
    let helper = match std::env::var_os(HELPER_VARIABLE) {
        Some(helper) => PathBuf::from(helper),
        None => return 171,
    };
    // PRESERVED IF PRESENT, and absent without complaint if it is not -- which
    // is the host control's case, where nothing inserted a library at all.
    // Requiring it here would fail the unrouted control for a reason that says
    // nothing about umbra, and it would buy no safety: if the variable were
    // missing in a ROUTED run the rebuild would preserve nothing, the exec'd
    // image would carry no interposer, and its `write` would reach the kernel
    // with a virtual descriptor number and be refused `EBADF` -- leaving
    // `<path>.exec` created and EMPTY. So the harness's byte assertion already
    // tells the preserve arm from the strip arm, and it does it on evidence
    // rather than on a precondition this process asserted about itself.
    let inserted = std::env::var_os(INSERT_VARIABLE);
    let produced = sentinel(destination, ".exec");

    let child = unsafe { fork() };
    if child < 0 {
        return if present(&produced) { 178 } else { 173 };
    }

    if child == 0 {
        // The rebuild. `argv[0]` is replaced with a name of this fixture's own
        // choosing rather than the image's path, every inherited variable is
        // dropped, `PATH` is stated explicitly, a marker records that the
        // rebuild happened, and the interposer variable is carried across
        // because mediation of this image's `write` depends on it.
        let argv0 = cname("umbra-lifecycle-envwriter");
        let case = cname("envwriter");
        let target = cstr(destination);
        let argv: [*const u8; 4] = [
            argv0.as_ptr(),
            case.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
        ];

        // The rebuilt environment, held as owned bytes so the pointers below
        // stay valid until `execve` consumes them.
        let mut entries: Vec<Vec<u8>> = vec![
            cname("PATH=/usr/bin:/bin"),
            cname("UMBRA_LIFECYCLE_REBUILT=1"),
        ];
        if let Some(value) = inserted.as_ref() {
            let mut insert = INSERT_VARIABLE.as_bytes().to_vec();
            insert.push(b'=');
            insert.extend_from_slice(value.as_bytes());
            insert.push(0);
            entries.push(insert);
        }
        let mut envp: Vec<*const u8> = entries.iter().map(|entry| entry.as_ptr()).collect();
        envp.push(std::ptr::null());

        let image = cstr(&helper);
        unsafe { execve(image.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        // `execve` returned, so the new image never ran and nothing was
        // measured about it. The errno is deliberately not reported: this is a
        // setup failure of the case rather than a disposition of umbra, and
        // 177 says exactly that much.
        let _ = errno();
        if present(&produced) {
            leave(178);
        }
        leave(177);
    }

    match reap(child) {
        Ok(0) => {}
        Ok(code) => return code,
        Err(code) => return code,
    }

    if !write_file(&sentinel(destination, ".execenvdone"), b"execenv") {
        return 171;
    }
    0
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let case = match arguments.next() {
        Some(case) => case,
        None => return 170,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 170,
    };
    if arguments.next().is_some() {
        return 170;
    }
    if destination.parent().is_none() {
        return 170;
    }
    match case.to_string_lossy().into_owned().as_str() {
        "sharedofd" => case_sharedofd(&destination),
        "cwdsplit" => case_cwdsplit(&destination),
        "execenv" => case_execenv(&destination),
        _ => 170,
    }
}

fn main() {
    // `dispatch` has returned, so every descriptor it opened is closed -- which
    // matters, because each close is itself a routed call this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
