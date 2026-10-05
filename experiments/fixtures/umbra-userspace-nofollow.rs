// The userspace-routing *path-walk* fixture: what a routed run does when it
// reaches a leaf through a directory descriptor rather than through an absolute
// path, and what `O_NOFOLLOW` means when the final component is a symlink.
//
// Every other fixture in this directory names its target by a whole path and
// lets the engine resolve it in one step. This one walks: it opens a directory
// first and then opens the leaf *relative to that descriptor*, which is the
// shape `openat`-based code uses and the shape in which "which component does
// `O_NOFOLLOW` govern" is even askable. The five stages below are the walk, and
// the fixture's whole contract is that its exit status names the FIRST one that
// failed:
//
//   (a) open the parent directory with `O_SEARCH` -- search-only, no read
//   (b) `openat` the leaf relative to that descriptor, for writing
//   (c) fd metadata on the leaf, requiring a regular file
//   (d) `set_len(0)` -- truncate before replacing
//   (e) write the replacement bytes
//
// MEASURED DISPOSITION AT THE PIN, and the reason each case exists. Measured
// against live NFS-Ganesha through `umbra-storage-nfs-userspace`, not predicted:
//
//   umbra-userspace-nofollow safe-path <path>
//       The happy-path walk composition baseline. (a), (b) and (c) are all
//       SERVED -- the routed open returns a virtual descriptor, `openat`
//       relative to it returns a second one, and `fstat` on it reports a
//       regular file -- and the walk dies at (d) with exit 155. The
//       inherited (d) characterization is #165's, NOT this fixture's: see
//       THE (d) STAGE BELONGS TO #165 below.
//
//   umbra-userspace-nofollow symlink-in-parent <path>
//       NOFOLLOW SCOPE. `link` is a symlink to the real parent directory and
//       the walk goes through it, with `O_NOFOLLOW` set on the leaf open.
//       The parent symlink IS followed and the leaf opens: `O_NOFOLLOW`
//       governs the FINAL component only, which is what POSIX says and what
//       the host does. Reaches (d), exit 155.
//
//   umbra-userspace-nofollow symlink-leaf <path>
//       The defect this fixture was written for. The leaf IS a symlink and
//       `O_NOFOLLOW` is set. The host answers `ELOOP`(62) to the caller at
//       stage (b) and the program continues; a routed run instead ENDS --
//       `InvalidPath during overlay: open refuses final symlink` -- so the
//       tracee is never resumed, there is no `finished:` line, and this
//       fixture's own exit code is never produced at all. 153 is what it
//       WOULD exit if the refusal were answered to it. The harness asserts
//       the run-ending shape, not a code.
//
//   umbra-userspace-nofollow symlink-leaf-follow <path>
//       THE FOLLOW PAIR, and it is what makes the case above a statement
//       about `O_NOFOLLOW` rather than about symlinks. Identical tree,
//       identical walk, one bit of difference -- `O_NOFOLLOW` is NOT set --
//       and the same leaf opens, `fstat`s and reaches (d), exit 155. Without
//       this half, `symlink-leaf` would be consistent with a routed run
//       refusing every symlink leaf, which it does not.
//
//   umbra-userspace-nofollow flagscan <path>
//       The open-flag admission scan, and the one case that reports through
//       the export rather than through its exit status, for the directory
//       fixture's reason: seven results do not fit in one exit code, and an
//       exit code cannot say WHICH combination changed. It opens one
//       directory seven ways and writes `<leaf>.flagscan` into the routed
//       workspace, one `label=ok` or `label=errno:N` line per combination,
//       which the harness reads back out through the NFSv4 client. Measured
//       routed: ALL SEVEN are admitted, including bare `O_EXEC`, which the
//       host refuses `EISDIR`(21) -- a divergence in the PERMISSIVE
//       direction, because `O_EXEC`'s bit is dropped by the decoder with no
//       field to hold it.
//
// THE (d) STAGE BELONGS TO #165, NOT TO THIS FIXTURE. Four of the shapes above
// reach (d) and fail there, and that failure is NOT this fixture's finding:
// `set_len` is `ftruncate`(201), which is already characterized by
// `a_rust_file_set_len_on_a_routed_descriptor_is_answered_ebadf` in
// `userspace_run.rs` under umbrella #165. This fixture carries (d) for one
// reason only -- it is the stage the walk must REACH to prove (a), (b) and (c)
// composed -- and the harness's assertions for the three reaching cases are
// about which stage was reached, never about `ftruncate` or its errno. When
// #165 is fixed those three cases advance to (e) and must be rewritten to
// assert the replacement bytes; they are not written to stay at 155 forever.
//
// THE MECHANISM BEHIND THAT `EBADF`, stated correctly because the sibling
// fixture states it wrongly. `ftruncate` is in NEITHER the interposer's
// replaced set NOR `abi::TRACED_STUBS`: the interposer replaces exactly four
// symbols (`open`, `read`, `write`, `close`) and refuses nothing at all. So the
// call is kernel-bare -- it reaches the kernel untouched, and the kernel has
// never heard of a descriptor above the `RLIMIT_NOFILE` fence umbra allocates
// above. The `EBADF` is the KERNEL's, not a refusal by anything in umbra. Same
// observable errno as a refusal, different mechanism -- and the mechanism is
// what a reader needs in order to predict the next call's disposition.
//
// WHY `O_NONBLOCK` IS ON EVERY LEAF OPEN. It is a no-op on a regular file, and
// every leaf this fixture opens is one, so it changes no measurement here. It
// is present because the opposite is unrecoverable: on a FIFO, `O_WRONLY` with
// no reader BLOCKS FOREVER, which was measured host-side (the control had to be
// killed) and which would hang the whole-session watchdog rather than fail a
// case. The flag documents the hazard at the call site that would meet it.
//
// THERE IS NO FIFO CASE, AND NO FIFO DISTINCTION IS DRAWN. A named-pipe leaf
// cannot be constructed at this pin by either route, so this fixture says
// NOTHING about how a routed run handles one: in-run `mkfifo` is kernel-bare
// and answers `ENOENT` because the kernel resolves a host path whose parent
// exists only in the shadow, and a host-seeded FIFO never gets as far as a
// tracee -- the workspace inventory scan refuses any entry that is not a
// regular file or a directory before the run launches. No
// handling-versus-blocking distinction can be drawn here AT ALL, in either
// direction. The `O_NONBLOCK` above is a hazard guard on an unreachable shape,
// not evidence about it. The same scan is why every symlink tree below is built
// IN-RUN rather than seeded: a seeded symlink is refused by name too.
//
// NO STDIO, DELIBERATELY, for `umbra-userspace-toy.c:21-27`'s reason: the
// verdict travels in the exit status alone. There is no `println!`, no
// `eprintln!`, and no `unwrap`/`expect`/`assert` anywhere below -- a panic would
// write a message and the message is the thing this fixture must not have.
// `flagscan`'s record is not an exception: it is written into the routed
// workspace and read back through the export, never to a stream.
//
// WHAT THIS FIXTURE DOES NOT CLAIM. It does not claim a measured syscall set;
// `nm -u` on a Rust binary is an upper bound and nothing more, for the reason
// `umbra-userspace-rustio.rs` records. What it proves is behavioural: the exit
// code below, plus the bytes the harness reads back out of the export through
// the NFSv4 client.
//
// TWO SENTINELS, because a run that ENDS produces no exit code and silence has
// to be told apart from "nothing ever ran":
//
//   <leaf>.<case>live   written BEFORE the walk starts. Its presence in the
//                       shadow proves this run was routing when it reached the
//                       refusal, so `symlink-leaf`'s assertion that nothing
//                       else is there means something.
//   <leaf>.<case>done   written ONLY after (e) and the read-back both
//                       succeeded. Absent on every shape measured at this pin.
//
// Exit codes are this program's own, and the slice 150-158 was verified free by
// enumerating every integer literal in every sibling fixture with comments
// stripped -- `toy`/`listing`/`listing-project`/`test-child` hold 0-9, `edges`
// 70-87, `rustio` 90-107, `buffered` 110-114, `stdio` 120-129, `append` pair
// 130-134, `descriptor` pair 140-148. `edges.c` also passes raw errnos through,
// and Darwin's `ELAST` is 107 on this host, so that passthrough cannot reach
// this slice either. Every code names one step:
//
//  150  wrong argument count, an unknown case name, or a <path> with no parent
//       directory to build the tree beside
//  151  the tree could not be built: mkdir, the leaf write, the symlink, the
//       pre-walk sentinel, or `flagscan`'s record write
//  152  stage (a): the search-only directory open failed
//  153  stage (b): the no-follow `openat` of the leaf failed
//  154  stage (c): fd metadata failed, or the leaf is not a regular file
//  155  stage (d): `set_len(0)` failed
//  156  stage (e): the write of the replacement bytes failed
//  157  the read-back disagrees with the bytes written, or `flagscan`'s record
//       does not read back as it was written
//  158  the leaf was MUTATED although a stage before (d) refused
//
// 158 is the one that makes the write-ascending invariant non-vacuous. Stages
// (a) through (c) are supposed to be read-only on the leaf: a refusal at any of
// them must leave the object byte-identical. 158 fires when a refusal happened
// AND the object changed anyway, which no code that only checked the exit
// status would catch. It is unreached at this pin and is here for the fix.
//
// This file is not a workspace member -- `experiments/` is outside `Cargo.toml`'s
// member list -- so `cargo fmt` and `clippy` never see it and it is hand-
// formatted. Build: `rustc --edition 2021 -O <this file> -o <bin>`.

use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

extern "C" {
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn openat(dirfd: i32, path: *const u8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn symlink(target: *const u8, link: *const u8) -> i32;
    fn __error() -> *mut i32;
}

/// `errno`, read through libc's own accessor rather than a global.
fn errno() -> i32 {
    unsafe { *__error() }
}

// Flag values measured on this host (`<fcntl.h>`, Darwin arm64) rather than
// recalled, because two of them are the whole point. `O_SEARCH` is not an
// independent bit: it is `O_EXEC | O_DIRECTORY`, so a decoder that masks only
// the `O_DIRECTORY` half still serves the open and silently loses the rest.
const O_WRONLY: i32 = 0x0000_0001;
const O_RDONLY: i32 = 0x0000_0000;
const O_NONBLOCK: i32 = 0x0000_0004;
const O_NOFOLLOW: i32 = 0x0000_0100;
const O_DIRECTORY: i32 = 0x0010_0000;
const O_CLOEXEC: i32 = 0x0100_0000;
const O_EXEC: i32 = 0x4000_0000;
const O_SEARCH: i32 = O_EXEC | O_DIRECTORY;

/// The bytes every case's leaf holds before the walk touches it.
const ORIGINAL: &[u8] = b"original bytes\n";
/// What stage (e) writes over them.
const REPLACEMENT: &[u8] = b"replaced by the no-follow walk\n";

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

/// Everything one walk case needs, built in-run.
struct Tree {
    /// The directory stage (a) opens -- possibly itself reached via a symlink.
    parent: PathBuf,
    /// The leaf name stage (b) opens relative to it.
    leaf: &'static str,
    /// The real file holding `ORIGINAL`, named WITHOUT going through any
    /// symlink, so a read-back proves the bytes rather than the link.
    verify: PathBuf,
    /// Whether stage (b) sets `O_NOFOLLOW`.
    no_follow: bool,
}

/// Build this case's tree inside the routed run.
///
/// Every case gets its own subtree, so no case can see another's leftovers even
/// if the harness ever shares one workspace between two of them. `mkdir` and
/// the leaf write go through `std::fs` -- both are routed -- while the symlink
/// goes through `symlink`(57) directly, because `std::os::unix::fs::symlink` is
/// the same call and the direct spelling keeps the errno readable.
fn build(case: &str, root: &Path) -> Option<Tree> {
    let base = root.join(case);
    if std::fs::create_dir(&base).is_err() {
        return None;
    }
    match case {
        // A plain directory holding a plain file: nothing to follow anywhere.
        // This is the baseline whose whole job is to reach (d).
        "safe-path" => {
            let parent = base.join("parent");
            if std::fs::create_dir(&parent).is_err() {
                return None;
            }
            let leaf = parent.join("leaf.txt");
            if std::fs::write(&leaf, ORIGINAL).is_err() {
                return None;
            }
            Some(Tree {
                parent,
                leaf: "leaf.txt",
                verify: leaf,
                no_follow: true,
            })
        }
        // `real/` holds the file; `link` points at `real`; the walk goes
        // through `link`. The symlink is therefore in a PARENT position and
        // `O_NOFOLLOW` on the leaf open must not reject it.
        "symlink-in-parent" => {
            let real = base.join("real");
            if std::fs::create_dir(&real).is_err() {
                return None;
            }
            let verify = real.join("leaf.txt");
            if std::fs::write(&verify, ORIGINAL).is_err() {
                return None;
            }
            let link = base.join("link");
            // Relative target: the link and its target are siblings, so this
            // resolves the same way wherever the workspace is rooted.
            if unsafe { symlink(cstr(Path::new("real")).as_ptr(), cstr(&link).as_ptr()) } != 0 {
                return None;
            }
            Some(Tree {
                parent: link,
                leaf: "leaf.txt",
                verify,
                no_follow: true,
            })
        }
        // One tree, two cases: `leaf.txt` is a symlink to the real
        // `target.txt` beside it. The ONLY difference between the two is
        // `no_follow`, which is what makes the pair a statement about the flag.
        "symlink-leaf" | "symlink-leaf-follow" => {
            let parent = base.join("parent");
            if std::fs::create_dir(&parent).is_err() {
                return None;
            }
            let verify = parent.join("target.txt");
            if std::fs::write(&verify, ORIGINAL).is_err() {
                return None;
            }
            let link = parent.join("leaf.txt");
            if unsafe { symlink(cstr(Path::new("target.txt")).as_ptr(), cstr(&link).as_ptr()) } != 0
            {
                return None;
            }
            Some(Tree {
                parent,
                leaf: "leaf.txt",
                verify,
                no_follow: case == "symlink-leaf",
            })
        }
        _ => None,
    }
}

/// Has the leaf changed although a stage before (d) refused?
///
/// A read error is NOT a mutation: a refused walk can leave a name that does
/// not resolve, and reporting that as a mutated object would be a wrong answer
/// that looks like a right one. Only bytes that are present AND different count.
fn mutated_before_write(verify: &Path) -> bool {
    match std::fs::read(verify) {
        Ok(bytes) => bytes != ORIGINAL,
        Err(_) => false,
    }
}

/// Stages (b) through (e) under an already-open directory descriptor.
fn walk(dirfd: i32, tree: &Tree) -> i32 {
    let extra = if tree.no_follow { O_NOFOLLOW } else { 0 };
    // `O_NONBLOCK` on every leaf open; see WHY `O_NONBLOCK` in the header.
    let flags = O_WRONLY | O_CLOEXEC | O_NONBLOCK | extra;
    let fd = unsafe { openat(dirfd, cname(tree.leaf).as_ptr(), flags) };
    if fd < 0 {
        // Stage (b) is where the host refuses a no-follow symlink leaf, with
        // `ELOOP`. A routed run does not get here at all -- it ends -- so this
        // arm is what a SOFTENED refusal would reach.
        return if mutated_before_write(&tree.verify) {
            158
        } else {
            153
        };
    }
    // The `File` owns the descriptor from here and closes it on drop, so there
    // is no explicit `close` for it: a second one would close a number the
    // process no longer owns.
    let mut file = unsafe { File::from_raw_fd(fd) };

    // Stage (c). The metadata comes from the DESCRIPTOR, never from the path:
    // `stat`(338) and `lstat`(340) are kernel-bare, so a path-based metadata
    // read would answer about the host rather than about the routed object.
    match file.metadata() {
        Ok(metadata) => {
            if !metadata.is_file() {
                return if mutated_before_write(&tree.verify) {
                    158
                } else {
                    154
                };
            }
        }
        Err(_) => {
            return if mutated_before_write(&tree.verify) {
                158
            } else {
                154
            };
        }
    }

    // Stage (d). This is #165's ground, carried here only so that reaching it
    // proves (a), (b) and (c) composed. See THE (d) STAGE BELONGS TO #165.
    if file.set_len(0).is_err() {
        return 155;
    }

    // Stage (e).
    if file.write_all(REPLACEMENT).is_err() {
        return 156;
    }
    // Drop the handle before reading back, so the close -- itself a routed
    // call -- has happened before the comparison.
    drop(file);

    match std::fs::read(&tree.verify) {
        Ok(bytes) if bytes == REPLACEMENT => 0,
        _ => 157,
    }
}

/// One walk case, end to end.
fn case_walk(case: &str, destination: &Path, root: &Path) -> i32 {
    // The pre-walk sentinel. It is written FIRST, before any tree exists,
    // because its whole job is to prove this run was routing at all -- and a
    // run that the refusal ends never gets to write anything later.
    if std::fs::write(sentinel(destination, &format!(".{case}live")), case.as_bytes()).is_err() {
        return 151;
    }
    let tree = match build(case, root) {
        Some(tree) => tree,
        None => return 151,
    };

    // Stage (a): search-only, no read permission requested. `O_SEARCH` is
    // `O_EXEC | O_DIRECTORY`; a decoder that keeps only the second half serves
    // this open as a plain directory read, which is what the routed side does.
    let dirfd = unsafe { open(cstr(&tree.parent).as_ptr(), O_SEARCH | O_CLOEXEC) };
    if dirfd < 0 {
        return if mutated_before_write(&tree.verify) {
            158
        } else {
            152
        };
    }
    let code = walk(dirfd, &tree);
    unsafe { close(dirfd) };
    if code != 0 {
        return code;
    }
    if std::fs::write(sentinel(destination, &format!(".{case}done")), case.as_bytes()).is_err() {
        return 151;
    }
    0
}

/// The open-flag admission scan.
///
/// Seven combinations against ONE directory, each reported by name into a file
/// in the routed workspace. The record is the assertion surface: an exit code
/// could say that something changed but never WHICH combination, and the
/// divergence this case exists to pin is a single row of it.
fn case_flagscan(destination: &Path, root: &Path) -> i32 {
    let dir = root.join("flagscan");
    if std::fs::create_dir(&dir).is_err() {
        return 151;
    }
    let path = cstr(&dir);
    let mut record = Vec::new();
    for (label, flags) in [
        ("O_RDONLY", O_RDONLY),
        ("O_RDONLY|O_DIRECTORY", O_RDONLY | O_DIRECTORY),
        ("O_RDONLY|O_DIRECTORY|O_CLOEXEC", O_RDONLY | O_DIRECTORY | O_CLOEXEC),
        ("O_SEARCH", O_SEARCH),
        ("O_SEARCH|O_CLOEXEC", O_SEARCH | O_CLOEXEC),
        ("O_EXEC", O_EXEC),
        ("O_RDONLY|O_NOFOLLOW|O_DIRECTORY", O_RDONLY | O_NOFOLLOW | O_DIRECTORY),
    ] {
        let fd = unsafe { open(path.as_ptr(), flags) };
        record.extend_from_slice(label.as_bytes());
        if fd < 0 {
            // The errno is part of the record on purpose: "refused" is not the
            // claim, "refused with THIS errno" is. A host answers `EISDIR`(21)
            // to bare `O_EXEC` and that number is the one a fix would produce.
            record.extend_from_slice(format!("=errno:{}\n", errno()).as_bytes());
        } else {
            unsafe { close(fd) };
            record.extend_from_slice(b"=ok\n");
        }
    }
    let report = sentinel(destination, ".flagscan");
    if std::fs::write(&report, &record).is_err() {
        return 151;
    }
    match std::fs::read(&report) {
        Ok(bytes) if bytes == record => 0,
        _ => 157,
    }
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let case = match arguments.next() {
        Some(case) => case,
        None => return 150,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 150,
    };
    if arguments.next().is_some() {
        return 150;
    }
    let root = match destination.parent() {
        Some(root) => root.to_path_buf(),
        None => return 150,
    };
    let case = case.to_string_lossy().into_owned();
    if case == "flagscan" {
        return case_flagscan(&destination, &root);
    }
    if case == "safe-path"
        || case == "symlink-in-parent"
        || case == "symlink-leaf"
        || case == "symlink-leaf-follow"
    {
        return case_walk(&case, &destination, &root);
    }
    150
}

fn main() {
    // `dispatch` has returned, so every handle it opened is closed -- which
    // matters, because the close is itself a routed call this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
