// The exec'd half of `umbra-userspace-lifecycle.rs`'s `execenv` case: a routed
// create, write and close issued by an image that was entered through `execve`
// with an environment its parent rebuilt from nothing.
//
// A SEPARATE BINARY RATHER THAN A FOURTH ARGV CASE, which is the opposite of the
// `edges` fixture's choice and is deliberate. `umbra-userspace-edges.c` keeps its
// exec'd helper as a case of itself and has the Rust driver copy the binary to a
// different basename, because umbra caches every tracee as a resigned twin at
// `<sha256 of contents>/<basename>` and an image with identical contents and an
// identical name would resign to the SAME twin path as the launch target -- so it
// would be armed for the wrong reason and report a pass against the very defect
// the case exists to catch. A genuinely different program has genuinely different
// contents, so its digest differs and the copy-to-a-new-basename step is not
// needed at all. Two files cost one more `rustc` line in CI and remove a
// mechanism that has to be explained every time it is read.
//
// WHAT IT MEASURES, and it is only half of a measurement on its own: that the
// interposer is live in an image reached through an `execve` whose `envp`
// retained NOTHING from the launch except `DYLD_INSERT_LIBRARIES`. dyld re-loads
// the interposer into the new address space because that variable rides in
// `envp`, and umbra re-arms it at the exec stop; the write below is what proves
// both happened. The other half is the parent's, in the lifecycle fixture, which
// reaps this process and reports its code.
//
// ITS CONTRACT IS THE LIFECYCLE FIXTURE'S OWN: `envwriter <path>`, the same
// `<case> <destination>` shape every fixture in this directory takes, so the
// argv the parent builds is uniform with the argv the harness builds. `argv[0]`
// is NOT this image's path -- the parent replaces it, which is part of what the
// rebuild is -- so nothing here may read it.
//
// NO STDIO, no `unwrap`, no `expect`, no `assert`, and every call raw, for the
// lifecycle fixture's reasons. It writes `<path>.exec` and nothing else, and the
// harness reads those bytes back out of the export through the NFSv4 client.
//
// EXIT CODES SHARE THE LIFECYCLE FIXTURE'S 170-178 SLICE, following the `append`
// and `descriptor` pairs rather than taking a slice of its own -- the harness
// knows which program produced a code because it knows which one it launched:
//
//  170  wrong argument count, an unknown case name, or a <path> with no parent
//       directory to write beside
//  176  the routed create, write or close of `<path>.exec` failed -- the write
//       that COMPLETES the `execenv` case, which is why it is 176 here and in
//       the lifecycle fixture both
//
// This file is not a workspace member, so `cargo fmt` and `clippy` never see it
// and it is hand-formatted. Build: `rustc --edition 2021 -O <this file> -o <bin>`.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

extern "C" {
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn write(fd: i32, buffer: *const u8, count: usize) -> isize;
}

const O_WRONLY: i32 = 0x0000_0001;
const O_CREAT: i32 = 0x0000_0200;
const O_TRUNC: i32 = 0x0000_0400;
const O_CLOEXEC: i32 = 0x0100_0000;

/// The mode the produced object is given, matching the lifecycle fixture's.
const MODE: u32 = 0o644;

/// The bytes this image writes, and the name of the case that launched it.
const PRODUCED: &[u8] = b"execenv";

/// A NUL-terminated copy of a path.
fn cstr(path: &Path) -> Vec<u8> {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// `<destination>.exec`, spelled the way the harness reads it back.
fn produced(destination: &Path) -> PathBuf {
    let mut bytes = destination.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(b".exec");
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// Every byte through raw `write`(2), looping over short transfers.
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

fn dispatch() -> i32 {
    // `skip(1)` and never a read of `argv[0]`: the parent replaced it.
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
    if case.to_string_lossy() != "envwriter" {
        return 170;
    }

    let name = cstr(&produced(&destination));
    let fd = unsafe { open(name.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, MODE) };
    if fd < 0 {
        return 176;
    }
    if !write_all(fd, PRODUCED) {
        unsafe { close(fd) };
        return 176;
    }
    if unsafe { close(fd) } != 0 {
        return 176;
    }
    0
}

fn main() {
    std::process::exit(dispatch());
}
