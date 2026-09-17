use super::*;
use std::{
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;
use umbra_storage_local::LocalStorage;
use uuid::Uuid;

#[derive(Default)]
struct Log {
    records: Vec<JournalRecord>,
    fail_prepare: bool,
    fail_commit: bool,
    fail_close: bool,
    closes: usize,
}
struct MemoryJournal {
    log: Arc<Mutex<Log>>,
    run: RunId,
    epoch: LeaseEpoch,
}
impl Journal for MemoryJournal {
    fn open(&mut self, _: &JournalOpenRequest) -> Result<RecoveryState> {
        Ok(recovery(self.run))
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        let mut log = self.log.lock().unwrap();
        if (log.fail_prepare && matches!(record.payload, JournalPayload::Prepare { .. }))
            || (log.fail_commit && matches!(record.payload, JournalPayload::Commit))
        {
            return Err(error(ErrorKind::Io, "injected journal failure"));
        }
        let sequence = Sequence(log.records.len() as u64 + 1);
        let mut record = record.clone();
        record.sequence = sequence;
        log.records.push(record);
        Ok(sequence)
    }
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        assert!(through.0 <= self.log.lock().unwrap().records.len() as u64);
        Ok(DurableSequence {
            run_id: self.run,
            writer_epoch: self.epoch,
            sequence: through,
        })
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let records: Vec<_> = self
            .log
            .lock()
            .unwrap()
            .records
            .iter()
            .filter(|r| r.sequence.0 > after.0)
            .cloned()
            .map(Ok)
            .collect();
        Ok(Box::new(records.into_iter()))
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        Ok(checkpoint.id)
    }
    fn close(&mut self) -> Result<()> {
        let mut log = self.log.lock().unwrap();
        log.closes += 1;
        if log.fail_close {
            return Err(error(ErrorKind::Io, "injected close failure"));
        }
        Ok(())
    }
}
fn recovery(run_id: RunId) -> RecoveryState {
    RecoveryState {
        run_id,
        checkpoint: None,
        last_valid_sequence: Sequence(0),
        durable: None,
        pending: vec![],
        tail: JournalTailRecovery::Intact,
        clean: true,
    }
}
fn bytes(value: &[u8]) -> BytePath {
    BytePath::new(value.to_vec()).unwrap()
}
fn context(run_id: RunId, epoch: Option<LeaseEpoch>) -> RequestContext {
    RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: epoch,
    }
}
fn open_storage(dir: &TempDir) -> (LocalStorage, RunBinding, WriterLease) {
    let mut storage = LocalStorage::new(dir.path()).unwrap();
    let run_id = RunId(Uuid::new_v4());
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "test-base".into(),
                fingerprint: vec![1],
            },
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
            writer_id: WriterId("test".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .unwrap();
    (storage, binding, lease)
}
fn native(path: &BytePath) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes()))
}
struct Fixture {
    overlay: Overlay,
    process: ProcessContext,
    base_root: PathBuf,
    shadow_root: PathBuf,
    control: PathBuf,
    log: Arc<Mutex<Log>>,
    _dirs: [TempDir; 2],
}

struct BadReceipt {
    inner: LocalStorage,
    mismatch: u8,
}
impl Storage for BadReceipt {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        self.inner.open_run(request)
    }
    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        self.inner.acquire_writer(request)
    }
    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        self.inner.renew_writer(lease)
    }
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        self.inner.release_writer(lease)
    }
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        self.inner.execute(request)
    }
    fn close_run(&mut self) -> Result<()> {
        self.inner.close_run()
    }
    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        let mut receipt = self.inner.flush(request)?;
        match self.mismatch {
            0 => receipt.run_id = RunId(Uuid::new_v4()),
            1 => receipt.writer_epoch = LeaseEpoch(receipt.writer_epoch.0 + 1),
            2 => receipt.durability = Durability::None,
            _ => unreachable!(),
        }
        Ok(receipt)
    }
}

/// Every `Create` the engine asked the shadow storage for, in order.
type Creates = Arc<Mutex<Vec<(StoragePath, CreateOptions)>>>;
/// Every path the engine asked the base to stat, in order.
type BaseStats = Arc<Mutex<Vec<StoragePath>>>;
/// Every `Unlink` the engine asked the shadow storage for, with the request
/// context it carried, in order.
type Unlinks = Arc<Mutex<Vec<(StoragePath, RequestContext)>>>;
/// Switch that makes every base *directory* stat fail with this kind.
type BaseFailure = Arc<Mutex<Option<ErrorKind>>>;
/// Every `SetMetadata` the engine asked the shadow storage for, in order.
///
/// The only umask-independent, filesystem-independent view of the ownership
/// carry: on a CI runner the base tree is already owned by the test process, so
/// a successful carry leaves the shadow object's uid exactly where a *missing*
/// carry would have left it. Asserting that no `SetMetadata` was emitted, or
/// that one naming the base's uid/gid was, is what distinguishes them.
type Metadata = Arc<Mutex<Vec<(StoragePath, MetadataUpdate)>>>;
/// Switch that makes every shadow `SetMetadata` fail with this kind.
///
/// How the privilege edge case is driven without root: a real `EPERM` needs a
/// base object owned by someone else, which CI cannot create. `ErrorKind::Denied`
/// is what `umbra-storage-local` maps that `EPERM` to, so injecting the kind
/// exercises the engine's fallback on exactly the value the kernel would have
/// produced.
type MetadataFailure = Arc<Mutex<Option<ErrorKind>>>;
/// Switch that makes every shadow `RemoveDirectory` fail with this kind.
///
/// The directory analogue of the mode-based `unlink(2)` injection
/// `a_rollback_that_cannot_unlink_poisons_instead_of_claiming_success` uses. A
/// mode cannot express it: the rollback's `rmdir` and the `create` that
/// materialised the directory need the same write bit on the same parent, so
/// revoking it would fail `prepare` instead of the undo. Modelled on
/// `WatchedBase::fail_directory_stat` -- a switch the test flips *between*
/// `prepare` and `abort`, so everything before the undo runs against a healthy
/// backend.
type RemoveDirectoryFailure = Arc<Mutex<Option<ErrorKind>>>;
/// Switch that makes every shadow `Unlink` under this anchor whose path starts
/// with these bytes fail with this kind.
///
/// The `Unlink` analogue of `RemoveDirectoryFailure`, and flipped the same way --
/// between `prepare` and `abort`, so the materialisation runs against a healthy
/// backend and only the undo is refused. A mode cannot express it for the Control
/// blobs at all, and for the placeholder the mode-based injection
/// `a_rollback_that_cannot_unlink_poisons_instead_of_claiming_success` uses would
/// refuse all three.
///
/// Selective rather than blanket because the symlink undo issues *three* unlinks
/// and each sibling has to refuse exactly one of them. By prefix rather than by
/// exact path because the backing index's own name is the backend object ID the
/// test never sees -- `symlinks/objects/` is the only handle a test has on it.
type UnlinkFailure = Arc<Mutex<Option<(ErrorKind, StorageAnchor, Vec<u8>)>>>;

/// Records the `CreateOptions` the engine *requests*, which is the only
/// umask-independent view of the mode `parents` chose. `LocalStorage` materialises
/// a directory with `DirBuilder::mode`, and `mkdir(2)` masks that with the process
/// umask, so a requested `0o777` is never observable on disk under the usual
/// `umask 022` -- it lands as exactly the `0o755` the #56 defect produced. Wraps
/// `LocalStorage` and delegates, like `BadReceipt`.
struct Recorder {
    inner: LocalStorage,
    creates: Creates,
    unlinks: Unlinks,
    metadata: Metadata,
    fail_remove_directory: RemoveDirectoryFailure,
    fail_unlink: UnlinkFailure,
    fail_metadata: MetadataFailure,
    /// Report `LocalStorage`'s capabilities with `STORAGE_OWNERSHIP_FIDELITY_V1`
    /// removed, modelling a backend that has not qualified ownership carry.
    ///
    /// Removing the name from a real backend's real set, rather than returning a
    /// hand-written `StorageCapabilities`, is deliberate: the degraded mode has
    /// to be the *same* backend minus one advertisement, or the test proves
    /// nothing about the gate.
    drop_ownership_fidelity: bool,
    /// Override the shadow backend's `storage-parent-identity-v1` advertisement
    /// independent of platform: `Some(true)` adds the name, `Some(false)`
    /// removes it, `None` leaves the backend's own answer.
    ///
    /// `LocalStorage` advertises the name on macOS and not on Linux, so an
    /// overlay test that let the platform decide would exercise a different code
    /// path on each. Forcing it makes the Case-A widening and the
    /// non-advertising fallback both testable on either host.
    force_parent_identity: Option<bool>,
    /// Report this uid/gid for these *shadow* paths on `Stat`, the shadow-side
    /// mirror of [`WatchedBase::force_owners`].
    ///
    /// `identity_at`'s Case A reads a *materialised shadow parent's* live
    /// identity, and the property under test is that the sentinel widens against
    /// that observation. Constructing a real shadow object owned by another
    /// user, or bearing a gid from a real setgid parent, needs a supplementary
    /// group and privilege CI does not portably have; reporting the identity is
    /// enough for everything decided at `resolve`, which is where the predicate
    /// lives. The empty path (`b""`) is the shadow root, so a test can pin the
    /// Case-B fallback's own answer too. Intercepts `Stat` after delegating, so
    /// only the reported uid/gid change and nothing else does.
    force_shadow_owners: Vec<(Vec<u8>, (u32, u32))>,
}
impl Storage for Recorder {
    fn capabilities(&self) -> StorageCapabilities {
        let mut capabilities = self.inner.capabilities();
        if self.drop_ownership_fidelity {
            capabilities
                .features
                .remove(capabilities::STORAGE_OWNERSHIP_FIDELITY_V1);
        }
        match self.force_parent_identity {
            Some(true) => {
                capabilities
                    .features
                    .insert(capabilities::STORAGE_PARENT_IDENTITY_V1.to_owned());
            }
            Some(false) => {
                capabilities
                    .features
                    .remove(capabilities::STORAGE_PARENT_IDENTITY_V1);
            }
            None => {}
        }
        capabilities
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        self.inner.open_run(request)
    }
    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        self.inner.acquire_writer(request)
    }
    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        self.inner.renew_writer(lease)
    }
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        self.inner.release_writer(lease)
    }
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        match &request.operation {
            StorageOperation::Create { path, options } => self
                .creates
                .lock()
                .unwrap()
                .push((path.clone(), options.clone())),
            StorageOperation::Unlink { path } => {
                self.unlinks
                    .lock()
                    .unwrap()
                    .push((path.clone(), request.context.clone()));
                // Recorded first and refused before delegating, so the sibling
                // that injected this sees both the attempt and the object the
                // engine could not remove, still standing.
                if let Some((kind, anchor, prefix)) = self.fail_unlink.lock().unwrap().as_ref() {
                    if path.anchor() == *anchor && path.as_bytes().starts_with(prefix) {
                        return Err(UmbraError::new(
                            *kind,
                            "test-storage",
                            "injected unlink failure",
                        ));
                    }
                }
            }
            StorageOperation::SetMetadata { path, update } => {
                self.metadata
                    .lock()
                    .unwrap()
                    .push((path.clone(), update.clone()));
                // Recorded first and refused before delegating, so a test can
                // assert both that the carry was attempted and that the object
                // kept the ownership the create gave it.
                if let Some(kind) = *self.fail_metadata.lock().unwrap() {
                    return Err(UmbraError::new(
                        kind,
                        "test-storage",
                        "injected set_metadata failure",
                    ));
                }
            }
            // Refused before delegating, so the directory the engine could not
            // remove is still standing when the test looks for it.
            StorageOperation::RemoveDirectory { .. } => {
                if let Some(kind) = *self.fail_remove_directory.lock().unwrap() {
                    return Err(UmbraError::new(
                        kind,
                        "test-storage",
                        "injected remove_directory failure",
                    ));
                }
            }
            _ => {}
        }
        let response = self.inner.execute(request)?;
        // Override the reported owner of a forced shadow path, after delegating,
        // so `identity_at`'s Case-A read of a materialised shadow parent sees the
        // identity the test named. Only uid/gid change; kind, mode and everything
        // else are the backend's own answer. When `force_shadow_owners` is empty
        // this is a no-op and the response passes straight through.
        if let StorageOperation::Stat { path } = &request.operation {
            if let StorageResponse::Stat(stat) = &response {
                if let Some((_, (uid, gid))) = self.force_shadow_owners.iter().find(|(name, _)| {
                    path.anchor() == StorageAnchor::Root && name.as_slice() == path.as_bytes()
                }) {
                    let mut stat = stat.clone();
                    stat.uid = *uid;
                    stat.gid = *gid;
                    return Ok(StorageResponse::Stat(stat));
                }
            }
        }
        Ok(response)
    }
    fn close_run(&mut self) -> Result<()> {
        self.inner.close_run()
    }
    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        self.inner.flush(request)
    }
}

/// Records every path the engine asks the base about, so a test can assert on
/// which paths reached the base at all -- the anchor guard in
/// `shadow_parent_mode` is a claim about exactly that. `force_directory_mode`
/// additionally reports an unmasked `st_mode` for directories, the way a `Base`
/// that does not mask its own stat would; `LocalStorage` masks at
/// `lib.rs:110`, so there is no other way to exercise the engine's own mask.
struct WatchedBase {
    inner: StorageBase,
    stats: BaseStats,
    force_directory_mode: Option<u32>,
    fail_directory_stat: BaseFailure,
    /// Report this uid/gid for every base object.
    ///
    /// A base owned by another user is the ordinary case for an overlay and the
    /// one CI cannot construct: creating a root-owned file needs root. Reporting
    /// the ownership is enough for everything decided *before* the chown --
    /// which is where the sentinel refusal lives -- and the chown itself is
    /// driven separately by `MetadataFailure`.
    force_owner: Option<(u32, u32)>,
    /// Report this uid/gid for these base paths only, overriding `force_owner`.
    ///
    /// The blanket switch above cannot express the property Axis 5 actually
    /// delivers: ownership carry is decided **per object**, so demonstrating it
    /// needs two targets with *opposite* answers reached through one engine,
    /// one backend and one base. A whole-fixture owner gives every object the
    /// same answer, which two fixtures can only put side by side, never in one
    /// run.
    force_owners: Vec<(Vec<u8>, (u32, u32))>,
}
impl Base for WatchedBase {
    fn stat(&mut self, path: &StoragePath) -> Result<BlobStat> {
        self.stats.lock().unwrap().push(path.clone());
        let mut stat = self.inner.stat(path)?;
        // Path-specific first, so a fixture can carve one object out of a
        // blanket owner, or name owners for some objects and leave the rest as
        // the real filesystem reports them.
        if let Some((_, (uid, gid))) = self
            .force_owners
            .iter()
            .find(|(name, _)| name.as_slice() == path.as_bytes())
        {
            stat.uid = *uid;
            stat.gid = *gid;
        } else if let Some((uid, gid)) = self.force_owner {
            stat.uid = uid;
            stat.gid = gid;
        }
        if stat.kind == ObjectKind::Directory {
            // Switchable so a test can let `resolve` run against a healthy base
            // and fail only the stat `shadow_parent_mode` makes during `prepare`.
            if let Some(kind) = *self.fail_directory_stat.lock().unwrap() {
                return Err(UmbraError::new(kind, "test-base", "injected base failure"));
            }
            if let Some(mode) = self.force_directory_mode {
                stat.mode = mode;
            }
        }
        Ok(stat)
    }
    fn read_link(&mut self, path: &StoragePath) -> Result<BytePath> {
        self.inner.read_link(path)
    }
    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize> {
        self.inner.read_at(path, offset, out)
    }
    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        self.inner.list(path, cursor, limit)
    }
    fn physical_path(&self, path: &StoragePath) -> Result<PhysicalPath> {
        self.inner.physical_path(path)
    }
}

/// Optional fixture wiring. Defaults reproduce `Fixture::new` exactly.
#[derive(Default)]
struct Setup<'a> {
    /// Durability-receipt corruption variant for `BadReceipt`.
    mismatch: Option<u8>,
    /// `chmod` applied to base paths once they are populated and before the base
    /// is frozen. `create_dir_all` goes through `mkdir(2)` and so cannot express
    /// a mode the umask would strip; `set_permissions` is `chmod(2)` and can.
    base_modes: &'a [(&'a [u8], u32)],
    /// Capture requested `CreateOptions`.
    creates: Option<Creates>,
    /// Capture `Unlink` requests and the context they carried.
    unlinks: Option<Unlinks>,
    /// Capture the paths the base is asked about.
    base_stats: Option<BaseStats>,
    /// Report this unmasked `st_mode` for every base directory.
    force_directory_mode: Option<u32>,
    /// Handle the test flips to make base directory stats fail.
    fail_directory_stat: Option<BaseFailure>,
    /// Handle the test flips to make shadow directory removals fail.
    fail_remove_directory: Option<RemoveDirectoryFailure>,
    /// Handle the test flips to make selected shadow unlinks fail.
    fail_unlink: Option<UnlinkFailure>,
    /// Capture the `SetMetadata` requests the engine issues.
    metadata: Option<Metadata>,
    /// Handle the test flips to make shadow `SetMetadata` fail.
    fail_metadata: Option<MetadataFailure>,
    /// Advertise the shadow backend's capabilities without ownership fidelity.
    drop_ownership_fidelity: bool,
    /// Report this uid/gid for every base object.
    force_owner: Option<(u32, u32)>,
    /// Report this uid/gid for these base paths only, overriding `force_owner`.
    force_owners: &'a [(&'a [u8], (u32, u32))],
    /// Override the shadow backend's parent-identity advertisement (see
    /// `Recorder::force_parent_identity`).
    force_parent_identity: Option<bool>,
    /// Report this uid/gid for these *shadow* paths (see
    /// `Recorder::force_shadow_owners`); `b""` is the shadow root.
    force_shadow_owners: &'a [(&'a [u8], (u32, u32))],
}

impl Fixture {
    fn new(files: &[(&[u8], &[u8])]) -> Self {
        Self::build(files, Setup::default())
    }
    fn with_bad_receipt(files: &[(&[u8], &[u8])], mismatch: Option<u8>) -> Self {
        Self::build(
            files,
            Setup {
                mismatch,
                ..Setup::default()
            },
        )
    }
    fn with_base_modes(files: &[(&[u8], &[u8])], base_modes: &[(&[u8], u32)]) -> Self {
        Self::build(
            files,
            Setup {
                base_modes,
                ..Setup::default()
            },
        )
    }
    fn build(files: &[(&[u8], &[u8])], setup: Setup) -> Self {
        let mismatch = setup.mismatch;
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        let (mut base, base_binding, base_lease) = open_storage(&base_dir);
        let base_root = native(base_binding.root.physical_path.as_ref().unwrap());
        for (name, content) in files {
            let file = base_root.join(std::ffi::OsStr::from_bytes(name));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        // After the tree exists and before the base is frozen. `chmod(2)` is not
        // umask-filtered, so this is the only way to give a base directory a mode
        // `create_dir_all` could not have produced.
        for (name, mode) in setup.base_modes {
            let path = base_root.join(std::ffi::OsStr::from_bytes(name));
            fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
            assert_eq!(
                mode_of(&path),
                *mode,
                "base fixture mode was not applied verbatim"
            );
        }
        base.release_writer(&base_lease).unwrap();
        let base = StorageBase::new(
            Box::new(base),
            base_binding.clone(),
            context(base_binding.run_id, None),
        )
        .unwrap();
        let (shadow, binding, lease) = open_storage(&shadow_dir);
        let shadow_root = native(binding.root.physical_path.as_ref().unwrap());
        let control = native(binding.control.physical_path.as_ref().unwrap());
        let log = Arc::new(Mutex::new(Log::default()));
        let journal = MemoryJournal {
            log: log.clone(),
            run: binding.run_id,
            epoch: lease.epoch,
        };
        let config = SessionConfig {
            context: context(binding.run_id, Some(lease.epoch)),
            recovery: recovery(binding.run_id),
            binding,
            lease,
        };
        let storage: Box<dyn Storage> = match (
            mismatch,
            setup
                .creates
                .clone()
                .or_else(|| setup.unlinks.clone().map(|_| Creates::default()))
                .or_else(|| {
                    setup
                        .fail_remove_directory
                        .clone()
                        .map(|_| Creates::default())
                })
                .or_else(|| setup.fail_unlink.clone().map(|_| Creates::default()))
                .or_else(|| setup.metadata.clone().map(|_| Creates::default()))
                .or_else(|| setup.fail_metadata.clone().map(|_| Creates::default()))
                .or_else(|| setup.drop_ownership_fidelity.then(Creates::default))
                .or_else(|| setup.force_parent_identity.map(|_| Creates::default()))
                .or_else(|| (!setup.force_shadow_owners.is_empty()).then(Creates::default)),
        ) {
            (Some(mismatch), creates) => {
                // `BadReceipt` does not record, so a caller asking for both would
                // get a silently empty recorder and a vacuously passing
                // assertion. No test needs either combination; say so loudly
                // rather than let a future one pass for the wrong reason. The
                // binding covers `unlinks`, `fail_remove_directory` and
                // `fail_unlink` as well, because a recorder is built for any of
                // the four.
                debug_assert!(
                    creates.is_none(),
                    "Setup::creates, Setup::unlinks, Setup::fail_remove_directory and Setup::fail_unlink are ignored when a bad receipt is requested"
                );
                Box::new(BadReceipt {
                    inner: shadow,
                    mismatch,
                })
            }
            (None, Some(creates)) => Box::new(Recorder {
                inner: shadow,
                creates,
                unlinks: setup.unlinks.unwrap_or_default(),
                metadata: setup.metadata.unwrap_or_default(),
                fail_remove_directory: setup.fail_remove_directory.unwrap_or_default(),
                fail_unlink: setup.fail_unlink.unwrap_or_default(),
                fail_metadata: setup.fail_metadata.unwrap_or_default(),
                drop_ownership_fidelity: setup.drop_ownership_fidelity,
                force_parent_identity: setup.force_parent_identity,
                force_shadow_owners: setup
                    .force_shadow_owners
                    .iter()
                    .map(|(name, owner)| (name.to_vec(), *owner))
                    .collect(),
            }),
            (None, None) => Box::new(shadow),
        };
        let base: Box<dyn Base> = match (
            setup.base_stats,
            setup.force_directory_mode,
            setup.fail_directory_stat,
            setup.force_owner,
            setup.force_owners.is_empty(),
        ) {
            (None, None, None, None, true) => Box::new(base),
            (stats, force_directory_mode, fail_directory_stat, force_owner, _) => {
                Box::new(WatchedBase {
                    inner: base,
                    stats: stats.unwrap_or_default(),
                    force_directory_mode,
                    fail_directory_stat: fail_directory_stat.unwrap_or_default(),
                    force_owner,
                    force_owners: setup
                        .force_owners
                        .iter()
                        .map(|(name, owner)| (name.to_vec(), *owner))
                        .collect(),
                })
            }
        };
        let mut overlay = Overlay::new(storage, Box::new(journal));
        overlay.bind(config, base).unwrap();
        let process = ProcessContext {
            task: TaskId(TaskIdentity {
                native_id: 1,
                generation: 1,
            }),
            parent: None,
            architecture: Architecture::Aarch64,
            abi: "test".into(),
            exec_generation: 0,
            root: bytes(b"/"),
            cwd: bytes(b"/"),
            fds: BTreeMap::new(),
        };
        Self {
            overlay,
            process,
            base_root,
            shadow_root,
            control,
            log,
            _dirs: [base_dir, shadow_dir],
        }
    }
    /// Record the kernel refusal of a transaction whose action is an
    /// `Emulate`, bypassing `observe_result`.
    ///
    /// `resolve` answers `Emulate(Success)` for `Symlink` and `Mkdir` (the
    /// `Symlink | Mkdir | Unlink` arm), and `observe_result` rejects any outcome
    /// that differs from the one an emulation reported. So a *refused* one of
    /// those cannot be driven through the public sequence at all -- which is the
    /// honest form of the reachability caveat the tests using this carry: no
    /// supervisor produces this sequence today, and the property under test is
    /// the engine's own abort rule at those branches.
    ///
    /// Writing the outcome directly is what keeps those tests discriminating. An
    /// abort with no corroborated outcome poisons regardless of what `rollback`
    /// holds, so without this the reconciling cases could not be driven at all and
    /// the poisoning ones -- the three
    /// `a_symlink_rollback_that_cannot_unlink_…_poisons` siblings, whose subject
    /// is a removal that *failed* -- would pass for the wrong reason.
    ///
    /// It is **not** a drop-in for `observe_result`: only the outcome field is
    /// written. The real call also appends a
    /// `JournalPayload::ObservedResult` record for a mutating plan, so a test
    /// that used this and then asserted on journal *shape* would see one record
    /// fewer than production and would be asserting a sequence the engine never
    /// emits. Nothing does that today. Most callers assert on `poisoned`, error
    /// kinds and the shadow tree;
    /// `an_uncorroborated_unlink_abort_poisons_and_still_destroys_nothing` also
    /// reads `f.log`, but for record *content* only -- that an `Abort` naming
    /// `KernelRefused` is present, and that no `Commit` is -- and a missing
    /// `ObservedResult` can falsify neither. Extend this helper to `record(..)`
    /// before adding the first assertion on journal shape rather than
    /// discovering the gap from a confusing diff.
    fn observe_emulated_refusal(&mut self, errno: Errno) {
        self.overlay.pending.as_mut().unwrap().outcome = Some(OperationOutcome::Failure(errno));
    }
    fn prepare(&mut self, op: &FsOp) -> PreparedAction {
        let action = self.overlay.resolve(&self.process, op).unwrap();
        self.overlay
            .prepare(OperationId(Uuid::new_v4()), &action)
            .unwrap()
    }
    fn complete(&mut self, prepared: PreparedAction) {
        self.overlay
            .observe_result(
                prepared.operation_id,
                &OperationOutcome::Success { return_value: 0 },
            )
            .unwrap();
        self.overlay.commit(prepared.operation_id).unwrap();
    }
    fn run(&mut self, op: &FsOp) {
        let prepared = self.prepare(op);
        if let ResolvedAction::Rewrite(rewrite) = &prepared.action {
            match &rewrite.operation {
                FsOp::Open { flags, .. } => {
                    let path = native(&rewrite.paths[0].path.0);
                    fs::OpenOptions::new()
                        .read(flags.read)
                        .write(flags.write)
                        .append(flags.append)
                        .truncate(flags.truncate)
                        .create(flags.create)
                        .create_new(flags.exclusive)
                        .open(path)
                        .unwrap();
                }
                FsOp::Rename { .. } => {
                    fs::rename(
                        native(&rewrite.paths[0].path.0),
                        native(&rewrite.paths[1].path.0),
                    )
                    .unwrap();
                }
                FsOp::Stat { .. } => {
                    fs::metadata(native(&rewrite.paths[0].path.0)).unwrap();
                }
                _ => panic!("unexpected rewrite"),
            }
        }
        self.complete(prepared);
    }
    fn read(&mut self, name: &[u8]) -> Result<Vec<u8>> {
        let mut buffer = vec![0; 128];
        let count = self.overlay.read_at(&root(name)?, 0, &mut buffer)?;
        buffer.truncate(count);
        Ok(buffer)
    }
}
fn open(path: &[u8], flags: OpenFlags) -> FsOp {
    FsOp::Open {
        dir: DirRef::Cwd,
        path: bytes(path),
        flags,
        mode: 0o640,
    }
}
fn stat(path: &[u8]) -> FsOp {
    FsOp::Stat {
        dir: DirRef::Cwd,
        path: bytes(path),
        follow: true,
    }
}
fn unlink(path: &[u8]) -> FsOp {
    FsOp::Unlink {
        dir: DirRef::Cwd,
        path: bytes(path),
        directory: false,
    }
}
fn rmdir(path: &[u8]) -> FsOp {
    FsOp::Unlink {
        dir: DirRef::Cwd,
        path: bytes(path),
        directory: true,
    }
}
fn rename(from: &[u8], to: &[u8]) -> FsOp {
    FsOp::Rename {
        from_dir: DirRef::Cwd,
        from: bytes(from),
        to_dir: DirRef::Cwd,
        to: bytes(to),
    }
}

#[test]
fn read_and_stat_prefer_shadow_and_copy_up_never_changes_base() {
    let mut f = Fixture::new(&[(b"nested/file", b"base bytes")]);
    assert_eq!(f.read(b"nested/file").unwrap(), b"base bytes");
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &stat(b"nested/file"))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        native(&plan.paths[0].path.0),
        f.base_root.join("nested/file")
    );
    f.run(&open(
        b"nested/file",
        OpenFlags {
            read: true,
            write: true,
            ..Default::default()
        },
    ));
    assert_eq!(
        fs::read(f.shadow_root.join("nested/file")).unwrap(),
        b"base bytes"
    );
    fs::write(f.shadow_root.join("nested/file"), b"shadow").unwrap();
    assert_eq!(f.read(b"nested/file").unwrap(), b"shadow");
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &stat(b"nested/file"))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        native(&plan.paths[0].path.0),
        f.shadow_root.join("nested/file")
    );
    f.run(&open(
        b"nested/file",
        OpenFlags {
            write: true,
            truncate: true,
            ..Default::default()
        },
    ));
    assert!(f.read(b"nested/file").unwrap().is_empty());
    assert_eq!(
        fs::read(f.base_root.join("nested/file")).unwrap(),
        b"base bytes"
    );
}

#[test]
fn create_parents_exclusive_and_no_mutation_during_resolve() {
    let mut f = Fixture::new(&[]);
    let op = open(
        b"new/deep/file",
        OpenFlags {
            write: true,
            create: true,
            exclusive: true,
            ..Default::default()
        },
    );
    f.overlay.resolve(&f.process, &op).unwrap();
    assert!(!f.shadow_root.join("new").exists());
    assert!(f.log.lock().unwrap().records.is_empty());
    f.run(&op);
    assert!(f.shadow_root.join("new/deep/file").is_file());
    assert_eq!(
        f.overlay.resolve(&f.process, &op).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        f.overlay
            .resolve(
                &f.process,
                &open(
                    b"missing",
                    OpenFlags {
                        write: true,
                        ..Default::default()
                    }
                )
            )
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

#[test]
fn base_and_shadow_unlinks_are_journaled_and_recreation_clears_whiteout() {
    let mut f = Fixture::new(&[(b"a", b"base"), (b"b", b"also base")]);
    fs::write(f.shadow_root.join("b"), b"shadow").unwrap();
    for name in [b"a", b"b"] {
        f.run(&unlink(name));
        assert_eq!(f.read(name).unwrap_err().kind, ErrorKind::NotFound);
        // The base object is still on the host, so a non-mutating resolve of the
        // whiteouted path must deny with ENOENT rather than raise a bare NotFound
        // the supervisor would resume into a host-visible read (#49).
        assert_eq!(
            f.overlay.resolve(&f.process, &stat(name)).unwrap(),
            ResolvedAction::Deny(Errno::ENOENT)
        );
        assert!(!f
            .shadow_root
            .join(std::ffi::OsStr::from_bytes(name))
            .exists());
        let marker = Overlay::marker(&root(name).unwrap()).unwrap();
        assert!(f
            .control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
            .is_file());
    }
    let records = f.log.lock().unwrap().records.clone();
    assert_eq!(records.len(), 6);
    assert!(
        matches!(&records[0].payload, JournalPayload::Prepare { intent: JournalIntent::Unlink { path, .. } } if path.as_bytes() == b"/a")
    );
    assert!(matches!(records[2].payload, JournalPayload::Commit));
    f.run(&open(
        b"a",
        OpenFlags {
            write: true,
            create: true,
            exclusive: true,
            ..Default::default()
        },
    ));
    assert!(f.read(b"a").unwrap().is_empty());
    assert!(!f.overlay.whiteouted(&root(b"a").unwrap()).unwrap());
    assert_eq!(fs::read(f.base_root.join("a")).unwrap(), b"base");
}

#[test]
fn whiteout_hidden_resolves_to_enoent_while_truly_absent_stays_not_found() {
    let mut f = Fixture::new(&[(b"gone", b"base"), (b"dir/child", b"base")]);
    // A whiteouted final component: the base object is still on the host, so a
    // non-mutating resolve must deny with ENOENT rather than raise a bare
    // NotFound the supervisor would resume into a host-visible read (#49).
    f.run(&unlink(b"gone"));
    assert_eq!(
        f.overlay.resolve(&f.process, &stat(b"gone")).unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
    // A name never in base or shadow is truly absent. The base *is* the host, so
    // its NotFound equals what the tracee's own syscall would see; it is left to
    // resume as passthrough, unchanged. This is the discriminator: the same op
    // shape yields Deny for a whiteout and NotFound for a true absence.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &stat(b"never"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    // A whiteouted *ancestor directory* hides everything beneath it. Path
    // resolution fails at the ancestor's own lookup, before the final-component
    // lookup runs, so a fix confined to the final lookup would leak this. Write
    // the directory marker the way the engine stores one (`rmdir` writes one
    // since #77; hand-writing it keeps this independent of the `FsOp::Unlink`
    // arm, and of that arm's own emptiness gate), then confirm the child
    // beneath it also denies with ENOENT.
    let marker = Overlay::marker(&root(b"dir").unwrap()).unwrap();
    let marker_path = f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()));
    fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    fs::write(&marker_path, b"").unwrap();
    assert!(f.overlay.whiteouted(&root(b"dir").unwrap()).unwrap());
    assert_eq!(
        f.overlay.resolve(&f.process, &stat(b"dir/child")).unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
}

#[test]
fn merged_pages_sort_deduplicate_hide_whiteouts_and_keep_snapshot_continuation() {
    let mut f = Fixture::new(&[(b"dir/a", b"a"), (b"dir/c", b"base c"), (b"dir/z", b"z")]);
    fs::create_dir(f.shadow_root.join("dir")).unwrap();
    fs::write(f.shadow_root.join("dir/c"), b"shadow c").unwrap();
    fs::write(f.shadow_root.join("dir/b"), b"b").unwrap();
    f.run(&unlink(b"dir/z"));
    let first = f.overlay.list(&root(b"dir").unwrap(), None, 1).unwrap();
    assert_eq!(first.entries[0].name.as_bytes(), b"a");
    let cursor = first.next.unwrap();
    f.run(&unlink(b"dir/b"));
    let next = f
        .overlay
        .list(&root(b"dir").unwrap(), Some(&cursor), 10)
        .unwrap();
    assert_eq!(
        next.entries
            .iter()
            .map(|e| e.name.as_bytes())
            .collect::<Vec<_>>(),
        vec![b"b", b"c"]
    );
    assert_eq!(next.entries[1].stat.len, 8);
    assert_eq!(
        f.overlay
            .list(&root(b"dir").unwrap(), Some(&cursor), 10)
            .unwrap(),
        next
    );
    assert_eq!(
        f.overlay
            .list(&root(b"").unwrap(), Some(&cursor), 1)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
    let fresh = f.overlay.list(&root(b"dir").unwrap(), None, 10).unwrap();
    assert_eq!(
        fresh
            .entries
            .iter()
            .map(|e| e.name.as_bytes())
            .collect::<Vec<_>>(),
        vec![b"a", b"c"]
    );
}

#[test]
fn rename_updates_source_and_destination_whiteouts_in_one_transaction() {
    let mut f = Fixture::new(&[(b"source", b"source bytes"), (b"dest", b"old destination")]);
    f.run(&unlink(b"dest"));
    let prepared = f.prepare(&rename(b"source", b"dest"));
    assert_eq!(f.read(b"source").unwrap_err().kind, ErrorKind::InvalidState);
    let ResolvedAction::Rewrite(plan) = &prepared.action else {
        panic!()
    };
    fs::rename(native(&plan.paths[0].path.0), native(&plan.paths[1].path.0)).unwrap();
    f.complete(prepared);
    assert_eq!(f.read(b"source").unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(f.read(b"dest").unwrap(), b"source bytes");
    assert!(!f.overlay.whiteouted(&root(b"dest").unwrap()).unwrap());
    assert_eq!(
        fs::read(f.base_root.join("source")).unwrap(),
        b"source bytes"
    );
    assert_eq!(
        fs::read(f.base_root.join("dest")).unwrap(),
        b"old destination"
    );
    let log = f.log.lock().unwrap();
    assert!(
        matches!(&log.records[3].payload, JournalPayload::Prepare { intent: JournalIntent::Rename { from, to, .. } } if from.as_bytes() == b"/source" && to.as_bytes() == b"/dest")
    );
    assert_eq!(log.records[3].operation_id, log.records[5].operation_id);
    drop(log);
    f.run(&rename(b"dest", b"dest"));
    assert_eq!(f.read(b"dest").unwrap(), b"source bytes");
}

#[test]
fn resolver_anchors_cwd_dirfd_and_refuses_escape_and_stale_descriptors() {
    let mut f = Fixture::new(&[(b"jail/dir/file", b"data")]);
    f.process.root = bytes(b"/jail");
    f.process.cwd = bytes(b"/jail/dir");
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Cwd, &bytes(b"../dir/./file"), false)
            .unwrap()
            .as_bytes(),
        b"jail/dir/file"
    );
    assert_eq!(
        f.overlay
            .resolve_path(
                &f.process,
                DirRef::Fd(TracedFd(99)),
                &bytes(b"/dir/file"),
                false
            )
            .unwrap()
            .as_bytes(),
        b"jail/dir/file"
    );
    for path in [b"../../outside".as_slice(), b"/../outside"] {
        assert_eq!(
            f.overlay
                .resolve_path(&f.process, DirRef::Cwd, &bytes(path), false)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidPath
        );
    }
    let object = f
        .overlay
        .lookup(&root(b"jail/dir").unwrap())
        .unwrap()
        .0
        .object_id;
    f.process.fds.insert(
        TracedFd(3),
        FdState {
            object,
            logical_path: Some(bytes(b"/jail/dir")),
            directory: true,
            flags: OpenFlags::default(),
        },
    );
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Fd(TracedFd(3)), &bytes(b"file"), false)
            .unwrap()
            .as_bytes(),
        b"jail/dir/file"
    );
    f.process.fds.get_mut(&TracedFd(3)).unwrap().object = ObjectId(Uuid::new_v4());
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Fd(TracedFd(3)), &bytes(b"file"), false)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
}

#[test]
fn non_utf8_paths_preserve_raw_bytes_in_resolution_and_journal() {
    let mut f = Fixture::new(&[]);
    let raw = b"raw-\xff";
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Cwd, &bytes(raw), true)
            .unwrap()
            .as_bytes(),
        raw
    );
    // Some APFS configurations reject these bytes. Preserve the native failure;
    // always exercise the byte resolver and marker encoding on every platform.
    let native_path = f.shadow_root.join(std::ffi::OsStr::from_bytes(raw));
    let marker = Overlay::marker(&root(raw).unwrap()).unwrap();
    assert!(marker.as_bytes().ends_with(b"7261772dff/.wh"));
    if fs::write(&native_path, b"raw").is_err() {
        return;
    }
    assert_eq!(f.read(raw).unwrap(), b"raw");
    assert_eq!(
        f.overlay
            .list(&root(b"").unwrap(), None, 10)
            .unwrap()
            .entries[0]
            .name
            .as_bytes(),
        raw
    );
    f.run(&unlink(raw));
    assert!(matches!(&f.log.lock().unwrap().records[0].payload,
        JournalPayload::Prepare { intent: JournalIntent::Unlink { path, .. } } if path.as_bytes() == b"/raw-\xff"));
    assert_eq!(
        f.overlay.whiteout_inventory().unwrap(),
        vec![bytes(b"/raw-\xff")]
    );
}

#[test]
fn journal_failures_block_mutation_or_require_recovery_without_false_success() {
    let mut f = Fixture::new(&[(b"file", b"base")]);
    f.log.lock().unwrap().fail_prepare = true;
    let action = f.overlay.resolve(&f.process, &unlink(b"file")).unwrap();
    assert_eq!(
        f.overlay
            .prepare(OperationId(Uuid::new_v4()), &action)
            .unwrap_err()
            .kind,
        ErrorKind::Io
    );
    assert!(!f.control.join("whiteouts").exists());
    assert_eq!(f.read(b"file").unwrap_err().kind, ErrorKind::InvalidState);
    let mut f = Fixture::new(&[(b"file", b"base")]);
    let prepared = f.prepare(&unlink(b"file"));
    f.overlay
        .observe_result(
            prepared.operation_id,
            &OperationOutcome::Success { return_value: 0 },
        )
        .unwrap();
    f.log.lock().unwrap().fail_commit = true;
    assert_eq!(
        f.overlay.commit(prepared.operation_id).unwrap_err().kind,
        ErrorKind::Io
    );
    assert_eq!(f.read(b"file").unwrap_err().kind, ErrorKind::InvalidState);
}

#[test]
fn rejects_unchecked_physical_symlinks_and_trailing_file_slash() {
    let mut f = Fixture::new(&[(b"dir/file", b"safe")]);
    std::os::unix::fs::symlink(f.base_root.join("dir"), f.shadow_root.join("link")).unwrap();
    for path in [b"link/file".as_slice(), b"link/../new", b"dir/file/"] {
        assert!(f.overlay.resolve(&f.process, &stat(path)).is_err());
    }
    assert_eq!(
        f.overlay
            .read_at(
                &StoragePath::new(StorageAnchor::Control, b"secret".to_vec()).unwrap(),
                0,
                &mut [0]
            )
            .unwrap_err()
            .kind,
        ErrorKind::Denied
    );
}

#[test]
fn transaction_order_and_checkpoint_whiteouts_are_authoritative() {
    let mut f = Fixture::new(&[(b"file", b"base")]);
    let prepared = f.prepare(&unlink(b"file"));
    assert_eq!(
        f.overlay.commit(prepared.operation_id).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    f.complete(prepared);
    let checkpoint = f
        .overlay
        .checkpoint(&CheckpointRequest {
            id: CheckpointId(Uuid::new_v4()),
            fingerprints: JournalFingerprints {
                base: "base".into(),
                toolchain: "test".into(),
                agent_provider_id: "test".into(),
                agent_version: "1".into(),
                supervisor_version: "1".into(),
            },
            state: JournalLogicalState {
                format_version: 1,
                entries: vec![],
                whiteouts: vec![bytes(b"/forged")],
                agent_session_id: "test".into(),
                agent_state_paths: vec![],
                metadata: vec![],
            },
        })
        .unwrap();
    assert_eq!(checkpoint.state.whiteouts, vec![bytes(b"/file")]);
    assert!(!checkpoint.clean);
}

struct TestDirectoryEncoder;
impl DirectoryEncoder for TestDirectoryEncoder {
    fn encode(
        &mut self,
        _: &ProcessContext,
        _: &FsOp,
        entries: &[DirectoryEntry],
    ) -> Result<EncodedDirectory> {
        // Test-only byte encoding at a synthetic tracee buffer. Production adapters
        // supply native dirent layout and the address from the stopped syscall.
        let data = entries
            .first()
            .map_or(vec![], |entry| entry.name.as_bytes().to_vec());
        Ok(EncodedDirectory {
            consumed: usize::from(!entries.is_empty()),
            result: EmulatedResult {
                outcome: OperationOutcome::Success {
                    return_value: data.len() as u64,
                },
                memory_writes: if data.is_empty() {
                    vec![]
                } else {
                    vec![MemoryWrite {
                        address: 0x1000,
                        bytes: data,
                    }]
                },
            },
        })
    }
}

#[test]
fn readdir_uses_injected_encoding_and_advances_only_after_commit() {
    let mut f = Fixture::new(&[(b"a", b"a"), (b"c", b"c")]);
    fs::write(f.shadow_root.join("b"), b"b").unwrap();
    let object = f.overlay.lookup(&root(b"").unwrap()).unwrap().0.object_id;
    f.process.fds.insert(
        TracedFd(3),
        FdState {
            object,
            logical_path: Some(bytes(b"/")),
            directory: true,
            flags: OpenFlags::default(),
        },
    );
    f.overlay
        .set_directory_encoder(Box::new(TestDirectoryEncoder))
        .unwrap();
    let op = FsOp::ReadDir {
        fd: TracedFd(3),
        max_bytes: 32,
    };
    let first = f.overlay.resolve(&f.process, &op).unwrap();
    assert_eq!(f.overlay.resolve(&f.process, &op).unwrap(), first);
    for name in [b"a".as_slice(), b"b", b"c", b""] {
        let prepared = f.prepare(&op);
        let ResolvedAction::Emulate(encoded) = &prepared.action else {
            panic!()
        };
        assert_eq!(
            encoded
                .memory_writes
                .first()
                .map_or(b"".as_slice(), |w| w.bytes.as_slice()),
            name
        );
        f.overlay
            .observe_result(prepared.operation_id, &encoded.outcome)
            .unwrap();
        f.overlay.commit(prepared.operation_id).unwrap();
        if name == b"a" {
            f.run(&unlink(b"b"));
        }
    }
}

#[test]
fn copy_up_spans_multiple_bounded_io_chunks_and_creates_directories() {
    let data: Vec<_> = (0..MAX_IO_BYTES + 31).map(|n| (n % 251) as u8).collect();
    let mut f = Fixture::new(&[(b"large", &data)]);
    f.run(&open(
        b"large",
        OpenFlags {
            write: true,
            ..Default::default()
        },
    ));
    assert_eq!(fs::read(f.shadow_root.join("large")).unwrap(), data);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"new/deep/dir"),
        mode: 0o750,
    });
    assert!(f.shadow_root.join("new/deep/dir").is_dir());
    assert!(!f.base_root.join("new").exists());
}

#[test]
fn same_path_base_rename_is_a_noop_and_a_cancelled_mutation_abort_does_not_claim_rollback() {
    // The abort here is `Cancelled`: the caller abandoned the transaction, which
    // is an interception failure and still poisons. A kernel-returned errno is
    // the other mode and does *not* behave this way -- see
    // `a_kernel_rejected_write_open_reconciles_and_the_session_keeps_serving_reads`.
    // The observed `Failure` below is only what puts the transaction in an
    // abortable state; it is not what decides the outcome.
    let mut f = Fixture::new(&[(b"base", b"unchanged")]);
    f.run(&rename(b"base", b"base"));
    assert!(!f.shadow_root.join("base").exists());
    assert_eq!(f.read(b"base").unwrap(), b"unchanged");
    let prepared = f.prepare(&open(
        b"base",
        OpenFlags {
            write: true,
            ..Default::default()
        },
    ));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(Errno(13)))
        .unwrap();
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::Cancelled)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState
    );
    assert_eq!(fs::read(f.shadow_root.join("base")).unwrap(), b"unchanged");
    assert_eq!(f.read(b"base").unwrap_err().kind, ErrorKind::InvalidState);
}

#[test]
fn standard_namespace_trait_object_uses_the_same_bound_engine() {
    let mut f = Fixture::new(&[(b"base", b"bytes")]);
    let config = f.overlay.config.take().unwrap();
    let base = f.overlay.base.take().unwrap();
    let (storage, journal) = f.overlay.into_backends();
    let mut namespace = crate::standard_namespace(storage, journal);
    namespace.bind(config, base).unwrap();
    let mut out = [0; 5];
    assert_eq!(
        namespace
            .read_at(&root(b"base").unwrap(), 0, &mut out)
            .unwrap(),
        5
    );
    assert_eq!(&out, b"bytes");
    let action = namespace.resolve(&f.process, &unlink(b"base")).unwrap();
    let id = OperationId(Uuid::new_v4());
    namespace.prepare(id, &action).unwrap();
    namespace
        .observe_result(id, &OperationOutcome::Success { return_value: 0 })
        .unwrap();
    namespace.commit(id).unwrap();
    assert_eq!(
        namespace
            .read_at(&root(b"base").unwrap(), 0, &mut out)
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

fn symlink(name: &[u8], target: &[u8]) -> FsOp {
    FsOp::Symlink {
        target: bytes(target),
        link_dir: DirRef::Cwd,
        link_name: bytes(name),
    }
}

#[test]
fn symlink_create_has_safe_placeholder_identity_metadata_and_journal_order() {
    let mut f = Fixture::new(&[]);
    let op = symlink(b"a/link", b"/b//./file");
    let action = f.overlay.resolve(&f.process, &op).unwrap();
    assert!(!f.shadow_root.join("a").exists());
    assert!(!f.control.join("symlinks").exists());
    let id = OperationId(Uuid::new_v4());
    let prepared = f.overlay.prepare(id, &action).unwrap();
    let placeholder = f.shadow_root.join("a/link");
    assert!(fs::symlink_metadata(&placeholder).unwrap().is_file());
    assert!(fs::read_link(&placeholder).is_err());
    assert!(fs::metadata(placeholder.join("escape")).is_err());
    assert_eq!(
        fs::read(f.control.join(format!("symlinks/targets/{}", id.0))).unwrap(),
        b"/b//./file"
    );
    f.complete(prepared);
    let stat = f.overlay.stat(&root(b"a/link").unwrap(), false).unwrap();
    assert_eq!(stat.object_id, ObjectId(id.0));
    assert_eq!(stat.kind, ObjectKind::LogicalSymlink);
    assert_eq!(stat.len, 10);
    assert_eq!(
        f.overlay.read_link(&root(b"a/link").unwrap()).unwrap(),
        bytes(b"/b//./file")
    );
    let page = f.overlay.list(&root(b"a").unwrap(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].stat, stat);
    let page = f.overlay.list(&root(b"").unwrap(), None, 10).unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|e| e.name.as_bytes())
            .collect::<Vec<_>>(),
        vec![b"a"]
    );
    let log = f.log.lock().unwrap();
    assert_eq!(log.records.len(), 3);
    assert!(matches!(&log.records[0].payload,
        JournalPayload::Prepare { intent: JournalIntent::Symlink { object, path, target } }
        if *object == ObjectId(id.0) && path.as_bytes() == b"/a/link" && target.as_bytes() == b"/b//./file"));
    assert!(matches!(
        log.records[1].payload,
        JournalPayload::ObservedResult { .. }
    ));
    assert!(matches!(log.records[2].payload, JournalPayload::Commit));
}

#[test]
fn readlink_non_utf8_is_verbatim_and_native_buffer_truncates_without_nul() {
    let mut f = Fixture::new(&[]);
    let target = b"\xff/bad//.././tail";
    f.run(&symlink(b"dir/link", target));
    assert_eq!(
        f.overlay
            .read_link(&root(b"dir/link").unwrap())
            .unwrap()
            .as_bytes(),
        target
    );
    let stat = f.overlay.stat(&root(b"dir").unwrap(), true).unwrap();
    f.process.fds.insert(
        TracedFd(9),
        FdState {
            object: stat.object_id,
            logical_path: Some(bytes(b"/dir")),
            directory: true,
            flags: OpenFlags::default(),
        },
    );
    for (dir, path, capacity) in [
        (DirRef::Cwd, b"/dir/link".as_slice(), 100),
        (DirRef::Fd(TracedFd(9)), b"link".as_slice(), 4),
    ] {
        f.overlay.set_readlink_buffer(0x1000, capacity).unwrap();
        let op = FsOp::ReadLink {
            dir,
            path: bytes(path),
        };
        let prepared = f.prepare(&op);
        let ResolvedAction::Emulate(result) = &prepared.action else {
            panic!("readlink must emulate")
        };
        let expected = &target[..target.len().min(capacity as usize)];
        assert_eq!(
            result.memory_writes,
            vec![MemoryWrite {
                address: 0x1000,
                bytes: expected.to_vec()
            }]
        );
        assert_eq!(
            result.outcome,
            OperationOutcome::Success {
                return_value: expected.len() as u64
            }
        );
        f.overlay
            .observe_result(prepared.operation_id, &result.outcome)
            .unwrap();
        f.overlay.commit(prepared.operation_id).unwrap();
        assert_eq!(
            f.overlay.resolve(&f.process, &op).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
    }
    assert!(f.overlay.set_readlink_buffer(0, 0).is_err());
    assert!(f.overlay.set_readlink_buffer(u64::MAX, 2).is_err());
    assert_eq!(
        f.overlay
            .read_link(&root(b"dir").unwrap())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
}

#[test]
fn symlink_traversal_opens_logical_target_and_copy_up_never_opens_placeholder() {
    let mut f = Fixture::new(&[(b"b/file", b"base")]);
    f.run(&symlink(b"a/link", b"/b/file"));
    let prepared = f.prepare(&open(
        b"/a/link",
        OpenFlags {
            read: true,
            ..OpenFlags::default()
        },
    ));
    let ResolvedAction::Rewrite(rewrite) = &prepared.action else {
        panic!("open rewrite")
    };
    assert_eq!(native(&rewrite.paths[0].path.0), f.base_root.join("b/file"));
    assert_eq!(fs::read(native(&rewrite.paths[0].path.0)).unwrap(), b"base");
    f.complete(prepared);
    f.run(&open(
        b"a/link",
        OpenFlags {
            write: true,
            truncate: true,
            ..OpenFlags::default()
        },
    ));
    assert_eq!(f.read(b"a/link").unwrap(), b"");
    assert_eq!(fs::read(f.base_root.join("b/file")).unwrap(), b"base");
    assert!(f.shadow_root.join("b/file").is_file());
    assert_eq!(
        f.overlay.read_link(&root(b"a/link").unwrap()).unwrap(),
        bytes(b"/b/file")
    );
    f.run(&symlink(b"alias", b"b"));
    assert_eq!(
        f.overlay
            .list(&root(b"alias").unwrap(), None, 10)
            .unwrap()
            .entries[0]
            .name
            .as_bytes(),
        b"file"
    );
    f.run(&rename(b"alias/file", b"alias/moved"));
    f.run(&unlink(b"alias/moved"));
    assert_eq!(f.read(b"b/moved").unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn symlink_parent_escape_is_rejected_before_prepare_including_process_root() {
    let mut f = Fixture::new(&[(b"jail/a/file", b"safe")]);
    f.run(&symlink(b"jail/a/link", b"../../../outside"));
    f.process.root = bytes(b"/jail");
    f.process.cwd = bytes(b"/jail");
    let direct = f
        .overlay
        .resolve(&f.process, &stat(b"../outside"))
        .unwrap_err();
    let count = f.log.lock().unwrap().records.len();
    for op in [
        stat(b"a/link"),
        open(
            b"a/link",
            OpenFlags {
                create: true,
                write: true,
                ..OpenFlags::default()
            },
        ),
        unlink(b"a/link/child"),
        rename(b"a/link/child", b"safe"),
    ] {
        let err = f.overlay.resolve(&f.process, &op).unwrap_err();
        assert_eq!(err.kind, direct.kind);
        assert_eq!(err.context, direct.context);
    }
    assert_eq!(f.log.lock().unwrap().records.len(), count);
    assert!(!f.shadow_root.join("outside").exists());
    assert!(!f.shadow_root.parent().unwrap().join("outside").exists());
}

#[test]
fn symlink_loop_and_long_chain_have_a_bounded_iterative_expansion() {
    let mut f = Fixture::new(&[(b"file", b"safe")]);
    f.run(&symlink(b"a", b"b"));
    f.run(&symlink(b"b", b"a"));
    for path in [b"a".as_slice(), b"a/../file"] {
        let err = f.overlay.resolve(&f.process, &stat(path)).unwrap_err();
        // A distinct kind, so a caller can answer with ELOOP without reading
        // the message; containment failures stay InvalidPath.
        assert_eq!(err.kind, ErrorKind::SymlinkLoop);
        assert_eq!(err.context, "symlink expansion limit exceeded");
    }
    for n in (0..=MAX_SYMLINK_EXPANSIONS).rev() {
        let target = if n == MAX_SYMLINK_EXPANSIONS {
            "file".to_string()
        } else {
            format!("link{}", n + 1)
        };
        f.run(&symlink(format!("link{n}").as_bytes(), target.as_bytes()));
    }
    assert_eq!(f.read(b"link1").unwrap(), b"safe");
    assert_eq!(
        f.read(b"link0").unwrap_err().context,
        "symlink expansion limit exceeded"
    );
}

#[test]
fn absolute_symlink_targets_start_at_logical_process_root() {
    let mut f = Fixture::new(&[(b"jail/x/y", b"inside"), (b"x/y", b"outside")]);
    f.run(&symlink(b"jail/a/b/link", b"/x/y"));
    f.process.root = bytes(b"/jail");
    f.process.cwd = bytes(b"/jail/a/b");
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Cwd, &bytes(b"link"), false)
            .unwrap(),
        root(b"jail/x/y").unwrap()
    );
    let action = f
        .overlay
        .resolve(
            &f.process,
            &open(
                b"link",
                OpenFlags {
                    read: true,
                    ..OpenFlags::default()
                },
            ),
        )
        .unwrap();
    let ResolvedAction::Rewrite(rewrite) = action else {
        panic!("open rewrite")
    };
    assert_eq!(
        fs::read(native(&rewrite.paths[0].path.0)).unwrap(),
        b"inside"
    );
}

#[test]
fn relative_symlink_targets_start_at_link_parent_and_expand_before_dotdot() {
    let mut f = Fixture::new(&[
        (b"a/c", b"relative"),
        (b"x/y/file", b"nested"),
        (b"x/file", b"parent"),
    ]);
    f.run(&symlink(b"a/b/link", b"../c"));
    assert_eq!(f.read(b"a/b/link").unwrap(), b"relative");
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Cwd, &bytes(b"/a/b/link"), false)
            .unwrap(),
        root(b"a/c").unwrap()
    );
    f.run(&symlink(b"dir", b"x/y"));
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Cwd, &bytes(b"dir/../file"), false)
            .unwrap(),
        root(b"x/file").unwrap()
    );
    assert!(f.overlay.resolve(&f.process, &stat(b"a/b/link/")).is_err());
}

#[test]
fn symlink_rename_unlink_exclusive_and_nofollow_act_on_link_identity() {
    let mut f = Fixture::new(&[(b"file", b"preserved")]);
    f.run(&symlink(b"link", b"file"));
    let id = f
        .overlay
        .stat(&root(b"link").unwrap(), false)
        .unwrap()
        .object_id;
    for flags in [
        OpenFlags {
            read: true,
            no_follow: true,
            ..OpenFlags::default()
        },
        OpenFlags {
            create: true,
            exclusive: true,
            ..OpenFlags::default()
        },
    ] {
        assert!(f
            .overlay
            .resolve(&f.process, &open(b"link", flags))
            .is_err());
    }
    f.run(&rename(b"link", b"moved"));
    assert_eq!(
        f.overlay
            .stat(&root(b"moved").unwrap(), false)
            .unwrap()
            .object_id,
        id
    );
    assert_eq!(f.read(b"moved").unwrap(), b"preserved");
    f.run(&symlink(b"dest", b"absent"));
    f.run(&rename(b"moved", b"dest"));
    assert_eq!(
        f.overlay.read_link(&root(b"dest").unwrap()).unwrap(),
        bytes(b"file")
    );
    f.run(&unlink(b"dest"));
    assert_eq!(f.read(b"file").unwrap(), b"preserved");
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        0
    );
    f.run(&symlink(b"dest", b"missing"));
    assert_eq!(f.read(b"dest").unwrap_err().kind, ErrorKind::NotFound);
    f.run(&open(
        b"dest",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("missing").is_file());
    assert_eq!(
        f.overlay.read_link(&root(b"dest").unwrap()).unwrap(),
        bytes(b"missing")
    );
}

#[test]
fn symlink_prepare_failure_leaves_no_placeholder_or_metadata() {
    let mut f = Fixture::new(&[]);
    f.log.lock().unwrap().fail_prepare = true;
    let action = f
        .overlay
        .resolve(&f.process, &symlink(b"link", b"target"))
        .unwrap();
    assert!(f
        .overlay
        .prepare(OperationId(Uuid::new_v4()), &action)
        .is_err());
    assert!(!f.shadow_root.join("link").exists());
    assert!(!f.control.join("symlinks").exists());
}

#[test]
fn native_lstat_receives_logical_metadata_and_never_rewrites_to_placeholder() {
    struct Encoder;
    impl StatEncoder for Encoder {
        fn encode(
            &mut self,
            _: &ProcessContext,
            _: &FsOp,
            stat: &BlobStat,
        ) -> Result<EmulatedResult> {
            assert_eq!(stat.kind, ObjectKind::LogicalSymlink);
            assert_eq!(stat.len, 4);
            assert_eq!(stat.mode, 0o777);
            Ok(EmulatedResult {
                outcome: OperationOutcome::Success { return_value: 0 },
                memory_writes: vec![MemoryWrite {
                    address: 0x2000,
                    bytes: stat.len.to_le_bytes().to_vec(),
                }],
            })
        }
    }
    let mut f = Fixture::new(&[(b"file", b"target")]);
    f.run(&symlink(b"link", b"file"));
    let op = FsOp::Stat {
        dir: DirRef::Cwd,
        path: bytes(b"link"),
        follow: false,
    };
    assert_eq!(
        f.overlay.resolve(&f.process, &op).unwrap_err().kind,
        ErrorKind::UnsupportedCapability
    );
    f.overlay.set_stat_encoder(Box::new(Encoder)).unwrap();
    let prepared = f.prepare(&op);
    assert!(matches!(prepared.action, ResolvedAction::Emulate(_)));
    f.complete(prepared);
    f.run(&stat(b"link"));
    assert_eq!(
        f.overlay.stat(&root(b"link").unwrap(), true).unwrap().kind,
        ObjectKind::File
    );
}

#[test]
fn immutable_base_symlinks_resolve_and_copy_up_preserves_identity_and_target() {
    let mut base = Fixture::new(&[]);
    base.run(&open(
        b"file",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    fs::write(base.shadow_root.join("file"), b"base bytes").unwrap();
    base.run(&symlink(b"link", b"file"));
    let id = base
        .overlay
        .stat(&root(b"link").unwrap(), false)
        .unwrap()
        .object_id;
    let config = base.overlay.config().unwrap().clone();
    let (storage, _) = base.overlay.into_backends();
    let mut reader = config.context;
    reader.writer_epoch = None;
    let mut f = Fixture::new(&[]);
    f.overlay.base = Some(Box::new(
        StorageBase::new(storage, config.binding, reader).unwrap(),
    ));
    assert_eq!(f.read(b"link").unwrap(), b"base bytes");
    assert_eq!(
        f.overlay.read_link(&root(b"link").unwrap()).unwrap(),
        bytes(b"file")
    );
    assert_eq!(
        f.overlay
            .list(&root(b"").unwrap(), None, 10)
            .unwrap()
            .entries[1]
            .stat
            .kind,
        ObjectKind::LogicalSymlink
    );
    f.run(&rename(b"link", b"moved"));
    assert_eq!(
        f.overlay
            .stat(&root(b"moved").unwrap(), false)
            .unwrap()
            .object_id,
        id
    );
    assert_eq!(
        f.overlay.read_link(&root(b"moved").unwrap()).unwrap(),
        bytes(b"file")
    );
    assert_eq!(f.read(b"link").unwrap_err().kind, ErrorKind::NotFound);
    assert!(base.shadow_root.join("link").exists());
    f.run(&unlink(b"moved"));
    assert_eq!(f.read(b"file").unwrap(), b"base bytes");
}

#[test]
fn missing_control_target_fails_before_rewrite_and_failed_readlink_consumes_buffer() {
    let mut f = Fixture::new(&[]);
    f.run(&symlink(b"link", b"target"));
    f.overlay.set_readlink_buffer(0x1000, 4).unwrap();
    let op = FsOp::ReadLink {
        dir: DirRef::Cwd,
        path: bytes(b"missing"),
    };
    assert_eq!(
        f.overlay.resolve(&f.process, &op).unwrap_err().kind,
        ErrorKind::NotFound
    );
    let op = FsOp::ReadLink {
        dir: DirRef::Cwd,
        path: bytes(b"link"),
    };
    assert_eq!(
        f.overlay.resolve(&f.process, &op).unwrap_err().kind,
        ErrorKind::UnsupportedCapability
    );
    let id = f
        .overlay
        .stat(&root(b"link").unwrap(), false)
        .unwrap()
        .object_id;
    fs::remove_file(f.control.join(format!("symlinks/targets/{}", id.0))).unwrap();
    let count = f.log.lock().unwrap().records.len();
    assert_eq!(
        f.overlay
            .resolve(&f.process, &stat(b"link"))
            .unwrap_err()
            .kind,
        ErrorKind::CorruptJournal
    );
    assert_eq!(f.log.lock().unwrap().records.len(), count);
}

#[test]
fn symlink_cwd_and_dirfd_anchors_are_resolved_before_identity_checks() {
    let mut f = Fixture::new(&[(b"dir/file", b"anchored")]);
    f.run(&symlink(b"alias", b"dir"));
    let object = f
        .overlay
        .stat(&root(b"dir").unwrap(), true)
        .unwrap()
        .object_id;
    f.process.fds.insert(
        TracedFd(7),
        FdState {
            object,
            logical_path: Some(bytes(b"/alias")),
            directory: true,
            flags: OpenFlags::default(),
        },
    );
    f.process.cwd = bytes(b"/alias");
    for dir in [DirRef::Cwd, DirRef::Fd(TracedFd(7))] {
        assert_eq!(
            f.overlay
                .resolve_path(&f.process, dir, &bytes(b"file"), false)
                .unwrap(),
            root(b"dir/file").unwrap()
        );
    }
    f.process.fds.get_mut(&TracedFd(7)).unwrap().object = ObjectId(Uuid::new_v4());
    assert_eq!(
        f.overlay
            .resolve_path(&f.process, DirRef::Fd(TracedFd(7)), &bytes(b"file"), false)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
}

#[test]
fn failed_completion_cleanup_is_terminal_and_is_not_repeated() {
    let mut f = Fixture::new(&[]);
    let run_id = f.overlay.config().unwrap().binding.run_id;
    f.log.lock().unwrap().fail_close = true;
    let e = f
        .overlay
        .finish_run(&FinishRunRequest {
            run_id,
            root_status: Some(ExitStatus::Code(0)),
            processes_exited: 1,
        })
        .unwrap_err();
    assert!(e.context.contains("journal close after durable completion"));
    assert!(f.overlay.poisoned);
    assert!(f.overlay.config.is_none());
    assert!(f.overlay.base.is_none());
    assert!(f.overlay.renew_writer().is_err());
    f.overlay
        .fail_run(&FailedRunRequest {
            run_id,
            reason: e.to_string(),
            tree_terminated: true,
        })
        .unwrap();
    assert_eq!(f.log.lock().unwrap().closes, 1);
}

#[test]
fn invalid_completion_receipts_never_publish_completion_or_release_authority() {
    for mismatch in 0..3 {
        let mut f = Fixture::with_bad_receipt(&[], Some(mismatch));
        let run_id = f.overlay.config().unwrap().binding.run_id;
        let e = f
            .overlay
            .finish_run(&FinishRunRequest {
                run_id,
                root_status: Some(ExitStatus::Code(0)),
                processes_exited: 1,
            })
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::ProtocolMismatch);
        assert!(f.overlay.completed_run.is_none());
        assert!(f.overlay.config.is_some());
        assert!(f.log.lock().unwrap().records.iter().all(|r| !matches!(
            r.payload,
            JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
        )));
        assert_eq!(f.log.lock().unwrap().closes, 0);
        // Pre-completion failure still permits the ordinary failure cleanup.
        f.overlay
            .fail_run(&FailedRunRequest {
                run_id,
                reason: e.to_string(),
                tree_terminated: true,
            })
            .unwrap();
        assert_eq!(f.log.lock().unwrap().closes, 1);
    }
}

#[test]
fn renewal_failure_latches_and_a_poisoned_session_cannot_renew() {
    let mut f = Fixture::new(&[]);
    let lease = f.overlay.config().unwrap().lease.clone();
    f.overlay.storage.release_writer(&lease).unwrap();
    assert!(f.overlay.renew_writer().is_err());
    assert!(f.overlay.poisoned);
    assert_eq!(
        f.overlay.renew_writer().unwrap_err().kind,
        ErrorKind::InvalidState
    );
}

fn access(path: &[u8], mode: AccessMode, follow: bool) -> FsOp {
    FsOp::Access {
        dir: DirRef::Cwd,
        path: bytes(path),
        mode,
        flags: AccessFlags {
            effective_ids: false,
            follow,
        },
    }
}
fn chown(path: &[u8], uid: Option<u32>, gid: Option<u32>, follow: bool) -> FsOp {
    FsOp::Fchownat {
        dir: DirRef::Cwd,
        path: bytes(path),
        uid,
        gid,
        flags: ChownFlags { follow },
    }
}
const READ: AccessMode = AccessMode {
    read: true,
    write: false,
    execute: false,
};

#[test]
fn access_probes_the_namespace_without_materialising_or_journaling() {
    let mut f = Fixture::new(&[(b"nested/file", b"base bytes")]);
    // A base-only object is probed against the immutable base's own physical
    // path: read-through, not copy-up.
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &access(b"nested/file", READ, true))
        .unwrap()
    else {
        panic!("access rewrite")
    };
    assert_eq!(
        native(&plan.paths[0].path.0),
        f.base_root.join("nested/file")
    );
    let prepared = f.prepare(&access(b"nested/file", READ, true));
    assert!(!f.shadow_root.join("nested/file").exists());
    f.complete(prepared);
    // A probe is not a mutation, so nothing reached the journal at all.
    assert!(f.log.lock().unwrap().records.is_empty());
    // W_OK is still a probe. Asking whether the object could be written must
    // not copy it up on the strength of the question.
    let write_probe = access(
        b"nested/file",
        AccessMode {
            read: false,
            write: true,
            execute: false,
        },
        true,
    );
    let prepared = f.prepare(&write_probe);
    f.complete(prepared);
    assert!(!f.shadow_root.join("nested/file").exists());
    assert!(f.log.lock().unwrap().records.is_empty());
    // Once the object is in the shadow the probe follows it there.
    f.run(&open(
        b"nested/file",
        OpenFlags {
            read: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &access(b"nested/file", READ, true))
        .unwrap()
    else {
        panic!("access rewrite")
    };
    assert_eq!(
        native(&plan.paths[0].path.0),
        f.shadow_root.join("nested/file")
    );
    // A truly-absent target resolves to NotFound: the base is the host, so the
    // supervisor resumes the tracee's own unrewritten faccessat and the kernel's
    // ENOENT equals the namespace's answer. This is the passthrough #49 must not
    // disturb.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &access(b"nested/missing", READ, true))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    // A whiteout-hidden target resolves to Deny(ENOENT) instead, so the supervisor
    // emulates the errno rather than resuming a faccessat that would see the
    // still-present base file (#49).
    f.run(&unlink(b"nested/file"));
    assert_eq!(
        f.overlay
            .resolve(&f.process, &access(b"nested/file", READ, true))
            .unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
    // A bare existence probe (F_OK, all-false) of a directory still resolves.
    assert!(matches!(
        f.overlay
            .resolve(&f.process, &access(b"nested", AccessMode::default(), true))
            .unwrap(),
        ResolvedAction::Rewrite(_)
    ));
}

#[test]
fn access_follows_a_logical_symlink_and_never_probes_its_placeholder() {
    let mut f = Fixture::new(&[(b"b/file", b"base")]);
    f.run(&symlink(b"a/link", b"/b/file"));
    // Following resolves to the target's physical path, not the link's.
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &access(b"a/link", READ, true))
        .unwrap()
    else {
        panic!("access rewrite")
    };
    assert_eq!(native(&plan.paths[0].path.0), f.base_root.join("b/file"));
    // Not following must not reach the placeholder: it is a regular file whose
    // own mode is not the 0o777 the overlay reports for every logical symlink.
    let placeholder = f.shadow_root.join("a/link");
    assert!(fs::symlink_metadata(&placeholder).unwrap().is_file());
    for mode in [
        AccessMode::default(),
        READ,
        AccessMode {
            read: true,
            write: true,
            execute: true,
        },
    ] {
        assert_eq!(
            f.overlay
                .resolve(&f.process, &access(b"a/link", mode, false))
                .unwrap(),
            ResolvedAction::Emulate(EmulatedResult {
                outcome: OperationOutcome::Success { return_value: 0 },
                memory_writes: vec![],
            }),
            "mode {mode:?}"
        );
    }
    // A dangling logical symlink still answers ENOENT when followed.
    f.run(&symlink(b"a/dangling", b"/b/absent"));
    assert_eq!(
        f.overlay
            .resolve(&f.process, &access(b"a/dangling", READ, true))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

#[test]
fn fchownat_follows_a_logical_symlink_to_the_object_it_owns() {
    // `follow_final`'s `_ => false` default is nofollow, which is wrong for
    // fchownat: POSIX follows unless AT_SYMLINK_NOFOLLOW is set. Losing the
    // arm that says so would silently chown the link placeholder instead of its
    // target, so pin the resolved path rather than trusting the comment.
    let mut f = Fixture::new(&[(b"b/file", b"base")]);
    f.run(&symlink(b"a/link", b"/b/file"));
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &chown(b"a/link", Some(501), Some(20), true))
        .unwrap()
    else {
        panic!("fchownat rewrite")
    };
    // The target's path, not the link's. Both are under the shadow root because
    // a mutation always rewrites there, so the file name is what discriminates.
    assert_eq!(native(&plan.paths[0].path.0), f.shadow_root.join("b/file"));
    // lchown acts on the link itself, which is the placeholder.
    let ResolvedAction::Rewrite(plan) = f
        .overlay
        .resolve(&f.process, &chown(b"a/link", Some(501), Some(20), false))
        .unwrap()
    else {
        panic!("fchownat rewrite")
    };
    assert_eq!(native(&plan.paths[0].path.0), f.shadow_root.join("a/link"));
}

#[test]
fn fchownat_journals_a_chown_intent_against_a_copied_up_object() {
    let mut f = Fixture::new(&[(b"nested/file", b"base bytes")]);
    // The journaled object is the one the namespace already knows about, so
    // read its identity before the transaction opens.
    let object = f
        .overlay
        .stat(&root(b"nested/file").unwrap(), true)
        .unwrap();
    // Ownership is applied by the kernel against the rewritten path, so the
    // rewrite must name the shadow even though the object is base-only.
    let action = f
        .overlay
        .resolve(
            &f.process,
            &chown(b"nested/file", Some(501), Some(20), true),
        )
        .unwrap();
    let ResolvedAction::Rewrite(plan) = &action else {
        panic!("fchownat rewrite")
    };
    assert_eq!(
        native(&plan.paths[0].path.0),
        f.shadow_root.join("nested/file")
    );
    assert!(!f.shadow_root.join("nested/file").exists());
    let id = OperationId(Uuid::new_v4());
    let prepared = f.overlay.prepare(id, &action).unwrap();
    // Copy-up ran before the kernel could touch the immutable base.
    assert_eq!(
        fs::read(f.shadow_root.join("nested/file")).unwrap(),
        b"base bytes"
    );
    f.complete(prepared);
    assert_eq!(
        fs::read(f.base_root.join("nested/file")).unwrap(),
        b"base bytes"
    );
    let log = f.log.lock().unwrap().records.clone();
    assert_eq!(log.len(), 3);
    // `object` is the pre-copy-up identity, so on its own the record would name
    // something the kernel never touched. The path locates the shadow object
    // that was actually chowned, and copy_up records that the materialisation
    // happened at all, so a reader can tell a pre-copy-up crash from a later one.
    let shadow_id = f
        .overlay
        .stat(&root(b"nested/file").unwrap(), true)
        .unwrap()
        .object_id;
    assert_ne!(
        shadow_id, object.object_id,
        "copy-up must give the shadow object its own identity, or this test proves nothing"
    );
    assert!(matches!(&log[0].payload,
        JournalPayload::Prepare { intent: JournalIntent::Chown { object: o, path, uid, gid, copy_up } }
        if *o == object.object_id
            && path.as_bytes() == b"/nested/file"
            && *uid == Some(501)
            && *gid == Some(20)
            && *copy_up));
    assert!(matches!(
        log[1].payload,
        JournalPayload::ObservedResult { .. }
    ));
    assert!(matches!(log[2].payload, JournalPayload::Commit));
}

#[test]
fn fchownat_carries_the_unchanged_id_sentinel_and_link_identity_into_the_journal() {
    let mut f = Fixture::new(&[]);
    f.run(&symlink(b"link", b"/target"));
    // lchown acts on the link itself. The placeholder carries the logical
    // symlink's own uid/gid, so the shadow already holds a chownable object
    // and copy-up is a no-op rather than a second placeholder.
    let object = f.overlay.stat(&root(b"link").unwrap(), false).unwrap();
    assert_eq!(object.kind, ObjectKind::LogicalSymlink);
    let prepared = f.prepare(&chown(b"link", None, None, false));
    let ResolvedAction::Rewrite(plan) = &prepared.action else {
        panic!("fchownat rewrite")
    };
    assert_eq!(native(&plan.paths[0].path.0), f.shadow_root.join("link"));
    f.complete(prepared);
    assert_eq!(
        f.overlay.read_link(&root(b"link").unwrap()).unwrap(),
        bytes(b"/target")
    );
    // The sentinel survives as `None`: a no-op request is journaled as a no-op,
    // never as a change to ID 4294967295.
    let log = f.log.lock().unwrap().records.clone();
    assert!(matches!(&log[3].payload,
        JournalPayload::Prepare {
            intent: JournalIntent::Chown { object: o, path, uid: None, gid: None, copy_up: false }
        }
        if *o == object.object_id && path.as_bytes() == b"/link"));
    drop(log);
    // An absent target is ENOENT before anything is journaled or copied up.
    let before = f.log.lock().unwrap().records.len();
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"absent", Some(0), None, true))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    assert_eq!(f.log.lock().unwrap().records.len(), before);
}

#[test]
fn a_base_only_directory_chown_is_refused_before_anything_is_journaled() {
    // prepare appends and flushes the Chown intent before the copy-up that
    // would fail here, so refusing at resolve is what keeps the journal free of
    // an ownership change that never happened. `chown -R` over a base tree
    // reaches this on its first directory.
    //
    // What this pins is the *session*, not the run. Fchownat is Materialise, so
    // the supervisor has no non-mutating NotFound resume to fall back on: the
    // refusal below propagates and ends the run rather than reaching the tracee
    // as an errno. The session staying usable is what keeps the journal
    // reconcilable, not what keeps the tracee running.
    let mut f = Fixture::new(&[(b"dir/file", b"base")]);
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"dir", Some(0), Some(0), true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    // Nothing durable, and nothing to reconcile: no intent, no Commit, no Abort.
    assert!(f.log.lock().unwrap().records.is_empty());
    assert!(!f.shadow_root.join("dir").exists());
    // The overlay session is usable afterwards rather than poisoned, which is
    // the whole point of refusing before prepare. The run still ends; see above.
    assert!(!f.overlay.poisoned);
    assert!(matches!(
        f.overlay.resolve(&f.process, &stat(b"dir/file")).unwrap(),
        ResolvedAction::Rewrite(_)
    ));
    // A directory already in the shadow needs no copy-up, so it still chowns.
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"made"),
        mode: 0o750,
    });
    let prepared = f.prepare(&chown(b"made", Some(501), Some(20), true));
    assert!(matches!(prepared.action, ResolvedAction::Rewrite(_)));
    f.complete(prepared);
    // Assert on the chown's own record. Taking the first Prepare here would pick
    // up the mkdir's Create and pass whether or not the chown journaled anything.
    let intents: Vec<_> = f
        .log
        .lock()
        .unwrap()
        .records
        .iter()
        .filter_map(|r| match &r.payload {
            JournalPayload::Prepare { intent } => Some(intent.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(intents.len(), 2, "one Create for the mkdir, one Chown");
    assert!(matches!(
        &intents[0],
        JournalIntent::Create {
            directory: true,
            ..
        }
    ));
    assert!(matches!(&intents[1],
        JournalIntent::Chown { path, uid: Some(501), gid: Some(20), copy_up: false, .. }
        if path.as_bytes() == b"/made"));
}

#[test]
fn an_unchanged_id_chown_of_a_base_object_is_refused_rather_than_silently_re_owning_it() {
    // The kernel sets only the IDs the tracee supplied. Were this allowed on a
    // shadow object that does not wear the base's ownership, a POSIX no-op
    // chown(-1, -1) would silently move the object to our identity, which is the
    // opposite of what the sentinel asks for.
    //
    // #60 lifted the half of this that was a missing mechanism: copy-up now
    // carries the base's uid/gid onto the shadow, so the sentinel is honourable
    // wherever the carry holds. What remains is privilege, per object -- and
    // this test is the case where it does not hold. `force_owner` is what makes
    // it that case: a base object owned by a uid umbra cannot take ownership of
    // is the ordinary situation for an overlay and the one CI cannot construct
    // for real, because creating a root-owned file needs root. Every assertion
    // below is the one this test has always made; only the fixture now names the
    // condition the refusal still depends on. The lifted half is pinned by
    // `an_unchanged_id_chown_is_admitted_once_copy_up_can_carry_the_base_owner`.
    let mut f = Fixture::build(
        &[(b"file", b"base bytes")],
        Setup {
            force_owner: Some((4242, 4242)),
            ..Setup::default()
        },
    );
    for (uid, gid) in [(None, None), (Some(501), None), (None, Some(20))] {
        assert_eq!(
            f.overlay
                .resolve(&f.process, &chown(b"file", uid, gid, true))
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedCapability,
            "uid {uid:?} gid {gid:?}"
        );
    }
    // Refused before anything durable or material happened.
    assert!(f.log.lock().unwrap().records.is_empty());
    assert!(!f.shadow_root.join("file").exists());
    assert!(!f.overlay.poisoned);
    // Setting both IDs inherits nothing from the copy, so it is still allowed.
    let prepared = f.prepare(&chown(b"file", Some(501), Some(20), true));
    f.complete(prepared);
    assert!(f.shadow_root.join("file").is_file());
    // And once the object is in the shadow, copy_up is a no-op, so the sentinel
    // is safe again: the ID left alone is the shadow object's own.
    let prepared = f.prepare(&chown(b"file", None, None, true));
    assert!(matches!(prepared.action, ResolvedAction::Rewrite(_)));
    f.complete(prepared);
}

#[test]
fn chowning_a_base_only_logical_symlink_copies_it_up_and_keeps_its_identity() {
    // copy_up's LogicalSymlink branch runs only for a base-only object, and it
    // re-creates the placeholder through create_symlink(.., stat.object_id),
    // deliberately preserving the base identity. So copy_up: true with an
    // *unchanged* object id is reachable — the one case where the journal
    // record's object still names the object the kernel chowns.
    let mut base = Fixture::new(&[]);
    base.run(&open(
        b"file",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    fs::write(base.shadow_root.join("file"), b"base bytes").unwrap();
    base.run(&symlink(b"link", b"file"));
    let id = base
        .overlay
        .stat(&root(b"link").unwrap(), false)
        .unwrap()
        .object_id;
    let config = base.overlay.config().unwrap().clone();
    let (storage, _) = base.overlay.into_backends();
    let mut reader = config.context;
    reader.writer_epoch = None;
    let mut f = Fixture::new(&[]);
    f.overlay.base = Some(Box::new(
        StorageBase::new(storage, config.binding, reader).unwrap(),
    ));
    // The link exists only in the base, so nothing is in the shadow yet.
    assert!(!f.shadow_root.join("link").exists());
    assert_eq!(
        f.overlay
            .stat(&root(b"link").unwrap(), false)
            .unwrap()
            .object_id,
        id
    );
    // lchown with both IDs set: allowed, and it enters the symlink copy-up.
    let prepared = f.prepare(&chown(b"link", Some(501), Some(20), false));
    assert!(fs::symlink_metadata(f.shadow_root.join("link"))
        .unwrap()
        .is_file());
    f.complete(prepared);
    // Identity and target both survive the materialisation.
    assert_eq!(
        f.overlay
            .stat(&root(b"link").unwrap(), false)
            .unwrap()
            .object_id,
        id
    );
    assert_eq!(
        f.overlay.read_link(&root(b"link").unwrap()).unwrap(),
        bytes(b"file")
    );
    let log = f.log.lock().unwrap().records.clone();
    assert!(matches!(&log[0].payload,
        JournalPayload::Prepare {
            intent: JournalIntent::Chown { object: o, path, copy_up: true, .. }
        }
        if *o == id && path.as_bytes() == b"/link"));
}

// Errnos the kernel routinely hands back on a Materialise operation. Named so
// the abort-contract tests read as the failures they model; test-local because
// nothing in the crate API spells them.
const EPERM: Errno = Errno(1);
const EACCES: Errno = Errno(13);
const ENOSPC: Errno = Errno(28);

// Case (a): a kernel-returned errno on a metadata mutation.
#[test]
fn a_kernel_rejected_chown_reconciles_and_leaves_the_session_usable() {
    // EPERM is the ordinary answer when an unprivileged tracee chowns to
    // another uid. Fchownat is a Materialise operation, so before #53 that
    // routine failure took the poisoning mutation abort path and ended the run
    // instead of handing the tracee its errno. `KernelRefused` names the
    // observed fact, so the abort reconciles: it returns Ok, the session stays
    // usable, and the supervisor goes on to resume the tracee.
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EPERM))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(EPERM))
        .unwrap();
    assert!(!f.overlay.poisoned);
    let payloads = f.log.lock().unwrap().records.clone();
    assert!(matches!(
        &payloads[0].payload,
        JournalPayload::Prepare {
            intent: JournalIntent::Chown { .. }
        }
    ));
    assert!(matches!(
        payloads[1].payload,
        JournalPayload::ObservedResult { .. }
    ));
    // Reconciling does not make the transaction vanish: the Abort record is
    // still written, and now names the kernel's own verdict.
    assert!(matches!(
        &payloads[2].payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "a failed chown must not commit"
    );
    // The copy-up is not rolled back, and abort does not claim it was: the
    // shadow object stays, carrying the base's bytes. `file` is at the root, so
    // `copy_up` materialised no ancestors here; under a not-yet-shadowed base
    // directory it would also have materialised one, and that one now carries the
    // base directory's own group and other bits rather than a conjured `0o755`
    // (#56 is fixed --
    // `a_kernel_refused_op_under_a_restricted_base_directory_reconciles_and_keeps_the_mode`
    // covers exactly this shape). Copy-up is uncounted by choice, not because it
    // changes nothing -- see `copy_up` and `Pending.rollback`.
    assert_eq!(fs::read(f.shadow_root.join("file")).unwrap(), b"base bytes");
    assert_eq!(fs::read(f.base_root.join("file")).unwrap(), b"base bytes");
    // The run continues: a reconciled session still serves reads and still
    // accepts the next transaction.
    assert_eq!(f.read(b"file").unwrap(), b"base bytes");
    let retried = f.prepare(&chown(b"file", Some(0), Some(0), true));
    f.complete(retried);
}

// Case (b): a kernel-returned errno on the rename/whiteout path.
#[test]
fn a_kernel_rejected_rename_reconciles_without_applying_commit_time_whiteouts() {
    // A rename across paths is the widest Materialise plan: it carries both
    // source/destination whiteouts and, when the destination is a logical
    // symlink, a retired symlink index. Those are applied in `commit`, never in
    // `abort`, so reconciling a refused rename must leave the namespace exactly
    // as it was -- no whiteout set, no index retired.
    let mut f = Fixture::new(&[(b"source", b"base bytes")]);
    f.run(&symlink(b"target", b"elsewhere"));
    let indexes = f.control.join("symlinks/objects");
    let count = |dir: &PathBuf| fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(
        count(&indexes),
        1,
        "the destination symlink has an index to retire"
    );
    // Everything the setup symlink journaled is already behind us; only the
    // refused rename's own records are inspected below.
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&rename(b"source", b"target"));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EACCES))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(EACCES))
        .unwrap();
    assert!(!f.overlay.poisoned);
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(matches!(
        &payloads[0].payload,
        JournalPayload::Prepare {
            intent: JournalIntent::Rename { .. }
        }
    ));
    assert!(matches!(
        payloads[1].payload,
        JournalPayload::ObservedResult { .. }
    ));
    assert!(matches!(
        &payloads[2].payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "the refused rename must not commit"
    );
    // No whiteout marker was written at all: the source is still visible and
    // the control tree has no whiteout namespace.
    assert!(!f.control.join("whiteouts").exists());
    assert_eq!(f.read(b"source").unwrap(), b"base bytes");
    // The destination index was not retired, so the destination is still the
    // logical symlink it was before the refused rename.
    assert_eq!(count(&indexes), 1);
    assert_eq!(
        f.overlay.read_link(&root(b"target").unwrap()).unwrap(),
        bytes(b"elsewhere")
    );
}

// Case (c): a kernel-returned errno on the data path.
#[test]
fn a_kernel_rejected_write_open_reconciles_and_the_session_keeps_serving_reads() {
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    // `FsOp::Write` itself cannot reach `prepare` in this engine: `resolve`
    // still refuses descriptor-addressed data operations as beyond-MVP, even
    // though `dispatch` already classifies them `Materialise`. The resolvable
    // data-path member of the same class is a write-mode `Open`, exercised
    // below; the real `FsOp::Write` is covered against the supervisor in
    // `umbra-supervisor`'s `events.rs` tests, which drive a namespace double.
    assert_eq!(
        f.overlay
            .resolve(
                &f.process,
                &FsOp::Write {
                    fd: TracedFd(3),
                    length: 4,
                    offset: None,
                },
            )
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    let prepared = f.prepare(&open(
        b"file",
        OpenFlags {
            write: true,
            ..Default::default()
        },
    ));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    let payloads = f.log.lock().unwrap().records.clone();
    assert!(matches!(
        &payloads.last().unwrap().payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert_eq!(f.read(b"file").unwrap(), b"base bytes");
    assert_eq!(fs::read(f.base_root.join("file")).unwrap(), b"base bytes");
}

// The two branches of `prepare`'s mutating `Open` arm. Nothing else in the suite
// distinguishes them -- the case (c) test above deliberately opens an *existing*
// file, so it exercises copy-up only -- and a change that erased the distinction
// would otherwise be invisible to CI. The boundary that used to run between them
// has moved: both reconcile now, and the poison line runs between a creation
// `abort` can unlink and one it cannot, pinned by the base-absent-parent sibling
// below.
#[test]
fn a_refused_copy_up_open_reconciles() {
    // The base already holds the object, so `prepare` duplicated it into the
    // shadow instead of creating it, and the transaction is reconcilable. A
    // root-level path, so no ancestors were materialised either.
    let mut f = Fixture::new(&[(b"existing", b"base bytes")]);
    let prepared = f.prepare(&open(
        b"existing",
        OpenFlags {
            write: true,
            ..Default::default()
        },
    ));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert_eq!(f.read(b"existing").unwrap(), b"base bytes");
    // The copy the shadow now holds is not rolled back and must not be: it
    // duplicates what the base already exposes, so nothing about the content
    // view turns on whether it is there.
    assert_eq!(
        fs::read(f.shadow_root.join("existing")).unwrap(),
        b"base bytes"
    );
}

// The creating side, which is what #55 changes. `prepare` materialises an object
// that did not logically exist; `abort` unlinks it and reconciles, so the tracee
// gets its errno and the namespace is left exactly as it was found.
#[test]
fn a_refused_creating_open_rolls_back_the_object_and_reconciles() {
    let mut f = Fixture::new(&[]);
    assert_eq!(f.read(b"fresh").unwrap_err().kind, ErrorKind::NotFound);
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&create_open(b"fresh"));
    assert!(
        f.shadow_root.join("fresh").exists(),
        "prepare materialised the object, so the rollback below is not vacuous"
    );
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // Filesystem-level, not merely the engine's view of it: a "rollback" that
    // only cleared in-memory state would satisfy every other assertion here.
    assert!(
        !f.shadow_root.join("fresh").exists(),
        "abort unlinked what prepare created"
    );
    // The divergence is *absent*, not merely refused. A session that reconciled
    // without rolling back would report the path present and empty.
    assert_eq!(f.read(b"fresh").unwrap_err().kind, ErrorKind::NotFound);
    // The Abort record is still written for every mutating abort (#53); the
    // rollback runs after it, and no Commit is forged to describe it.
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(matches!(
        &payloads.last().unwrap().payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "a refused open must not commit"
    );
    // The load-bearing one. Before the rollback this path answered every later
    // `O_CREAT|O_EXCL` with `AlreadyExists` forever, which wedges the lock-file
    // and atomic-temp-file idioms permanently after a single transient ENOSPC.
    // It now resolves, and the whole retry runs to completion.
    assert!(matches!(
        f.overlay
            .resolve(&f.process, &exclusive_open(b"fresh"))
            .unwrap(),
        ResolvedAction::Rewrite(_)
    ));
    f.run(&exclusive_open(b"fresh"));
    assert_eq!(f.read(b"fresh").unwrap(), b"");
}

// The rollback's storage requests belong to the transaction, not to the run.
// `context()` seeds each request's operation ID from the live `Pending`, falling
// back to the run-level context when there is none, so clearing `self.pending`
// before the unlink loop would silently charge the undo of a journaled `Prepare`
// to the session -- the one storage request whose attribution a backend log most
// needs to be right.
#[test]
fn the_rollbacks_unlink_is_attributed_to_the_transaction_it_undoes() {
    let unlinks = Unlinks::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            unlinks: Some(unlinks.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&create_open(b"fresh"));
    unlinks.lock().unwrap().clear();
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    let unlinks = unlinks.lock().unwrap();
    let (path, context) = match unlinks.as_slice() {
        [one] => one,
        other => panic!("the rollback issues exactly one unlink, got {other:?}"),
    };
    assert_eq!(path, &root(b"fresh").unwrap());
    // Derived, not equal: `context()` gives every request inside one transaction
    // its own identity so a backend cannot bind two different requests to one
    // idempotency key. What must hold is that the *seed* was the transaction's
    // ID rather than the run-level one.
    assert_ne!(context.operation_id, prepared.operation_id);
    assert!(
        (1..=64).any(
            |serial| prepared.operation_id.derive(f.overlay.session, serial)
                == context.operation_id
        ),
        "the unlink must derive from the aborted transaction's operation ID"
    );
}

// The benign-ancestor half at the `create` site. The reconciled-rename test below
// pins this at the `parents` site, but this is where a rollback entry and a
// surviving benign ancestor actually coexist: `dir` is base-only, so `parents`
// materialises a shadow of a directory the run already exposes and does not
// report it, while the file underneath it goes on the rollback list and is
// unlinked. This is the pin that keeps #64's directory rollback from
// over-reaching into "remove every ancestor prepare touched".
#[test]
fn a_refused_creating_open_under_a_base_only_parent_rolls_back_only_the_object() {
    // `0o555` on the base directory, so the widening is observable: without it
    // the shadow would come back `0o555` too, and against a default `0o755` base
    // the assertion below would hold whether or not the widening happened.
    let mut f = Fixture::with_base_modes(&[(b"dir/other", b"other")], &[(b"dir", 0o555)]);
    assert!(!f.shadow_root.join("dir").exists());
    let prepared = f.prepare(&create_open(b"dir/fresh"));
    assert!(f.shadow_root.join("dir/fresh").exists());
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // The object is gone; the ancestor stays. Removing the ancestor is possible
    // since #64 -- `LocalStorage` answers `RemoveDirectory` now -- and is still
    // not wanted: it shadows a directory the run already exposed, so `parents`
    // does not report it and it never reaches the rollback list. That this
    // survives a build that *can* remove it is what makes the assertion mean
    // something it did not mean before.
    assert!(!f.shadow_root.join("dir/fresh").exists());
    assert!(f.shadow_root.join("dir").is_dir());
    assert_eq!(f.read(b"dir/fresh").unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(f.read(b"dir/other").unwrap(), b"other");
    // What the ancestor does leave behind, so the narrowed claim in `parents` is
    // pinned rather than asserted only in prose: a shadow directory now outranks
    // the base one in `lookup`, and it carries #56's widened owner bits, so a
    // `0o555` base directory reads back `0o755` after a syscall the tracee was
    // told had failed. This is the commit path's pre-existing divergence,
    // inherited rather than introduced -- `copy_up` leaves the same thing behind
    // on the same walk -- but it survives a *reconciled abort*, which is what this
    // pins. `& !umask()` because `mkdir(2)` masks what `LocalStorage` requests.
    assert_eq!(mode_of(&shadow(&f, b"dir")), 0o755 & !umask());
    // And the base is never modified to make any of it work.
    assert_eq!(mode_of(&f.base_root.join("dir")), 0o555);
    // The retry is not wedged: the path is creatable again.
    f.run(&exclusive_open(b"dir/fresh"));
    assert_eq!(f.read(b"dir/fresh").unwrap(), b"");
}

// The sibling that used to carry the poison half at this site, flipped by
// [#64](https://github.com/invakid404/umbra/issues/64). `newdir` exists in
// neither the shadow nor the base, so `parents` still materialises a directory
// shadowing nothing -- the materialisation is unchanged and is still the case
// under test. What changed is its undo: `LocalStorage` answers `RemoveDirectory`
// now, so the ancestor goes on `rollback` rather than latching the since-retired
// `Pending.created`, and the refusal reconciles. The poison half did not disappear
// with it; it moved to the siblings whose subject is an undo that *failed* --
// `a_refused_rename_whose_destination_parent_rollback_fails_poisons` here, and the
// three `a_symlink_rollback_that_cannot_unlink_…_poisons` at the symlink arm.
// [#69](https://github.com/invakid404/umbra/issues/69) retired the latch
// mechanism entirely, so that is now the only surviving one.
#[test]
fn a_refused_creating_open_under_a_base_absent_parent_rolls_back_the_object_and_its_ancestor() {
    let mut f = Fixture::new(&[]);
    assert_eq!(
        f.read(b"newdir/fresh").unwrap_err().kind,
        ErrorKind::NotFound
    );
    let prepared = f.prepare(&create_open(b"newdir/fresh"));
    assert!(
        f.shadow_root.join("newdir").is_dir(),
        "prepare materialised a directory that shadows nothing"
    );
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // The *ancestor* is the assertion that earns the rename: a build that
    // unlinked the file and left the phantom directory standing would pass on
    // `newdir/fresh` alone, and would have left exactly the divergence the gate
    // exists to prevent.
    assert!(!f.shadow_root.join("newdir/fresh").exists());
    assert!(
        !f.shadow_root.join("newdir").exists(),
        "the ancestor materialised over nothing must be gone too"
    );
    assert_eq!(
        f.read(b"newdir/fresh").unwrap_err().kind,
        ErrorKind::NotFound
    );
    // The retry is not wedged, one level deeper than the root-level case above:
    // the `O_CREAT|O_EXCL` lock-file idiom works after the refusal instead of
    // answering `AlreadyExists` forever.
    f.run(&exclusive_open(b"newdir/fresh"));
    assert_eq!(f.read(b"newdir/fresh").unwrap(), b"");
}

// The multi-level case, and the only place the reverse-insertion claim in
// `Pending.rollback` is observable: with one ancestor, "leaf before parents" and
// "any order" are the same walk. `a`, `a/b` and `a/b/c` are all materialised over
// nothing, so the list is `[(a,Dir), (a/b,Dir), (a/b/c,Dir), (a/b/c/fresh,File)]`
// and only the reverse walk gives each `rmdir` an empty directory -- forward
// order would meet `ENOTEMPTY` at `a` and poison.
#[test]
fn a_refused_creating_open_rolls_back_a_whole_materialised_ancestor_cascade() {
    let mut f = Fixture::new(&[]);
    let prepared = f.prepare(&create_open(b"a/b/c/fresh"));
    assert!(f.shadow_root.join("a/b/c/fresh").exists());
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // Every level, not just the deepest: a rollback that stopped at `a/b/c`
    // would leave two phantom directories and still satisfy an assertion about
    // the leaf alone.
    for level in ["a/b/c/fresh", "a/b/c", "a/b", "a"] {
        assert!(
            !f.shadow_root.join(level).exists(),
            "{level} survived the rollback"
        );
    }
    assert_eq!(
        f.read(b"a/b/c/fresh").unwrap_err().kind,
        ErrorKind::NotFound
    );
    f.run(&exclusive_open(b"a/b/c/fresh"));
    assert_eq!(f.read(b"a/b/c/fresh").unwrap(), b"");
}

// The flip of `a_refused_symlink_creation_still_poisons`, which this replaces.
// That test pinned the last op whose `prepare` the engine refused to undo: the
// placeholder is a file `Unlink` could always have removed, but `create_symlink`
// also writes two Control blobs, and the backing index is keyed on the
// placeholder's backend `ObjectId`, so dropping the placeholder alone would
// strand it. [#69](https://github.com/invakid404/umbra/issues/69) makes that one
// entry -- `RollbackEntry::Symlink`, three recorded paths removed index-first --
// and the refusal reconciles.
//
// The poison half did not disappear with the latch, and this test is not where it
// went: it moved to the three
// `a_symlink_rollback_that_cannot_unlink_…_poisons` siblings below, one per
// removal, which is the same shape #55 and #64 gave their own flipped tests.
//
// Honest about reachability, on the same terms as
// `a_refused_mkdir_rolls_back_the_directory_it_created` below and for the same
// reason. `resolve` answers `Emulate` for `FsOp::Symlink`, so the supervisor
// never produces this sequence and `observe_emulated_refusal` fabricates it. The
// caveat has changed sign rather than gone: it used to excuse a poison the
// corroboration conjunct would have produced anyway, and that argument died with
// the latch. It now says only that the arm is wired ahead of a non-emulated
// `symlinkat`, and `Overlay::abort` is a namespace API this suite calls directly,
// so the rule is pinned at the layer that owns it.
#[test]
fn a_refused_symlink_creation_rolls_back_the_placeholder_and_both_control_blobs() {
    let mut f = Fixture::new(&[]);
    assert_eq!(f.read(b"link").unwrap_err().kind, ErrorKind::NotFound);
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    let id = prepared.operation_id;
    assert!(
        fs::symlink_metadata(f.shadow_root.join("link"))
            .unwrap()
            .is_file(),
        "prepare wrote the placeholder, so the rollback below is not vacuous"
    );
    assert!(f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // Filesystem-level, and all three objects: a build that unlinked the
    // placeholder alone would satisfy the `read` below and still leave a backing
    // index pointing at a backend object ID that no longer exists.
    assert!(
        !f.shadow_root.join("link").exists(),
        "abort unlinked the placeholder"
    );
    assert!(
        !f.control
            .join(format!("symlinks/targets/{}", id.0))
            .exists(),
        "the Control target blob is gone too"
    );
    // By count rather than by name: the index is keyed on the *backend* object ID
    // the shadow assigned the placeholder, which this test never sees -- and that
    // opacity is precisely why the entry has to carry the path.
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        0,
        "the backing index is gone, not stranded"
    );
    assert_eq!(f.read(b"link").unwrap_err().kind, ErrorKind::NotFound);
    // The load-bearing one, the same as at the creating-`Open` site: before the
    // rollback this path answered every later `O_CREAT|O_EXCL` with
    // `AlreadyExists` forever.
    f.run(&exclusive_open(b"link"));
    assert_eq!(f.read(b"link").unwrap(), b"");
}

// The four-objects case, and the replacement for
// `a_latched_transaction_undoes_nothing_it_created`, whose own prose named the
// four: "a symlink's placeholder, its materialised ancestor, its Control target
// blob and its backing index are four objects that would each have to survive a
// partial undo." Three of them were what the issue counted; the fourth is the
// finding. `create_symlink` reaches `create` -> `parents` for the placeholder, so
// `symlink("newdir/link", …)` materialises `newdir/` over nothing, and a build
// that undid only the three named pieces would leave a phantom directory standing
// after a reconciled abort -- precisely the divergence
// [#64](https://github.com/invakid404/umbra/issues/64) closed for the
// creating-`Open` arm, reintroduced here.
//
// The ancestor is not folded into `RollbackEntry::Symlink`. It is an ordinary
// `Directory` entry the arm queues *before* its own, so the reverse walk unwinds
// it leaf-first like every other materialised ancestor -- which is also what makes
// the `remove_directory` meet an empty directory here, the three `unlink`s having
// run first.
#[test]
fn a_refused_symlink_creation_rolls_back_its_materialised_ancestor_too() {
    let mut f = Fixture::new(&[]);
    assert_eq!(
        f.read(b"newdir/link").unwrap_err().kind,
        ErrorKind::NotFound
    );
    let prepared = f.prepare(&symlink(b"newdir/link", b"/target"));
    let id = prepared.operation_id;
    assert!(
        f.shadow_root.join("newdir").is_dir(),
        "prepare materialised a directory that shadows nothing"
    );
    assert!(f.shadow_root.join("newdir/link").exists());
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // The *ancestor* is the assertion that earns this test: a build that removed
    // the placeholder and both blobs and left `newdir/` standing would pass every
    // other assertion here and would have left exactly the divergence the gate
    // exists to prevent.
    assert!(!f.shadow_root.join("newdir/link").exists());
    assert!(
        !f.shadow_root.join("newdir").exists(),
        "the ancestor materialised for the placeholder must be gone too"
    );
    assert!(!f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        0,
        "the backing index is gone, not stranded"
    );
    assert_eq!(
        f.read(b"newdir/link").unwrap_err().kind,
        ErrorKind::NotFound
    );
    // The retry is not wedged, one level deeper than the root-level case above.
    f.run(&exclusive_open(b"newdir/link"));
    assert_eq!(f.read(b"newdir/link").unwrap(), b"");
}

// The symlink undo's own failure path, in three siblings -- one per removal. This
// is #55's rollback-fails-poison rule and #64's `remove_directory` analogue
// extended to the three `unlink`s of `RollbackEntry::Symlink`, and it is where the
// poison half of `a_refused_symlink_creation_still_poisons` went.
//
// Each asserts the backend's own kind, deliberately not `InvalidState`. This is
// the one `abort` path whose error is the storage backend's rather than the
// engine's, and the engine must not classify it: the same refusal is `Io` from
// `LocalStorage`, `InvalidState` from `umbra-storage-tar` and the kernel's errno
// from the NFS backends. A sibling asserting `InvalidState` out of habit would be
// asserting the engine had classified something it must pass through.
//
// Together they also pin the *order*, which is otherwise unobservable: what is
// still standing after each failure is a function of how far the loop got. Step 1
// is the one step that is forced -- the index's name is derived from the
// placeholder's backend object ID -- so it goes first, and the residue tables
// below are the visible consequence.

// Step 1. Nothing has been removed yet, so all three objects are still standing:
// an intact, self-consistent logical symlink.
#[test]
fn a_symlink_rollback_that_cannot_unlink_the_backing_index_poisons() {
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    let id = prepared.operation_id;
    f.observe_emulated_refusal(ENOSPC);
    // Flipped after `prepare`, so the materialisation ran against a healthy
    // backend and only the undo is refused. By prefix because the index's name is
    // the backend object ID this test never sees.
    *failure.lock().unwrap() = Some((
        ErrorKind::Io,
        StorageAnchor::Control,
        b"symlinks/objects/".to_vec(),
    ));
    assert_eq!(
        f.overlay
            .abort(id, &AbortReason::KernelRefused(ENOSPC))
            .unwrap_err()
            .kind,
        ErrorKind::Io,
        "a failed rollback reports the backend's error, not the engine's"
    );
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        1
    );
    assert!(f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    assert!(
        f.shadow_root.join("link").exists(),
        "index-first means a step-1 failure removes nothing"
    );
}

// Step 2. The ordering claim, stated as a residue: the index is already gone, so a
// partial undo never strands it. What is left is a placeholder and an orphan blob,
// and nothing points at either.
#[test]
fn a_symlink_rollback_that_cannot_unlink_the_target_blob_poisons() {
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    let id = prepared.operation_id;
    f.observe_emulated_refusal(ENOSPC);
    *failure.lock().unwrap() = Some((
        ErrorKind::Io,
        StorageAnchor::Control,
        b"symlinks/targets/".to_vec(),
    ));
    assert_eq!(
        f.overlay
            .abort(id, &AbortReason::KernelRefused(ENOSPC))
            .unwrap_err()
            .kind,
        ErrorKind::Io
    );
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        0,
        "the index went first, so a failure here cannot strand it"
    );
    assert!(f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    assert!(f.shadow_root.join("link").exists());
}

// Step 3, and the one of the three that can plausibly fail on an otherwise
// healthy backend: the placeholder is Root-anchored, inside the tracee-visible
// shadow tree and subject to its directory permissions. Ordering it last is the
// residue-quality tiebreak `RollbackEntry::Symlink` describes -- both control
// blobs are already gone, so the residue is a phantom empty file with nothing
// dangling behind it.
#[test]
fn a_symlink_rollback_that_cannot_unlink_the_placeholder_poisons() {
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    let id = prepared.operation_id;
    f.observe_emulated_refusal(ENOSPC);
    *failure.lock().unwrap() = Some((ErrorKind::Io, StorageAnchor::Root, b"link".to_vec()));
    assert_eq!(
        f.overlay
            .abort(id, &AbortReason::KernelRefused(ENOSPC))
            .unwrap_err()
            .kind,
        ErrorKind::Io
    );
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        0
    );
    assert!(!f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    assert!(
        f.shadow_root.join("link").exists(),
        "the phantom path is the residue, and nothing dangles behind it"
    );
}

// The success path, where the order buys nothing and is unobservable in the tree:
// all three removals succeed and every assertion about what is left would hold
// under either order. So it is pinned against the recorder instead. The siblings
// above observe the order through failure injection; this one observes it
// directly, and is what a reordering of `RollbackEntry::Symlink`'s fields would
// have to flip.
#[test]
fn the_symlink_undo_unlinks_the_index_before_the_blob_and_the_placeholder() {
    let unlinks = Unlinks::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            unlinks: Some(unlinks.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    let id = prepared.operation_id;
    // Cleared after `prepare`, so only the undo's requests are under assertion.
    unlinks.lock().unwrap().clear();
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    let unlinks = unlinks.lock().unwrap();
    let paths: Vec<_> = unlinks.iter().map(|(path, _)| path.clone()).collect();
    let [index, blob, placeholder] = paths.as_slice() else {
        panic!("the symlink undo issues exactly three unlinks, got {paths:?}")
    };
    // The index by prefix -- its name is the backend object ID the test never
    // sees, which is the whole reason the entry has to carry the path.
    assert_eq!(index.anchor(), StorageAnchor::Control);
    assert!(index.as_bytes().starts_with(b"symlinks/objects/"));
    assert_eq!(
        blob,
        &StoragePath::new(
            StorageAnchor::Control,
            format!("symlinks/targets/{}", id.0).into_bytes()
        )
        .unwrap()
    );
    assert_eq!(placeholder, &root(b"link").unwrap());
}

// The newly wired `FsOp::Mkdir` arm, which latched unconditionally until #64 on
// the premise that `LocalStorage` could not remove a directory. Both the
// directory the op names and the ancestor `parents` materialised under it are
// removable now, so the arm records an undo instead.
//
// Honest about reachability: `resolve` answers `Emulate` for `Mkdir`, so the
// supervisor's `syscall_exit` never drives a `KernelRefused` abort here -- see
// `observe_emulated_refusal`, which is why the outcome is seeded rather than
// observed. `Overlay::abort` is a namespace API this suite calls directly, so
// the engine's rule is pinned at the layer that owns it, and the arm is not
// dead-and-unpinned against the day a non-emulated `mkdir` reaches it.
#[test]
fn a_refused_mkdir_rolls_back_the_directory_it_created() {
    let mut f = Fixture::new(&[]);
    let prepared = f.prepare(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"/newdir/made"),
        mode: 0o755,
    });
    assert!(f.shadow_root.join("newdir/made").is_dir());
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert!(!f.shadow_root.join("newdir/made").exists());
    assert!(
        !f.shadow_root.join("newdir").exists(),
        "the ancestor materialised under it goes back too"
    );
    assert_eq!(
        f.read(b"newdir/made").unwrap_err().kind,
        ErrorKind::NotFound
    );
}

// The `FsOp::Unlink` destroy-gap (#66), from both directions. `prepare` used to
// call `remove_symlink_index` and `storage.unlink` inline, which made an
// `Unlink` the one `Whiteout`-class transaction carrying an effect outside
// `commit`: an abort of it -- reconciling or poisoning -- arrived after the
// shadow object was already gone, and shadow-only bytes have no second copy
// anywhere in the run. Nor was an abort the reachable path: the supervisor
// refuses an `Emulate` only after `prepare` has run, so an intercepted
// `unlink(2)` abandoned the run into recovery, taking any shadow object its
// target had with it. The destruction is a `Pending.destroy` plan now,
// performed in `commit` beside the whiteout it belongs to.
//
// Honest about reachability, in the same idiom as the `Mkdir` arm above:
// `resolve` answers `Emulate` for `Unlink`, so the supervisor's `syscall_exit`
// never drives a `KernelRefused` abort here -- see `observe_emulated_refusal`,
// which is why the outcome is seeded rather than observed, and why
// `kernel_refusal.rs` carries no sibling for any of this. `Overlay::abort` and
// `Overlay::commit` are namespace APIs this suite calls directly, so the rule is
// pinned at the layer that owns it. None of these assert on journal *shape*,
// which the seeding helper cannot reproduce.
#[test]
fn a_refused_unlink_leaves_the_shadow_object_and_its_bytes_standing() {
    let mut f = Fixture::new(&[]);
    // Shadow-only, created in-run: its bytes exist nowhere else. A copied-up base
    // file would have left the base as a second copy and made the assertion below
    // pass for a reason that has nothing to do with the gap.
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    fs::write(f.shadow_root.join("fresh"), b"shadow only").unwrap();
    let prepared = f.prepare(&unlink(b"fresh"));
    assert!(f.shadow_root.join("fresh").exists());
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // Asserted at the filesystem, not at the engine's view of it: the object is
    // there and so are its bytes. Before the destruction moved into `commit`
    // this read failed outright.
    assert_eq!(
        fs::read(f.shadow_root.join("fresh")).unwrap(),
        b"shadow only"
    );
    assert_eq!(f.read(b"fresh").unwrap(), b"shadow only");
    assert!(
        !f.control.join("whiteouts").exists(),
        "the whiteout is a commit-time plan a reconciled abort never applied"
    );
}

#[test]
fn a_refused_unlink_of_a_logical_symlink_leaves_the_link_readable() {
    let mut f = Fixture::new(&[]);
    f.run(&symlink(b"link", b"/target"));
    let indexes = f.control.join("symlinks/objects");
    let count = |dir: &PathBuf| fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count(&indexes), 1, "the link has a backing index to lose");
    let prepared = f.prepare(&unlink(b"link"));
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    // The index is the half a fix that deferred only the object unlink would
    // miss: without it the placeholder reads back as an ordinary empty `0o444`
    // file rather than as a logical symlink.
    assert_eq!(count(&indexes), 1);
    assert!(f.shadow_root.join("link").exists());
    assert_eq!(
        f.overlay.read_link(&root(b"link").unwrap()).unwrap(),
        bytes(b"/target")
    );
}

// The invariant `Pending.unreconcilable` was specified for and then not shipped:
// no `prepare` arm performs an irreversible effect, so no arm would have set it,
// and a flag no arm sets is the dead latch #69 removed. This is the compiler
// behind that sentence, and it deliberately spans both sides of the transaction
// so it cannot pass by the work having been deleted rather than deferred.
#[test]
fn an_unlink_prepare_destroys_nothing_before_commit() {
    let unlinks = Unlinks::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            unlinks: Some(unlinks.clone()),
            ..Setup::default()
        },
    );
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    // Cleared after the setup, so only the unlink transaction's own requests are
    // under assertion.
    unlinks.lock().unwrap().clear();
    let prepared = f.prepare(&unlink(b"fresh"));
    assert!(
        unlinks.lock().unwrap().is_empty(),
        "prepare records a plan; it performs no destruction"
    );
    assert!(f.shadow_root.join("fresh").exists());
    f.complete(prepared);
    assert_eq!(
        unlinks.lock().unwrap().len(),
        1,
        "commit is where the destruction happens"
    );
    assert!(!f.shadow_root.join("fresh").exists());
}

// The anti-regression for the happy path: deferring is not forgetting. If the
// plan ever stopped being consumed, every assertion here flips.
#[test]
fn the_unlink_commit_still_removes_the_object_the_index_and_sets_the_whiteout() {
    let mut f = Fixture::new(&[]);
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    f.run(&unlink(b"fresh"));
    assert!(!f.shadow_root.join("fresh").exists());
    let marker = Overlay::marker(&root(b"fresh").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    assert_eq!(f.read(b"fresh").unwrap_err().kind, ErrorKind::NotFound);

    let mut f = Fixture::new(&[]);
    f.run(&symlink(b"link", b"/target"));
    let indexes = f.control.join("symlinks/objects");
    let count = |dir: &PathBuf| fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count(&indexes), 1);
    f.run(&unlink(b"link"));
    assert!(!f.shadow_root.join("link").exists());
    assert_eq!(count(&indexes), 0, "the index goes with the placeholder");
    let marker = Overlay::marker(&root(b"link").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    assert_eq!(
        f.overlay
            .read_link(&root(b"link").unwrap())
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

// Reconciling leaves no half-state behind: no stale `destroy`, no stale whiteout
// plan, no consumed operation slot. The retry is the proof, in the idiom case (a)
// ends on.
#[test]
fn a_reconciled_unlink_leaves_the_session_usable_and_the_retried_unlink_commits() {
    let mut f = Fixture::new(&[]);
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    fs::write(f.shadow_root.join("fresh"), b"shadow only").unwrap();
    let prepared = f.prepare(&unlink(b"fresh"));
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert_eq!(f.read(b"fresh").unwrap(), b"shadow only");
    let retried = f.prepare(&unlink(b"fresh"));
    f.complete(retried);
    assert!(!f.shadow_root.join("fresh").exists());
    assert_eq!(f.read(b"fresh").unwrap_err().kind, ErrorKind::NotFound);
}

// The poison mirror. Moving the removal into `commit` does not make the
// transaction infallible -- it makes it atomic in the direction that matters. A
// commit-time destruction that fails still poisons, and the residue is the one
// recovery can act on: the object is still standing.
#[test]
fn an_unlink_commit_that_cannot_unlink_the_object_poisons_with_the_backends_own_kind() {
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    fs::write(f.shadow_root.join("fresh"), b"shadow only").unwrap();
    let prepared = f.prepare(&unlink(b"fresh"));
    let id = prepared.operation_id;
    f.overlay
        .observe_result(id, &OperationOutcome::Success { return_value: 0 })
        .unwrap();
    // Flipped after `prepare` -- which destroys nothing now -- so only the
    // commit-time removal is refused.
    *failure.lock().unwrap() = Some((ErrorKind::Io, StorageAnchor::Root, b"fresh".to_vec()));
    assert_eq!(
        f.overlay.commit(id).unwrap_err().kind,
        ErrorKind::Io,
        "a failed destruction reports the backend's error, not the engine's"
    );
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read(f.shadow_root.join("fresh")).unwrap(),
        b"shadow only",
        "the object the destruction could not remove is what recovery finds"
    );
    // The marker went first and is inert while the object stands: a shadow
    // object outranks its own whiteout, so the residue is the pre-transaction
    // view rather than a base object resurfacing at a path claimed deleted.
    let marker = Overlay::marker(&root(b"fresh").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
}

// #69's ordering rule, transcribed into `commit` and pinned there: the index name
// is derived from a live `shadow_stat` of the placeholder, so an index-first
// failure strands nothing. The sibling in spirit of
// `a_symlink_rollback_that_cannot_unlink_the_placeholder_poisons`, one phase over.
#[test]
fn an_unlink_commit_that_cannot_remove_the_symlink_index_poisons_before_the_placeholder_goes() {
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    f.run(&symlink(b"link", b"/target"));
    let prepared = f.prepare(&unlink(b"link"));
    let id = prepared.operation_id;
    f.overlay
        .observe_result(id, &OperationOutcome::Success { return_value: 0 })
        .unwrap();
    // By prefix because the index's name is the backend object ID this test never
    // sees -- the same handle the rollback siblings use.
    *failure.lock().unwrap() = Some((
        ErrorKind::Io,
        StorageAnchor::Control,
        b"symlinks/objects/".to_vec(),
    ));
    assert_eq!(f.overlay.commit(id).unwrap_err().kind, ErrorKind::Io);
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        f.shadow_root.join("link").exists(),
        "index-first means a failure there removes nothing, so the residue is a \
         live logical symlink rather than an index nothing can re-derive"
    );
}

// The other half of the gate, and the arm an `Unlink` actually takes today:
// `resolve` answers `Emulate(Success)` for it, `observe_result` refuses any
// outcome differing from the emulated one, so a real session's abort can only be
// uncorroborated. Before this change that arm destroyed first and poisoned
// second; this is the test that would have failed.
#[test]
fn an_uncorroborated_unlink_abort_poisons_and_still_destroys_nothing() {
    let mut f = Fixture::new(&[]);
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    fs::write(f.shadow_root.join("fresh"), b"shadow only").unwrap();
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&unlink(b"fresh"));
    f.observe_emulated_refusal(EPERM);
    let failure = f
        .overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::InvalidState);
    assert_eq!(failure.context, "aborted effects require reconciliation");
    assert!(f.overlay.poisoned);
    assert_eq!(
        fs::read(f.shadow_root.join("fresh")).unwrap(),
        b"shadow only",
        "poisoning is not a licence to have destroyed something first"
    );
    assert!(!f.control.join("whiteouts").exists());
    // #53's guarantee: every mutating abort journals its reason, poison arm
    // included. Asserted by presence rather than by position -- the seeded
    // outcome appends no `ObservedResult`, so this suite's records are one
    // shorter here than production's.
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(payloads.iter().any(
        |r| matches!(&r.payload, JournalPayload::Abort { reason } if reason.contains("KernelRefused"))
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "a poisoned unlink must not commit"
    );
}

// The residue-ordering claim of the arm, pinned against the recorder in the idiom
// `the_symlink_undo_unlinks_the_index_before_the_blob_and_the_placeholder`
// established. It needs the refusal as a stop rather than reading a successful
// run: the marker is a `Create` and the destruction an `Unlink`, so the two logs
// share no clock and a run in which both succeed is identical under either order.
// `Recorder` records the request and refuses it before delegating, and the
// closure returns at that point, so the creates below are exactly the ones that
// ran before the removal was attempted.
#[test]
fn the_unlink_commit_writes_the_whiteout_marker_before_it_removes_the_object() {
    let creates = Creates::default();
    let unlinks = Unlinks::default();
    let failure = UnlinkFailure::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            creates: Some(creates.clone()),
            unlinks: Some(unlinks.clone()),
            fail_unlink: Some(failure.clone()),
            ..Setup::default()
        },
    );
    f.run(&open(
        b"fresh",
        OpenFlags {
            write: true,
            create: true,
            ..Default::default()
        },
    ));
    creates.lock().unwrap().clear();
    unlinks.lock().unwrap().clear();
    let prepared = f.prepare(&unlink(b"fresh"));
    let id = prepared.operation_id;
    f.overlay
        .observe_result(id, &OperationOutcome::Success { return_value: 0 })
        .unwrap();
    *failure.lock().unwrap() = Some((ErrorKind::Io, StorageAnchor::Root, b"fresh".to_vec()));
    assert_eq!(f.overlay.commit(id).unwrap_err().kind, ErrorKind::Io);
    let unlinks = unlinks.lock().unwrap();
    let paths: Vec<_> = unlinks.iter().map(|(path, _)| path.clone()).collect();
    assert_eq!(
        paths.as_slice(),
        [root(b"fresh").unwrap()],
        "the destruction was attempted, and it is the only unlink the commit issues"
    );
    let marker = Overlay::marker(&root(b"fresh").unwrap()).unwrap();
    assert!(
        creates
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == &marker),
        "the marker was already written when the removal was attempted; under the \
         other order the removal would have been refused first and this create \
         would never have run"
    );
}

// `Pending.rollback` is per transaction, not per session, and that is the property
// that survives the retirement of `Pending.created` -- which this test used to
// pin, as `the_creation_latch_is_per_transaction_not_per_session`. A stale entry
// would be worse than a stale latch: the second transaction's reconciled abort
// would issue a removal against the *first* transaction's path, undoing a
// committed object to reconcile an uncommitted one.
//
// The supervisor's `Journaling` double flattens this to a session constant and
// says so; nothing else in either suite drives two transactions through the gate,
// so it is pinned here. The first transaction is a symlink because its undo is the
// widest -- three `unlink`s -- so a leaked entry would be loudest there.
#[test]
fn the_rollback_list_is_per_transaction_not_per_session() {
    let unlinks = Unlinks::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            unlinks: Some(unlinks.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&symlink(b"link", b"/target"));
    unlinks.lock().unwrap().clear();
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert_eq!(
        unlinks.lock().unwrap().len(),
        3,
        "the symlink undo is three unlinks"
    );

    // A second transaction at a different path, through the same session. Its
    // rollback list is its own: one object, one unlink.
    let prepared = f.prepare(&create_open(b"fresh"));
    // Cleared after the prepare, so only the second undo's requests are under
    // assertion — the first transaction's three are dropped here too.
    unlinks.lock().unwrap().clear();
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    let unlinks = unlinks.lock().unwrap();
    let paths: Vec<_> = unlinks.iter().map(|(path, _)| path.clone()).collect();
    assert_eq!(
        paths,
        vec![root(b"fresh").unwrap()],
        "the second transaction removes its own object and nothing else"
    );
    assert!(!f.shadow_root.join("fresh").exists());
}

// The same boundary on the rename plan, which materialises through `parents`
// rather than `create`. Case (b) above renames onto an existing root-level path;
// this one has to create a destination parent directory that shadows nothing.
// That used to be the load-bearing pin for the un-rollbackable half at the
// `parents` site -- `fresh/` has no base counterpart, so #55's narrowing left it
// exactly where #53 put it -- and #64 removes it: the ancestor is `rmdir`-able,
// so the arm records it and the refusal reconciles. Its assertions now match its
// reconciling neighbour below, which is the point: the two halves of the
// `parents` site answer the same way once the undo exists, and the difference
// that remains is only which ancestors survive.
#[test]
fn a_refused_rename_that_created_destination_parents_rolls_them_back() {
    let mut f = Fixture::new(&[(b"source", b"base bytes")]);
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&rename(b"source", b"fresh/target"));
    assert!(
        f.shadow_root.join("fresh").is_dir(),
        "prepare created the destination parent"
    );
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EACCES))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(EACCES))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert!(
        !f.shadow_root.join("fresh").exists(),
        "the phantom destination parent is gone"
    );
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(matches!(
        &payloads.last().unwrap().payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "the refused rename must not commit"
    );
    // Commit-time effects stay unapplied, exactly as in the neighbour: no
    // whiteout was written at all, so the source is still visible.
    assert!(!f.control.join("whiteouts").exists());
    assert_eq!(f.read(b"source").unwrap(), b"base bytes");
    assert_eq!(
        f.read(b"fresh/target").unwrap_err().kind,
        ErrorKind::NotFound
    );
}

// The poison half's second surviving mechanism at this site, and the one that
// replaces "a directory cannot be removed": a removal that *failed*. This is
// #55's rollback-fails-poison rule extended to directories, and the reason the
// flipped test above could not simply drop its poison assertions.
//
// The asserted kind is the backend's own -- `Io` here, from the injected
// failure -- and deliberately not `InvalidState`. This is the one `abort` path
// whose error is the storage backend's rather than the engine's, and the engine
// must not match on it: the same refusal is `Io` with `ENOTEMPTY` from
// `LocalStorage`, `InvalidState` from `umbra-storage-tar` and the kernel's errno
// from the NFS backends. A sibling that asserted `InvalidState` out of habit
// would be asserting the engine had classified something it must pass through.
#[test]
fn a_refused_rename_whose_destination_parent_rollback_fails_poisons() {
    let failure = RemoveDirectoryFailure::default();
    let mut f = Fixture::build(
        &[(b"source", b"base bytes")],
        Setup {
            fail_remove_directory: Some(failure.clone()),
            ..Setup::default()
        },
    );
    let prepared = f.prepare(&rename(b"source", b"fresh/target"));
    assert!(f.shadow_root.join("fresh").is_dir());
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EACCES))
        .unwrap();
    // Flipped after `prepare`, so the materialisation ran against a healthy
    // backend and only the undo is refused.
    *failure.lock().unwrap() = Some(ErrorKind::Io);
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::KernelRefused(EACCES))
            .unwrap_err()
            .kind,
        ErrorKind::Io,
        "a failed rollback reports the backend's error, not the engine's"
    );
    assert!(f.overlay.poisoned);
    // The directory the engine could not remove is still there, which is exactly
    // why the run had to end: reporting a reconciliation it did not perform would
    // leave a phantom path answering `AlreadyExists` forever.
    assert!(f.shadow_root.join("fresh").is_dir());
}

// Its complement, and what #55 newly delivers at the `parents` site: a
// destination parent the base holds but the shadow has not copied up yet. The
// predicate used to be shadow-shaped, so this poisoned; it is logical now, the
// materialised ancestor shadows a directory the run already exposes, and the
// refusal reconciles. Matches the assertions of the reconciled-rename test in
// case (b) above, because the same commit-only effects must stay unapplied.
#[test]
fn a_refused_rename_onto_a_base_only_destination_parent_reconciles() {
    let mut f = Fixture::new(&[(b"source", b"base bytes"), (b"dir/other", b"other")]);
    assert!(
        !f.shadow_root.join("dir").exists(),
        "the destination parent is base-only, which is the case under test"
    );
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&rename(b"source", b"dir/target"));
    assert!(
        f.shadow_root.join("dir").is_dir(),
        "prepare materialised a shadow of the base directory"
    );
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EACCES))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(EACCES))
        .unwrap();
    assert!(!f.overlay.poisoned);
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(matches!(
        &payloads.last().unwrap().payload,
        JournalPayload::Abort { reason } if reason.contains("KernelRefused")
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "the refused rename must not commit"
    );
    // Commit-time effects stay unapplied: no whiteout was written at all, so the
    // source is still visible and the control tree has no whiteout namespace.
    assert!(!f.control.join("whiteouts").exists());
    assert_eq!(f.read(b"source").unwrap(), b"base bytes");
    assert_eq!(f.read(b"dir/target").unwrap_err().kind, ErrorKind::NotFound);
    // Nothing was rolled back either, and nothing should have been: the shadow
    // directory is a faithful stand-in for the base directory it shadows (#56),
    // publishing no path the run did not already expose.
    assert!(f.shadow_root.join("dir").is_dir());
    assert_eq!(f.read(b"dir/other").unwrap(), b"other");
}

// Case (d), the guard: every abort that is *not* a corroborated kernel refusal
// still poisons a mutating transaction. This is what keeps the two modes
// structurally distinct rather than collapsing into one always-reconcile path.
#[test]
fn a_genuine_mid_transaction_abort_still_poisons_a_mutation() {
    for reason in [
        AbortReason::Cancelled,
        AbortReason::RecoveryRequired("interception lost".into()),
        AbortReason::Failed(error(ErrorKind::Io, "supervisor gave up")),
    ] {
        let mut f = Fixture::new(&[(b"file", b"base bytes")]);
        let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
        f.overlay
            .observe_result(prepared.operation_id, &OperationOutcome::Failure(EPERM))
            .unwrap();
        assert_eq!(
            f.overlay
                .abort(prepared.operation_id, &reason)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidState,
            "{reason:?} is an interception failure, not an observed kernel verdict"
        );
        assert!(f.overlay.poisoned, "{reason:?} must poison");
    }
}

// The same guard on a transaction that actually *has* a rollback entry, which
// the loop above cannot reach: `chown` prepares through `copy_up` alone, so its
// `Pending.rollback` is empty and an unlink loop that ran unconditionally would
// still pass it. A creating `Open` is the branch where the two interact.
//
// The ordering is the invariant: the rollback runs only *after* the corroboration
// gate has been cleared, never before it. An uncorroborated abort means the
// interception broke down and the kernel's real verdict is unknown -- the
// rewritten `open` may well have succeeded, and the tracee may be holding a live
// fd to that very object. Unlinking there would destroy data on the strength of a
// verdict nobody observed, so the object must still be standing afterwards.
#[test]
fn an_uncorroborated_abort_does_not_roll_back_what_prepare_created() {
    for reason in [
        AbortReason::Cancelled,
        AbortReason::RecoveryRequired("interception lost".into()),
        AbortReason::Failed(error(ErrorKind::Io, "supervisor gave up")),
    ] {
        let mut f = Fixture::new(&[]);
        let prepared = f.prepare(&create_open(b"fresh"));
        assert!(f.shadow_root.join("fresh").exists());
        // Observed, so the *only* thing separating this from the reconciling
        // test is the reason itself.
        f.overlay
            .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
            .unwrap();
        assert_eq!(
            f.overlay
                .abort(prepared.operation_id, &reason)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidState,
            "{reason:?} is an interception failure, not an observed kernel verdict"
        );
        assert!(f.overlay.poisoned, "{reason:?} must poison");
        assert!(
            f.shadow_root.join("fresh").exists(),
            "{reason:?} must not unlink: the kernel's verdict was never established"
        );
    }
}

// The uncorroborated-abort property with a genuinely multi-part rollback list,
// which is what the deleted `a_latched_transaction_undoes_nothing_it_created`
// said it could not construct. That test drove this exact shape -- a symlink
// under an absent parent -- to show that a transaction poisoning at the latch
// undid nothing on the way out, and had to note that a latched transaction's
// `rollback` was always empty, so the "consumes nothing" half was vacuous there.
// After [#69](https://github.com/invakid404/umbra/issues/69) there is no latch,
// the same shape records *four* objects, and the early return reached through the
// gate's surviving conjunct makes the assertion mean what it always claimed to.
//
// The claimed errno is one the session never observed, which is an interception
// inconsistency rather than a refused syscall: the abort must poison **and
// consume nothing**, because the early return sits above the rollback loop.
#[test]
fn an_uncorroborated_abort_of_a_symlink_undoes_none_of_the_four_objects() {
    let mut f = Fixture::new(&[]);
    let prepared = f.prepare(&symlink(b"newdir/link", b"/target"));
    let id = prepared.operation_id;
    assert!(f.shadow_root.join("newdir/link").exists());
    // Observed, so the *only* thing separating this from the reconciling sibling
    // above is that the claimed errno is not the one the session saw.
    f.observe_emulated_refusal(ENOSPC);
    assert_eq!(
        f.overlay
            .abort(id, &AbortReason::KernelRefused(EACCES))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState,
        "the claimed errno is not the one the session observed"
    );
    assert!(f.overlay.poisoned);
    // All four objects `prepare` wrote: the placeholder, the ancestor `parents`
    // materialised for it, the Control target blob, and the backing index. A
    // poisoned run is left exactly as it was, not partly undone. The index is
    // asserted by count rather than by name because it is keyed on the *backend*
    // object ID the shadow assigned the placeholder, which this test never sees.
    assert!(
        f.shadow_root.join("newdir/link").exists(),
        "a poisoned run is left exactly as it was, not partly undone"
    );
    assert!(f.shadow_root.join("newdir").is_dir());
    assert!(f
        .control
        .join(format!("symlinks/targets/{}", id.0))
        .exists());
    assert_eq!(
        fs::read_dir(f.control.join("symlinks/objects"))
            .unwrap()
            .count(),
        1,
        "the backing index survives too, or a partial undo stranded it"
    );
}

// The rollback's own failure path, which is the one `abort` return whose error
// kind is the storage backend's rather than `InvalidState`. Revoking write on the
// shadow root makes the `unlink(2)` fail after `prepare` has already created the
// object; the session must poison rather than report a reconciliation it did not
// perform, and the object it could not remove must still be there.
//
// Mode-based injection because `Setup` has no storage-failure hook -- it injects
// base-stat and journal failures only. A uid that ignores the mode makes the
// `unwrap_err` below panic, so under root this fails loudly rather than passing
// vacuously.
#[test]
fn a_rollback_that_cannot_unlink_poisons_instead_of_claiming_success() {
    let mut f = Fixture::new(&[]);
    let prepared = f.prepare(&create_open(b"fresh"));
    let object = f.shadow_root.join("fresh");
    assert!(object.exists());
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    // r-x: the directory is still searchable, so the object is still reachable
    // and `stat`-able; only the unlink is refused.
    let restore = fs::metadata(&f.shadow_root).unwrap().permissions();
    fs::set_permissions(&f.shadow_root, fs::Permissions::from_mode(0o500)).unwrap();
    let aborted = f
        .overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC));
    // Before the unwrap, not after: a uid that ignores the mode makes this an
    // `Ok` and the unwrap below a panic, and an unrestored `0o500` would then
    // defeat `TempDir::drop` and leak the directory.
    fs::set_permissions(&f.shadow_root, restore).unwrap();
    let failure = aborted.unwrap_err();
    assert_ne!(
        failure.kind,
        ErrorKind::NotFound,
        "the unlink failed for want of permission, not because the object was gone"
    );
    assert!(
        f.overlay.poisoned,
        "a rollback that did not happen must not leave the session usable"
    );
    assert!(
        object.exists(),
        "the object the rollback could not remove is still standing, which is why \
         the session had to poison"
    );
}

#[test]
fn an_uncorroborated_kernel_refusal_claim_poisons_instead_of_reconciling() {
    // The reason is a claim by the caller; the observed outcome is this
    // session's own record. A claim with no observed verdict behind it, or one
    // naming an errno the session never saw, is an interception inconsistency,
    // so it fails closed onto the poison path.
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::KernelRefused(EPERM))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState,
        "no result was ever observed for this transaction"
    );
    assert!(f.overlay.poisoned);

    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EPERM))
        .unwrap();
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::KernelRefused(EACCES))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState,
        "the claimed errno is not the one the session observed"
    );
    assert!(f.overlay.poisoned);

    // An observed *success* is not a refusal either, whatever the caller claims.
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
    f.overlay
        .observe_result(
            prepared.operation_id,
            &OperationOutcome::Success { return_value: 0 },
        )
        .unwrap();
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::KernelRefused(EPERM))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState
    );
    assert!(f.overlay.poisoned);
}

#[test]
fn a_non_mutating_abort_stays_infallible_under_every_reason() {
    // ReadThrough operations have no effects to reconcile, so abort has always
    // returned Ok for them regardless of reason. `KernelRefused` joins that set
    // without disturbing it -- including when nothing was observed, because the
    // corroboration check only gates the mutation path.
    for reason in [
        AbortReason::Cancelled,
        AbortReason::RecoveryRequired("gone".into()),
        AbortReason::Failed(error(ErrorKind::Io, "gone")),
        AbortReason::KernelRefused(EPERM),
    ] {
        let mut f = Fixture::new(&[(b"file", b"base bytes")]);
        let prepared = f.prepare(&stat(b"file"));
        f.overlay.abort(prepared.operation_id, &reason).unwrap();
        assert!(!f.overlay.poisoned, "{reason:?} must not poison a lookup");
    }
}

// ---------------------------------------------------------------------------
// #56: the mode `parents` gives a shadow ancestor it has to materialise.
//
// Until #56 that mode was a hardcoded `0o755`, so a base directory at `0700` was
// answered as `0755` from the moment anything under it was touched -- on the
// success path and on #53's reconciled-abort path alike. It now carries the mode
// of the base directory it shadows.

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}
/// The process umask, measured rather than assumed.
///
/// `LocalStorage` materialises a directory with `DirBuilder::mode`, and
/// `mkdir(2)` masks that with the umask, so the mode on disk is not the mode the
/// engine asked for: under the usual `umask 022` a requested `0o777` lands as
/// exactly the `0o755` the defect produced. Hard-coding `0o022` here would make
/// these tests fail under a different umask for a reason that has nothing to do
/// with #56, so measure it. The umask-free assertion is the *requested* mode,
/// through `Recorder`; these on-disk checks only confirm the real effect.
fn umask() -> u32 {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    fs::DirBuilder::new().mode(0o777).create(&probe).unwrap();
    0o777 & !mode_of(&probe)
}
/// The mode `parents` asked for when it created the shadow directory at `path`.
fn requested_directory_mode(creates: &Creates, path: &[u8]) -> Option<u32> {
    creates
        .lock()
        .unwrap()
        .iter()
        .find(|(p, options)| {
            p.anchor() == StorageAnchor::Root
                && p.as_bytes() == path
                && options.kind == CreateKind::Directory
        })
        .map(|(_, options)| options.mode)
}
fn shadow(f: &Fixture, path: &[u8]) -> PathBuf {
    f.shadow_root.join(std::ffi::OsStr::from_bytes(path))
}
fn write_open(path: &[u8]) -> FsOp {
    open(
        path,
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    )
}
fn create_open(path: &[u8]) -> FsOp {
    open(
        path,
        OpenFlags {
            write: true,
            create: true,
            ..OpenFlags::default()
        },
    )
}
fn exclusive_open(path: &[u8]) -> FsOp {
    open(
        path,
        OpenFlags {
            write: true,
            create: true,
            exclusive: true,
            ..OpenFlags::default()
        },
    )
}

// Case (a).
#[test]
fn copy_up_carries_the_base_directory_mode_onto_a_new_shadow_ancestor() {
    // `0o777` is deliberately not asserted on disk: `umask 022` turns it into
    // `0o755`, which is the value the defect produced, so an on-disk assertion on
    // it could never tell the fix from the bug. It is covered through the
    // requested mode, which no umask touches.
    //
    // The rows are chosen along two axes, because the first table was chosen along
    // one and missed a whole defect class. `0o700`/`0o750`/`0o777` carry
    // owner-write and are umask-safe; `0o555`/`0o500`/`0o511` are the read-only
    // base directories umbra exists to overlay, and their expected result is *not*
    // the base mode -- see `shadow_parent_mode` and the `| 0o700` widening.
    for (mode, expected) in [
        (0o700u32, 0o700u32),
        (0o750, 0o750),
        (0o777, 0o777),
        // Owner bits widened; group and other preserved exactly.
        (0o555, 0o755),
        (0o500, 0o700),
        (0o511, 0o711),
    ] {
        let creates = Creates::default();
        let mut f = Fixture::build(
            &[(b"d/f", b"base bytes")],
            Setup {
                base_modes: &[(b"d", mode)],
                creates: Some(creates.clone()),
                ..Setup::default()
            },
        );
        assert!(!shadow(&f, b"d").exists(), "no shadow ancestor yet");
        f.run(&write_open(b"d/f"));
        assert_eq!(
            requested_directory_mode(&creates, b"d"),
            Some(expected),
            "shadow ancestor takes the base's group/other bits with owner bits widened"
        );
        assert_eq!(
            expected & 0o077,
            mode & 0o077,
            "widening must never touch group or other bits"
        );
        assert_eq!(mode_of(&shadow(&f, b"d")), expected & !umask());
        // The base is untouched, as always.
        assert_eq!(mode_of(&f.base_root.join("d")), mode);
    }
}

// Case (j): a base directory with no owner-write must not produce a shadow
// ancestor umbra cannot write into.
#[test]
fn a_read_only_base_directory_still_yields_a_writable_shadow_ancestor() {
    // The canonical overlay base: the Nix store is `r-xr-xr-x`, the Go module
    // cache is `0o555`, `chmod -w` source trees are the same shape. Making a
    // read-only tree writable is the whole point of an overlay.
    //
    // umbra *owns* the shadow, so only the owner bits of the shadow apply to it.
    // Copying `0o555` verbatim gives a shadow directory the engine cannot create
    // inside; the next `create` fails EACCES, and `parents` runs inside `prepare`,
    // whose closure sets `poisoned` on any error. The session would be dead, with
    // no reconcile path -- for an operation POSIX itself allows, since writing
    // `d/f` needs write on the *file*, not on `d`.
    for mode in [0o555u32, 0o500, 0o511] {
        let mut f = Fixture::with_base_modes(&[(b"d/f", b"base bytes")], &[(b"d", mode)]);
        let action = f.overlay.resolve(&f.process, &write_open(b"d/f")).unwrap();
        let prepared = f
            .overlay
            .prepare(OperationId(Uuid::new_v4()), &action)
            .unwrap_or_else(|e| panic!("prepare under a {mode:o} base directory failed: {e:?}"));
        assert!(
            !f.overlay.poisoned,
            "a {mode:o} base directory must not poison the run"
        );
        f.complete(prepared);
        assert!(!f.overlay.poisoned);
        // The run still serves reads, and the copy-up really happened.
        assert_eq!(f.read(b"d/f").unwrap(), b"base bytes");
        assert_eq!(fs::read(shadow(&f, b"d/f")).unwrap(), b"base bytes");
        // The base is never modified to make this work.
        assert_eq!(mode_of(&f.base_root.join("d")), mode);
    }

    // The same through a chain and through Mkdir, which materialise ancestors by
    // other routes than copy-up.
    let mut f = Fixture::with_base_modes(
        &[(b"a/b/c/f", b"base bytes")],
        &[(b"a/b/c", 0o555), (b"a/b", 0o555), (b"a", 0o555)],
    );
    f.run(&write_open(b"a/b/c/f"));
    assert!(!f.overlay.poisoned, "a read-only chain must not poison");
    assert_eq!(f.read(b"a/b/c/f").unwrap(), b"base bytes");

    let mut f = Fixture::with_base_modes(&[(b"d/keep", b"base bytes")], &[(b"d", 0o555)]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"d/made"),
        mode: 0o755,
    });
    assert!(
        !f.overlay.poisoned,
        "mkdir under a read-only base must not poison"
    );
    assert!(shadow(&f, b"d/made").is_dir());

    // Owner-*execute* is as load-bearing as owner-write: without it the engine
    // cannot resolve a name inside the ancestor it just made. A base directory
    // with no owner-x cannot be used on disk -- the test process owns it and
    // could not traverse it either -- so report the mode through the base instead
    // of setting it, which is what `force_directory_mode` is for. `0o055` is also
    // the shape where the base is *less* permissive to its owner than to others.
    for (reported, expected) in [(0o444u32, 0o744u32), (0o055, 0o755)] {
        let creates = Creates::default();
        let mut f = Fixture::build(
            &[(b"d/f", b"base bytes")],
            Setup {
                creates: Some(creates.clone()),
                force_directory_mode: Some(reported),
                ..Setup::default()
            },
        );
        f.run(&write_open(b"d/f"));
        assert!(!f.overlay.poisoned);
        assert_eq!(
            requested_directory_mode(&creates, b"d"),
            Some(expected),
            "owner bits must be widened to rwx, not merely to rw"
        );
        assert_eq!(
            expected & 0o077,
            reported & 0o077,
            "widening must never touch group or other bits"
        );
        assert_eq!(f.read(b"d/f").unwrap(), b"base bytes");
    }
}

// Case (b).
#[test]
fn every_shadow_ancestor_in_a_chain_carries_its_own_base_directory_mode() {
    // The case a `parents(path, mode: u32)` signature structurally cannot serve:
    // one call materialises three ancestors that need three different modes, so
    // the mode has to be re-resolved per ancestor rather than handed in.
    let creates = Creates::default();
    let levels: [(&[u8], u32); 3] = [(b"a", 0o700), (b"a/b", 0o750), (b"a/b/c", 0o755)];
    let mut f = Fixture::build(
        &[(b"a/b/c/f", b"base bytes")],
        // Deepest first: each chmod still leaves owner rwx, so the walk can reach
        // the level below it.
        Setup {
            base_modes: &[(b"a/b/c", 0o755), (b"a/b", 0o750), (b"a", 0o700)],
            creates: Some(creates.clone()),
            ..Setup::default()
        },
    );
    f.run(&write_open(b"a/b/c/f"));
    for (path, mode) in levels {
        assert_eq!(
            requested_directory_mode(&creates, path),
            Some(mode),
            "each ancestor takes its own base counterpart's mode, not the chain's deepest or shallowest"
        );
        assert_eq!(mode_of(&shadow(&f, path)), mode & !umask());
    }
}

// Cases (c) and (d1).
#[test]
fn shadow_ancestors_keep_the_default_mode_when_the_base_holds_no_counterpart() {
    // (c) A base directory left at whatever `create_dir_all` produced is answered
    // with exactly that, so the ordinary case is unchanged. Comparing shadow
    // against base rather than against a literal keeps this umask-independent.
    let mut f = Fixture::new(&[(b"d/f", b"base bytes")]);
    f.run(&write_open(b"d/f"));
    assert_eq!(
        mode_of(&shadow(&f, b"d")),
        mode_of(&f.base_root.join("d")),
        "a default-mode base directory is still answered with its own mode"
    );

    // (d1) A path with no base counterpart at any level. The base answers
    // NotFound for `x` and for `x/y`, and that must not escape `parents`: the
    // ancestors are created at the default mode and the run continues.
    let creates = Creates::default();
    let mut f = Fixture::build(
        &[],
        Setup {
            creates: Some(creates.clone()),
            ..Setup::default()
        },
    );
    f.run(&create_open(b"x/y/z"));
    assert_eq!(requested_directory_mode(&creates, b"x"), Some(0o755));
    assert_eq!(requested_directory_mode(&creates, b"x/y"), Some(0o755));
    assert_eq!(fs::read(shadow(&f, b"x/y/z")).unwrap(), b"");
    // #54's shape is untouched: a truly-absent non-mutating resolve is still a
    // bare NotFound the supervisor may resume, never Deny(ENOENT).
    assert_eq!(
        f.overlay
            .resolve(&f.process, &stat(b"never"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

// Case (d2).
#[test]
fn a_kernel_refused_op_under_a_restricted_base_directory_reconciles_and_keeps_the_mode() {
    // #53/#59: a kernel refusal on a Materialise op reconciles instead of
    // poisoning, and that depends on `parents`'s bool -- and so on what reaches
    // `Pending.rollback` -- being what it always was. #56 changed the mode and nothing else, so the
    // refusal must still reconcile, and the ancestor `copy_up` materialised on
    // the way must carry the base's `0700` even though the tracee was told its
    // syscall failed.
    let mut f = Fixture::with_base_modes(&[(b"d/file", b"base bytes")], &[(b"d", 0o700)]);
    let prepared = f.prepare(&chown(b"d/file", Some(0), Some(0), true));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(EPERM))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(EPERM))
        .unwrap();
    assert!(
        !f.overlay.poisoned,
        "a kernel-refused chown under a materialised ancestor must still reconcile"
    );
    assert_eq!(mode_of(&shadow(&f, b"d")), 0o700 & !umask());
    // The session is still usable, which is the whole point of reconciling.
    assert_eq!(f.read(b"d/file").unwrap(), b"base bytes");
}

// Case (e).
#[test]
fn a_whiteouted_base_directory_does_not_lend_its_mode_to_its_replacement() {
    let creates = Creates::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes"), (b"e/f", b"base bytes")],
        Setup {
            base_modes: &[(b"d", 0o700), (b"e", 0o700)],
            creates: Some(creates.clone()),
            ..Setup::default()
        },
    );
    // `rmdir` whiteouts a directory since #77, but the markers stay
    // hand-written here on purpose: the subject is `parents` and
    // `shadow_parent_mode`, and writing them the way the engine stores them --
    // as `whiteout_hidden_resolves_to_enoent_while_truly_absent_stays_not_found`
    // does -- keeps this independent of the `FsOp::Unlink` arm, so a change
    // there cannot make this pass or fail for an unrelated reason.
    for name in [&b"d"[..], b"e"] {
        let marker = Overlay::marker(&root(name).unwrap()).unwrap();
        let path = f
            .control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"").unwrap();
    }
    // Reachable, not hypothetical: `walk` swallows a whiteouted ancestor's
    // NotFound under `create_parents`, and `absent(lookup(path))` swallows the
    // final component's, so the create goes through and `parents` materialises a
    // shadow `d` over the grave of the base's `d`. That is a new directory, not
    // the deleted one, so it must not wear the dead one's `0700`.
    f.run(&create_open(b"d/new"));
    assert_eq!(
        requested_directory_mode(&creates, b"d"),
        Some(0o755),
        "a whiteouted base directory is logically deleted; its mode must not be inherited"
    );

    // Latch-pollution guard (#49/#54). `shadow_parent_mode` probes the marker
    // directly rather than calling `whiteouted`, which sets `self.whiteout_hit`
    // -- the latch `hidden_or` consumes to turn a later NotFound into
    // Deny(ENOENT). `parents` is driven here directly and not through `f.run`
    // because `resolve` clears that latch on entry: going through an operation
    // would clear whatever `parents` had set during the preceding prepare/commit
    // and could not discriminate. That reset is also why the end-to-end
    // assertion below holds either way -- this direct one is the discriminating
    // check.
    f.overlay.whiteout_hit = false;
    assert_eq!(
        f.overlay.parents(&root(b"e/new").unwrap()).unwrap(),
        vec![root(b"e").unwrap()],
        "the shadow ancestor was missing, so parents reports having created it"
    );
    assert!(
        !f.overlay.whiteout_hit,
        "parents must not latch whiteout_hit: hidden_or would turn an unrelated later NotFound into Deny(ENOENT)"
    );
    assert_eq!(requested_directory_mode(&creates, b"e"), Some(0o755));
    assert_eq!(
        f.overlay
            .resolve(&f.process, &stat(b"never"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

// Case (f).
#[test]
fn control_anchored_shadow_ancestors_never_consult_the_base() {
    // `write_control` (symlink target and index blobs) and `set_whiteout`
    // (whiteout markers) drive `parents` over Control-anchored paths. `Base` is
    // Root-anchored only -- `lookup` calls anything else Denied -- so those
    // ancestors must never reach it. Without the anchor guard this fires on every
    // FsOp::Symlink and every FsOp::Unlink.
    let stats = BaseStats::default();
    let mut f = Fixture::build(
        &[(b"gone", b"base bytes")],
        Setup {
            base_stats: Some(stats.clone()),
            ..Setup::default()
        },
    );
    f.run(&symlink(b"a/link", b"/gone"));
    f.run(&unlink(b"gone"));
    let asked = stats.lock().unwrap().clone();
    let control: Vec<_> = asked
        .iter()
        .filter(|p| p.anchor() != StorageAnchor::Root)
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect();
    assert!(
        control.is_empty(),
        "the base was consulted for control paths: {control:?}"
    );
    assert!(
        !asked.is_empty(),
        "the base is still consulted for root paths, so this is not vacuous"
    );
}

// The two shapes under (a) that no operation can drive end to end, plus the mask.
// Also the row-by-row pin on the creation verdict `parents` folds into
// `Pending.rollback` (#55): the mode rounds a missing answer *down* to the
// default, the verdict rounds the same non-answer *up* to "a logical creation, so
// record an undo for it", and only a corroborated live base directory answers
// benign.
#[test]
fn a_non_directory_or_absent_base_ancestor_falls_back_instead_of_inheriting() {
    let mut f = Fixture::with_base_modes(&[(b"d/f", b"base bytes")], &[(b"d", 0o700)]);
    // `walk` rejects a non-directory ancestor before `parents` ever runs, so
    // these are exercised directly. The guards are still load-bearing: a `Base`
    // whose tree disagrees with the walk's view must never put a file's mode on a
    // directory, and must never propagate its own error out of `parents`.
    let mut whiteout = WhiteoutScan::Unscanned;
    assert_eq!(
        f.overlay
            .shadow_parent_mode(&root(b"d").unwrap(), &mut whiteout)
            .unwrap(),
        ShadowAncestor {
            mode: 0o700,
            shadows_base_directory: true,
        },
        "a live base directory is the one arm that is not a logical creation"
    );
    let mut whiteout = WhiteoutScan::Unscanned;
    assert_eq!(
        f.overlay
            .shadow_parent_mode(&root(b"d/f").unwrap(), &mut whiteout)
            .unwrap(),
        ShadowAncestor {
            mode: 0o755,
            shadows_base_directory: false,
        },
        "a file's mode must not be adopted for a directory, nor its existence \
         read as a directory this shadows"
    );
    let mut whiteout = WhiteoutScan::Unscanned;
    assert_eq!(
        f.overlay
            .shadow_parent_mode(&root(b"nowhere").unwrap(), &mut whiteout)
            .unwrap(),
        ShadowAncestor {
            mode: 0o755,
            shadows_base_directory: false,
        },
        "an absent base ancestor falls back rather than propagating NotFound"
    );
    // The remaining row of the verdict table, reached by handing the walk a scan
    // that has already found a marker above this ancestor. The base directory is
    // there and legible; it is *logically deleted*, so the shadow materialised
    // over its grave is a different directory and wears neither its mode nor its
    // existence.
    let mut whiteout = WhiteoutScan::Hidden;
    assert_eq!(
        f.overlay
            .shadow_parent_mode(&root(b"d").unwrap(), &mut whiteout)
            .unwrap(),
        ShadowAncestor {
            mode: 0o755,
            shadows_base_directory: false,
        },
        "a whiteouted base directory is not something a shadow can be said to shadow"
    );

    // `& 0o7777`: `LocalStorage` masks its own stat (`lib.rs:110`), so only a
    // base that reports a raw `st_mode` can exercise the engine's mask. Without
    // it the mode carries S_IFDIR, `LocalStorage` rejects the create outright
    // (`mode & !0o7777 != 0`), and a fidelity bug becomes a failed run.
    let creates = Creates::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes")],
        Setup {
            base_modes: &[(b"d", 0o700)],
            creates: Some(creates.clone()),
            force_directory_mode: Some(0o040700),
            ..Setup::default()
        },
    );
    f.run(&write_open(b"d/f"));
    assert_eq!(requested_directory_mode(&creates, b"d"), Some(0o700));
}

// Case (h): the whiteouted ancestor is already in the shadow.
#[test]
fn a_whiteouted_ancestor_already_in_the_shadow_still_blocks_inheritance() {
    // `parents` skips ancestors that already exist in the shadow, so those never
    // reach `shadow_parent_mode` and a scan that only looked at the ancestors it
    // was handed would never see a marker above them. The shape is not
    // hypothetical: `prepare`'s `FsOp::Mkdir` arm deliberately keeps an existing
    // directory whiteout as an opaque-base marker, so after whiteout-then-recreate
    // the shadow directory `a` and `a`'s marker coexist. Everything the base holds
    // under `a` is logically deleted, and none of it may lend its mode.
    let creates = Creates::default();
    let mut f = Fixture::build(
        &[(b"a/b/c/f", b"base bytes")],
        Setup {
            base_modes: &[(b"a/b/c", 0o777), (b"a/b", 0o700), (b"a", 0o750)],
            creates: Some(creates.clone()),
            ..Setup::default()
        },
    );
    let marker = Overlay::marker(&root(b"a").unwrap()).unwrap();
    let path = f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"").unwrap();
    // Recreate `a` in the shadow. The marker survives, which is the point.
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"a"),
        mode: 0o755,
    });
    assert!(path.exists(), "mkdir must keep the opaque-base marker");
    assert!(shadow(&f, b"a").is_dir());
    creates.lock().unwrap().clear();

    // `a` is now skipped by `parents` -- present in the shadow -- so `a/b` is the
    // first ancestor `shadow_parent_mode` is asked about, and the marker that
    // hides it sits above it.
    f.run(&create_open(b"a/b/c/f"));
    for name in [&b"a/b"[..], b"a/b/c"] {
        assert_eq!(
            requested_directory_mode(&creates, name),
            Some(0o755),
            "an ancestor under a whiteouted prefix must not inherit the dead base mode"
        );
    }
    // The `0o777` level is the one that shows why this is not cosmetic: inherited,
    // it would be a world-writable shadow directory taken from a directory the
    // tracee deleted. `LocalStorage` masks it to `0o755` via umask, so the
    // requested mode above is the assertion that sees it; a backend honouring
    // `CreateOptions.mode` as written would materialise it.
    assert!(
        !creates
            .lock()
            .unwrap()
            .iter()
            .any(|(_, o)| o.kind == CreateKind::Directory && o.mode == 0o777),
        "no shadow directory may be requested world-writable from a dead base"
    );
    // Still non-latching (#49/#54), and the run is still usable.
    f.overlay.whiteout_hit = false;
    f.overlay.parents(&root(b"a/b/c/other").unwrap()).unwrap();
    assert!(!f.overlay.whiteout_hit, "the prefix scan must not latch");
    assert_eq!(
        f.overlay
            .resolve(&f.process, &stat(b"never"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

// Case (i): the blanket base-error swallow.
#[test]
fn a_failing_base_stat_falls_back_to_the_default_mode_instead_of_poisoning() {
    // `shadow_parent_mode` swallows *every* base-stat error, not just NotFound.
    // Deliberate: `parents` runs inside `prepare` and `commit`, so a base that
    // fails there would turn a create that succeeds today into a poisoned run and
    // take #53/#59's kernel-refusal reconciliation with it -- on a path that
    // never consulted the base at all before #56. The cost of swallowing is one
    // directory's mode fidelity, degrading to exactly the pre-#56 value and never
    // to a wrong non-default one. This pins the non-NotFound half of that arm,
    // which nothing else exercises.
    let creates = Creates::default();
    let failure = BaseFailure::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes")],
        Setup {
            base_modes: &[(b"d", 0o700)],
            creates: Some(creates.clone()),
            fail_directory_stat: Some(failure.clone()),
            ..Setup::default()
        },
    );
    // Resolve against a healthy base, then fail only the stat `parents` makes.
    let action = f.overlay.resolve(&f.process, &write_open(b"d/f")).unwrap();
    *failure.lock().unwrap() = Some(ErrorKind::Io);
    let prepared = f
        .overlay
        .prepare(OperationId(Uuid::new_v4()), &action)
        .expect("a failing base stat must not fail prepare");
    assert_eq!(
        requested_directory_mode(&creates, b"d"),
        Some(0o755),
        "an unreadable base ancestor falls back to the default, not to an error"
    );
    assert!(!f.overlay.poisoned);
    // The transaction still completes and the session stays usable.
    f.complete(prepared);
    assert!(!f.overlay.poisoned);
    *failure.lock().unwrap() = None;
    assert_eq!(f.read(b"d/f").unwrap(), b"base bytes");
}

// The other half of case (i). The swallow still applies to the *mode* only:
// `shadow_parent_mode` rounds an illegible base down to the default mode so
// `prepare` cannot fail, and still reports `shadows_base_directory = false` for
// the same non-answer, so `d` is treated as materialised over nothing.
//
// What that verdict *costs* is what [#64](https://github.com/invakid404/umbra/issues/64)
// changed, and the change is a retirement rather than a reversal. The row read
// "fail closed" because a directory could not be removed, so the engine had to
// guess what the base held and had to guess the expensive way: a wrong guess
// towards "benign" publishes a phantom path that answers `AlreadyExists`
// forever. With the removal available the guess is unnecessary under *both*
// readings -- if the base does hold a directory at `d` the shadow was a benign
// uncopied shadow and removing it is harmless; if it holds nothing the shadow
// was a phantom and removing it is required -- so the evidence row stops having
// a consequence instead of acquiring the opposite one. #64's body does not name
// this test; it flips all the same.
//
// The companion `a_failing_base_stat_falls_back_to_the_default_mode_instead_of_poisoning`
// above is untouched: the mode half of the swallow is exactly as it was, and
// what collapses is only the swallow-for-mode / fail-closed-for-creation
// asymmetry the pair used to document.
#[test]
fn a_creating_open_under_an_illegible_base_ancestor_is_rolled_back_rather_than_guessed() {
    let failure = BaseFailure::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes")],
        Setup {
            fail_directory_stat: Some(failure.clone()),
            ..Setup::default()
        },
    );
    // Resolve against a healthy base, then fail only the stat `parents` makes,
    // exactly as the mode half above does.
    let action = f
        .overlay
        .resolve(&f.process, &create_open(b"d/fresh"))
        .unwrap();
    *failure.lock().unwrap() = Some(ErrorKind::Io);
    let prepared = f
        .overlay
        .prepare(OperationId(Uuid::new_v4()), &action)
        .expect("a failing base stat must not fail prepare");
    assert!(!f.overlay.poisoned);
    assert!(f.shadow_root.join("d").is_dir());
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(ENOSPC))
        .unwrap();
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert!(
        !f.shadow_root.join("d").exists(),
        "an ancestor materialised on evidence that never arrived is removed, not kept"
    );
    // The base is untouched by any of it, so the reading under which `d` was a
    // benign shadow loses nothing: with the injection cleared, `d/f` reads back.
    *failure.lock().unwrap() = None;
    assert_eq!(f.read(b"d/f").unwrap(), b"base bytes");
}

// `FsOp::Unlink { directory: true }` (#77). The arm refused with "rmdir awaits
// backend directory removal support" until here, on a premise that had been
// false since #64 gave `LocalStorage` its `RemoveDirectory` arm --
// `Overlay::abort` has been calling `storage.remove_directory` on the reconcile
// path throughout, pinned by
// `a_refused_mkdir_rolls_back_the_directory_it_created`. A directory unlink is
// `Dispatch::Whiteout` like a file one and inherits #66's contract verbatim:
// `prepare` records a `Pending::destroy` plan, `commit` performs it beside the
// marker, an abort destroys nothing because nothing was created. The only
// directory-specific addition is the emptiness gate, and it is taken against the
// *merged* view. Three tests own that between them, and which one owns which
// direction matters: `a_base_directory_whose_children_were_all_unlinked_rmdirs_as_empty`
// and `an_opaque_base_directory_rmdirs_on_its_shadow_contents_alone` are the two
// cases that point opposite ways, and both falsify a predicate that consults the
// base listing separately -- but both assert *success*, so neither can catch a
// predicate that is too permissive. `a_rmdir_counts_base_and_shadow_children_alike`
// is the one that asserts refusals, and it is what falsifies a shadow-half-only
// check.
//
// Honest about reachability, in the idiom the `Mkdir` and file-`Unlink` blocks
// above already carry: `resolve` answers `Emulate` for `Unlink`, so the
// supervisor refuses it at entry and `kernel_refusal.rs` can carry no sibling
// for any of this. `Overlay::abort` and `Overlay::commit` are namespace APIs
// this suite calls directly. The refusals seed the outcome through
// `observe_emulated_refusal`, so none of these assert on journal *shape*.
//
// The gate's refusal is an `Err(Denied)`, not an `Emulate(Failure(ENOTEMPTY))`:
// the supervisor refuses an `Emulate` only after `prepare` has journaled, and
// the engine has no view of the tracee ABI in which `ENOTEMPTY` is 39 on Linux
// and 66 on macOS/BSD. See the arm's own comment.

// Case 1. The whole plan end to end on the simplest shape: a directory this run
// created, with nothing in the base behind it.
#[test]
fn an_empty_shadow_only_directory_rmdir_removes_it_and_leaves_a_marker() {
    let mut f = Fixture::new(&[]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"fresh"),
        mode: 0o755,
    });
    assert!(f.shadow_root.join("fresh").is_dir());
    f.run(&rmdir(b"fresh"));
    assert!(
        !f.shadow_root.join("fresh").exists(),
        "commit performs the remove_directory the plan recorded"
    );
    let marker = Overlay::marker(&root(b"fresh").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    assert_eq!(
        f.overlay.resolve(&f.process, &stat(b"fresh")).unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
    assert!(f
        .overlay
        .list(&root(b"").unwrap(), None, 10)
        .unwrap()
        .entries
        .is_empty());
}

// Case 2. Nothing of this directory was ever in the shadow, so `commit`'s
// `shadow_stat` re-probe skips the destroy entirely and the marker is the whole
// effect. The base directory is immutable and must come through untouched.
#[test]
fn an_empty_base_only_directory_rmdir_marks_it_and_destroys_nothing() {
    let mut f = Fixture::new(&[]);
    // An *empty* base directory: the fixture's file list can only make
    // directories as a side effect of the files under them.
    fs::create_dir(f.base_root.join("empty")).unwrap();
    f.run(&rmdir(b"empty"));
    assert!(
        f.base_root.join("empty").is_dir(),
        "the base is immutable; hiding it is the whole of the effect"
    );
    assert!(!f.shadow_root.join("empty").exists());
    let marker = Overlay::marker(&root(b"empty").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    // #49: the base object is still on the host, so a NotFound the supervisor
    // would resume as passthrough must not escape -- it denies with ENOENT.
    assert_eq!(
        f.overlay.resolve(&f.process, &stat(b"empty")).unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
}

// Case 2b, and one of the two tests that decide the gate's predicate. The base
// listing still reports both children; the merged view does not, because every
// name carries a whiteout. A predicate that consults the base list separately
// refuses here -- and `rm d/* && rmdir d` is the single most common real
// sequence there is.
//
// It does **not** discriminate against a shadow-half-only predicate, and saying
// so is the point rather than an omission. `d`'s children are base-only, so a
// shadow-only check sees no shadow directory at all, calls that empty, agrees
// with the assertion below and permits the rmdir. No test that asserts success
// can falsify a predicate whose error is being too *permissive*. That direction
// needs a refusal to assert against while an un-unlinked child is still there,
// which is what `a_rmdir_counts_base_and_shadow_children_alike` owns.
#[test]
fn a_base_directory_whose_children_were_all_unlinked_rmdirs_as_empty() {
    let mut f = Fixture::new(&[(b"d/a", b"a bytes"), (b"d/b", b"b bytes")]);
    f.run(&unlink(b"d/a"));
    f.run(&unlink(b"d/b"));
    assert!(
        f.overlay
            .list(&root(b"d").unwrap(), None, 10)
            .unwrap()
            .entries
            .is_empty(),
        "the merged view is what the tracee can observe, and it is empty"
    );
    assert!(
        f.base_root.join("d/a").is_file() && f.base_root.join("d/b").is_file(),
        "the base still holds both, which is exactly why a base-shaped check fails"
    );
    f.run(&rmdir(b"d"));
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    assert!(f.base_root.join("d").is_dir());
    assert!(f
        .overlay
        .list(&root(b"").unwrap(), None, 10)
        .unwrap()
        .entries
        .is_empty());
}

// Case 3b, the other deciding test, and it points the opposite way: the base
// directory is *non-empty and unmarked* underneath, and the rmdir must still
// succeed, because the marker left standing by the earlier rmdir makes the
// directory opaque and `merged` skips the base enumeration outright. A predicate
// that consulted the base would refuse a directory the tracee has just observed
// as empty.
#[test]
fn an_opaque_base_directory_rmdirs_on_its_shadow_contents_alone() {
    let mut f = Fixture::new(&[(b"d/f", b"base bytes")]);
    f.run(&unlink(b"d/f"));
    f.run(&rmdir(b"d"));
    // Written *after* the marker, so it carries no whiteout of its own and
    // `Base::list` reports it. That is what makes this decisive: a predicate
    // that consulted the base listing separately sees a non-empty directory and
    // refuses. The merged view hides it either way -- the opaque-base gate skips
    // the base enumeration outright, and `whiteouted`'s prefix scan would filter
    // the name under the surviving marker regardless.
    fs::write(f.base_root.join("d/late"), b"late bytes").unwrap();
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"d"),
        mode: 0o755,
    });
    assert!(f.shadow_root.join("d").is_dir());
    assert!(
        f.overlay
            .list(&root(b"d").unwrap(), None, 10)
            .unwrap()
            .entries
            .is_empty(),
        "the recreated directory is opaque: the base half is not enumerated at all"
    );
    // `set_whiteout` is idempotent, so re-marking the already-opaque directory
    // is a no-op rather than a double create.
    f.run(&rmdir(b"d"));
    assert!(!f.shadow_root.join("d").exists());
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .is_file());
    assert!(f.base_root.join("d/late").is_file());
}

// Cases 3 and 4. One visible child is enough to refuse, wherever it lives, and
// the refusal is a gate rather than a latch: remove the child and the retried
// rmdir commits.
#[test]
fn a_rmdir_counts_base_and_shadow_children_alike() {
    let mut f = Fixture::new(&[(b"baseonly/child", b"base"), (b"both/child", b"base")]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"shadowonly"),
        mode: 0o755,
    });
    f.run(&create_open(b"shadowonly/child"));
    // A base child copied into the shadow: visible through both halves at once,
    // and the merged view must not double-count or miss it.
    f.run(&open(
        b"both/child",
        OpenFlags {
            read: true,
            write: true,
            ..Default::default()
        },
    ));
    assert!(f.shadow_root.join("both/child").is_file());
    for name in [&b"baseonly"[..], b"shadowonly", b"both"] {
        let failure = f.overlay.resolve(&f.process, &rmdir(name)).unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Denied);
        assert_eq!(failure.context, "rmdir target is not empty");
        let child = [name, b"/child"].concat();
        f.run(&unlink(&child));
        f.run(&rmdir(name));
        let marker = Overlay::marker(&root(name).unwrap()).unwrap();
        assert!(f
            .control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
            .is_file());
        assert!(!f
            .shadow_root
            .join(std::ffi::OsStr::from_bytes(name))
            .exists());
    }
    assert!(f
        .overlay
        .list(&root(b"").unwrap(), None, 10)
        .unwrap()
        .entries
        .is_empty());
}

// Why the gate is at `resolve` and not at `prepare`, in the shape
// `a_base_only_directory_chown_is_refused_before_anything_is_journaled`
// established for `FsOp::Fchownat`. `prepare` appends and flushes the Unlink
// intent before the arm bodies run, and any error after that poisons -- so a
// prepare-time refusal would leave a durable record of an rmdir that never
// happened, with neither Commit nor Abort, for what is an ordinary POSIX
// outcome. Refusing at `resolve` costs the run nothing at all.
#[test]
fn a_non_empty_rmdir_is_refused_before_anything_is_journaled() {
    let mut f = Fixture::new(&[]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"d"),
        mode: 0o755,
    });
    f.run(&create_open(b"d/child"));
    let before = f.log.lock().unwrap().records.len();
    let failure = f.overlay.resolve(&f.process, &rmdir(b"d")).unwrap_err();
    assert_eq!(failure.kind, ErrorKind::Denied);
    assert_eq!(failure.context, "rmdir target is not empty");
    assert_eq!(
        f.log.lock().unwrap().records.len(),
        before,
        "a refusal at resolve journals nothing"
    );
    assert!(!f.overlay.poisoned);
    assert!(f.overlay.pending.is_none());
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(!f
        .control
        .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
        .exists());
    assert!(f.shadow_root.join("d").is_dir());
    // The session is still usable, which is the whole of the improvement over
    // the refusal this replaced: that one ended the run.
    assert_eq!(f.read(b"d/child").unwrap(), b"");
    f.run(&unlink(b"d/child"));
    f.run(&rmdir(b"d"));
    assert!(!f.shadow_root.join("d").exists());
}

// The end-to-end pin the "Keep an existing directory whiteout as an opaque-base
// marker" comment in `prepare`'s `FsOp::Mkdir` arm has never had. The marker
// that survives the recreation is load-bearing in both directions: `lookup`
// consults the shadow first, so the recreated directory outranks it and is
// visible; `merged` consults it, so the base children stay hidden.
#[test]
fn rmdir_then_mkdir_keeps_the_marker_so_base_contents_stay_hidden() {
    let mut f = Fixture::new(&[(b"d/old", b"base bytes")]);
    f.run(&unlink(b"d/old"));
    f.run(&rmdir(b"d"));
    // As in `an_opaque_base_directory_rmdirs_on_its_shadow_contents_alone`: an
    // unmarked base child, so the hiding below is the surviving marker's doing
    // and not a per-name whiteout the unlink above left behind.
    fs::write(f.base_root.join("d/hidden"), b"still on the host").unwrap();
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"d"),
        mode: 0o755,
    });
    f.run(&create_open(b"d/new"));
    assert_eq!(
        f.overlay
            .list(&root(b"d").unwrap(), None, 10)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.name.as_bytes().to_vec())
            .collect::<Vec<_>>(),
        vec![b"new".to_vec()],
        "the recreated directory shows its own contents, not the base's"
    );
    assert_eq!(
        f.overlay.resolve(&f.process, &stat(b"d/hidden")).unwrap(),
        ResolvedAction::Deny(Errno::ENOENT)
    );
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(
        f.control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
            .is_file(),
        "the mkdir arm keeps the marker; consuming it would expose the base"
    );
    // The directory itself is reachable, so the marker hides the base without
    // hiding the recreation.
    assert_eq!(f.read(b"d/new").unwrap(), b"");
    assert_eq!(
        fs::read(f.base_root.join("d/hidden")).unwrap(),
        b"still on the host"
    );
}

// `merged`'s per-name whiteout filter, from the parent's side: the rmdir'd name
// leaves the listing its parent answers, and its unrelated sibling does not.
#[test]
fn a_rmdir_removes_the_name_from_its_parents_merged_listing() {
    let mut f = Fixture::new(&[(b"p/keep", b"base bytes")]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"p/gone"),
        mode: 0o755,
    });
    let names = |f: &mut Fixture| {
        f.overlay
            .list(&root(b"p").unwrap(), None, 10)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.name.as_bytes().to_vec())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&mut f), vec![b"gone".to_vec(), b"keep".to_vec()]);
    f.run(&rmdir(b"p/gone"));
    assert_eq!(names(&mut f), vec![b"keep".to_vec()]);
}

// The poison mirror, directory-shaped. Sibling of
// `an_unlink_prepare_destroys_nothing_before_commit`: the destruction is a plan,
// and the phase that performs it is `commit`.
#[test]
fn an_rmdir_prepare_destroys_nothing_before_commit() {
    let mut f = Fixture::new(&[]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"fresh"),
        mode: 0o755,
    });
    let prepared = f.prepare(&rmdir(b"fresh"));
    assert!(
        f.shadow_root.join("fresh").is_dir(),
        "prepare records a plan; it performs no destruction"
    );
    assert!(
        !f.control.join("whiteouts").exists(),
        "the marker is a commit-time effect too"
    );
    f.complete(prepared);
    assert!(
        !f.shadow_root.join("fresh").exists(),
        "commit is where the destruction happens"
    );
}

// Sibling of `a_refused_unlink_leaves_the_shadow_object_and_its_bytes_standing`,
// and the reason the destruction had to be a plan: `Overlay::abort`'s gate is
// corroboration alone, unchanged by this work, and a reconciled abort of an
// rmdir must leave the directory and everything it was hiding exactly as it
// found them. Both shapes are driven, because they lose different things: a
// shadow-only directory has no second copy anywhere in the run, and a base
// directory's children would be hidden by a marker that should never have been
// written.
#[test]
fn a_refused_rmdir_leaves_the_directory_and_its_contents_standing() {
    let mut f = Fixture::new(&[(b"d/a", b"base bytes")]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"fresh"),
        mode: 0o755,
    });
    let prepared = f.prepare(&rmdir(b"fresh"));
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert!(f.shadow_root.join("fresh").is_dir());
    assert!(
        !f.control.join("whiteouts").exists(),
        "the whiteout is a commit-time plan a reconciled abort never applied"
    );
    // The base-backed half. `d` is empty in the merged view only because its one
    // child is whiteouted, so the child's bytes are what a premature destroy or
    // a prematurely written marker would take with it.
    f.run(&unlink(b"d/a"));
    let prepared = f.prepare(&rmdir(b"d"));
    f.observe_emulated_refusal(ENOSPC);
    f.overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap();
    assert!(!f.overlay.poisoned);
    assert!(f.base_root.join("d").is_dir());
    assert_eq!(fs::read(f.base_root.join("d/a")).unwrap(), b"base bytes");
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(
        !f.control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
            .exists(),
        "no marker on the directory: an aborted rmdir hides nothing"
    );
    // And the tracee's view is what it was: the directory is still listable, and
    // `fresh` is still named by the root.
    assert!(f
        .overlay
        .list(&root(b"d").unwrap(), None, 10)
        .unwrap()
        .entries
        .is_empty());
    assert_eq!(
        f.overlay
            .list(&root(b"").unwrap(), None, 10)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.name.as_bytes().to_vec())
            .collect::<Vec<_>>(),
        vec![b"d".to_vec(), b"fresh".to_vec()]
    );
}

// The arm a real session's rmdir abort would take, and the corroboration gate
// holding: `resolve` answers `Emulate(Success)`, `observe_result` refuses any
// differing outcome, so the claimed errno cannot match the observed one and the
// abort poisons. Sibling of
// `an_uncorroborated_unlink_abort_poisons_and_still_destroys_nothing` -- and
// poisoning is still not a licence to have destroyed something first.
#[test]
fn an_uncorroborated_rmdir_abort_poisons_and_still_destroys_nothing() {
    let mut f = Fixture::new(&[]);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"fresh"),
        mode: 0o755,
    });
    let before = f.log.lock().unwrap().records.len();
    let prepared = f.prepare(&rmdir(b"fresh"));
    f.observe_emulated_refusal(EPERM);
    let failure = f
        .overlay
        .abort(prepared.operation_id, &AbortReason::KernelRefused(ENOSPC))
        .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::InvalidState);
    assert_eq!(failure.context, "aborted effects require reconciliation");
    assert!(f.overlay.poisoned);
    assert!(f.shadow_root.join("fresh").is_dir());
    assert!(!f.control.join("whiteouts").exists());
    // #53: every mutating abort journals its reason, poison arm included.
    // Asserted by presence, not position -- the seeded outcome appends no
    // `ObservedResult`, so these records are one shorter than production's.
    let payloads = f.log.lock().unwrap().records[before..].to_vec();
    assert!(payloads.iter().any(
        |r| matches!(&r.payload, JournalPayload::Abort { reason } if reason.contains("KernelRefused"))
    ));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "a poisoned rmdir must not commit"
    );
}

// The `Destroy::Directory` arm's own failure, and the marker-before-destroy
// ordering it inherits. Sibling of
// `an_unlink_commit_that_cannot_unlink_the_object_poisons_with_the_backends_own_kind`,
// with `fail_remove_directory` flipped after `prepare` for the same reason.
//
// The residue is where the file argument stops transferring, and this asserts
// the difference rather than the likeness. For a file, marker-first leaves the
// pre-transaction view: the shadow object outranks its own whiteout in `lookup`,
// and a file has no contents for the marker to suppress. For a directory the
// marker also makes `merged` skip the base half, so what recovery finds is "the
// directory is still there, but its base children are hidden" -- not the
// pre-transaction view. The order is still right: reversing it would let a
// *successful* destroy followed by a failed `set_whiteout` expose the entire
// base directory at a path the tracee was told it had removed, and lying to a
// live tracee beats an imperfect residue in an already-poisoned session whose
// only reader is recovery.
#[test]
fn an_rmdir_commit_that_cannot_remove_the_directory_poisons_with_the_backends_own_kind() {
    let failure = RemoveDirectoryFailure::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes")],
        Setup {
            fail_remove_directory: Some(failure.clone()),
            ..Setup::default()
        },
    );
    f.run(&unlink(b"d/f"));
    // A shadow `d` over the base `d`, so there is something for the destroy to
    // fail on and base children for the marker to hide.
    f.run(&create_open(b"d/tmp"));
    f.run(&unlink(b"d/tmp"));
    assert!(f.shadow_root.join("d").is_dir());
    let prepared = f.prepare(&rmdir(b"d"));
    let id = prepared.operation_id;
    f.overlay
        .observe_result(id, &OperationOutcome::Success { return_value: 0 })
        .unwrap();
    // Flipped after `prepare` -- which destroys nothing now -- so only the
    // commit-time removal is refused.
    *failure.lock().unwrap() = Some(ErrorKind::Io);
    assert_eq!(
        f.overlay.commit(id).unwrap_err().kind,
        ErrorKind::Io,
        "a failed destruction reports the backend's error, not the engine's"
    );
    assert!(f.overlay.poisoned);
    assert!(
        f.shadow_root.join("d").is_dir(),
        "the directory the destruction could not remove is what recovery finds"
    );
    let marker = Overlay::marker(&root(b"d").unwrap()).unwrap();
    assert!(
        f.control
            .join(std::ffi::OsStr::from_bytes(marker.as_bytes()))
            .is_file(),
        "the marker went first, and for a directory that means the base children \
         are hidden while the directory itself still stands"
    );
    assert!(f.base_root.join("d/f").is_file());
}

// ---------------------------------------------------------------------------
// Ownership fidelity on materialised shadow objects
// ([#60](https://github.com/invakid404/umbra/issues/60),
// [#61](https://github.com/invakid404/umbra/issues/61)).
//
// `CreateOptions` carries a mode and no uid/gid, so the engine emits a
// `SetMetadata` after each of its two `create` sites and the shadow object takes
// the ownership of the base object it shadows. Where there is no base
// counterpart there is no carry; where privilege refuses, the object keeps
// umbra's ownership and the run continues.
//
// Why these assert on the emitted `SetMetadata` rather than on `st_uid`: CI runs
// as one user, so the base tree is already owned by the test process and a
// *successful* carry leaves the shadow object's uid exactly where a *missing*
// carry would have left it. The request is the only thing that distinguishes
// them. `Metadata` says the same thing at more length.
// ---------------------------------------------------------------------------

/// The carry requests for `path`, in order.
fn carried(metadata: &Metadata, path: &[u8]) -> Vec<MetadataUpdate> {
    metadata
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.anchor() == StorageAnchor::Root && p.as_bytes() == path)
        .map(|(_, update)| update.clone())
        .collect()
}
/// Every path a carry was requested for, in order.
fn carried_paths(metadata: &Metadata) -> Vec<String> {
    metadata
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect()
}
/// The uid/gid the base reports for `name`, which is what a carry must name.
fn base_owner(f: &mut Fixture, name: &[u8]) -> (u32, u32) {
    let stat = f.overlay.base().stat(&root(name).unwrap()).unwrap();
    (stat.uid, stat.gid)
}
fn with_metadata(files: &[(&[u8], &[u8])], metadata: &Metadata) -> Fixture {
    Fixture::build(
        files,
        Setup {
            metadata: Some(metadata.clone()),
            ..Setup::default()
        },
    )
}

// (a) `parents()` -- #60. Directory targets.

// δ step 3, the negative half: an ancestor with no base counterpart is a *new*
// directory, not a shadow of anything, so there is nothing whose ownership it
// could take and umbra's is simply correct.
#[test]
fn a_shadow_only_ancestor_takes_no_base_ownership() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"elsewhere", b"base bytes")], &metadata);
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"fresh/deep/dir"),
        mode: 0o750,
    });
    assert!(f.shadow_root.join("fresh/deep/dir").is_dir());
    assert!(!f.base_root.join("fresh").exists());
    assert!(
        carried_paths(&metadata).is_empty(),
        "no ancestor here shadows a base directory, so nothing may be chowned: {:?}",
        carried_paths(&metadata)
    );
}

// δ step 1, at `parents`. The one arm the base corroborates is the one arm that
// carries, and it carries the base directory's own uid/gid -- not a constant,
// and not the object's.
#[test]
fn a_base_only_ancestor_carries_the_base_directorys_ownership() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"d/f", b"base bytes")], &metadata);
    let owner = base_owner(&mut f, b"d");
    f.run(&open(
        b"d/new",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("d/new").is_file());
    assert_eq!(
        carried(&metadata, b"d"),
        vec![MetadataUpdate {
            mode: None,
            uid: Some(owner.0),
            gid: Some(owner.1),
            accessed_nanos: None,
            modified_nanos: None,
        }],
        "the materialised ancestor takes the base directory's ownership, and \
         names only ownership -- the mode was already settled by `create`"
    );
    assert_eq!(
        carried_paths(&metadata),
        vec!["d".to_owned()],
        "the created object itself is not a shadow of a base object and must not \
         be carried onto"
    );
}

// The existing-shadow branch must not grow a side effect. Re-chowning an
// ancestor the run has already materialised would overwrite ownership the tracee
// may have set on it deliberately.
#[test]
fn an_ancestor_already_in_the_shadow_is_not_re_owned() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"d/f", b"base bytes")], &metadata);
    let creating = |name: &[u8]| {
        open(
            name,
            OpenFlags {
                create: true,
                write: true,
                ..OpenFlags::default()
            },
        )
    };
    f.run(&creating(b"d/one"));
    assert_eq!(carried_paths(&metadata), vec!["d".to_owned()]);
    f.run(&creating(b"d/two"));
    assert_eq!(
        carried_paths(&metadata),
        vec!["d".to_owned()],
        "the second walk skips the ancestor it found in the shadow, so it must \
         not chown it a second time"
    );
}

// The grave case. `shadow_parent_mode` already refuses to let the shadow wear a
// whiteouted base directory's *mode*, on the grounds that the directory over its
// grave is a different directory; its ownership follows the same rule, and for
// the same reason.
#[test]
fn an_ancestor_over_a_whiteouted_base_directory_carries_no_ownership() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"d/f", b"base bytes")], &metadata);
    f.run(&unlink(b"d/f"));
    f.run(&rmdir(b"d"));
    metadata.lock().unwrap().clear();
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"d/fresh"),
        mode: 0o750,
    });
    assert!(f.shadow_root.join("d/fresh").is_dir());
    assert!(
        carried_paths(&metadata).is_empty(),
        "a directory materialised over a whiteouted base directory is a new \
         directory and must no more wear the dead one's uid than its mode: {:?}",
        carried_paths(&metadata)
    );
}

// Axis 6, pinned. The constant is *more* load-bearing after a carry, not less:
// once the ancestor wears the base's uid, umbra is no longer its owner, so the
// `other` bits would govern umbra's own writes into it. A `0o555` base directory
// carried without the widening leaves the next `create` inside it with `r-x`.
//
// Also pins that this PR neither widens nor narrows the mode divergence #56
// documented: the ancestor still reads back `0o755` against the usual umask,
// exactly as it did before ownership carry existed.
#[test]
fn shadow_owner_bits_survive_the_ownership_carry() {
    let metadata = Metadata::default();
    let creates = Creates::default();
    let mut f = Fixture::build(
        &[(b"d/f", b"base bytes")],
        Setup {
            base_modes: &[(b"d", 0o555)],
            creates: Some(creates.clone()),
            metadata: Some(metadata.clone()),
            ..Setup::default()
        },
    );
    let owner = base_owner(&mut f, b"d");
    f.run(&open(
        b"d/new",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    let requested = creates
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p.as_bytes() == b"d")
        .map(|(_, options)| options.mode)
        .expect("the ancestor was created");
    assert_eq!(
        requested,
        0o555 | SHADOW_OWNER_BITS,
        "the widening is what lets umbra create inside the ancestor it carried"
    );
    assert_eq!(
        carried(&metadata, b"d"),
        vec![MetadataUpdate {
            mode: None,
            uid: Some(owner.0),
            gid: Some(owner.1),
            accessed_nanos: None,
            modified_nanos: None,
        }]
    );
    // The chown ran after the create, and a successful `chown(2)` clears only
    // setuid/setgid -- the owner permission bits survive it.
    assert_eq!(mode_of(&f.shadow_root.join("d")), 0o755 & !umask());
    assert!(
        f.shadow_root.join("d/new").is_file(),
        "the create inside the carried ancestor is the thing the widening exists \
         to keep working"
    );
}

// (b) `copy_up()` -- #61. File targets.

// The #61 fidelity test and, simultaneously, the regression test for a latent
// `EACCES` that predates it: `copy_up` created the shadow at the base's mode
// verbatim, and both syscall backends reopen the object *write-only* to copy the
// content into it. Opening your own `0o444` file `O_WRONLY` is `EACCES`, so
// copy-up of a read-only base file with content has been failing here all along.
// `| SHADOW_OWNER_BITS` at the `create` is what fixes it, and this test cannot
// pass without it.
#[test]
fn copy_up_of_a_read_only_base_file_carries_content_and_base_ownership() {
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"ro", b"base bytes")],
        Setup {
            base_modes: &[(b"ro", 0o444)],
            metadata: Some(metadata.clone()),
            ..Setup::default()
        },
    );
    let owner = base_owner(&mut f, b"ro");
    f.run(&open(
        b"ro",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert_eq!(
        fs::read(f.shadow_root.join("ro")).unwrap(),
        b"base bytes",
        "the content copy is what the missing owner-write bit used to refuse"
    );
    assert_eq!(
        carried(&metadata, b"ro"),
        vec![MetadataUpdate {
            mode: None,
            uid: Some(owner.0),
            gid: Some(owner.1),
            accessed_nanos: None,
            modified_nanos: None,
        }]
    );
    assert_eq!(fs::read(f.base_root.join("ro")).unwrap(), b"base bytes");
}

#[test]
fn copy_up_of_a_private_base_file_carries_ownership_and_keeps_its_mode() {
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"private", b"base bytes")],
        Setup {
            base_modes: &[(b"private", 0o600)],
            metadata: Some(metadata.clone()),
            ..Setup::default()
        },
    );
    let owner = base_owner(&mut f, b"private");
    f.run(&open(
        b"private",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert_eq!(
        fs::read(f.shadow_root.join("private")).unwrap(),
        b"base bytes"
    );
    // `0o600 | 0o700` is `0o700`: the widening adds nothing a `0o600` base had
    // not already granted its owner except execute, and the umask does not touch
    // owner bits.
    assert_eq!(mode_of(&f.shadow_root.join("private")), 0o700 & !umask());
    assert_eq!(carried(&metadata, b"private").len(), 1);
    assert_eq!(carried(&metadata, b"private")[0].uid, Some(owner.0));
    assert_eq!(carried(&metadata, b"private")[0].gid, Some(owner.1));
}

#[test]
fn copy_up_of_an_executable_base_file_keeps_its_execute_bits_through_the_carry() {
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"tool", b"base bytes")],
        Setup {
            base_modes: &[(b"tool", 0o755)],
            metadata: Some(metadata.clone()),
            ..Setup::default()
        },
    );
    let owner = base_owner(&mut f, b"tool");
    f.run(&open(
        b"tool",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    // The chown runs after the create and after the writes. An unprivileged
    // `chown(2)` clears setuid/setgid and nothing else, so the execute bits the
    // base granted are still there.
    assert_eq!(mode_of(&f.shadow_root.join("tool")), 0o755 & !umask());
    assert_eq!(carried(&metadata, b"tool").len(), 1);
    assert_eq!(carried(&metadata, b"tool")[0].uid, Some(owner.0));
    assert_eq!(carried(&metadata, b"tool")[0].gid, Some(owner.1));
}

// `copy_up`'s early return for an object already in the shadow must not grow a
// side effect: the object is not a shadow of a base object any more, and
// chowning it would overwrite whatever the run has since made of it.
#[test]
fn copy_up_of_an_already_shadowed_file_emits_no_set_metadata() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[], &metadata);
    f.run(&open(
        b"made",
        OpenFlags {
            create: true,
            write: true,
            ..OpenFlags::default()
        },
    ));
    metadata.lock().unwrap().clear();
    let prepared = f.prepare(&chown(b"made", Some(501), Some(20), true));
    f.complete(prepared);
    assert!(
        carried_paths(&metadata).is_empty(),
        "copy-up returned early, so nothing was materialised and nothing may be \
         carried: {:?}",
        carried_paths(&metadata)
    );
}

// `copy_up` routes a logical symlink to `create_symlink`, which is a different
// contract: the placeholder carries a `0o444` mode and `logical_stat`
// substitutes `0o777`, none of which this change may perturb. The placeholder is
// not a copy of the base object, so it takes none of its ownership either.
#[test]
fn copy_up_of_a_logical_symlink_carries_no_ownership() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"target", b"base bytes")], &metadata);
    f.run(&symlink(b"link", b"target"));
    let prepared = f.prepare(&chown(b"link", Some(501), Some(20), false));
    f.complete(prepared);
    assert!(
        carried_paths(&metadata).is_empty(),
        "the symlink placeholder is not a copy of anything: {:?}",
        carried_paths(&metadata)
    );
}

// The guardrail's regression test. A carry failure that is *not* `Denied` is an
// ordinary `prepare` error, handled by the machinery that already handles a
// `create` failure -- `Pending.rollback` and `Pending.destroy` are not widened
// for it, and `copy_up` still records nothing on either.
#[test]
fn a_failed_ownership_carry_propagates_without_touching_rollback_or_destroy() {
    let failure = MetadataFailure::default();
    let mut f = Fixture::build(
        &[(b"file", b"base bytes")],
        Setup {
            fail_metadata: Some(failure.clone()),
            ..Setup::default()
        },
    );
    *failure.lock().unwrap() = Some(ErrorKind::Io);
    let action = f
        .overlay
        .resolve(
            &f.process,
            &open(
                b"file",
                OpenFlags {
                    write: true,
                    ..OpenFlags::default()
                },
            ),
        )
        .unwrap();
    let id = OperationId(Uuid::new_v4());
    assert_eq!(
        f.overlay.prepare(id, &action).unwrap_err().kind,
        ErrorKind::Io,
        "anything but `Denied` propagates, exactly as a `create` failure does"
    );
    let pending = f.overlay.pending.as_ref().expect("the transaction is open");
    assert!(
        pending.rollback.is_empty(),
        "copy-up records no undo, and a failed carry inside it adds none"
    );
    assert!(
        pending.destroy.is_none(),
        "#66's destroy plan is for `Unlink` and must not have grown a second writer"
    );
}

// (c) Privilege -- Axis 5.

// Pick (ii): fall back to umbra's uid. Fail-closed was rejected outright,
// because `parents` and `copy_up` run inside `prepare` where a failure poisons
// the run -- so refusing a base object umbra cannot chown to would not degrade
// the session, it would end it, for the overlay's primary use case.
//
// Driven by injection rather than by root: `ErrorKind::Denied` is what
// `umbra-storage-local` maps a cross-uid `EPERM` to, so this is the engine
// meeting exactly the value the kernel would have produced.
#[test]
fn an_ownership_carry_the_backend_denies_falls_back_to_umbras_uid() {
    let failure = MetadataFailure::default();
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"d/file", b"base bytes")],
        Setup {
            metadata: Some(metadata.clone()),
            fail_metadata: Some(failure.clone()),
            ..Setup::default()
        },
    );
    *failure.lock().unwrap() = Some(ErrorKind::Denied);
    f.run(&open(
        b"d/file",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    // The operation succeeded: both the ancestor and the object were carried
    // onto, both were refused, and neither refusal reached the tracee.
    assert_eq!(
        carried_paths(&metadata),
        vec!["d".to_owned(), "d/file".to_owned()]
    );
    assert!(!f.overlay.poisoned);
    assert!(f.shadow_root.join("d").is_dir());
    assert_eq!(
        fs::read(f.shadow_root.join("d/file")).unwrap(),
        b"base bytes"
    );
}

// The lifted half of #60, and the pair to
// `an_unchanged_id_chown_of_a_base_object_is_refused_rather_than_silently_re_owning_it`.
// Without this the PR would change ownership bits and unblock nothing.
#[test]
fn an_unchanged_id_chown_is_admitted_once_copy_up_can_carry_the_base_owner() {
    let metadata = Metadata::default();
    let mut f = with_metadata(&[(b"file", b"base bytes")], &metadata);
    let owner = base_owner(&mut f, b"file");
    for (uid, gid) in [(None, None), (Some(owner.0), None), (None, Some(owner.1))] {
        let action = f
            .overlay
            .resolve(&f.process, &chown(b"file", uid, gid, true))
            .unwrap_or_else(|e| panic!("uid {uid:?} gid {gid:?} was refused: {e:?}"));
        assert!(matches!(action, ResolvedAction::Rewrite(_)));
        let prepared = f
            .overlay
            .prepare(OperationId(Uuid::new_v4()), &action)
            .unwrap();
        f.complete(prepared);
    }
    assert!(f.shadow_root.join("file").is_file());
    assert_eq!(
        carried(&metadata, b"file").len(),
        1,
        "the first chown copied the object up and carried its ownership; the two \
         after it found it in the shadow and carried nothing"
    );
    assert!(!f.overlay.poisoned);
}

// Axis 5 narrowed to per object rather than per run, which is what privilege to
// chown actually is: umbra may own one base file and not the one beside it. Both
// targets are in the same run, against the same backend, and get opposite
// answers.
#[test]
fn the_sentinel_refusal_tracks_ownership_carry_per_object_not_per_run() {
    // Both targets live in one base, reached through one engine, one backend and
    // one shadow, and get *opposite* answers. Two fixtures could only put the
    // two answers side by side; "per object, not per run" is the claim that one
    // run gives different answers for different objects, so it needs one run.
    // `force_owners` carves `theirs` out and leaves `ours` as the filesystem
    // reports it -- which is umbra's own identity, since CI creates the fixture.
    let mut f = Fixture::build(
        &[(b"ours", b"base bytes"), (b"theirs", b"base bytes")],
        Setup {
            force_owners: &[(b"theirs", (4242, 4242))],
            ..Setup::default()
        },
    );
    assert!(
        f.overlay
            .resolve(&f.process, &chown(b"ours", None, None, true))
            .is_ok(),
        "the base object umbra already owns carries, so the sentinel is honourable"
    );
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"theirs", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "the carry cannot be guaranteed for this object, so the refusal stands \
         -- in the same run that just admitted the one beside it"
    );
    // And the order is not what decides it: ask again, reversed.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"theirs", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    assert!(f
        .overlay
        .resolve(&f.process, &chown(b"ours", None, None, true))
        .is_ok());
    assert!(!f.overlay.poisoned);
}

// The capability gate's safety test, and the reason this change is safe to land:
// against a backend that has not qualified ownership carry, no `SetMetadata` is
// emitted at all and the engine behaves exactly as it did before the carry
// existed -- including the sentinel refusal, which has no carry to be lifted by.
#[test]
fn a_backend_without_ownership_fidelity_emits_no_set_metadata_at_all() {
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"d/file", b"base bytes"), (b"d/untouched", b"base bytes")],
        Setup {
            metadata: Some(metadata.clone()),
            drop_ownership_fidelity: true,
            ..Setup::default()
        },
    );
    assert!(!f.overlay.ownership_fidelity());
    f.run(&open(
        b"d/file",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert_eq!(
        fs::read(f.shadow_root.join("d/file")).unwrap(),
        b"base bytes"
    );
    assert!(
        carried_paths(&metadata).is_empty(),
        "the gate is what makes the degraded mode a true no-op: {:?}",
        carried_paths(&metadata)
    );
    // Asked about the object still only in the base: the refusal is about what
    // copy-up would produce, and `d/file` has already been copied up above.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"d/untouched", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "with nothing to carry the ownership, the sentinel refusal is unchanged"
    );
}

// #84 Case A: where the shadow parent already exists and the backend advertises
// `storage-parent-identity-v1`, the sentinel is admitted against the parent's
// *observed* identity rather than the shadow root's. A base object whose owner
// matches the identity a `create` under that materialised parent will produce is
// a true no-op to carry, so the "unchanged" chown is honourable -- and today,
// with only the shadow-root answer, it is refused.
#[test]
fn an_unchanged_id_chown_is_admitted_against_a_materialised_parent_the_shadow_object_will_inherit()
{
    let mut f = Fixture::build(
        &[(b"d/anchor", b"x"), (b"d/file", b"base bytes")],
        Setup {
            // The base target wears (4242, 4242), and the materialised shadow
            // parent `d` is made to report the same -- the identity a child
            // `create` under it will inherit on a parent-identity backend.
            force_owners: &[(b"d/file", (4242, 4242))],
            force_shadow_owners: &[(b"d", (4242, 4242))],
            force_parent_identity: Some(true),
            ..Setup::default()
        },
    );
    // Copy a sibling up so the shadow parent `d` exists at resolve time; without
    // this the lookup falls back to the shadow root (that is the next test).
    f.run(&open(
        b"d/anchor",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("d").is_dir());
    // Today's whole answer -- the shadow root -- would refuse: umbra is not uid
    // 4242, so the base target does not match it.
    assert_ne!(f.overlay.shadow_identity().unwrap(), (4242, 4242));
    // But the object will be created under `d`, which wears (4242, 4242), so the
    // carry is a genuine no-op and the sentinel is honourable.
    let action = f
        .overlay
        .resolve(&f.process, &chown(b"d/file", None, None, true))
        .expect("the sentinel is admitted against the materialised parent");
    assert!(matches!(action, ResolvedAction::Rewrite(_)));
    assert!(!f.overlay.poisoned);
}

// #84's load-bearing soundness test, the fail-open guard from audit section A.3.
// At resolve time the shadow parent usually does not exist yet, and predicting
// its post-carry identity from the *base* parent would admit the sentinel in
// exactly the case where the carry is then `Denied` and swallowed -- silently
// re-owning the file. `identity_at` reads the *shadow* parent, so an absent one
// falls back to the shadow root: today's exact, conservative answer.
#[test]
fn the_parent_lookup_falls_back_to_the_shadow_root_when_the_parent_is_not_materialised() {
    let mut f = Fixture::build(
        &[(b"d/file", b"base bytes")],
        Setup {
            // The A.3 trap made concrete: base dir `d` and base file `d/file`
            // share an owner umbra is not. Reading the *base* parent would see
            // (4242,4242) == (4242,4242) and admit; the shadow parent does not
            // exist, so `parents` would create it as umbra, the carry would be
            // `Denied` and swallowed, and the "unchanged" chown would have
            // changed the owner. Reading the absent *shadow* parent refuses.
            force_owners: &[(b"d", (4242, 4242)), (b"d/file", (4242, 4242))],
            force_parent_identity: Some(true),
            ..Setup::default()
        },
    );
    // The base parent and the base target match -- the condition a base-parent
    // read would (wrongly) admit on.
    assert_eq!(base_owner(&mut f, b"d"), (4242, 4242));
    assert_eq!(base_owner(&mut f, b"d/file"), (4242, 4242));
    // And the shadow parent is genuinely absent, so this is Case B.
    assert!(
        !f.shadow_root.join("d").exists(),
        "the shadow parent must not be materialised for this to test the fallback"
    );
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"d/file", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "the fallback answer is the shadow root, which umbra owns and (4242,4242) does not match"
    );
    // Refused before anything durable or material happened -- exactly the
    // regression this guard forbids.
    assert!(f.log.lock().unwrap().records.is_empty());
    assert!(!f.shadow_root.join("d/file").exists());
    assert!(!f.overlay.poisoned);
}

// The nfs / nfs-userspace disposition (audit C-ii (a)): a backend that does not
// advertise the name is read the old way. The same materialised-parent situation
// the Case-A test *admits* is refused here, because the lookup is gated on the
// advertisement and falls back to the shadow root without it.
#[test]
fn a_backend_that_does_not_advertise_parent_identity_keeps_the_shadow_root_answer() {
    let mut f = Fixture::build(
        &[(b"d/anchor", b"x"), (b"d/file", b"base bytes")],
        Setup {
            force_owners: &[(b"d/file", (4242, 4242))],
            force_shadow_owners: &[(b"d", (4242, 4242))],
            // The one difference from the Case-A test: the backend stays silent.
            force_parent_identity: Some(false),
            ..Setup::default()
        },
    );
    f.run(&open(
        b"d/anchor",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("d").is_dir());
    assert!(!f.overlay.parent_identity_backend());
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"d/file", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "with the name unadvertised the answer is byte-identical to before #84"
    );
    assert!(!f.overlay.poisoned);
}

// The gate order: `ownership_will_carry` checks ownership fidelity *first*, so
// the parent-identity lookup never runs against a backend that has not qualified
// the carry. Even with the name advertised and a materialised parent that would
// admit, the missing carry mechanism refuses.
#[test]
fn the_parent_identity_name_is_ignored_without_ownership_fidelity() {
    let mut f = Fixture::build(
        &[(b"d/anchor", b"x"), (b"d/file", b"base bytes")],
        Setup {
            force_owners: &[(b"d/file", (4242, 4242))],
            force_shadow_owners: &[(b"d", (4242, 4242))],
            force_parent_identity: Some(true),
            drop_ownership_fidelity: true,
            ..Setup::default()
        },
    );
    f.run(&open(
        b"d/anchor",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("d").is_dir());
    assert!(
        f.overlay.parent_identity_backend(),
        "the name is advertised, so this is genuinely testing that it is ignored"
    );
    assert!(
        !f.overlay.ownership_fidelity(),
        "but ownership fidelity is not, and it is the first gate"
    );
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"d/file", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "no carry mechanism means no admission, whatever the parent identity is"
    );
    assert!(!f.overlay.poisoned);
}

// Section B, the setgid gid-leak corner turned into an honest refusal. A base
// object matching the shadow *root* (so today's answer admits it) is created
// under a parent that will hand it a *different gid* -- a setgid, or on macOS
// any, parent. The created object would then wear the parent's gid, not the
// base's, and the "unchanged" chown would silently change it. Case A reads the
// parent and refuses. Case B (parent not yet materialised) keeps today's
// exposure: this fix reduces the corner, it does not eliminate it.
#[test]
fn a_setgid_parent_that_will_hand_down_its_gid_refuses_the_unchanged_id_sentinel() {
    let mut f = Fixture::build(
        &[(b"d/anchor", b"x"), (b"d/file", b"base bytes")],
        Setup {
            // The shadow root is made to report (7777, 20), so `shadow_identity`
            // -- the Case-B fallback and today's whole answer -- is (7777, 20),
            // and the base target wears exactly that: today's shadow-root
            // predicate admits. The shadow parent `d` reports the same uid but
            // gid 4242, modelling a setgid (or BSD) parent that hands a new
            // child gid 4242 -- a real supplementary group CI cannot portably
            // construct. Case A reads `d` and sees the mismatch.
            force_shadow_owners: &[(b"", (7777, 20)), (b"d", (7777, 4242))],
            force_owners: &[(b"d/file", (7777, 20))],
            force_parent_identity: Some(true),
            ..Setup::default()
        },
    );
    f.run(&open(
        b"d/anchor",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert!(f.shadow_root.join("d").is_dir());
    // Today's fallback answer would admit: the base target wears exactly the
    // shadow root's (forced) identity.
    assert_eq!(
        f.overlay.shadow_identity().unwrap(),
        base_owner(&mut f, b"d/file"),
        "the base target matches the shadow root, so the old shadow-root predicate admits"
    );
    // But the shadow parent will hand the child gid 4242, so the carry is not a
    // no-op and the sentinel is refused honestly instead of leaking the gid.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &chown(b"d/file", None, None, true))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability,
        "the object would inherit the parent's gid, not the base's, so 'unchanged' cannot hold"
    );
    assert!(!f.overlay.poisoned);
}

// A store that declines the metadata update is a *refusal to carry*, not a
// failure of the operation that needed it. `Denied` is the privilege shape;
// `UnsupportedCapability` is the store's own, and it is the same class of answer
// arriving under a different kind -- an NFS export that does not honour SETATTR
// owner attributes replies `NFS4ERR_NOTSUPP`, and a mounted export that cannot
// chown answers `ENOTSUP`. Before this was handled, such a store poisoned the
// run on the first materialised object, because `copy_up` and `parents` run
// inside `prepare`.
#[test]
fn an_ownership_carry_the_store_says_it_cannot_do_is_not_fatal_either() {
    let failure = MetadataFailure::default();
    let metadata = Metadata::default();
    let mut f = Fixture::build(
        &[(b"d/file", b"base bytes")],
        Setup {
            metadata: Some(metadata.clone()),
            fail_metadata: Some(failure.clone()),
            ..Setup::default()
        },
    );
    *failure.lock().unwrap() = Some(ErrorKind::UnsupportedCapability);
    f.run(&open(
        b"d/file",
        OpenFlags {
            write: true,
            ..OpenFlags::default()
        },
    ));
    assert_eq!(
        carried_paths(&metadata),
        vec!["d".to_owned(), "d/file".to_owned()],
        "both the ancestor and the object were carried onto, and both refused"
    );
    assert!(
        !f.overlay.poisoned,
        "a declined carry must not poison the run"
    );
    assert!(f.shadow_root.join("d").is_dir());
    assert_eq!(
        fs::read(f.shadow_root.join("d/file")).unwrap(),
        b"base bytes"
    );
}

// The one kind that stays fatal, and the reason it does: `NotImplemented` names
// a deferred or unbound code path rather than a store declining a supported
// request, so swallowing it would hide the wiring gap it exists to report.
#[test]
fn an_unimplemented_ownership_carry_still_propagates() {
    let failure = MetadataFailure::default();
    let mut f = Fixture::build(
        &[(b"file", b"base bytes")],
        Setup {
            fail_metadata: Some(failure.clone()),
            ..Setup::default()
        },
    );
    *failure.lock().unwrap() = Some(ErrorKind::NotImplemented);
    let action = f
        .overlay
        .resolve(
            &f.process,
            &open(
                b"file",
                OpenFlags {
                    write: true,
                    ..OpenFlags::default()
                },
            ),
        )
        .unwrap();
    assert_eq!(
        f.overlay
            .prepare(OperationId(Uuid::new_v4()), &action)
            .unwrap_err()
            .kind,
        ErrorKind::NotImplemented
    );
}
