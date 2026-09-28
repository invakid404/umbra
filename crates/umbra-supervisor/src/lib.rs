//! Synchronous supervisor assembly using only injected backend contracts.
//!
//! Construct on the dedicated debug-control thread; platform objects need not be
//! Send. Storage and Journal have one owner in the standard overlay. Future workers
//! require bounded messages and explicit ordering; trace polling must not starve
//! writer renewal. Provider loss must retain enforcement and block mutations.
//!
//! Construction performs no I/O. [`run`] owns the staged composition of one
//! command run: validate, connect storage, open the run, take the writer lease,
//! open the journal, bind the namespace, render enforcement, launch stopped, drive
//! events, then tear down in order. Checkpoint, resume and recovery remain
//! unimplemented and say so.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use umbra_agent::Agent;
pub use umbra_core::CheckpointRequest;
use umbra_core::{
    AgentLaunchRequest, AgentSession, BytePath, Checkpoint, ExitStatus, FsOp, ObjectId,
    OperationId, OperationOutcome, PreparedRewrite, ProcessContext, ProcessHandle, Prot,
    QuiescedTree, RegisterSet, ResolvedAction, Result, RunId, TaskId, TerminationPolicy, ThreadId,
    UmbraError,
};
use umbra_journal::Journal;
use umbra_overlay::{standard_namespace, NamespaceSession};
use umbra_platform::PlatformSession;
use umbra_storage::Storage;

/// Approved workspace inventory and the read-only base built from it.
pub mod base;
/// The production directory encoder and its per-syscall buffer binding.
pub mod directory;
mod events;
/// Staged composition of one command run.
pub mod run;
/// Runtime rendering of the single Seatbelt policy template.
pub mod sandbox;

pub use run::{
    resume, run, CommandLaunch, ResumeOutcome, ResumeSpec, RunLaunch, RunObserver, RunOutcome,
    RunPersistence, RunSpec,
};

/// Runtime inventory, never persisted or restored across hosts.
#[derive(Clone, Debug, Default)]
pub struct ProcessTree {
    /// Root.
    pub root: Option<ProcessHandle>,
    /// Generation-bearing identities distinguish reused native PIDs.
    pub processes: BTreeMap<TaskId, ProcessState>,
}

/// Handoff §5.1: context owns task/parent identity, architecture, ABI, exec
/// generation, logical cwd/root, and fd/directory-fd bindings. Fork clones logical
/// context; exec retains applicable descriptors and replaces mappings/breakpoints.
#[derive(Clone, Debug)]
pub struct ProcessState {
    /// Context associated with this value or operation.
    pub context: ProcessContext,
    /// Children.
    pub children: BTreeSet<TaskId>,
    /// Threads.
    pub threads: BTreeMap<ThreadId, ThreadState>,
    /// Mappings.
    pub mappings: Vec<ExecutableMapping>,
    /// Breakpoints.
    pub breakpoints: BTreeMap<u64, BreakpointState>,
    /// Child capture.
    pub child_capture: ChildCaptureState,
    /// Lifecycle.
    pub lifecycle: ProcessLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Process lifecycle.
pub enum ProcessLifecycle {
    /// Stopped.
    Stopped,
    /// Running.
    Running,
    /// Quiesced.
    Quiesced,
    /// Exited.
    Exited(ExitStatus),
}

/// Unverified or pending capture cannot authorize child mutations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildCaptureState {
    /// Unverified.
    Unverified,
    /// Pending.
    Pending,
    /// Armed.
    Armed,
    /// Quiesced.
    Quiesced,
}

/// Addresses belong to the tracee at runtime, never to persistent object identity.
#[derive(Clone, Debug)]
pub struct ExecutableMapping {
    /// Start.
    pub start: u64,
    /// Length.
    pub length: u64,
    /// File offset.
    pub file_offset: u64,
    /// Object.
    pub object: Option<ObjectId>,
    /// Logical path.
    pub logical_path: Option<BytePath>,
    /// Protection.
    pub protection: Prot,
}

/// Inventory only; native installation and instruction repair belong to Platform.
#[derive(Clone, Debug)]
pub struct BreakpointState {
    /// Address.
    pub address: u64,
    /// Original instruction.
    pub original_instruction: Vec<u8>,
    /// Armed.
    pub armed: bool,
}

#[derive(Clone, Debug)]
/// Thread state.
pub struct ThreadState {
    /// Thread.
    pub thread: ThreadId,
    /// Stop reason.
    pub stop_reason: Option<StopReason>,
    /// Current syscall.
    pub current_syscall: Option<InterceptedSyscall>,
    /// Scratch.
    pub scratch: Option<ScratchAllocation>,
    /// Saved instruction.
    pub saved_instruction: Option<SavedInstruction>,
    /// Saved registers.
    pub saved_registers: Option<RegisterSet>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Stop reason.
pub enum StopReason {
    /// Launch.
    Launch,
    /// Syscall entry.
    SyscallEntry,
    /// Syscall exit.
    SyscallExit,
    /// Child capture.
    ChildCapture,
    /// Exec.
    Exec,
    /// Breakpoint.
    Breakpoint {
        /// Address.
        address: u64,
    },
    /// Signal.
    Signal(i32),
    /// Quiescence.
    Quiescence,
}

#[derive(Clone, Debug)]
/// Scratch allocation.
pub struct ScratchAllocation {
    /// Address.
    pub address: u64,
    /// Capacity.
    pub capacity: u64,
    /// Used.
    pub used: u64,
}

#[derive(Clone, Debug)]
/// Saved instruction.
pub struct SavedInstruction {
    /// Address.
    pub address: u64,
    /// Owned bytes; no UTF-8 conversion is implied.
    pub bytes: Vec<u8>,
}

/// Required order from handoff §5.2, with kernel/emulation alternatives.
/// No transitions are implemented. Abort cannot undo arbitrary completed writes;
/// reconcile against durable intent or retain a stopped, recovery-required run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStage {
    /// Observed entry.
    ObservedEntry,
    /// Decoded.
    Decoded,
    /// Resolved.
    Resolved,
    /// Prepared.
    Prepared,
    /// Kernel executed.
    KernelExecuted,
    /// Emulated.
    Emulated,
    /// Result observed.
    ResultObserved,
    /// Metadata committed.
    MetadataCommitted,
    /// Tracee resumed.
    TraceeResumed,
    /// Aborted.
    Aborted,
    /// Recovery required.
    RecoveryRequired,
}

/// Runtime transaction inventory; prepared physical rewrites are never persisted.
#[derive(Clone, Debug)]
pub struct InterceptedSyscall {
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Stage.
    pub stage: TransactionStage,
    /// Operation.
    pub operation: Option<FsOp>,
    /// Action.
    pub action: Option<ResolvedAction>,
    /// Rewrite.
    pub rewrite: Option<PreparedRewrite>,
    /// Outcome.
    pub outcome: Option<OperationOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Run lifecycle.
pub enum RunLifecycle {
    /// Created.
    Created,
    /// Starting.
    Starting,
    /// Running.
    Running,
    /// Quiescing.
    Quiescing,
    /// Quiesced.
    Quiesced,
    /// Stopping.
    Stopping,
    /// Checkpointing.
    Checkpointing,
    /// Recovering.
    Recovering,
    /// Recovery required.
    RecoveryRequired,
    /// Stopped.
    Stopped,
    /// Handed off.
    HandedOff,
}

#[derive(Clone, Debug)]
/// Handoff request.
pub struct HandoffRequest {
    /// Request a logical checkpoint after the caller establishes quiescence.
    pub checkpoint: CheckpointRequest,
    /// Termination.
    pub termination: TerminationPolicy,
}

/// Reserved for durable publication followed by fenced writer-lease release.
#[derive(Clone, Debug)]
pub struct HandoffReceipt {
    /// Request a logical checkpoint after the caller establishes quiescence.
    pub checkpoint: Checkpoint,
    /// Session.
    pub session: AgentSession,
}

/// Runtime assembly state, separate from persistent checkpoint records.
#[derive(Clone, Debug)]
pub struct SupervisorState {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Lifecycle.
    pub lifecycle: RunLifecycle,
    /// Processes.
    pub processes: ProcessTree,
    /// Session.
    pub session: Option<AgentSession>,
}

/// Bounds on one run's event loop, validated before the target is launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunBudget {
    /// Renew the writer lease at least this often.
    pub renew_after: Duration,
    /// Logical working directory the target was launched in.
    pub cwd: BytePath,
    /// Architecture and ABI label negotiated with the platform provider.
    pub architecture: umbra_core::Architecture,
    /// Abi.
    pub abi: String,
    /// Lowest descriptor number this run's interposer may be issued, and the
    /// `RLIMIT_NOFILE` ceiling the kernel's own allocation was fenced to.
    ///
    /// `Some` exactly when the run is *routed* -- storage with no kernel-visible
    /// path, whose tracee operations umbra services itself -- because the two are
    /// set together by `run` from the same evidence. It is therefore also the
    /// switch that keeps a rewrite-backed run's event loop byte-identical to what
    /// it was: with `None` the supervisor binds no routing, allocates no
    /// descriptor and touches `ProcessContext::fds` exactly as often as before,
    /// which is never.
    pub descriptor_floor: Option<u32>,
}

/// Contracts only: no backend-kind enum, concrete backend import, or provider I/O.
pub struct Supervisor {
    platform: PlatformSession,
    namespace: Box<dyn NamespaceSession + Send>,
    agent: Option<Box<dyn Agent>>,
    state: SupervisorState,
    budget: Option<RunBudget>,
    live: u64,
    exited: u64,
    root_status: Option<ExitStatus>,
    renew_at: Option<Instant>,
    operations: BTreeMap<ThreadId, OperationId>,
    /// What a routed operation's *observed success* must do to the issuing
    /// process's descriptor table, recorded at the entry that planned it.
    ///
    /// Applied at the exit, never at the entry, and that ordering is the contract
    /// [`ProcessContext`] already states: the logical context is "updated only
    /// after observed success". A descriptor inserted at the entry would exist for
    /// an open that the exit reported failed, and a position advanced there would
    /// move for a write that did not happen.
    routed: BTreeMap<ThreadId, RoutedEffect>,
    /// The output buffer of the directory read currently stopped at its entry,
    /// shared with the `DirectoryEncoder` injected into the namespace.
    ///
    /// `FsOp::ReadDir` says how many bytes may move and never where they go --
    /// that is the ABI's knowledge and the overlay has none of it -- so the
    /// address travels beside the operation exactly as a routed request's
    /// runtime details do.
    directory: crate::directory::DirectoryBuffer,
    /// Set once an interception, provider or authority failure makes further
    /// resumes unsafe. Nothing is resumed after this, in any code path.
    poisoned: bool,
}

/// The descriptor-table effect one routed operation has once it has succeeded.
///
/// One variant per piece of logical process state routing services, and each one
/// owns a different half of it. There is deliberately no variant for "nothing to
/// do": an operation with no effect records no entry, so the map's contents are
/// the set of pending effects rather than a log.
#[derive(Clone, Debug)]
enum RoutedEffect {
    /// A routed `open` succeeded: bind the descriptor the namespace resolved.
    Opened(umbra_core::TracedFd, umbra_core::FdState),
    /// A routed `read` or `write` succeeded: advance the position by the byte
    /// count the *outcome* reported, never by the count the call requested.
    Advanced(umbra_core::TracedFd),
    /// A routed `close` succeeded: release the binding.
    Closed(umbra_core::TracedFd),
    /// A routed `fchdir` succeeded: move the logical working directory to the
    /// descriptor's own logical path.
    ///
    /// The descriptor rather than the path, so the path is read out of
    /// `ProcessContext::fds` at the moment the move is applied. Recording the
    /// path at the entry would let a `close` between the entry and the exit
    /// leave this holding a name the process no longer has open.
    ChangedCwd(umbra_core::TracedFd),
    /// A routed `chdir` succeeded: move the logical working directory to the
    /// absolute logical path the namespace resolved its operand to.
    ///
    /// The path rather than a descriptor, which is the one way this differs from
    /// [`Self::ChangedCwd`] and is forced by what the two calls are given.
    /// `fchdir` names something already in `ProcessContext::fds`, so the path can
    /// be looked up at the moment the move is applied and cannot go stale in the
    /// caller's hands. `chdir` names a path operand and binds nothing, so there
    /// is no later place to read the answer from -- the resolution happened in
    /// the namespace, which is where symlink expansion, whiteout traversal and
    /// the logical-root containment check live, and it is carried here rather
    /// than redone.
    ///
    /// It is an absolute *logical* path for `ChangedCwd`'s reason, checked the
    /// same way where it is applied: `cwd` is the anchor relative resolution is
    /// measured against, so a relative value would anchor against itself.
    MovedCwd(umbra_core::BytePath),
}

/// Returned ownership does not imply providers were closed or a clean shutdown.
pub struct SupervisorParts {
    /// Platform.
    pub platform: PlatformSession,
    /// Namespace.
    pub namespace: Box<dyn NamespaceSession + Send>,
    /// Agent, absent for a raw command run.
    pub agent: Option<Box<dyn Agent>>,
    /// State.
    pub state: SupervisorState,
}

impl Supervisor {
    /// Assemble any Storage/Journal implementations into the standard overlay.
    /// No I/O, lease acquisition, worker threads, or tracees are started.
    pub fn new(
        run_id: RunId,
        platform: PlatformSession,
        storage: Box<dyn Storage>,
        journal: Box<dyn Journal>,
        agent: Option<Box<dyn Agent>>,
    ) -> Self {
        Self::with_namespace(
            run_id,
            platform,
            standard_namespace(storage, journal),
            agent,
        )
    }

    /// Inject an alternative namespace owning its own storage/journal access.
    pub fn with_namespace(
        run_id: RunId,
        platform: PlatformSession,
        namespace: Box<dyn NamespaceSession + Send>,
        agent: Option<Box<dyn Agent>>,
    ) -> Self {
        Self {
            platform,
            namespace,
            agent,
            state: SupervisorState {
                run_id,
                lifecycle: RunLifecycle::Created,
                processes: ProcessTree::default(),
                session: None,
            },
            budget: None,
            live: 0,
            exited: 0,
            root_status: None,
            renew_at: None,
            operations: BTreeMap::new(),
            routed: BTreeMap::new(),
            directory: crate::directory::DirectoryBuffer::default(),
            poisoned: false,
        }
    }

    /// State.
    pub fn state(&self) -> &SupervisorState {
        &self.state
    }

    /// Into parts.
    pub fn into_parts(self) -> SupervisorParts {
        SupervisorParts {
            platform: self.platform,
            namespace: self.namespace,
            agent: self.agent,
            state: self.state,
        }
    }

    /// Describe an adapter session's launch. Storage, authority, journal and
    /// enforcement are prepared by [`run`]; this only asks the injected adapter
    /// for its plan, and fails when no adapter was selected.
    pub fn launch(&mut self, request: &AgentLaunchRequest) -> Result<ProcessHandle> {
        let agent = self.agent.as_ref().ok_or_else(|| {
            UmbraError::new(
                umbra_core::ErrorKind::InvalidState,
                "supervisor.launch",
                "no agent adapter was selected for this run",
            )
        })?;
        let _plan = agent.launch_plan(request)?;
        Err(UmbraError::not_implemented("supervisor.launch.agent_plan"))
    }

    /// Reject new children/mutations and stop the entire tree at known boundaries.
    pub fn quiesce(&mut self) -> Result<QuiescedTree> {
        Err(UmbraError::not_implemented("supervisor.quiesce"))
    }

    /// Coordinate orderly agent exit and remaining descendants. Termination alone
    /// does not establish a clean checkpoint or authorize lease release.
    pub fn stop(&mut self, _policy: TerminationPolicy) -> Result<()> {
        Err(UmbraError::not_implemented("supervisor.stop"))
    }

    /// Require quiescence, reconcile transactions, flush tracee files, storage,
    /// journal and manifest, then durably publish an immutable logical snapshot.
    pub fn checkpoint(&mut self, _request: &CheckpointRequest) -> Result<Checkpoint> {
        Err(UmbraError::not_implemented("supervisor.checkpoint"))
    }

    /// Orderly stop, durable clean checkpoint, then release fenced writer authority.
    pub fn handoff(&mut self, _request: &HandoffRequest) -> Result<HandoffReceipt> {
        Err(UmbraError::not_implemented("supervisor.handoff"))
    }

    /// Reopen an existing run, reacquire exclusive ownership, and report whether
    /// what the last session left behind can be reconciled.
    ///
    /// **Associated, not a method, and that is forced rather than chosen.** A
    /// `Supervisor` reaches storage and the journal only through its
    /// `NamespaceSession`, which exposes neither `open_run` nor `acquire_writer`
    /// nor any journal call — so `&mut self` could not open anything. Reopening
    /// is composition, and composition is what [`crate::run::resume`] does, in the
    /// same shape and the same file as [`crate::run::run`].
    ///
    /// Returning [`ResumeOutcome`] rather than a `ProcessHandle` is the other
    /// half of that: a reopen classifies and closes, it does not launch. Relaunch
    /// from a checkpoint is a separate capability that checkpoint-based recovery
    /// would have to exist for first, and it does not.
    ///
    /// `Ok` means the reopen succeeded, **not** that the run is usable — read
    /// [`ResumeOutcome::recovery_required`].
    pub fn resume(spec: ResumeSpec) -> Result<ResumeOutcome> {
        crate::run::resume(spec)
    }

    /// Reconcile unfinished effects using stable IDs and durable intent. Unsafe
    /// takeover or uncertain effects must leave the run stopped.
    ///
    /// The same operation as [`Supervisor::resume`], delegating to it rather than
    /// restating it, because there is no earlier point at which the two could
    /// diverge: the verdict is produced by `bind`, and `bind` happens after
    /// storage, the writer lease and the journal are already open. There is no
    /// "recovery mode" to enter beforehand — a run that needs recovery and one
    /// that does not are opened identically and tell you apart only afterwards.
    /// The two names are kept because the caller's *intent* differs, and a
    /// `recover` that redirected to `resume` silently would be worse than one
    /// that says it is the same call.
    ///
    /// Reconciliation itself — repairing the tree rather than detecting that it
    /// cannot be trusted — remains unimplemented. This reports; it does not
    /// repair.
    pub fn recover(spec: ResumeSpec) -> Result<ResumeOutcome> {
        Self::resume(spec)
    }
}
