// The userspace-routing *library-wrapper* fixture: what a routed run does when
// the program reaches the filesystem through `std::fs` rather than through
// `open`/`read`/`write` directly.
//
// `umbra-userspace-toy.c` and `umbra-userspace-edges.c` both issue bare
// syscalls, so every transfer they make is one the fixture itself sized and
// looped. That leaves a hole this file fills. `std::fs::File::create`,
// `Write::write_all`, `Read::read_to_end` and `std::fs::read` do their own
// short-I/O completion *inside the library*, invisibly to the program, and they
// take a capacity hint from a descriptor's metadata before they start. Codex's
// synchronous exec-server core is built out of exactly those calls
// (`codex-rs/exec-server/src/no_follow/unix.rs:105-146`), so whether they are
// routed, whether their internal loops finish, and whether the sizes they read
// back are exact is a property nothing in this suite asserted before.
//
// The case reports through the exit status, so the assertion needs no output
// channel and no debugger:
//
// TWO cases, and they are deliberately two RUNS rather than one. The registry
// `timeout_ms` is a whole-session watchdog (see `BIG`), so every leg sharing a run
// shares one budget with process start and the twin resign. Leg vi is the
// expensive one and it is on its own.
//
//   umbra-userspace-rustio edit <path>
//       Five legs in one run, in this order, because the order is what the
//       harness's mutation-probe discriminators rest on -- a probe that breaks a
//       late leg leaves the earlier objects in the export as positive evidence
//       that the run was alive:
//
//         i.   Read the seeded source `seed.txt` beside <path>: `File::open`,
//              then `File::metadata` on the descriptor, requiring a regular
//              file and a size that agrees with the bytes, then `read_to_end`.
//         ii.  Write a modified version to <path> with `File::create` +
//              `write_all`.
//         iii. Close, reopen, and compare: `std::fs::read` for the bytes and
//              `File::metadata().len()` for the size.
//         v.   Replace <path> with strictly fewer bytes, requiring the exact new
//              length and no byte of the old tail. **This runs before leg iv**,
//              which is why the legs are listed here in execution order rather
//              than in numeric order: every leg touching <path> has to finish
//              before the first leg that touches anything else, or a probe that
//              breaks a later leg has no settled earlier object to point at.
//         iv.  `std::fs::create_dir(<path>.d)`, then create, write, reopen and
//              compare `<path>.d/leaf.txt` -- a new file under a directory the
//              same run just made.
//
//   umbra-userspace-rustio big <path>
//       vi.  Leg vi alone. Write `BIG` bytes to <path>.big in one `write_all` --
//            a payload sized to cross the backend's per-call bound more than
//            once; see that constant for why it is the size it is, and for what
//            its own run does and does not buy. The harness mirrors the same
//            number as `RUSTIO_BIG` and the two must agree; only the harness's
//            assertions check that they do. Then read it back twice, once with
//            `std::fs::read` and once with `File::read_to_end`, requiring both to
//            be exact and to agree with each other.
//
//       This case reads nothing `edit` wrote and needs no seeded source, so the
//       two runs are independent and may be launched in either order.
//
// NO STDIO, DELIBERATELY, for `umbra-userspace-toy.c:21-27`'s reason: the
// verdict travels in the exit status alone. There is no `println!`, no
// `eprintln!`, and no `unwrap`/`expect`/`assert` anywhere below -- a panic would
// write a message and the message is the thing this fixture must not have.
//
// WHAT THIS FIXTURE DOES NOT CLAIM. It does not claim a measured set of
// syscalls. `nm -u` on a Rust binary is an *upper bound* and nothing more:
// measured on this host, a build with every `std::fs::metadata` call removed
// still references `_stat`, and a Rust binary references `_writev`, `_mmap`,
// `_dup`, `_fcntl`, `_opendir` and `_getcwd` without calling any of them. The
// C toy can argue from `nm -u` because it is 85 lines of C; this cannot, and no
// comment here may pretend otherwise. What this fixture proves is behavioural
// and nothing besides: the exit code below, plus the bytes and sizes the
// harness reads back out of the export through the NFSv4 client.
//
// DELIBERATELY ABSENT, each for a measured reason:
//
//   * `std::fs::metadata(path)` and `symlink_metadata`. These reach `stat`(338)
//     and `lstat`(340), which are in neither `abi::TRACED_STUBS` nor the
//     interposer's replaced set -- they go straight to the kernel. For a path
//     only the run created the kernel answers ENOENT, which is a wrong answer
//     that looks like a right one. An assertion on it would pin the kernel
//     rather than umbra, so every metadata read below comes from a descriptor.
//     That is also what codex's own core does (`no_follow/unix.rs:107,135`).
//   * `File::set_len`. It is `ftruncate`(201), which answers EBADF on a virtual
//     descriptor -- but NOT because anything in umbra refuses it, which is what
//     this note used to say. There is no "interposer's refused
//     descriptor-relative set": the interposer replaces exactly four symbols
//     (`open`, `read`, `write`, `close`) and refuses nothing at all, and
//     `ftruncate` is in neither that set nor `abi::TRACED_STUBS`. So the call is
//     kernel-bare -- it reaches the kernel untouched, and the EBADF is the
//     KERNEL's, for a descriptor above the `RLIMIT_NOFILE` fence umbra allocates
//     above. Same observable errno, different mechanism, and the mechanism is
//     what a reader needs in order to predict the next call's disposition.
//     Codex's no-follow write path uses it, so leg v is *not* a literal
//     transcription: it reaches truncation through an `O_TRUNC` open instead.
//   * `fs::read_dir`, `sync_all`/`fsync`, `fs::copy`, `fs::canonicalize`,
//     threads. The first is unclaimed by the interposer and has its own
//     fixture; the second is absent from codex's surface entirely *and*
//     refused; the rest reach calls outside the routed set for no coverage.
//
// Exit codes are this program's own and are kept clear of
// `umbra-userspace-edges.c`'s 70-86, so a status is never ambiguous between the
// two fixtures. Every code names one step:
//
//   90  wrong argument count, unknown case, or a <path> with no parent
//       directory to find the seeded source beside
//   91  the seeded source file could not be opened
//   92  fd metadata on the source failed, the descriptor does not describe a
//       regular file, or it reported a length that contradicts the bytes it then
//       handed over
//   93  the source could not be read, or its bytes are not the seed although the
//       length its descriptor reported for them was right
//   94  create/open for writing failed
//   95  write_all failed
//   96  reopen after writing failed
//   97  the reopened size disagrees with the bytes written
//   98  the bytes read back differ from the bytes written
//   99  mkdir of the new directory failed
//  100  create or write of the new file under it failed
//  101  the new file read back wrong
//  102  the shorter replacement left the old length
//  103  the shorter replacement left old bytes in the tail
//  104  the multi-chunk write failed
//  105  the multi-chunk read-back failed or came up short
//  106  the multi-chunk read-back differs from what was written
//  107  `std::fs::read` and `File::read_to_end` disagree
//
// This file is not a workspace member -- `Cargo.toml` lists 17 members, all
// `crates/*` -- so `cargo fmt --check` and `cargo clippy` never see it and it
// is hand-formatted.
//
// Build: rustc --edition 2021 -O umbra-userspace-rustio.rs -o umbra-userspace-rustio

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The bytes `launch` seeds `workspace/seed.txt` with. Must match
/// `userspace_run.rs`'s `launch`, which writes `b"seed\n"`.
const SEED: &[u8] = b"seed\n";

/// The modified version leg ii writes over the destination.
///
/// Deliberately *not* mirrored by a constant in `userspace_run.rs`: leg v
/// replaces these bytes before the run ends, so the harness can never observe
/// them through the client and would only be declaring a value it cannot check.
/// What pins this leg from outside is leg iii's own comparison, reported through
/// exit codes 97 and 98.
const MODIFIED: &[u8] = b"seed\nrustio replaced this line\n";

/// Leg v's strictly shorter replacement. Must match `RUSTIO_SHORT`.
///
/// It differs from `MODIFIED` in its second byte as well as its length, so a
/// replacement that kept the old prefix is caught alongside one that kept the
/// old tail.
const SHORT: &[u8] = b"short\n";

/// The file leg iv writes under the directory the same run created. Must match
/// `RUSTIO_LEAF`.
const LEAF: &[u8] = b"leaf under a routed directory\n";

/// Leg vi's payload size. It is the **smallest** value that satisfies this leg's
/// purpose, and it is small deliberately.
///
/// **The floor, and it is exactly where this sits.** The payload has to cross
/// `LibnfsRawTransport`'s 1 048 572-byte per-call bound *more than once*, because
/// that is what catches a library loop which completes exactly one remainder and
/// then stops. Two bounds is 2 097 144; this is 8 bytes past it, so the transfer
/// is two clamped calls plus an 8-byte third. A loop that stopped after one
/// remainder would leave those 8 bytes unwritten and the size assertion sees it.
///
/// **The ceiling is a watchdog, and it is why this is not larger.** The registry
/// `timeout_ms` is not only the per-request IPC deadline: it is also the tracer's
/// *whole-session* watchdog (`native.rs:1573`, `rsp.rs:146`), set before the twin
/// is even resigned, and the writer lease refuses any value at or above 15 s
/// (`check_renewal_budget`: `renew_after_millis / 2 <= timeout_ms`), so the
/// harness's 12 s is already near the top of the legal range and cannot be
/// raised. Leg vi moves this payload three times -- one `write_all`, then
/// `fs::read`, then `read_to_end` -- so its cost is three times whatever this is.
///
/// **The margin is load-bound, not size-bound, and that is the thing to know
/// before touching this number.** Measured on a 10-core host, **leg vi alone in a
/// run of its own** at 12 000 ms: unloaded it completes at every size up to
/// 2 621 440; with 4 CPU burners 2 621 440 already expires the watchdog while this
/// size still passes; with 8 burners even 1 572 864 expires it. So raising this
/// constant does not cost proportional headroom -- it costs the tolerance for
/// other work on the same machine, which is what made an earlier 3 145 728 and
/// then a 2 621 440 fail. Do not raise it without re-measuring under load.
///
/// **Read that figure for exactly the session it describes.** It was taken with
/// leg vi alone, and for one release of this fixture leg vi ran *sixth inside
/// `edit`*, sharing its 12 000 ms with process start, the twin resign and five
/// other legs -- so the number described a session the harness never actually ran.
/// That gap is why leg vi is now case `big`, in a routed run of its own: the
/// measured session and the executed session are the same session again.
///
/// **What that split buys, stated without overclaiming.** It raises the
/// contention threshold, because leg vi no longer shares a budget. It does **not**
/// make this case saturation-proof, and the ladder above is the reason: leg vi
/// alone, dedicated run, full budget, still expires the watchdog at 8 burners.
/// The residual is real and stays disclosed -- on a saturated host this is the
/// most watchdog-exposed step in the job, because it makes three passes over this
/// payload where the C `bigio` control makes two.
const BIG: usize = 2_097_152;

/// Leg vi's payload, whose bytes are position-dependent on purpose.
///
/// A constant fill cannot tell a correct read from one that returned the right
/// number of bytes from the wrong offset. This can, at any offset the harness
/// asks for -- which is why it asks at both chunk boundaries. Must match
/// `rustio_big_byte` in `userspace_run.rs`.
fn big_payload() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BIG);
    for index in 0..BIG {
        bytes.push(index.wrapping_mul(31).wrapping_add(7) as u8);
    }
    bytes
}

/// `<destination><suffix>`, as bytes rather than as text: the harness hands over
/// a path it canonicalized and nothing here may assume it is UTF-8.
fn sibling(destination: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = destination.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Legs i and iii both need the size a descriptor reports for an object it has
/// open, and both must let a failure through rather than defaulting it.
///
/// **The `Err` arm is the whole reason this is not `.ok()`, and it is what makes
/// the `fstat` mutation probe non-vacuous.** `std::fs::read` takes its capacity
/// hint through `.ok()` (`library/std/src/fs.rs:343`), so a routed `fstat` that
/// answered EBADF would cost it a hint and change nothing observable -- the read
/// still completes by probing. The leg that a broken `fstat` actually breaks is
/// the one codex wrote, which propagates: `file.metadata()?.is_file()`
/// (`no_follow/unix.rs:107`). Swallow the error here and the probe passes while
/// proving nothing at all.
fn descriptor_length(file: &File, require_regular: bool) -> Option<u64> {
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return None,
    };
    if require_regular && !metadata.is_file() {
        return None;
    }
    Some(metadata.len())
}

/// Write `bytes` over `path` through `File::create`, which opens
/// `O_WRONLY|O_CREAT|O_TRUNC`.
///
/// `File::create` rather than `set_len`: truncation has to be reached through
/// the open, because `set_len` is `ftruncate` and a virtual descriptor answers
/// it EBADF. `Ok(())` means the file was also closed, since the handle is
/// dropped at the end of this scope.
fn replace(path: &Path, bytes: &[u8], open_code: i32, write_code: i32) -> Result<(), i32> {
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(_) => return Err(open_code),
    };
    if file.write_all(bytes).is_err() {
        return Err(write_code);
    }
    Ok(())
}

/// The whole of case `edit`: legs i to vi, in the order the harness's probe
/// discriminators depend on.
fn case_edit(destination: &Path) -> i32 {
    let parent = match destination.parent() {
        Some(parent) => parent,
        None => return 90,
    };

    // Leg i -- read the existing source file, with its metadata taken from the
    // descriptor. This is `no_follow::open_file_sync`'s shape: open, then
    // require the descriptor to describe a regular file, then read it whole.
    let source = parent.join("seed.txt");
    let mut file = match File::open(&source) {
        Ok(file) => file,
        Err(_) => return 91,
    };
    let reported = match descriptor_length(&file, true) {
        Some(length) => length,
        None => return 92,
    };
    let mut seed = Vec::new();
    if file.read_to_end(&mut seed).is_err() {
        return 93;
    }
    drop(file);
    // The size and the bytes are separated on purpose, and which code each gets
    // is what makes two different mutation probes tell two different stories. A
    // descriptor that reports a length contradicting the bytes it then hands
    // over is fd metadata being wrong, which is 92's subject; bytes that are not
    // the seed while the length was right is the read direction, which is 93's.
    // Folded into one code, a probe on either could satisfy the other's case.
    if reported != seed.len() as u64 {
        return 92;
    }
    if seed != SEED {
        return 93;
    }

    // Leg ii -- write a modified version of it to the destination.
    if let Err(code) = replace(destination, MODIFIED, 94, 95) {
        return code;
    }

    // Leg iii -- close, reopen, compare. The size is checked before the bytes
    // so that a write direction which reported success and stored nothing has
    // one exit code rather than two.
    let back = match fs::read(destination) {
        Ok(back) => back,
        Err(_) => return 96,
    };
    let file = match File::open(destination) {
        Ok(file) => file,
        Err(_) => return 96,
    };
    let reported = match descriptor_length(&file, true) {
        Some(length) => length,
        None => return 96,
    };
    drop(file);
    if reported != MODIFIED.len() as u64 {
        return 97;
    }
    if back != MODIFIED {
        return 98;
    }

    // Leg v -- the shorter replacement, reaching truncation through the open.
    // Both halves are required: the exact new length from the descriptor, and
    // the bytes, so neither a stale tail nor a stale length can pass alone.
    if let Err(code) = replace(destination, SHORT, 94, 95) {
        return code;
    }
    let file = match File::open(destination) {
        Ok(file) => file,
        Err(_) => return 96,
    };
    let reported = match descriptor_length(&file, true) {
        Some(length) => length,
        None => return 96,
    };
    drop(file);
    if reported != SHORT.len() as u64 {
        return 102;
    }
    match fs::read(destination) {
        Ok(back) if back == SHORT => {}
        _ => return 103,
    }

    // Leg iv -- a new file, named absolutely, inside a directory this same run
    // created. `create_directory` then `write_file` is codex's own sequence.
    let directory = sibling(destination, ".d");
    if fs::create_dir(&directory).is_err() {
        return 99;
    }
    let leaf = directory.join("leaf.txt");
    if replace(&leaf, LEAF, 100, 100).is_err() {
        return 100;
    }
    match fs::read(&leaf) {
        Ok(back) if back == LEAF => {}
        _ => return 101,
    }

    0
}

/// The whole of case `big`: leg vi, in a routed run of its own.
///
/// **Why this is a separate case and not the sixth leg of `edit`.** The registry
/// `timeout_ms` is a whole-*session* watchdog, not a per-call one, and it is armed
/// before the twin is even resigned. Run sixth inside `edit`, leg vi's budget was
/// therefore shared with process start, the resign, and five other legs -- so the
/// margin measured for leg vi on its own never described the session the harness
/// actually ran. Giving it its own run makes the measured thing and the run thing
/// the same thing. It does not make it saturation-proof; `BIG` says what it buys.
///
/// This case depends on nothing `edit` does: it derives `<path>.big` from the one
/// path it is given and needs no seeded source, so the two runs are independent
/// and may be launched in either order.
fn case_big(destination: &Path) -> i32 {
    // Leg vi -- one `write_all` of a payload that crosses the backend's
    // per-call bound twice, read back through both of std's wrappers.
    //
    // The two wrappers are here because they take *different* routed paths and
    // both matter. `std::fs::read` sizes its buffer from the descriptor's
    // metadata alone. `File::read_to_end` goes through
    // `buffer_capacity_required`, which also asks `stream_position()` --
    // `lseek`(199), which a virtual descriptor refuses with EBADF. Both of its
    // legs are `.ok()?`, so the hint degrades to `None` and
    // `default_read_to_end` still reads correctly by probing. Requiring the two
    // to *agree* is the only thing here that would catch a future `lseek` that
    // answered a wrong offset instead of refusing outright.
    let big = sibling(destination, ".big");
    let payload = big_payload();
    if replace(&big, &payload, 104, 104).is_err() {
        return 104;
    }
    let via_fs_read = match fs::read(&big) {
        Ok(bytes) => bytes,
        Err(_) => return 105,
    };
    if via_fs_read.len() != payload.len() {
        return 105;
    }
    if via_fs_read != payload {
        return 106;
    }
    let mut file = match File::open(&big) {
        Ok(file) => file,
        Err(_) => return 105,
    };
    let mut via_read_to_end = Vec::new();
    if file.read_to_end(&mut via_read_to_end).is_err() {
        return 105;
    }
    drop(file);
    if via_read_to_end != via_fs_read {
        return 107;
    }

    0
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let case = match arguments.next() {
        Some(case) => case,
        None => return 90,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 90,
    };
    if arguments.next().is_some() {
        return 90;
    }
    if case == *"edit" {
        return case_edit(&destination);
    }
    if case == *"big" {
        return case_big(&destination);
    }
    90
}

fn main() {
    // `dispatch` has returned, so every handle it opened is closed -- which
    // matters, because the close is itself a routed call this fixture is here to
    // cross. `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
