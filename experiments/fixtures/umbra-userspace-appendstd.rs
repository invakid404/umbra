// The append fixture's Rust half: the same three shapes as
// `umbra-userspace-append.c`, issued through `std::fs::OpenOptions` instead of
// a bare `open(2)`.
//
// WHY A PAIR
// ----------
// `umbra-userspace-stdio.c` + `umbra-userspace-buffered.rs` are the precedent,
// and the construction here is identical: one operation, two call paths, and
// the comparison is the measurement. Read this file with the C one; its header
// carries the shared argument -- what the three cases are, what #161 is, why
// the absent case is an *ordering control* rather than an append measurement,
// why the sentinel's placement differs by case, and what each exit code means.
// Only what is specific to Rust is repeated below.
//
// WHAT THE SECOND CALL PATH IS FOR
// ---------------------------------
// The question a one-language fixture cannot answer is whether the refusal is
// a property of the *flag* or of the *caller*. `umbra-userspace-buffered.rs`
// exists because exactly that question had a surprising answer one slice over:
// C stdio's final write escapes routing through `__write_nocancel`(397) while
// Rust's identical-looking buffered write does not, because std's write is
// compiled into the executable where `DYLD_INTERPOSE` reaches it. So "the two
// languages do the same thing here" is not safe to assume in this tree -- it is
// the thing that was measured and came out false last time.
//
// For append the mechanism is different and the pairing is still load-bearing.
// `OpenOptions::append(true)` is not a Rust-level emulation: std sets
// `O_APPEND` in the flag word it hands to `open`, and `open` is in
// `abi::TRACED_STUBS`. So both halves of this pair are expected to present the
// same flag bit at the same decision point. That expectation is what the pair
// *checks*; it is not an assumption either file is written on.
//
// Case 3 is also the Codex shape the slate cites. A persistence layer that
// opens its log append-mode-and-create-if-needed is `.append(true)
// .create(true)`, which is `create-absent` exactly.
//
// WHY `.read(true)` IS IN `bare-existing` AND NOTHING TURNS ON IT
// ----------------------------------------------------------------
// The persistence shape this case transcribes opens its file for reading *and*
// appending, so the open carries `read`, `write` and `append` all true. It is
// included for that fidelity and for no other reason: the append bit alone
// decides the refusal, so the case would reach the same decision point without
// it. Stated here because a reader who assumed `.read(true)` was load-bearing
// would protect it from an edit that does not matter, and one who assumed it
// was accidental would delete a deliberate transcription.
//
// NO DIAGNOSTIC OUTPUT, DELIBERATELY, and no `unwrap`/`expect`/`assert`
// anywhere below. `umbra-userspace-toy.c:21-27`'s reason: a traced tracee
// inherits the platform provider's closed standard output, so nothing printed
// here is readable by any harness, and a panic's message is the one thing this
// program must not produce. The verdict is the exit status -- and for the two
// cases that reach the append check, the verdict is the *run's*, read from the
// journal, because this program is never resumed to report one.
//
// EXIT CODES
// ----------
// Code-for-code identical with `umbra-userspace-append.c`'s, which is the
// point: a status means the same thing whichever half of the pair produced it.
// Kept clear of `umbra-userspace-edges.c`'s 70-87, this file's own sibling
// `umbra-userspace-rustio.rs`'s 90-107, `umbra-userspace-buffered.rs`'s 110-114
// and `umbra-userspace-stdio.c`'s 120-129. 130 is the first free *decade above
// the allocated range*, which is the unit every fixture above allocates in:
// the blocks ascend 70s, 90s-100s, 110s, 120s, now 130s. The qualifier is
// load-bearing -- 10-69 is entirely unused, so 130 is not the first free
// decade, it is the first free one that continues the ascent. 88-89, 108-109
// and 115-119 are free as well, but no block here starts mid-decade.
// (buffered.rs records edges.c as 70-86; edges.c does define 87, so 70-87 is
// the accurate range. buffered.rs is a shipped fixture and is left untouched.)
//
//   errno an errno, returned verbatim -- expected from `bare-absent` alone
//         (2 = ENOENT); from either other case it is a disposition change.
//         **The bound is 106, not 64**: `ELAST` is 107 on this SDK and real
//         values reach 106 (`EOVERFLOW` 84, `EOWNERDEAD` 105, `EQFULL` 106), so
//         a verbatim errno in 0-106 overlaps *three* of the blocks above, not
//         one: the 2-9 range shared by `umbra-userspace-toy.c`,
//         `umbra-userspace-listing.c` and `umbra-userspace-listing-project.c`;
//         `umbra-userspace-edges.c`'s 70-87; and `umbra-userspace-rustio.rs`'s
//         90-107, where a status in 90-106 is ambiguous between an errno from
//         this pair and a step code from `rustio`.
//
//         None of that is a defect. `ENOENT` (2) is the only errno these cases
//         can produce -- every other disposition is a run stop, which returns
//         no status at all -- so the 2-9 overlap is the only one ever reached
//         and the other two are theoretical; no errno is >= 130, so this
//         file's own block stays unambiguous; and the harness always knows
//         which fixture it launched. The ambiguity is a human-diagnosis
//         nicety, not a correctness problem.
//   130   wrong argument count, or an unknown case name
//   131   an open that was expected to be refused SUCCEEDED -- the
//         non-green-wash alarm, and what fires when append is admitted
//   132   setup failed: the target could not be prepared or inspected
//   133   the refused open failed but reported no errno
//   134   the liveness sentinel's own routed write failed
//
// `raw_os_error()` is how the errno is recovered, and it is taken through the
// `Option` rather than past it: `io::Error`s that carry no OS errno exist, and
// one of those flattened to 0 would be indistinguishable from success. The
// `None` arm is 133, which is the same claim `umbra-userspace-edges.c`'s 81
// makes.
//
// WHEN THIS GOES RED. Deliberately -- see the C half's header. The day a routed
// `O_APPEND` is admitted, `bare-existing` and `create-absent` exit 131. Each
// test's doc comment in `userspace_run.rs` says what to change; for
// `bare-existing` the characterization is replaced by the positive invariant,
// that an appended write **extends** `seed\n` rather than replacing it.
//
// This file is not a workspace member -- `Cargo.toml` lists its members under
// `crates/*` -- so `cargo fmt --check` and `cargo clippy` never see it and it
// is hand-formatted. `umbra-userspace-rustio.rs` and
// `umbra-userspace-buffered.rs` say the same of themselves.
//
// Build: rustc --edition 2021 -O umbra-userspace-appendstd.rs -o umbra-userspace-appendstd

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// One routed create-and-write, used only by the sentinel below.
fn routed_write(path: &Path, bytes: &[u8]) -> i32 {
    let file = match File::create(path) {
        Ok(file) => file,
        Err(_) => return 134,
    };
    let mut file = file;
    if file.write_all(bytes).is_err() {
        return 134;
    }
    // Taken through the `Result` rather than left to `Drop`, which discards it:
    // a sentinel whose close failed has not proven the bytes landed, and this
    // is the one call in the file whose success the harness depends on.
    if file.flush().is_err() {
        return 134;
    }
    drop(file);
    0
}

/// Write the liveness sentinel at `<path>.<case>live`, payload the case name.
///
/// Byte-identical in placement and payload to the C half's, so the harness
/// reads both halves of the pair the same way.
fn sentinel(path: &Path, case: &str) -> i32 {
    routed_write(&derive(path, &format!(".{case}live")), case.as_bytes())
}

/// Report what an open that was expected to be refused actually did.
///
/// In a tree whose refusal still fires the caller **does not return** from the
/// open at all -- the run is stopped inside it. Every arm below is therefore a
/// finding rather than an ordinary outcome.
fn report(opened: std::io::Result<File>) -> i32 {
    match opened {
        Ok(file) => {
            drop(file);
            131
        }
        Err(error) => match error.raw_os_error() {
            Some(0) | None => 133,
            Some(errno) => errno,
        },
    }
}

/// `.read(true).append(true)` against the harness's pre-seeded, nonempty
/// `seed.txt` -- the shape with content to append to.
///
/// The target's length is checked first for the C half's reason: this case
/// means nothing unless the file actually has content, so "exists and is not
/// empty" is measured rather than assumed.
fn case_bare_existing(destination: &Path) -> i32 {
    let target = match destination.parent() {
        Some(parent) => parent.join("seed.txt"),
        None => return 132,
    };
    match std::fs::metadata(&target) {
        Ok(info) if info.len() > 0 => {}
        _ => return 132,
    }
    let failure = sentinel(destination, "bare-existing");
    if failure != 0 {
        return failure;
    }
    report(OpenOptions::new().read(true).append(true).open(&target))
}

/// `.append(true)` against a sibling that is not there -- the ordering control.
///
/// No `create`, so name resolution answers before the append check is reached.
/// The sentinel goes **after**, per `missingparent`: the refusal is non-fatal,
/// so the run is still alive to write it.
fn case_bare_absent(destination: &Path) -> i32 {
    let target = derive(destination, ".ab");
    let answer = report(OpenOptions::new().append(true).open(&target));
    if answer == 131 || answer == 133 {
        return answer;
    }
    let failure = sentinel(destination, "bare-absent");
    if failure != 0 {
        return failure;
    }
    answer
}

/// `.append(true).create(true)` against a sibling that is not there.
///
/// `create` is what carries this past the absent test and into the append
/// check. The target is deliberately not pre-created: what the harness reads
/// this case for is that the refused open created nothing.
fn case_create_absent(destination: &Path) -> i32 {
    let target = derive(destination, ".ap");
    let failure = sentinel(destination, "create-absent");
    if failure != 0 {
        return failure;
    }
    report(OpenOptions::new().append(true).create(true).open(&target))
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let case = match arguments.next() {
        Some(case) => case,
        None => return 130,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 130,
    };
    // Nothing may follow the destination. A fixture that ignored extra
    // arguments would silently accept a harness that had started passing it
    // something this file does not understand.
    if arguments.next().is_some() {
        return 130;
    }
    match case.to_str() {
        Some("bare-existing") => case_bare_existing(&destination),
        Some("bare-absent") => case_bare_absent(&destination),
        Some("create-absent") => case_create_absent(&destination),
        _ => 130,
    }
}

fn main() {
    // `dispatch` has returned, so every file it opened is closed -- and those
    // closes are themselves routed calls this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
