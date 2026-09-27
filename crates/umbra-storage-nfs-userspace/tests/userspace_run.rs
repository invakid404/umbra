//! `umbra run` over the userspace NFSv4 client, end to end, and the two
//! mutation probes that prove the routing is load-bearing.
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
//! Three separate invocations, because the probes are compile-time:
//!
//! ```text
//! # end to end: the toy must exit 0
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
//! ```
//!
//! `UMBRA_MUTATION_PROBE` selects which case runs; it does **not** enable a
//! probe. The probes are cargo features on `umbra-overlay`, so the `umbra`
//! binary either contains one or does not, and a mismatch between the variable
//! and the binary fails the case rather than passing it: each probe asserts an
//! exact exit code *and* a store state that only that probe produces.
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
