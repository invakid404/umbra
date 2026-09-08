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

use std::time::Instant;

use umbra_core::{
    AbortReason, ErrorKind, ExitStatus, FsOp, LaunchSpec, OperationId, OperationOutcome,
    ProcessContext, ProcessHandle, ResolvedAction, Result, ResumeCommand, ResumeMode, TaskId,
    TerminationPolicy, ThreadId, TraceEvent, UmbraError,
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
}

impl TraceMemory for TaskMemory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
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
        let result = self.dispatch(event);
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
                if Some(ProcessHandle(task)) == self.state.processes.root {
                    self.root_status = Some(status);
                }
                if let Some(process) = self.state.processes.processes.get_mut(&task) {
                    if matches!(process.lifecycle, ProcessLifecycle::Exited(_)) {
                        return Ok(());
                    }
                    process.lifecycle = ProcessLifecycle::Exited(status);
                    for thread in process.threads.keys() {
                        self.operations.remove(thread);
                    }
                    process.threads.clear();
                }
                self.exited += 1;
                self.live = self.live.saturating_sub(1);
                Ok(())
            }
            TraceEvent::Signal {
                task,
                thread,
                signal,
            } => {
                self.track_thread(task, thread, StopReason::Signal(signal))?;
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
        let id = OperationId(Uuid::new_v4());
        let prepared = self.namespace.prepare(id, &action)?;
        self.operations.insert(thread, id);
        match &prepared.action {
            ResolvedAction::Rewrite(physical) => {
                self.apply_rewrite(task, thread, &mut registers, physical, &operation)?;
                self.resume_thread(thread)
            }
            ResolvedAction::AllowBaseRead => self.resume_thread(thread),
            // Denial and emulation both require the platform to skip the original
            // trap. Rewriting return registers alone would resume the very syscall
            // the namespace refused, so this fails closed instead.
            ResolvedAction::Deny(_) | ResolvedAction::Emulate(_) => Err(error(
                ErrorKind::UnsupportedCapability,
                "supervisor.syscall_entry",
                "safe syscall emulation is not implemented; the platform contract \
                 has no way to skip a trapped syscall without executing it",
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
        let plan = self.platform.control.prepare_rewrite(
            thread,
            &target.path.0,
            physical.operation.clone(),
        )?;
        for write in &plan.memory_writes {
            self.platform
                .control
                .write_memory(task, write.address, &write.bytes)?;
        }
        self.platform.abi.apply_rewrite(registers, &plan)?;
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
        self.namespace.observe_result(id, &outcome)?;
        match outcome {
            OperationOutcome::Success { .. } => {
                self.namespace.commit(id)?;
            }
            // A kernel errno is an observed syscall outcome, not an interception
            // failure: reconcile the transaction and let the tracee see it.
            OperationOutcome::Failure(errno) => {
                self.namespace.abort(
                    id,
                    &AbortReason::Failed(
                        error(
                            ErrorKind::Io,
                            "supervisor.syscall_exit",
                            "operation failed in the kernel",
                        )
                        .with_errno(errno),
                    ),
                )?;
            }
        }
        self.resume_thread(thread)
    }

    fn service_renewal(&mut self) -> Result<()> {
        let (Some(deadline), Some(budget)) = (self.renew_at, self.budget.as_ref()) else {
            return Ok(());
        };
        if Instant::now() < deadline {
            return Ok(());
        }
        let interval = budget.renew_after;
        self.namespace.renew_writer().inspect_err(|_| {
            // Without proven authority nothing may resume; the caller terminates.
            self.poisoned = true;
        })?;
        self.renew_at = Some(Instant::now() + interval);
        Ok(())
    }

    fn resume_thread(&mut self, thread: ThreadId) -> Result<()> {
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
                context.fds.retain(|_, fd| !fd.flags.close_on_exec);
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
