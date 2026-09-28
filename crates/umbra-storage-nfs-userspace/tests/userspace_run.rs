//! `umbra run` over the userspace NFSv4 client, end to end -- the toy program,
//! the three standard utilities, and the six mutation probes that prove the
//! routing is load-bearing.
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
//! **Six separate invocations**, because the probes are compile-time and no two
//! may be enabled at once. Always rebuild the unmutated binaries afterwards.
//!
//! ```text
//! # end to end: the toy must exit 0, and the utility matrix must pass
//! cargo build --workspace --bins
//! cargo test -p umbra-storage-nfs-userspace --features transport-raw --test userspace_run
//!
//! # probe A -- read routing broken; the toy must exit 8
//! cargo build -p umbra-cli --features mutation-probe-read
//! UMBRA_MUTATION_PROBE=read cargo test -p umbra-storage-nfs-userspace \
//!     --features transport-raw --test userspace_run
//!
//! # probe B -- write routing broken; the toy must exit 7
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
//! ```
//!
//! **The probes are not all on the same crate, and the build command differs
//! because of it.** A and B and C are features of `umbra-overlay`, forwarded
//! through `umbra-supervisor` to `umbra-cli`, so they go into the `umbra`
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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use umbra_core::{
    BytePath, JournalAccess, JournalControlBinding, JournalFormatPolicy, JournalLifecycle,
    JournalOpenRequest, JournalPayload, PhysicalPath, RunId, Sequence,
};
use umbra_journal::Journal;
use umbra_journal_file::FileJournal;
use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, Nfs4Type, RawTransport,
};

/// Bytes the toy writes and reads back. Must match `umbra-userspace-toy.c`.
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

/// Launch one of the edge cases, which take a case name before the path.
fn routed_edge(scratch: &Path, host: &str, port: u16, case: &str, env: &[&str]) -> Run {
    let edges = fixture_binary(scratch, "umbra-userspace-edges", "UMBRA_EDGES_PATH");
    launch(scratch, host, port, &edges, &[case], env)
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
/// which closes that half. A probe cannot be hard-failed the same way -- five
/// probes across six invocations means four or five legitimately skip every
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
