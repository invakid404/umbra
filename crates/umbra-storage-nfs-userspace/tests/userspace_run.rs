//! `umbra run` over the userspace NFSv4 client, end to end -- the toy program,
//! the edge and directory-listing fixtures, the Rust library-wrapper fixture,
//! the buffered-output pair, the three standard utilities, and the seven
//! mutation probes that prove the routing is load-bearing.
//!
//! **Six kinds of fixture, and two of them are not C.** Four of them --
//! `umbra-userspace-toy.c`, `-edges.c`, `-listing.c`, and the Apple binaries the
//! utility matrix launches -- reach the filesystem through bare syscalls or
//! through libc, so every transfer they make is one they sized themselves.
//! `umbra-userspace-rustio.rs` reaches it through `std::fs`'s own wrappers,
//! which take a capacity hint from a descriptor and complete their own short I/O
//! *inside the library*. `umbra-userspace-stdio.c` and
//! `umbra-userspace-buffered.rs` are a **pair**, and they exist for one
//! comparison: the first writes through C stdio, whose flush reaches the kernel
//! as `__write_nocancel`(397) -- a call with **no `TRACED_STUBS` row**, issued
//! from inside the dyld shared cache where interposition cannot rebind it -- and
//! the second writes through a Rust `BufWriter`, whose final `write` is compiled
//! into the executable where interposition does reach it. Breakpoints are not
//! the limitation: `fopen`'s 398 and `fclose`'s 399 fire from inside libsystem
//! on this very path, which is why the object is created and released cleanly
//! and only its bytes are lost. Both Rust fixtures are built by bare `rustc`
//! rather than cargo, because `experiments/` is not a workspace member; see
//! `rust_fixture_binary`.
//!
//! **Two cases here assert a defect rather than a fix, and they say so.** The
//! stdio fixture's `checked` and `ignored` cases characterize
//! [#127](https://github.com/invakid404/umbra/issues/127): buffered output to a
//! routed descriptor persists *nothing*, while the object itself is created and
//! released perfectly cleanly -- and the `ignored` shape does it at exit 0.
//! Neither is `#[ignore]`d. No step of this job passes `--include-ignored`, so
//! an ignored case would report green without ever running, which is the
//! skip-as-pass shape those two cases exist to catch. They assert today's wrong
//! answer positively instead; the fix to #127 turns both of them red on
//! purpose, and each one's doc comment says what to change when it does.
//!
//! # What these cases assert, and why here
//!
//! A supervised run whose storage is this provider has no kernel-visible path
//! anywhere: the tracee's `open`, `read`, `write` and `close` are serviced by
//! umbra through the overlay and this client, not by rewriting a syscall's path
//! operand. That is the whole behaviour change, and nothing below infers it --
//! every assertion is made against something observable outside the run.
//!
//! They live in this crate rather than beside `run_fixtures.rs` because the
//! read-back has to go **through the client**: the point of a userspace NFSv4
//! backend is that nothing is mounted, so "the bytes are in the export" can only
//! honestly be checked by speaking NFSv4 to the server. That needs
//! `transport-raw`, which is this crate's feature and is deliberately off in a
//! default build.
//!
//! # Running them
//!
//! **Eight separate invocations**, because the probes are compile-time and no
//! two may be enabled at once. Always rebuild the unmutated binaries
//! afterwards.
//!
//! ```text
//! # end to end: the toy and the Rust I/O fixture must exit 0, and the utility
//! # matrix must pass
//! cargo build --workspace --bins
//! cargo test -p umbra-storage-nfs-userspace --features transport-raw --test userspace_run
//!
//! # probe A -- read routing broken; the toy must exit 8
//! cargo build -p umbra-cli --features mutation-probe-read
//! UMBRA_MUTATION_PROBE=read cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe B -- write routing broken; the toy must exit 7, and the raw and
//! #            BufWriter writers must keep exit 0 while their objects go empty
//! cargo build -p umbra-cli --features mutation-probe-write
//! UMBRA_MUTATION_PROBE=write cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe C -- fstat answers EBADF; `touch <absent>` must fail with its
//! #            empty file still in the export
//! cargo build -p umbra-cli --features mutation-probe-fstat
//! UMBRA_MUTATION_PROBE=fstat cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe D -- the mkdir(136) decode arm removed; the directory must appear
//! #            nowhere while `touch <absent>` still exits 0
//! # NOTE the crate: this probe is NOT on umbra-cli.
//! cargo build -p umbra-platform-macos --features mutation-probe-mkdir
//! UMBRA_MUTATION_PROBE=mkdir cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe E -- setattrlistat refused; `touch <existing>` must fail while
//! #            `touch <absent>` still exits 0
//! cargo build -p umbra-platform-macos --features mutation-probe-setattrlistat
//! UMBRA_MUTATION_PROBE=setattrlistat cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe F -- each directory entry's name reversed; the run must still exit 0
//! #            while the names read back are wrong
//! cargo build -p umbra-cli --features mutation-probe-readdir
//! UMBRA_MUTATION_PROBE=readdir cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe G -- a closed descriptor's residual directory page is never
//! #            evicted; the second enumeration of a re-opened directory must
//! #            lose the names created since the first
//! cargo build -p umbra-cli --features mutation-probe-dircache
//! UMBRA_MUTATION_PROBE=dircache cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//! ```
//!
//! **The probes are not all on the same crate, and the build command differs
//! because of it.** A, B, C and G are features of `umbra-overlay`, and F is one
//! of `umbra-supervisor`; all five are forwarded to `umbra-cli`, so they go
//! into the `umbra`
//! binary. D and E are features of `umbra-platform-macos`, which is a provider
//! *executable* the registry names rather than a library `umbra` links -- so
//! they go into that binary, and `cargo build -p umbra-cli --features
//! mutation-probe-mkdir` is not merely useless, it does not exist as a feature
//! there. Each probe's `#[test]` doc names the crate to build.
//!
//! `UMBRA_MUTATION_PROBE` selects which case runs; it does **not** enable a
//! probe. A mismatch between the variable and the binaries fails the case
//! rather than passing it, and each probe carries its own reason why: A and B
//! assert an exact exit code from the toy, which only that mutation produces;
//! C, D and E assert that the utility they target exits **nonzero** -- Apple's
//! binaries exit 1 on any failure, so an exact code is not available -- paired
//! with a positive assertion that a *different* utility still succeeds and
//! still reaches the store. That pair is what makes them non-vacuous: probe D
//! originally had only the negative half and passed against a tree whose
//! tracer died before `main` for an unrelated reason.
#![cfg(all(
    feature = "transport-raw",
    target_os = "macos",
    target_arch = "aarch64"
))]

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use umbra_core::{
    BlobStat, BytePath, DirectoryEntry, JournalAccess, JournalControlBinding, JournalFormatPolicy,
    JournalLifecycle, JournalOpenRequest, JournalPayload, ObjectId, ObjectKind, PhysicalPath,
    RunId, Sequence,
};
use umbra_journal::Journal;
use umbra_journal_file::FileJournal;
// The *production* `getattrlistbulk` record encoder, as a dev-dependency. The
// pagination assertion calls it rather than transcribing its five-line record
// formula, so the case cannot quietly stop paginating when `dirents.rs`
// changes: a transcribed formula that drifts from the encoder makes the
// assertion pass while meaning nothing.
use umbra_platform::dirents;
use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, Nfs4Type, RawTransport,
};

/// Bytes the toy writes and reads back.
///
/// Must match `umbra-userspace-toy.c`, `umbra-userspace-stdio.c` and
/// `umbra-userspace-buffered.rs`. The last two share it with the toy on
/// purpose rather than by accident: the buffered-output comparison is only
/// exact if every case persisted the same payload, so one constant here is
/// what makes "the same bytes in every case" a property of the suite instead
/// of a convention nothing checks. It must also stay below 4096 bytes -- see
/// `umbra-userspace-stdio.c`'s header for the measured reason.
const PAYLOAD: &[u8] = b"umbra-userspace-nfs\n";

/// The export and run parent the fixture carries, matching the other live suites.
const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

/// Two-tier fixture input, in the established style: skipped by default, and a
/// hard failure when `UMBRA_INTEGRATION_REQUIRED` says the environment provides
/// it.
fn input(name: &str) -> Option<OsString> {
    let value = std::env::var_os(name).filter(|value| !value.is_empty());
    assert!(
        value.is_some() || std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
        "required integration needs {name}"
    );
    if value.is_none() {
        eprintln!("SKIP: set {name}");
    }
    value
}

/// Which mutation probe the `umbra` binary under test was built with, as the
/// caller declares it. `None` means an unmutated binary.
fn declared_probe() -> Option<String> {
    std::env::var("UMBRA_MUTATION_PROBE")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Where the built binaries are.
///
/// A test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so its
/// grandparent is the directory holding `umbra` and every provider executable.
/// `UMBRA_BIN_DIR` overrides it for a layout this does not describe.
fn binaries() -> PathBuf {
    if let Some(explicit) = std::env::var_os("UMBRA_BIN_DIR") {
        return PathBuf::from(explicit);
    }
    let test = std::env::current_exe().expect("this test binary's own path");
    test.parent()
        .and_then(Path::parent)
        .expect("a test binary lives under <target>/<profile>/deps")
        .to_path_buf()
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// `host:port` of the live Ganesha fixture.
fn fixture() -> Option<(String, u16)> {
    let target = input("UMBRA_NFS_RAW_FIXTURE")?;
    let target = target.to_string_lossy().into_owned();
    let (host, port) = target
        .rsplit_once(':')
        .expect("UMBRA_NFS_RAW_FIXTURE=host:port");
    Some((host.to_owned(), port.parse().expect("numeric port")))
}

/// The compiled toy. Built by CI beside the other C fixture; compiled here when
/// the environment did not provide one, so a local run needs no extra step.
fn toy(scratch: &Path) -> PathBuf {
    fixture_binary(scratch, "umbra-userspace-toy", "UMBRA_USERSPACE_TOY_PATH")
}

/// The compiled directory-listing fixture, which reads a directory through
/// `fts` exactly as `/bin/ls` does and writes the names it got into a file.
fn listing_fixture(scratch: &Path) -> PathBuf {
    fixture_binary(
        scratch,
        "umbra-userspace-listing",
        "UMBRA_USERSPACE_LISTING_PATH",
    )
}

/// The compiled project-tree listing fixture: the control's sibling, which
/// enumerates a directory **twice** over one reused descriptor number, and
/// records the number each enumeration started on. It writes nothing into the
/// directory it reads -- see `project_listing_run` for why that matters.
///
/// A separate `.c` file rather than an argv mode inside
/// `umbra-userspace-listing.c`, deliberately. The established pattern for a
/// multi-shape C fixture here is argv dispatch -- `umbra-userspace-edges.c`
/// carries about twenty `case_*` functions behind a `strcmp` -- and it would
/// have saved the ~15 duplicated lines of `emit` and `fts` boilerplate. The
/// design gate declined it anyway: the listing fixture is the *control* for
/// everything in this area, and the cheapest way to keep it a control is for
/// its file to stay byte-identical.
fn project_listing_fixture(scratch: &Path) -> PathBuf {
    fixture_binary(
        scratch,
        "umbra-userspace-listing-project",
        "UMBRA_USERSPACE_LISTING_PROJECT_PATH",
    )
}

/// Run the listing fixture over a routed workspace holding `names`, and return
/// the run together with the host path of the file it was told to write.
///
/// The directory being listed is a *subdirectory*, and the output file is at the
/// workspace root, deliberately: writing the output into the directory under
/// enumeration would race the enumeration itself, and whether the new name
/// appeared would depend on when the shadow observed it.
fn listing_run(scratch: &Path, host: &str, port: u16, names: &[&str]) -> (Run, PathBuf) {
    let workspace = scratch.join("listing-workspace");
    let state = scratch.join("listing-state");
    let entries = workspace.join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
    for name in names {
        std::fs::write(entries.join(name), b"x").unwrap();
    }
    let workspace = workspace.canonicalize().unwrap();
    let entries = workspace.join("entries");
    let destination = workspace.join("listing.txt");
    let registry = registry(scratch, host, port);
    let program = listing_fixture(scratch);

    let output = Command::new(binaries().join("umbra"))
        .args(["run", "--registry"])
        .arg(&registry)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--experimental")
        .arg("--")
        .arg(&program)
        .arg(&entries)
        .arg(&destination)
        .stdin(Stdio::null())
        .output()
        .expect("umbra run");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let run_id = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("umbra: run ")
                .and_then(|line| line.strip_suffix(" prepared"))
        })
        .unwrap_or_else(|| panic!("no prepared run id in:\n{stderr}"));
    let run = Run {
        run_id: RunId(uuid::Uuid::parse_str(run_id).unwrap()),
        status: output.status.code(),
        stderr,
        destination: destination.clone(),
        state,
        workspace,
        registry,
    };
    (run, destination)
}

/// The names the listing fixture wrote, read back out of the store through the
/// userspace client, sorted so the assertion does not depend on enumeration
/// order.
fn listed_names(host: &str, port: u16, run: &Run, destination: &Path) -> Vec<String> {
    let stored = read_through_client(host, port, &utility_shadow(run, destination))
        .expect("the listing fixture's output file is not in the export");
    let mut names: Vec<String> = String::from_utf8(stored)
        .expect("the fixture writes names, and this fixture's names are UTF-8")
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// Refuse any change the listing run could have made to its host workspace.
///
/// `assert_workspace_pristine` cannot be reused: it pins the seeded workspace to
/// exactly `seed.txt`, and this one legitimately seeds a directory to enumerate
/// as well. The property is the same and is asserted at both levels -- the
/// workspace root holds what it was seeded with and nothing the run wrote, and
/// the enumerated directory still holds exactly the names it was given, so the
/// run neither created the listing file on the host nor disturbed what it read.
fn assert_listing_workspace_pristine(run: &Run, names: &[&str]) {
    let read = |path: &Path| -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    };
    assert_eq!(
        read(&run.workspace),
        ["entries", "seed.txt"],
        "the listing run changed its host workspace"
    );
    assert_eq!(
        read(&run.workspace.join("entries")),
        names,
        "the listing run changed the host directory it enumerated"
    );
}

/// The project-shaped core of the two-pass tree, seeded on the host as base
/// entries. Nine names, chosen for *record shapes* the control's three flat
/// ASCII files cannot produce: two hidden, four containing a space, one 2-byte
/// and one 3-byte UTF-8 name, and two directories -- whose records are 4 bytes
/// shorter than a file's, because a directory entry omits
/// `ATTR_FILE_LINKCOUNT` (`umbra-platform/src/dirents.rs:275-277`).
const PROJECT_CORE_FILES: &[&str] = &[
    ".gitignore",
    ".hidden config",
    "Cargo.toml",
    "README copy.md",
    "naïve café.txt",
    "two words.rs",
    "日本語ファイル.txt",
];

/// The two multibyte names in the core, called out because the assertions need
/// them by name: a reversed multibyte name is what makes `listed_lines` return
/// bytes, and both of these must land beyond the first reply for the
/// residual-page coverage to mean anything.
const PROJECT_CORE_UTF8_NAMES: &[&str] = &["naïve café.txt", "日本語ファイル.txt"];

/// Seeded, and deliberately never entered. The fixture keeps
/// `fts_set(FTS_SKIP)`; these exist for their directory *records*, not to be
/// descended. Recursive traversal is explicitly not claimed by this slice
/// (`README.md:688`) and nothing here changes that.
const PROJECT_CORE_DIRECTORIES: &[&str] = &["src", "test fixtures"];

/// Pagination ballast, and named as ballast rather than dressed up as project
/// files.
///
/// **Being honest about why it is here.** No realistic small project tree
/// reaches 32 KiB of encoded records -- that is roughly 200 entries at
/// 100-byte names, or 455 at 14 -- and pretending otherwise by inventing two
/// hundred plausible-looking source files would make the fixture lie about what
/// it is. The core above carries the semantic coverage; this carries the bytes.
///
/// 230 at a 100-byte name is the measured choice. 210 entries is the bare
/// minimum that clears the reply buffer, by 8 bytes, which is too fragile to be
/// worth having; 239 clears it by 4 648 (14 %). A 14-byte name would need 456
/// entries and roughly double the round trips `Overlay::merged` makes.
const PROJECT_BALLAST: usize = 230;

/// Bytes in each ballast name. Encoded record: `align8(48 + 4 + 100 + 1)` = 160.
const PROJECT_BALLAST_BYTES: usize = 100;

/// The `getattrlistbulk` buffer `fts` passes, every call.
///
/// Measured on this host rather than assumed: lldb on `getattrlistbulk` with a
/// probe using the shipped fixture's flags reads `x3 = 0x8000` under
/// `fts_build` -> `advance_directory`, and `x3` is the register
/// `umbra-platform-macos/src/abi.rs:559` reads as `max_bytes`. 32768 is far
/// below that path's 1 MiB clamp, so it arrives unmodified.
const FTS_REPLY_BYTES: u32 = 32768;

/// The ballast names, in the order the fixture's seeding produces them.
fn project_ballast_names() -> Vec<String> {
    (0..PROJECT_BALLAST)
        .map(|index| {
            let mut name = format!("ballast-{index:04}-");
            while name.len() < PROJECT_BALLAST_BYTES {
                name.push('p');
            }
            assert_eq!(
                name.len(),
                PROJECT_BALLAST_BYTES,
                "a ballast name must be exactly {PROJECT_BALLAST_BYTES} bytes or the measured \
                 pagination threshold no longer describes the tree"
            );
            name
        })
        .collect()
}

/// Every name **each** pass must report: the core, the directories and the
/// ballast. Both passes enumerate the same tree and nothing writes into it, so
/// one expectation serves both -- and pass 2 matching it *is* the eviction
/// proof, because a surviving residual page would answer the empty remainder
/// pass 1 left instead.
fn project_tree_names() -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = PROJECT_CORE_FILES
        .iter()
        .chain(PROJECT_CORE_DIRECTORIES.iter())
        .map(|name| name.as_bytes().to_vec())
        .chain(project_ballast_names().into_iter().map(String::into_bytes))
        .collect();
    names.sort();
    names
}

/// The tree as the production encoder sees it: byte-sorted `DirectoryEntry`
/// values, which is the order `Overlay::merged` returns and therefore the order
/// the reply pages are packed in.
///
/// Only `name` and `stat.kind` affect a record's size, which is what the
/// assertions here are about; the rest is filled with values a real entry could
/// carry.
fn project_directory_entries(names: &[Vec<u8>]) -> Vec<DirectoryEntry> {
    let directories: Vec<&[u8]> = PROJECT_CORE_DIRECTORIES
        .iter()
        .map(|name| name.as_bytes())
        .collect();
    names
        .iter()
        .enumerate()
        .map(|(index, name)| DirectoryEntry {
            name: BytePath::new(name.clone()).expect("a seeded name is a nonempty NUL-free byte"),
            stat: BlobStat {
                object_id: ObjectId(uuid::Uuid::from_u128(index as u128 + 1)),
                kind: if directories.contains(&name.as_slice()) {
                    ObjectKind::Directory
                } else {
                    ObjectKind::File
                },
                len: 1,
                link_count: 1,
                mode: 0o644,
                uid: 0,
                gid: 0,
                modified_nanos: 0,
            },
        })
        .collect()
}

/// Run the project-tree listing fixture over a routed workspace and return the
/// run together with the host path of the file it was told to write.
///
/// A sibling of `listing_run` rather than a generalisation of it. That helper is
/// shared by the control case and by the `readdir` probe case, and widening its
/// signature would put this change inside the control's own code path -- which
/// is the one thing the design gate held fixed.
///
/// The output file goes at the workspace root for `listing_run`'s documented
/// reason, which still holds -- and here it carries a second reason that is
/// load-bearing rather than tidy. **Nothing in this case writes into the
/// enumerated directory, and that is a precondition, not an accident.** A write
/// there materialises the directory into the shadow, and a routed `stat` by path
/// on a shadow object is refused `ENOTSUP` at `engine.rs:3020-3023` -- a
/// limitation the comment above it defers to a later slice. `fts` stats a root
/// entry before walking it, so pass 2 would report nothing for a reason that has
/// nothing to do with the directory cache this case exists to exercise.
///
/// **So the base-plus-shadow merge is deliberately NOT claimed here.** An earlier
/// draft created two files between the passes to prove it and measured exactly
/// that refusal; the claim moved to the slice that lifts the `Stat` limitation.
/// An edit that reintroduces a write into the enumerated directory -- including
/// moving the output file there -- silently returns this case to that path.
fn project_listing_run(scratch: &Path, host: &str, port: u16) -> (Run, PathBuf) {
    let workspace = scratch.join("project-listing-workspace");
    let state = scratch.join("project-listing-state");
    let entries = workspace.join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
    for name in PROJECT_CORE_FILES {
        std::fs::write(entries.join(name), b"x").unwrap();
    }
    for name in PROJECT_CORE_DIRECTORIES {
        std::fs::create_dir(entries.join(name)).unwrap();
    }
    for name in project_ballast_names() {
        std::fs::write(entries.join(&name), b"x").unwrap();
    }
    let workspace = workspace.canonicalize().unwrap();
    let entries = workspace.join("entries");
    let destination = workspace.join("listing.txt");
    let registry = registry(scratch, host, port);
    let program = project_listing_fixture(scratch);

    let output = Command::new(binaries().join("umbra"))
        .args(["run", "--registry"])
        .arg(&registry)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--experimental")
        .arg("--")
        .arg(&program)
        .arg(&entries)
        .arg(&destination)
        .stdin(Stdio::null())
        .output()
        .expect("umbra run");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let run_id = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("umbra: run ")
                .and_then(|line| line.strip_suffix(" prepared"))
        })
        .unwrap_or_else(|| panic!("no prepared run id in:\n{stderr}"));
    let run = Run {
        run_id: RunId(uuid::Uuid::parse_str(run_id).unwrap()),
        status: output.status.code(),
        stderr,
        destination: destination.clone(),
        state,
        workspace,
        registry,
    };
    (run, destination)
}

/// The fixture's output read back through the client as **bytes**, one entry
/// per line, in the order the fixture wrote them.
///
/// Two differences from `listed_names`, both mandatory rather than stylistic:
///
/// **Bytes, not `String`.** `listed_names` unwraps `String::from_utf8`. This
/// tree carries multibyte UTF-8 names, and the `readdir` probe byte-reverses
/// each name (`umbra-supervisor/src/directory.rs:145-156`) -- byte-reversing
/// multibyte UTF-8 yields invalid UTF-8, so the probe case would panic inside
/// the helper instead of asserting.
///
/// **Unsorted.** `listed_names` sorts so the assertion does not depend on
/// enumeration order, which is right for a one-pass case. Here the order is
/// what separates pass 1 from pass 2, so sorting happens later, per pass.
fn listed_lines(host: &str, port: u16, run: &Run, destination: &Path) -> Vec<Vec<u8>> {
    let stored = read_whole_object_through_client(host, port, &utility_shadow(run, destination))
        .expect("the project listing fixture's output file is not in the export");
    stored
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// One object's **whole** contents, through the client, however many `READ`s
/// that takes.
///
/// A third sibling of `read_through_client` and `stored_size_through_client`,
/// and it exists because neither of those fits this case. The first caps its
/// `READ` at 4 KiB, which is right for the small payloads it checks and silently
/// wrong here: this fixture's output is about 48 KiB, so that helper returns the
/// first page and a listing assertion built on it would compare 45 lines against
/// 480 and fail for a reason that has nothing to do with the directory read.
/// Measured exactly that way before this helper existed. The second answers the
/// size without the bytes, which is what the 2 MiB case wants and not what a
/// line-by-line assertion can use.
///
/// `read_through_client` is deliberately left alone rather than generalised: it
/// is shared with the control case and with four others, and the 4 KiB cap is
/// load-bearing for the 2 MiB payload they have to not pull back.
fn read_whole_object_through_client(
    host: &str,
    port: u16,
    components: &[Vec<u8>],
) -> Option<Vec<u8>> {
    let mut config = RawTransportConfig::loopback(port);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 10_000 };
    let deadline = config.limits.default_deadline;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let mut handle = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    let mut attributes = None;
    for component in components {
        let name = ComponentName::new(component.clone()).expect("component");
        let (next, found) = transport
            .lookup(&handle, &name, AttrMask::STAT, deadline)
            .ok()?;
        handle = next;
        attributes = Some(found);
    }
    let attributes = attributes.expect("at least one component");
    assert_eq!(
        attributes.file_type,
        Some(Nfs4Type::Regular),
        "the shadow object is not a regular file"
    );
    let length = attributes.size.unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(length as usize);
    // A short `READ` reply is a real answer rather than an error, so this reads
    // until the server reports EOF or stops making progress -- not until one
    // call happens to return everything.
    while (bytes.len() as u64) < length {
        let want = (length - bytes.len() as u64).min(4096) as u32;
        let reply = transport
            .read(
                &handle,
                umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
                bytes.len() as u64,
                want,
                deadline,
            )
            .expect("READ with the anonymous stateid");
        let empty = reply.data.is_empty();
        bytes.extend_from_slice(&reply.data);
        if reply.eof || empty {
            break;
        }
    }
    assert_eq!(
        bytes.len() as u64,
        length,
        "the export reports a {length}-byte object but served {} bytes",
        bytes.len()
    );
    Some(bytes)
}

/// Split the fixture's output into its two passes.
///
/// The fixture emits `probe-fd=<n>` immediately before each `fts_open`, so that
/// line both carries the descriptor observation and delimits the pass that
/// follows it. Returns the recorded descriptor number and the sorted names, per
/// pass, and insists there are exactly two.
fn project_passes(lines: &[Vec<u8>]) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut passes: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    for line in lines {
        if let Some(number) = line.strip_prefix(b"probe-fd=".as_slice()) {
            passes.push((number.to_vec(), Vec::new()));
            continue;
        }
        passes
            .last_mut()
            .expect("the fixture emits probe-fd=<n> before the first name it reports")
            .1
            .push(line.clone());
    }
    assert_eq!(
        passes.len(),
        2,
        "the fixture enumerates twice and marks each pass with probe-fd=<n>; \
         got {} marker(s)",
        passes.len()
    );
    for pass in &mut passes {
        pass.1.sort();
    }
    passes
}

/// Refuse any change the project listing run could have made to its host
/// workspace.
///
/// `assert_listing_workspace_pristine` cannot be reused: it is shared with the
/// control case and takes the enumerated directory's exact contents as a
/// `&[&str]`, and this tree's contents are 239 generated names. The property
/// asserted is the same one, at both levels -- the workspace root holds what it
/// was seeded with, and the enumerated directory still holds exactly the tree it
/// was given, so the run neither created the listing file on the host nor
/// disturbed what it read.
///
/// It reads the host with `std::fs::read_dir`, as the sibling does, and that is
/// inside the guardrail: the rule the design gate ratified is no `read_dir` in
/// the *traced fixture*. This inspects host state, which is the one thing the
/// client cannot be asked about.
fn assert_project_workspace_pristine(run: &Run) {
    let read = |path: &Path| -> Vec<Vec<u8>> {
        let mut found: Vec<Vec<u8>> = std::fs::read_dir(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
            .map(|entry| entry.unwrap().file_name().as_bytes().to_vec())
            .collect();
        found.sort();
        found
    };
    assert_eq!(
        read(&run.workspace),
        vec![b"entries".to_vec(), b"seed.txt".to_vec()],
        "the project listing run changed its host workspace"
    );
    assert_eq!(
        read(&run.workspace.join("entries")),
        project_tree_names(),
        "the project listing run changed the host directory it enumerated"
    );
}

/// One compiled C fixture from `experiments/fixtures/`.
fn fixture_binary(scratch: &Path, name: &str, variable: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os(variable) {
        let path = PathBuf::from(explicit);
        assert!(path.is_file(), "{variable}: {}", path.display());
        return path;
    }
    let source = repository().join(format!("experiments/fixtures/{name}.c"));
    let binary = scratch.join(name);
    let built = Command::new("clang")
        .args(["-arch", "arm64", "-O1"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("clang is part of the Command Line Tools this host already requires");
    assert!(
        built.status.success(),
        "compiling {}: {}",
        source.display(),
        String::from_utf8_lossy(&built.stderr)
    );
    binary
}

/// One compiled Rust fixture from `experiments/fixtures/`.
///
/// The sibling of `fixture_binary`, and a separate function rather than that one
/// parameterised by compiler: the source extension, the flags, and the reason
/// the file exists at all are different in each, and a `match` on a language tag
/// inside one function would be longer than the two functions are.
///
/// `rustc` is no new host requirement. `rust-toolchain.toml` pins the toolchain
/// the whole workspace is already built with, and CI builds the workspace before
/// every proof step in this job. No `--target` is passed, for the reason
/// `fixture_binary` passes `-arch arm64`: this file only compiles on
/// `aarch64-apple-darwin` and the host toolchain is native to it.
fn rust_fixture_binary(scratch: &Path, name: &str, variable: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os(variable) {
        let path = PathBuf::from(explicit);
        assert!(path.is_file(), "{variable}: {}", path.display());
        return path;
    }
    let source = repository().join(format!("experiments/fixtures/{name}.rs"));
    let binary = scratch.join(name);
    let built = Command::new("rustc")
        .args(["--edition", "2021", "-O"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("rustc is the toolchain this workspace is already built with");
    assert!(
        built.status.success(),
        "compiling {}: {}",
        source.display(),
        String::from_utf8_lossy(&built.stderr)
    );
    binary
}

/// A registry naming the built provider executables and this fixture's export.
///
/// `timeout_ms` is the authoritative per-request IPC deadline and it is bounded
/// from above by the writer lease: `check_renewal_budget` requires the renewal
/// interval plus this deadline to fit inside the 30 s lease, so anything at or
/// above 15 s is refused before the run starts. 12 s leaves the slowest live
/// COMPOUND ample headroom under that ceiling.
fn registry(scratch: &Path, host: &str, port: u16) -> PathBuf {
    let repository = repository();
    let binaries = binaries();
    let bytes =
        |path: &Path| -> serde_json::Value { serde_json::json!(path.as_os_str().as_bytes()) };
    let descriptor = |package: &str, capabilities: &[&str], options: Vec<u8>| {
        let mut value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                repository
                    .join("crates")
                    .join(package)
                    .join("provider.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let executable = binaries.join(package);
        assert!(
            executable.is_file(),
            "build the workspace binaries first: {}",
            executable.display()
        );
        value["executable"] = bytes(&executable);
        value["capabilities"] = serde_json::json!(capabilities);
        value["options"] = serde_json::json!(options);
        value
    };
    let configuration = serde_json::json!({
        "host": host.as_bytes(),
        "port": port,
        "export": EXPORT,
        "run_parent": RUN_PARENT,
        "root_anchor": b"root",
        "control_anchor": b"control",
        "deadline": {"millis": 10_000},
    });
    let registry = serde_json::json!({"timeout_ms": 12_000, "providers": {
        "platform": descriptor(
            "umbra-platform-macos",
            &[
                "sandboxed-stopped-launch-v1",
                "experimental-syscall-rewrite-v1",
                "experimental-userspace-interpose-v1",
            ],
            vec![],
        ),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor(
            "umbra-storage-nfs-userspace",
            &["userspace-nfsv4-v1", "experimental-userspace-routing-v1"],
            serde_json::to_vec(&configuration).unwrap(),
        ),
    }});
    let path = scratch.join("registry.json");
    std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
    path
}

/// One completed `umbra run` and everything an assertion needs from it.
struct Run {
    run_id: RunId,
    status: Option<i32>,
    stderr: String,
    /// Host path the toy names, which must never be created.
    destination: PathBuf,
    /// The run's per-run host state root, holding the journal and the tracee's
    /// one host write allowance.
    state: PathBuf,
    workspace: PathBuf,
    registry: PathBuf,
}

/// Launch the toy under `umbra run` against the live fixture.
fn routed_run(scratch: &Path, host: &str, port: u16) -> Run {
    let toy = toy(scratch);
    launch(scratch, host, port, &toy, &[], &[])
}

/// The edge fixture again, copied to a basename of its own, for the cases whose
/// tracee `exec`s a **different image**.
///
/// The copy is what makes it different, and the mechanism is worth stating
/// because the case is worthless without it. umbra runs every tracee as a
/// resigned twin cached at `<sha256 of contents>/<basename>`, and the
/// interposer used to be armed only in the one twin path umbra named at launch.
/// A copy with the same basename resigns to *that same path* -- identical
/// contents, identical digest, identical name -- and would have been armed for
/// the wrong reason, reporting a pass against the defect it exists to catch. A
/// different basename resigns to a sibling path inside the same digest folder,
/// which is a different image to umbra and the same program to the fixture.
fn exec_helper(scratch: &Path) -> PathBuf {
    let source = fixture_binary(scratch, "umbra-userspace-edges", "UMBRA_EDGES_PATH");
    let helper = scratch.join("umbra-edge-exec-helper");
    std::fs::copy(&source, &helper).expect("copying the edge fixture to a helper basename");
    let mut mode = std::fs::metadata(&helper).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&helper, mode).unwrap();
    helper
}

/// An image umbra will resign but the kernel will refuse to execute, for the
/// failed-`exec` case.
///
/// Both properties are load-bearing and the shape that has them is narrow.
/// umbra resigns every image a tracee execs *with its own entitlements* and
/// verifies they read back, before rewriting the operand to name the twin -- so
/// the file has to survive that. A dylib does not: it signs, but `codesign -d
/// --entitlements` reports nothing, and `cache::resign` fails
/// `missing twin entitlement` before the exec is reached, which is a different
/// (and safe) outcome that would not exercise R1 at all. Measured, not assumed.
///
/// What does work is an ordinary arm64 executable with its **execute bits
/// cleared**. `codesign` cares about the content, so signing and strict
/// verification both pass and the entitlements read back; `fs::copy` preserves
/// the mode into the twin, so the rewritten exec names an unexecutable file
/// too. Measured: `execv` on it fails `EACCES`(13).
///
/// That pair is exactly the state R1 is about: umbra has resigned a new image
/// and recorded it, while the tracee never left the old one.
fn unexecutable_image(scratch: &Path) -> PathBuf {
    let source = fixture_binary(scratch, "umbra-userspace-edges", "UMBRA_EDGES_PATH");
    let image = scratch.join("umbra-edge-unexecutable");
    std::fs::copy(&source, &image).expect("copying the edge fixture to an unexecutable image");
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o644))
        .expect("clearing the execute bits is what makes the exec fail");
    image
}

/// Launch one edge case whose tracee execs [`exec_helper`], naming it through
/// the environment so the fixture's own `argc == 3` contract is untouched.
fn routed_exec_edge(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let helper = exec_helper(scratch);
    let entry = format!("UMBRA_EDGE_HELPER={}", helper.display());
    routed_edge(scratch, host, port, case, &[&entry])
}

/// Launch one of the edge cases, which take a case name before the path.
fn routed_edge(scratch: &Path, host: &str, port: u16, case: &str, env: &[&str]) -> Run {
    let edges = fixture_binary(scratch, "umbra-userspace-edges", "UMBRA_EDGES_PATH");
    launch(scratch, host, port, &edges, &[case], env)
}

/// Launch the Rust I/O fixture, which takes a case name before the path exactly
/// as the edge fixture does.
///
/// `launch` is untouched and has to be: it already seeds `workspace/seed.txt`,
/// which *is* the existing source file the fixture's first leg reads, and it
/// already appends the destination after the leading arguments. So the fixture
/// receives `<case> <workspace>/out.txt` and derives every sibling it needs from
/// that path.
fn routed_rust_io(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = rust_fixture_binary(scratch, "umbra-userspace-rustio", "UMBRA_RUSTIO_PATH");
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Launch one of the stdio fixture's cases, which take a case name before the
/// path exactly as the edge fixture's do.
fn routed_stdio(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = fixture_binary(scratch, "umbra-userspace-stdio", "UMBRA_STDIO_PATH");
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Launch the buffered Rust control, which takes the destination and nothing
/// else.
///
/// No case name, because the fixture has exactly one case and refuses a second
/// argument -- so `leading` is empty and `launch` appends the destination
/// alone, as it does for the toy. The fixture's own header records why a second
/// case was measured and then left out.
fn routed_buffered(scratch: &Path, host: &str, port: u16) -> Run {
    let fixture = rust_fixture_binary(scratch, "umbra-userspace-buffered", "UMBRA_BUFFERED_PATH");
    launch(scratch, host, port, &fixture, &[], &[])
}

/// Launch one of the append fixture's cases, which take a case name before the
/// path exactly as the edge fixture's do.
fn routed_append(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = fixture_binary(scratch, "umbra-userspace-append", "UMBRA_APPEND_PATH");
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Launch the Rust half of the append pair, which takes the same three case
/// names as the C half and derives the same targets from the same destination.
fn routed_append_std(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = rust_fixture_binary(scratch, "umbra-userspace-appendstd", "UMBRA_APPENDSTD_PATH");
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Launch one of the descriptor fixture's cases, which take a case name before
/// the path exactly as the append fixture's do.
fn routed_descriptor(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = fixture_binary(
        scratch,
        "umbra-userspace-descriptor",
        "UMBRA_DESCRIPTOR_PATH",
    );
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Launch the Rust half of the descriptor pair, which takes its own five case
/// names and derives the same targets from the same destination.
///
/// Five rather than the C half's seven, and the gap is a measurement rather
/// than an omission: `std` has no positional-I/O method on a `File` on this
/// platform without `FileExt`, so `pread`/`pwrite` have no Rust call site to
/// issue here. The fixture pair's headers record it.
fn routed_descriptor_std(scratch: &Path, host: &str, port: u16, case: &str) -> Run {
    let fixture = rust_fixture_binary(
        scratch,
        "umbra-userspace-descriptorstd",
        "UMBRA_DESCRIPTORSTD_PATH",
    );
    launch(scratch, host, port, &fixture, &[case], &[])
}

/// Run one supervised command against the live fixture and collect its verdict.
fn launch(
    scratch: &Path,
    host: &str,
    port: u16,
    program: &Path,
    leading: &[&str],
    env: &[&str],
) -> Run {
    let workspace = scratch.join("workspace");
    let state = scratch.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let destination = workspace.join("out.txt");
    let registry = registry(scratch, host, port);

    let mut command = Command::new(binaries().join("umbra"));
    command
        .args(["run", "--registry"])
        .arg(&registry)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--experimental");
    for entry in env {
        command.arg("--env").arg(entry);
    }
    let output = command
        .arg("--")
        .arg(program)
        .args(leading)
        .arg(&destination)
        .stdin(Stdio::null())
        .output()
        .expect("umbra run");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let run_id = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("umbra: run ")
                .and_then(|line| line.strip_suffix(" prepared"))
        })
        .unwrap_or_else(|| panic!("no prepared run id in:\n{stderr}"));
    Run {
        run_id: RunId(uuid::Uuid::parse_str(run_id).unwrap()),
        status: output.status.code(),
        stderr,
        destination,
        state,
        workspace,
        registry,
    }
}

impl Run {
    /// The toy's own exit code, as umbra reported it.
    ///
    /// umbra's exit status is its own, not the child's -- a nonzero child is
    /// `ProcessFailed` -- so the child's code is read from the status line umbra
    /// prints, which carries it verbatim.
    fn child_exit(&self) -> i32 {
        let marker = "finished: Some(Code(";
        let line = self
            .stderr
            .lines()
            .find(|line| line.contains(marker))
            .unwrap_or_else(|| panic!("no child status in:\n{}", self.stderr));
        let start = line.find(marker).unwrap() + marker.len();
        let rest = &line[start..];
        let end = rest.find(')').unwrap();
        rest[..end].parse().unwrap()
    }

    /// This run's host state directories.
    fn state_root(&self) -> PathBuf {
        self.state.join(self.run_id.0.to_string())
    }

    /// The logical path the toy wrote, as components below the run's root anchor.
    ///
    /// The overlay shadows an absolute logical path under `root/`, so the
    /// components are the destination's, minus its leading separator.
    fn shadow_components(&self) -> Vec<Vec<u8>> {
        self.destination
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    }
}

/// Read one object back **through the userspace client**, with nothing mounted.
///
/// Deliberately not through `NfsUserspaceStorage`: this is the independent half
/// of the assertion, so it speaks NFSv4 to the server itself -- `PUTROOTFH`,
/// `LOOKUP` per component, `READ` -- and shares no resolution code with the run
/// whose result it is checking. `None` means the name does not exist.
fn read_through_client(host: &str, port: u16, components: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut config = RawTransportConfig::loopback(port);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 10_000 };
    let deadline = config.limits.default_deadline;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let mut handle = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    let mut attributes = None;
    for component in components {
        let name = ComponentName::new(component.clone()).expect("component");
        let (next, found) = transport
            .lookup(&handle, &name, AttrMask::STAT, deadline)
            .ok()?;
        handle = next;
        attributes = Some(found);
    }
    let attributes = attributes.expect("at least one component");
    assert_eq!(
        attributes.file_type,
        Some(Nfs4Type::Regular),
        "the shadow object is not a regular file"
    );
    let length = attributes.size.unwrap_or(0);
    if length == 0 {
        return Some(Vec::new());
    }
    let read = transport
        .read(
            &handle,
            umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
            0,
            length.min(4096) as u32,
            deadline,
        )
        .expect("READ with the anonymous stateid");
    Some(read.data)
}

/// The **size** one object has in the export, through the client.
///
/// `read_through_client` caps its `READ` at 4 KiB, which is right for the small
/// payloads it checks and useless for the 2 MiB one. This asks the server for the
/// attribute instead, so a case whose claim is "every byte landed" can assert it
/// out of band without pulling two megabytes back over NFS. `None` means the name
/// does not exist.
fn stored_size_through_client(host: &str, port: u16, components: &[Vec<u8>]) -> Option<u64> {
    let mut config = RawTransportConfig::loopback(port);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 10_000 };
    let deadline = config.limits.default_deadline;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let mut handle = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    let mut attributes = None;
    for component in components {
        let name = ComponentName::new(component.clone()).expect("component");
        let (next, found) = transport
            .lookup(&handle, &name, AttrMask::STAT, deadline)
            .ok()?;
        handle = next;
        attributes = Some(found);
    }
    let attributes = attributes.expect("at least one component");
    assert_eq!(
        attributes.file_type,
        Some(Nfs4Type::Regular),
        "the shadow object is not a regular file"
    );
    attributes.size
}

/// One byte **range** of an object in the export, through the client.
///
/// Neither existing read-back can answer what a multi-chunk payload needs.
/// `read_through_client` caps its `READ` at 4 KiB, which is right for the small
/// objects it checks; `stored_size_through_client` asks for the size and nothing
/// else. What discriminates a library loop that finished its transfer from one
/// that completed a single remainder and stopped is the bytes *at* the chunk
/// boundaries, and this asks for exactly those -- without pulling three
/// megabytes back over NFS to get them.
///
/// A separate function rather than an offset parameter on the existing one, for
/// the reason `attributes_through_client` is separate: a caller that wants a
/// whole small object should not have to name a range to say so.
///
/// `count` must stay at or below the backend's 1 048 572-byte per-call bound, or
/// the read this performs is itself the thing under test. `None` means the name
/// does not exist.
fn read_range_through_client(
    host: &str,
    port: u16,
    components: &[Vec<u8>],
    offset: u64,
    count: u32,
) -> Option<Vec<u8>> {
    let mut config = RawTransportConfig::loopback(port);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 10_000 };
    let deadline = config.limits.default_deadline;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let mut handle = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    let mut attributes = None;
    for component in components {
        let name = ComponentName::new(component.clone()).expect("component");
        let (next, found) = transport
            .lookup(&handle, &name, AttrMask::STAT, deadline)
            .ok()?;
        handle = next;
        attributes = Some(found);
    }
    let attributes = attributes.expect("at least one component");
    assert_eq!(
        attributes.file_type,
        Some(Nfs4Type::Regular),
        "the shadow object is not a regular file"
    );
    let read = transport
        .read(
            &handle,
            umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
            offset,
            count,
            deadline,
        )
        .expect("READ with the anonymous stateid");
    Some(read.data)
}

/// The components of one run's shadow object, below the export root.
fn shadow_path(run: &Run) -> Vec<Vec<u8>> {
    let mut components = vec![
        EXPORT.to_vec(),
        RUN_PARENT.to_vec(),
        run.run_id.0.to_string().into_bytes(),
        b"root".to_vec(),
    ];
    components.extend(run.shadow_components());
    components
}

/// Whether the run's journal records it as completed.
///
/// Opens the real `FileJournal` read-only and replays it, so the format header,
/// frame length, checksum and sequence ordering are all checked by the backend
/// before any payload is looked at. Every failure panics rather than answering
/// `false`: a log this cannot read is not a log without a completion record.
fn journal_records_completion(run: &Run) -> bool {
    let control = run.state_root().join("journal");
    let directory = PhysicalPath(BytePath::new(control.as_os_str().as_bytes().to_vec()).unwrap());
    let mut journal = FileJournal::new();
    journal
        .open(&JournalOpenRequest {
            control: JournalControlBinding {
                run_id: run.run_id,
                directory,
            },
            access: JournalAccess::ReadOnly,
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        })
        .unwrap_or_else(|e| panic!("opening {} read-only: {e}", control.display()));
    let completed = journal
        .replay(Sequence(0))
        .unwrap_or_else(|e| panic!("replaying {}: {e}", control.display()))
        .map(|record| record.unwrap_or_else(|e| panic!("undecodable journal record: {e}")))
        .any(|record| {
            matches!(
                record.payload,
                JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
            )
        });
    completed
}

/// Every NFS filesystem this host currently has mounted, as `(from, on)`.
///
/// `getmntinfo(MNT_NOWAIT)` rather than parsing `mount(8)`: it is the same list
/// the kernel answers `statfs` from, and it needs no output format to stay
/// stable. `MNT_NOWAIT` because a hung server must not hang the assertion.
fn nfs_mounts() -> Vec<(String, String)> {
    let mut found = Vec::new();
    // SAFETY: `getmntinfo` fills a pointer to a static buffer it owns and returns
    // its entry count; the entries are read, never written or freed, before any
    // further call. Each name is a NUL-terminated C string inside one entry.
    unsafe {
        let mut buffer: *mut libc::statfs = std::ptr::null_mut();
        let count = libc::getmntinfo(&mut buffer, libc::MNT_NOWAIT);
        for index in 0..count.max(0) as isize {
            let entry = &*buffer.offset(index);
            let kind = std::ffi::CStr::from_ptr(entry.f_fstypename.as_ptr())
                .to_string_lossy()
                .into_owned();
            if !kind.starts_with("nfs") {
                continue;
            }
            found.push((
                std::ffi::CStr::from_ptr(entry.f_mntfromname.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                std::ffi::CStr::from_ptr(entry.f_mntonname.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    }
    found
}

/// Refuse the one thing that would make this whole proof an illusion.
///
/// The addendum ratified this as the structural assertion beside
/// `physical_path is None`: *"assert no NFS mount covers the run root, guarding
/// against a helpfully-mounted export making the result an illusion."* Without
/// it, a host that happened to have the fixture export mounted could satisfy
/// every other assertion here through the kernel client, and nothing would say
/// so.
///
/// Two checks, because there are two ways to be fooled. A mount **covering** any
/// path the run touches could serve the bytes; a mount **of the fixture itself**,
/// anywhere, could be how they got there. Called before and after the run, so it
/// covers the run's duration rather than an instant.
fn assert_no_nfs_mount(host: &str, port: u16, paths: &[&Path], when: &str) {
    let mounts = nfs_mounts();
    let endpoint = format!("{host}:{port}");
    for (from, on) in &mounts {
        for path in paths {
            assert!(
                !path.starts_with(on),
                "{when}: an NFS mount ({from} on {on}) covers {} -- the kernel client \
                 could be serving this run, so nothing below proves the userspace \
                 client did",
                path.display()
            );
        }
        // Matched on the parsed `<server>:<export>` rather than on a bare host
        // substring. `from.contains(host)` tripped on *any* mount whose source
        // named this address -- a second, unrelated loopback NFS export on a
        // different port would have failed a run it says nothing about -- and a
        // check that can fire for an unrelated reason is a check nobody trusts.
        // What actually matters is one of two things: the source names this
        // fixture's `host:port`, or it names this host together with the export
        // the fixture serves.
        let (server, export) = from.rsplit_once(':').unwrap_or((from.as_str(), ""));
        assert!(
            !from.contains(&endpoint)
                && !(server == host
                    && export
                        .trim_start_matches('/')
                        .as_bytes()
                        .starts_with(EXPORT)),
            "{when}: the fixture export is mounted ({from} on {on}); the read-back \
             could be answered through the kernel client"
        );
    }
}

/// Refuse anything under the tracee's one host write allowance.
///
/// The Seatbelt template carries exactly one write rule and it names a real
/// path; a routed run points it at an empty per-run directory rather than at the
/// store, which is not on this host. So this asserts **this run did not use its
/// one host write allowance** -- which is what it can assert. It is not a
/// detector for anything that escaped routing: an escaped write that names some
/// other path is refused by the profile and leaves nothing here to find. What
/// establishes that the bytes went to the store is `read_through_client`, and
/// what establishes they did not go to the host is the destination-absent and
/// workspace-pristine assertions beside it.
fn assert_host_write_root_empty(run: &Run) {
    let host = run.state_root().join("host");
    let entries: Vec<_> = std::fs::read_dir(&host)
        .unwrap_or_else(|e| panic!("reading {}: {e}", host.display()))
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "the tracee's host write allowance is not empty: {entries:?} -- an operation \
         escaped routing and reached the host"
    );
}

/// Refuse any change to the seeded workspace.
fn assert_workspace_pristine(run: &Run) {
    let mut names: Vec<_> = std::fs::read_dir(&run.workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [std::ffi::OsString::from("seed.txt")],
        "the workspace changed"
    );
}

#[test]
fn a_routed_run_creates_writes_reopens_reads_and_compares_end_to_end() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    // 0. Nothing is mounted. Checked before the run so a pre-existing mount
    //    cannot have served it, and again after so one appearing mid-run cannot
    //    either. Every assertion below is only worth what this one is.
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_run(scratch.path(), &host, port);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    // 1. The toy's own verdict. It wrote, closed, reopened, read and compared;
    //    exit zero is the comparison succeeding, which no unrouted or
    //    half-routed path can produce -- the host destination does not exist, so
    //    an unrouted reopen would have failed with exit 6.
    assert_eq!(
        run.status,
        Some(0),
        "umbra run failed (child exit {:?}):\n{}",
        run.stderr
            .lines()
            .find(|line| line.contains("finished:"))
            .unwrap_or("<no status line>"),
        run.stderr
    );

    // 2. The bytes are in the Ganesha export, read back **through the client**
    //    with nothing mounted, over a connection this test opened itself.
    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored, PAYLOAD,
        "the export does not hold the bytes the toy wrote"
    );

    // 3. The host path the toy named was never created, and the workspace is
    //    byte-identical to the one preparation approved.
    assert!(
        !run.destination.exists(),
        "the host destination {} was created",
        run.destination.display()
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);

    // 4. The run is journalled complete, in the log beside this state root.
    assert!(
        journal_records_completion(&run),
        "no RunCompleted record in the run's journal"
    );

    // 5. The writer lease was released. Asserted behaviourally rather than by
    //    looking for a marker file: this backend cannot unlink one -- the frozen
    //    `Nfs4Op` set carries no REMOVE -- so it records the release *in* the
    //    marker, and a test reading the file would be asserting its private
    //    format. A reopen acquires with `TakeoverPolicy::Refuse`, so it succeeds
    //    only against a run whose previous writer provably let go.
    let reopened = Command::new(binaries().join("umbra"))
        .args(["resume", "--registry"])
        .arg(&run.registry)
        .arg("--workspace")
        .arg(&run.workspace)
        .arg("--state-dir")
        .arg(&run.state)
        .arg(run.run_id.0.to_string())
        .stdin(Stdio::null())
        .output()
        .expect("umbra resume");
    let reopen_stderr = String::from_utf8_lossy(&reopened.stderr);
    assert!(
        reopened.status.success(),
        "the run could not be reopened, so the writer lease was not released:\n{reopen_stderr}"
    );
    assert!(
        reopen_stderr.contains("requires no reconciliation"),
        "a cleanly finished run reopened as needing recovery:\n{reopen_stderr}"
    );
}

#[test]
fn a_reopen_without_the_run_s_own_journal_refuses_rather_than_reporting_it_healthy() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_run(scratch.path(), &host, port);
    assert_eq!(run.status, Some(0), "{}", run.stderr);

    // A routed run's journal lives outside its store, so reopening it needs both
    // halves. A writer `journal.open` against an empty directory would establish
    // a FRESH log, which has nothing pending, which would make the reopen report
    // `recovery_required: false` for a run it never classified. That is the
    // fail-open the nonce fence exists to remove, and this is the case that pins
    // it: the store is reachable, the log is not, and the answer must be a
    // refusal rather than a clean verdict.
    let elsewhere = scratch.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let reopened = Command::new(binaries().join("umbra"))
        .args(["resume", "--registry"])
        .arg(&run.registry)
        .arg("--workspace")
        .arg(&run.workspace)
        .arg("--state-dir")
        .arg(&elsewhere)
        .arg(run.run_id.0.to_string())
        .stdin(Stdio::null())
        .output()
        .expect("umbra resume");
    let stderr = String::from_utf8_lossy(&reopened.stderr);
    assert!(
        !reopened.status.success(),
        "a reopen with no journal evidence succeeded:\n{stderr}"
    );
    assert!(
        stderr.contains("journal evidence was not found"),
        "the refusal does not name the missing evidence:\n{stderr}"
    );
    assert!(
        !stderr.contains("requires no reconciliation"),
        "a missing-evidence reopen reported a clean verdict:\n{stderr}"
    );
}

#[test]
fn mutation_probe_read_makes_the_toy_reject_the_bytes_it_read() {
    if declared_probe().as_deref() != Some("read") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=read with a binary built with \
                   --features mutation-probe-read"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_run(scratch.path(), &host, port);

    // The toy compared what it read against what it wrote and rejected the
    // mismatch. Exit 8 is its comparison arm and nothing else reaches it.
    assert_eq!(
        run.child_exit(),
        8,
        "breaking read routing did not make the comparison fail:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );

    // The discriminator, and the reason this cannot pass by breaking the write
    // direction instead: the export still holds exactly the bytes the toy wrote.
    // Only the bytes handed *back* were corrupted.
    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored, PAYLOAD,
        "the read probe also changed what reached the store, so it is not isolated \
         to the read direction"
    );
    assert!(!run.destination.exists());
    assert_host_write_root_empty(&run);
}

#[test]
fn mutation_probe_write_makes_the_read_back_come_up_short() {
    if declared_probe().as_deref() != Some("write") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=write with a binary built with \
                   --features mutation-probe-write"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_run(scratch.path(), &host, port);

    // A different arm of the toy from probe A's, on purpose: the write was
    // reported successful, so the failure surfaces at the read-back's byte count.
    // Exit 7 and exit 8 cannot stand in for one another.
    assert_eq!(
        run.child_exit(),
        7,
        "breaking write routing did not make the read-back come up short:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );

    // The discriminator. A short read-back alone could be explained by a read
    // fault; an object in the export that is absent or empty could not.
    match read_through_client(&host, port, &shadow_path(&run)) {
        None => {}
        Some(stored) => assert!(
            stored.is_empty(),
            "the write probe left {} bytes in the export",
            stored.len()
        ),
    }
    assert!(!run.destination.exists());
    assert_host_write_root_empty(&run);
}

// ---------------------------------------------------------------------------
// Regressions for round 1. Each of these was a dead run, a wrong errno, or a
// launch that could not start; each is now an ordinary answer. They share the
// e2e case's fixture gate and its mutated-binary skip, because a mutated binary
// breaks routing on purpose and would fail them for the wrong reason.
// ---------------------------------------------------------------------------

/// A routed operation the overlay refuses with `NotFound` answers **ENOENT**.
///
/// It answered `ENOSYS` (78): the supervisor resumed the tracee's own syscall on
/// a non-mutating `NotFound`, and for a routed operation the tracee's own
/// syscall is umbra's reserved trap, which Darwin's `nosys` answers. `ENOENT` is
/// the most common file-operation failure there is and programs branch on it.
#[test]
fn a_routed_open_of_an_absent_path_answers_enoent_rather_than_enosys() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "notfound", &[]);
    assert_eq!(
        run.child_exit(),
        2,
        "a routed open of an absent path did not answer ENOENT:\n{}",
        run.stderr
    );
}

/// A routed transfer with a bad buffer pointer answers **EFAULT**.
///
/// It used to end the run: `io_buffer` knew the errno but the binding is built
/// before `resolve`, and `resolve` is the only producer of a tracee-visible
/// refusal, so the errno had nowhere to go. `read(fd, NULL, 4)` is an ordinary
/// program bug.
#[test]
fn a_routed_read_with_a_null_buffer_answers_efault_rather_than_killing_the_run() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "efault", &[]);
    assert_eq!(
        run.child_exit(),
        14,
        "a routed read with a null buffer did not answer EFAULT:\n{}",
        run.stderr
    );
    // The run reached its own teardown rather than aborting, which is the other
    // half of the finding: the failure belongs to the call, not to the run.
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
}

/// A routed transfer larger than the backend's per-call bound answers **short**.
///
/// Both clamps used the *contract* ceiling (1 MiB); `read_at`/`write_at` enforce
/// the run's own `max_io_bytes`, 1 048 572 for `LibnfsRawTransport`. Every
/// request above that was clamped to a value the backend refused with an error,
/// and the run ended. The fixture writes and reads 2 MiB in one call each,
/// requires the first call to answer short rather than fail, finishes the
/// remainder, and compares.
#[test]
fn a_routed_transfer_past_the_backend_bound_is_short_rather_than_fatal() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "bigio", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "a 2 MiB round trip through routed short I/O failed:\n{}",
        run.stderr
    );
    assert_eq!(run.status, Some(0));

    // **Out of band, because the fixture's own compare runs inside the tracee.**
    // Exit zero now proves the first transfer in each direction was *short* (the
    // fixture rejects a complete one), which is the clamp this case is named for;
    // it does not by itself prove the remaining bytes reached the store. The
    // object's size in the export does. Asked as an attribute rather than read
    // back, because two megabytes over NFS would prove the same thing slower.
    assert_eq!(
        stored_size_through_client(&host, port, &shadow_path(&run)),
        Some(2 * 1024 * 1024),
        "the export does not hold all 2 MiB the fixture wrote, so the short-I/O \
         loop did not finish what the clamp started"
    );

    assert!(!run.destination.exists());
    assert_host_write_root_empty(&run);
}

/// A program whose library initializer touches a file **starts**.
///
/// umbra can only learn the interposer's load address after dyld has mapped it,
/// which means running the tracee to its entry point, which means every
/// initializer has already run. A library that armed itself from its own
/// constructor was therefore live with no breakpoint on its trap for that whole
/// window: measured, the first `open` from an initializer took `SIGSYS` and the
/// launch died before `main` with an undecoded debugger packet and a
/// recovery-required run. umbra now arms the library itself, after planting the
/// breakpoints, so the window is a passthrough.
#[test]
fn a_file_touching_library_initializer_no_longer_kills_the_launch() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "ctor", &["UMBRA_EDGE_CTOR=1"]);
    assert_eq!(
        run.status,
        Some(0),
        "a file-touching library initializer failed the launch:\n{}",
        run.stderr
    );
    assert_eq!(run.child_exit(), 0);
}

/// Removing **the log alone** makes a reopen refuse.
///
/// The fence used to compare the store's nonce against a `journal-id` file
/// sitting *beside* the log, which a vanished log leaves untouched. Measured,
/// before this: the reopen established a fresh journal -- nothing pending, so
/// `bind` did not poison -- and reported "requires no reconciliation", exit 0,
/// for a run whose evidence was gone. The nonce now lives in the log's own first
/// record, which a fresh log does not have.
#[test]
fn a_reopen_whose_log_alone_was_removed_refuses() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_run(scratch.path(), &host, port);
    assert_eq!(run.status, Some(0), "{}", run.stderr);

    // The backend appends `journal` to the control directory it is handed, so the
    // log is one level below this run's journal directory.
    let log = run.state_root().join("journal").join("journal").join("log");
    assert!(log.is_file(), "no log at {}", log.display());
    std::fs::remove_file(&log).unwrap();

    let reopened = Command::new(binaries().join("umbra"))
        .args(["resume", "--registry"])
        .arg(&run.registry)
        .arg("--workspace")
        .arg(&run.workspace)
        .arg("--state-dir")
        .arg(&run.state)
        .arg(run.run_id.0.to_string())
        .stdin(Stdio::null())
        .output()
        .expect("umbra resume");
    let stderr = String::from_utf8_lossy(&reopened.stderr);
    assert!(
        !reopened.status.success(),
        "a reopen whose log was removed succeeded:\n{stderr}"
    );
    assert!(
        stderr.contains("journal evidence was not found"),
        "the refusal does not name the missing evidence:\n{stderr}"
    );
    assert!(
        !stderr.contains("requires no reconciliation"),
        "a run whose log was removed reopened as healthy:\n{stderr}"
    );
}

/// A declined reopen leaves the state root exactly as it found it.
///
/// `resume` used to create `<state>/<run-id>/{journal,host}` before verifying,
/// so a refusal left two empty directories behind -- including the very empty
/// journal directory whose existence-without-a-log the fence exists to refuse.
#[test]
fn a_declined_reopen_creates_nothing_under_the_state_root() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_run(scratch.path(), &host, port);
    assert_eq!(run.status, Some(0), "{}", run.stderr);

    let elsewhere = scratch.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let reopened = Command::new(binaries().join("umbra"))
        .args(["resume", "--registry"])
        .arg(&run.registry)
        .arg("--workspace")
        .arg(&run.workspace)
        .arg("--state-dir")
        .arg(&elsewhere)
        .arg(run.run_id.0.to_string())
        .stdin(Stdio::null())
        .output()
        .expect("umbra resume");
    let stderr = String::from_utf8_lossy(&reopened.stderr);
    assert!(
        !reopened.status.success(),
        "the reopen succeeded:\n{stderr}"
    );
    // Named, not merely unsuccessful. A reopen that failed for some *other*
    // reason -- an unreadable registry, a crash before it looked at anything --
    // would also leave the state root empty, and this case would have reported
    // that as the property it is about.
    assert!(
        stderr.contains("journal evidence was not found"),
        "the reopen failed for a different reason than the declined verdict:\n{stderr}"
    );
    let left: Vec<_> = std::fs::read_dir(&elsewhere)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        left.is_empty(),
        "a declined reopen left {left:?} behind under the state root"
    );
}

/// A routed `O_TRUNC` open of a non-empty object shortens it.
///
/// The only case that reaches `StorageOperation::Truncate` through the userspace
/// client. The plain toy never does: its one `O_TRUNC` open *creates* the object,
/// so the engine's `length != 0` guard skips the arm every time. Round 1 found
/// that arm working but pinned by nothing, and it is the one place a routed open
/// has to perform by itself what the kernel would have done for a rewritten one.
#[test]
fn a_routed_o_trunc_open_shortens_a_non_empty_object() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "trunc", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "a routed O_TRUNC open did not shorten the object:\n{}",
        run.stderr
    );

    // Through the client, independently: the export holds the two bytes the
    // second open wrote and nothing of the ten the first did.
    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored, b"ab",
        "the export still holds the pre-truncate bytes"
    );
}

/// A path operation this slice cannot route is **refused to the tracee**, and
/// the run finishes.
///
/// `rewrite` needs a kernel path, and this backend has none — so every
/// intercepted path operation other than `open` reaching an object the run
/// created or copied up used to end the run with an internal
/// `UnsupportedCapability`, where the identical program on `local` exits 0.
/// That is reachable by `create a file, then stat it`, which is what `cp`,
/// `install` and both the Rust and Go standard libraries do.
///
/// It is still unsupported — routing it properly means answering `Stat`/`Access`
/// through an ABI encoder, a larger slice than the toy this one claims — but it
/// is now a visible `ENOTSUP`, which is the discipline every other unsupported
/// routed operation already holds.
#[test]
fn an_unroutable_path_operation_is_refused_to_the_tracee_and_the_run_finishes() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    // Both arms of the decoder reach the same refusal, by different routes.
    for case in ["statat", "accessat"] {
        let scratch = tempfile::tempdir().unwrap();
        let run = routed_edge(scratch.path(), &host, port, case, &[]);
        assert_eq!(
            run.child_exit(),
            45,
            "{case}: a path operation on a run-created object did not answer \
             ENOTSUP:\n{}",
            run.stderr
        );
        // The other half, and the one that makes this a fix rather than a
        // rename: the run reached its own teardown. An internal error would have
        // stopped it before any status line.
        assert!(
            run.stderr.contains("finished:"),
            "{case}: the run did not finish:\n{}",
            run.stderr
        );
        assert_host_write_root_empty(&run);
    }
}

/// A forked child of a routed tracee is mediated, and mediated from its first
/// instruction.
///
/// `fork` copies the address space, so the child starts with the interposer
/// mapped and **already armed** -- its routing traps are live before umbra has
/// attached a single breakpoint to the new session. umbra used to answer that by
/// re-running the load-address search, which resumes the tracee to `main`; a
/// forked child is past `main` and never reaches it again, so it ran unmediated
/// until its first routed call became an `svc` the kernel does not know, took
/// `SIGSYS`, and ended the run. CodeRabbit found it on PR #116 (item 3) and the
/// unmediated window was the serious half.
///
/// The fixture's child does a whole create/write/close/reopen/read/compare on a
/// path of its own, so a pass means the child's routing actually worked rather
/// than merely not crashing, and the parent writes afterwards, so a pass also
/// means the fork did not cost the parent its own mediation. **Both objects are
/// then read back out of the export through the client**, because the fixture's
/// own compare happens inside the tracee and cannot distinguish a write that
/// reached the store from one that never left the process.
#[test]
fn a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "fork", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "a forked child did not complete a routed round trip:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );

    // **Both writes read back out of the export, through the client.** The
    // fixture's own compare runs inside the tracee, so on its own it cannot tell
    // a routed write from one the interposer answered without reaching the store.
    // These two assertions are what make the claim out-of-band, and they are
    // separate objects so neither half can borrow the other's evidence.
    let mut child_object = shadow_path(&run);
    let leaf = child_object.last_mut().expect("the shadow path has a leaf");
    leaf.extend_from_slice(b".child");
    assert_eq!(
        read_through_client(&host, port, &child_object)
            .expect("the forked child's object exists in the export")
            .as_slice(),
        b"fork",
        "the forked child's write did not reach the store"
    );
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run))
            .expect("the parent's object exists in the export")
            .as_slice(),
        b"parent",
        "the parent stopped routing after the fork"
    );

    // Neither process reached the host: the whole point is that the child was
    // mediated, not that it was refused.
    assert!(!run.destination.exists());
    assert_host_write_root_empty(&run);
}

/// The other half of the fork story: a routed descriptor **survives** the fork,
/// offset included, and the child's write through it reaches the store.
///
/// Written to check the opposite. A routed descriptor is virtual, so there is
/// nothing for the kernel to inherit, and the plausible reading of
/// `routed_binding` -- a per-process table the child starts empty -- predicts
/// `EBADF`. Measured, that is wrong: the supervisor inherits the descriptor
/// table across a fork (`Fork preserves every inherited descriptor`, which until
/// now meant kernel descriptors only), and the routed entries come with it
/// carrying their offsets. So this is a capability rather than a limit, and it is
/// asserted **through the client** rather than from the tracee's exit status,
/// because the exit alone cannot tell a real write from a discarded one.
#[test]
fn a_routed_descriptor_survives_a_fork_and_the_child_s_write_reaches_the_store() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "forkfd", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "the child's write through an inherited routed descriptor failed:\n{}",
        run.stderr
    );
    // The load-bearing assertion, and the reason the exit status is not enough:
    // `seed` from the parent, `child` from the forked child, in that order, which
    // is only possible if the descriptor's *offset* was inherited too.
    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored,
        b"seedchild",
        "the inherited descriptor did not continue the parent's write: {:?}",
        String::from_utf8_lossy(&stored)
    );
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
    assert_host_write_root_empty(&run);
}

/// The third half of the fork story, and the one that was broken: a forked child
/// that **`exec`s a different binary** is mediated in the new image too.
///
/// `exec` is not `fork`. `fork` copies the address space, so the interposer
/// arrives in the child already armed and umbra must *not* re-arm it. `exec`
/// replaces the address space: dyld re-loads the interposer, because
/// `DYLD_INSERT_LIBRARIES` rides in `envp` and survives the exec, and it re-runs
/// its constructor -- so the library arrives mapped and **inert**, with its
/// `__DATA,__umbra_arm` control block back to zeroes. umbra has to arm it again,
/// exactly as it arms the launch target's.
///
/// It used to arm only the one image named at launch. `install()` compared the
/// running image against a `target` assigned once, at launch, and never
/// reassigned, so a child that exec'd anything else took the "this run routes
/// nothing" arm and no control block was written. **Silently** -- no refusal,
/// no diagnostic, no event.
///
/// **What the tracee actually saw is narrower than "unmediated", and the
/// narrow version is the measured one.** Only the *interposer* was un-armed.
/// The tracer's own work is unconditional: `install()` re-plants every
/// `TRACED_STUBS` breakpoint after an exec, so the exec'd child's `open` was
/// still routed at the libc stub and still returned a **virtual descriptor**.
/// But `write` and `close` reach umbra through the interposer and nowhere else,
/// so they went to libc carrying a descriptor number the kernel does not own,
/// and the kernel answered **`EBADF`**. The object was therefore **created and
/// left empty** in the export. Nothing reached the host; Seatbelt refused
/// nothing. An earlier draft of this comment said the opposite -- that reads
/// reached the host and the write failed closed, so the object never appeared
/// -- and a review mutation disproved it by reading `[]` back through the
/// client.
///
/// **What each assertion below is worth, since they are not interchangeable.**
/// The exit status is checked first because it is the most legible failure, and
/// for *this* defect it does discriminate -- measured on the unfixed tree, the
/// exec'd child exited 9 (`EBADF`) where a fixed one exits 0, and that is the
/// assertion that fires first. What it cannot do is establish the claim this
/// case is named for: a tracee's own exit tells nobody whether the bytes reached
/// the store, only that its own calls returned what it expected. That is why the
/// object is read back **through the client**, out of band.
///
/// And it is read back for its **bytes**, not its mere presence, because the
/// broken tree leaves a name here too -- an empty one. Presence alone would
/// pass against the defect.
#[test]
fn a_forked_child_that_execs_a_different_binary_is_mediated_in_the_new_image() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_exec_edge(scratch.path(), &host, port, "forkexec");
    assert_eq!(
        run.child_exit(),
        0,
        "a forked child's exec'd image did not complete a routed write:\n{}",
        run.stderr
    );

    // The load-bearing assertion. The exec'd image's own write, read back out of
    // the export by a client that shares no resolution code with the run.
    let mut exec_object = shadow_path(&run);
    exec_object
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(b".exec");
    assert_eq!(
        read_through_client(&host, port, &exec_object)
            .expect("the exec'd child's object exists in the export")
            .as_slice(),
        b"execchild",
        "the exec'd child's write did not reach the store"
    );
    // And the fork did not cost the parent its own mediation, which is the
    // control: without it a tree that stopped routing entirely would satisfy
    // nothing above but would also not be distinguished from one that recovered.
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run))
            .expect("the parent's object exists in the export")
            .as_slice(),
        b"parent",
        "the parent stopped routing after its child exec'd"
    );
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
    assert!(!run.destination.exists());
    assert_host_write_root_empty(&run);
}

/// A **failed** `exec` must not move umbra's record of which image a session is
/// running, because the next `fork` hands that record to the child.
///
/// The regression test for a defect this slice introduced and review round 1
/// found. `intercept` used to write the resigned image into `Session::twin` at
/// the syscall **entry**, while the tracee was still sitting on the `svc`. A
/// successful `execve` never returns, so that was harmless for the case it was
/// written for. A **failed** `execve` does return -- to `finish_return`'s
/// `ReturnKind::Exec` arm, which is the failed-exec return by construction --
/// and nothing put the old value back.
///
/// **Why that became fatal only with this slice.** Before the interposer
/// requirement followed `twin`, a stale value had no consumer that cared. Now
/// `attach_child` points the child's requirement at it. A wrong image means
/// `install()` matches nothing and takes the "this run routes nothing" arm, so
/// **no trap site is breakpointed** -- while the control block the child
/// inherited through `fork` is **armed**, because `fork` copied it. The child's
/// first routed call then issues `svc #0x80` with `UMBRA_TRAP_NUMBER` in `x16`
/// and no breakpoint covers it: `SIGSYS`, and the run dies on an undecoded
/// exception with no diagnosis. That is the #116 defect shape, which the
/// ratification record names as the one thing not to reopen, reached from a
/// direction the guardrail does not cover.
///
/// The trigger is an arm64 executable with its execute bits cleared: umbra's
/// resign succeeds on it, so a new image path really does enter umbra's
/// records, and the kernel refuses to run it, so the tracee carries on in the
/// image it already had. See [`unexecutable_image`].
///
/// Both writes are read back **through the client**, because the run merely
/// surviving is necessary and not sufficient -- a run whose child routed
/// nothing at all would still exit 0 here.
#[test]
fn a_failed_exec_does_not_leave_a_later_fork_pointed_at_an_image_the_tracee_never_ran() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let image = unexecutable_image(scratch.path());
    let entry = format!("UMBRA_EDGE_NOEXEC={}", image.display());
    let run = routed_edge(scratch.path(), &host, port, "failedexec", &[&entry]);
    assert_eq!(
        run.child_exit(),
        0,
        "a fork after a failed exec did not complete a routed write:\n{}",
        run.stderr
    );

    // The load-bearing assertion: the child forked *after* the failed exec was
    // mediated. Before the fix this run did not get here at all -- it died with
    // an undecoded SIGSYS and `child_exit` found no status line to read.
    let mut child_object = shadow_path(&run);
    child_object
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(b".child");
    assert_eq!(
        read_through_client(&host, port, &child_object)
            .expect("the child forked after a failed exec has its object in the export")
            .as_slice(),
        b"failedexec",
        "the child forked after a failed exec did not route"
    );
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run))
            .expect("the parent's object exists in the export")
            .as_slice(),
        b"parent",
        "the parent stopped routing after its own exec failed"
    );
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
    assert_host_write_root_empty(&run);
}

/// `chdir`(12) moves the **logical** working directory, so a relative write in
/// an exec'd child lands where the program said rather than where it started.
///
/// This is #121's escalation and it is asserted on the **entry name**, in both
/// directions, because a wrong anchor does not lose the write -- it puts it
/// somewhere else. `ProcessContext::cwd` is what `DirRef::Cwd` resolution is
/// anchored against, and until now exactly one thing moved it after launch: a
/// routed `fchdir`. `chdir`(12) was declared and inert -- nothing decoded the
/// number -- so an unintercepted `chdir` reached the kernel, moved the *host*
/// working directory, and left umbra's logical copy at the launch directory.
/// The relative `open` that followed then resolved against the workspace root.
///
/// So the failure this pins is `leaf.txt` appearing beside the workspace root
/// instead of inside `out.txt.d`, and both halves are checked: the right name
/// holds the bytes, and the wrong name does not exist at all. Asserting only the
/// first would pass against an implementation that wrote to both.
///
/// The directory is created **through routing**, so it exists only in the run's
/// shadow. There is deliberately nothing on the host for a stray `chdir` to land
/// in, which is what stops the case passing for a reason it did not test.
#[test]
fn a_chdir_in_an_exec_d_child_moves_the_logical_cwd_its_relative_write_resolves_against() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_exec_edge(scratch.path(), &host, port, "chdirchild");
    assert_eq!(
        run.child_exit(),
        0,
        "the exec'd child's chdir-then-relative-write failed:\n{}",
        run.stderr
    );

    // The right entry name: inside the directory the child moved into.
    let mut moved = shadow_path(&run);
    moved
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(b".d");
    moved.push(b"leaf.txt".to_vec());
    assert_eq!(
        read_through_client(&host, port, &moved)
            .expect("the relative write landed in the directory the child chdir'd into")
            .as_slice(),
        b"chdirchild",
        "the relative write did not resolve against the moved working directory"
    );

    // The wrong entry name: beside the workspace root, where a stale logical cwd
    // would have anchored it. This is the half that makes the assertion
    // discriminating rather than merely satisfiable.
    let mut stale = shadow_path(&run);
    stale.pop();
    stale.push(b"leaf.txt".to_vec());
    assert!(
        read_through_client(&host, port, &stale).is_none(),
        "the relative write also landed at the launch directory, so the logical \
         working directory did not move"
    );
    assert_host_write_root_empty(&run);
}

/// The three `chdir` operand shapes `chdirchild` does not reach: a name that
/// does not resolve, a name that is not a directory, and a **relative** one.
///
/// `chdirchild` exercises exactly one shape -- an absolute operand naming a
/// directory that exists -- which leaves the branch a real shell uses most
/// (`cd sub`) uncovered, along with both refusals. The refusals matter for a
/// reason particular to a routed run: umbra cannot fall back to resuming the
/// tracee's own syscall, because for a routed operation that syscall is umbra's
/// reserved trap, which Darwin's `nosys` answers `ENOSYS`(78) while posting
/// `SIGSYS`. So `ENOENT` and `ENOTDIR` have to be *produced* by the namespace,
/// and a case that never asks for them cannot tell a correct refusal from a
/// dead run.
///
/// The fixture checks both errnos itself, since an errno is the whole of each
/// answer. What it cannot check from inside the tracee is where the relative
/// write landed, so that is asserted here, **through the client, on the entry
/// name** -- and two relative moves are chained precisely so the assertion is
/// not satisfiable by one: `leaf.txt` can only be two directories deep if the
/// first `chdir` moved the anchor the second was resolved against.
#[test]
fn a_chdir_answers_enoent_and_enotdir_to_the_tracee_and_anchors_a_relative_operand() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "chdirshapes", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "a chdir refusal or a relative chdir did not behave:\n{}",
        run.stderr
    );

    // Two directories deep, which only the chained relative moves can reach.
    let mut nested = shadow_path(&run);
    nested
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(b".outer");
    nested.push(b"inner".to_vec());
    nested.push(b"leaf.txt".to_vec());
    assert_eq!(
        read_through_client(&host, port, &nested)
            .expect("the relative write landed two directories down")
            .as_slice(),
        b"chdirshapes",
        "a relative chdir did not anchor against the working directory umbra held"
    );

    // And not at the launch directory, which is where a relative operand that
    // was resolved against a stale anchor would have put it.
    let mut stale = shadow_path(&run);
    stale.pop();
    stale.push(b"leaf.txt".to_vec());
    assert!(
        read_through_client(&host, port, &stale).is_none(),
        "the relative write also landed at the launch directory"
    );
    assert_host_write_root_empty(&run);
}

/// Two generations deep: a grandchild of a routed tracee routes too.
///
/// Each process is discovered from its own parent's syscall return -- the
/// `fork` breakpoint fires, the return gate reports the new pid, and umbra opens
/// a fresh RSP connection to it -- so a grandchild is reachable only if that
/// discovery *composes*. The intermediate child has to be mediated closely
/// enough for its own `fork` breakpoint to fire, which a child that merely
/// survived its parent's fork would not be.
///
/// All three writes are read back through the client, and they are three
/// separate objects so no generation can borrow another's evidence.
#[test]
fn a_grandchild_of_a_routed_tracee_routes_and_so_does_every_generation_above_it() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "grandchild", &[]);
    assert_eq!(
        run.child_exit(),
        0,
        "a grandchild of a routed tracee did not complete a routed write:\n{}",
        run.stderr
    );
    for (suffix, expected) in [
        (&b".grand"[..], &b"grandchild"[..]),
        (&b".mid"[..], &b"middle"[..]),
    ] {
        let mut object = shadow_path(&run);
        object
            .last_mut()
            .expect("the shadow path has a leaf")
            .extend_from_slice(suffix);
        assert_eq!(
            read_through_client(&host, port, &object)
                .unwrap_or_else(|| panic!(
                    "the object for {} is not in the export",
                    String::from_utf8_lossy(suffix)
                ))
                .as_slice(),
            expected,
            "the write for {} did not reach the store",
            String::from_utf8_lossy(suffix)
        );
    }
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run))
            .expect("the parent's object exists in the export")
            .as_slice(),
        b"parent",
        "the parent stopped routing after two generations forked below it"
    );
    assert_host_write_root_empty(&run);
}

/// A forked child's writes belong to the **parent's run**, so a run that never
/// completes covers them.
///
/// What this can assert is bounded by what umbra actually implements, and the
/// bound is worth stating rather than papering over. There is no run-level
/// rollback surface: `umbra stop` and `umbra checkpoint` are
/// `not_implemented`, and `abort` is explicitly documented as
/// reconcile-or-abandon that **does not claim arbitrary writes were undone**. So
/// "the child's writes are gone" is not a promise this codebase makes, and a
/// test asserting it would be pinning a behaviour that does not exist.
///
/// What *is* claimed, and what this pins, is the structural half: there is one
/// `NamespaceSession` per **run**, not per process, and `track_process` clones
/// the parent's context into the child rather than opening a transaction of its
/// own. So no per-process transaction exists that could commit independently of
/// the parent's, and the run's terminal evidence speaks for every process in it.
/// The parent stops the run after reaping the child -- an `O_APPEND` routed open,
/// which the namespace refuses as unsupported because there is no atomic
/// append-at-end storage operation behind it -- and the journal is then read for
/// its `RunCompleted` record, which a failed run must not have.
///
/// The child's object is read back through the client as well, and its presence
/// is the point rather than an embarrassment: it establishes that the child
/// really did write into the parent's shadow, which is what makes the missing
/// completion record cover it.
#[test]
fn a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "rollbackchild", &[]);

    // The run failed, and `umbra` says so rather than reporting a tidy exit.
    //
    // **Three assertions, and they carry different claims. Which one carries
    // which is worth stating, because an earlier version of this comment got it
    // wrong.**
    //
    // This first one is the weakest and proves the least: the fixture returns 71
    // if the `O_APPEND` open unexpectedly *succeeds*, and a nonzero tracee exit
    // is also reported as a failed run, so it holds either way. What tells those
    // apart is the **journal** assertion at the end -- a run that merely ended
    // with a nonzero child still reaches `finish_run` and still records
    // `RunCompleted`, while one stopped by a refused operation does not. So the
    // journal pins *that the refusal fired*.
    //
    // Neither of those is the structural claim this case exists for. That one is
    // carried by the **middle** assertion: the child's object read back at
    // `shadow_path(&run)` -- the *parent's* shadow -- which would answer `None`
    // if the child had a shadow or a transaction of its own to commit into.
    assert_ne!(
        run.status,
        Some(0),
        "the refused O_APPEND open did not stop the run:\n{}",
        run.stderr
    );

    // The child wrote into the parent's shadow. One run, one shadow: this is the
    // premise the assertion below depends on, so it is measured rather than
    // assumed.
    let mut child_object = shadow_path(&run);
    child_object
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(b".child");
    assert_eq!(
        read_through_client(&host, port, &child_object)
            .expect("the forked child's object is in the parent run's shadow")
            .as_slice(),
        b"rollbackchild",
        "the child did not write into the parent's run shadow"
    );

    // And the run carries no terminal completion record, so nothing in it --
    // the child's writes included -- is a completed run's output.
    assert!(
        !journal_records_completion(&run),
        "a run stopped by a refused operation still recorded RunCompleted"
    );
    assert_host_write_root_empty(&run);
}

/// The refusal above covers only what it has to: a path operation on an object
/// the run never touched still resolves through the read-only base and is
/// answered exactly as before.
///
/// Without this, the ENOTSUP arm could have been a blanket refusal of every path
/// operation on a routed run and every assertion above would still pass.
#[test]
fn a_path_operation_on_an_untouched_base_object_still_succeeds() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_edge(scratch.path(), &host, port, "statbase", &[]);
    assert_eq!(
        run.status,
        Some(0),
        "a stat of a host file the run never touched was refused:\n{}",
        run.stderr
    );
    assert_eq!(run.child_exit(), 0);
}

// ---------------------------------------------------------------------------
// Recoverable application errors on a routed run, each paired with a liveness
// sentinel.
//
// Four cases, and **three of the four end the run**. That is the finding rather
// than an inconvenience, and it decides the shape of every assertion below.
// `Overlay::resolve` answers an exclusive-create collision and a
// mkdir-on-existing with `Err(AlreadyExists)` and a non-empty removal with
// `Err(Denied)`; the one recovering arm in `syscall_entry` takes only a
// `NotFound` that is neither a mutation nor routed, so none of the three
// qualifies, the `Err` propagates, and `handle_event` poisons the run. The
// tracee is never resumed.
//
// The chain, in `crates/umbra-supervisor/src/events.rs`: `syscall_entry` opens
// at `:301`, the `resolve` call is `:552`, the sole recovering arm `:571`, the
// propagation `:579`, and the poisoning itself is in `handle_event`, `:197-204`.
//
// So a sentinel after the refusal cannot run, and these three place it
// **before**. What it proves is correspondingly narrower, and the narrow version
// is the one claimed: *the run was alive up to the refusal, and the writes it
// made before it are in the run's shadow.* Not "supervision survived the
// error" -- it did not. `journal_records_completion` is what carries that half,
// exactly as it does for `rollbackchild` above, and `run.status != Some(0)`
// alone would be vacuous without it: a fixture that merely returned 71 lands
// there too.
//
// Only `missingparent` is non-fatal, so it is the only one that places its
// sentinel after the operation and the only one that can call `child_exit()`.
//
// **All four assert today's measured answer, not the answer POSIX gives.** Two
// of them characterize #156 and one #81, and each doc comment says what to
// change when its issue is fixed. The #127 pair at the top of this file is the
// precedent.
// ---------------------------------------------------------------------------

/// **A creating `open` under an ancestor that is not there succeeds, and its
/// bytes land in the export** --
/// [#67](https://github.com/invakid404/umbra/issues/67), end to end on a routed
/// run.
///
/// POSIX answers `ENOENT` for `open("a/b", O_CREAT)` when `a` does not exist.
/// The routed namespace takes `create_parents` from `flags.create`, swallows the
/// absent non-final component during the walk, and materializes the ancestor as
/// a directory -- so the `open` succeeds. This asserts that, positively,
/// because it is what happens today. When #67 is fixed this case goes red on
/// purpose: invert it to require a nonzero child exit and to assert the nested
/// object is absent from the export.
///
/// **That inversion is not complete on the Rust side alone -- the fixture's
/// sentinel has to move with it.** The sentinel sits *after* the characterized
/// `open`, so a fixed #67 makes the `open` fail, `case_missingparent` returns
/// `errno` before it ever reaches `sentinel()`, and the sentinel object is never
/// written -- which fails the sentinel assertion below too, for a reason that
/// has nothing to do with liveness. Measured unrouted, where the `open` already
/// fails today: the case exits 2 and writes nothing at all, sentinel included.
/// So on the fix, move the sentinel **before** the `open`, where the three
/// run-fatal cases already place theirs, and the liveness evidence survives the
/// inversion.
///
/// **What is new here is the routed half alone, and the distinction is worth
/// keeping straight.** Whether the divergence is real was already settled before
/// this case existed, by two passing `umbra-overlay` tests that execute it
/// in-process against `LocalStorage`. Neither says anything about a routed run,
/// where three things differ and none of them had been measured: the ancestor is
/// created through `NfsUserspaceStorage` and the NFSv4 client rather than by
/// `mkdir`(2), the shadow is an export over a read-only host base rather than a
/// local directory, and the `open` arrives through the breakpoint path rather
/// than as a direct `FsOp`. This is the first evidence that the divergence
/// survives all three.
///
/// The sentinel is placed **after** the operation, which only this one of the
/// four cases can do: its bytes prove the tracee was resumed *past* the `open`
/// rather than merely alive before it. `child_exit()` is available for the same
/// reason, and is unavailable in the three cases below.
#[test]
fn a_creating_open_under_an_absent_parent_succeeds_against_live_ganesha_67() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_edge(scratch.path(), &host, port, "missingparent", &[]);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    // The fixture's own verdict: the `open` returned a descriptor, the write and
    // close both worked, and the sentinel after them did too. A nonzero exit
    // here is this case's own diagnosis that #67 no longer reproduces.
    assert_eq!(
        run.child_exit(),
        0,
        "a creating open under an absent parent did not succeed -- see this \
         case's doc comment before changing anything:\n{}",
        run.stderr
    );
    assert_eq!(
        run.status,
        Some(0),
        "the run did not finish cleanly:\n{}",
        run.stderr
    );

    // The claim, read back through the client with nothing mounted: nothing ever
    // created `<destination>.mp`, and the object under it is in the export
    // anyway.
    assert_eq!(
        read_through_client(&host, port, &rustio_sibling(&run, b".mp", Some(b"file")))
            .expect("the object under the absent ancestor is in the export")
            .as_slice(),
        b"missingparent",
        "the nested object does not hold the bytes the fixture wrote"
    );

    // The sentinel, which ran *after* the characterized `open`.
    assert_eq!(
        read_through_client(
            &host,
            port,
            &rustio_sibling(&run, b".missingparentlive", None)
        )
        .expect("the liveness sentinel is in the export")
        .as_slice(),
        b"missingparent",
        "the sentinel's bytes are not this case's own"
    );

    assert!(
        journal_records_completion(&run),
        "a run that finished cleanly recorded no RunCompleted"
    );
    assert_host_write_root_empty(&run);
    assert_workspace_pristine(&run);
}

/// **A second `O_CREAT|O_EXCL` on a path the run already created ends the run**
/// rather than answering the tracee `EEXIST` --
/// [#156](https://github.com/invakid404/umbra/issues/156).
///
/// `resolve` returns `Err(ErrorKind::AlreadyExists, "exclusive create target
/// exists")`, which is not the one recovered shape, so it propagates out of
/// `syscall_entry` (`events.rs:579`) and `handle_event` poisons the run
/// (`:197-204`). The tracee never sees the collision
/// and is never resumed, which is why the sentinel is written **before** it and
/// why `child_exit()` is not called here: a run stopped this way never prints
/// the `finished:` line that function parses, and calling it would panic.
///
/// **What this pins, exactly.** `status != Some(0)` is the weakest of the three
/// assertions and would hold for a merely-nonzero child as well -- the fixture
/// returns 71 if the refusal ever stops firing, and that lands here too. The
/// absent `RunCompleted` record is what tells those apart, because a run that
/// ended with a nonzero child still reaches `finish_run` and still records it.
/// And the sentinel's bytes, read back out of a run with no completion record,
/// are the structural half: the writes made before the refusal are in the run's
/// shadow.
///
/// When #156 is fixed, the tracee sees `EEXIST` and the run finishes: this case
/// then returns 71. Rewrite it to require `child_exit() == 17`, drop the journal
/// assertion, and change the fixture to report the `errno` rather than 71.
#[test]
fn an_exclusive_create_collision_ends_the_run_rather_than_answering_eexist() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_edge(scratch.path(), &host, port, "exclcollide", &[]);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_ne!(
        run.status,
        Some(0),
        "the refused exclusive create did not stop the run:\n{}",
        run.stderr
    );

    // The sentinel was written before the collision, so these bytes say the run
    // was alive up to it -- and they name this case, so a cross-wired read-back
    // cannot satisfy the assertion.
    assert_eq!(
        read_through_client(
            &host,
            port,
            &rustio_sibling(&run, b".exclcollidelive", None)
        )
        .expect("the liveness sentinel is in the run's shadow")
        .as_slice(),
        b"exclcollide",
        "the sentinel's bytes are not this case's own"
    );

    // And the first exclusive create's own object, the one the collision was
    // against, is still in the export with its bytes.
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run))
            .expect("the first exclusive create's object is in the run's shadow")
            .as_slice(),
        b"exclcollide",
        "the collision took the bytes written before it"
    );

    assert!(
        !journal_records_completion(&run),
        "a run stopped by a refused exclusive create still recorded RunCompleted"
    );
    assert_host_write_root_empty(&run);
    assert_workspace_pristine(&run);
}

/// **`std::fs::create_dir_all` on a directory that already exists ends a routed
/// run** -- [#156](https://github.com/invakid404/umbra/issues/156).
///
/// That framing is the measured one and it is deliberately not the weaker
/// "wrong errno" or "false success": there is no wrong errno here and nothing
/// reports success. `resolve` returns `Err(ErrorKind::AlreadyExists, "mkdir
/// target exists")` and the run dies.
///
/// **Why `create_dir_all` is the headline rather than a bare `mkdir`.** Measured
/// on this host: `create_dir_all` on an already-existing directory issues
/// exactly one `mkdir`(2) on that path, unconditionally and before any existence
/// check, then swallows the `EEXIST` and returns `Ok`. `mkdir` is a
/// `TRACED_STUBS` row, so that call reaches `FsOp::Mkdir` and the run is gone.
/// Every Rust program that ensures an output directory before writing into it
/// does this, which is what makes the case worth a run of its own.
///
/// Sentinel **before**, `child_exit()` not called, and the absent
/// `RunCompleted` record carrying the claim -- all for the reasons
/// `an_exclusive_create_collision_ends_the_run_rather_than_answering_eexist`
/// states. When #156 is fixed this case returns 71; rewrite it to require
/// `child_exit() == 17` and drop the journal assertion -- which also needs the
/// matching fixture change `case_mkdirexists` names, reporting the second
/// `mkdir`'s `errno` instead of reaching 71.
#[test]
fn a_mkdir_on_an_existing_directory_ends_the_run_rather_than_answering_eexist() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_edge(scratch.path(), &host, port, "mkdirexists", &[]);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_ne!(
        run.status,
        Some(0),
        "the refused mkdir on an existing directory did not stop the run:\n{}",
        run.stderr
    );

    assert_eq!(
        read_through_client(
            &host,
            port,
            &rustio_sibling(&run, b".mkdirexistslive", None)
        )
        .expect("the liveness sentinel is in the run's shadow")
        .as_slice(),
        b"mkdirexists",
        "the sentinel's bytes are not this case's own"
    );

    // The directory the first `mkdir` created is still there, and it is a
    // directory rather than whatever the lookup chain happened to resolve to.
    // `attributes_through_client` rather than `read_through_client`, which
    // requires a regular file before it reads a byte.
    assert_eq!(
        attributes_through_client(&host, port, &rustio_sibling(&run, b".me", None))
            .expect("the first mkdir's directory is in the run's shadow")
            .file_type,
        Some(Nfs4Type::Directory),
        "the first mkdir did not leave a directory behind"
    );

    assert!(
        !journal_records_completion(&run),
        "a run stopped by a refused mkdir still recorded RunCompleted"
    );
    assert_host_write_root_empty(&run);
    assert_workspace_pristine(&run);
}

/// **Removing a directory that is not empty ends the run** rather than
/// answering the tracee `ENOTEMPTY` --
/// [#81](https://github.com/invakid404/umbra/issues/81).
///
/// `resolve` enumerates the merged directory and returns
/// `Err(ErrorKind::Denied, "rmdir target is not empty")`, which propagates and
/// poisons the run. The overlay's own unit test for this refusal asserts
/// `!poisoned` on the `Overlay` object and that nothing was journaled; neither
/// contradicts this. Those say the *session* is reusable. The `Err` still
/// reaches `handle_event`, which poisons the *run* and sets
/// `RunLifecycle::RecoveryRequired`, and that is the half only an end-to-end
/// case can observe.
///
/// **The fixture uses `unlinkat(AT_FDCWD, dir, AT_REMOVEDIR)` and the choice is
/// load-bearing.** Bare `rmdir`(2) has no `TRACED_STUBS` row and is not one of
/// the interposer's four entries, so it is intercepted by neither mechanism: it
/// reaches the kernel against a path whose shadow object has no host existence
/// and answers `ENOENT`. A case written that way would look like an ordinary
/// error and would measure nothing at all about this refusal. The fixture's own
/// comment pins it so a later simplification cannot void the case quietly.
///
/// Sentinel **before**, `child_exit()` not called, journal record absent. When
/// #81 lands its `Deny(ENOTEMPTY)` this case returns 71; rewrite it to require
/// `child_exit() == 66` -- `ENOTEMPTY` is 66 on Darwin -- and drop the journal
/// assertion, which also needs the matching fixture change `case_rmdirfull`
/// names, reporting the `unlinkat`'s `errno` instead of reaching 71.
#[test]
fn a_non_empty_directory_removal_ends_the_run_rather_than_answering_enotempty_81() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_edge(scratch.path(), &host, port, "rmdirfull", &[]);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_ne!(
        run.status,
        Some(0),
        "the refused non-empty removal did not stop the run:\n{}",
        run.stderr
    );

    assert_eq!(
        read_through_client(&host, port, &rustio_sibling(&run, b".rmdirfulllive", None))
            .expect("the liveness sentinel is in the run's shadow")
            .as_slice(),
        b"rmdirfull",
        "the sentinel's bytes are not this case's own"
    );

    // The directory survived the refusal, which is what makes "not empty" the
    // reason it was refused rather than a guess.
    assert_eq!(
        attributes_through_client(&host, port, &rustio_sibling(&run, b".rf", None))
            .expect("the directory is in the run's shadow")
            .file_type,
        Some(Nfs4Type::Directory),
        "the refused removal took the directory anyway"
    );

    // And so did the child that made it non-empty, with its bytes.
    assert_eq!(
        read_through_client(&host, port, &rustio_sibling(&run, b".rf", Some(b"child")))
            .expect("the directory's child is in the run's shadow")
            .as_slice(),
        b"rmdirfull",
        "the entry that made the directory non-empty is not in the export"
    );

    assert!(
        !journal_records_completion(&run),
        "a run stopped by a refused non-empty removal still recorded RunCompleted"
    );
    assert_host_write_root_empty(&run);
    assert_workspace_pristine(&run);
}

// ---------------------------------------------------------------------------
// Standard utilities over the userspace client.
//
// `crates/umbra-cli/tests/run_fixtures.rs` already has a `local_utility_matrix`
// and an `nfs_utility_matrix`, and both are rewrite-backed: the shadow is a host
// path, so a case can read it with `std::fs`. This is the third matrix, on the
// **userspace** registry, and it lives here rather than beside those two for the
// reason this file's header states -- nothing is mounted, so "the object is in
// the store" can only honestly be checked by speaking NFSv4 to the server, which
// needs `transport-raw` and therefore this crate.
//
// The cases are `/bin/mkdir`, `/usr/bin/touch` and `/bin/cat`: Apple's own
// binaries, not a fixture this repository compiled. `/bin/ls` is deliberately
// absent and is not claimed -- directory reads are unrouted in every form (see
// LIMITS item 6 in `umbra_interpose.c`), and a partial `ls` lists correct names
// while exiting 1, which is a dishonest exit code rather than a cheap `ls`.
//
// **What is not asserted: printed stdout.** The provider transport points the
// tracee's stdout at `/dev/null`, so a filter's output is not observable from
// here -- and pinning it empty would pin that limitation rather than any
// behaviour. `cat`'s proof is the pair of legs `run_fixtures.rs` established:
// the present operand exits 0 with the host untouched, and the absent one makes
// `cat` name itself, its operand and the verdict on stderr, which no program
// that did not run can compose.
// ---------------------------------------------------------------------------

/// What one standard utility must leave behind after one routed `umbra run`.
enum Utility {
    /// A directory at the operand, in the store, with the host untouched.
    Directory,
    /// An empty regular file at the operand, in the store, host untouched.
    ///
    /// Exit zero is the `fstat` proof and is the reason this case exists.
    /// Measured before `fstat`(339) was routed: `touch <absent>` *did* create
    /// the file and then exited **1** with `touch: <path>: Bad file
    /// descriptor`, because its `fstat` on the virtual descriptor reached a
    /// kernel that does not know the number. Creating the file was never the
    /// hard part; reporting honestly that it had been created was.
    EmptyFile,
    /// The seeded file, copied up with its bytes intact and its modification
    /// time moved into this run's window.
    ///
    /// **The bound is a sanity check, not the discriminator, and saying so is
    /// the point.** Copy-up does not carry the base object's times, so a shadow
    /// copy made by copy-up alone would also carry a time inside the window --
    /// this case cannot tell that apart from a time `setattrlistat` set. What
    /// can, and does, is `mutation_probe_setattrlistat_makes_touch_on_an_\
    /// existing_file_fail`: with the decode refused, this exact case exits
    /// nonzero while the `EmptyFile` case still passes.
    TimesAdvanced,
    /// The utility read an operand that exists, exited zero, and left the host
    /// workspace byte-identical.
    ReadOnly,
    /// The operand does not exist, so the utility fails and names it on stderr.
    Diagnoses,
}

/// One `umbra run` of one standard utility against a fresh, seeded workspace.
///
/// A run is a single launch, so the utilities cannot be chained: each case gets
/// its own workspace, its own state directory and its own run.
fn utility_run(
    scratch: &Path,
    host: &str,
    port: u16,
    index: usize,
    program: &str,
    relative: &str,
) -> (Run, PathBuf) {
    let workspace = scratch.join(format!("utility-{index}"));
    let state = scratch.join(format!("utility-state-{index}"));
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let operand = workspace.join(relative);
    let registry = registry(scratch, host, port);

    let output = Command::new(binaries().join("umbra"))
        .args(["run", "--registry"])
        .arg(&registry)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--state-dir")
        .arg(&state)
        .arg("--experimental")
        .arg("--")
        .arg(program)
        .arg(&operand)
        .stdin(Stdio::null())
        .output()
        .expect("umbra run");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let run_id = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("umbra: run ")
                .and_then(|line| line.strip_suffix(" prepared"))
        })
        .unwrap_or_else(|| panic!("no prepared run id in:\n{stderr}"));
    let run = Run {
        run_id: RunId(uuid::Uuid::parse_str(run_id).unwrap()),
        status: output.status.code(),
        stderr,
        destination: operand.clone(),
        state,
        workspace,
        registry,
    };
    (run, operand)
}

/// One object's attributes in the export, through the client, with nothing
/// mounted. `None` means the name does not exist.
///
/// The sibling of `read_through_client`, split out rather than folded into it
/// because that function asserts the object is a regular file before it reads:
/// a `mkdir` case has to be able to ask what kind of object is there, and a
/// `touch` case has to read a time, neither of which is a byte of content.
fn attributes_through_client(
    host: &str,
    port: u16,
    components: &[Vec<u8>],
) -> Option<umbra_storage_nfs_userspace::transport::Attributes> {
    let mut config = RawTransportConfig::loopback(port);
    config.host = host.to_owned();
    config.limits.default_deadline = Deadline { millis: 10_000 };
    let deadline = config.limits.default_deadline;
    let mut transport = LibnfsRawTransport::connect(config).expect("connect to the fixture");
    let mut handle = transport
        .root_filehandle(deadline)
        .expect("PUTROOTFH; GETFH");
    let mut attributes = None;
    for component in components {
        let name = ComponentName::new(component.clone()).expect("component");
        let (next, found) = transport
            .lookup(&handle, &name, AttrMask::STAT, deadline)
            .ok()?;
        handle = next;
        attributes = Some(found);
    }
    Some(attributes.expect("at least one component"))
}

/// The components of one run's shadow object for an arbitrary operand.
fn utility_shadow(run: &Run, operand: &Path) -> Vec<Vec<u8>> {
    let mut components = vec![
        EXPORT.to_vec(),
        RUN_PARENT.to_vec(),
        run.run_id.0.to_string().into_bytes(),
        b"root".to_vec(),
    ];
    components.extend(
        operand
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec),
    );
    components
}

/// Seconds since the epoch, for bounding a modification time against the window
/// the run actually occupied.
fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs() as i64
}

/// The matrix itself. Returns the cases so each probe can name the one it breaks.
const UTILITY_CASES: &[(&str, &str, Utility)] = &[
    ("/bin/mkdir", "bar", Utility::Directory),
    ("/usr/bin/touch", "foo", Utility::EmptyFile),
    ("/usr/bin/touch", "seed.txt", Utility::TimesAdvanced),
    ("/bin/cat", "seed.txt", Utility::ReadOnly),
    ("/bin/cat", "absent.txt", Utility::Diagnoses),
    // `/bin/ls` on the workspace root. It carried `Expect::ReadOnly` in the
    // rewrite-backed matrix long before this slice and passed there, because a
    // rewrite-backed descriptor is a kernel descriptor and every directory
    // syscall reaches the kernel. On *this* registry the descriptor is virtual,
    // and until directory reads were routed the run did not merely fail -- it
    // stopped, at the routed `open`, with `routed open of a directory`, for
    // every operand shape including a plain file and an absent path (`fts` opens
    // `"."` before it looks at the operand).
    //
    // **What this row proves is exit 0 and an untouched host, not the listing.**
    // A traced tracee inherits the platform provider's standard output, which is
    // `Stdio::null()`, so no test can read what `ls` printed. That is exactly
    // why the design gate ratified asserting on *entries* instead of on the exit
    // code, and the entries are proven by
    // `a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client`
    // below, which drives the same `libsystem_c` `fts` code through the same
    // routed calls and writes what it read where the client can read it back.
    ("/bin/ls", "", Utility::ReadOnly),
];

/// Run one case and assert everything it claims. `before` is the epoch second
/// read immediately ahead of the launch.
fn assert_utility_case(
    host: &str,
    port: u16,
    run: &Run,
    operand: &Path,
    case: &str,
    expect: &Utility,
    before: i64,
) {
    let shadow = utility_shadow(run, operand);
    match expect {
        Utility::Directory | Utility::EmptyFile | Utility::TimesAdvanced | Utility::ReadOnly => {
            assert_eq!(
                run.status,
                Some(0),
                "{case}: umbra run failed (child {:?}):\n{}",
                run.stderr
                    .lines()
                    .find(|line| line.contains("finished:"))
                    .unwrap_or("<no status line>"),
                run.stderr
            );
        }
        Utility::Diagnoses => {
            assert_ne!(run.status, Some(0), "{case}: {}", run.stderr);
            let reported = format!(
                "{}: {}: No such file or directory",
                Path::new("/bin/cat").file_name().unwrap().to_string_lossy(),
                operand.display()
            );
            assert!(
                run.stderr.contains(&reported),
                "{case}: expected {reported:?} on the tracee's stderr:\n{}",
                run.stderr
            );
        }
    }
    match expect {
        Utility::Directory => {
            let found = attributes_through_client(host, port, &shadow)
                .unwrap_or_else(|| panic!("{case}: the operand is not in the export"));
            assert_eq!(
                found.file_type,
                Some(Nfs4Type::Directory),
                "{case}: the store holds {:?}, not a directory",
                found.file_type
            );
        }
        Utility::EmptyFile => {
            let stored = read_through_client(host, port, &shadow)
                .unwrap_or_else(|| panic!("{case}: the operand is not in the export"));
            assert!(
                stored.is_empty(),
                "{case}: touch left {} bytes in the export",
                stored.len()
            );
        }
        Utility::TimesAdvanced => {
            let stored = read_through_client(host, port, &shadow)
                .unwrap_or_else(|| panic!("{case}: the operand is not in the export"));
            assert_eq!(stored, b"seed\n", "{case}: the copy-up lost the bytes");
            let found = attributes_through_client(host, port, &shadow)
                .unwrap_or_else(|| panic!("{case}: the operand is not in the export"));
            let modified = found
                .time_modify
                .unwrap_or_else(|| panic!("{case}: the server returned no TIME_MODIFY"));
            // A second of slack on the lower bound only, for a server whose
            // clock is a tick behind this process's. The upper bound is read
            // after the run, so no slack is owed there.
            assert!(
                modified.seconds >= before - 1 && modified.seconds <= epoch_seconds() + 1,
                "{case}: the shadow's mtime {} is outside this run's window [{before}, {}]",
                modified.seconds,
                epoch_seconds()
            );
        }
        Utility::ReadOnly | Utility::Diagnoses => {}
    }
    // The host, in both the places the operation could have escaped to.
    if !matches!(expect, Utility::TimesAdvanced) {
        assert!(
            !operand.exists() || matches!(expect, Utility::ReadOnly),
            "{case}: the host operand {} was created",
            operand.display()
        );
    }
    assert_workspace_pristine(run);
    assert_host_write_root_empty(run);
    assert!(
        journal_records_completion(run) || !matches!(run.status, Some(0)),
        "{case}: a successful run with no RunCompleted record"
    );
}

#[test]
fn standard_utilities_run_over_the_userspace_client() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    // The same provisioning switch the rewrite-backed matrices use -- the
    // `UMBRA_TEST_FIXTURE_PATH` gate at the head of `run_fixtures.rs`'s
    // `utilities()`: it is the suite's existing signal for "this host has
    // workspace binaries built and debugger permission granted", which is
    // exactly what a real traced launch of an Apple binary needs. A second
    // switch is not invented for it. The endpoint gate below is this file's own
    // and is not a second provisioning switch either -- without a live server
    // there is no userspace registry to point at.
    //
    // **Read as a signal, not a path.** This case never launches what the
    // variable points at; the programs it runs are Apple's own. The userspace
    // job points it at the toy it already builds, which is why that is not a
    // category error.
    //
    // **Through `input()` rather than a bare `var_os`, and that is the fix for
    // a real failure.** A bare check skips silently. This case shipped that
    // way, the CI job that could run it never set the variable, and the job
    // reported `ok` while proving nothing -- under a job name that claims the
    // utilities work over the userspace client. `input()` asserts when
    // `UMBRA_INTEGRATION_REQUIRED` is set, which that job sets for the whole
    // run, so an unwired proof is now a red job rather than a quiet pass. A
    // developer without the fixture still gets a skip.
    let Some(_signal) = input("UMBRA_TEST_FIXTURE_PATH") else {
        return;
    };
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the utility matrix");
    for (index, (program, relative, expect)) in UTILITY_CASES.iter().enumerate() {
        let case = format!("{program} <workspace>/{relative}");
        let before = epoch_seconds();
        let (run, operand) = utility_run(scratch.path(), &host, port, index, program, relative);
        assert_utility_case(&host, port, &run, &operand, &case, expect, before);
        eprintln!("PASS userspace {case}: store holds it, host untouched");
    }
    assert_no_nfs_mount(&host, port, &paths, "after the utility matrix");
}

/// Every probe this file defines is actually wired into the job that can run
/// it.
///
/// **This is the guard for the failure that produced it.** All six probes and
/// the utility matrix were written, merged and reported green while the
/// `Userspace-routed run over the live NFSv4 client` job ran none of them: it
/// carried #116's three proof steps and never set `UMBRA_MUTATION_PROBE` to
/// `fstat`, `mkdir` or `setattrlistat`, nor rebuilt `umbra-platform-macos` with
/// the two features that live there. Each probe dutifully skipped and reported
/// `ok`. The job name says the utilities are proven over the userspace client;
/// nothing had run.
///
/// The matrix's own gate is hard-failed under `UMBRA_INTEGRATION_REQUIRED` now,
/// which closes that half. A probe cannot be hard-failed the same way -- seven
/// probes across eight invocations means six or seven legitimately skip every
/// time -- so the property is checked here instead: for each probe, the
/// workflow must both select it and build the crate that carries it.
///
/// **What this does not cover**, said plainly because overclaiming a remedy is
/// how the last two rounds went wrong: it reads the workflow as text. It does
/// not prove the job runs, that the runner exists, that the steps are ordered
/// so no two probes are live at once, or that a step's `cargo test` selector
/// reaches this file. It proves that a probe named here is named there too,
/// which is exactly the link that was missing.
#[test]
fn every_mutation_probe_is_wired_into_the_userspace_job() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    let job = workflow
        .split_once("native-userspace-routing:")
        .expect("the userspace-routing job is defined in ci.yml")
        .1;
    // The crate whose cargo feature carries each probe. `read`/`write`/`fstat`
    // are `umbra-overlay` features reaching `target/debug/umbra` through
    // `umbra-cli`; `mkdir`/`setattrlistat` are `umbra-platform-macos`
    // features reaching that provider executable, which `umbra-cli` does not
    // link and so cannot forward.
    for (probe, package) in [
        ("read", "umbra-cli"),
        ("write", "umbra-cli"),
        ("fstat", "umbra-cli"),
        ("mkdir", "umbra-platform-macos"),
        ("setattrlistat", "umbra-platform-macos"),
        // `readdir` breaks the production `DirectoryEncoder`, which is a
        // `umbra-supervisor` type reaching `target/debug/umbra` through
        // `umbra-cli` -- so it is built like the first three, not like the two
        // provider-executable probes.
        ("readdir", "umbra-cli"),
        // `dircache` removes `Engine::commit`'s eviction of a closed
        // descriptor's residual directory page. An `umbra-overlay` feature like
        // the first three, so it reaches the same binary the same way.
        ("dircache", "umbra-cli"),
    ] {
        assert!(
            job.contains(&format!("UMBRA_MUTATION_PROBE: {probe}")),
            "probe {probe} is defined in this file but the userspace job never              selects it, so it skips and reports ok"
        );
        assert!(
            job.contains(&format!(
                "cargo build -p {package} --features mutation-probe-{probe}"
            )),
            "probe {probe} is selected by the userspace job but the job never              builds {package} with it, so the run is unmutated and the case              fails for the wrong reason"
        );
    }
    // The matrix needs no probe; it needs its provisioning signal.
    assert!(
        job.contains("UMBRA_TEST_FIXTURE_PATH="),
        "the userspace job does not set UMBRA_TEST_FIXTURE_PATH, so          `standard_utilities_run_over_the_userspace_client` cannot run there"
    );
    // Both mutated binaries must be restored: this runner is persistent.
    let restore = job
        .split_once("Restore unmutated binaries")
        .expect("the job restores unmutated binaries")
        .1;
    for package in ["umbra-cli", "umbra-platform-macos"] {
        assert!(
            restore.contains(&format!("cargo build -p {package}
")),
            "the restore step does not rebuild an unmutated {package}, leaving a              probe in target/debug for whatever runs on this runner next"
        );
    }
}

/// **The entry proof: a directory read on a virtual descriptor returns the
/// shadow's merged names, through the same `fts` that `/bin/ls` runs.**
///
/// This is the case the whole slice exists for, and it asserts on *names*
/// because nothing else can. The design gate recorded the measurement: nine
/// errnos swept through `getattrlistbulk` all leave `ls` on exit 1, and so do
/// the shipped mutation probes, so **the exit code cannot discriminate** a
/// served directory read from a refused one. Worse, it cannot discriminate a
/// correct listing from an *empty* one -- a `getattrlistbulk` that answers "zero
/// entries" means end-of-directory, and `ls` then prints nothing and exits 0.
/// Measured, during this slice: with the encoder wired but the descriptor's
/// object kind still reported as a non-directory, the run exited 0 having listed
/// nothing at all.
///
/// **Why a fixture rather than `/bin/ls` itself.** A traced tracee inherits the
/// platform provider's standard output, and providers are spawned with
/// `Stdio::null()`, so no harness can read what a traced program printed.
/// `/bin/ls` is therefore observable only by its exit status, which the
/// paragraph above disqualifies. The fixture writes the names it read into the
/// routed workspace instead, and this reads them back out of the export through
/// the userspace NFSv4 client with nothing mounted.
///
/// **It is not a re-implementation of `ls`.** It calls `fts_open`/`fts_read`/
/// `fts_children` with `FTS_PHYSICAL | FTS_NOSTAT` and `FTS_NAMEONLY` -- the
/// flags plain `ls` passes -- so the consumer of umbra's encoded records is
/// Apple's own `fts` inside `libsystem_c.dylib`, the same code `/bin/ls` runs.
/// Calling `getattrlistbulk` directly would only check umbra's encoder against
/// umbra's own idea of the format.
///
/// What it therefore proves together: the routed directory `open` is no longer
/// refused, `getattrlistbulk`(461) is decoded and answered from the merged view,
/// the reply is a record layout `fts` accepts, `fchdir`(13) on a virtual
/// descriptor succeeds, `__close_nocancel`(399) releases it, and the names that
/// come out are the shadow's.
#[test]
fn a_directory_listing_through_fts_reaches_the_tracee_over_the_userspace_client() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let names = ["alpha.txt", "beta.txt", "gamma.txt"];
    let (run, destination) = listing_run(scratch.path(), &host, port, &names);
    assert_eq!(
        run.status,
        Some(0),
        "the listing fixture failed over the userspace client:\n{}",
        run.stderr
    );
    assert_eq!(
        listed_names(&host, port, &run, &destination),
        names,
        "the names read back through the client are not the shadow's entries"
    );
    // The host keeps none of it: not the listing the fixture wrote, and not the
    // directory it read.
    assert!(
        !destination.exists(),
        "the listing escaped to the host at {}",
        destination.display()
    );
    assert_listing_workspace_pristine(&run, &names);
    assert_host_write_root_empty(&run);
}

/// Probe F -- the directory reply's names are corrupted.
///
/// **The probe the design gate ratified in place of a distinct exit code**, and
/// the reason it asserts the way it does is the reason that substitution was
/// needed. Every refusal of a directory read, at every errno, leaves the tracee
/// on exit 1, and the three shipped `umbra-cli` probes land there too; an empty
/// listing leaves it on exit 0. So the status says nothing, and only the names
/// can tell the difference.
///
/// The probe reverses each name and changes nothing else: every record length,
/// name reference and alignment is what the encoder would have written, the
/// reply still walks, and the tracee still exits 0. So the negative half here is
/// not "the run broke" -- it is that the *names are wrong* while everything
/// around them still works.
///
/// **The positive half is load-bearing**, for the reason probe D's doc records:
/// a probe that passes for a cause outside its own mutation certifies exactly
/// what it cannot detect. So this also requires the run to still exit 0 and to
/// still have produced a file of the right shape -- one line per entry, the
/// right number of them, each one a *permutation* of a real name. Nothing
/// upstream of the encoding can satisfy that and still get the names wrong: a
/// broken open, a refused `getattrlistbulk`, a dead `fchdir` or an unrouted
/// `close` all fail to produce the file at all.
#[test]
fn mutation_probe_readdir_makes_the_listed_names_wrong() {
    if declared_probe().as_deref() != Some("readdir") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=readdir with a binary built with \
                   --features mutation-probe-readdir"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    // Deliberately not palindromes: the probe reverses each name, so a
    // palindromic one would come back correct and weaken the assertion.
    let names = ["alpha.txt", "beta.txt", "gamma.txt"];
    let (run, destination) = listing_run(scratch.path(), &host, port, &names);
    // Positive half: the probe breaks the names and nothing else.
    assert_eq!(
        run.status,
        Some(0),
        "the probe was meant to corrupt names, not to break the run:\n{}",
        run.stderr
    );
    let listed = listed_names(&host, port, &run, &destination);
    assert_eq!(
        listed.len(),
        names.len(),
        "the probe changed how many entries were reported, so it is not isolated \
         to the encoded names: {listed:?}"
    );
    for name in &listed {
        let restored: String = name.chars().rev().collect();
        assert!(
            names.contains(&restored.as_str()),
            "{name:?} is not a reversal of any real entry, so something other \
             than the probe changed the reply"
        );
    }
    // Negative half, and the whole point: the names are wrong.
    assert_ne!(
        listed, names,
        "breaking the encoded names did not change what the client reads back, \
         so this proof never depended on the directory encoding at all"
    );
    assert!(!destination.exists());
    assert_listing_workspace_pristine(&run, &names);
    assert_host_write_root_empty(&run);
}

/// **A project-shaped directory, enumerated twice over one reused descriptor
/// number, paginating both times.** The regression guard the control cannot be.
///
/// **What it guards is live and was un-exercised.** The residual-page cache is
/// keyed on the descriptor *number* (`umbra-overlay/src/engine.rs`'s
/// `DirectoryKey`), a routed `close` marks the descriptor closed, and
/// `Engine::commit` evicts the key. That eviction works -- and until this case
/// nothing ran it, because every shipped fixture reads each directory once.
/// `crates/umbra-storage-nfs-userspace/README.md:688` records what that costs:
/// an earlier defect here was first mis-diagnosed as a recursion problem, and
/// *"fixtures that read each directory once, which is how this was first
/// checked, could not have shown either the defect or the fix."* This is that
/// fixture, and the `dircache` probe below is what proves the assertion is not
/// vacuous.
///
/// What the control proves and this adds, item by item:
///
/// - **Pagination.** The control's three names encode to 192 bytes, one reply.
///   This tree encodes to 37 416, so at least two replies carry entries and
///   every page after the first comes from the cached remainder. The assertion
///   calls the production encoder to establish that rather than hard-coding a
///   count, so editing a name or the ballast count cannot silently turn the
///   case back into a one-reply one while it keeps passing.
/// - **Record shapes.** Two hidden names, four with spaces, a 2-byte and a
///   3-byte UTF-8 name, and two directory records -- which are 4 bytes shorter
///   than a file's. Under byte-sorted packing every one of those except the two
///   leading hidden names falls on the *second* reply, so they are served from
///   the cache-hit path rather than the first `merged` call.
/// - **Descriptor reuse, and why it is not assumed.** umbra allocates the
///   lowest free number at or above the run's floor, so close-then-reopen
///   reuses it; `dup2` would not work here at all, since `FsOp::Dup` is decoded
///   nowhere and reaches the kernel as `EBADF`. The fixture records the number
///   it is handed immediately before each `fts_open`, and the two must match --
///   which is what makes the cache key collide, and therefore what makes pass 2
///   a test of eviction rather than of a fresh key.
/// - **Eviction.** Pass 2 reporting the same 239 names is the proof. Nothing is
///   written between the passes, so the only thing that can make the second
///   enumeration differ is what umbra did with the first one's leftovers: a
///   surviving residual page answers the empty remainder a completed
///   enumeration leaves, which `fts` reads as end-of-directory. The failure
///   this catches is therefore 0 names against 239, with exit 0 either way --
///   which is why it asserts on names and not on status.
/// - **Pagination does not contaminate the reuse.** The two properties are
///   asserted together on purpose: a tree that pages leaves a *different* cache
///   state behind than one that fits a single reply, and this is the only case
///   that shows the second enumeration is clean after a paged first one.
///
/// **What it deliberately does not claim.** The base-plus-shadow merge. An
/// earlier draft created two files in the enumerated directory between the
/// passes to prove it; measured, that write materialises the directory into the
/// shadow, and a routed `stat` by path on a shadow object is refused `ENOTSUP`
/// at `engine.rs:3020-3023` -- a limitation the comment above it already
/// defers. `fts` stats a root before walking it, so pass 2 reported nothing for
/// a reason unrelated to the cache. That claim belongs to the slice that lifts
/// the `Stat` limitation.
#[test]
fn a_project_tree_listing_paginates_and_survives_a_reused_descriptor_number() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let began = std::time::Instant::now();
    let (run, destination) = project_listing_run(scratch.path(), &host, port);
    let elapsed = began.elapsed();
    // `Overlay::merged` re-looks-up every entry, so 239 entries is several
    // hundred NFSv4 round trips per enumeration and this case is materially
    // slower than the control's three names. The registry's 12 s deadline is
    // per *request*, so no single call is at risk; the total is worth printing
    // so a slow runner is diagnosable rather than mysterious.
    eprintln!(
        "PASS userspace project listing: two passes over {} entries in {:.1}s",
        PROJECT_BALLAST + PROJECT_CORE_FILES.len() + PROJECT_CORE_DIRECTORIES.len(),
        elapsed.as_secs_f64()
    );
    assert_eq!(
        run.status,
        Some(0),
        "the project listing fixture failed over the userspace client:\n{}",
        run.stderr
    );

    let passes = project_passes(&listed_lines(&host, port, &run, &destination));
    let expected = project_tree_names();

    // Pass 1: the seeded tree, byte for byte.
    assert_eq!(
        passes[0].1, expected,
        "the first enumeration's names are not the shadow's entries"
    );

    // Pass 2: the same tree again, and this is the eviction proof. Nothing was
    // written between the passes, so the only thing that can make the second
    // enumeration differ from the first is what umbra did with the first one's
    // leftovers. A residual page that outlived its descriptor answers the empty
    // remainder a completed enumeration leaves behind, which `fts` reads as
    // end-of-directory -- so the failure this catches is 0 names against 239,
    // with the run still exiting 0.
    assert_eq!(
        passes[1].1, expected,
        "the second enumeration did not re-read the directory, so the residual \
         page outlived the descriptor that produced it"
    );

    // The pagination claim, asserted against the production encoder rather than
    // a transcribed formula: if one whole reply cannot hold the tree, at least
    // two replies carry entries and the second one came from the cache.
    let entries = project_directory_entries(&expected);
    let (_, consumed) = dirents::encode(&entries, FTS_REPLY_BYTES)
        .expect("the production encoder serves this tree's records");
    assert!(
        consumed < entries.len(),
        "the whole {}-entry tree fits in one {FTS_REPLY_BYTES}-byte reply \
         ({consumed} consumed), so this case no longer paginates and the \
         residual-page path it exists to cover is unexercised",
        entries.len()
    );

    // The residual page's *contents*, which is the part worth pinning as the
    // fixture is maintained.
    //
    // **What this can and cannot assert, stated precisely.** The output file is a
    // flat list of names with no reply boundaries in it, so which reply carried a
    // given name is **not observable from it at all** -- that was measured by
    // instrumenting `resolve_directory`, not by reading this file. Asserting that
    // a residual name "came back" would also be vacuous: the pass-1 equality
    // above already pins every name, and `residual` is a subslice of the same
    // expectation.
    //
    // What is *not* vacuous is the encoder's own split. Both UTF-8 names and both
    // directory records -- the record shapes that differ from the control's flat
    // ASCII files, a directory record being 4 bytes shorter for want of
    // `ATTR_FILE_LINKCOUNT` -- must fall beyond reply 1, because being served
    // from the cache-hit path rather than the first `merged` call is the whole
    // reason they are in the tree. An edit to a name or to the ballast count that
    // slid them onto reply 1 would leave every other assertion here passing while
    // quietly ending that coverage.
    let residual = &expected[consumed..];
    assert!(
        residual.len() > 1,
        "the model puts only {} entry on the second reply, which is too thin to \
         carry the record shapes this case exists to exercise",
        residual.len()
    );
    for shape in PROJECT_CORE_DIRECTORIES
        .iter()
        .chain(PROJECT_CORE_UTF8_NAMES.iter())
    {
        assert!(
            residual.contains(&shape.as_bytes().to_vec()),
            "{shape:?} is one of the record shapes this case exists to serve from \
             the cached residual page, and the production encoder now packs it \
             into the first reply ({consumed} of {} entries), so that coverage is \
             gone",
            expected.len()
        );
    }

    // The descriptor number was reused, which is what made the cache key
    // collide. Precisely: this proves the allocator's free-slot state was
    // identical at the start of both passes. Combined with lowest-free
    // allocation -- a property of `allocate_descriptor`'s source, not of this
    // measurement -- `fts` received the same number both times.
    assert_eq!(
        passes[0].0, passes[1].0,
        "the two enumerations started from different descriptor numbers, so the \
         cache key never collided and the second pass proves nothing about \
         eviction"
    );

    // The host keeps none of it: not the listing the fixture wrote, and not the
    // tree it read.
    assert!(
        !destination.exists(),
        "the listing escaped to the host at {}",
        destination.display()
    );
    assert_project_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
    assert!(
        journal_records_completion(&run),
        "the run's journal does not record completion"
    );
}

/// Probe F again, now across a paginated listing and a reused descriptor.
///
/// **What this adds over `mutation_probe_readdir_makes_the_listed_names_wrong`,
/// which is the only reason a second case for one probe is worth having.** That
/// one exercises a single reply of three short ASCII names. Nothing in it shows
/// that the *second* reply of an enumeration, or the *second* enumeration on a
/// reused descriptor, is produced by `AbiDirectoryEncoder::encode` at all rather
/// than fabricated somewhere else -- the residual page is served from a cached
/// entry list, and a reader is entitled to ask whether that path re-encodes.
/// Under this probe it must, in all four places, or the names come back right.
///
/// It follows the sibling's five-part shape, in **bytes**: the probe reverses
/// each name bytewise, and a reversed multibyte UTF-8 name is not valid UTF-8,
/// so `listed_names` would panic inside the helper rather than assert. That is
/// what `listed_lines` exists for.
#[test]
fn mutation_probe_readdir_corrupts_a_paginated_listing_across_a_descriptor_reuse() {
    if declared_probe().as_deref() != Some("readdir") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=readdir with a binary built with \
                   --features mutation-probe-readdir"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let (run, destination) = project_listing_run(scratch.path(), &host, port);
    // Positive half: the probe breaks the names and nothing else. None of this
    // tree's names is a palindrome -- the ballast names end in a run of `p` and
    // begin `ballast-`, and no core name reads the same backwards -- so a
    // reversal is always observable.
    assert_eq!(
        run.status,
        Some(0),
        "the probe was meant to corrupt names, not to break the run:\n{}",
        run.stderr
    );
    let passes = project_passes(&listed_lines(&host, port, &run, &destination));
    let expected = project_tree_names();

    // Positive: the counts are untouched, on both pages of both passes. A probe
    // that changed how many entries were reported would not be isolated to the
    // encoded names.
    assert_eq!(
        passes[0].1.len(),
        expected.len(),
        "the probe changed how many entries the first enumeration reported"
    );
    assert_eq!(
        passes[1].1.len(),
        expected.len(),
        "the probe changed how many entries the second enumeration reported"
    );

    // Positive: every name is a byte reversal of a real one, so what came back
    // is a permutation of the real reply and not some other failure's output.
    // Nothing upstream of the encoding can satisfy this and still get the names
    // wrong: a broken open, a refused `getattrlistbulk`, a dead `fchdir` or an
    // unrouted `close` all fail to produce the file at all.
    for pass in [&passes[0].1, &passes[1].1] {
        for name in pass {
            let restored: Vec<u8> = name.iter().rev().copied().collect();
            assert!(
                expected.contains(&restored),
                "{:?} is not a byte reversal of any real entry, so something \
                 other than the probe changed the reply",
                String::from_utf8_lossy(name)
            );
        }
    }

    // Positive: the descriptor was still reused, so the corruption is being
    // observed on the same key collision the unmutated case establishes rather
    // than on a different one.
    assert_eq!(
        passes[0].0, passes[1].0,
        "the two enumerations started from different descriptor numbers"
    );

    // Negative half, and the whole point: the names are wrong -- in the first
    // reply, in the cached residual page, and after the reuse.
    assert_ne!(
        passes[0].1, expected,
        "breaking the encoded names did not change what the first enumeration \
         reads back, so this proof never depended on the directory encoding"
    );
    assert_ne!(
        passes[1].1, expected,
        "breaking the encoded names did not change what the second enumeration \
         reads back, so the post-reuse reply is not encoder-produced"
    );

    assert!(!destination.exists());
    assert_project_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
}

/// Probe G -- the residual-page cache is never evicted on `close`.
///
/// **The probe that makes the eviction a live mutation instead of a historical
/// counterfactual.** The eviction is live on master and nothing exercised it,
/// so without this probe the arc's own second pass would be the only thing
/// asserting it -- and a test whose subject is never broken on purpose is a
/// test nobody has checked. It deletes exactly the `retain` in `Engine::commit`
/// and nothing else, so the insert still happens, the `close` is still routed
/// and journalled, and the tracee still sees success.
///
/// **The discriminator cannot be an exit code, and that is the point.** The
/// behaviour it restores leaves the tracee on *exit 0* -- the first
/// enumeration's names, then nothing. A completed enumeration leaves the cache
/// entry holding an **empty remainder**, `resolve_directory` prefers that cache
/// over `merged`, and an empty `getattrlistbulk` reply means end-of-directory --
/// so `fts` stops, the fixture writes what it has, and the run completes
/// normally. So this asserts on the second enumeration's *names and bytes*: it
/// must come back **empty** where the unmutated run reports all 239.
///
/// The positive half is load-bearing for the reason probe D's doc records -- a
/// probe that passes for a cause outside its own mutation certifies exactly what
/// it cannot detect. So the run must still exit 0, the *first* pass must still
/// report the full tree byte for byte, and the descriptor must still have been
/// reused. Those three together exclude every failure that is not this
/// mutation: a broken open, a refused directory read or a dead `fchdir` would
/// cost pass 1 its names, and a different descriptor number would mean pass 2
/// read a fresh key rather than a surviving one.
///
/// **One precondition this case relies on and does not assert.** The signature
/// it checks -- exit 0, pass 1 full, descriptor reused, pass 2 empty -- is
/// *also* exactly what a routed `stat` refused `ENOTSUP` produces: `fts` stats a
/// root before walking it, gets `FTS_NS`, and reports nothing while still
/// exiting 0 (measured; see `project_listing_run`'s doc and
/// `engine.rs:3009-3018`). That path is unreachable here only because the
/// fixture writes nothing into the enumerated directory, so the directory is
/// never materialised into the shadow. An edit that reintroduces such a write --
/// including moving the output file out of the workspace root and into the
/// enumerated directory -- would leave this probe **passing for the wrong
/// reason**, which is the one failure mode a mutation probe must not have.
/// Deliberately a note and not an assertion: distinguishing the two causes needs
/// reply-level observability the output file does not carry, and the non-vacuity
/// this probe does rest on is the ON/OFF pair, which is checked by running the
/// unmutated case against both feature states.
#[test]
fn mutation_probe_dircache_lets_a_closed_descriptors_page_outlive_it() {
    if declared_probe().as_deref() != Some("dircache") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=dircache with a binary built with \
                   --features mutation-probe-dircache"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let (run, destination) = project_listing_run(scratch.path(), &host, port);
    // Positive half: the probe breaks the second enumeration and nothing else.
    assert_eq!(
        run.status,
        Some(0),
        "the probe was meant to strand a cached page, not to break the run:\n{}",
        run.stderr
    );
    let passes = project_passes(&listed_lines(&host, port, &run, &destination));
    let expected = project_tree_names();

    // Positive: pass 1 is untouched, byte for byte. The mutation is on `close`,
    // so the first enumeration cannot be affected by it -- and if it were, the
    // negative half below would be satisfiable by a plain broken listing.
    assert_eq!(
        passes[0].1, expected,
        "the probe changed the first enumeration, so it is not isolated to the \
         eviction and the negative half proves nothing"
    );

    // Positive: the descriptor number was still reused, so pass 2 really did
    // reproduce pass 1's cache key. Without this the absence below could just
    // be a fresh key answering from a different entry.
    assert_eq!(
        passes[0].0, passes[1].0,
        "the two enumerations started from different descriptor numbers, so pass \
         2 did not read the page this probe stranded"
    );

    // Negative half, on names and bytes: the second enumeration did not re-read
    // the directory, because it was answered from the page the closed descriptor
    // left behind.
    assert_ne!(
        passes[1].1, expected,
        "the second enumeration still reported the tree with the eviction \
         removed, so this proof never depended on the eviction at all"
    );
    // And specifically the shape the stranded page produces: a completed
    // enumeration's remainder is empty, so pass 2 reports nothing at all. This
    // is the bytes half of the assertion -- not "different names" but "no
    // names", which no other probe in this file can produce.
    assert!(
        passes[1].1.is_empty(),
        "the stranded page should have answered end-of-directory, but the second \
         enumeration reported {} names: {:?}",
        passes[1].1.len(),
        passes[1]
            .1
            .iter()
            .take(4)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect::<Vec<_>>()
    );

    assert!(!destination.exists());
    assert_project_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
}

/// Probe C -- `fstat` on a virtual descriptor answers `EBADF`.
///
/// Its discriminator is a *pair*, and no other probe here can produce it:
/// `touch <absent>` exits nonzero **and the empty file is still in the export**.
/// The creation never depended on the reply; only the honest exit code did.
#[test]
fn mutation_probe_fstat_makes_touch_fail_while_still_creating_the_file() {
    if declared_probe().as_deref() != Some("fstat") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=fstat with a binary built with \
                   --features mutation-probe-fstat"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let (run, operand) = utility_run(scratch.path(), &host, port, 0, "/usr/bin/touch", "foo");
    assert_ne!(
        run.status,
        Some(0),
        "breaking fstat routing did not make touch fail:\n{}",
        run.stderr
    );
    let stored = read_through_client(host.as_str(), port, &utility_shadow(&run, &operand))
        .expect("the probe breaks the fstat reply, not the create");
    assert!(
        stored.is_empty(),
        "the probe changed what reached the store, so it is not isolated to the \
         fstat reply: {} bytes",
        stored.len()
    );
    assert!(!operand.exists());
    assert_host_write_root_empty(&run);
}

/// Probe D -- the `mkdir`(136) decode arm is removed.
///
/// Its negative half is that the directory exists **nowhere**: not on the host,
/// and not in the export. With the arm gone, 136 reaches the decoder's
/// "unclassified Darwin syscall" refusal, which `syscall_entry` propagates and
/// which stops the run -- so nothing is ever captured. (That is *not* the
/// pre-#114 `RefusedByEnforcement` behaviour, and this doc used to say it was:
/// before the arm existed, 136 was not breakpointed at all, so it never reached
/// the decoder. The probe creates a third state.)
///
/// **The positive half is load-bearing and was added in round 1.** With only
/// the negative assertions, this case passed against a tree whose *second*
/// admission gate never learned 136/189/339/524: `/bin/mkdir` died before
/// `main` on `intercepted raw syscall 339`, which satisfies "exits nonzero" and
/// "nothing anywhere" without saying a word about the decode arm. A probe that
/// passes for a cause outside its own mutation certifies exactly what it cannot
/// detect, which inverts the discipline the probes exist for. So this case now
/// also requires `touch <absent>` to still exit 0 with its empty file in the
/// export, in the same probe binary — which no common cause upstream of the
/// decode arm can satisfy. Probes C and E already had such a half; D did not,
/// and it was the only one that could pass vacuously.
#[test]
fn mutation_probe_mkdir_makes_the_directory_appear_nowhere() {
    if declared_probe().as_deref() != Some("mkdir") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=mkdir with umbra-platform-macos built \
                   with --features mutation-probe-mkdir"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let (run, operand) = utility_run(scratch.path(), &host, port, 0, "/bin/mkdir", "bar");
    assert_ne!(
        run.status,
        Some(0),
        "removing the mkdir decode arm did not make mkdir fail:\n{}",
        run.stderr
    );
    assert!(
        attributes_through_client(&host, port, &utility_shadow(&run, &operand)).is_none(),
        "a refused mkdir was captured in the export"
    );
    assert!(!operand.exists(), "a refused mkdir reached the host");
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);

    // The positive half. Everything above is satisfied by any failure that
    // stops `/bin/mkdir` for any reason; this is the assertion that is not.
    let (touched, file) = utility_run(scratch.path(), &host, port, 1, "/usr/bin/touch", "foo");
    assert_eq!(
        touched.status,
        Some(0),
        "the probe broke more than the mkdir decode arm, so this case proves \
         nothing about it:\n{}",
        touched.stderr
    );
    let stored = read_through_client(host.as_str(), port, &utility_shadow(&touched, &file))
        .expect("the create path is untouched by this probe");
    assert!(stored.is_empty());
}

/// Probe E -- `setattrlistat`(524) is refused at the decode.
///
/// Its discriminator is the pair no other probe reproduces: `touch <existing>`
/// fails while `touch <absent>` still succeeds and still leaves its empty file
/// in the export, because the create path never reaches this syscall at all.
/// Both halves are asserted here rather than one, so a probe that broke `touch`
/// outright could not satisfy it.
///
/// **How it fails is not how an unrouted `setattrlistat` failed**, and the
/// assertion is written for what the probe does rather than for what it might
/// be imagined to restore. Refusing at the decode stops the *run*: measured on
/// `--local-dev`, the mutated binary emits `UnsupportedCapability during macos:
/// mutation probe: setattrlistat is refused` with no tracee message and no
/// child status, where the unmutated one answers the tracee `Operation not
/// supported` and finishes. `assert_ne!(status, Some(0))` covers both, which is
/// why it is written that way and not against a child exit code.
#[test]
fn mutation_probe_setattrlistat_makes_touch_on_an_existing_file_fail() {
    if declared_probe().as_deref() != Some("setattrlistat") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=setattrlistat with umbra-platform-macos \
                   built with --features mutation-probe-setattrlistat"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let (existing, _) = utility_run(scratch.path(), &host, port, 0, "/usr/bin/touch", "seed.txt");
    assert_ne!(
        existing.status,
        Some(0),
        "refusing setattrlistat did not make touch-on-existing fail:\n{}",
        existing.stderr
    );
    assert_workspace_pristine(&existing);
    assert_host_write_root_empty(&existing);

    let (absent, operand) = utility_run(scratch.path(), &host, port, 1, "/usr/bin/touch", "foo");
    assert_eq!(
        absent.status,
        Some(0),
        "the probe also broke touch's create path, so it is not isolated to \
         setattrlistat:\n{}",
        absent.stderr
    );
    let stored = read_through_client(host.as_str(), port, &utility_shadow(&absent, &operand))
        .expect("the create path is untouched by this probe");
    assert!(stored.is_empty());
}

// ---------------------------------------------------------------------------
// Codex slate rank 1 -- a small-file edit and read-back through `std::fs`'s own
// wrappers rather than through bare syscalls.
//
// Every fixture above this line issues `open`/`read`/`write`/`close` itself, so
// every transfer it makes is one it sized and looped by hand. The library
// wrappers codex's synchronous exec-server core is built from do neither: they
// take a capacity hint from a descriptor's metadata and they complete their own
// short I/O *inside std*, invisibly to the program. Whether they are routed,
// whether those internal loops finish, and whether the sizes they read back are
// exact is the gap these cases close.
//
// The four probe cases below reuse the four shipped probes -- `read`, `write`,
// `fstat`, `mkdir` -- and add no cargo feature, which is why
// `every_mutation_probe_is_wired_into_the_userspace_job` is byte-identical to
// what it was before this slice.
//
// **KNOWN COVERAGE LIMIT, AND IT FALLS ON THE HEADLINE LEG.** None of those four
// probes reaches leg vi, and since leg vi moved into its own case `big` that is
// now structural rather than incidental: the probe cases launch `edit`, which does
// not contain leg vi at all. Even within `edit` each probe breaks the earliest leg
// that uses its mechanism and the run stops there -- `read` and `fstat` in leg i,
// `write` in leg iii, `mkdir` in leg iv. So leg vi has **no mutation backstop**. Its assertions are
// positive-only: the stored size, the bytes at both chunk boundaries, and the two
// read wrappers agreeing. If routing for a multi-chunk transfer regressed in a way
// that still satisfied all three, nothing here would fail.
//
// This is **not** the claim that nothing above covers a multi-chunk transfer --
// the `bigio` control does, at the same size, and it is mutation-covered by the
// shipped `read` and `write` probes through the toy. What no probe reaches is
// this leg's own subject: that *std's* loops, the ones the program cannot see,
// finish such a transfer. `bigio` cannot stand in for that, because its loop is
// hand-written in the fixture and asserts its own shortness.
//
// That is a deliberate consequence of reusing the shipped probes rather than an
// oversight. A probe that stopped at leg vi would have to be a new cargo feature
// on a production crate, which is what the design gate refused: it would add a
// seventh probe, a seventh CI proof step, and a row in the wiring test above --
// whose byte-identity is the evidence that refusal was honoured. Recorded here so
// the gap is read off the file rather than rediscovered.
// ---------------------------------------------------------------------------

/// Bytes the Rust I/O fixture leaves behind, which must match the constants of
/// the same shape in `experiments/fixtures/umbra-userspace-rustio.rs`; nothing
/// but these assertions checks that they agree, exactly as `PAYLOAD` above is
/// the only thing pinning the toy's own message.
///
/// The fixture's `MODIFIED` has no constant here on purpose: leg v overwrites it
/// before the run ends, so it is never observable through the client and the
/// harness would be declaring a value it cannot check.
const RUSTIO_SHORT: &[u8] = b"short\n";
const RUSTIO_LEAF: &[u8] = b"leaf under a routed directory\n";

/// Leg vi's payload size, matching `BIG` in the fixture, which carries the full
/// reasoning. In short: it is the **smallest** size that crosses the backend's
/// 1 048 572-byte per-call bound more than once -- 8 bytes past two bounds -- and
/// it is no larger because leg vi moves it three times inside a per-run session
/// watchdog that is this harness's own `timeout_ms` (`registry`, above) and that
/// the writer lease will not let anyone raise. That margin is spent by CPU
/// contention rather than by bytes: measured, 2 621 440 expires the watchdog with
/// four competing CPU workers on a ten-core host while this size does not.
const RUSTIO_BIG: u64 = 2_097_152;

/// `LibnfsRawTransport`'s per-call bound, and therefore where leg vi's chunk
/// boundaries fall.
const RUSTIO_BOUND: u64 = 1_048_572;

/// Leg vi's payload byte at `index`, which must match `big_payload` in the
/// fixture.
///
/// The payload is position-dependent for one reason: a constant fill cannot tell
/// a correct read from one that returned the right *number* of bytes from the
/// wrong offset, and the offsets this suite asks about are the chunk boundaries,
/// where an offset error is exactly what a broken loop would produce.
fn rustio_big_byte(index: u64) -> u8 {
    (index as usize).wrapping_mul(31).wrapping_add(7) as u8
}

/// Eight payload bytes starting at `offset`, as the fixture would have written
/// them.
fn rustio_big_range(offset: u64) -> Vec<u8> {
    (offset..offset + 8).map(rustio_big_byte).collect()
}

/// The shadow components of one sibling of the run's destination.
///
/// The fixture derives `<path>.d/leaf.txt` and `<path>.big` from the one path the
/// harness gave it, so the harness derives the same names the same way rather
/// than hard-coding `out.txt`.
///
/// Shared with the edge fixture's four recoverable-error cases, which derive
/// their operands and their liveness sentinels the same way. The `rustio_`
/// prefix is where it was first needed, not a restriction on who may call it.
fn rustio_sibling(run: &Run, suffix: &[u8], leaf: Option<&[u8]>) -> Vec<Vec<u8>> {
    let mut components = shadow_path(run);
    components
        .last_mut()
        .expect("the destination has at least one component")
        .extend_from_slice(suffix);
    if let Some(leaf) = leaf {
        components.push(leaf.to_vec());
    }
    components
}

/// **The slate's own case: read an existing file with its metadata, write a
/// modified version, close, reopen and compare -- all of it through `std::fs`.**
///
/// Six legs across **two routed runs**. Case `edit` carries five of them and the
/// order inside it is load-bearing rather than incidental: legs i, ii, iii and v
/// act on `<path>` and leg iv on `<path>.d/leaf.txt`, so each of the four probe
/// cases below can point at an object an *earlier* leg already put in the export
/// as positive evidence that the run was alive when its own leg broke. Case `big`
/// carries leg vi by itself, because the watchdog is per session rather than per
/// call and leg vi is the leg that spends it; `RUSTIO_BIG` has the measurement
/// and states what the separation does and does not buy.
///
/// What this asserts that no case above it does:
///
/// * **A library wrapper's own completion loop finishes a routed transfer.**
///   `bigio` proves the backend answers a over-bound transfer short and that a
///   loop *the fixture wrote* finishes it. `write_all` and `default_read_to_end`
///   loop inside std where the program cannot see them, and leg vi's payload
///   crosses the bound twice so a loop that completes one remainder and stops is
///   caught.
/// * **A size taken from a descriptor is exact.** Nothing above compares
///   `fstat`'s `st_size` against what was written. Codex's read and write paths
///   both branch on `file.metadata()?`, so a routed `fstat` that answered a
///   plausible wrong size would satisfy every shipped case.
/// * **Truncation to a shorter non-empty length through a library wrapper.**
///   `trunc` pins 10 bytes to 2 by hand; leg v reaches the same arm through
///   `File::create` and requires both the exact new length and the absence of
///   the old tail.
/// * **`std::fs::read` and `File::read_to_end` agree.** They take different
///   routed paths -- only the second asks `stream_position()`, which is
///   `lseek`(199), which a virtual descriptor refuses with EBADF and std then
///   degrades to reading by probing. Requiring them to agree is the only thing
///   here that would catch a future `lseek` answering a *wrong* offset rather
///   than refusing.
#[test]
fn a_rust_library_write_reopen_and_compare_crosses_the_std_fs_wrappers() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    // **Two routed runs, not one, and the separation is the point.** The registry
    // `timeout_ms` is a whole-session watchdog armed before the twin is resigned,
    // so legs sharing a run share one budget. Leg vi is the expensive leg; giving
    // it case `big` and a run of its own means the margin measured for it
    // describes the session that actually executes. It does not make it
    // saturation-proof -- see `RUSTIO_BIG`.
    //
    // **Separate scratch directories, deliberately.** `launch` derives
    // `workspace`, `state` and `registry.json` from `scratch`, and
    // `rust_fixture_binary` compiles the fixture to `scratch/<name>`. One shared
    // scratch would work -- shadows are keyed by run id and every per-run
    // assertion below is per-run -- but it would also rewrite the very executable
    // the first run just launched, which is the `ETXTBSY` class this repository
    // already had to remove from another test. Two directories cost one extra
    // `rustc` invocation locally, nothing at all in CI where `UMBRA_RUSTIO_PATH`
    // is set, and leave no question to answer.
    let scratch = tempfile::tempdir().unwrap();
    let big_scratch = tempfile::tempdir().unwrap();
    // Nothing is mounted, checked before and after for the reason every case
    // here checks it: a helpfully-mounted export would satisfy everything below
    // through the kernel client and nothing would say so. Both scratch roots are
    // covered, because both hold a run.
    let paths: Vec<&Path> = vec![scratch.path(), big_scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the runs");
    let run = routed_rust_io(scratch.path(), &host, port, "edit");
    let big_run = routed_rust_io(big_scratch.path(), &host, port, "big");
    assert_no_nfs_mount(&host, port, &paths, "after the runs");

    // 1. Each fixture's own verdict. Exit zero is every leg of that case
    //    agreeing, and each leg has its own exit code, so a nonzero status names
    //    the step that failed rather than the run.
    assert_eq!(
        run.child_exit(),
        0,
        "a std::fs edit/read-back round trip over the userspace client failed:\n{}",
        run.stderr
    );
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert_eq!(
        big_run.child_exit(),
        0,
        "the multi-chunk write and its two read-backs failed:\n{}",
        big_run.stderr
    );
    assert_eq!(big_run.status, Some(0), "{}", big_run.stderr);

    // 2. Legs ii, iii and v, out of band: the destination in the export holds
    //    exactly the shorter replacement. Its *size* is asserted beside its
    //    bytes because they are different claims -- the bytes could be right
    //    with a stale tail behind them, and `trunc` above cannot see that
    //    because it reads the whole object back in one 4 KiB `READ`.
    let destination = shadow_path(&run);
    assert_eq!(
        read_through_client(&host, port, &destination).as_deref(),
        Some(RUSTIO_SHORT),
        "the export does not hold the shorter replacement leg v wrote"
    );
    assert_eq!(
        stored_size_through_client(&host, port, &destination),
        Some(RUSTIO_SHORT.len() as u64),
        "the shorter replacement left the object's old length behind it"
    );

    // 3. Leg iv: a file named absolutely inside a directory the same `edit` run
    //    created, which is codex's `create_directory` then `write_file`
    //    sequence and which no case above performs.
    let leaf = rustio_sibling(&run, b".d", Some(b"leaf.txt"));
    assert_eq!(
        read_through_client(&host, port, &leaf).as_deref(),
        Some(RUSTIO_LEAF),
        "the export does not hold the file leg iv wrote under the directory the \
         same run made"
    );

    // 4. Leg vi, out of the **second** run. The size proves std's `write_all`
    //    finished a transfer that crossed the per-call bound twice; the ranged
    //    reads prove it finished it
    //    *in order*. Asked at both boundaries and at both ends, because a loop
    //    that completed one remainder and stopped, or one that repeated a chunk,
    //    would leave the right number of bytes with the wrong ones at a seam --
    //    which is invisible to a size and invisible to a constant fill.
    //
    //    **This payload is the same 2 MiB the `bigio` control uses, and the
    //    overlap is in the number only.** `bigio`'s loop is written in the
    //    fixture and asserts its own shortness, and its read-back is a size;
    //    here the loops are std's own and invisible to the program, the bytes are
    //    position-dependent so a seam can be probed, the boundaries are read back
    //    through the client from both sides, and the same object is read twice
    //    through two wrappers that take different routed paths. The size is
    //    where it is because a watchdog put it there (`RUSTIO_BIG`), not because
    //    2 MiB is the interesting quantity.
    let big = rustio_sibling(&big_run, b".big", None);
    assert_eq!(
        stored_size_through_client(&host, port, &big),
        Some(RUSTIO_BIG),
        "the export does not hold all {RUSTIO_BIG} bytes std's write_all was given \
         in one call"
    );
    // Both sides of both chunk boundaries, plus the start. At this payload
    // `2 * RUSTIO_BOUND` is also `RUSTIO_BIG - 8`, so the last probe covers the
    // final 8-byte chunk and the second boundary at once -- which is why the
    // list names it once rather than twice.
    for offset in [
        0,
        RUSTIO_BOUND - 8,
        RUSTIO_BOUND,
        2 * RUSTIO_BOUND - 8,
        2 * RUSTIO_BOUND,
    ] {
        assert_eq!(
            read_range_through_client(&host, port, &big, offset, 8),
            Some(rustio_big_range(offset)),
            "the eight bytes at offset {offset} are not the ones the fixture \
             wrote there, so the transfer landed out of order or short"
        );
    }

    // 5. None of it reached the host, for **either** run. Between them the two
    //    fixtures named four paths and created none of them here: each workspace
    //    still holds exactly its seed.
    for (label, each) in [("edit", &run), ("big", &big_run)] {
        assert!(
            !each.destination.exists(),
            "{label}: the host destination {} was created",
            each.destination.display()
        );
        assert_workspace_pristine(each);
        assert_host_write_root_empty(each);

        // 6. And each run is journalled complete rather than merely quiet.
        assert!(
            journal_records_completion(each),
            "{label}: no RunCompleted record in the run's journal"
        );
    }
}

/// Probe A against the Rust fixture -- a routed `read` hands back bytes that are
/// not what the store holds.
///
/// **The exit code is 93, not leg vi's 106, and the reason is the probe's own
/// shape rather than a choice.** The mutation XORs the first byte of *every*
/// routed read reply (`engine.rs:1896-1900`), so the first leg that reads is the
/// leg that breaks -- and that is leg i, which reads the seeded source and
/// compares it. Measured: `finished: Some(Code(93))`. No ordering of the legs
/// avoids this, because a mutation on a whole direction breaks the earliest user
/// of that direction by construction.
///
/// **The discriminating half, stated for exactly what it supports and no more.**
/// Because the run stops in leg i, there is no later object in the export to
/// point at, so this case cannot use the surviving-object discriminator the
/// `mkdir` case below uses. What it has instead is the *code*, and what the code
/// rules out is narrower than "the read direction":
///
/// * 93 is reached only after `File::open` answered -- otherwise 91 -- and only
///   after the descriptor's own `metadata()` answered for a regular file --
///   otherwise 92. The `fstat` case below measures 92 on this same fixture, so
///   that second exclusion is observed rather than argued.
/// * 93 covers **two** arms of leg i and does not separate them: `read_to_end`
///   returning `Err`, and bytes that are not the seed. On the first arm the
///   length comparison is never reached, so 93 does **not** imply that the
///   descriptor's reported length agreed with the bytes -- only the second arm
///   establishes that, because a disagreement there returns 92.
/// * So what this case establishes is narrower than "the read direction": the
///   routed open and the routed fd metadata both answered, and the routed read
///   still did not deliver the seed. It does not, on its own, prove the bytes
///   were corrupted rather than the call refused.
///
/// That the shipped mutation corrupts bytes while leaving the count and the
/// mechanism alone is a property of the probe's own code
/// (`engine.rs:1896-1900`), not something this assertion establishes.
#[test]
fn mutation_probe_read_makes_the_rust_wrapper_read_back_reject() {
    if declared_probe().as_deref() != Some("read") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=read with a binary built with \
                   --features mutation-probe-read"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_rust_io(scratch.path(), &host, port, "edit");

    assert_eq!(
        run.child_exit(),
        93,
        "breaking read routing did not make the library read-back reject the \
         bytes:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );
    // The failure belongs to the call, not to the run.
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
    // Leg i rejected before leg ii ever wrote, so the destination is not in the
    // export at all. Asserted rather than assumed: if it *were* there, this
    // probe would have let a write through and the exit code would be describing
    // something other than the read direction.
    assert!(
        read_through_client(&host, port, &shadow_path(&run)).is_none(),
        "the run wrote the destination before rejecting its own source read, so \
         exit 93 is not describing leg i"
    );
    assert!(!run.destination.exists());
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
}

/// Probe B against the Rust fixture -- a routed `write` reports success and
/// stores nothing, and the library's own reopen catches it by *size*.
///
/// **The exit code is 97, not leg vi's 105.** The mutation drops the storage
/// writes while keeping the success `resolve` planned (`engine.rs:3772-3773`), so
/// the tracee is told its write landed and the first leg that reads back is the
/// leg that notices. That is leg iii, whose size check runs before its byte
/// check precisely so this has one code rather than two. Measured:
/// `finished: Some(Code(97))`.
///
/// **This is the case that pins `fstat`'s size against what was written**, which
/// nothing in this file did before: `EmptyFile` proves `touch` can report
/// honestly, and no other case compares a descriptor's `st_size` to a byte count
/// it chose. Codex's read and write paths both branch on `file.metadata()?`
/// (`no_follow/unix.rs:107`, `:135`), so a routed `fstat` answering a plausible
/// wrong size is exactly the defect this catches.
///
/// **The discriminating half** is the pair the shipped `write` probe uses and no
/// read fault can reproduce: the object in the export is absent or empty. And
/// unlike the read case above, this one also carries a genuine positive -- exit
/// 97 rather than 93 means leg i read the seed back correctly through the same
/// routing in the same run, so the read direction is intact and only the write
/// direction is not.
#[test]
fn mutation_probe_write_makes_the_rust_reopen_disagree_on_size() {
    if declared_probe().as_deref() != Some("write") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=write with a binary built with \
                   --features mutation-probe-write"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_rust_io(scratch.path(), &host, port, "edit");

    assert_eq!(
        run.child_exit(),
        97,
        "breaking write routing did not make the library's reopen disagree about \
         the size:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );
    // The discriminator. A wrong size alone could be explained by a broken
    // `fstat`; an object in the export that is absent or empty could not, and the
    // case above measures 92 for a broken `fstat` rather than 97.
    match read_through_client(&host, port, &shadow_path(&run)) {
        None => {}
        Some(stored) => assert!(
            stored.is_empty(),
            "the write probe left {} bytes in the export, so the run's own \
             size complaint is not about a write that vanished",
            stored.len()
        ),
    }
    assert!(!run.destination.exists());
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
}

/// Probe C against the Rust fixture -- `fstat` on a virtual descriptor answers
/// `EBADF`, and the leg that notices is the one codex actually wrote.
///
/// **This is the highest-value probe here and it took reading std to place.**
/// `std::fs::read` asks for its capacity hint with `.ok()`
/// (`library/std/src/fs.rs:343`), so a routed `fstat` answering EBADF costs it a
/// hint and changes nothing observable -- the read still completes by probing. A
/// fixture built only out of `fs::read` would pass this probe while proving
/// nothing about `fstat` at all, which is the #134 failure shape exactly. The leg
/// that does break is the one that propagates, `file.metadata()?.is_file()`
/// (`no_follow/unix.rs:107`), which is why `descriptor_length` in the fixture
/// returns `None` on `Err` instead of defaulting. Measured:
/// `finished: Some(Code(92))`.
///
/// **The discriminating half**: 92 rather than 91 means the routed `open` of the
/// base object answered in this same run, so the mutation is isolated to the
/// descriptor's metadata reply -- the same "this breaks the reply and nothing
/// else" claim the shipped `touch` case makes, asserted here against a library
/// wrapper instead of an Apple binary.
#[test]
fn mutation_probe_fstat_makes_the_rust_metadata_check_fail() {
    if declared_probe().as_deref() != Some("fstat") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=fstat with a binary built with \
                   --features mutation-probe-fstat"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_rust_io(scratch.path(), &host, port, "edit");

    assert_eq!(
        run.child_exit(),
        92,
        "breaking fstat routing did not make the propagating metadata check \
         fail:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );
    assert!(
        run.stderr.contains("finished:"),
        "the run did not finish:\n{}",
        run.stderr
    );
    // Nothing was written: the metadata check is the first thing leg i does after
    // the open, so a destination in the export would mean the run got further
    // than exit 92 claims.
    assert!(
        read_through_client(&host, port, &shadow_path(&run)).is_none(),
        "the run reached its write legs, so exit 92 is not describing leg i's \
         metadata check"
    );
    assert!(!run.destination.exists());
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
}

/// Probe D against the Rust fixture -- the `mkdir`(136) decode arm is removed and
/// `std::fs::create_dir` reaches nothing.
///
/// **There is no child exit code to assert, and that is measured rather than
/// assumed.** With the arm gone, 136 reaches the decoder's "unclassified Darwin
/// syscall" refusal, which `syscall_entry` propagates and which stops the *run*:
/// measured, umbra emits `UnsupportedCapability during macos: unclassified
/// Darwin syscall 136` with no status line at all, so `child_exit()` would find
/// nothing to parse. This case therefore asserts `status != Some(0)`, exactly as
/// the shipped `/bin/mkdir` case does and for the same reason.
///
/// **This is the one case here whose discriminator is a surviving object, and it
/// is the strongest of the four.** Legs i, ii, iii and v all complete before leg
/// iv is reached -- that is what the fixture's execution order is *for* -- so the
/// destination is in the export holding the shorter replacement, byte for byte,
/// while the directory and the file under it exist nowhere. No common cause
/// upstream of the decode arm satisfies that pair: a broken open, a broken read,
/// a broken write or a broken `fstat` each stop the fixture in leg i or leg iii
/// with a child exit code, which is a state this case would reject.
#[test]
fn mutation_probe_mkdir_makes_the_rust_new_file_appear_nowhere() {
    if declared_probe().as_deref() != Some("mkdir") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=mkdir with umbra-platform-macos built \
                   with --features mutation-probe-mkdir"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let run = routed_rust_io(scratch.path(), &host, port, "edit");

    assert_ne!(
        run.status,
        Some(0),
        "removing the mkdir decode arm did not stop the run:\n{}",
        run.stderr
    );
    // The negative half: neither the directory nor the file the fixture would
    // have put inside it is anywhere.
    let leaf = rustio_sibling(&run, b".d", Some(b"leaf.txt"));
    assert!(
        attributes_through_client(&host, port, &leaf).is_none(),
        "a refused mkdir still captured the file underneath it"
    );
    assert!(
        attributes_through_client(&host, port, &rustio_sibling(&run, b".d", None)).is_none(),
        "a refused mkdir was captured in the export"
    );
    assert!(
        !run.destination.exists(),
        "the run reached the host destination"
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);

    // The positive half, and everything above is satisfiable by any failure that
    // stops the run early -- this is the assertion that is not. The destination
    // carries leg v's shorter replacement, so the routed open, write, close,
    // reopen, `fstat` and truncate all worked in this same mutated binary and
    // only the directory leg broke.
    assert_eq!(
        read_through_client(&host, port, &shadow_path(&run)).as_deref(),
        Some(RUSTIO_SHORT),
        "the destination the legs before mkdir wrote is not in the export with \
         its bytes, so this case proves nothing about the mkdir decode arm"
    );
}

/// The buffered-output positive control: bare `write(2)` on a routed
/// descriptor, from the executable's own call site.
///
/// **This case is what makes the two #127 characterizations below worth
/// anything.** They assert `Some(vec![])` -- an object that exists in the
/// export and holds no bytes -- and a `read_through_client` that always
/// returned an empty vector would satisfy them while measuring nothing. This
/// case puts the same call, against the same export, in the same slice, and
/// requires the full payload and its exact size back. That pairing is the
/// non-vacuity argument for cases 2 and 3; no shipped mutation probe supplies
/// one, because #127 already produces the `write` probe's effect on those two
/// shapes.
///
/// It is also the half of the comparison that localises the defect. The same
/// bytes, the same destination, the same run shape -- the only difference is
/// whether the final `write` instruction lives in the executable or inside the
/// dyld shared cache. Measured: `finished: Some(Code(0))` and 20 bytes.
#[test]
fn a_raw_write_to_a_routed_descriptor_persists_every_byte() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    // Nothing is mounted, before and after, for the reason every case here
    // checks it: a helpfully-mounted export would satisfy the read-back through
    // the kernel client and nothing below would say so.
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_stdio(scratch.path(), &host, port, "raw");
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_eq!(
        run.child_exit(),
        0,
        "the raw write did not report success:\n{}",
        run.stderr
    );
    assert_eq!(run.status, Some(0), "umbra run failed:\n{}", run.stderr);

    let components = shadow_path(&run);
    let stored = read_through_client(&host, port, &components)
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored, PAYLOAD,
        "the export does not hold the bytes the raw case wrote"
    );
    // The size attribute as well as the bytes: `read_through_client` caps its
    // READ, so a longer object that happened to start with the payload would
    // satisfy the comparison above on its own.
    assert_eq!(
        stored_size_through_client(&host, port, &components),
        Some(PAYLOAD.len() as u64),
        "the export's size attribute disagrees with the bytes the raw case wrote"
    );

    assert!(
        !run.destination.exists(),
        "the host destination {} was created",
        run.destination.display()
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
    assert!(
        journal_records_completion(&run),
        "no RunCompleted record in the run's journal"
    );
}

/// **Characterization of [#127](https://github.com/invakid404/umbra/issues/127),
/// not a passing behaviour.** Buffered stdio output to a routed descriptor
/// persists nothing, and this is the shape in which the program is at least
/// told.
///
/// What is asserted is today's wrong answer, stated positively: exit 126, and
/// an object in the export that **exists** and holds **zero** bytes. The fix to
/// #127 will make this case red, and that is intended -- when it goes red, the
/// assertions to change are the exit code (to 0) and the byte comparison (to
/// `PAYLOAD`, as in the raw case above). Do not reach for `#[ignore]` instead:
/// no step of this job passes `--include-ignored`, so an ignored case reports
/// green without running.
///
/// **Why 126 and not some other arm.** The fixture gives `fflush` and `fclose`
/// separate exit codes precisely so the status says which one reported first.
/// Measured: `fflush` returns -1 with `errno` EBADF and `ferror` already set,
/// so `fclose`'s own code (129) is never reached. The chain underneath is
/// `__sflush` -> `_swrite` -> `__swrite` -> `__write_nocancel`(397), which is in
/// neither `abi::TRACED_STUBS` nor anything `DYLD_INTERPOSE` can rebind. Public
/// `write`(4) is never entered on this path, whatever #127's own text says, so
/// routing `write` alone would leave this case unchanged.
///
/// **Why the object is not simply missing.** `fopen`'s `__open_nocancel`(398)
/// and `fclose`'s `__close_nocancel`(399) *are* breakpointed, so creation and
/// release route normally. Only the bytes are lost. `read_through_client`
/// distinguishes the two outcomes -- `None` for an absent name, `Some(vec![])`
/// for an empty object -- and this case needs the second, because an absent
/// object would be a different and much louder defect.
#[test]
fn a_checked_buffered_write_to_a_routed_descriptor_fails_observably_127() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_stdio(scratch.path(), &host, port, "checked");
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_eq!(
        run.child_exit(),
        126,
        "the checked buffered write did not fail at the flush arm; #127 may be \
         fixed, in which case this characterization is the thing to update:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported a failed run as success"
    );

    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists even though no byte of the payload reached it");
    assert!(
        stored.is_empty(),
        "the export holds {} bytes, so buffered stdio output now reaches a \
         routed descriptor and this characterization of #127 is stale",
        stored.len()
    );

    assert!(
        !run.destination.exists(),
        "the host destination {} was created",
        run.destination.display()
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
    // The run itself completed normally. The tracee failed; the supervisor did
    // not, and a case that could not tell those apart would not be measuring
    // #127.
    assert!(
        journal_records_completion(&run),
        "no RunCompleted record in the run's journal"
    );
}

/// **Characterization of [#127](https://github.com/invakid404/umbra/issues/127)
/// in its silent shape -- the one this slice exists for.** The same buffered
/// write as above, with `fclose`'s result discarded, which is what a great many
/// programs do. The process exits **0**, umbra reports success, and the export
/// holds an object that exists with **zero bytes** in it. Nothing anywhere
/// reports a problem.
///
/// **The two assertions have to be made together or the case is worthless.**
/// Exit 0 alone is what a correct run looks like. An empty object alone could be
/// a run that died. Exit 0 *and* an empty object is the wrong answer that looks
/// like a right one, and it is the only combination that distinguishes this
/// defect from both of its neighbours.
///
/// **Why the payload has to stay small, which is a constraint on `PAYLOAD` and
/// not on this case.** stdio sizes its buffer from the descriptor's
/// `st_blksize`, which this backend reports as 4096. Measured sweep: at 4095
/// bytes `fprintf` returns the full count with `ferror` clear, and the export
/// holds nothing -- silent. At 4096 the buffer spills during the `fprintf`
/// itself, which returns -1, and the fixture exits 125 instead: the defect stops
/// being silent and this case stops testing it.
///
/// The fix to #127 turns this case red, on purpose. When it does, the assertion
/// to change is the byte comparison -- to `PAYLOAD` -- while the exit code stays
/// 0. As rank 4 of the slate puts it: do not label a current defect acceptable
/// permanent behaviour just to get a green test. This case is green because it
/// asserts the defect, and it is the reason the defect cannot now change shape
/// unnoticed.
#[test]
fn a_buffered_write_whose_fclose_is_ignored_exits_zero_with_an_empty_object_127() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_stdio(scratch.path(), &host, port, "ignored");
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    // Half one: everything the program and the supervisor can see says success.
    assert_eq!(
        run.child_exit(),
        0,
        "the ignored-fclose case did not exit zero, so it is no longer the \
         silent shape of #127:\n{}",
        run.stderr
    );
    assert_eq!(
        run.status,
        Some(0),
        "umbra run failed, which is a different defect from the silent one this \
         case characterizes:\n{}",
        run.stderr
    );

    // Half two: and the file is empty. The export is the only place this is
    // visible, which is exactly why "inspect it independently through the NFS
    // client" is a requirement of the experiment and not decoration.
    let stored = read_through_client(&host, port, &shadow_path(&run))
        .expect("the shadow object exists in the export, empty, rather than being absent");
    assert!(
        stored.is_empty(),
        "the export holds {} bytes, so buffered stdio output now persists and \
         this characterization of #127 is stale",
        stored.len()
    );

    assert!(
        !run.destination.exists(),
        "the host destination {} was created",
        run.destination.display()
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
    assert!(
        journal_records_completion(&run),
        "no RunCompleted record in the run's journal"
    );
}

/// The control that **passes**: a Rust `BufWriter` with an explicit, checked
/// flush persists every byte over the same routed destination the stdio cases
/// lose theirs on.
///
/// **This is the case that says what #127 is actually about.** Buffering is not
/// the fault line. `std`'s final `write` is compiled into the executable, which
/// puts it at exactly the call sites `DYLD_INTERPOSE` rebinds, so a buffered
/// Rust write routes and lands. C stdio's is inside the dyld shared cache, where
/// interposition cannot reach and no `TRACED_STUBS` row covers it. Without this
/// case, #127 reads as "buffered writes are broken" and the next person to work
/// on it looks in the wrong place.
///
/// So this is a control and not a second defect case: it exits 0 with all 20
/// bytes in the export on master, and if it ever goes red that is a regression
/// in routing rather than a known-unfixed shape.
///
/// Measured: `finished: Some(Code(0))`, 20 bytes. The fixture's own header
/// records the second case that was measured and then deliberately left out --
/// `BufWriter` + `drop` lands the same bytes here and the same zero bytes under
/// the `write` probe, so it could never fail differently from this one.
#[test]
fn a_rust_bufwriter_with_an_explicit_flush_persists_every_byte() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<&Path> = vec![scratch.path()];
    assert_no_nfs_mount(&host, port, &paths, "before the run");
    let run = routed_buffered(scratch.path(), &host, port);
    assert_no_nfs_mount(&host, port, &paths, "after the run");

    assert_eq!(
        run.child_exit(),
        0,
        "the BufWriter control failed; 113 is its explicit flush, which is the \
         arm C stdio fails on:\n{}",
        run.stderr
    );
    assert_eq!(run.status, Some(0), "umbra run failed:\n{}", run.stderr);

    let components = shadow_path(&run);
    let stored = read_through_client(&host, port, &components)
        .expect("the shadow object exists in the export");
    assert_eq!(
        stored, PAYLOAD,
        "the export does not hold the bytes the BufWriter flushed"
    );
    assert_eq!(
        stored_size_through_client(&host, port, &components),
        Some(PAYLOAD.len() as u64),
        "the export's size attribute disagrees with the bytes the BufWriter flushed"
    );

    assert!(
        !run.destination.exists(),
        "the host destination {} was created",
        run.destination.display()
    );
    assert_workspace_pristine(&run);
    assert_host_write_root_empty(&run);
    assert!(
        journal_records_completion(&run),
        "no RunCompleted record in the run's journal"
    );
}

/// Probe B against both writers that are supposed to work -- and the reason the
/// two cases that *do* work are not asserting their exit codes alone.
///
/// **The discriminator is the byte count, and nothing else.** Under
/// `mutation-probe-write` the overlay drops the bytes and still reports the
/// success the transaction planned, so the tracee is told its write landed.
/// Measured, both writers, both worlds: exit **0** with no probe and exit **0**
/// under the probe, while the export goes from the full payload to empty. A
/// case resting on the exit code would therefore pass in both worlds and mean
/// nothing; the two cases above rest on the bytes, and this is the measurement
/// that proves they had to.
///
/// **Why this probe and no new one.** `write` is shipped, and a new probe is not
/// a local edit: it needs a cargo feature on a production crate, a `#[cfg]` in
/// production source, a ninth proof step in `ci.yml` and a row in
/// `every_mutation_probe_is_wired_into_the_userspace_job`. Reusing `write` costs
/// none of that.
///
/// **And it is honestly vacuous on the two stdio cases, which is why they are
/// not here.** #127 already produces this probe's exact effect on them -- exit
/// code unchanged, zero bytes -- so a probe case over `checked` or `ignored`
/// would assert the same state in both worlds. Their non-vacuity comes from
/// pairing with the raw control instead, which is what
/// `a_raw_write_to_a_routed_descriptor_persists_every_byte` is for.
#[test]
fn mutation_probe_write_empties_both_routed_writers_while_leaving_their_exit_codes_zero() {
    if declared_probe().as_deref() != Some("write") {
        eprintln!(
            "SKIP: set UMBRA_MUTATION_PROBE=write with a binary built with \
                   --features mutation-probe-write"
        );
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    // **Two runs, and two scratch directories.** `launch` derives `workspace`,
    // `state` and `registry.json` from `scratch`, and the fixture helpers
    // compile into `scratch/<name>`. One shared scratch would also work here --
    // the two fixtures have different basenames and every assertion below is
    // per-run -- but it would rewrite files beside an executable a previous run
    // just launched, which is the `ETXTBSY` class this repository already had to
    // remove from another test. Two directories cost one extra compile locally
    // and nothing at all in CI, where both `UMBRA_*_PATH` variables are set.
    let raw_scratch = tempfile::tempdir().unwrap();
    let buffered_scratch = tempfile::tempdir().unwrap();
    let raw = routed_stdio(raw_scratch.path(), &host, port, "raw");
    let buffered = routed_buffered(buffered_scratch.path(), &host, port);

    for (run, writer) in [
        (&raw, "the raw write(2)"),
        (&buffered, "the Rust BufWriter"),
    ] {
        // The half that must *not* move. This is the whole claim: breaking write
        // routing is invisible in the status.
        assert_eq!(
            run.child_exit(),
            0,
            "{writer} noticed the dropped bytes in its own exit code, so this \
             probe is no longer the silent one this case rests on:\n{}",
            run.stderr
        );
        assert_eq!(
            run.status,
            Some(0),
            "umbra reported {writer}'s run as failed under the write probe:\n{}",
            run.stderr
        );

        // The half that must: the object **exists** and holds **nothing**.
        //
        // Both halves are required, and requiring the first is the point.
        // Measured: this probe drops the bytes inside the write path and does
        // not touch the `open(O_CREAT)` that created the object, so under the
        // probe both writers leave an object that is *present* and empty --
        // never absent. An assertion that also accepted absence would admit a
        // state no measurement has produced, and would stop discriminating a
        // probe that silently stopped creating objects at all, or a read-back
        // that never works.
        //
        // **This diverges from the two shipped `write`-probe cases on purpose**,
        // and the divergence is not an oversight.
        // `mutation_probe_write_makes_the_read_back_come_up_short` and
        // `mutation_probe_write_makes_the_rust_reopen_disagree_on_size` both
        // still tolerate absence: they predate the measurement above, and
        // tightening them would edit shipped controls that other proofs rest
        // on, which this slice does not do. So the looser form stays where it
        // is and the tighter form is used here, where the measurement is in
        // hand.
        let Some(stored) = read_through_client(&host, port, &shadow_path(run)) else {
            panic!(
                "the export holds no object at all for {writer}; the write probe \
                 drops bytes but leaves the object the O_CREAT open made, so an \
                 absent name means something other than this probe emptied it"
            )
        };
        assert!(
            stored.is_empty(),
            "the write probe left {} bytes in the export for {writer}, so \
             routing its write is not what put them there",
            stored.len()
        );

        assert!(
            !run.destination.exists(),
            "the host destination {} was created",
            run.destination.display()
        );
        assert_workspace_pristine(run);
        assert_host_write_root_empty(run);
    }
}

// ---------------------------------------------------------------------------
// The routed `O_APPEND` refusal -- issue #161, characterized in both languages.
//
// https://github.com/invakid404/umbra/issues/161 -- "overlay: O_APPEND is
// deliberately refused with `Err(UnsupportedCapability)`, ending the run".
//
// Six cases over the `umbra-userspace-append.c` / `umbra-userspace-appendstd.rs`
// pair, measured against live Ganesha before any assertion below was written.
// Four reach the append check and are stopped by it; two do not reach it at all
// and are the **ordering controls** that say so.
//
// The ordering is the part that is easy to get wrong, and it is why there are
// six cases rather than four. `Overlay::resolve` answers an absent target
// first: a target that does not resolve, opened without `O_CREAT`, is `ENOENT`
// from name resolution, decided before the `O_APPEND` test is reached. So a
// *bare* append to an absent path is not an append measurement at all -- it is
// `notfound` with extra flags, and the tracee gets an ordinary errno while the
// run survives. Adding `O_CREAT` is what carries the call past that first test
// and into the refusal.
//
// A change that moved the append test above the absent test would silently
// convert the two controls from a tracee `ENOENT` into a run stop, and nothing
// else in this file would notice. That is what they are here to catch.
//
// Measured, and identical in both languages: the four that reach the check are
// stopped with `UnsupportedCapability during overlay: routed open with
// O_APPEND: ... (errno: None)` and the tracee is never resumed -- there is no
// `finished:` line at all, so it receives no answer, not even a failure. The
// C half and the Rust half produce byte-identical refusal text, which is the
// pair's own finding: `OpenOptions::append(true)` is not an emulation, it sets
// the same flag bit and arrives at the same decision point.
// ---------------------------------------------------------------------------

/// The four shared assertions for a shape whose routed `O_APPEND` stopped the
/// run, with the sentinel read that makes the absence assertions mean
/// something.
///
/// A helper rather than four transcriptions, because these four claims have to
/// stay identical across the pair for the C-against-Rust comparison to be
/// exact; four copies would be four places for one of them to drift.
///
/// **Which assertion carries which claim**, following the discipline
/// `a_forked_child_s_writes_are_scoped_to_the_parent_s_run_and_its_terminal_evidence`
/// states for the same refusal:
///
/// * `status != Some(0)` is the weakest and proves the least -- a merely
///   nonzero tracee also produces it. It is here for the diagnostic, not the
///   claim.
/// * The **absent `finished:` line** is the tracee-visible half: umbra prints
///   that line with the child's code whenever the child was resumed and exited,
///   and the controls below do produce one. Its absence is how "the tracee got
///   no answer at all" is observed rather than inferred.
/// * The **journal** is what pins that *the refusal fired*: a run that ended
///   with a nonzero child still reaches `finish_run` and still records
///   `RunCompleted`; one stopped by a refused operation does not.
/// * The **sentinel** read is what makes every *absence* assertion at the call
///   sites non-vacuous. It is written before the refusal, so reading it back
///   through the client proves the run's shadow directory exists and was being
///   routed into right up to the refused open -- without it, a `None` for the
///   append target could just as well mean the shadow was never created and the
///   case would be asserting nothing.
fn assert_append_refusal_stopped_the_run(run: &Run, host: &str, port: u16, case: &str) {
    assert_ne!(
        run.status,
        Some(0),
        "the refused O_APPEND open did not fail the run:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("routed open with O_APPEND"),
        "the run did not stop on the O_APPEND refusal:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("finished:"),
        "the tracee was resumed and reported a status, so the refusal was \
         answered to it rather than stopping the run:\n{}",
        run.stderr
    );
    assert!(
        !journal_records_completion(run),
        "a run stopped by the O_APPEND refusal still recorded RunCompleted"
    );

    let mut live = shadow_path(run);
    live.last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(format!(".{case}live").as_bytes());
    assert_eq!(
        read_through_client(host, port, &live)
            .expect("the pre-refusal sentinel is in the run's shadow")
            .as_slice(),
        case.as_bytes(),
        "the run was not routing into its shadow before the refused open, so \
         nothing below about what the shadow does *not* hold means anything"
    );

    assert_host_write_root_empty(run);
    assert_workspace_pristine(run);
}

/// One shadow object of this run, named by a suffix on the destination's leaf.
fn shadow_leaf(run: &Run, suffix: &[u8]) -> Vec<Vec<u8>> {
    let mut components = shadow_path(run);
    components
        .last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(suffix);
    components
}

/// The run's shadow sibling of the harness's pre-seeded `seed.txt`.
fn shadow_seed(run: &Run) -> Vec<Vec<u8>> {
    let mut components = shadow_path(run);
    components.pop();
    components.push(b"seed.txt".to_vec());
    components
}

/// Assert the base `seed.txt` survived a refused append **byte for byte**.
///
/// This is the half of the slate's invariant that nothing in this file covered
/// before. `assert_workspace_pristine` compares directory entry *names* only,
/// so a truncation of `seed.txt` -- which is exactly what a mishandled append
/// open would produce, since `O_APPEND`'s neighbour `O_TRUNC` is honoured on
/// this path -- passes it in silence. Reading the bytes is what catches that.
///
/// The shadow read beside it is the other half: the base file must not have
/// been copied up either. The refusal returns from `resolve`, which runs before
/// `prepare` mints an `OperationId` and before any copy-up, so nothing should
/// exist at that name -- and the sentinel assertion above is what makes this
/// `None` evidence rather than an accident of a missing shadow.
fn assert_seed_untouched(run: &Run, host: &str, port: u16) {
    assert_eq!(
        std::fs::read(run.workspace.join("seed.txt")).expect("the seeded base file"),
        b"seed\n",
        "the refused append changed the base file's bytes"
    );
    assert_eq!(
        read_through_client(host, port, &shadow_seed(run)),
        None,
        "the refused append copied the base file up into the run's shadow"
    );
}

/// A routed **bare** `O_APPEND` on a file that exists and is **not empty**
/// stops the run -- #161.
///
/// MEASURED, not predicted. `open(seed.txt, O_WRONLY | O_APPEND)` -- flag word
/// `0x9` on Darwin -- resolves, reaches the append check and is refused with
/// `UnsupportedCapability during overlay: routed open with O_APPEND: no atomic
/// append-at-end storage operation exists, and stat-then-write is wrong for a
/// second writer (errno: None)`. umbra exits non-zero, the journal carries no
/// `RunCompleted`, and there is **no `finished:` line**: the tracee is never
/// resumed, so it receives no answer at all -- not an errno, not a short write.
/// A program cannot branch on this the way it branches on `ENOENT`.
///
/// This is the shape that matters most of the six, and `seed.txt` is why: it is
/// the only target here that **has content to append to**, so it is the only
/// one where "append" has an observable meaning beyond "create". That makes it
/// the case that pins the no-truncation half of the invariant -- the bytes are
/// still `seed\n` afterwards, which `assert_workspace_pristine` cannot see
/// because it compares entry names only.
///
/// WHAT REPLACES THIS WHEN APPEND IS ADMITTED. #161 carries two candidate
/// remedies and one of them lifts the refusal. On that day the fixture's open
/// succeeds, it exits `131`, and this case fails on the `finished:` assertion
/// -- deliberately, rather than passing vacuously. What to write in its place
/// is the **positive invariant**, which is expressible here and in
/// `a_rust_openoptions_append_to_a_non_empty_file_stops_the_run_161` and nowhere
/// else in this set: *each successful write extends the existing content*. Keep
/// the `seed.txt` read and change what it expects -- `seed\n` followed by the
/// appended bytes, not the appended bytes alone, which is what a silent
/// truncate-and-write would leave. The other remedy, answering `Deny(ENOTSUP)`,
/// turns this into a tracee-errno case shaped like the two controls below.
#[test]
fn a_routed_bare_append_to_a_non_empty_file_stops_the_run_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append(scratch.path(), &host, port, "bare-existing");

    assert_append_refusal_stopped_the_run(&run, &host, port, "bare-existing");
    assert_seed_untouched(&run, &host, port);
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// A routed `O_CREAT | O_WRONLY | O_APPEND` on an **absent** path stops the run
/// and creates **nothing** -- #161.
///
/// MEASURED. `O_CREAT` is what carries this past the absent test that answers
/// the control below, so the call reaches the append check and is refused with
/// the same `UnsupportedCapability` and the same missing `finished:` line as
/// the bare case above. Flag word `0x209`.
///
/// This is the **E4 shape**: a persistence layer that opens its log
/// append-mode-and-create-if-needed issues exactly this, which is why the
/// create-absent pair exists at all rather than only the two bare cases.
///
/// `umbra-userspace-edges.c::case_rollbackchild` already issues this same flag
/// combination and its test already asserts the run-stop disposition, so the
/// *disposition* is not what this adds. What it adds is that the refused open
/// left **nothing behind**: no object at the target in the run's shadow.
/// `rollbackchild`'s subject is run-scoping and it asserts nothing about
/// creation, so this is net-new coverage rather than a second copy of it.
///
/// WHAT REPLACES THIS WHEN APPEND IS ADMITTED. The fixture's open succeeds and
/// it exits `131`, failing this case on purpose. The replacement is not the
/// positive append invariant -- there is no prior content here to extend, so
/// this case cannot express it -- but the creation half: the target now exists
/// in the shadow holding exactly the bytes written. Invert the
/// `read_through_client` assertion below rather than deleting it.
#[test]
fn a_routed_creating_append_to_an_absent_path_stops_the_run_and_creates_nothing_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append(scratch.path(), &host, port, "create-absent");

    assert_append_refusal_stopped_the_run(&run, &host, port, "create-absent");
    assert_eq!(
        read_through_client(&host, port, &shadow_leaf(&run, b".ap")),
        None,
        "the refused creating append left an object behind in the run's shadow"
    );
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// Rust's `OpenOptions::append(true)` on a non-empty file stops the run exactly
/// as the C bare case does -- #161.
///
/// MEASURED, and the measurement is the point of the pair. `std` does not
/// emulate append: it sets `O_APPEND` in the flag word it hands to `open`,
/// which is a traced stub, so this arrives at the same decision point and is
/// refused with **byte-identical** text to
/// `a_routed_bare_append_to_a_non_empty_file_stops_the_run_161`. Same missing
/// `finished:` line, same absent `RunCompleted`.
///
/// That the two languages agree is not a foregone conclusion in this tree, and
/// that is why the pair exists rather than one fixture. One slice over,
/// `umbra-userspace-stdio.c` and `umbra-userspace-buffered.rs` measured the
/// opposite answer to the same question: C stdio's buffered write escapes
/// routing entirely through `__write_nocancel`(397) while Rust's does not,
/// because std's write is compiled into the executable where `DYLD_INTERPOSE`
/// reaches it. "Both languages do the same thing here" had to be measured.
///
/// This is the **E3 shape**. The fixture opens with `.read(true).append(true)`
/// because the persistence code it transcribes does; the append bit alone
/// decides the refusal, so the read bit is fidelity rather than mechanism.
///
/// WHAT REPLACES THIS WHEN APPEND IS ADMITTED. The same replacement as its C
/// twin, and it should be made to both in one change or the pair stops being a
/// comparison: the positive invariant, *each successful write extends the
/// existing content* -- `seed\n` followed by the appended bytes.
#[test]
fn a_rust_openoptions_append_to_a_non_empty_file_stops_the_run_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append_std(scratch.path(), &host, port, "bare-existing");

    assert_append_refusal_stopped_the_run(&run, &host, port, "bare-existing");
    assert_seed_untouched(&run, &host, port);
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// Rust's `.append(true).create(true)` on an absent path stops the run and
/// creates nothing -- #161.
///
/// MEASURED. The Rust half of the **E4 shape**, and the same refusal text as
/// its C twin. `create(true)` plays the role `O_CREAT` plays there: it carries
/// the call past the absent test and into the append check.
///
/// WHAT REPLACES THIS WHEN APPEND IS ADMITTED. As with the C twin: no prior
/// content, so no positive append invariant to state -- invert the shadow
/// assertion to require the created object instead of its absence.
#[test]
fn a_rust_openoptions_creating_append_to_an_absent_path_stops_the_run_and_creates_nothing_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append_std(scratch.path(), &host, port, "create-absent");

    assert_append_refusal_stopped_the_run(&run, &host, port, "create-absent");
    assert_eq!(
        read_through_client(&host, port, &shadow_leaf(&run, b".ap")),
        None,
        "the refused creating append left an object behind in the run's shadow"
    );
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// **Ordering control.** A routed *bare* `O_APPEND` on an absent path answers
/// the tracee `ENOENT` and the run survives -- it never reaches the append
/// check at all.
///
/// MEASURED, and it is the opposite disposition to all four cases above:
/// `child_exit()` is `2`, umbra prints its `finished: Some(Code(2))` line, and
/// the journal **does** record `RunCompleted`. Nothing in the run's stderr
/// mentions `O_APPEND`.
///
/// This case is not an append measurement and must not be read as one. Without
/// `O_CREAT` the target does not resolve and name resolution answers first, so
/// what it actually measures is `notfound` reached through a path carrying the
/// append bit. It is here for one reason: a change that moved the append test
/// above the absent test would convert this from a tracee errno into a run
/// stop, and every other case in this set would keep passing. It is the
/// positive half whose absence `umbra-userspace-edges.c`'s header records probe
/// D as having had.
///
/// Its sentinel is written **after** the refused open rather than before, per
/// `missingparent`: the refusal here is non-fatal, so the run is still alive to
/// write it, and that it lands is the second half of the claim -- the run kept
/// routing past the refused call rather than merely surviving it.
///
/// WHAT CHANGES WHEN APPEND IS ADMITTED: nothing. This case does not reach the
/// append check, so lifting the refusal leaves it exactly as it is -- which is
/// what makes it a control. If it *does* change, the ordering changed with it,
/// and that is the regression it exists to report.
#[test]
fn a_routed_bare_append_to_an_absent_path_answers_enoent_before_the_append_check_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append(scratch.path(), &host, port, "bare-absent");

    assert_control_answered_enoent(&run, &host, port, "bare-absent");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// **Ordering control**, the Rust half: `OpenOptions::append(true)` with no
/// `create` on an absent path answers `ENOENT` and the run survives.
///
/// MEASURED, and identical to the C control -- `child_exit()` `2`, a
/// `finished:` line, a recorded `RunCompleted`. `std` reports it as
/// `ErrorKind::NotFound` with `raw_os_error() == Some(2)`, and the fixture
/// returns that errno verbatim rather than a code of its own, so the `2`
/// asserted here is the tracee's own answer rather than a fixture convention.
///
/// Both halves of the pair are controlled because the ordering claim has to
/// hold for both call paths: `std` could have chosen to pre-`stat`, or to add
/// `O_CREAT` under some condition, and then this would not be the same
/// measurement as its C twin. It is.
///
/// WHAT CHANGES WHEN APPEND IS ADMITTED: nothing, for the C control's reason.
#[test]
fn a_rust_openoptions_append_to_an_absent_path_answers_enoent_before_the_append_check_161() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_append_std(scratch.path(), &host, port, "bare-absent");

    assert_control_answered_enoent(&run, &host, port, "bare-absent");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// The shared assertions for the two ordering controls.
///
/// The mirror image of `assert_append_refusal_stopped_the_run`, and worth
/// reading against it: every claim is inverted. The tracee *does* get an answer
/// and it is `ENOENT`; the run *does* record `RunCompleted`; the stderr does
/// **not** mention `O_APPEND`. That last one is the ordering claim stated
/// directly -- if the append check had fired, its message would be there.
///
/// The sentinel is read for the same reason as in the stop case, but it proves
/// something slightly different here: written *after* the refused open, its
/// presence says the run kept routing past the refusal rather than limping to
/// an exit.
fn assert_control_answered_enoent(run: &Run, host: &str, port: u16, case: &str) {
    assert_eq!(
        run.child_exit(),
        2,
        "a bare routed append to an absent path did not answer ENOENT:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("O_APPEND"),
        "the append check fired on a path that should have been answered by \
         name resolution first -- the ordering changed:\n{}",
        run.stderr
    );
    assert!(
        journal_records_completion(run),
        "a run whose refusal was answered to the tracee recorded no \
         RunCompleted, so it was stopped rather than answered"
    );
    let mut live = shadow_path(run);
    live.last_mut()
        .expect("the shadow path has a leaf")
        .extend_from_slice(format!(".{case}live").as_bytes());
    assert_eq!(
        read_through_client(host, port, &live)
            .expect("the post-refusal sentinel is in the run's shadow")
            .as_slice(),
        case.as_bytes(),
        "the run did not keep routing after the refused open"
    );
    assert_eq!(
        read_through_client(host, port, &shadow_leaf(run, b".ab")),
        None,
        "the refused bare append created the object it could not open"
    );
    assert_host_write_root_empty(run);
    assert_workspace_pristine(run);
}

/// The shared assertions for the ten descriptor-primitive refusals.
///
/// `assert_control_answered_enoent`'s shape rather than
/// `assert_append_refusal_stopped_the_run`'s, and the choice is a measurement
/// rather than a preference. The append refusal is *propagated*: the run stops
/// inside the open, the tracee is never resumed, and there is no `finished:`
/// line and no `RunCompleted` to read. This refusal is **answered**: every one
/// of the ten produces `finished: Some(Code(9))`, a recorded `RunCompleted`,
/// and `umbra` exiting non-zero only because `ProcessFailed` wraps a nonzero
/// child. Transcribing the stop-shaped helper here would have asserted the
/// opposite of what happens.
///
/// Nine claims, and the order is the order they become readable:
///
/// 1. **`child_exit() == 9`.** The tracee's own answer, `EBADF`, read from the
///    status line umbra prints. A program *can* branch on this, which is what
///    separates this disposition from the append pair's.
/// 2. **umbra itself failed.** `ProcessFailed` wrapping a nonzero child, so a
///    green `umbra` exit here would mean the fixture reported success.
/// 3. **No `UnsupportedCapability` in the stderr.** The P1 claim stated
///    directly: if this refusal were propagated the way `O_APPEND`'s is, that
///    diagnostic would be there and there would be no exit code above to read.
/// 4. **`RunCompleted` is recorded.** The journal half of the same claim. A run
///    stopped by a refused operation never reaches `finish_run`.
/// 5. **The pre-sentinel reads back as the case name.** Written before the
///    primitive, so it proves the run was alive and routing into its shadow
///    right up to the refusal -- this file's standing discipline, where an
///    absence assertion is never allowed to stand alone.
/// 6. **The post-sentinel reads back as the case name.** Written after the
///    refusal, so it proves routing *survived* it. Five and six together are
///    the slate's boundedness invariant, asserted on bytes through the client
///    rather than inferred from the exit code. The fixture also read this
///    object back in-process, through routing, before exiting; this read comes
///    over NFSv4 from the server instead, so the claim is made twice by two
///    paths that share no code.
/// 7. **The host `seed.txt` is still `seed\n`.** `assert_workspace_pristine`
///    compares entry *names* only, so a truncation of the base file -- exactly
///    what a mishandled `ftruncate` or `pwrite` would produce -- passes it in
///    silence. Reading the bytes is what catches that.
/// 8. **The shadow `seed.txt` is `Some(b"seed\n")` -- present and byte-identical.**
///    This is the one place `assert_seed_untouched` must **not** be copied, and
///    the reason is measured. That helper asserts the shadow object is `None`,
///    on the correct ground that an append refusal returns from `resolve`
///    before any copy-up. Here the open must *succeed* to produce a descriptor
///    at all, and MEASURED: it copies the base file up, in all ten cases. So
///    the assertion is the stronger one -- the object exists and its bytes are
///    unchanged -- and transcribing the `None` would have failed every case for
///    the wrong reason.
/// 9. **Host negative space.** No host write allowance was spent and the
///    workspace holds exactly `seed.txt`.
fn assert_descriptor_refusal_answered_ebadf(run: &Run, host: &str, port: u16, case: &str) {
    assert_eq!(
        run.child_exit(),
        9,
        "the refused descriptor primitive did not answer EBADF to the tracee:\n{}",
        run.stderr
    );
    assert_ne!(
        run.status,
        Some(0),
        "umbra reported success for a run whose child failed:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("UnsupportedCapability"),
        "the refusal was propagated as an unsupported capability instead of \
         being answered to the tracee -- the disposition changed:\n{}",
        run.stderr
    );
    assert!(
        journal_records_completion(run),
        "a run whose refusal was answered to the tracee recorded no \
         RunCompleted, so it was stopped rather than answered"
    );
    assert_eq!(
        read_through_client(
            host,
            port,
            &shadow_leaf(run, format!(".{case}live").as_bytes())
        )
        .expect("the pre-refusal sentinel is in the run's shadow")
        .as_slice(),
        case.as_bytes(),
        "the run was not routing into its shadow before the refused primitive, \
         so nothing else here means anything"
    );
    assert_eq!(
        read_through_client(
            host,
            port,
            &shadow_leaf(run, format!(".{case}after").as_bytes())
        )
        .expect("the post-refusal sentinel is in the run's shadow")
        .as_slice(),
        case.as_bytes(),
        "the run did not keep routing after the refused primitive, so the \
         refusal was not bounded"
    );
    assert_eq!(
        std::fs::read(run.workspace.join("seed.txt")).expect("the seeded base file"),
        b"seed\n",
        "the refused primitive changed the base file's bytes"
    );
    assert_eq!(
        read_through_client(host, port, &shadow_seed(run)).as_deref(),
        Some(&b"seed\n"[..]),
        "the routed open's copy-up of the base file is missing or no longer \
         byte-identical, so the refusal was not without effect"
    );
    assert_host_write_root_empty(run);
    assert_workspace_pristine(run);
}

/// The shared assertions for the two served-member controls.
///
/// The mirror image of the helper above, and worth reading against it: the
/// primitive **succeeds** on the very descriptor the ten are refused on, so the
/// fixture exits `141` rather than an errno, and the post-sentinel it would
/// have written on a refusal is **absent**.
///
/// That absence is the sharpest claim here and it is non-vacuous for a reason
/// the file already relies on elsewhere: the pre-sentinel is read back in the
/// same breath, so the shadow demonstrably exists and was being routed into.
/// A `None` for the post-sentinel therefore means the fixture never reached its
/// refusal path, which is the same statement as "the primitive was served" made
/// on bytes instead of on an exit code.
///
/// **Why this is the slice's non-vacuity mechanism.** Ten `EBADF`s on their own
/// are consistent with a duller claim than the one being made -- that
/// descriptor-relative calls simply do not work on a routed descriptor, or that
/// the open never routed. `fstat` and `File::metadata` are descriptor-relative,
/// are issued on the same descriptor at the same point in the same run, and are
/// answered. That makes the ten refusals a statement about `TRACED_STUBS`
/// *membership* -- these calls have no row -- rather than about descriptors.
/// It is stronger than a mutation probe would be here, because it runs in the
/// **unmutated** job.
fn assert_descriptor_control_was_served(run: &Run, host: &str, port: u16, case: &str) {
    assert_eq!(
        run.child_exit(),
        141,
        "a served member was not served on the routed descriptor -- the \
         membership claim the ten refusals rest on has changed:\n{}",
        run.stderr
    );
    assert!(
        journal_records_completion(run),
        "the control run recorded no RunCompleted"
    );
    assert_eq!(
        read_through_client(
            host,
            port,
            &shadow_leaf(run, format!(".{case}live").as_bytes())
        )
        .expect("the control's sentinel is in the run's shadow")
        .as_slice(),
        case.as_bytes(),
        "the control was not routing into its shadow, so the absence below \
         would be asserting nothing"
    );
    assert_eq!(
        read_through_client(
            host,
            port,
            &shadow_leaf(run, format!(".{case}after").as_bytes())
        ),
        None,
        "the control wrote its post-refusal sentinel, so it took the refusal \
         path after all"
    );
    assert_eq!(
        std::fs::read(run.workspace.join("seed.txt")).expect("the seeded base file"),
        b"seed\n",
        "the served primitive changed the base file's bytes"
    );
    assert_eq!(
        read_through_client(host, port, &shadow_seed(run)).as_deref(),
        Some(&b"seed\n"[..]),
        "the routed open's copy-up of the base file is missing or no longer \
         byte-identical"
    );
    assert_host_write_root_empty(run);
    assert_workspace_pristine(run);
}

/// `lseek(fd, 0, SEEK_END)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED, not predicted. The routed `open` of `seed.txt` succeeds and
/// returns a descriptor above the 4096 fence -- it is umbra's -- and `read` on
/// that same descriptor is served. `lseek`(199) has no row in
/// `abi::TRACED_STUBS`, so no breakpoint covers it, the call reaches the kernel
/// bare, and the kernel has never heard of the number: `errno` is `EBADF`(9),
/// the tracee is resumed to report it, and the run records `RunCompleted`.
///
/// This is the one a reader expects to work, and that is why it leads: a
/// descriptor with no file position is not a file descriptor in any sense a
/// program recognises. Every `std`-style read-then-seek-then-read loop, every
/// "how big is this" probe written as seek-to-end, and every format reader that
/// rewinds breaks on it, and breaks with an errno that says the descriptor is
/// invalid rather than that the operation is unsupported -- which sends the
/// caller looking for its own bug.
///
/// WHAT REPLACES THIS WHEN `lseek` IS ADMITTED. The fixture exits `141` and
/// this case fails on `child_exit() == 9`, deliberately, rather than passing
/// vacuously. What to write in its place is the **positive invariant**: a seek
/// to `SEEK_END` returns the object's size -- `5` for `seed\n` -- a seek to
/// `SEEK_SET` returns `0`, and a `read` after each one starts at the offset the
/// seek named. Keep the two sentinels and the shadow `seed.txt` read exactly as
/// they are; it is only the exit code and the new offset claim that change.
/// Nothing recorded here is acceptable permanent behaviour.
#[test]
fn a_routed_lseek_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "lseek");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "lseek");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `fcntl(fd, F_DUPFD, 0)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED. `fcntl`(92) has no `TRACED_STUBS` row, so the aliasing request
/// never reaches the overlay and the kernel answers `EBADF`(9) for a number it
/// does not own. The run survives and records `RunCompleted`.
///
/// The command is `F_DUPFD`(0), the bare duplicate. Its Rust twin issues
/// `F_DUPFD_CLOEXEC`(67) instead -- a different command on the same syscall,
/// which is why
/// `a_rust_file_try_clone_on_a_routed_descriptor_is_answered_ebadf` is a
/// separate case and not a duplicate of this one. A read-only command on the
/// same syscall (`F_GETFL`) was measured to be refused identically, so the
/// refusal is the **syscall's** and not the command's; that is recorded here
/// rather than given a case of its own, because it adds no disposition. Bare
/// `dup`(41) is a third syscall again and is deliberately out of scope: it is
/// in neither language's `std` path.
///
/// WHAT REPLACES THIS WHEN `fcntl` IS ADMITTED. The positive invariant for an
/// alias is **shared offsets**: the two descriptors name one open file
/// description, so a `read` on the duplicate continues where a `read` on the
/// original stopped, a seek through either is visible through the other, and
/// closing one leaves the other usable. Assert that, not merely that the
/// duplicate is a number above the fence -- a duplicate with its own private
/// offset would satisfy the weaker claim and be wrong.
#[test]
fn a_routed_fcntl_dupfd_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "dupfd");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "dupfd");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `ftruncate(fd, 2)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED. `ftruncate`(201) has no `TRACED_STUBS` row and the kernel answers
/// `EBADF`(9) for a descriptor number it does not own.
///
/// This is the destructive one, and the assertion that matters most is the one
/// that could not be read off an exit code: the base file's bytes are still
/// `seed\n`, on the host *and* in the shadow. A truncation that half-landed --
/// the shape where the call fails after the object has already been shortened
/// -- would exit `9` exactly like a clean refusal and pass any assertion that
/// only read the status. `assert_workspace_pristine` would not catch it either,
/// since it compares entry names. The byte reads are what make the refusal
/// *bounded* rather than merely reported.
///
/// WHAT REPLACES THIS WHEN `ftruncate` IS ADMITTED. The positive invariant is
/// **exact truncation**: after `ftruncate(fd, 2)` the object is exactly two
/// bytes, those bytes are `se`, and a read past the new end returns nothing.
/// Extension is the other half and is worth asserting in the same breath --
/// `ftruncate` past the end zero-fills, so growing `seed\n` to 8 bytes must
/// leave `seed\n\0\0\0` and not 8 bytes of anything else. Keep both byte reads
/// and change what they expect.
#[test]
fn a_routed_ftruncate_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "ftruncate");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "ftruncate");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `fsync(fd)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED. `fsync`(95) has no `TRACED_STUBS` row and the kernel answers
/// `EBADF`(9).
///
/// This is the refusal a caller is least able to compensate for, and the reason
/// is the direction of the error. A program that cannot seek can often restructure
/// itself to read forward; a program that cannot *flush* has no way to obtain
/// the guarantee it asked for, and the only honest responses are to fail the
/// write it was about to acknowledge or to proceed without durability. `EBADF`
/// is also the least informative answer it could get: the descriptor it just
/// wrote through successfully is reported invalid, which reads as a bug in the
/// caller rather than as a missing capability.
///
/// Note the asymmetry with the Rust half, which is measured and not incidental:
/// `File::sync_all` does **not** issue this syscall. It issues
/// `fcntl(F_FULLFSYNC)`, so this case is the only `fsync`(2) call site in the
/// pair. Its Rust counterpart is
/// `a_rust_file_sync_all_on_a_routed_descriptor_is_answered_ebadf`.
///
/// WHAT REPLACES THIS WHEN `fsync` IS ADMITTED. The positive invariant is an
/// **honest sync result**: `fsync` returns zero only once the bytes written
/// through that descriptor are durable in the store, and returns an error
/// otherwise -- never zero on a flush that did not happen. Assert it by reading
/// the written bytes back through the client *after* the successful `fsync` and
/// before the descriptor is closed, which is the only ordering where "durable
/// now" is distinguishable from "durable at close".
///
/// **Admitting the syscall is not sufficient here, and this case must not be
/// rewritten as though it were.** An honest sync result cannot be issued until
/// the userspace-NFS flush work lands -- verifier-mismatch recovery, and the
/// strict-remote-persistence mode that is still refused at `open_run`, which
/// are **#108 items 2-3**. A `TRACED_STUBS` row for 95 added before then would
/// buy a call that returns zero with no durability guarantee behind it, which
/// is a worse outcome than the `EBADF` recorded here: it is the
/// silent-wrong-answer shape rather than the loud-refusal shape. If this case
/// goes red because the row was added alone, the replacement assertion is that
/// `fsync` reports an **error**.
#[test]
fn a_routed_fsync_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "fsync");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "fsync");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `pread(fd, buf, 4, 0)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED, and the sharpest pair in the file: `read` on this very descriptor
/// is **served**, and `pread`(153) -- the same transfer at an explicit offset --
/// is not. So what the refusal isolates is *positional*, not reading. The
/// descriptor is umbra's and the data is reachable through it, and the two
/// spellings are served by **different mechanisms, not by one table**: `read`
/// is one of the four functions the interposer replaces, `fstat`(339) below is
/// reached through a `TRACED_STUBS` breakpoint, and `pread` has **neither** --
/// no interposer entry and no row. "Not interposed" is not the same as "not
/// routed", which is the distinction `umbra_interpose.c`'s own LIMITS block
/// warns against collapsing and which this file's header states for
/// `__write_nocancel`(397).
///
/// This is the shape a reader of a random-access format issues -- an index, a
/// page cache, a database file -- and such a reader rarely has a fallback,
/// because "seek then read" is also refused (see
/// `a_routed_lseek_on_a_routed_descriptor_is_answered_ebadf`). The two
/// refusals together leave no way to read anything but a stream.
///
/// WHAT REPLACES THIS WHEN `pread` IS ADMITTED. The positive invariant is an
/// **unchanged file offset**: `pread` returns the bytes at the offset it names
/// -- `seed` for offset 0, length 4 -- and the descriptor's own position is
/// exactly what it was before the call. Assert the second half by reading
/// through the same descriptor afterwards and requiring that it continues from
/// where it was, not from where the `pread` ended; a `pread` implemented as
/// seek-read-seek would pass the first half and fail this.
#[test]
fn a_routed_pread_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "pread");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "pread");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `pwrite(fd, "X", 1, 0)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED. `pwrite`(154) has no `TRACED_STUBS` row, while plain `write` on
/// the same descriptor is served -- the `pread` case's isolation, in the
/// writing direction.
///
/// It is also the second destructive case, and the byte assertions carry the
/// same weight they do for `ftruncate`: a `pwrite` that landed its byte and
/// *then* failed would exit `9` indistinguishably from one that never touched
/// the object. The base file is still `seed\n` on the host and byte-identical
/// in the shadow, so the single `X` this case tried to place at offset 0 went
/// nowhere.
///
/// WHAT REPLACES THIS WHEN `pwrite` IS ADMITTED. The positive invariant is the
/// `pread` case's, in the other direction: the byte lands at the offset named
/// and **nowhere else** -- `Xeed\n`, not `seed\nX` and not `X` alone -- and the
/// descriptor's own position is unchanged afterwards. The "nowhere else" half
/// is the one that needs the shadow read: an implementation that appended, or
/// that truncated and rewrote, would satisfy "the byte is in the file".
#[test]
fn a_routed_pwrite_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "pwrite");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "pwrite");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `File::seek(SeekFrom::End(0))` on a routed descriptor is answered `EBADF`.
///
/// MEASURED, and the Rust half of the pair agrees with the C half here:
/// `std` compiles this straight to `lseek`(199), the same syscall
/// `a_routed_lseek_on_a_routed_descriptor_is_answered_ebadf` issues, and the
/// disposition is identical -- `child_exit()` `9`, a `finished:` line, a
/// recorded `RunCompleted`.
///
/// That agreement is the measurement rather than an assumption, and it is not
/// the default outcome in this file: `umbra-userspace-buffered.rs` exists
/// because C stdio and Rust `std` were measured to reach *different* syscalls
/// for an identical-looking write, and two of this pair's four shared shapes do
/// the same. Both halves are tested for that reason -- `std` could have
/// pre-`stat`ed, or cached the length, and then this would not be the same
/// measurement as its C twin. It is.
///
/// WHAT REPLACES THIS WHEN `lseek` IS ADMITTED. The C case's positive
/// invariant, expressed through `std`: `seek(End(0))` returns `5` for `seed\n`,
/// `rewind()` returns the position to `0`, and `Read` after each starts at the
/// offset the seek named. `stream_position()` is worth asserting in the same
/// breath, since it is the method callers actually use and it is a `seek` of
/// its own.
#[test]
fn a_rust_file_seek_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor_std(scratch.path(), &host, port, "seek");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "seek");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `File::try_clone()` on a routed descriptor is answered `EBADF`.
///
/// MEASURED, and this is where the pair earns its existence. `try_clone` does
/// **not** issue the `F_DUPFD`(0) its C twin issues: it issues `fcntl`(92) with
/// command **67**, `F_DUPFD_CLOEXEC`, because `std` sets the close-on-exec
/// variant unconditionally. Same syscall number, different command, and a
/// C-only fixture would have reported the wrong call for Rust's aliasing path.
/// The disposition is the same -- `EBADF`(9), run surviving -- because
/// `fcntl` has no `TRACED_STUBS` row at all, so no command of it is reachable.
///
/// Bare `dup`(41) is a third syscall again. It was measured to be refused
/// identically and is deliberately **not** a case here: it is in neither
/// language's `std` path, so a case for it would be scope rather than coverage.
///
/// WHAT REPLACES THIS WHEN `fcntl` IS ADMITTED. The alias invariant --
/// **shared offsets** -- stated through `std`: the clone and the original name
/// one open file description, so reading through the clone continues where the
/// original stopped, a `seek` through either is visible through the other, and
/// dropping one leaves the other usable. Assert the close-on-exec bit too,
/// since `try_clone` is specified to set it and a `F_DUPFD`-based
/// implementation would silently not.
#[test]
fn a_rust_file_try_clone_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor_std(scratch.path(), &host, port, "try_clone");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "try_clone");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `File::set_len(2)` on a routed descriptor is answered `EBADF`.
///
/// MEASURED. `set_len` compiles to `ftruncate`(201) -- the same syscall the C
/// half's `ftruncate` case issues, so the pair agrees here as it does for
/// `seek` -- and the kernel answers `EBADF`(9) for a descriptor number it does
/// not own.
///
/// The destructive-case reasoning is the C twin's and applies unchanged: the
/// byte reads on the host and in the shadow are what separate a clean refusal
/// from a truncation that half-landed, and neither the exit code nor
/// `assert_workspace_pristine` could tell them apart.
///
/// WHAT REPLACES THIS WHEN `ftruncate` IS ADMITTED. **Exact truncation**,
/// through `std`: after `set_len(2)` the file is exactly two bytes and they are
/// `se`; after `set_len(8)` it is exactly eight and the tail is zero-filled,
/// `seed\n\0\0\0`. `std` documents both directions of `set_len`, so both are
/// expressible here and both should be asserted -- an implementation that only
/// shortened would pass a one-directional test.
#[test]
fn a_rust_file_set_len_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor_std(scratch.path(), &host, port, "set_len");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "set_len");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// `File::sync_all()` on a routed descriptor is answered `EBADF`.
///
/// MEASURED, and the second place the pair's two halves diverge. `sync_all`
/// does **not** issue `fsync`(95): it issues `fcntl`(92) with command **51**,
/// `F_FULLFSYNC`, which is Darwin's stronger barrier. `File::sync_data` was
/// measured to issue the identical call on this SDK, so **Rust has no
/// `fsync`(2) call site for a `File` at all** -- which is why there is no
/// `sync_data` case: it would be a byte-identical measurement under a different
/// name, and the slate's one-scenario-per-primitive rule excludes it.
///
/// The disposition is `EBADF`(9) with the run surviving, the same as the C
/// half's `fsync`, for the same root cause: `fcntl` has no `TRACED_STUBS` row,
/// so no command of it reaches the overlay. The durability reasoning is the C
/// twin's -- a caller that cannot flush cannot obtain the guarantee it asked
/// for, and `EBADF` tells it to look for a bug in itself instead.
///
/// WHAT REPLACES THIS WHEN `fcntl` IS ADMITTED. An **honest sync result**:
/// `sync_all` returns `Ok(())` only once the bytes written through that `File`
/// are durable in the store, and an `Err` otherwise -- never `Ok` for a flush
/// that did not happen, which is the failure mode `umbra-userspace-stdio.c`
/// characterizes for a different call. Assert it by reading the written bytes
/// back through the client after the `Ok` and before the `File` is dropped.
/// `sync_data`'s own result is worth asserting beside it once the call is
/// reachable, since the two are the same syscall today and need not stay so.
///
/// **Admitting `fcntl` is not sufficient here either**, for the C twin's
/// reason: the honest result this case is waiting on is gated on the
/// userspace-NFS flush work -- verifier-mismatch recovery, and the
/// strict-remote-persistence mode still refused at `open_run`, which are
/// **#108 items 2-3** -- not on the syscall becoming reachable. An `Ok(())`
/// issued before then would be the silent wrong answer, and the replacement
/// assertion in that interim is that `sync_all` returns an `Err`.
#[test]
fn a_rust_file_sync_all_on_a_routed_descriptor_is_answered_ebadf() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor_std(scratch.path(), &host, port, "sync_all");

    assert_descriptor_refusal_answered_ebadf(&run, &host, port, "sync_all");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// **Served-member control**, the C half: `fstat(fd, &info)` **succeeds** on the
/// very descriptor the six C primitives are refused on.
///
/// MEASURED: `child_exit()` `141` -- the fixture's "the primitive succeeded"
/// status -- a recorded `RunCompleted`, and no post-refusal sentinel, because
/// the fixture never reached the path that writes one.
///
/// This is the most important case in the slice and the reason the other ten
/// mean what they claim. Six `EBADF`s on their own are consistent with a much
/// duller reading: that descriptor-relative calls do not work on a routed
/// descriptor, or that the `open` never routed at all and the fence guard is
/// the only thing standing between this suite and a green test asserting
/// nothing. `fstat`(339) is descriptor-relative, is issued on the same
/// descriptor at the same point in the same run, and is **answered**. So the
/// refusals are a statement about `TRACED_STUBS` **membership** -- one row per
/// admitted call, and the six have no row -- rather than about descriptors.
///
/// It is also this slice's non-vacuity mechanism in place of a mutation probe,
/// and a stronger one, because it runs in the **unmutated** job rather than
/// only under a deliberately broken binary.
///
/// WHAT CHANGES WHEN THE OTHERS ARE ADMITTED: nothing here, and that is the
/// point -- this case is the invariant the others are measured against. It
/// **inverts**, though: anything but `141` is a regression rather than
/// progress, and if `fstat` ever starts answering `EBADF` the ten refusal cases
/// stop being evidence of anything and should be read as broken rather than as
/// newly correct.
#[test]
fn a_routed_fstat_is_served_on_the_descriptor_the_others_are_refused_on() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor(scratch.path(), &host, port, "fstat");

    assert_descriptor_control_was_served(&run, &host, port, "fstat");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}

/// **Served-member control**, the Rust half: `File::metadata()` **succeeds** on
/// the very descriptor the four Rust primitives are refused on.
///
/// MEASURED, and identical to the C control -- `child_exit()` `141`, a recorded
/// `RunCompleted`, no post-refusal sentinel. `std`'s `metadata` on an open
/// `File` is `fstat`(339), the same syscall the C control issues, so the pair
/// agrees here.
///
/// Both halves are controlled rather than one, for the reason the append pair
/// controls both and for a reason this pair has measured twice: `std` does not
/// reliably reach the same syscall as the obvious C spelling. It does not for
/// `try_clone` and it does not for `sync_all`. So "the Rust side's served
/// member is served" is a separate measurement from the C side's, not a
/// corollary of it, and without it the four Rust refusals would rest on the C
/// control alone.
///
/// WHAT CHANGES WHEN THE OTHERS ARE ADMITTED: nothing, and it inverts the same
/// way the C control does -- anything but `141` here is a regression, and it
/// would invalidate the four Rust refusal cases rather than improve them.
#[test]
fn a_rust_file_metadata_is_served_on_the_descriptor_the_others_are_refused_on() {
    if declared_probe().is_some() {
        eprintln!("SKIP: UMBRA_MUTATION_PROBE names a mutated binary");
        return;
    }
    let Some((host, port)) = fixture() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    assert_no_nfs_mount(&host, port, &[scratch.path()], "before the run");
    let run = routed_descriptor_std(scratch.path(), &host, port, "metadata");

    assert_descriptor_control_was_served(&run, &host, port, "metadata");
    assert_no_nfs_mount(&host, port, &[scratch.path()], "after the run");
}
