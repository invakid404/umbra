// The buffered-output *control*: a Rust `BufWriter` over a routed destination,
// with an explicit, checked flush.
//
// WHAT IT CONTROLS FOR
// --------------------
// `umbra-userspace-stdio.c`'s two buffered cases measure issue #127: C stdio's
// flush reaches the kernel through `__write_nocancel`(397), which is in neither
// `abi::TRACED_STUBS` nor anything `DYLD_INTERPOSE` can rebind, so a routed
// descriptor's buffered bytes are dropped with `EBADF` and the export keeps an
// object that exists and is empty.
//
// The obvious next question is whether "buffered output" is the thing that
// breaks. It is not, and this fixture is the measurement that says so. Rust's
// `BufWriter` is not C stdio: the `write` that `std` issues at the end of its
// buffer is compiled *into the executable*, which puts it at exactly the call
// sites `DYLD_INTERPOSE` rebinds. Measured end to end on this host: this case
// exits 0 and the export holds every byte.
//
// So the fault line is not buffering. It is whether the final write instruction
// is covered by either mechanism. C stdio's is `__write_nocancel`(397), which
// has no row in `TRACED_STUBS` and is issued from inside the dyld shared cache
// where interposition cannot rebind it, so neither mechanism covers it; Rust's
// is in the traced executable, where interposition does. A breakpoint would
// have reached stdio's call perfectly well -- 398, 399 and 339 all fire from
// inside libsystem on this path -- there simply is no row for 397. Without this
// control, #127 would read as "buffered writes are broken", and the next person
// to touch it would look in the wrong place.
//
// That also makes this a genuine control rather than a second defect case: it
// passes on master. Nothing below characterizes a known defect, and if this
// case ever goes red it is a regression in routing, not a known-unfixed shape.
//
// WHY A FILE OF ITS OWN, NOT A CASE IN `umbra-userspace-rustio.rs`
// ----------------------------------------------------------------
// `rustio` is the cheaper edit -- it already dispatches on a case name and the
// harness already has a launcher for it -- and it is still the wrong place,
// for two reasons.
//
// Blast radius. Five of the harness's tests hang off `rustio`: its baseline and
// four mutation probes. A compile error or an exit-code collision there reddens
// proofs that currently pass and are themselves serving as controls.
//
// Stated purpose. `rustio`'s header argues at length, with line references, that
// its legs are transcriptions of codex's own synchronous exec-server calls. A
// `BufWriter` control is not from that surface; it is here to isolate the C
// stdio axis above. Adding it would make a careful file's own doc comment partly
// false, which is a worse outcome than one more fixture in a directory that
// already holds six.
//
// ONE CASE, NOT TWO, AND THE SECOND ONE IS MEASURED RATHER THAN ASSUMED
// ---------------------------------------------------------------------
// The tempting second case is the Rust analogue of `ignored`: `BufWriter` +
// `write_all` + `drop`, where `Drop` flushes and swallows whatever the flush
// says. Measured, both ways: it lands the same bytes as this case on master,
// and the same zero bytes under the `write` mutation probe, with exit 0 in all
// four combinations. There is no world in which the two cases differ, so the
// second one would be a test that cannot fail differently from this one. It is
// deliberately absent.
//
// The explicit flush is not decoration either. It is the half of the invariant
// this slice is built on -- "successful output means the complete expected file
// exists; any failed flush is observable" -- that a control has to exercise: a
// case that never asks whether its flush worked cannot demonstrate that asking
// gets a truthful answer.
//
// NO DIAGNOSTIC OUTPUT, DELIBERATELY, and no `unwrap`/`expect`/`assert`
// anywhere below. `umbra-userspace-toy.c:21-27`'s reason: a traced tracee
// inherits the platform provider's closed standard output, so nothing printed
// here is readable by any harness, and a panic's message is the one thing this
// program must not produce. The verdict is the exit status; the bytes are the
// harness's to read out of the export through the NFSv4 client.
//
// WHAT THIS FIXTURE DOES NOT CLAIM. Not a measured syscall set: `nm -u` on a
// Rust binary is an upper bound and nothing more, as `umbra-userspace-rustio.rs`
// records in detail. What it proves is the exit code below plus the bytes and
// the size the harness reads back.
//
// EXIT CODES
// ----------
// This program's own, kept clear of `umbra-userspace-edges.c`'s 70-86,
// `umbra-userspace-rustio.rs`'s 90-107 and `umbra-userspace-stdio.c`'s 120-129,
// so a status is never ambiguous between fixtures. Every code names one step:
//
//   110  wrong argument count
//   111  create/open of the destination failed
//   112  `write_all` into the buffered writer failed
//   113  the explicit `flush` reported failure   <- the control's whole point
//   114  `into_inner` reported failure after a flush that had succeeded
//
// There is deliberately no code for a failed *close*. `File`'s `Drop` discards
// the result of its `close` and `std` exposes no way to consult it, so a code
// here would name a condition this program cannot observe. The close is still
// crossed -- it is a routed call, and `main` below is written so that it
// happens before the process exits.
//
// This file is not a workspace member -- `Cargo.toml` lists 17 members, all
// `crates/*` -- so `cargo fmt --check` and `cargo clippy` never see it and it
// is hand-formatted.
//
// Build: rustc --edition 2021 -O umbra-userspace-buffered.rs -o umbra-userspace-buffered

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The bytes this control persists.
///
/// Must match `userspace_run.rs`'s `PAYLOAD` and `umbra-userspace-stdio.c`'s,
/// and that is the requirement rather than an incidental duplication: the
/// comparison this fixture exists to support -- C stdio's empty object against
/// this one's complete file -- is only exact if both cases wrote the same bytes
/// to the same kind of destination.
const PAYLOAD: &[u8] = b"umbra-userspace-nfs\n";

/// `BufWriter` + `write_all` + an explicit, checked `flush`.
fn case_flushed(destination: &Path) -> i32 {
    let file = match File::create(destination) {
        Ok(file) => file,
        Err(_) => return 111,
    };
    let mut writer = BufWriter::new(file);
    // 20 bytes against `BufWriter`'s 8 KiB default capacity, so this call
    // reaches no descriptor at all: it copies into the buffer and returns.
    // Everything that touches the routed descriptor happens in the flush
    // below, which is what makes the flush worth checking separately.
    if writer.write_all(PAYLOAD).is_err() {
        return 112;
    }
    if writer.flush().is_err() {
        return 113;
    }
    // The buffer is empty now, so this cannot flush again and cannot fail --
    // but it is taken through the `Result` rather than past it, because
    // `into_inner` on an unflushed writer *can* fail and a fixture that
    // swallowed that would be asserting less than it looks like it does.
    let file = match writer.into_inner() {
        Ok(file) => file,
        Err(_) => return 114,
    };
    // Explicit, so the close is inside this function and ahead of the
    // `process::exit` in `main` rather than depending on where a temporary
    // happened to be dropped.
    drop(file);
    0
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 110,
    };
    // One case, so there is no case name to dispatch on and nothing may follow
    // the destination. A fixture that ignored extra arguments would silently
    // accept a harness that had started passing it a case name.
    if arguments.next().is_some() {
        return 110;
    }
    case_flushed(&destination)
}

fn main() {
    // `dispatch` has returned, so the file it opened is closed -- which matters,
    // because that close is itself a routed call this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
