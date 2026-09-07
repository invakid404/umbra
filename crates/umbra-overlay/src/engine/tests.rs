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
impl Fixture {
    fn new(files: &[(&[u8], &[u8])]) -> Self {
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
        };
        let mut overlay = Overlay::new(Box::new(shadow), Box::new(journal));
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
fn rejects_symlinks_including_before_parent_components_and_trailing_file_slash() {
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
