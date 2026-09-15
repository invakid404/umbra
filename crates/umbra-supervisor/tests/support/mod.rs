//! Bring-up for a real [`Overlay`] bound to real [`LocalStorage`] driven through
//! a real [`Supervisor`].
//!
//! This is a purpose-built copy of the run/lease/base bring-up in
//! `umbra-overlay`'s `engine::tests::Fixture`, not a share of it: an integration
//! test links the library built *without* `cfg(test)`, so no `#[cfg(test)]` item
//! in `umbra-overlay` — `Fixture`, `MemoryJournal`, `Recorder`, `WatchedBase` —
//! exists in the rlib this target links, whatever its visibility.
//!
//! Copying is safe here in a way the usual objection assumes it is not. What is
//! copied is compiler-checked: `open_storage`, `StorageBase::new`,
//! `SessionConfig`, and the hand-built recovery inventory `Overlay::bind`
//! requires because `bind` never calls `Journal::open`. If any of those change
//! shape, both copies fail to compile on the same commit. What drifted twice in
//! `umbra-supervisor/src/events.rs` was a *rule restated in a comment*, which is
//! exactly what this harness replaces.
//!
//! Deliberately smaller than `Fixture`: no `Setup`, no `BadReceipt`, no
//! `base_modes`, no storage `Recorder` and no `WatchedBase`. Nothing here
//! injects a storage failure — the one test that needs a refused rollback
//! provokes a real `EACCES` with `chmod`, the way `umbra-overlay`'s own
//! `a_rollback_that_cannot_unlink_poisons_instead_of_claiming_success` does.

pub mod journal;

use std::{
    ffi::OsStr,
    fs,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use tempfile::TempDir;
use umbra_core::{
    AbortReason, AcquireWriterRequest, Architecture, BytePath, Checkpoint, CheckpointRequest,
    CommitReceipt, EmulatedResult, Errno, FsOp, IdempotencyKey, ImmutableBaseContract,
    LaunchPolicy, LaunchSpec, OpenRunIntent, OpenRunRequest, OperationId, OperationOutcome,
    PathOperand, PathRewrite, PersistencePolicy, PhysicalOperation, PhysicalPath,
    PlatformCapabilities, PreparedAction, PreparedRewrite, ProcessContext, ProcessHandle,
    QuiescedTree, RegisterSet, RequestContext, ResolvedAction, Result, ResumeCommand, RunBinding,
    RunId, SandboxRequirement, StoragePolicy, TakeoverPolicy, TaskId, TaskIdentity,
    TerminationPolicy, ThreadId, TraceEvent, WriterId, WriterLease,
};
use umbra_overlay::{NamespaceResolver, NamespaceSession, Overlay, SessionConfig, StorageBase};
use umbra_platform::{PlatformSession, SyscallAbi, TraceBackend, TraceControl, TraceMemory};
use umbra_storage::Storage;
use umbra_storage_local::LocalStorage;
use umbra_supervisor::{RunBudget, Supervisor};
use uuid::Uuid;

use journal::{recovery, MemoryJournal};

/// The one supervised task the harness launches, and the one thread on it.
fn task() -> TaskId {
    TaskId(TaskIdentity {
        native_id: 1,
        generation: 1,
    })
}

/// What the platform was asked to do, in order.
///
/// The same shape as the `Recorded`/`Recording` pair in
/// `umbra-supervisor/src/events.rs`, so an assertion reads the same in both
/// places; the difference is that here the entry *and* the exit are driven, so
/// every exit-phase assertion is taken against a suffix.
enum Recorded {
    /// Carries no payload, unlike the inline double's: an emulation is always a
    /// failure here — every test below asserts that the exit path emulated
    /// *nothing* — so the value would never be read, and a recorded value no
    /// assertion reads is the vacuous kind of coverage this harness exists to
    /// remove. Give it the `EmulatedResult` back when the first assertion wants
    /// it.
    Emulate,
    SetRegisters(ThreadId),
    Resume(ResumeCommand),
}

#[derive(Default)]
struct Recording {
    events: Vec<Recorded>,
}

/// A platform double that records what the supervisor drives it to do.
///
/// `decode_entry` answers with whatever op the harness scripted for the entry
/// about to be driven, so no real register decoding is needed and `read_memory`
/// is never reached. `prepare_rewrite` is implemented — unlike the inline
/// tests' `Recorder`, which never drives an entry and so inherits the trait's
/// refusing default — and returns a plan echoing the prepared physical path
/// with no memory writes, which is what keeps `write_memory` unreachable too.
struct Platform {
    log: Arc<Mutex<Recording>>,
    op: Arc<Mutex<Option<FsOp>>>,
}

impl Platform {
    fn push(&self, event: Recorded) {
        self.log.lock().unwrap().events.push(event);
    }
}

impl TraceBackend for Platform {
    fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
        // Deliberately unrecorded: the log is the syscall-path log, and
        // `launch_prepared` runs before any test takes a mark.
        Ok(ProcessHandle(task()))
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        unreachable!("the harness hands the supervisor its events directly")
    }
    fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
        unreachable!("the scripted decoder reads no tracee memory")
    }
    fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
        unreachable!("the prepared rewrite carries no memory writes")
    }
    fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
        unreachable!("the harness supplies the entry's registers")
    }
    fn set_registers(&mut self, thread: ThreadId, _: &RegisterSet) -> Result<()> {
        self.push(Recorded::SetRegisters(thread));
        Ok(())
    }
    fn resume(&mut self, command: ResumeCommand) -> Result<()> {
        self.push(Recorded::Resume(command));
        Ok(())
    }
}

impl TraceControl for Platform {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }
    fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
        unreachable!("no test quiesces the tree")
    }
    fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
        Ok(())
    }
    fn prepare_rewrite(
        &mut self,
        _: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite> {
        Ok(PreparedRewrite {
            operation: PhysicalOperation {
                operation,
                paths: vec![PathRewrite {
                    operand: PathOperand::Path,
                    path: PhysicalPath(path.clone()),
                }],
            },
            arguments: vec![],
            memory_writes: vec![],
        })
    }
}

impl SyscallAbi for Platform {
    fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> {
        Ok(Some(
            self.op
                .lock()
                .unwrap()
                .clone()
                .expect("the harness scripts one op before every entry"),
        ))
    }
    fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
        // Unrecorded on purpose: the `set_registers` that follows it is the
        // observable step, and recording both would make every entry-phase
        // assertion carry a step no inline test spells.
        Ok(())
    }
    fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> {
        self.push(Recorded::Emulate);
        Ok(())
    }
}

/// Forwards every call to the real [`Overlay`], recording a *different* errno
/// than the one the supervisor observed.
///
/// Models an interception inconsistency — a session that journaled one kernel
/// verdict while the exit path claims another. `syscall_exit` always calls
/// `observe_result` with the outcome it is about to name in
/// `AbortReason::KernelRefused`, so corroboration can never fail while the
/// namespace sees what the supervisor saw; skewing the observed value is the
/// only way `Overlay::abort`'s corroboration gate is reachable from a driven
/// exit. `bind` happens on the inner `Overlay` before it is wrapped.
struct SkewedOutcome {
    inner: Overlay,
    skew: Errno,
}

impl NamespaceResolver for SkewedOutcome {
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp) -> Result<ResolvedAction> {
        self.inner.resolve(context, operation)
    }
}

impl NamespaceSession for SkewedOutcome {
    fn prepare(
        &mut self,
        operation: OperationId,
        action: &ResolvedAction,
    ) -> Result<PreparedAction> {
        self.inner.prepare(operation, action)
    }
    fn observe_result(&mut self, operation: OperationId, _: &OperationOutcome) -> Result<()> {
        self.inner
            .observe_result(operation, &OperationOutcome::Failure(self.skew))
    }
    fn commit(&mut self, operation: OperationId) -> Result<CommitReceipt> {
        self.inner.commit(operation)
    }
    fn abort(&mut self, operation: OperationId, reason: &AbortReason) -> Result<()> {
        self.inner.abort(operation, reason)
    }
    fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint> {
        self.inner.checkpoint(request)
    }
    fn renew_writer(&mut self) -> Result<WriterLease> {
        self.inner.renew_writer()
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

fn context(run_id: RunId, epoch: Option<umbra_core::LeaseEpoch>) -> RequestContext {
    RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: epoch,
    }
}

fn native(path: &BytePath) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(path.as_bytes()))
}

/// A real `Overlay` over real storage, inside a real `Supervisor`.
pub struct Harness {
    /// The supervisor under test; assertions read `is_poisoned` and `state`.
    pub supervisor: Supervisor,
    /// Physical root of the shadow run, where every materialised object lands.
    pub shadow_root: PathBuf,
    log: Arc<Mutex<Recording>>,
    op: Arc<Mutex<Option<FsOp>>>,
    thread: ThreadId,
    _dirs: [TempDir; 2],
}

impl Harness {
    /// A base holding `files`, and an unskewed namespace.
    pub fn new(files: &[(&[u8], &[u8])]) -> Self {
        Self::build(files, None)
    }

    /// The same, with `observe_result` recording `skew` instead of the outcome
    /// the supervisor observed. See [`SkewedOutcome`].
    pub fn with_skewed_outcome(files: &[(&[u8], &[u8])], skew: Errno) -> Self {
        Self::build(files, Some(skew))
    }

    fn build(files: &[(&[u8], &[u8])], skew: Option<Errno>) -> Self {
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();

        let (mut base, base_binding, base_lease) = open_storage(&base_dir);
        let base_root = native(base_binding.root.physical_path.as_ref().unwrap());
        for (name, content) in files {
            let file = base_root.join(OsStr::from_bytes(name));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        // The base is frozen before it is wrapped: an immutable base holds no
        // writer authority.
        base.release_writer(&base_lease).unwrap();
        let base = StorageBase::new(
            Box::new(base),
            base_binding.clone(),
            context(base_binding.run_id, None),
        )
        .unwrap();

        let (shadow, binding, lease) = open_storage(&shadow_dir);
        // Hoisted before `binding` moves into `SessionConfig`, and then used by
        // every layer: the storage run, the journal, the session context, the
        // recovery inventory and the supervisor all name one run. In production
        // they do; `Supervisor::with_namespace` stores the id without checking
        // it against the namespace, so a second id here would be invisible to
        // every assertion today and would quietly make the first journal-shape
        // test assert across two unrelated runs.
        let run_id = binding.run_id;
        let shadow_root = native(binding.root.physical_path.as_ref().unwrap());
        let journal = MemoryJournal::new(run_id, lease.epoch);
        let config = SessionConfig {
            context: context(run_id, Some(lease.epoch)),
            recovery: recovery(run_id),
            binding,
            lease,
        };
        let mut overlay = Overlay::new(Box::new(shadow), Box::new(journal));
        overlay.bind(config, Box::new(base)).unwrap();
        let namespace: Box<dyn NamespaceSession + Send> = match skew {
            None => Box::new(overlay),
            Some(skew) => Box::new(SkewedOutcome {
                inner: overlay,
                skew,
            }),
        };

        let log = Arc::new(Mutex::new(Recording::default()));
        let op = Arc::new(Mutex::new(None));
        let mut supervisor = Supervisor::with_namespace(
            run_id,
            PlatformSession {
                control: Box::new(Platform {
                    log: log.clone(),
                    op: op.clone(),
                }),
                abi: Box::new(Platform {
                    log: log.clone(),
                    op: op.clone(),
                }),
            },
            namespace,
            None,
        );
        // Through the public door rather than by poking private fields: the
        // double's `launch` returns the one task, and `launch_prepared` sets the
        // lifecycle, the root, the live count and the renewal deadline from it.
        supervisor
            .launch_prepared(launch_spec(), budget())
            .expect("the platform double launches");

        Self {
            supervisor,
            shadow_root,
            log,
            op,
            thread: ThreadId(task().0),
            _dirs: [base_dir, shadow_dir],
        }
    }

    /// The single thread every event in these tests is driven on.
    pub fn thread(&self) -> ThreadId {
        self.thread
    }

    /// Current length of the platform log; exit-phase assertions are suffixes
    /// taken from a mark.
    pub fn mark(&self) -> usize {
        self.log.lock().unwrap().events.len()
    }

    /// Drive one `SyscallEntry` carrying `op`, returning what the supervisor
    /// made of it.
    pub fn entry(&mut self, op: FsOp) -> Result<()> {
        *self.op.lock().unwrap() = Some(op);
        let thread = self.thread;
        self.supervisor.handle_event(TraceEvent::SyscallEntry {
            task: task(),
            thread,
            registers: RegisterSet::new(Architecture::Aarch64, vec![]).unwrap(),
        })
    }

    /// Drive an entry that must resolve to a `Rewrite`, assert the shared
    /// preconditions once, and return the mark the exit is asserted against.
    ///
    /// Both halves of the transaction are driven here, unlike the inline
    /// supervisor tests which seed the operation slot and skip the entry. So the
    /// platform log is never `["resume"]` on its own — a `Rewrite` entry records
    /// `["set_registers", "resume"]` first — and every exit-phase assertion must
    /// be a suffix after the returned mark rather than the whole log.
    pub fn prepared_entry(&mut self, op: FsOp, shadow_object: &str) -> usize {
        let before = self.mark();
        self.entry(op)
            .expect("the entry must resolve, prepare and resume");
        assert_eq!(
            self.order_since(before),
            vec!["set_registers", "resume"],
            "the entry must have produced a Rewrite the platform installed and resumed"
        );
        assert!(
            self.shadow_root.join(shadow_object).exists(),
            "prepare must have materialised the shadow object the exit reconciles"
        );
        self.mark()
    }

    /// Drive the matching `SyscallExit` carrying a kernel refusal.
    pub fn exit(&mut self, errno: Errno) -> Result<()> {
        let thread = self.thread;
        self.supervisor.handle_event(TraceEvent::SyscallExit {
            task: task(),
            thread,
            outcome: OperationOutcome::Failure(errno),
        })
    }

    /// What the platform was asked to do after `mark`, in order.
    pub fn order_since(&self, mark: usize) -> Vec<&'static str> {
        self.log.lock().unwrap().events[mark..]
            .iter()
            .map(|e| match e {
                Recorded::Emulate => "emulate",
                Recorded::SetRegisters(_) => "set_registers",
                Recorded::Resume(_) => "resume",
            })
            .collect()
    }

    /// The resume commands issued after `mark`.
    pub fn resumes_since(&self, mark: usize) -> Vec<ResumeCommand> {
        self.log.lock().unwrap().events[mark..]
            .iter()
            .filter_map(|e| match e {
                Recorded::Resume(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// The threads whose registers were installed after `mark`.
    pub fn set_register_threads_since(&self, mark: usize) -> Vec<ThreadId> {
        self.log.lock().unwrap().events[mark..]
            .iter()
            .filter_map(|e| match e {
                Recorded::SetRegisters(t) => Some(*t),
                _ => None,
            })
            .collect()
    }
}

/// The launch the platform double answers. Its contents are never inspected:
/// the double returns the one task whatever it is handed.
fn launch_spec() -> LaunchSpec {
    LaunchSpec {
        executable: BytePath::new(b"/usr/bin/true".to_vec()).unwrap(),
        argv: vec![b"true".to_vec()],
        environment: vec![],
        cwd: BytePath::new(b"/".to_vec()).unwrap(),
        policy: LaunchPolicy {
            persistence: PersistencePolicy::LocalDevelopment,
            inherited_fds: vec![],
        },
        sandbox: SandboxRequirement::UnsandboxedExperiment,
    }
}

/// `renew_after` is an hour, so no renewal is ever due inside a test and
/// `NamespaceSession::renew_writer` is never reached. `cwd` is the logical root,
/// which is what makes a `DirRef::Cwd` operand with a relative path resolve the
/// way `umbra-overlay`'s own fixture resolves it.
fn budget() -> RunBudget {
    RunBudget {
        renew_after: Duration::from_secs(3600),
        cwd: BytePath::new(b"/".to_vec()).unwrap(),
        architecture: Architecture::Aarch64,
        abi: "test".into(),
    }
}
