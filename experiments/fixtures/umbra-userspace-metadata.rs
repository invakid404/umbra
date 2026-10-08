// The userspace-routing *symlink-read* fixture: what a routed run does when it
// asks the namespace to read back a symlink it created inside its own shadow.
//
// Both cases below build the same tree and read the same link; the only
// difference is WHICH SYSCALL asks, because `readlink`(58) and
// `readlinkat`(473) are two separate `TRACED_STUBS` rows that decode to one
// `FsOp::ReadLink`. The pair is what makes the finding a statement about the
// operation rather than about one stub. Two stages, and the fixture's whole
// contract is that its exit status names the FIRST one that failed:
//
//   (a) read the link -- `readlink` on its whole path, or `readlinkat` on its
//       name relative to a descriptor for its parent
//   (b) the bytes handed back are the link's target, exactly and entirely
//
// MEASURED DISPOSITION AT THE PIN, against live NFS-Ganesha through
// `umbra-storage-nfs-userspace`, not predicted:
//
//   umbra-userspace-metadata readlink-shadow <path>
//       The run ENDS. `UnsupportedCapability during overlay: ReadLink requires
//       set_readlink_buffer; typed read_link is available (errno: None)` --
//       umbra exits 1, the tracee is never resumed, there is no `finished:`
//       line, the journal records no `RunCompleted`, and this fixture's own
//       exit code is never produced at all. The harness asserts the
//       run-ending shape, not a code.
//
//   umbra-userspace-metadata readlinkat-shadow <path>
//       IDENTICAL, byte for byte, including the message. The descriptor-
//       relative form reaches the same engine arm through the same decode, so
//       the gap is in the operation's supervisor binding and not in either
//       stub. Measured, not assumed: the dirfd open that precedes it is
//       SERVED, so the run dies on the link read rather than before it.
//
// THE GAP IS A SUPERVISOR BINDING, NOT A REFUSAL, and that distinction is the
// finding. `FsOp::ReadLink`'s engine arm EXISTS and is unconditional --
// `umbra-overlay/src/engine.rs:2855`, no `routed()` guard and no shadow guard --
// and it resolves the link's target correctly before it fails. What it then
// needs is a buffer bound by `Overlay::set_readlink_buffer`, and **no
// production caller on the routed path ever binds one**: `routing_for` in
// `umbra-supervisor/src/events.rs` binds buffers for `FsOp::Open`, `FsOp::Read`
// and `FsOp::Write`, the stat buffer for `FsOp::Fstat` and the directory buffer
// for `FsOp::ReadDir`, and `FsOp::ReadLink` appears nowhere in that file's
// complete set of `FsOp` variants. Every `set_readlink_buffer` call site in the
// tree is a unit test or a test harness. So the engine arm is reached with
// `readlink_buffer == None` and raises -- and an `Err` out of `resolve` is
// raised rather than answered, which ends the run.
//
// THIS IS A DIFFERENT CLASS FROM THE THREE NAMED UMBRELLAS, which is why this
// fixture exists rather than a case being added to an existing one. It is not
// #152 (`ENOTSUP` answered to the tracee at engine.rs:3009, run survives, and
// whose residual is path `Stat`/`Access` only -- `ReadLink` was routed out of
// that arm deliberately). It is not #165 (the KERNEL's `EBADF` on a descriptor
// above umbra's `RLIMIT_NOFILE` fence, run survives). It is not #167
// (path-walk). It is a run-ending internal error on a traced, decoded,
// path-resolved operation whose engine arm exists and whose supervisor binding
// does not.
//
// THE FIX IS NOT IN THIS FIXTURE'S SCOPE, and these cases are written to go RED
// when it lands rather than to hold the current behaviour open. What to write
// in their place is stated in the harness beside each assertion: the tracee's
// own successful `readlink`, the target bytes `target.txt`, this fixture's exit
// 0, both sentinels present, and a `RunCompleted` in the journal.
//
// WHY `extern "C"` AND NOT `std`, AND NOT THE `libc` CRATE. `std::fs::read_link`
// reaches `readlink`(58), which is traced, so the path form alone could have
// been written in safe `std`. `readlinkat`(473) could not: `std` offers no
// descriptor-relative link read at all, so 473 has no Rust call site without a
// declaration. Declaring both keeps the pair symmetric -- one call shape, one
// buffer, one comparison -- so a difference between the two arms is a
// difference in umbra rather than in how this file reached them. The `libc`
// crate is not used because this file is built by bare `rustc` with no
// dependency resolution at all.
//
// WHAT THIS FIXTURE DOES NOT COVER, and the reasons are measured rather than
// editorial:
//
//   * `stat` and `lstat`. They are KERNEL-BARE -- `stat` issues 338 and `lstat`
//     issues 340, measured on this host by scanning each resolved libc stub for
//     the `movz x16, #imm16` preceding its `svc`, and neither number appears in
//     `abi::TRACED_STUBS` or the interposer's replaced set. A routed run never
//     sees the call, so an assertion on one would pin the KERNEL rather than
//     umbra. `stat_and_lstat_and_the_getdirentries_fallback_are_kernel_bare` in
//     `userspace_run.rs` pins that classification at the source level instead,
//     which needs no run at all. `umbra-userspace-rustio.rs`'s DELIBERATELY
//     ABSENT block reached the same conclusion first.
//   * `lstat` as a case distinct from `stat`. It CANNOT diverge by
//     construction: `469`/`470` both decode to one `FsOp::Stat` differing only
//     in a `follow` bool, and neither relevant engine guard inspects it.
//   * `fstat`. It is SERVED -- `resolve_routed_fstat` plus `routed_stat` -- and
//     already covered twice in `userspace_run.rs`. It is the documented
//     EXCEPTION to #165's descriptor-op family, not a member of it; the four
//     Rust `File` methods in that family are `seek`, `set_len`, `sync_all` and
//     `try_clone`.
//   * Directory enumeration. SERVED through `getattrlistbulk`(461), already
//     covered by two cases and guarded by two mutation probes. The
//     `getdirentries`(196) / `__getdirentries64`(344) fallback is kernel-bare
//     and is reached only by a FAILED 461, which serving 461 means never
//     entering.
//   * Metadata on an untouched base-layer object, which is served and already
//     covered.
//
// NO STDIO, DELIBERATELY, for `umbra-userspace-toy.c:21-27`'s reason: the
// verdict travels in the exit status alone. There is no `println!`, no
// `eprintln!`, and no `unwrap`/`expect`/`assert` anywhere below -- a panic would
// write a message and the message is the thing this fixture must not have. That
// matters more here than in any sibling: the measured disposition is that the
// process is KILLED mid-sequence, and a fixture that printed would leave output
// interleaved with the supervisor's own diagnostics on the one stream a reader
// uses to tell the two apart.
//
// TWO SENTINELS, because a run that ENDS produces no exit code and silence has
// to be told apart from "nothing ever ran". This is load-bearing here rather
// than defensive -- it is the ONLY evidence that the run was routing when it
// died, and without it every negative assertion in the harness would be
// satisfied just as well by a run that failed before it launched:
//
//   <leaf>.<case>live   written BEFORE the tree exists. Measured PRESENT in the
//                       shadow for both cases, holding the case name.
//   <leaf>.<case>done   written ONLY after stage (b) and the target read-back
//                       both succeeded. Measured ABSENT for both cases.
//
// Exit codes are this program's own, and the slice 160-168 was verified free by
// stripping comments from all thirteen sibling fixtures and `userspace_run.rs`
// and enumerating every integer literal in 10..255 -- thirteen rather than the
// twelve the design gate's audit said, because `umbra-test-child.c` is one of
// them and the audit's own enumeration listed it while still calling the set
// twelve -- `toy`/`listing`/
// `listing-project`/`test-child` hold 0-9, `edges` 70-87, `rustio` 90-107,
// `buffered` 110-114, `stdio` 120-129, `append` pair 130-134, `descriptor` pair
// 140-148, `nofollow` 150-158. `edges.c` also passes raw errnos through, and
// Darwin's `ELAST` is 107 on this host, so that passthrough cannot reach this
// slice either. Every code names one step:
//
//  160  wrong argument count, an unknown case name, or a <path> with no parent
//       directory to build the tree beside
//  161  the tree could not be built: the subtree `mkdir`, the target write, the
//       in-run symlink, the pre-sequence sentinel, or -- for the descriptor
//       form -- the parent directory open that precedes the link read
//  162  stage (a): the link read was refused with `ENOTSUP`(45)
//  163  stage (a): the link read failed with any OTHER errno
//  164  stage (b): the read succeeded but the bytes are not the link's target
//  165  the link's target did not survive the read unchanged -- its bytes
//       differ, or it is gone -- although reading a link is read-only
//
// THIS ALLOCATION WAS REVISED FROM THE ONE THE DESIGN GATE'S AUDIT PROPOSED,
// and the note matters because the audit's table is archived and a later slice
// may read it rather than this file. The audit's (F) sketch read `162` =
// stage (a) failed, `163` = stage (b) wrong bytes, `164` = mutated although a
// stage before it refused, with `165`/`166` reserved for a second syscall arm.
// Shipped instead: stage (a) splits by errno into `162`/`163`, stage (b)'s
// wrong-bytes code moves to `164`, and the read-only violation moves to `165`.
// The reason is the paragraph below -- the per-arm reservation buys nothing the
// argv does not already say, and the codes it frees buy the errno split, which
// cannot be recovered from outside the process. WHERE THE TWO DISAGREE, THIS
// FILE IS AUTHORITATIVE: it is the program that produces the codes. A harness
// assertion keyed to the audit's meaning of `164` would fire on wrong bytes
// rather than on a mutation.
//
// 166-168 are unallocated. The two arms SHARE one stage-code set rather than
// taking one each, following `umbra-userspace-nofollow.rs`, whose five cases
// share nine codes: the case name is already the discriminator, so a per-arm
// split would spend two codes to say what the argv the harness passed already
// says. The codes that buys are spent on splitting stage (a) BY ERRNO instead,
// which is the half that cannot be recovered from outside the process.
//
// 162 AND 165 ARE THE TWO THAT MAKE THIS NON-VACUOUS. Neither is reachable at
// this pin -- the run ends before any exit code is produced -- and both are
// here for the fix. 162 is what a refusal SOFTENED into #152's shape would
// produce, and keeping it apart from 163 is what makes "the refusal softened"
// distinguishable from "the refusal changed into a different defect". 165
// carries the read-only invariant: reading a link must not touch its target, so
// a case that refused AND changed the bytes is a far larger defect than the
// disposition the rest of this file pins, and no code that only checked the
// exit status would catch it. On a REFUSED read 165 means the bytes changed and
// nothing else -- a name that no longer resolves is not a mutation, because a
// refused sequence can leave one and reporting that as a changed object would
// be a wrong answer that looks like a right one. On a SUCCESSFUL read it also
// covers the target having vanished, because there a missing target is itself
// the contradiction: the call just reported the link resolves to it.
//
// This file is not a workspace member -- `experiments/` is outside `Cargo.toml`'s
// member list -- so `cargo fmt` and `clippy` never see it and it is hand-
// formatted. Build: `rustc --edition 2021 -O <this file> -o <bin>`.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

extern "C" {
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn symlink(target: *const u8, link: *const u8) -> i32;
    fn readlink(path: *const u8, buffer: *mut u8, size: usize) -> isize;
    fn readlinkat(dirfd: i32, path: *const u8, buffer: *mut u8, size: usize) -> isize;
    fn __error() -> *mut i32;
}

/// `errno`, read through libc's own accessor rather than a global.
fn errno() -> i32 {
    unsafe { *__error() }
}

// Flag values measured on this host (`<fcntl.h>`, Darwin arm64) rather than
// recalled. `O_SEARCH` is not an independent bit: it is `O_EXEC | O_DIRECTORY`,
// and it is the spelling `readlinkat`'s dirfd needs -- search on the directory,
// no read of it.
const O_DIRECTORY: i32 = 0x0010_0000;
const O_CLOEXEC: i32 = 0x0100_0000;
const O_EXEC: i32 = 0x4000_0000;
const O_SEARCH: i32 = O_EXEC | O_DIRECTORY;

/// The bytes the link's target holds, and which no case here may change.
const ORIGINAL: &[u8] = b"original bytes\n";

/// The link's target, as the string `readlink` must hand back.
///
/// Relative on purpose: the link and its target are siblings, so this resolves
/// the same way wherever the workspace is rooted, and the bytes a correct
/// `readlink` returns are exactly these and nothing longer.
const TARGET: &str = "target.txt";

/// The leaf name the symlink is created under, in every case.
const LINK: &str = "link";

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

/// Everything one case needs, built in-run.
struct Tree {
    /// The directory holding both the link and its target.
    parent: PathBuf,
    /// The symlink itself, named by whole path.
    link: PathBuf,
    /// The real file holding `ORIGINAL`, named WITHOUT going through the link,
    /// so a read-back proves the bytes rather than the link.
    verify: PathBuf,
}

/// Build this case's tree inside the routed run.
///
/// Every case gets its own subtree, so no case can see another's leftovers even
/// if the harness ever shares one workspace between two of them. `create_dir`
/// and the target write go through `std::fs` -- both are routed -- while the
/// symlink goes through `symlink`(57) directly, because
/// `std::os::unix::fs::symlink` is the same call and the direct spelling keeps
/// the errno readable.
///
/// **The tree is built IN-RUN and could not be seeded instead.** The supervisor
/// walks the approved workspace before the tracee exists and refuses any entry
/// that is not a regular file or a directory, so a host-seeded symlink stops the
/// run before it launches -- asserted by
/// `a_host_seeded_symlink_in_the_approved_workspace_stops_the_run_before_it_launches`.
/// In-run is the only construction this fixture has.
fn build(case: &str, root: &Path) -> Option<Tree> {
    let base = root.join(case);
    if std::fs::create_dir(&base).is_err() {
        return None;
    }
    let parent = base.join("parent");
    if std::fs::create_dir(&parent).is_err() {
        return None;
    }
    let verify = parent.join(TARGET);
    if std::fs::write(&verify, ORIGINAL).is_err() {
        return None;
    }
    let link = parent.join(LINK);
    if unsafe { symlink(cstr(Path::new(TARGET)).as_ptr(), cstr(&link).as_ptr()) } != 0 {
        return None;
    }
    Some(Tree {
        parent,
        link,
        verify,
    })
}

/// Has the link's target changed although reading the link refused?
///
/// A read error is NOT a mutation: a refused sequence can leave a name that does
/// not resolve, and reporting that as a mutated object would be a wrong answer
/// that looks like a right one. Only bytes that are present AND different count.
fn mutated(verify: &Path) -> bool {
    match std::fs::read(verify) {
        Ok(bytes) => bytes != ORIGINAL,
        Err(_) => false,
    }
}

/// `ENOTSUP` on this host, measured from `<sys/errno.h>` rather than recalled.
///
/// The number #152's refusal arm answers, and the one a softened `ReadLink`
/// refusal would most likely carry -- which is why stage (a) splits on it rather
/// than reporting "failed".
const ENOTSUP: i32 = 45;

/// Stage (a) and (b) for one case, given what the call reported.
///
/// `count` is the syscall's own return: negative is stage (a) failing, and any
/// non-negative value is a length into `buffer` that stage (b) then compares
/// against `TARGET`. The comparison is byte-exact and length-exact on purpose --
/// a short answer, a padded answer and an answer about a different object are
/// three different defects and all three are wrong here.
///
/// **Stage (a) splits on the errno and that is the non-vacuity gate.** "Refused"
/// is not the claim; "refused with THIS errno" is. A code that said only
/// "stage (a) failed" would be satisfied by every errno equally, so the day the
/// run-ending refusal softens into an answer the fixture could not say whether
/// it had softened into #152's `ENOTSUP` or into something else entirely -- and
/// those are different defects with different fixes.
fn verdict(count: isize, buffer: &[u8], tree: &Tree) -> i32 {
    if count < 0 {
        let failure = errno();
        // Stage (a). This is the arm a SOFTENED refusal would reach: an errno
        // answered to the tracee rather than an error that ends the run.
        if mutated(&tree.verify) {
            return 165;
        }
        return if failure == ENOTSUP { 162 } else { 163 };
    }
    let count = count as usize;
    if count > buffer.len() || &buffer[..count] != TARGET.as_bytes() {
        return if mutated(&tree.verify) { 165 } else { 164 };
    }
    // Reading a link is read-only on its target, so the check applies on the
    // success path too: the bytes must still be there and still be `ORIGINAL`.
    match std::fs::read(&tree.verify) {
        Ok(bytes) if bytes == ORIGINAL => 0,
        _ => 165,
    }
}

/// `readlink`(58) on the whole path of the shadow symlink.
fn case_readlink(tree: &Tree) -> i32 {
    let path = cstr(&tree.link);
    let mut buffer = [0u8; 256];
    let count = unsafe { readlink(path.as_ptr(), buffer.as_mut_ptr(), buffer.len()) };
    verdict(count, &buffer, tree)
}

/// `readlinkat`(473) on the link's name, relative to a descriptor for its parent.
///
/// The other decoder arm, and the reason this fixture declares its calls rather
/// than using `std`: `std::fs::read_link` reaches `readlink`(58), but `std`
/// offers no `readlinkat` at all, so 473 has no Rust call site without this
/// declaration. `O_SEARCH` rather than `O_RDONLY` because search is all a dirfd
/// operand needs, and because it is the spelling the walk fixture measured as
/// served.
fn case_readlinkat(tree: &Tree) -> i32 {
    let dirfd = unsafe { open(cstr(&tree.parent).as_ptr(), O_SEARCH | O_CLOEXEC) };
    if dirfd < 0 {
        // The directory open is not this fixture's subject -- it is the walk
        // fixture's measured stage (a) -- so its failure reports as the tree
        // not having been buildable rather than as a `readlinkat` verdict.
        return 161;
    }
    let name = cname(LINK);
    let mut buffer = [0u8; 256];
    let count = unsafe { readlinkat(dirfd, name.as_ptr(), buffer.as_mut_ptr(), buffer.len()) };
    // The errno belongs to `readlinkat`, so the `close` must not run before the
    // verdict is computed from it.
    let code = verdict(count, &buffer, tree);
    unsafe { close(dirfd) };
    code
}

/// One case, end to end.
fn case(case: &str, destination: &Path, root: &Path) -> i32 {
    // The pre-sequence sentinel. It is written FIRST, before any tree exists,
    // because its whole job is to prove this run was routing at all -- and a run
    // that an internal error ends never gets to write anything later.
    if std::fs::write(sentinel(destination, &format!(".{case}live")), case.as_bytes()).is_err() {
        return 161;
    }
    let tree = match build(case, root) {
        Some(tree) => tree,
        None => return 161,
    };
    let code = match case {
        "readlink-shadow" => case_readlink(&tree),
        "readlinkat-shadow" => case_readlinkat(&tree),
        _ => return 160,
    };
    if code != 0 {
        return code;
    }
    if std::fs::write(sentinel(destination, &format!(".{case}done")), case.as_bytes()).is_err() {
        return 161;
    }
    0
}

fn dispatch() -> i32 {
    let mut arguments = std::env::args_os().skip(1);
    let name = match arguments.next() {
        Some(name) => name,
        None => return 160,
    };
    let destination = match arguments.next() {
        Some(destination) => PathBuf::from(destination),
        None => return 160,
    };
    if arguments.next().is_some() {
        return 160;
    }
    let root = match destination.parent() {
        Some(root) => root.to_path_buf(),
        None => return 160,
    };
    let name = name.to_string_lossy().into_owned();
    if name != "readlink-shadow" && name != "readlinkat-shadow" {
        return 160;
    }
    case(&name, &destination, &root)
}

fn main() {
    // `dispatch` has returned, so every handle it opened is closed -- which
    // matters, because the close is itself a routed call this fixture crosses.
    // `process::exit` runs no destructors, so it must come after.
    std::process::exit(dispatch());
}
