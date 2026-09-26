//! r1-r4: reopening an existing run, over the real provider executables.
//!
//! These are the first tests in this workspace that drive `umbra-supervisor`
//! through the provider transport rather than through in-process doubles, and
//! that is deliberate: the reopen path's whole job is composition -- connect
//! storage, open the run with `OpenExisting`, reacquire writer authority, replay
//! the journal, bind, classify, close -- and a double would exercise none of it.
//!
//! Each case builds the run's durable state with the backends as *libraries*,
//! then reopens it through the same backends as *binaries*. What produced the
//! bytes does not matter to a reopen; what matters is that the reopen reads them
//! through the real wire.

use std::path::{Path, PathBuf};

use umbra_core::{
    provider::{encode, ProviderDescriptor, ProviderRegistry},
    AcquireWriterRequest, BytePath, ImmutableBaseContract, JournalAccess, JournalControlBinding,
    JournalFencingEvidence, JournalFormatPolicy, JournalIntent, JournalLifecycle,
    JournalOpenRequest, JournalPayload, JournalRecord, JournalWriterAuthority, ObjectId,
    OpenRunIntent, OpenRunRequest, OperationId, PhysicalPath, RunId, Sequence, StoragePolicy,
    TakeoverPolicy, WriterId,
};
use umbra_journal::Journal;
use umbra_journal_file::FileJournal;
use umbra_storage::Storage;
use umbra_storage_local::LocalStorage;
use umbra_supervisor::{base::WorkspaceInventory, ResumeSpec, RunPersistence, Supervisor};

/// The provider executables cargo builds for this workspace, if they are there.
///
/// **`cargo test` does not build other packages' `[[bin]]` targets.** It compiles
/// them only as test harnesses under `deps/`, so a plain
/// `target/<profile>/umbra-storage-local` exists only once something has run
/// `cargo build --bins`. A suite that assumed otherwise passes on a developer's
/// warm tree and fails on a clean checkout, which is how this arrived.
///
/// So: skip when they are absent, and fail when `UMBRA_INTEGRATION_REQUIRED` says
/// absence is not acceptable -- the same gate `umbra-cli`'s `run_fixtures` and
/// `umbra-platform-macos`'s suites use, for the same reason. `CARGO_BIN_EXE_*`
/// cannot help: it covers only the test's own package, and these belong to
/// others.
fn provider_bin(name: &str) -> Option<PathBuf> {
    let mut path = std::env::current_exe().expect("test binary has a path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
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
/// allowed. Probed together so a suite skips as a whole rather than half-running.
fn providers() -> Option<(PathBuf, PathBuf)> {
    Some((
        provider_bin("umbra-storage-local")?,
        provider_bin("umbra-journal-file")?,
    ))
}

/// A path's bytes as a host path, with no lossy UTF-8 conversion.
fn native(path: &BytePath) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes()))
}

fn descriptor(id: &str, role: &str, executable: PathBuf, options: Vec<u8>) -> ProviderDescriptor {
    use std::os::unix::ffi::OsStrExt;
    ProviderDescriptor {
        id: id.to_owned(),
        role: role.to_owned(),
        protocol_version: umbra_core::provider::PROTOCOL_VERSION,
        executable: BytePath::new(executable.as_os_str().as_bytes().to_vec()).unwrap(),
        capabilities: Default::default(),
        options,
    }
}

fn registry(storage_root: &Path, bins: (PathBuf, PathBuf)) -> ProviderRegistry {
    use std::os::unix::ffi::OsStrExt;
    let (storage_bin, journal_bin) = bins;
    let root = BytePath::new(storage_root.as_os_str().as_bytes().to_vec()).unwrap();
    let mut providers = std::collections::BTreeMap::new();
    let mut storage = descriptor("local", "storage", storage_bin, encode(&root).unwrap());
    // The persistence mode this registry is qualified for. `resume` requires it
    // exactly as `run` does, so an empty capability set here would be refused --
    // which is the enforcement working, not a fixture detail.
    storage
        .capabilities
        .insert(umbra_core::capabilities::STORAGE_LOCAL_DEVELOPMENT_V1.to_owned());
    providers.insert("storage".to_owned(), storage);
    providers.insert(
        "journal".to_owned(),
        descriptor("file", "journal", journal_bin, vec![]),
    );
    ProviderRegistry {
        providers,
        timeout_ms: 5000,
    }
}

/// A workspace whose fingerprint the reopen must reproduce exactly. `OpenExisting`
/// validates the contract against the run's own manifest, so this is not
/// incidental setup -- it is half of what the reopen proves.
fn workspace(dir: &Path) -> (PathBuf, ImmutableBaseContract) {
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("input.txt"), b"unchanged").unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let contract = WorkspaceInventory::capture(&workspace).unwrap().contract();
    (workspace, contract)
}

/// Create the run and hand back the control directory its journal lives in.
fn create_run(storage_root: &Path, run_id: RunId, contract: &ImmutableBaseContract) -> PathBuf {
    std::fs::create_dir_all(storage_root).unwrap();
    let mut storage = LocalStorage::new(storage_root).unwrap();
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: contract.clone(),
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
    control
}

fn open_journal(control: &Path, run_id: RunId, epoch: u64) -> (FileJournal, JournalOpenRequest) {
    use std::os::unix::ffi::OsStrExt;
    let request = JournalOpenRequest {
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
            lease_epoch: umbra_core::LeaseEpoch(epoch),
            fencing: JournalFencingEvidence::Fenced {
                mechanism: "test".into(),
                evidence: vec![],
            },
        }),
        format: JournalFormatPolicy {
            readable_versions: vec![1],
            write_version: 1,
        },
    };
    (FileJournal::new(), request)
}

fn record(epoch: u64, id: OperationId, payload: JournalPayload) -> JournalRecord {
    JournalRecord {
        format_version: 1,
        sequence: Sequence(0),
        operation_id: id,
        writer_epoch: umbra_core::LeaseEpoch(epoch),
        payload,
    }
}

fn spec(
    storage_root: &Path,
    run_id: RunId,
    workspace: PathBuf,
    bins: (PathBuf, PathBuf),
) -> ResumeSpec {
    ResumeSpec {
        registry: registry(storage_root, bins),
        run_id,
        workspace,
        persistence: RunPersistence::LocalDevelopment,
    }
}

/// r1: a run whose journal holds only finished work reopens clean.
///
/// `last_valid_sequence` is deliberately non-zero -- this is the E6 case, the one
/// `bind` could not have reached before #65 dropped that predicate, and a reopen
/// that refused it would make the whole widening vacuous.
#[test]
fn resuming_a_cleanly_finished_run_binds_without_requiring_recovery() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let id = OperationId(uuid::Uuid::new_v4());
    let (mut journal, request) = open_journal(&control, run_id, 1);
    journal.open(&request).unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Prepare {
                intent: JournalIntent::Unlink {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/gone".to_vec()).unwrap(),
                    directory: false,
                },
            },
        ))
        .unwrap();
    journal
        .append(&record(1, id, JournalPayload::Commit))
        .unwrap();
    journal.flush(Sequence(2)).unwrap();
    journal.close().unwrap();

    let outcome =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap();
    assert_eq!(outcome.run_id, run_id);
    assert!(
        !outcome.recovery_required,
        "a committed transaction leaves nothing to reconcile"
    );
    assert_eq!(outcome.unfinished, 0);
    assert_ne!(
        outcome.last_valid_sequence,
        Sequence(0),
        "the journal is not pristine, which is exactly what a reopen must tolerate"
    );
    assert!(
        outcome.writer_epoch.0 > 1,
        "reacquiring writer authority advances the run's epoch past the creator's; \
         see `run::resume` for which backend does this where"
    );
}

/// r2: a run that crashed between `prepare` and its terminal record reopens
/// poisoned. The intent implies a creation whose undo list died with the process.
#[test]
fn resuming_a_run_that_crashed_mid_transaction_requires_recovery() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let id = OperationId(uuid::Uuid::new_v4());
    let (mut journal, request) = open_journal(&control, run_id, 1);
    journal.open(&request).unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Prepare {
                intent: JournalIntent::Create {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                    directory: false,
                    mode: 0o644,
                },
            },
        ))
        .unwrap();
    // No terminal record: the session died here.
    journal.flush(Sequence(1)).unwrap();
    journal.close().unwrap();

    let outcome =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap();
    assert!(
        outcome.recovery_required,
        "a creating Prepare with no terminal record must poison the reopened session"
    );
    assert_eq!(outcome.unfinished, 1);
}

/// r2b: the durable declaration path. An uncorroborated abort journals
/// `RecoveryRequired` before its `Abort`, and the `Abort` empties the inventory --
/// so this run reopens with *nothing* pending and must still poison.
#[test]
fn resuming_a_run_a_previous_session_declared_unrecoverable_requires_recovery() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let id = OperationId(uuid::Uuid::new_v4());
    let (mut journal, request) = open_journal(&control, run_id, 1);
    journal.open(&request).unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Prepare {
                intent: JournalIntent::Create {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                    directory: false,
                    mode: 0o644,
                },
            },
        ))
        .unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired),
        ))
        .unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Abort {
                reason: "Cancelled".into(),
            },
        ))
        .unwrap();
    journal.flush(Sequence(3)).unwrap();
    journal.close().unwrap();

    let outcome =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap();
    assert_eq!(
        outcome.unfinished, 0,
        "the Abort is terminal, so the inventory cannot carry this verdict"
    );
    assert!(
        outcome.recovery_required,
        "the declaration is what survives the inventory being emptied"
    );
}

/// r3: `recover` is `resume`. Asserted on the same durable state rather than by
/// reading the delegation, so a future divergence shows up here.
#[test]
fn recover_reports_exactly_what_resume_reports() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let id = OperationId(uuid::Uuid::new_v4());
    let (mut journal, request) = open_journal(&control, run_id, 1);
    journal.open(&request).unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Prepare {
                intent: JournalIntent::Symlink {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/link".to_vec()).unwrap(),
                    target: BytePath::new(b"/target".to_vec()).unwrap(),
                },
            },
        ))
        .unwrap();
    journal.flush(Sequence(1)).unwrap();
    journal.close().unwrap();

    let by_recover = Supervisor::recover(spec(
        &storage_root,
        run_id,
        workspace_dir.clone(),
        bins.clone(),
    ))
    .unwrap();
    let by_resume =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap();

    assert!(by_recover.recovery_required);
    assert_eq!(by_recover.recovery_required, by_resume.recovery_required);
    assert_eq!(by_recover.unfinished, by_resume.unfinished);
    assert_eq!(
        by_recover.last_valid_sequence,
        by_resume.last_valid_sequence
    );
    // The one field that must differ: each reopen takes its own writer authority.
    assert!(
        by_resume.writer_epoch.0 > by_recover.writer_epoch.0,
        "a second reopen must not reuse the first's epoch"
    );
}

/// The reopen refuses what `bind` refuses and nothing more. A checkpoint is
/// checkpoint-based recovery's problem, which is not implemented -- and refusing
/// it is not the same as poisoning, because poisoning names a specific
/// transaction as unreconciled and this evidence does not support that.
#[test]
fn resuming_a_run_with_a_checkpoint_is_refused_rather_than_classified() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let (mut journal, request) = open_journal(&control, run_id, 1);
    let state = journal.open(&request).unwrap();
    assert!(state.checkpoint.is_none());
    journal
        .append(&record(
            1,
            OperationId(uuid::Uuid::new_v4()),
            JournalPayload::Lifecycle(JournalLifecycle::Started),
        ))
        .unwrap();
    journal.flush(Sequence(1)).unwrap();
    let checkpoint = umbra_core::Checkpoint {
        format_version: 1,
        id: umbra_core::CheckpointId(uuid::Uuid::new_v4()),
        run_id,
        writer_epoch: umbra_core::LeaseEpoch(1),
        last_committed: umbra_core::DurableSequence {
            run_id,
            writer_epoch: umbra_core::LeaseEpoch(1),
            sequence: Sequence(1),
        },
        fingerprints: umbra_core::JournalFingerprints {
            base: contract.identity.clone(),
            toolchain: "test".into(),
            agent_provider_id: "test".into(),
            agent_version: "test".into(),
            supervisor_version: "test".into(),
        },
        state: umbra_core::JournalLogicalState {
            format_version: 1,
            entries: vec![],
            whiteouts: vec![],
            agent_session_id: "test".into(),
            agent_state_paths: vec![],
            metadata: vec![],
        },
        clean: true,
    };
    journal.write_checkpoint(&checkpoint).unwrap();
    journal.close().unwrap();

    let refused =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap_err();
    assert_eq!(refused.kind, umbra_core::ErrorKind::UnsupportedCapability);
}

/// CR10: a registry `run` refuses must not be accepted by `resume`.
///
/// `run` rejects any registry carrying a `namespace` role, because this
/// supervisor always builds its own `standard_namespace` over the storage and
/// journal roles and would otherwise silently discard the configured provider.
/// The reopen path binds through `standard_namespace` too, so the same refusal
/// has to hold — and it holds here because both paths now call `admit_run`
/// rather than each keeping a list.
///
/// The capability is declared, deliberately: `admit_run` checks it first so an
/// unqualified descriptor is named as such, and this asserts the *second*
/// refusal, the one that says the role cannot be routed at all.
#[test]
fn resuming_with_a_configured_namespace_role_is_refused_as_run_refuses_it() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    create_run(&storage_root, run_id, &contract);

    let mut spec = spec(&storage_root, run_id, workspace_dir, bins.clone());
    let mut namespace = descriptor("alt", "namespace", bins.0.clone(), vec![]);
    namespace
        .capabilities
        .insert(umbra_core::capabilities::NAMESPACE_RUN_LIFECYCLE_V1.to_owned());
    spec.registry
        .providers
        .insert("namespace".to_owned(), namespace);

    let refused = Supervisor::resume(spec).unwrap_err();
    assert_eq!(refused.kind, umbra_core::ErrorKind::UnsupportedCapability);
    assert!(
        refused.context.contains("does not yet route a"),
        "the refusal must say the role cannot be routed, not merely that something \
         is unqualified: {}",
        refused.context
    );
}

/// CR11: reopening a healthy run twice leaves its journal exactly as it was.
///
/// Every link was checked against source before this was written, because a test
/// that pins the wrong invariant is worse than none:
///
/// - `Overlay::fail_run` with `tree_terminated: true` closes the journal,
///   releases the writer and closes storage. It appends no record — it is the
///   path that deliberately writes *no* completion evidence.
/// - `FileJournal::close` calls `sync_all` on the log handle. A flush, not an
///   append.
/// - `Overlay::bind` sets session state and nothing else; it never records.
///
/// So a reopen is observation-only as far as the log is concerned, and
/// `last_valid_sequence` must not move. What *does* move is the writer epoch,
/// because each reopen takes its own authority — asserted here too, so "nothing
/// changed" cannot be read more broadly than it is true.
#[test]
fn reopening_a_healthy_run_twice_leaves_the_journal_where_it_was() {
    let Some(bins) = providers() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (workspace_dir, contract) = workspace(dir.path());
    let storage_root = dir.path().join("storage");
    let run_id = RunId(uuid::Uuid::new_v4());
    let control = create_run(&storage_root, run_id, &contract);

    let id = OperationId(uuid::Uuid::new_v4());
    let (mut journal, request) = open_journal(&control, run_id, 1);
    journal.open(&request).unwrap();
    journal
        .append(&record(
            1,
            id,
            JournalPayload::Prepare {
                intent: JournalIntent::Unlink {
                    object: ObjectId(uuid::Uuid::new_v4()),
                    path: BytePath::new(b"/gone".to_vec()).unwrap(),
                    directory: false,
                },
            },
        ))
        .unwrap();
    journal
        .append(&record(1, id, JournalPayload::Commit))
        .unwrap();
    journal.flush(Sequence(2)).unwrap();
    journal.close().unwrap();

    let first = Supervisor::resume(spec(
        &storage_root,
        run_id,
        workspace_dir.clone(),
        bins.clone(),
    ))
    .unwrap();
    let second =
        Supervisor::resume(spec(&storage_root, run_id, workspace_dir, bins.clone())).unwrap();

    assert!(!first.recovery_required, "the run is healthy to begin with");
    assert!(
        !second.recovery_required,
        "and reopening it must not make it unhealthy"
    );
    assert_eq!(
        first.last_valid_sequence, second.last_valid_sequence,
        "a reopen observes the log; it must not extend it"
    );
    assert_eq!(first.unfinished, 0);
    assert_eq!(second.unfinished, 0);
    assert!(
        second.writer_epoch.0 > first.writer_epoch.0,
        "the one thing that does change: each reopen takes its own authority"
    );
}
