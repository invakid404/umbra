use super::*;
use std::{
    fs,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
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

impl Fixture {
    fn new(files: &[(&[u8], &[u8])]) -> Self {
        Self::with_bad_receipt(files, None)
    }
    fn with_bad_receipt(files: &[(&[u8], &[u8])], mismatch: Option<u8>) -> Self {
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        let (mut base, base_binding, base_lease) = open_storage(&base_dir);
        let base_root = native(base_binding.root.physical_path.as_ref().unwrap());
        for (name, content) in files {
            let file = base_root.join(std::ffi::OsStr::from_bytes(name));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
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
        let storage: Box<dyn Storage> = match mismatch {
            Some(mismatch) => Box::new(BadReceipt {
                inner: shadow,
                mismatch,
            }),
            None => Box::new(shadow),
        };
        let mut overlay = Overlay::new(storage, Box::new(journal));
        overlay.bind(config, Box::new(base)).unwrap();
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
        assert_eq!(
            f.overlay.resolve(&f.process, &stat(name)).unwrap_err().kind,
            ErrorKind::NotFound
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
fn same_path_base_rename_is_a_noop_and_mutation_abort_does_not_claim_rollback() {
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
    // An absent target, and one hidden by a whiteout, are both ENOENT *from the
    // resolver*. This is the namespace's answer at the resolve boundary, not
    // what a tracee observes: the supervisor turns NotFound on a non-mutating
    // operation into a plain resume, so the tracee's own unrewritten faccessat
    // still runs against the host and can see a whiteouted base file. That gap
    // is the supervisor's, it predates this operation and it applies equally to
    // Stat/Read/ReadLink, so nothing here should be read as an end-to-end
    // guarantee; issue #49 tracks closing it.
    assert_eq!(
        f.overlay
            .resolve(&f.process, &access(b"nested/missing", READ, true))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    f.run(&unlink(b"nested/file"));
    assert_eq!(
        f.overlay
            .resolve(&f.process, &access(b"nested/file", READ, true))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
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
    // The session is usable afterwards rather than poisoned, which is the whole
    // point of refusing before prepare.
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
    // copy_up recreates a base object through CreateOptions, which carries mode
    // but no uid/gid, so the shadow belongs to whoever runs umbra. The kernel then
    // sets only the IDs the tracee supplied. Were this allowed, a POSIX no-op
    // chown(-1, -1) would silently move the object to our identity, which is the
    // opposite of what the sentinel asks for.
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
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
fn a_kernel_rejected_chown_aborts_into_a_session_that_requires_reconciliation() {
    // EPERM is the ordinary answer when an unprivileged tracee chowns to
    // another uid. Because Fchownat is a Materialise operation, that routine
    // failure takes the mutation abort path: the Abort is journaled, the
    // session is poisoned and abort itself errors, which ends the run rather
    // than handing the tracee its errno. Pinned here so the limitation is a
    // recorded property rather than something discovered in a live run.
    let mut f = Fixture::new(&[(b"file", b"base bytes")]);
    let prepared = f.prepare(&chown(b"file", Some(0), Some(0), true));
    f.overlay
        .observe_result(prepared.operation_id, &OperationOutcome::Failure(Errno(1)))
        .unwrap();
    assert_eq!(
        f.overlay
            .abort(prepared.operation_id, &AbortReason::Cancelled)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidState
    );
    assert!(f.overlay.poisoned);
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
    assert!(matches!(payloads[2].payload, JournalPayload::Abort { .. }));
    assert!(
        !payloads
            .iter()
            .any(|r| matches!(r.payload, JournalPayload::Commit)),
        "a failed chown must not commit"
    );
    // The copy-up is not rolled back, and abort does not claim it was: the
    // shadow object stays, byte-identical to the base it came from.
    assert_eq!(fs::read(f.shadow_root.join("file")).unwrap(), b"base bytes");
    assert_eq!(fs::read(f.base_root.join("file")).unwrap(), b"base bytes");
}
