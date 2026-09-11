#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod support;
use std::{
    collections::BTreeMap,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use umbra_core::*;
use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};
use umbra_platform_macos::{DarwinArm64Abi, MacosTraceBackend, Options};
fn byte_path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}
fn bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}
struct Memory<'a>(&'a mut MacosTraceBackend, TaskId);
impl TraceMemory for Memory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        self.0.read_memory(self.1, address, out)
    }
}
/// Bare-name cases: the child is launched as `<vendor path> <case> <output>`,
/// and this harness owns the `CAPTURED` verdict.
fn fixture(case: &str, expected: &[u8]) {
    fixture_argv(case, expected, true, |vendor, host| {
        vec![bytes(vendor), case.as_bytes().to_vec(), bytes(host)]
    })
}
/// `argv` builds the launch argv from the absolute vendor executable and the
/// logical output path. `harness_verdict` is false for cases whose child prints
/// `CAPTURED <case>` itself; every assertion below still gates the test result.
fn fixture_argv(
    case: &str,
    expected: &[u8],
    harness_verdict: bool,
    argv: impl FnOnce(&Path, &Path) -> Vec<Vec<u8>>,
) {
    let (Some(fixture), Some(root)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        assert!(
            std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
            "required integration needs UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT"
        );
        eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return;
    };
    let fixture = PathBuf::from(fixture);
    let root = support::redirect_root(root);
    let host_dir =
        std::env::temp_dir().join(format!("umbra-rust-fixture-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&host_dir).unwrap();
    let host = host_dir.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    assert!(!host.exists(), "fixture host output already exists");
    assert!(!shadow.exists(), "fixture shadow output already exists");
    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 25_000,
        ..Options::default()
    });
    let process = tracer
        .launch_experimental(LaunchSpec {
            executable: byte_path(&fixture),
            argv: argv(&fixture, &host),
            environment: vec![],
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            },
            // Direct tracer coverage, deliberately without enforcement: these
            // cases measure interception, not the sandbox boundary. `umbra run`
            // cannot select this; it always renders and requires a profile.
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let mut live = 1;
    let mut opens = 0;
    let mut threads = BTreeMap::new();
    while live > 0 {
        let event = tracer.next_event().unwrap();
        eprintln!("{case}: {event:?}");
        let resume = match event {
            TraceEvent::ThreadStarted { task, thread } => {
                threads.insert(task, thread);
                Some(thread)
            }
            TraceEvent::Child { .. } => {
                live += 1;
                None
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let op = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .unwrap()
                    .unwrap();
                let FsOp::Open {
                    ref path, flags, ..
                } = op
                else {
                    panic!("unexpected operation")
                };
                assert!(path.is_absolute(), "relative paths are not qualified");
                // Only rewrite write-intent opens to the shadow root; leave
                // read-only opens (library/dyld loads, runtime resource reads)
                // pointing at the host so the tracee can actually start.
                let write_intent = flags.write || flags.append || flags.create || flags.truncate;
                if write_intent {
                    let physical = root.join(
                        Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
                            .strip_prefix("/")
                            .unwrap(),
                    );
                    std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
                    let plan = tracer
                        .prepare_rewrite(thread, &byte_path(&physical), op)
                        .unwrap();
                    for write in &plan.memory_writes {
                        tracer
                            .write_memory(task, write.address, &write.bytes)
                            .unwrap();
                    }
                    DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                    tracer.set_registers(thread, &registers).unwrap();
                    opens += 1;
                }
                Some(thread)
            }
            TraceEvent::SyscallExit { thread, .. } => {
                // Non-rewritten (read-only) opens legitimately return errors
                // (files missing under this test harness); the target write
                // opens are asserted by the final shadow-content check.
                Some(thread)
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected event {other:?}"),
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    tracer
        .terminate(process, TerminationPolicy::Immediate)
        .unwrap();
    assert!(opens > 0);
    assert!(!host.exists(), "MISSED {case}: host output exists");
    assert_eq!(
        std::fs::read(&shadow).unwrap(),
        expected,
        "MISSED {case}: shadow content"
    );
    if harness_verdict {
        eprintln!("CAPTURED {case}");
    }
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(host_dir).unwrap();
}
#[test]
fn open_libc() {
    fixture("open-libc", b"libc\n")
}
#[test]
fn open_svc() {
    fixture("open-svc", b"libc\n")
}
#[test]
fn fork_write() {
    fixture("fork-write", b"fork\n")
}
#[test]
fn posix_spawn_write() {
    fixture("posix-spawn-write", b"libc\n")
}
#[test]
fn exec_write() {
    fixture("exec-write", b"libc\n")
}
#[test]
fn grandchild_write() {
    fixture("grandchild-write", b"grandchild\n")
}
#[test]
fn dup_inherit_write() {
    fixture("dup-inherit-write", b"dup\n")
}
/// WNOHANG must be honoured for both `__wait4` (syscall 7) and
/// `__wait4_nocancel` (400). The child is held on a pipe, so every poll runs
/// against a live, unreaped child: a poll that wrongly blocks deadlocks and
/// fails against the 25 s session deadline instead of passing on timing. Under
/// the tracer a native wait would report ECHILD — debugger attach reparents
/// children — so the zero returns the fixture asserts can only come from the
/// tracer's own virtualization. The fixture prints `CAPTURED wnohang-wait`.
#[test]
fn wnohang_wait() {
    fixture_argv("wnohang-wait", b"wnohang\n", false, |vendor, host| {
        vec![bytes(vendor), b"--wnohang-wait".to_vec(), bytes(host)]
    })
}
/// argv[0] must arrive as the absolute vendor path even though the tracer
/// launches a signed twin. The launcher is handed a deliberately different
/// argv[0], so the child's check — argv[0] equal to the vendor path and
/// different from the image actually running — can only hold if `launch_traced`
/// substituted it. The child prints `CAPTURED argv0-check` once that check
/// passes; the shadow content asserted here is written only after it.
#[test]
fn argv0_check() {
    fixture_argv("argv0-check", b"argv0\n", false, |vendor, host| {
        vec![
            b"umbra-decoy-argv0".to_vec(),
            b"--argv0-check".to_vec(),
            bytes(host),
            bytes(vendor),
        ]
    })
}

// ---------------------------------------------------------------------------
// dirfd-rename runs the same tracer through the real overlay transaction flow
// -- resolve, prepare, apply the rewrite, observe_result, commit -- instead of
// the prefix redirect the cases above use. A prefix redirect is not copy-up or
// whiteout semantics, so it cannot qualify a two-operand rename.
// ---------------------------------------------------------------------------
use std::sync::{Arc, Mutex};
use umbra_journal::Journal;
use umbra_overlay::{
    NamespaceResolver, NamespaceSession, Overlay, SessionConfig, StatEncoder, StorageBase,
};
use umbra_platform_macos::abi;
use umbra_storage::Storage;
use umbra_storage_local::LocalStorage;
use uuid::Uuid;

/// Logical roots the overlay-backed cases work under. These are namespace
/// paths, not host paths: nothing exists there on the host, so an operand the
/// tracer fails to rewrite lands somewhere absent and fails loudly instead of
/// quietly working.
const DIRFD_ROOT: &[u8] = b"/umbra-dirfd";
const SYMLINK_ROOT: &[u8] = b"/umbra-symlink";

/// Output-buffer address for the stat currently stopped at its entry. The
/// overlay's encoder contract carries logical metadata only, so the harness
/// binds the tracee's buffer alongside it.
#[derive(Clone, Default)]
struct StatBuffer(Arc<Mutex<Option<u64>>>);
/// Encodes overlay metadata into Darwin's `struct stat`. A logical symlink is
/// reported as a link with its target length, never as the placeholder object
/// the overlay stores for it.
struct DarwinStat(StatBuffer);
impl StatEncoder for DarwinStat {
    fn encode(
        &mut self,
        _context: &ProcessContext,
        _operation: &FsOp,
        stat: &BlobStat,
    ) -> Result<EmulatedResult> {
        let address = self.0 .0.lock().unwrap().take().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                "stat encoder",
                "no bound output buffer",
            )
        })?;
        Ok(EmulatedResult {
            outcome: OperationOutcome::Success { return_value: 0 },
            memory_writes: vec![MemoryWrite {
                address,
                bytes: abi::encode_stat(stat)?,
            }],
        })
    }
}

/// Records every journal record: the overlay's Prepare/ObservedResult/Commit
/// ordering is evidence this test asserts, not noise to discard.
struct RecordingJournal {
    records: Arc<Mutex<Vec<JournalRecord>>>,
    run: RunId,
    epoch: LeaseEpoch,
}
impl Journal for RecordingJournal {
    fn open(&mut self, _: &JournalOpenRequest) -> Result<RecoveryState> {
        Ok(recovery(self.run))
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        let mut records = self.records.lock().unwrap();
        let sequence = Sequence(records.len() as u64 + 1);
        let mut record = record.clone();
        record.sequence = sequence;
        records.push(record);
        Ok(sequence)
    }
    fn flush(&mut self, sequence: Sequence) -> Result<DurableSequence> {
        Ok(DurableSequence {
            run_id: self.run,
            writer_epoch: self.epoch,
            sequence,
        })
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let records = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.sequence.0 > after.0)
            .cloned()
            .map(Ok)
            .collect::<Vec<_>>();
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
fn request(run_id: RunId, epoch: Option<LeaseEpoch>) -> RequestContext {
    RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: epoch,
    }
}
fn open_run(dir: &Path) -> (LocalStorage, RunBinding, WriterLease) {
    let mut storage = LocalStorage::new(dir).unwrap();
    let run_id = RunId(Uuid::new_v4());
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "umbra-dirfd-base".into(),
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
            writer_id: WriterId("umbra-dirfd".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .unwrap();
    (storage, binding, lease)
}
/// Every (anchor, path) pair an operation names, so the harness can tell the
/// case's own traffic from dyld and library startup reads.
fn operands(op: &FsOp) -> Vec<(DirRef, &BytePath)> {
    match op {
        FsOp::Open { dir, path, .. }
        | FsOp::Stat { dir, path, .. }
        | FsOp::Access { dir, path, .. }
        | FsOp::Unlink { dir, path, .. }
        | FsOp::ReadLink { dir, path }
        | FsOp::Mkdir { dir, path, .. }
        | FsOp::Chmod { dir, path, .. }
        | FsOp::Fchownat { dir, path, .. } => vec![(*dir, path)],
        FsOp::Rename {
            from_dir,
            from,
            to_dir,
            to,
        } => vec![(*from_dir, from), (*to_dir, to)],
        FsOp::Link {
            from_dir,
            from,
            to_dir,
            to,
            ..
        } => vec![(*from_dir, from), (*to_dir, to)],
        FsOp::Symlink {
            link_dir,
            link_name,
            ..
        } => vec![(*link_dir, link_name)],
        _ => vec![],
    }
}
/// True when the operation belongs to the case rather than to startup traffic.
///
/// This harness binds an empty immutable base, so dyld and the shared cache
/// could not resolve through it; those reads are left pointing at the host,
/// unrewritten. Everything the case itself does is either under the logical
/// root or anchored on a descriptor this harness tracked.
fn case_traffic(op: &FsOp, root: &[u8], fds: &BTreeMap<TracedFd, FdState>) -> bool {
    operands(op).iter().any(|(dir, path)| {
        path.as_bytes().starts_with(root) || matches!(dir, DirRef::Fd(fd) if fds.contains_key(fd))
    })
}
/// Errno for a refused resolution. A refusal is a real syscall result for the
/// tracee, not a harness failure: this case deliberately looks up a name the
/// rename removed. Structured kinds are translated; anything else is a genuine
/// harness failure and stays loud.
fn denial(error: &UmbraError) -> Errno {
    error.errno.unwrap_or_else(|| match error.kind {
        ErrorKind::NotFound => Errno(2),
        ErrorKind::Denied => Errno(13),
        ErrorKind::AlreadyExists => Errno(17),
        ErrorKind::InvalidPath => Errno(22),
        // Loop exhaustion is a distinct kind precisely so this mapping does
        // not have to read an error message. ELOOP is 62 on Darwin.
        ErrorKind::SymlinkLoop => Errno(62),
        _ => panic!("MISSED dirfd-rename: {error:?}"),
    })
}
/// Absolute logical path of an operand, for descriptor bookkeeping.
fn logical(dir: DirRef, path: &BytePath, fds: &BTreeMap<TracedFd, FdState>) -> BytePath {
    if path.is_absolute() {
        return path.clone();
    }
    let anchor = match dir {
        DirRef::Fd(fd) => fds[&fd].logical_path.clone().unwrap(),
        DirRef::Cwd => panic!("relative cwd anchor is not used by this case"),
    };
    let mut bytes = anchor.as_bytes().to_vec();
    bytes.push(b'/');
    bytes.extend(path.as_bytes());
    BytePath::new(bytes).unwrap()
}

/// Drives one fixture case through the real overlay transaction flow --
/// `resolve` -> `prepare` -> apply the rewrite or the emulated result ->
/// `observe_result` -> `commit`/`abort` -- against a real local storage shadow,
/// an approved empty immutable base and a recording journal. A prefix redirect
/// is not copy-up or whiteout semantics, so it cannot qualify these cases.
///
/// dyld and library startup reads are left pointing at the host, unrewritten:
/// the bound base is empty, so the tracee could not otherwise start. That
/// carve-out is a property of this harness, not of the tracer.
fn overlay_fixture(
    case: &str,
    option: &str,
    root: &[u8],
    verify: impl FnOnce(&mut Overlay, &ProcessContext, &[JournalRecord], &Path, &[FsOp]),
) {
    let (Some(fixture), Some(_)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        assert!(
            std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
            "required integration needs UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT"
        );
        eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return;
    };
    let fixture = PathBuf::from(fixture);
    let host_root = Path::new(std::ffi::OsStr::from_bytes(root));
    assert!(
        !host_root.exists(),
        "the logical root must not exist on the host"
    );

    let base_dir = tempfile::tempdir().unwrap();
    let shadow_dir = tempfile::tempdir().unwrap();
    let (mut base_storage, base_binding, base_lease) = open_run(base_dir.path());
    let base_root = PathBuf::from(std::ffi::OsStr::from_bytes(
        base_binding.root.physical_path.as_ref().unwrap().as_bytes(),
    ));
    base_storage.release_writer(&base_lease).unwrap();
    let base = StorageBase::new(
        Box::new(base_storage),
        base_binding.clone(),
        request(base_binding.run_id, None),
    )
    .unwrap();
    let (shadow, binding, lease) = open_run(shadow_dir.path());
    let records = Arc::new(Mutex::new(Vec::new()));
    let journal = RecordingJournal {
        records: records.clone(),
        run: binding.run_id,
        epoch: lease.epoch,
    };
    let control_root = PathBuf::from(std::ffi::OsStr::from_bytes(
        binding.control.physical_path.as_ref().unwrap().as_bytes(),
    ));
    let config = SessionConfig {
        context: request(binding.run_id, Some(lease.epoch)),
        recovery: recovery(binding.run_id),
        binding,
        lease,
    };
    let mut overlay = Overlay::new(Box::new(shadow), Box::new(journal));
    overlay.bind(config, Box::new(base)).unwrap();
    let stat_buffer = StatBuffer::default();
    overlay
        .set_stat_encoder(Box::new(DarwinStat(stat_buffer.clone())))
        .unwrap();

    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 25_000,
        ..Options::default()
    });
    let handle = tracer
        .launch_experimental(LaunchSpec {
            executable: byte_path(&fixture),
            argv: vec![bytes(&fixture), option.as_bytes().to_vec(), root.to_vec()],
            environment: vec![],
            sandbox: SandboxRequirement::UnsandboxedExperiment,
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            },
        })
        .unwrap();
    let mut process = ProcessContext {
        task: TaskId(TaskIdentity {
            native_id: 0,
            generation: 0,
        }),
        parent: None,
        architecture: Architecture::Aarch64,
        abi: "darwin-arm64-abi-v1".into(),
        exec_generation: 0,
        root: byte_path(Path::new("/")),
        cwd: byte_path(&std::env::current_dir().unwrap()),
        fds: BTreeMap::new(),
    };
    let mut pending: Option<(PreparedAction, FsOp)> = None;
    let mut observed = Vec::new();
    let mut live = 1;
    while live > 0 {
        let event = tracer.next_event().unwrap();
        let resume = match event {
            TraceEvent::ThreadStarted { task, thread } => {
                process.task = task;
                Some(thread)
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let op = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .unwrap();
                match op {
                    Some(op) if case_traffic(&op, root, &process.fds) => {
                        eprintln!("{case}: {op:?}");
                        observed.push(op.clone());
                        // The overlay owns logical metadata, so the tracee's
                        // own output buffers are bound here, from the stopped
                        // call's registers, before resolution consumes them.
                        match &op {
                            FsOp::ReadLink { .. } => {
                                let (address, len) = abi::readlink_buffer(&registers).unwrap();
                                overlay.set_readlink_buffer(address, len).unwrap();
                            }
                            FsOp::Stat { .. } => {
                                *stat_buffer.0.lock().unwrap() =
                                    Some(abi::stat_buffer(&registers).unwrap());
                            }
                            _ => {}
                        }
                        let action = match overlay.resolve(&process, &op) {
                            Ok(action) => action,
                            Err(refused) => {
                                let result = EmulatedResult {
                                    outcome: OperationOutcome::Failure(denial(&refused)),
                                    memory_writes: vec![],
                                };
                                DarwinArm64Abi
                                    .emulate_result(&mut registers, &result)
                                    .unwrap();
                                tracer.set_registers(thread, &registers).unwrap();
                                tracer
                                    .resume(ResumeCommand {
                                        thread,
                                        mode: ResumeMode::Syscall,
                                        signal: None,
                                    })
                                    .unwrap();
                                continue;
                            }
                        };
                        let prepared = overlay
                            .prepare(OperationId(Uuid::new_v4()), &action)
                            .unwrap();
                        // Emulated results never reach the kernel: the tracer
                        // steps the PC past the `svc` and synthesises the exit,
                        // so the same transaction bookkeeping still applies.
                        let physical = match &prepared.action {
                            ResolvedAction::Rewrite(physical) => physical,
                            ResolvedAction::Emulate(result) => {
                                for write in &result.memory_writes {
                                    tracer
                                        .write_memory(task, write.address, &write.bytes)
                                        .unwrap();
                                }
                                DarwinArm64Abi
                                    .emulate_result(&mut registers, result)
                                    .unwrap();
                                tracer.set_registers(thread, &registers).unwrap();
                                pending = Some((prepared, op));
                                tracer
                                    .resume(ResumeCommand {
                                        thread,
                                        mode: ResumeMode::Syscall,
                                        signal: None,
                                    })
                                    .unwrap();
                                continue;
                            }
                            other => panic!("MISSED {case}: {other:?}"),
                        };
                        let plan = tracer.prepare_physical(thread, physical.clone()).unwrap();
                        // Any two-operand syscall must rewrite both endpoints
                        // into separate buffers; one rewritten endpoint would
                        // mutate outside the shadow.
                        if plan.arguments.len() == 2 {
                            let slots = plan.arguments.iter().map(|a| a.index).collect::<Vec<_>>();
                            assert_eq!(
                                slots,
                                vec![1, 3],
                                "MISSED {case}: two-operand rewrite must use x1 and x3"
                            );
                            assert_ne!(
                                plan.arguments[0].value, plan.arguments[1].value,
                                "MISSED {case}: operands share a scratch buffer"
                            );
                            assert_eq!(plan.memory_writes.len(), 2);
                            for write in &plan.memory_writes {
                                assert!(
                                    write.bytes.starts_with(b"/") && write.bytes.ends_with(&[0]),
                                    "MISSED {case}: operand is not an absolute C string"
                                );
                            }
                        }
                        for write in &plan.memory_writes {
                            tracer
                                .write_memory(task, write.address, &write.bytes)
                                .unwrap();
                        }
                        DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                        tracer.set_registers(thread, &registers).unwrap();
                        pending = Some((prepared, op));
                    }
                    // Startup traffic: left pointing at the host, unrewritten.
                    _ => {}
                }
                Some(thread)
            }
            TraceEvent::SyscallExit {
                thread, outcome, ..
            } => {
                if let Some((prepared, op)) = pending.take() {
                    overlay
                        .observe_result(prepared.operation_id, &outcome)
                        .unwrap();
                    match &outcome {
                        OperationOutcome::Success { return_value } => {
                            overlay.commit(prepared.operation_id).unwrap();
                            if let FsOp::Open {
                                dir, path, flags, ..
                            } = &op
                            {
                                if flags.directory {
                                    let path = logical(*dir, path, &process.fds);
                                    let anchored = overlay
                                        .resolve_path(&process, DirRef::Cwd, &path, false)
                                        .unwrap();
                                    let stat = overlay.stat(&anchored, true).unwrap();
                                    process.fds.insert(
                                        TracedFd(*return_value as i32),
                                        FdState {
                                            object: stat.object_id,
                                            logical_path: Some(path),
                                            directory: true,
                                            flags: *flags,
                                        },
                                    );
                                }
                            }
                        }
                        OperationOutcome::Failure(errno) => {
                            // A refused lookup is a real outcome for this case:
                            // the fixture proves the renamed-away source is gone.
                            overlay
                                .abort(
                                    prepared.operation_id,
                                    &AbortReason::Failed(UmbraError::new(
                                        ErrorKind::NotFound,
                                        "dirfd-rename",
                                        format!("errno {}", errno.0),
                                    )),
                                )
                                .unwrap();
                        }
                    }
                }
                Some(thread)
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected event {other:?}"),
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    tracer
        .terminate(handle, TerminationPolicy::Immediate)
        .unwrap();

    verify(
        &mut overlay,
        &process,
        &records.lock().unwrap(),
        &control_root,
        &observed,
    );
    assert!(
        !host_root.exists(),
        "MISSED {case}: the case touched the host"
    );
    assert_eq!(
        std::fs::read_dir(&base_root).unwrap().count(),
        0,
        "MISSED {case}: the immutable base changed"
    );
}

/// `renameat` across two directory descriptors, resolved and executed through
/// the overlay. Both path operands must be rewritten to distinct scratch
/// buffers holding shadow paths; the dirfd registers stay as the tracee set
/// them, because a qualified physical path is absolute and the kernel ignores
/// the anchor for an absolute path. The fixture prints `CAPTURED dirfd-rename`
/// once its own end-to-end reads agree.
#[test]
fn dirfd_rename() {
    overlay_fixture(
        "dirfd-rename",
        "--dirfd-rename",
        DIRFD_ROOT,
        |overlay, process, records, _control, observed| {
            assert_eq!(
                observed
                    .iter()
                    .filter(|op| matches!(op, FsOp::Rename { .. }))
                    .count(),
                2,
                "MISSED dirfd-rename: both rename legs must run"
            );
            let destination = BytePath::new([DIRFD_ROOT, b"/db/c"].concat()).unwrap();
            let anchored = overlay
                .resolve_path(process, DirRef::Cwd, &destination, false)
                .unwrap();
            let stat = overlay.stat(&anchored, true).unwrap();
            assert_eq!(stat.kind, ObjectKind::File);
            assert_eq!(stat.len, 6, "MISSED dirfd-rename: destination bytes");
            let source = BytePath::new([DIRFD_ROOT, b"/da/a"].concat()).unwrap();
            let absent = overlay
                .resolve_path(process, DirRef::Cwd, &source, false)
                .and_then(|p| overlay.stat(&p, true));
            assert!(
                absent.is_err(),
                "MISSED dirfd-rename: renamed source is still visible"
            );
            assert!(
                records.windows(3).any(|w| matches!(
                    w[0].payload,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Rename { .. }
                    }
                ) && matches!(
                    w[1].payload,
                    JournalPayload::ObservedResult { .. }
                ) && matches!(w[2].payload, JournalPayload::Commit)),
                "MISSED dirfd-rename: no journaled rename transaction"
            );
        },
    )
}

/// Logical symlinks end to end: `symlink`/`symlinkat` create control metadata
/// rather than a filesystem symlink, `readlink`/`readlinkat` are answered from
/// that metadata with exact bytes and no NUL, a no-follow stat reports a link
/// rather than the stored placeholder, and a two-link loop exhausts the
/// expansion bound as ELOOP. The fixture prints `CAPTURED symlink-cycle` once
/// all of that agrees from inside the tracee.
#[test]
fn symlink_cycle() {
    overlay_fixture(
        "symlink-cycle",
        "--symlink-cycle",
        SYMLINK_ROOT,
        |_overlay, _process, records, control, observed| {
            let kinds =
                |mut f: Box<dyn FnMut(&FsOp) -> bool>| observed.iter().filter(|op| f(op)).count();
            // symlink(57) and symlinkat(474); readlink(58) and readlinkat(473).
            assert_eq!(
                kinds(Box::new(|op| matches!(op, FsOp::Symlink { .. }))),
                5,
                "MISSED symlink-cycle: both create forms must run"
            );
            assert_eq!(
                kinds(Box::new(|op| matches!(op, FsOp::ReadLink { .. }))),
                3,
                "MISSED symlink-cycle: both read forms must run"
            );
            assert_eq!(
                kinds(Box::new(|op| matches!(
                    op,
                    FsOp::Stat { follow: false, .. }
                ))),
                1,
                "MISSED symlink-cycle: the no-follow stat must run"
            );
            // Target metadata holds literal tracee-visible bytes. A
            // physicalized target would name the temporary shadow root, which
            // is not in this set; the absolute one is a logical path.
            let mut stored = std::fs::read_dir(control.join("symlinks/targets"))
                .unwrap()
                .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>();
            stored.sort();
            let mut expected = vec![
                b"data-\xff".to_vec(),
                b"data".to_vec(),
                [SYMLINK_ROOT, b"/sl/data"].concat(),
                b"loop1".to_vec(),
                b"loop2".to_vec(),
            ];
            expected.sort();
            assert_eq!(
                stored, expected,
                "MISSED symlink-cycle: stored targets are not the literal bytes"
            );
            assert!(
                records.windows(3).any(|w| matches!(
                    w[0].payload,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Symlink { .. }
                    }
                ) && matches!(
                    w[1].payload,
                    JournalPayload::ObservedResult { .. }
                ) && matches!(w[2].payload, JournalPayload::Commit)),
                "MISSED symlink-cycle: no journaled symlink transaction"
            );
        },
    )
}
