//! The bounded event loop on the debug-control thread.
//!
//! Ordering per stopped syscall entry is: decode through the negotiated ABI,
//! resolve through the namespace, prepare (which journals intent and performs any
//! copy-up or creation), ask the platform to prepare a physical rewrite, apply it,
//! and only then resume to the matching exit. At the exit the observed outcome is
//! recorded, the transaction is committed or aborted, and the thread is resumed.
//!
//! Every failure in that chain poisons the run: nothing is resumed afterwards, the
//! tree is terminated by the caller, and the original error is preserved. A
//! mutation is never allowed to proceed because a step could not be completed.

use std::time::{Duration, Instant};

use umbra_core::{
    AbortReason, EmulatedResult, ErrorKind, ExitStatus, FsOp, LaunchSpec, OperationId,
    OperationOutcome, ProcessContext, ProcessHandle, ResolvedAction, Result, ResumeCommand,
    ResumeMode, TaskId, TerminationPolicy, ThreadId, TraceEvent, UmbraError,
};
use umbra_platform::{TraceControl, TraceMemory};
use uuid::Uuid;

use crate::{
    ChildCaptureState, ProcessLifecycle, ProcessState, RunBudget, RunLifecycle, StopReason,
    Supervisor, ThreadState,
};

/// Adapter binding the ABI decoder to one stopped task for the length of a decode.
struct TaskMemory<'a> {
    control: &'a mut dyn TraceControl,
    task: TaskId,
    namespace: &'a mut dyn umbra_overlay::NamespaceSession,
    renew_at: &'a mut Option<Instant>,
    interval: Option<Duration>,
}

fn renew_due(
    namespace: &mut dyn umbra_overlay::NamespaceSession,
    renew_at: &mut Option<Instant>,
    interval: Option<Duration>,
) -> Result<()> {
    let (Some(deadline), Some(interval)) = (*renew_at, interval) else {
        return Ok(());
    };
    let now = Instant::now();
    if now >= deadline {
        namespace.renew_writer()?;
        // Account for renewal's own provider latency, rather than adding it to
        // the next interval after the call returns.
        *renew_at = Some(now + interval);
    }
    Ok(())
}

impl TraceMemory for TaskMemory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        renew_due(self.namespace, self.renew_at, self.interval)?;
        self.control.read_memory(self.task, address, out)
    }
}

fn error(kind: ErrorKind, operation: &str, context: impl Into<String>) -> UmbraError {
    UmbraError::new(kind, operation, context)
}

impl Supervisor {
    /// Launch a fully prepared command under tracing and take ownership of its tree.
    ///
    /// Preparation — storage, writer authority, journal, namespace binding and the
    /// rendered profile inside `spec` — has already happened. The platform returns
    /// only once the target is stopped, so the first resume is ours.
    pub fn launch_prepared(
        &mut self,
        spec: LaunchSpec,
        budget: RunBudget,
    ) -> Result<ProcessHandle> {
        if self.state.lifecycle != RunLifecycle::Created {
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.launch_prepared",
                "this supervisor has already started a run",
            ));
        }
        self.state.lifecycle = RunLifecycle::Starting;
        let process = self.platform.control.launch(spec)?;
        self.renew_at = Some(Instant::now() + budget.renew_after);
        self.budget = Some(budget);
        self.state.processes.root = Some(process);
        self.track_process(process.0, None);
        self.live = 1;
        self.state.lifecycle = RunLifecycle::Running;
        Ok(process)
    }

    /// Drive the tree to completion.
    ///
    /// The loop ends when every supervised process has exited, never merely when
    /// the root does: a descendant outliving its parent still holds writable
    /// descriptors into the run.
    pub fn run(&mut self) -> Result<()> {
        if self.state.lifecycle != RunLifecycle::Running {
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.run",
                "no launched tree to drive",
            ));
        }
        while self.live > 0 {
            self.step()?;
        }
        self.state.lifecycle = RunLifecycle::Stopped;
        Ok(())
    }

    /// Service renewal, then process at most one event.
    pub fn step(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.step",
                "run is poisoned; no further events may be serviced",
            ));
        }
        self.service_renewal()?;
        let event = self.platform.control.next_event().inspect_err(|_| {
            // A lost or timed-out event channel means we can no longer prove what
            // the tree is doing. Nothing may be resumed after this point.
            self.poisoned = true;
        })?;
        self.handle_event(event)
    }

    /// Dispatch one event and issue at most one resume for it.
    pub fn handle_event(&mut self, event: TraceEvent) -> Result<()> {
        let result = self.service_renewal().and_then(|()| self.dispatch(event));
        if result.is_err() {
            self.poisoned = true;
            self.state.lifecycle = RunLifecycle::RecoveryRequired;
        }
        result
    }

    fn dispatch(&mut self, event: TraceEvent) -> Result<()> {
        match event {
            TraceEvent::ThreadStarted { task, thread } => {
                self.track_thread(task, thread, StopReason::Launch)?;
                self.resume_thread(thread)
            }
            TraceEvent::ThreadExited { task, thread } => {
                if let Some(process) = self.state.processes.processes.get_mut(&task) {
                    process.threads.remove(&thread);
                }
                self.operations.remove(&thread);
                Ok(())
            }
            TraceEvent::Child { parent, child, .. } => {
                // The child is stopped before its first instruction; count it
                // exactly once even if ThreadStarted arrives for it separately.
                if !self.state.processes.processes.contains_key(&child.0) {
                    self.track_process(child.0, Some(parent));
                    self.live += 1;
                }
                if let Some(process) = self.state.processes.processes.get_mut(&parent) {
                    process.children.insert(child.0);
                }
                Ok(())
            }
            TraceEvent::Exec {
                task,
                thread,
                exec_generation,
            } => {
                let process = self.process_mut(task)?;
                process.context.exec_generation = exec_generation;
                // Close-on-exec descriptors are gone and mappings are replaced;
                // the backend re-arms interception before exposing this stop.
                process.context.fds.retain(|_, fd| !fd.flags.close_on_exec);
                process.mappings.clear();
                process.breakpoints.clear();
                self.track_thread(task, thread, StopReason::Exec)?;
                self.resume_thread(thread)
            }
            TraceEvent::Exit { task, status } => {
                let process = self
                    .state
                    .processes
                    .processes
                    .get_mut(&task)
                    .ok_or_else(|| {
                        error(
                            ErrorKind::InvalidState,
                            "supervisor.exit",
                            "exit for an uncaptured task",
                        )
                    })?;
                if matches!(process.lifecycle, ProcessLifecycle::Exited(_)) {
                    return Ok(());
                }
                process.lifecycle = ProcessLifecycle::Exited(status);
                for thread in process.threads.keys() {
                    self.operations.remove(thread);
                }
                process.threads.clear();
                if Some(ProcessHandle(task)) == self.state.processes.root {
                    self.root_status = Some(status);
                }
                self.exited += 1;
                debug_assert!(self.live > 0, "known process exit must have been counted");
                self.live = self.live.saturating_sub(1);
                Ok(())
            }
            TraceEvent::Signal {
                task,
                thread,
                signal,
            } => {
                self.track_thread(task, thread, StopReason::Signal(signal))?;
                self.service_renewal()?;
                self.platform.control.resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: Some(signal),
                })
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                registers,
            } => self.syscall_entry(task, thread, registers),
            TraceEvent::SyscallExit {
                task,
                thread,
                outcome,
            } => self.syscall_exit(task, thread, outcome),
        }
    }

    fn syscall_entry(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        mut registers: umbra_core::RegisterSet,
    ) -> Result<()> {
        if self.operations.contains_key(&thread) {
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.syscall_entry",
                "entry received while an operation is still awaiting its exit",
            ));
        }
        self.track_thread(task, thread, StopReason::SyscallEntry)?;
        let operation = {
            let mut memory = TaskMemory {
                control: &mut *self.platform.control,
                task,
                namespace: &mut *self.namespace,
                renew_at: &mut self.renew_at,
                interval: self.budget.as_ref().map(|b| b.renew_after),
            };
            self.platform.abi.decode_entry(&registers, &mut memory)?
        };
        // `None` is a positively classified non-filesystem call, never an unknown
        // syscall: the decoder errors on anything it cannot classify.
        let Some(operation) = operation else {
            return self.resume_thread(thread);
        };
        let mutation = matches!(
            umbra_overlay::dispatch(&operation),
            umbra_overlay::Dispatch::Materialise | umbra_overlay::Dispatch::Whiteout
        );
        let context = self.process(task)?.context.clone();
        self.service_renewal()?;
        let action = match self.namespace.resolve(&context, &operation) {
            Ok(action) => action,
            Err(e) if e.kind == ErrorKind::NotFound && !mutation => {
                // The namespace says nothing exists at this logical path. The base
                // layer is the host filesystem itself, so letting the unmodified
                // read run produces exactly the outcome the namespace predicts.
                // This equivalence holds only for that base; a base that differs
                // from the host requires emulated denial instead.
                return self.resume_thread(thread);
            }
            Err(e) => return Err(e),
        };
        // A denial executes no syscall, touches no storage and mutates nothing, so
        // it needs no journaled preparation and no operation slot: answer the
        // tracee here, before minting an OperationId. This is only reachable for a
        // whiteout-hidden non-mutating path; every other NotFound still resumes the
        // tracee's own syscall in the resolve arm above.
        if let ResolvedAction::Deny(errno) = action {
            let result = EmulatedResult {
                outcome: OperationOutcome::Failure(errno),
                memory_writes: vec![],
            };
            // The backend must skip the trapped syscall, not merely rewrite its
            // return registers. On Darwin arm64 the entry stop is the `svc` itself
            // and `emulate_result` steps PC past it; a backend that cannot do this
            // (the Linux stub's `emulate_result` returns an error) fails closed
            // here rather than resuming the call it was told to refuse.
            self.platform.abi.emulate_result(&mut registers, &result)?;
            self.service_renewal()?;
            return self
                .platform
                .control
                .set_registers(thread, &registers)
                .and_then(|()| self.resume_thread(thread));
        }
        let id = OperationId(Uuid::new_v4());
        self.service_renewal()?;
        let prepared = self.namespace.prepare(id, &action)?;
        self.operations.insert(thread, id);
        match &prepared.action {
            ResolvedAction::Rewrite(physical) => {
                self.apply_rewrite(task, thread, &mut registers, physical, &operation)?;
                self.resume_thread(thread)
            }
            ResolvedAction::AllowBaseRead => self.resume_thread(thread),
            // Emulation still requires the platform to skip the original trap.
            // Rewriting return registers alone would resume the very syscall the
            // namespace refused, so this fails closed. The emulated-result paths
            // (`ReadLink`, logical-symlink `Stat`) are a separate wiring gap, not
            // #49; `Deny` is handled above and never reaches here.
            ResolvedAction::Deny(_) | ResolvedAction::Emulate(_) => Err(error(
                ErrorKind::UnsupportedCapability,
                "supervisor.syscall_entry",
                "safe syscall emulation is not implemented for this action",
            )),
        }
    }

    fn apply_rewrite(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        registers: &mut umbra_core::RegisterSet,
        physical: &umbra_core::PhysicalOperation,
        original: &FsOp,
    ) -> Result<()> {
        let [target] = physical.paths.as_slice() else {
            return Err(error(
                ErrorKind::UnsupportedCapability,
                "supervisor.rewrite",
                "multi-path rewrites are not supported on this surface",
            ));
        };
        // The prepared operation, not the one the tracee issued: the namespace has
        // already created or copied up the target and cleared create/exclusive, so
        // replacing only the path pointer would re-run stale O_EXCL semantics.
        let _ = original;
        self.service_renewal()?;
        let plan = self.platform.control.prepare_rewrite(
            thread,
            &target.path.0,
            physical.operation.clone(),
        )?;
        for write in &plan.memory_writes {
            self.service_renewal()?;
            self.platform
                .control
                .write_memory(task, write.address, &write.bytes)?;
        }
        self.platform.abi.apply_rewrite(registers, &plan)?;
        self.service_renewal()?;
        self.platform.control.set_registers(thread, registers)
    }

    fn syscall_exit(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        outcome: OperationOutcome,
    ) -> Result<()> {
        self.track_thread(task, thread, StopReason::SyscallExit)?;
        let Some(id) = self.operations.remove(&thread) else {
            // An exit for a call we never intercepted: nothing was rewritten, so
            // the kernel result stands as-is.
            return self.resume_thread(thread);
        };
        self.service_renewal()?;
        self.namespace.observe_result(id, &outcome)?;
        self.service_renewal()?;
        match outcome {
            OperationOutcome::Success { .. } => {
                self.namespace.commit(id)?;
            }
            // A kernel errno is an observed syscall outcome, not an interception
            // failure: reconcile the transaction and let the tracee see it.
            // `KernelRefused` is what makes the namespace agree — it names the
            // observed fact instead of synthesising an interception failure, so
            // the namespace reconciles instead of poisoning, the `resume_thread`
            // below is reached, and the tracee's own return register, already
            // carrying the kernel's errno, stands
            // ([#53](https://github.com/invakid404/umbra/issues/53)). No register
            // work is needed here: the rewritten syscall executed, unlike the
            // `Deny` path above, where nothing ran and the result is emulated.
            OperationOutcome::Failure(errno) => {
                self.namespace
                    .abort(id, &AbortReason::KernelRefused(errno))?;
            }
        }
        self.resume_thread(thread)
    }

    fn service_renewal(&mut self) -> Result<()> {
        renew_due(
            &mut *self.namespace,
            &mut self.renew_at,
            self.budget.as_ref().map(|b| b.renew_after),
        )
        .inspect_err(|_| {
            self.poisoned = true;
            self.state.lifecycle = RunLifecycle::RecoveryRequired;
        })
    }

    fn resume_thread(&mut self, thread: ThreadId) -> Result<()> {
        self.service_renewal()?;
        if self.poisoned {
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.resume",
                "refusing to resume a poisoned run",
            ));
        }
        self.platform.control.resume(ResumeCommand {
            thread,
            mode: ResumeMode::Syscall,
            signal: None,
        })
    }

    fn track_process(&mut self, task: TaskId, parent: Option<TaskId>) {
        let budget = self.budget.clone();
        let inherited = parent
            .and_then(|p| self.state.processes.processes.get(&p))
            .map(|p| p.context.clone());
        let context = match inherited {
            // Fork clones the parent's logical context and inherited descriptors.
            Some(mut context) => {
                context.task = task;
                context.parent = parent;
                context
            }
            None => ProcessContext {
                task,
                parent,
                architecture: budget
                    .as_ref()
                    .map(|b| b.architecture.clone())
                    .unwrap_or(umbra_core::Architecture::Unsupported("unnegotiated".into())),
                abi: budget.as_ref().map(|b| b.abi.clone()).unwrap_or_default(),
                exec_generation: 0,
                cwd: budget
                    .as_ref()
                    .map(|b| b.cwd.clone())
                    .unwrap_or_else(|| umbra_core::BytePath::new(b"/".to_vec()).expect("root")),
                root: umbra_core::BytePath::new(b"/".to_vec()).expect("root"),
                fds: Default::default(),
            },
        };
        self.state.processes.processes.insert(
            task,
            ProcessState {
                context,
                children: Default::default(),
                threads: Default::default(),
                mappings: Vec::new(),
                breakpoints: Default::default(),
                child_capture: ChildCaptureState::Armed,
                lifecycle: ProcessLifecycle::Stopped,
            },
        );
    }

    fn track_thread(&mut self, task: TaskId, thread: ThreadId, reason: StopReason) -> Result<()> {
        if !self.state.processes.processes.contains_key(&task) {
            // A stop for a task we never captured means descendant capture was
            // missed; that is exactly the condition that must not be resumed.
            return Err(error(
                ErrorKind::InvalidState,
                "supervisor.track_thread",
                "event for an uncaptured task",
            ));
        }
        let process = self.process_mut(task)?;
        process.lifecycle = ProcessLifecycle::Stopped;
        process
            .threads
            .entry(thread)
            .or_insert_with(|| ThreadState {
                thread,
                stop_reason: None,
                current_syscall: None,
                scratch: None,
                saved_instruction: None,
                saved_registers: None,
            })
            .stop_reason = Some(reason);
        Ok(())
    }

    fn process(&self, task: TaskId) -> Result<&ProcessState> {
        self.state.processes.processes.get(&task).ok_or_else(|| {
            error(
                ErrorKind::InvalidState,
                "supervisor.process",
                "unknown supervised task",
            )
        })
    }

    fn process_mut(&mut self, task: TaskId) -> Result<&mut ProcessState> {
        self.state
            .processes
            .processes
            .get_mut(&task)
            .ok_or_else(|| {
                error(
                    ErrorKind::InvalidState,
                    "supervisor.process",
                    "unknown supervised task",
                )
            })
    }

    /// Root process outcome, once observed.
    pub fn root_status(&self) -> Option<ExitStatus> {
        self.root_status
    }

    /// Number of supervised processes observed exiting.
    pub fn processes_exited(&self) -> u64 {
        self.exited
    }

    /// Whether a failure has made further resumes unsafe.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Terminate the whole supervised tree and reap it.
    ///
    /// Returns success only when the platform confirms termination; the caller
    /// uses that confirmation to decide whether writer authority may be released.
    pub fn terminate_tree(&mut self, policy: TerminationPolicy) -> Result<()> {
        let Some(process) = self.state.processes.root else {
            return Ok(());
        };
        self.state.lifecycle = RunLifecycle::Stopping;
        self.platform.control.terminate(process, policy)?;
        self.live = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::*;
    use umbra_overlay::{NamespaceResolver, NamespaceSession};
    use umbra_platform::{PlatformSession, SyscallAbi, TraceBackend};

    struct Fake;
    impl TraceBackend for Fake {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            unreachable!()
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            unreachable!()
        }
        fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
            panic!("renewal failure must precede memory read")
        }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
            unreachable!()
        }
        fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
            unreachable!()
        }
        fn resume(&mut self, _: ResumeCommand) -> Result<()> {
            Ok(())
        }
    }
    impl TraceControl for Fake {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities::default()
        }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
            unreachable!()
        }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
            Ok(())
        }
    }
    impl SyscallAbi for Fake {
        fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> {
            unreachable!()
        }
        fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
            unreachable!()
        }
        fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> {
            unreachable!()
        }
    }
    impl NamespaceResolver for Fake {
        fn resolve(&mut self, _: &ProcessContext, _: &FsOp) -> Result<ResolvedAction> {
            unreachable!()
        }
    }
    impl NamespaceSession for Fake {
        fn prepare(&mut self, _: OperationId, _: &ResolvedAction) -> Result<PreparedAction> {
            unreachable!()
        }
        fn observe_result(&mut self, _: OperationId, _: &OperationOutcome) -> Result<()> {
            unreachable!()
        }
        fn commit(&mut self, _: OperationId) -> Result<CommitReceipt> {
            unreachable!()
        }
        fn abort(&mut self, _: OperationId, _: &AbortReason) -> Result<()> {
            unreachable!()
        }
        fn checkpoint(&mut self, _: &CheckpointRequest) -> Result<Checkpoint> {
            unreachable!()
        }
        fn renew_writer(&mut self) -> Result<WriterLease> {
            Err(UmbraError::new(
                ErrorKind::LeaseLost,
                "test",
                "renewal failed",
            ))
        }
    }
    fn task(id: u64) -> TaskId {
        TaskId(TaskIdentity {
            native_id: id,
            generation: 1,
        })
    }
    fn supervisor() -> Supervisor {
        let mut s = Supervisor::with_namespace(
            RunId(Uuid::new_v4()),
            PlatformSession {
                control: Box::new(Fake),
                abi: Box::new(Fake),
            },
            Box::new(Fake),
            None,
        );
        s.track_process(task(1), None);
        s.state.processes.root = Some(ProcessHandle(task(1)));
        s.live = 1;
        s.state.lifecycle = RunLifecycle::Running;
        s
    }
    #[test]
    fn fork_keeps_cloexec_dirfd_until_child_exec() {
        let mut s = supervisor();
        s.process_mut(task(1)).unwrap().context.fds.insert(
            TracedFd(3),
            FdState {
                object: ObjectId(Uuid::new_v4()),
                logical_path: Some(BytePath::new(b"/directory".to_vec()).unwrap()),
                directory: true,
                flags: OpenFlags {
                    close_on_exec: true,
                    ..OpenFlags::default()
                },
            },
        );
        s.handle_event(TraceEvent::Child {
            parent: task(1),
            child: ProcessHandle(task(2)),
            kind: ChildKind::Fork,
        })
        .unwrap();
        assert_eq!(
            s.process(task(2)).unwrap().context.fds,
            s.process(task(1)).unwrap().context.fds
        );
        s.handle_event(TraceEvent::Exec {
            task: task(2),
            thread: ThreadId(task(2).0),
            exec_generation: 1,
        })
        .unwrap();
        assert!(s.process(task(2)).unwrap().context.fds.is_empty());
        assert!(s
            .process(task(1))
            .unwrap()
            .context
            .fds
            .contains_key(&TracedFd(3)));
    }
    #[test]
    fn unknown_exit_cannot_complete_a_live_tree_and_duplicates_do_not_count() {
        let mut s = supervisor();
        assert_eq!(
            s.handle_event(TraceEvent::Exit {
                task: task(99),
                status: ExitStatus::Code(0)
            })
            .unwrap_err()
            .kind,
            ErrorKind::InvalidState
        );
        assert_eq!((s.live, s.exited), (1, 0));
        assert!(s.poisoned);
        assert!(s.run().is_err());
        let mut s = supervisor();
        for _ in 0..2 {
            s.handle_event(TraceEvent::Exit {
                task: task(1),
                status: ExitStatus::Code(0),
            })
            .unwrap();
        }
        assert_eq!((s.live, s.exited), (0, 1));
    }
    #[test]
    fn decoder_callback_renews_before_another_provider_read() {
        let mut control = Fake;
        let mut namespace = Fake;
        let mut deadline = Some(Instant::now());
        let mut memory = TaskMemory {
            control: &mut control,
            namespace: &mut namespace,
            task: task(1),
            renew_at: &mut deadline,
            interval: Some(Duration::from_secs(1)),
        };
        assert_eq!(
            memory.read(0, &mut [0]).unwrap_err().kind,
            ErrorKind::LeaseLost
        );
    }

    use std::sync::{Arc, Mutex};

    // What the platform was asked to do, in order, so a test can pin both the
    // emulated value and that registers were installed before the thread ran.
    enum Recorded {
        Emulate(EmulatedResult),
        SetRegisters(ThreadId),
        Resume(ResumeCommand),
    }
    #[derive(Default)]
    struct Recording {
        events: Vec<Recorded>,
    }
    impl Recording {
        fn order(&self) -> Vec<&'static str> {
            self.events
                .iter()
                .map(|e| match e {
                    Recorded::Emulate(_) => "emulate",
                    Recorded::SetRegisters(_) => "set_registers",
                    Recorded::Resume(_) => "resume",
                })
                .collect()
        }
        fn emulated(&self) -> Vec<EmulatedResult> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Recorded::Emulate(r) => Some(r.clone()),
                    _ => None,
                })
                .collect()
        }
        fn set_register_threads(&self) -> Vec<ThreadId> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Recorded::SetRegisters(t) => Some(*t),
                    _ => None,
                })
                .collect()
        }
        fn resumes(&self) -> Vec<ResumeCommand> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Recorded::Resume(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    // A platform double that records what the supervisor drives it to do. Its
    // `decode_entry` returns a fixed `FsOp`, so no real register decoding is
    // needed; every other unused entry point is `unreachable!`.
    struct Recorder {
        op: FsOp,
        log: Arc<Mutex<Recording>>,
    }
    impl TraceBackend for Recorder {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            unreachable!()
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            unreachable!()
        }
        fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
            unreachable!()
        }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
            unreachable!()
        }
        fn set_registers(&mut self, thread: ThreadId, _: &RegisterSet) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .events
                .push(Recorded::SetRegisters(thread));
            Ok(())
        }
        fn resume(&mut self, command: ResumeCommand) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .events
                .push(Recorded::Resume(command));
            Ok(())
        }
    }
    impl TraceControl for Recorder {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities::default()
        }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
            unreachable!()
        }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
            Ok(())
        }
    }
    impl SyscallAbi for Recorder {
        fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> {
            Ok(Some(self.op.clone()))
        }
        fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
            unreachable!()
        }
        fn emulate_result(&self, _: &mut RegisterSet, result: &EmulatedResult) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .events
                .push(Recorded::Emulate(result.clone()));
            Ok(())
        }
    }

    // A namespace double that answers `resolve` with a scripted result. Every
    // journaled entry point panics: a denial or a resumed passthrough must never
    // mint an operation, so reaching one is the bug the tests guard against.
    struct Scripted {
        answer: Result<ResolvedAction>,
    }
    impl NamespaceResolver for Scripted {
        fn resolve(&mut self, _: &ProcessContext, _: &FsOp) -> Result<ResolvedAction> {
            self.answer.clone()
        }
    }
    impl NamespaceSession for Scripted {
        fn prepare(&mut self, _: OperationId, _: &ResolvedAction) -> Result<PreparedAction> {
            panic!("a denial or passthrough must not reach journaled preparation")
        }
        fn observe_result(&mut self, _: OperationId, _: &OperationOutcome) -> Result<()> {
            panic!("no operation was minted to observe")
        }
        fn commit(&mut self, _: OperationId) -> Result<CommitReceipt> {
            panic!("no operation was minted to commit")
        }
        fn abort(&mut self, _: OperationId, _: &AbortReason) -> Result<()> {
            panic!("no operation was minted to abort")
        }
        fn checkpoint(&mut self, _: &CheckpointRequest) -> Result<Checkpoint> {
            unreachable!()
        }
        fn renew_writer(&mut self) -> Result<WriterLease> {
            unreachable!()
        }
    }

    fn scripted_supervisor(
        op: FsOp,
        answer: Result<ResolvedAction>,
    ) -> (Supervisor, Arc<Mutex<Recording>>) {
        let log = Arc::new(Mutex::new(Recording::default()));
        let mut s = Supervisor::with_namespace(
            RunId(Uuid::new_v4()),
            PlatformSession {
                control: Box::new(Recorder {
                    op: op.clone(),
                    log: log.clone(),
                }),
                abi: Box::new(Recorder {
                    op,
                    log: log.clone(),
                }),
            },
            Box::new(Scripted { answer }),
            None,
        );
        s.track_process(task(1), None);
        s.state.processes.root = Some(ProcessHandle(task(1)));
        s.live = 1;
        s.state.lifecycle = RunLifecycle::Running;
        (s, log)
    }

    // A non-mutating `Stat` (dispatch => ReadThrough); (a) and (b) differ only in
    // the scripted resolve answer.
    fn stat_entry() -> (FsOp, TraceEvent) {
        let op = FsOp::Stat {
            dir: DirRef::Cwd,
            path: BytePath::new(b"/probe".to_vec()).unwrap(),
            follow: true,
        };
        let event = TraceEvent::SyscallEntry {
            task: task(1),
            thread: ThreadId(task(1).0),
            registers: RegisterSet::new(Architecture::Aarch64, vec![]).unwrap(),
        };
        (op, event)
    }

    #[test]
    fn absent_not_found_still_resumes_the_tracees_own_syscall() {
        let (op, event) = stat_entry();
        let (mut s, log) = scripted_supervisor(
            op,
            Err(UmbraError::new(
                ErrorKind::NotFound,
                "overlay.resolve",
                "target absent",
            )),
        );
        assert!(s.handle_event(event).is_ok());
        let log = log.lock().unwrap();
        // The tracee's own unrewritten syscall is resumed byte-for-byte: nothing
        // is emulated and no register is installed.
        assert_eq!(log.order(), vec!["resume"]);
        assert!(log.set_register_threads().is_empty());
        assert_eq!(
            log.resumes(),
            vec![ResumeCommand {
                thread: ThreadId(task(1).0),
                mode: ResumeMode::Syscall,
                signal: None,
            }]
        );
        // No operation slot was minted (the Scripted panics did not fire).
        assert!(s.operations.is_empty());
        assert!(!s.is_poisoned());
    }

    // What the namespace was asked to journal on the exit path, so a test can
    // pin the abort *reason* and not merely that abort was called.
    #[derive(Default)]
    struct Journaled {
        observed: Vec<OperationOutcome>,
        aborts: Vec<AbortReason>,
        commits: usize,
    }

    // A namespace double for the syscall-exit path. It mirrors the overlay's
    // real abort contract instead of accepting anything: a mutating transaction
    // is reconcilable only when the reason is a `KernelRefused` naming the errno
    // this session itself observed, and every other reason is an interception
    // failure that errors. Getting the reason right is the supervisor's job
    // ([#53](https://github.com/invakid404/umbra/issues/53)), so the double
    // enforces it end to end rather than rubber-stamping the call. `refuse_abort`
    // models a namespace that cannot reconcile at all, which must still poison.
    struct Journaling {
        log: Arc<Mutex<Journaled>>,
        refuse_abort: bool,
    }
    fn unreconcilable() -> UmbraError {
        UmbraError::new(
            ErrorKind::InvalidState,
            "overlay.abort",
            "aborted effects require reconciliation",
        )
    }
    impl NamespaceResolver for Journaling {
        fn resolve(&mut self, _: &ProcessContext, _: &FsOp) -> Result<ResolvedAction> {
            panic!("only the syscall exit is driven; the entry already resolved")
        }
    }
    impl NamespaceSession for Journaling {
        fn prepare(&mut self, _: OperationId, _: &ResolvedAction) -> Result<PreparedAction> {
            panic!("only the syscall exit is driven; the entry already prepared")
        }
        fn observe_result(&mut self, _: OperationId, outcome: &OperationOutcome) -> Result<()> {
            self.log.lock().unwrap().observed.push(outcome.clone());
            Ok(())
        }
        fn commit(&mut self, _: OperationId) -> Result<CommitReceipt> {
            self.log.lock().unwrap().commits += 1;
            panic!("an observed kernel failure must never commit")
        }
        fn abort(&mut self, _: OperationId, reason: &AbortReason) -> Result<()> {
            let mut log = self.log.lock().unwrap();
            log.aborts.push(reason.clone());
            if self.refuse_abort {
                return Err(unreconcilable());
            }
            let reconcilable = matches!(
                (reason, log.observed.last()),
                (
                    AbortReason::KernelRefused(claimed),
                    Some(OperationOutcome::Failure(observed))
                ) if claimed == observed
            );
            if reconcilable {
                Ok(())
            } else {
                Err(unreconcilable())
            }
        }
        fn checkpoint(&mut self, _: &CheckpointRequest) -> Result<Checkpoint> {
            unreachable!()
        }
        fn renew_writer(&mut self) -> Result<WriterLease> {
            unreachable!()
        }
    }

    fn exit_supervisor(
        op: FsOp,
        refuse_abort: bool,
    ) -> (Supervisor, Arc<Mutex<Recording>>, Arc<Mutex<Journaled>>) {
        let log = Arc::new(Mutex::new(Recording::default()));
        let journaled = Arc::new(Mutex::new(Journaled::default()));
        let mut s = Supervisor::with_namespace(
            RunId(Uuid::new_v4()),
            PlatformSession {
                control: Box::new(Recorder {
                    op: op.clone(),
                    log: log.clone(),
                }),
                abi: Box::new(Recorder {
                    op,
                    log: log.clone(),
                }),
            },
            Box::new(Journaling {
                log: journaled.clone(),
                refuse_abort,
            }),
            None,
        );
        s.track_process(task(1), None);
        s.state.processes.root = Some(ProcessHandle(task(1)));
        s.live = 1;
        s.state.lifecycle = RunLifecycle::Running;
        (s, log, journaled)
    }

    // The shared body of the kernel-refusal cases. The entry path is covered
    // elsewhere, so only the exit is driven: the operation slot the entry would
    // have minted is seeded, which keeps the recorded platform calls below the
    // exit's behaviour alone.
    fn a_refused_syscall_reconciles_and_resumes(op: FsOp, errno: Errno) {
        assert!(
            matches!(
                umbra_overlay::dispatch(&op),
                umbra_overlay::Dispatch::Materialise | umbra_overlay::Dispatch::Whiteout
            ),
            "the contract under test is the Materialise/Whiteout class"
        );
        let thread = ThreadId(task(1).0);
        let (mut s, log, journaled) = exit_supervisor(op, false);
        s.operations.insert(thread, OperationId(Uuid::new_v4()));
        s.handle_event(TraceEvent::SyscallExit {
            task: task(1),
            thread,
            outcome: OperationOutcome::Failure(errno),
        })
        .unwrap();
        // The run continues: no poison latch and no recovery lifecycle.
        assert!(!s.is_poisoned());
        assert_eq!(s.state.lifecycle, RunLifecycle::Running);
        // The slot is released, so the thread's next entry is not rejected as
        // one still awaiting its exit.
        assert!(s.operations.is_empty());
        let journaled = journaled.lock().unwrap();
        // Assert the reason's value, not just the call: a synthetic
        // `Failed(..)` here is exactly the pre-#53 behaviour, and a bare count
        // would not notice it coming back.
        assert!(
            matches!(journaled.aborts.as_slice(), [AbortReason::KernelRefused(e)] if *e == errno),
            "the observed errno must reach the namespace as the abort reason"
        );
        assert_eq!(journaled.observed, vec![OperationOutcome::Failure(errno)]);
        assert_eq!(journaled.commits, 0);
        let log = log.lock().unwrap();
        // The rewritten syscall executed, so the tracee's own return register
        // already carries the kernel's errno: the exit resumes it and emulates
        // nothing. Contrast the `Deny` path, where no syscall ran.
        assert_eq!(log.order(), vec!["resume"]);
        assert!(log.set_register_threads().is_empty());
        assert_eq!(
            log.resumes(),
            vec![ResumeCommand {
                thread,
                mode: ResumeMode::Syscall,
                signal: None,
            }]
        );
    }

    // Case (a): a metadata mutation the kernel refuses with EPERM, which is what
    // an unprivileged tracee gets routinely. `Chmod` also pins that the fix is
    // keyed on the dispatch class and not on the variants the overlay engine
    // happens to resolve today.
    #[test]
    fn a_kernel_refused_chmod_is_reconciled_and_the_tracee_keeps_its_errno() {
        a_refused_syscall_reconciles_and_resumes(
            FsOp::Chmod {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/file".to_vec()).unwrap(),
                mode: 0o600,
                follow: true,
            },
            Errno(1),
        );
    }

    // Case (c): the data path, to show the reconciliation is class-wide rather
    // than metadata-shaped.
    #[test]
    fn a_kernel_refused_write_is_reconciled_and_the_tracee_keeps_its_errno() {
        a_refused_syscall_reconciles_and_resumes(
            FsOp::Write {
                fd: TracedFd(3),
                length: 4096,
                offset: None,
            },
            Errno(28),
        );
    }

    // Case (d) at supervisor level: reconciliation is the namespace's verdict to
    // give, not an error the supervisor may swallow. When the namespace cannot
    // reconcile, the exit still fails closed -- the run poisons, the lifecycle
    // latches, and nothing is resumed.
    #[test]
    fn an_unreconcilable_abort_still_poisons_the_run_and_resumes_nothing() {
        let thread = ThreadId(task(1).0);
        let (mut s, log, journaled) = exit_supervisor(
            FsOp::Chmod {
                dir: DirRef::Cwd,
                path: BytePath::new(b"/file".to_vec()).unwrap(),
                mode: 0o600,
                follow: true,
            },
            true,
        );
        s.operations.insert(thread, OperationId(Uuid::new_v4()));
        assert_eq!(
            s.handle_event(TraceEvent::SyscallExit {
                task: task(1),
                thread,
                outcome: OperationOutcome::Failure(Errno(1)),
            })
            .unwrap_err()
            .kind,
            ErrorKind::InvalidState
        );
        assert!(s.is_poisoned());
        assert_eq!(s.state.lifecycle, RunLifecycle::RecoveryRequired);
        assert_eq!(journaled.lock().unwrap().aborts.len(), 1);
        assert!(
            log.lock().unwrap().order().is_empty(),
            "a run that could not reconcile must resume nothing"
        );
    }

    #[test]
    fn whiteout_hidden_path_is_denied_with_enoent_not_resumed_unrewritten() {
        let (op, event) = stat_entry();
        let (mut s, log) = scripted_supervisor(op, Ok(ResolvedAction::Deny(Errno::ENOENT)));
        assert!(s.handle_event(event).is_ok());
        let log = log.lock().unwrap();
        // Emulate first, then install the errno-bearing registers, then resume:
        // the registers must carry the errno before the thread runs.
        assert_eq!(log.order(), vec!["emulate", "set_registers", "resume"]);
        // Assert the emulated value, not just the call: a bare count would
        // survive mutating the errno the tracee is handed.
        assert_eq!(
            log.emulated(),
            vec![EmulatedResult {
                outcome: OperationOutcome::Failure(Errno::ENOENT),
                memory_writes: vec![],
            }]
        );
        assert_eq!(log.set_register_threads(), vec![ThreadId(task(1).0)]);
        assert_eq!(log.resumes().len(), 1);
        // A denial mints no journaled operation, so nothing awaits an exit slot
        // that would mis-attribute the synthesised SyscallExit.
        assert!(s.operations.is_empty());
        assert!(!s.is_poisoned());
    }
}
