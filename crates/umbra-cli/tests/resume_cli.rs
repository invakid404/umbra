//! `umbra resume`: the exit contract for a run that requires recovery.
//!
//! Driven through the real binary and the real provider executables, because the
//! thing under test is what an operator's shell sees: a status line on stderr and
//! a nonzero exit, with no panic and no stack trace. A run that cannot be served
//! must not exit 0, and a diagnosis must not look like a crash.
//!
//! Like the other CLI tests here, every invocation runs with stdin closed.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use umbra_core::{
    provider::encode, AcquireWriterRequest, BytePath, ImmutableBaseContract, JournalAccess,
    JournalControlBinding, JournalFencingEvidence, JournalFormatPolicy, JournalIntent,
    JournalOpenRequest, JournalPayload, JournalRecord, JournalWriterAuthority, LeaseEpoch,
    ObjectId, OpenRunIntent, OpenRunRequest, OperationId, PhysicalPath, RunId, Sequence,
    StoragePolicy, TakeoverPolicy, WriterId,
};
use umbra_journal::Journal;
use umbra_journal_file::FileJournal;
use umbra_storage::Storage;
use umbra_storage_local::LocalStorage;
use umbra_supervisor::base::WorkspaceInventory;

fn umbra() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbra"));
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = umbra().args(args).output().expect("umbra binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A scratch directory that dies with the test.
///
/// Returns the guard alongside the path: dropping it removes the tree, so a run
/// of this suite leaves no storage roots, journals or registry files behind. The
/// path is canonicalised because the workspace fingerprint is computed from it
/// and `OpenExisting` validates that fingerprint against the run's own manifest
/// -- on macOS `TempDir` hands back a `/var/...` path that is a symlink to
/// `/private/var/...`, and the two do not fingerprint alike.
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}

/// The provider executables cargo builds for this workspace, if they are there.
///
/// **`cargo test` does not build other packages' `[[bin]]` targets.** It compiles
/// them only as test harnesses under `deps/`, so a plain
/// `target/<profile>/umbra-storage-local` exists only once something has run
/// `cargo build --bins`. A suite that assumed otherwise passes on a developer's
/// warm tree and fails on a clean checkout, which is how this arrived.
///
/// So: skip when they are absent, and fail when `UMBRA_INTEGRATION_REQUIRED` says
/// absence is not acceptable -- the same gate `run_fixtures` next door uses, for
/// the same reason. `CARGO_BIN_EXE_umbra` still resolves, because that binary is
/// this package's own; the providers are not.
fn provider_bin(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_umbra"));
    path.pop();
    let path = path.join(name);
    if !path.is_file() {
        assert!(
            std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
            "required integration needs the workspace binaries: run `cargo build \
             --workspace --bins` before this suite"
        );
        eprintln!(
            "SKIP {}: provider executable {} is missing; run `cargo build --workspace --bins`",
            name,
            path.display()
        );
        return None;
    }
    Some(path)
}

/// Both provider executables, or `None` when either is absent and skipping is
/// allowed. Probed together so a test skips as a whole rather than half-running.
fn providers() -> Option<(PathBuf, PathBuf)> {
    Some((
        provider_bin("umbra-storage-local")?,
        provider_bin("umbra-journal-file")?,
    ))
}

fn bytes_array(value: &[u8]) -> String {
    value
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn registry_file(path: &Path, storage_root: &Path, bins: &(PathBuf, PathBuf)) {
    let root = BytePath::new(storage_root.as_os_str().as_bytes().to_vec()).unwrap();
    let options = encode(&root).unwrap();
    // `local-development-v1` is the persistence mode these invocations select
    // with `--local-dev`. `resume` requires it exactly as `run` does, so an empty
    // capability set here would be refused -- which is the enforcement working.
    let storage = format!(
        r#""storage":{{"id":"local","role":"storage","protocol_version":2,"executable":[{}],"capabilities":["{}"],"options":[{}]}}"#,
        bytes_array(bins.0.as_os_str().as_bytes()),
        umbra_core::capabilities::STORAGE_LOCAL_DEVELOPMENT_V1,
        bytes_array(&options),
    );
    let journal = format!(
        r#""journal":{{"id":"file","role":"journal","protocol_version":2,"executable":[{}],"capabilities":[],"options":[]}}"#,
        bytes_array(bins.1.as_os_str().as_bytes()),
    );
    let body = format!(r#"{{"timeout_ms":5000,"providers":{{{storage},{journal}}}}}"#);
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
}

fn native(path: &BytePath) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes()))
}

/// Create a run whose journal holds a creating `Prepare` and no terminal record:
/// the shape a crash between `prepare` and `commit` leaves.
fn crashed_run(storage_root: &Path, workspace: &Path) -> RunId {
    let contract: ImmutableBaseContract =
        WorkspaceInventory::capture(workspace).unwrap().contract();
    let run_id = RunId(uuid::Uuid::new_v4());
    std::fs::create_dir_all(storage_root).unwrap();
    let mut storage = LocalStorage::new(storage_root).unwrap();
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: contract,
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        })
        .unwrap();
    let lease = storage
        .acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: WriterId("creator".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .unwrap();
    let control = native(binding.control.physical_path.as_ref().unwrap());
    storage.release_writer(&lease).unwrap();
    storage.close_run().unwrap();

    let mut journal = FileJournal::new();
    journal
        .open(&JournalOpenRequest {
            control: JournalControlBinding {
                run_id,
                directory: PhysicalPath(
                    BytePath::new(control.as_os_str().as_bytes().to_vec()).unwrap(),
                ),
            },
            access: JournalAccess::Writer(JournalWriterAuthority {
                run_id,
                writer_instance_id: "creator".into(),
                host_fingerprint: "test".into(),
                supervisor_version: "test".into(),
                acquired_at: 0,
                renewed_at: 0,
                lease_epoch: LeaseEpoch(1),
                fencing: JournalFencingEvidence::Fenced {
                    mechanism: "test".into(),
                    evidence: vec![],
                },
            }),
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        })
        .unwrap();
    journal
        .append(&JournalRecord {
            format_version: 1,
            sequence: Sequence(0),
            operation_id: OperationId(uuid::Uuid::new_v4()),
            writer_epoch: LeaseEpoch(1),
            payload: JournalPayload::Prepare {
                intent: JournalIntent::Create {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                    directory: false,
                    mode: 0o644,
                },
            },
        })
        .unwrap();
    journal.flush(Sequence(1)).unwrap();
    journal.close().unwrap();
    run_id
}

/// r4. A recovery-required run exits nonzero, says why in one line, and does not
/// look like a malfunction: no panic, no backtrace, and the reopen itself is
/// reported as having succeeded.
#[test]
fn resume_exits_nonzero_with_a_diagnostic_when_the_run_requires_recovery() {
    let Some(bins) = providers() else { return };
    let (_scratch, dir) = scratch();
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("input.txt"), b"unchanged").unwrap();
    let storage_root = dir.join("storage");
    let run_id = crashed_run(&storage_root, &workspace);
    let registry = dir.join("registry.json");
    registry_file(&registry, &storage_root, &bins);

    let (code, stdout, stderr) = run(&[
        "resume",
        &run_id.0.to_string(),
        "--registry",
        registry.to_str().unwrap(),
        "--workspace",
        workspace.to_str().unwrap(),
        "--local-dev",
    ]);

    assert_ne!(code, 0, "a run that cannot be served must not exit 0");
    assert!(
        stdout.is_empty(),
        "status belongs on stderr; stdout stays clean: {stdout}"
    );
    assert!(
        stderr.contains("requires recovery"),
        "the operator must be told what is wrong: {stderr}"
    );
    assert!(
        stderr.contains(&run_id.0.to_string()),
        "and which run it is about: {stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
        "a diagnosis must not look like a crash: {stderr}"
    );
}

/// The complement, so the nonzero exit above is about the verdict and not about
/// the command being broken: a healthy run reopens, reports, and exits 0.
#[test]
fn resume_exits_zero_when_the_run_needs_no_reconciliation() {
    let Some(bins) = providers() else { return };
    let (_scratch, dir) = scratch();
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("input.txt"), b"unchanged").unwrap();
    let storage_root = dir.join("storage");
    let contract = WorkspaceInventory::capture(&workspace).unwrap().contract();
    let run_id = RunId(uuid::Uuid::new_v4());
    std::fs::create_dir_all(&storage_root).unwrap();
    let mut storage = LocalStorage::new(&storage_root).unwrap();
    storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: contract,
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        })
        .unwrap();
    storage.close_run().unwrap();
    let registry = dir.join("registry.json");
    registry_file(&registry, &storage_root, &bins);

    let (code, _stdout, stderr) = run(&[
        "resume",
        &run_id.0.to_string(),
        "--registry",
        registry.to_str().unwrap(),
        "--workspace",
        workspace.to_str().unwrap(),
        "--local-dev",
    ]);
    assert_eq!(code, 0, "a healthy run reopens cleanly: {stderr}");
    assert!(stderr.contains("requires no reconciliation"), "{stderr}");
}

/// Resume is configured exactly like `run`: storage comes from the registry, and
/// the legacy flag is refused rather than quietly ignored.
#[test]
fn resume_refuses_the_legacy_storage_root_flag() {
    let (_scratch, dir) = scratch();
    let (code, _stdout, stderr) = run(&[
        "--storage-root",
        dir.to_str().unwrap(),
        "resume",
        &uuid::Uuid::new_v4().to_string(),
        "--registry",
        dir.join("absent.json").to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("--storage-root"), "{stderr}");
}
