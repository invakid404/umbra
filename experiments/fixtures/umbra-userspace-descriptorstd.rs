// The descriptor fixture's Rust half: four `std::fs::File` methods issued on a
// routed descriptor, plus the one `std` call that is answered on it.
//
// WHY A PAIR
// ----------
// `umbra-userspace-stdio.c` + `umbra-userspace-buffered.rs` and then
// `umbra-userspace-append.c` + `umbra-userspace-appendstd.rs` are the
// precedent, and the construction here is identical: one operation, two call
// paths, and the comparison is the measurement. Read this file with the C one;
// its header carries the shared argument -- what the cases are, why the served
// member is the most important case in the file, why every case has a fence
// guard, why there are two sentinels rather than one, and what each exit code
// means. Only what is specific to Rust is repeated below.
//
// WHAT THE SECOND CALL PATH IS FOR, AND WHY IT PAID OFF HERE
// -----------------------------------------------------------
// `umbra-userspace-buffered.rs` exists because "the two languages do the same
// thing here" was measured once in this tree and came out **false**: C stdio's
// final write escapes routing through `__write_nocancel`(397) while Rust's
// identical-looking buffered write does not. For the append pair the same
// question came out true -- both halves set `O_APPEND` in the flag word they
// hand to `open`.
//
// For this slice it comes out **false again, and in a new way**: two of the
// four shared shapes do not present the same syscall at all.
//
//   seek       `File::seek(SeekFrom::End(0))` -> `lseek`(199). Same syscall as
//              the C half's `lseek` case. The pair agrees here.
//   try_clone  `File::try_clone()` -> `fcntl`(92) with command **67**,
//              `F_DUPFD_CLOEXEC` -- **not** the `F_DUPFD` command 0 the C
//              half's `dupfd` case issues. std sets the close-on-exec variant
//              unconditionally. Measured, and the reason this case is not a
//              duplicate of `dupfd`.
//              A bare `dup`(41) is a *third* syscall again, and is deliberately
//              not a case here: it is in neither language's `std` path, so
//              adding it would be scope rather than coverage.
//   set_len    `File::set_len(2)` -> `ftruncate`(201). Same syscall as the C
//              half's `ftruncate`. The pair agrees here.
//   sync_all   `File::sync_all()` -> `fcntl`(92) with command **51**,
//              `F_FULLFSYNC` -- **not** `fsync`(95), which the C half's `fsync`
//              case issues. Measured. `File::sync_data` was measured to issue
//              the identical call on this SDK, so **Rust has no `fsync`(2) call
//              site for a `File` at all** and there is no second case to add:
//              `sync_data` would be a byte-identical measurement under a
//              different name.
//   metadata   `File::metadata()` -> `fstat`(339). **The served-member
//              control**, same as the C half's `fstat`, and it SUCCEEDS on the
//              very descriptor the four are refused on.
//
// Both halves are controlled, not just one, for the reason the append pair
// controls both: whether `std` reaches the same decision point is the
// measurement, and here it demonstrably does not for `try_clone` and
// `sync_all`. A C-only fixture would have reported the wrong call for half the
// Rust surface, and a Rust-only one would have missed `fsync`(95) and
// `pread`/`pwrite` entirely -- `std` has no positional-I/O method on `File` on
// this platform without `FileExt`, which is why those two cases live in the C
// half alone.
//
// THE FENCE GUARD THROUGH `std`
// ------------------------------
// `AsRawFd` is the only part of this file that reaches below `std::fs`, and it
// reads a number rather than issuing a call. It is what makes the case a
// statement about a *routed* descriptor: see the C half's "THE FENCE GUARD".
//
// WHAT THIS HALF CANNOT CHECK, AND WHY THAT IS LEFT AS IS
// --------------------------------------------------------
// **147 is unreachable from this half.** It is the one exit code of the nine
// that this file cannot produce: `std` exposes no `fcntl`, so an unrelated
// kernel descriptor's flags cannot be read before and after the refusal. The
// alternatives were declined rather than overlooked -- a `libc` dependency,
// which no fixture here has, and a hand-declared `extern "C"` block, which
// would be the first `unsafe` in any Rust fixture in this tree. The claim is
// measured by the C half on the same descriptor in the same shape, and the
// code's *meaning* stays shared across the pair even where its reachability
// does not. `umbra-userspace-appendstd.rs` already has exactly this shape: the
// truncation arm of its 132 is reachable from the C half alone, because
// `OsString` grows. 148, the other half of the boundedness invariant, is
// reachable here and is checked.
//
// NO DIAGNOSTIC OUTPUT, DELIBERATELY, and no `unwrap`/`expect`/`assert`
// anywhere below. `umbra-userspace-toy.c:21-27`'s reason: a traced tracee
// inherits the platform provider's closed standard output, so nothing printed
// here is readable by any harness, and a panic's message is the one thing this
// program must not produce. The verdict is the exit status -- and unlike the
// append pair's refused cases, it is this program's own, because the refusal is
// answered to the tracee and the run survives it.
//
// EXIT CODES
// ----------
// Code-for-code identical with `umbra-userspace-descriptor.c`'s, which is the
// point: a status means the same thing whichever half of the pair produced it.
// Kept clear of `umbra-userspace-edges.c`'s 70-87, this file's sibling
// `umbra-userspace-rustio.rs`'s 90-107, `umbra-userspace-buffered.rs`'s
// 110-114, `umbra-userspace-stdio.c`'s 120-126 and 129 and the append pair's
// 130-134. 140 is the first free decade above the allocated range and 149 is
// left free; the C half's header gives the full argument and the per-code
// meanings, and this file adds nothing to them.
//
// WHEN THIS GOES RED. Deliberately, and one case at a time -- see the C half's
// header. On the day a primitive is admitted its case exits 141 and its harness
// assertion fails on `child_exit() == 9`. Each test's doc comment in
// `userspace_run.rs` says what positive invariant replaces the
// characterization: shared offsets for `try_clone`'s alias, exact truncation
// for `set_len`, an honest result for `sync_all`. `metadata` is the exception
// and inverts -- anything but 141 there is a regression.
//
// This file is not a workspace member -- `Cargo.toml` lists its members under
// `crates/*` -- so `cargo fmt --check` and `cargo clippy` never see it and it
// is hand-formatted. `umbra-userspace-rustio.rs`,
// `umbra-userspace-buffered.rs` and `umbra-userspace-appendstd.rs` say the same
// of themselves.
//
// Build: rustc --edition 2021 -O umbra-userspace-descriptorstd.rs -o umbra-userspace-descriptorstd

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// `EBADF`, spelled out because `std` does not export errno constants and this
/// file takes no dependency that would.
///
/// The C half reaches the same value through `<errno.h>`. 9 on every Darwin and
/// Linux ABI, and the only errno these cases were measured to produce.
const EBADF: i32 = 9;

/// The fence umbra's descriptor numbers start at. See the C half.
const FENCE: i32 = 4096;

/// Build `<path><suffix>`.
///
/// The Rust half of the C file's `derive`. No truncation check is needed or
/// possible here -- `OsString` grows -- which is itself the reason the C side
/// has one and this does not.
fn derive(path: &Path, suffix: &str) -> PathBuf {
    let mut derived = path.as_os_str().to_owned();
    derived.push(suffix);
    PathBuf::from(derived)
}

/// One routed create-and-write, used only by the sentinels below.
fn routed_write(path: &Path, bytes: &[u8]) -> i32 {
    let file = match File::create(path) {
        Ok(file) => file,
        Err(_) => return 144,
    };
    let mut file = file;
    if file.write_all(bytes).is_err() {
        return 144;
    }
    // Taken through the `Result` rather than left to `Drop`, which discards it:
    // a sentinel whose close failed has not proven the bytes landed, and these
    // are the two calls in the file whose success the harness depends on.
    if file.flush().is_err() {
        return 144;
    }
    drop(file);
    0
}

/// Write one of the two liveness sentinels, at `<path>.<case><when>`.
///
/// Byte-identical in placement and payload to the C half's, so the harness
/// reads both halves of the pair the same way.
fn sentinel(path: &Path, case: &str, when: &str) -> i32 {
    routed_write(&derive(path, &format!(".{case}{when}")), case.as_bytes())
}

/// Write the post-refusal sentinel and read it straight back, through routing.
///
/// The C half's `survived`, and for its reason: a write that returned zero has
/// not proven its payload landed, which is the distinction
/// `umbra-userspace-stdio.c` exists for.
fn survived(path: &Path, case: &str) -> i32 {
    let failure = sentinel(path, case, "after");
    if failure != 0 {
        return failure;
    }
    let mut file = match File::open(derive(path, &format!(".{case}after"))) {
        Ok(file) => file,
        Err(_) => return 148,
    };
    let mut back = Vec::new();
    if file.read_to_end(&mut back).is_err() {
        return 148;
    }
    if back != case.as_bytes() {
        return 148;
    }
    0
}

/// Report what a primitive that was expected to be refused actually did, and
/// check the reachable half of the boundedness invariant when it was refused.
///
/// Shared by all five cases, because the reporting is the delicate part and
/// must not drift between them: a *successful* primitive is 141 -- the alarm
/// for four of the cases and the expected status for the control -- an errno
/// other than `EBADF` is a disposition change worth its own code, and a failure
/// without an errno must never read as success.
fn report(path: &Path, case: &str, outcome: Option<std::io::Error>) -> i32 {
    let error = match outcome {
        None => return 141,
        Some(error) => error,
    };
    match error.raw_os_error() {
        Some(0) | None => 143,
        Some(errno) if errno != EBADF => 146,
        Some(errno) => {
            let failure = survived(path, case);
            if failure != 0 {
                return failure;
            }
            errno
        }
    }
}

/// Everything the five cases share: the pre-refusal sentinel, the target, and
/// the routed open behind the fence.
///
/// One function rather than five copies, for the C half's reason: a case that
/// differed here by accident would be measuring something other than its
/// primitive, and the slice turns on all five reaching the identical state
/// before the one call that distinguishes them.
///
/// `Err` carries the exit code, so a caller cannot mistake a setup failure for
/// a finding.
fn prepare(destination: &Path, case: &str) -> Result<File, i32> {
    let failure = sentinel(destination, case, "live");
    if failure != 0 {
        return Err(failure);
    }
    let target = match destination.parent() {
        Some(parent) => parent.join("seed.txt"),
        None => return Err(142),
    };
    // "Exists and is not empty" is checked rather than assumed, for the C
    // half's reason: `set_len`'s refusal is unobservable against a zero-length
    // file.
    match std::fs::metadata(&target) {
        Ok(info) if info.len() > 0 => {}
        _ => return Err(142),
    }
    let file = match OpenOptions::new().read(true).write(true).open(&target) {
        Ok(file) => file,
        Err(_) => return Err(142),
    };
    if file.as_raw_fd() < FENCE {
        return Err(145);
    }
    Ok(file)
}

/// One case: prepare, issue EXACTLY ONE primitive, report.
fn run_case(destination: &Path, case: &str) -> i32 {
    let mut file = match prepare(destination, case) {
        Ok(file) => file,
        Err(code) => return code,
    };
    let outcome = match case {
        "seek" => file.seek(SeekFrom::End(0)).map(|_| ()).err(),
        "try_clone" => file.try_clone().map(|_| ()).err(),
        "set_len" => file.set_len(2).err(),
        "sync_all" => file.sync_all().err(),
        "metadata" => file.metadata().map(|_| ()).err(),
        _ => return 140,
    };
    let verdict = report(destination, case, outcome);
    // The refused descriptor is closed last and its result is discarded -- the
    // C half's `(void)close(fd)`, for its reason. `drop` is the only close
    // `std` offers, and it discards the result anyway.
    drop(file);
    verdict
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let case = match arguments.next() {
        Some(case) => case,
        None => return 140,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 140,
    };
    // Nothing may follow the destination. A fixture that ignored extra
    // arguments would silently accept a harness that had started passing it
    // something this file does not understand.
    if arguments.next().is_some() {
        return 140;
    }
    match case.to_str() {
        Some(case @ ("seek" | "try_clone" | "set_len" | "sync_all" | "metadata")) => {
            run_case(&destination, case)
        }
        _ => 140,
    }
}

fn main() {
    // `dispatch` has returned, so every file it opened is closed -- and those
    // closes are themselves routed calls this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
