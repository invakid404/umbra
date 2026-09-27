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
    ChildCaptureState, ProcessLifecycle, ProcessState, RoutedEffect, RunBudget, RunLifecycle,
    StopReason, Supervisor, ThreadState,
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

/// Whether a descriptor is one umbra issued, rather than one the kernel did.
///
/// **This is the Rust twin of `umbra_owns` in `umbra_interpose.c`, and it is
/// named after it deliberately** so the correspondence is nominal rather than
/// only semantic. The C applies the test before interposing `read`, `write` and
/// `close`; this applies it before resolving an `fstat`, which reaches umbra
/// through the tracer instead because libsystem calls its own entry point and
/// there is nothing to interpose. Term for term:
///
/// ```text
/// C:     umbra_active()   &&  fd >= 0   &&  (uint64_t)fd >= umbra_control.floor
/// Rust:  floor.is_some()  &&  fd.0 >= 0 &&  (fd.0 as u32)  >= floor
/// ```
///
/// The *value* cannot drift: `descriptor_floor` is `DESCRIPTOR_FENCE`, which is
/// also the `RLIMIT_NOFILE` written into `umbra_control.floor`, and there is one
/// constant and one assignment in the tree. The *comparison* is written twice,
/// in two languages, and what links them is
/// `umbra_platform_macos::abi::tests::the_interposers_descriptor_test_is_the_one_the_supervisor_applies`,
/// which pins the C's text and names this function.
///
/// **That pairing is a change-detector, not a proof of agreement**, and the
/// table above is the only thing asserting the two predicates mean the same:
/// edit the C and this function consistently wrong and both tests pass. The
/// cited test's own doc comment sets out the three parts and their limits.
///
/// `floor` is `None` on a rewrite-backed run, which has no virtual descriptors
/// at all: every descriptor is the kernel's, so nothing is owned and every
/// `fstat` passes through exactly as it did before the call was breakpointed.
///
/// Negative descriptors are excluded rather than wrapped. `fd` is a signed
/// `int` and `-1` is what a failed `open` returns; casting it to `u32` would
/// make it `0xffff_ffff`, which is above any plausible floor and would have
/// umbra answer for a descriptor that does not exist.
fn umbra_owns_descriptor(floor: Option<u32>, fd: umbra_core::TracedFd) -> bool {
    matches!(floor, Some(floor) if fd.0 >= 0 && (fd.0 as u32) >= floor)
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
        // **The production directory encoder, injected here and nowhere else.**
        //
        // `Engine::resolve_directory` refuses without one, so a routed run whose
        // tracee reads a directory depends on this call having happened. It is
        // on `launch_prepared` rather than beside each `standard_namespace`
        // because this is the single path every launched run takes -- one site
        // cannot drift from another the way `install()`'s symbol list drifted
        // from `intercept()`'s number list ([#116]), which is the defect this
        // whole slice is a second instance of.
        //
        // Gated on the run being *routed* and on the ABI the encoder's wire
        // format was measured against. A rewrite-backed run's directory
        // descriptors are kernel descriptors that never reach the namespace, so
        // injecting there would add an encoder nothing calls; a future ABI would
        // get Darwin's `getattrlistbulk` records, which is worse than getting
        // the refusal.
        //
        // [#116]: https://github.com/invakid404/umbra/issues/116
        if budget.descriptor_floor.is_some()
            && budget.abi == umbra_platform::dirents::DARWIN_ARM64_ABI
        {
            self.namespace.set_directory_encoder(Box::new(
                crate::directory::AbiDirectoryEncoder::new(self.directory.clone()),
            ))?;
        }
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
        // **The descriptor fence, applied to the one descriptor-relative call
        // the tracer carries rather than the interposer.**
        //
        // `read`, `write` and `close` reach umbra through the interposer, which
        // applies this test in C before it ever traps: below the floor is a
        // kernel descriptor and goes to libc unchanged, at or above it is
        // umbra's (`umbra_interpose.c`, VIRTUAL DESCRIPTORS). `fstat` has no
        // interposed entry point -- libsystem calls its own -- so it is
        // breakpointed at the libc stub instead, and *every* `fstat` in the
        // process traps here, including libsystem's own on kernel descriptors.
        //
        // Measured: XPC bundle resolution and `_os_feature_table_once` `fstat`
        // fd 3 during startup, and `cat` `fstat`s fd 1. Resolving those through
        // the namespace would answer `EBADF` for descriptors the kernel owns and
        // is perfectly able to answer -- turning a working program into a broken
        // one, which is the opposite of what routing this call is for.
        //
        // So the same test is applied here, against the same constant: the run's
        // `descriptor_floor`, which is `DESCRIPTOR_FENCE` and is also the
        // tracee's `RLIMIT_NOFILE`. No second allocator and no second number
        // space -- this reads the one the fence already established. A
        // rewrite-backed run has no floor and therefore no virtual descriptors
        // at all, so every `fstat` on one passes through, which is exactly what
        // it did before this call was breakpointed.
        //
        // **This is the twin of `umbra_owns` in `umbra_interpose.c`**, and the
        // two must agree. The value cannot drift -- one `DESCRIPTOR_FENCE`, one
        // assignment tree-wide -- but the comparison is written twice, in two
        // languages, and nothing the compiler does links them. What links them
        // is `abi.rs`'s
        // `the_interposers_descriptor_test_is_the_one_the_supervisor_applies`,
        // which pins the C's text and names this site. Change one, change both.
        //
        // The predicate is `umbra_owns_descriptor`, named after its twin so the
        // correspondence is nominal rather than only semantic, and tested over
        // both branches -- which matters because only one of them is reachable
        // on a registry that can run without a live NFSv4 fixture.
        //
        // **The set grew from one call to four and the test is unchanged.**
        // `fstat` was the first call breakpointed at a libc stub rather than
        // interposed; `getattrlistbulk`, `fchdir` and the two `close` forms
        // joined it for the same reason -- `fts` issues all of them from inside
        // `libsystem_c`, where `DYLD_INTERPOSE` cannot reach. Every one of them
        // therefore traps for *every* descriptor in the process, umbra's and the
        // kernel's alike, and every one of them must pass the kernel's through
        // untouched.
        //
        // This is also what keeps `/bin/ls` working on the rewrite-backed
        // registries, where it already worked before this slice: those runs have
        // no `descriptor_floor`, so `umbra_owns_descriptor` is false for every
        // descriptor and all four calls resume into the kernel exactly as they
        // did when they were not breakpointed at all.
        //
        // **That sentence was false when it was first written, and what made it
        // false is worth keeping rather than quietly deleting.** It was true of
        // three calls. `getattrlistbulk` also carries an *attribute request*,
        // and this slice first refused an unserved one inside `decode_entry` --
        // which runs before this test. So the refusal fired for every descriptor
        // in the process, on runs with no floor at all, and `ls -l`/`-t`/`-i`/
        // `-p`/`-S`/`-F`/`-s`/`-n` went from exit 0 on `--local-dev` to a
        // stopped run. The invariant now holds for all four because the
        // attribute refusal moved *below* this test, to
        // `unserved_directory_request`. Anything later that can refuse a
        // breakpointed call before this point breaks it again, and the way to
        // tell is that the four calls have no other gate: this is it.
        if let FsOp::Fstat { fd }
        | FsOp::ReadDir { fd, .. }
        | FsOp::Fchdir { fd }
        | FsOp::Close { fd } = &operation
        {
            let floor = self.budget.as_ref().and_then(|b| b.descriptor_floor);
            if !umbra_owns_descriptor(floor, *fd) {
                return self.resume_thread(thread);
            }
        }
        // **The `fstat` output buffer, bound once and here.** An `fstat` this
        // far in will be answered by umbra, so the `struct stat` has to be
        // written into the tracee -- and where it goes is the tracee's own
        // argument, which the tracee can get wrong.
        //
        // `fstat(vfd, NULL)` is an ordinary program bug and Darwin answers it
        // `EFAULT`. Binding it through `io_binding` carries that errno as a
        // *value* instead of an error, so the refusal is answered to the tracee
        // below rather than stopping the run -- the rule `routing_for`'s doc
        // comment states, reached from a second entry point. Measured before
        // that rule existed: `read(fd, NULL, 4)` ended the run.
        //
        // Bound *before* `set_routed_request` so a refusal can skip `resolve`
        // without stranding a routing binding the namespace consumes on
        // resolution, and bound once so the seam below needs no second call --
        // the ABI is a subprocess behind IPC and `fstat` is issued by every
        // locale open in every process.
        let stat_buffer = match &operation {
            FsOp::Fstat { .. } => Self::io_binding(&*self.platform.abi, &registers)?,
            _ => None,
        };
        if let Some(Err(errno)) = stat_buffer {
            return self.deny_to_tracee(thread, &mut registers, errno);
        }
        // **The directory output buffer, bound here for `fstat`'s reasons and
        // published rather than carried.** `FsOp::ReadDir` names a byte bound
        // and no address, so the address has to travel beside it; the encoder
        // the namespace calls during `resolve` reads it from the cell this sets.
        //
        // Cleared on every other operation, not merely overwritten on the next
        // directory read: a binding left behind is one a later `ReadDir` could
        // answer from after this call was refused, writing one directory's
        // entries into another call's buffer. That is the rule
        // `routing_for` states for `set_routed_request` and
        // `set_readlink_buffer`, reached from a third entry point.
        match &operation {
            FsOp::ReadDir { fd, .. } if !context.fds.contains_key(fd) => {
                // **A descriptor umbra never issued is `EBADF` before it is
                // anything else, and that is the kernel's own precedence.**
                //
                // The attribute check runs at the syscall entry and
                // `routed_binding`'s `EBADF` inside `resolve`, so without this
                // an unserved request on a stale descriptor answered `ENOTSUP` —
                // telling a program probing a dead descriptor "Operation not
                // supported" when the kernel, and umbra's own `fstat`,
                // `fchdir` and `close`, all say "Bad file descriptor". Skipping
                // the validation here lets `resolve` answer, which is where the
                // other three already answer it.
                //
                // Reading `context.fds` here is the descriptor allocator's own
                // idiom (`allocate_descriptor`), not a new channel.
                self.directory.clear()?
            }
            FsOp::ReadDir { .. } => match Self::io_binding(&*self.platform.abi, &registers)? {
                Some(Ok(buffer)) => {
                    // **The attribute request, refused here and not one function
                    // earlier, and that placement is the whole of finding F1.**
                    //
                    // `getattrlistbulk` declares the layout of its own reply in
                    // an `attrlist` block, so a set umbra cannot encode has to be
                    // refused -- but this slice first refused it inside
                    // `decode_entry`, which runs *before* the descriptor test
                    // above. Measured side by side against master: `ls -l`,
                    // `-t`, `-i`, `-p`, `-S`, `-F`, `-s` and `-n` exited 0 on
                    // `--local-dev` and stopped the run afterwards, on kernel
                    // descriptors, on runs with no `descriptor_floor` at all.
                    //
                    // Being *after* the fence is what makes the refusal correct
                    // rather than merely survivable, and an errno alone would not
                    // have done it: measured, the kernel **serves** the wider set
                    // (`common=0x82079e0b file=0x0000022d` returns records,
                    // errno 0). On a descriptor umbra does not own the only right
                    // answer is to get out of the way, which the fence now does.
                    // On one umbra *does* own, `ENOTSUP` is umbra's own answer
                    // and is bound to the tracee rather than ending the run.
                    if let Some(errno) = self.unserved_directory_request(task, &registers)? {
                        return self.deny_to_tracee(thread, &mut registers, errno);
                    }
                    self.directory.set(buffer)?
                }
                // The tracee's own null, overflowing or empty buffer. Answered
                // to it as an errno rather than raised, exactly as
                // `fstat(vfd, NULL)` is.
                Some(Err(errno)) => return self.deny_to_tracee(thread, &mut registers, errno),
                None => {
                    return Err(error(
                        ErrorKind::ProtocolMismatch,
                        "supervisor.directory",
                        "the ABI reported no output buffer for a directory read",
                    ))
                }
            },
            _ => self.directory.clear()?,
        }
        // Routed runs only. On a rewrite-backed run `descriptor_floor` is `None`
        // and none of this executes, so that path reaches `resolve` exactly as it
        // did before.
        let routed = self
            .budget
            .as_ref()
            .and_then(|b| b.descriptor_floor)
            .is_some();
        if routed {
            let routing = self.routing_for(task, thread, &operation, &registers, &context)?;
            self.service_renewal()?;
            self.namespace.set_routed_request(routing)?;
        }
        self.service_renewal()?;
        let action = match self.namespace.resolve(&context, &operation) {
            Ok(action) => action,
            // **A routed operation must never be resumed.** The fallback below
            // resumes the tracee's own syscall; for a routed operation that
            // syscall is umbra's reserved trap, which Darwin's `nosys` answers
            // with `ENOSYS` (78) while posting `SIGSYS`. Measured: a tracee
            // opening an absent path exited **78**, where POSIX says `ENOENT`
            // (2) -- the most common file-operation failure there is, and one
            // programs branch on.
            //
            // The routed answer is produced by `resolve` itself, as
            // `Deny(ENOENT)`, and arrives on the `Ok` path below. **This used to
            // be an arm here matching `NotFound` on a routed run, and that was
            // too wide:** it also caught a `NotFound` raised by `Storage` for an
            // object the overlay's own journal says it committed, and reported a
            // namespace/store disagreement to the program as an ordinary missing
            // file. The engine now decides where it knows -- `hidden_or` and
            // `routed_binding` -- so a `Storage` `NotFound` still reaches the arm
            // below and still stops the run.
            Err(e) if e.kind == ErrorKind::NotFound && !mutation && !routed => {
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
        // tracee here, before minting an OperationId.
        //
        // **Eight things reach it**, and the list is kept current because it is
        // the only place they are enumerated together. Every one names the
        // `umbra-overlay` *function* that produces it, never a line: this block
        // carried twelve line citations that were all correct on master and all
        // wrong the moment this change grew `engine.rs`, and one of them landed
        // on a different `Deny` -- which reads as a confirmation and is not.
        // A function name survives an insertion above it; see the note on that
        // remedy's limits over `citations_in_this_file_name_items_not_lines`.
        //
        // 1. A whiteout-hidden non-mutating path -> `ENOENT`, from
        //    `Overlay::hidden_or`.
        // 2. A routed run whose *name does not resolve* -> `ENOENT`, from
        //    `Overlay::hidden_or` for a path operation and
        //    `Overlay::routed_binding` for a descriptor whose name stopped
        //    resolving. Narrower than it reads: this is name resolution only,
        //    never a `NotFound` the `Storage` raised for an object the journal
        //    says it committed -- that one reaches the `Err(e)` arm and stops
        //    the run, which is the whole point of deciding it in the engine
        //    rather than here.
        // 3. `EBADF` on a routed run -- a descriptor umbra never issued or one
        //    carrying no logical path (`Overlay::routed_binding`), a directory
        //    or one opened without the access the operation needs
        //    (`resolve_routed_read`, `resolve_routed_write`), or one an `fstat`,
        //    an `fchdir` or a directory read named that none of those resolves
        //    (`resolve_routed_fstat`, `resolve_routed_fchdir`,
        //    `resolve_directory`). The last two are this slice's, and
        //    `resolve_directory` reached `routed_binding` only after review:
        //    it resolved the descriptor itself and raised `StaleHandle`, which
        //    is an `Err` that stopped the run where its three siblings answered
        //    the tracee.
        // 4. A routing input the tracee could not supply, carried as a value
        //    through `RoutedInput` rather than as an error: `EFAULT` for a bad
        //    transfer buffer (`resolve_routed_read`, `resolve_routed_write`)
        //    and `EMFILE` for an exhausted fenced descriptor range (the routed
        //    `FsOp::Open` arm of `Overlay::resolve`).
        // 5. `ENOTSUP` for an intercepted path operation on a routed run that
        //    `rewrite` would have had to name a shadow path for -- `fstatat`,
        //    `faccessat`, `renameat` and the rest on an object this run created
        //    or copied up. The `_ if self.routed()?` fallthrough of
        //    `Overlay::resolve`'s action match. Without it those stopped the run.
        // 6. `ENOTDIR` for a descriptor-relative call that needs a directory and
        //    did not get one -- `fchdir` onto a non-directory
        //    (`resolve_routed_fchdir`) and a directory read on one
        //    (`resolve_directory`). A **seventh errno class**, added by this
        //    slice; before it, the only descriptor refusals were `EBADF` and
        //    `ENOENT`.
        // 7. `ENOTSUP` for a directory read whose caller asked for an attribute
        //    set this ABI cannot encode -- `Supervisor::unserved_directory_request`
        //    here, not the engine. It is the one entry on this list the
        //    *namespace* never sees, and the one that must be evaluated after
        //    the descriptor floor test rather than before it: the kernel serves
        //    the wider set, so a descriptor umbra does not own has to reach it.
        // 8. `ENOTSUP` for an `FsOp::SetTimes` whose storage cannot set times --
        //    the `STORAGE_TIMESTAMP_FIDELITY_V1` check in `Overlay::resolve`'s
        //    validation match. **Added by the same change that added this
        //    line**, and it belongs here for the reason the header gives: three
        //    of the four backends refuse a timestamp update, and reaching that
        //    refusal from inside `prepare` would flush an intent for a change
        //    that never happened and stop the run.
        //
        // A seventh tracee-visible refusal exists and deliberately does *not*
        // reach this point: a bad `fstat` output pointer is bound and answered
        // above, before `set_routed_request`, because skipping `resolve` after
        // binding a routing request would strand it. It answers through the
        // same `deny_to_tracee` and is named here so the enumeration is whole.
        //
        // On a rewrite-backed run every other `NotFound` still resumes the
        // tracee's own syscall in the arm above.
        if let ResolvedAction::Deny(errno) = action {
            return self.deny_to_tracee(thread, &mut registers, errno);
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
            // Emulation requires the platform to skip the original trap. Rewriting
            // return registers alone would resume the very syscall the namespace
            // answered, so a backend whose `emulate_result` cannot step past the
            // trap (the Linux stub) fails here rather than running it.
            //
            // The exit is not synthesised here and does not need to be: the macOS
            // backend's `resume` observes that the PC has moved past the entry it
            // recorded and emits the matching `SyscallExit` itself, carrying the
            // outcome out of the registers this call just set. So the transaction
            // this entry opened is observed and committed by the ordinary exit
            // path, in the ordinary order, and `self.operations` is drained there
            // like every other intercepted call's.
            ResolvedAction::Emulate(result) => {
                for write in self
                    .routed_stat_write(&operation, stat_buffer)?
                    .iter()
                    .chain(&result.memory_writes)
                {
                    self.service_renewal()?;
                    self.platform
                        .control
                        .write_memory(task, write.address, &write.bytes)?;
                }
                self.platform.abi.emulate_result(&mut registers, result)?;
                self.service_renewal()?;
                self.platform.control.set_registers(thread, &registers)?;
                self.record_routed_effect(thread, &operation)?;
                self.resume_thread(thread)
            }
            // `Deny` is answered above, before an operation ID is minted, and
            // never reaches here.
            ResolvedAction::Deny(_) => Err(error(
                ErrorKind::InvalidState,
                "supervisor.syscall_entry",
                "a denial reached the prepared-action dispatch",
            )),
        }
    }

    /// Build the routing binding for one entry on a routed run.
    ///
    /// Three operations need one and the rest need an empty binding rather than
    /// no call: `set_routed_request` consumes on resolution, so leaving a stale
    /// binding in place for the next operation is the failure mode this avoids.
    ///
    /// **A refusal the tracee caused is bound, not returned.** This runs before
    /// `resolve`, and `resolve` is the only thing that can produce an answer the
    /// tracee sees -- so returning `Err` for a bad pointer or a full descriptor
    /// range stopped the whole run over an ordinary program bug. Measured, before
    /// this: `read(fd, NULL, 4)` ended the run where Darwin answers `EFAULT`.
    /// `Err` is now reserved for a failure of *interception* rather than of the
    /// request.
    fn routing_for(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        operation: &FsOp,
        registers: &umbra_core::RegisterSet,
        context: &ProcessContext,
    ) -> Result<umbra_core::RoutedRequest> {
        let mut routing = umbra_core::RoutedRequest::default();
        match operation {
            FsOp::Open { .. } => routing.descriptor = Some(self.allocate_descriptor(context)?),
            // `None` is a zero-length read, which the namespace answers without a
            // buffer; `Some(Err(..))` is the tracee's own bad pointer.
            FsOp::Read { .. } => {
                routing.read_buffer = Self::io_binding(&*self.platform.abi, registers)?
            }
            FsOp::Write { length, .. } => {
                // The bytes, read out of the stopped tracee. `FsOp::Write` carries
                // their count and never their content, and the namespace is what
                // persists them, so this is the one place they cross.
                let buffer = match Self::io_binding(&*self.platform.abi, registers)? {
                    Some(Err(errno)) => {
                        routing.write_bytes = Some(Err(errno));
                        return Ok(routing);
                    }
                    Some(Ok(buffer)) => buffer,
                    None => {
                        // A zero-length write. The namespace still needs a binding
                        // whose length matches the operation, which for zero bytes
                        // is an empty one rather than an absent one.
                        if *length == 0 {
                            routing.write_bytes = Some(Ok(Vec::new()));
                            return Ok(routing);
                        }
                        return Err(error(
                            ErrorKind::ProtocolMismatch,
                            "supervisor.routing",
                            "the ABI reported no source buffer for a nonempty routed write",
                        ));
                    }
                };
                if buffer.length as u64 != *length {
                    return Err(error(
                        ErrorKind::ProtocolMismatch,
                        "supervisor.routing",
                        "the ABI's write buffer length disagrees with the decoded operation",
                    ));
                }
                let mut bytes = vec![0u8; buffer.length as usize];
                self.service_renewal()?;
                let read = self
                    .platform
                    .control
                    .read_memory(task, buffer.address, &mut bytes);
                routing.write_bytes = Some(match read {
                    Ok(()) => Ok(bytes),
                    Err(e) => Err(self.address_fault_or(thread, e)?),
                });
            }
            _ => {}
        }
        Ok(routing)
    }

    /// Decide whether a failed read of the tracee's own transfer buffer was the
    /// tracee's bad pointer or umbra's broken tracer, and answer only the first.
    ///
    /// A source buffer umbra cannot read *is* the tracee's bad pointer, and
    /// `EFAULT` is what the kernel would have answered for the same `write` --
    /// but only when the tracer is otherwise healthy. An earlier shape mapped
    /// **every** failure of that read to `EFAULT`, so a provider-IPC failure, a
    /// dead debugger port or an expired session deadline during it was reported
    /// to the program as a bad pointer and the run carried on to die one syscall
    /// later somewhere else. That is the diagnosis-destroying misattribution
    /// this slice fixed in the other direction for `ENOSYS`.
    ///
    /// So the tracer is probed before the errno is believed: reading the
    /// stopped thread's registers needs the same provider connection, the same
    /// debugger port and the same unexpired deadline, and names no tracee
    /// address at all. If that succeeds, the failure was specific to the address
    /// the tracee supplied; if it does not, the original error propagates and
    /// stops the run. One extra round trip, on the failure path only.
    ///
    /// A kind other than `Io` propagates too: the memory proxy answers
    /// `ProtocolMismatch` for a malformed request, which is umbra's own wiring
    /// rather than anything the program did.
    fn address_fault_or(
        &mut self,
        thread: ThreadId,
        error: UmbraError,
    ) -> Result<umbra_core::Errno> {
        if error.kind != ErrorKind::Io {
            return Err(error);
        }
        match self.platform.control.registers(thread) {
            Ok(_) => Ok(umbra_core::Errno(14)),
            Err(_) => Err(error),
        }
    }

    /// The ABI's buffer binding for this entry, with a tracee-caused refusal
    /// carried as a value.
    ///
    /// `io_buffer` already knows the POSIX answer for a null or overflowing
    /// transfer buffer -- it attaches `Errno(14)` to the error it returns. This
    /// separates that from a genuine interception failure, which carries no
    /// errno and still stops the run.
    fn io_binding(
        abi: &dyn umbra_platform::SyscallAbi,
        registers: &umbra_core::RegisterSet,
    ) -> Result<Option<umbra_core::RoutedInput<umbra_core::IoBuffer>>> {
        match abi.io_buffer(registers) {
            Ok(Some(buffer)) => Ok(Some(Ok(buffer))),
            Ok(None) => Ok(None),
            Err(e) => match e.errno {
                Some(errno) => Ok(Some(Err(errno))),
                None => Err(e),
            },
        }
    }

    /// `Some(errno)` if the stopped directory read asks for an attribute set
    /// this ABI cannot encode, having read the request block out of the tracee.
    ///
    /// Split across the two layers that own the two halves: the ABI answers
    /// where the block is and checks the part of the request that lives in a
    /// register, and this reads the block and checks the attribute bitmaps
    /// against the one shape the encoder produces. Both are consulted only for a
    /// descriptor that passed the fence above, which is the ordering F1 was.
    ///
    /// A refusal carries its errno as a *value*, exactly as `io_binding` does
    /// and for the same reason: asking for attributes umbra does not model is
    /// something an ordinary program does, not a failure of interception.
    ///
    /// **Every exit, and which of them may end the run.** This enumeration
    /// exists because the pass that added this function gave an errno to one of
    /// the validation's two refusal arms and not the other, and nothing here
    /// said what the set of exits was. Fixing the arm the reviewer found would
    /// have left the next one to be found the same way.
    ///
    /// 1. The ABI reports no block. **Ends the run** -- and that is a wiring
    ///    fault, not a tracee's. The default `directory_request` refuses rather
    ///    than reporting `None` for exactly this reason, so an ABI that decodes
    ///    461 without modelling the request cannot reach here silently; this arm
    ///    survives for an ABI that returns `Ok(None)` for a `ReadDir` anyway,
    ///    which is the same wiring fault `io_binding`'s `None` is already a
    ///    `ProtocolMismatch` for.
    /// 2. The ABI refuses **with** an errno -- a bad `attrlist` pointer
    ///    (`EFAULT`) or an unserved `options` word (`ENOTSUP`). Answered.
    /// 3. The ABI refuses **without** one -- a register read that failed, i.e.
    ///    interception itself broke. **Ends the run**, correctly.
    /// 4. The block is the wrong width. **Ends the run**: a provider
    ///    contradicting its own ABI's fixed-width struct, and the bound is
    ///    checked *before* the read so a bad width cannot make this copy an
    ///    arbitrary span out of the tracee.
    /// 5. Reading the block faults. **Ends the run.** This is the pre-existing
    ///    tree-wide unmapped-pointer class -- `fstat`, `open` and `read` all do
    ///    the same, measured -- tracked as its own issue rather than fixed here.
    /// 6. The bitmaps are unserved. Answered `ENOTSUP`; the sweep
    ///    `every_attribute_request_refusal_carries_a_bindable_errno` is what
    ///    keeps this arm from becoming (3).
    /// 7. Served. `Ok(None)`, and the read proceeds.
    ///
    /// So the run-ending exits are exactly: no block, a broken register read, a
    /// provider lying about the width, and a faulting read. **None of them is
    /// reachable by a tracee's choice of attribute list** -- which is the
    /// property the defect broke and the sweep test now pins.
    ///
    /// **That claim is about this check, and it was once written about the
    /// request, which is a different and false thing.** A `getattrlistbulk`
    /// request carries more tracee-supplied values than the attribute list, and
    /// the one this function never sees is the output buffer's size: it reaches
    /// `Overlay::resolve_directory`'s bound check and `dirents::encode`'s
    /// capacity arm, both **downstream of here**, and both ended the run until
    /// the pass that corrected this sentence. An enumeration scoped to one
    /// function cannot speak for a request that outlives it.
    ///
    /// The enumeration that does speak for the whole request is the input table
    /// over `umbra_platform_macos::abi`'s `directory_bytes`, which walks all six
    /// tracee-supplied values of a 461 to their dispositions. This list and that
    /// table are the two halves: exits here, inputs there.
    fn unserved_directory_request(
        &mut self,
        task: TaskId,
        registers: &umbra_core::RegisterSet,
    ) -> Result<Option<umbra_core::Errno>> {
        let block = match self.platform.abi.directory_request(registers) {
            Ok(Some(block)) => block,
            // Exit 1. **A hard failure, matching `io_binding`'s `None` on this
            // same operation**, which is already a `ProtocolMismatch`. Two
            // optional methods answering for one syscall, one treated as a
            // wiring fault and the other as "nothing to check", is how a
            // validation gets skipped silently -- and skipping this one serves
            // umbra's own record layout for whatever bitmap the tracee actually
            // asked for.
            Ok(None) => {
                return Err(error(
                    ErrorKind::ProtocolMismatch,
                    "supervisor.directory",
                    "the ABI reported no attribute request block for a directory read",
                ))
            }
            Err(e) => {
                return match e.errno {
                    Some(errno) => Ok(Some(errno)),
                    None => Err(e),
                }
            }
        };
        if block.length as usize != umbra_platform::dirents::ATTRLIST_BYTES {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "supervisor.directory",
                "the ABI reported an attribute request block of the wrong width",
            ));
        }
        let mut attrlist = [0u8; umbra_platform::dirents::ATTRLIST_BYTES];
        self.service_renewal()?;
        self.platform
            .control
            .read_memory(task, block.address, &mut attrlist)?;
        match umbra_platform::dirents::RequestedAttributes::decode(&attrlist) {
            Ok(_) => Ok(None),
            Err(e) => match e.errno {
                Some(errno) => Ok(Some(errno)),
                None => Err(e),
            },
        }
    }

    /// The descriptor number a routed `open` will be answered with.
    ///
    /// The lowest free number in `[floor, floor + floor)`, which is POSIX's own
    /// rule applied to umbra's half of the descriptor space. Free is decided
    /// against this process's own bindings only, and that is sufficient rather
    /// than approximate: the fence lowered the tracee's `RLIMIT_NOFILE` -- soft
    /// and hard -- to exactly this floor before the target exec'd, so the kernel
    /// cannot issue a number at or above it to this process or any descendant,
    /// and the tracee cannot raise the boundary back.
    ///
    /// Exhausting the range is **bound as `EMFILE`**, not returned as an error.
    /// That is the honest POSIX answer to "no descriptor is available", and it
    /// reaches the tracee because `resolve` turns a bound refusal into a
    /// `Deny` -- an earlier shape returned `Err` here and stopped the whole run.
    /// `Err` from this function means the run has no descriptor fence at all,
    /// which is a wiring fault rather than a request the tracee made.
    fn allocate_descriptor(
        &self,
        context: &ProcessContext,
    ) -> Result<umbra_core::RoutedInput<umbra_core::TracedFd>> {
        let floor = self
            .budget
            .as_ref()
            .and_then(|budget| budget.descriptor_floor)
            .ok_or_else(|| {
                error(
                    ErrorKind::InvalidState,
                    "supervisor.routing",
                    "a routed open needs this run's descriptor fence",
                )
            })?;
        // **Bounded, and the bound is the one the tracee was told about.**
        //
        // `DESCRIPTOR_FENCE` is a single constant used twice: as the tracee's
        // `RLIMIT_NOFILE` and as this floor. So `[floor, floor + floor)` makes
        // umbra's range exactly as large as the kernel's, which is what lets the
        // advertised limit mean something: the tracee can hold at most
        // `descriptor_limit` kernel descriptors *and* at most `descriptor_limit`
        // routed ones, and `ProcessContext::fds` is bounded by the second.
        //
        // It used to scan to `i32::MAX`, which made two claims false at once.
        // The tracee was told `getrlimit = 4096` while umbra would hand out far
        // more -- an advertised limit that was not the enforced one -- and the
        // `EMFILE` refusal was unreachable: measured, a tracee held **400**
        // routed descriptors with no refusal and the practical ceiling was the
        // run deadline, not the fence. Capping here makes the refusal reachable
        // and the map bounded.
        let ceiling = i64::from(floor) * 2;
        let mut candidate = i64::from(floor);
        // A descriptor is an `i32`; the fence keeps this range far below that
        // ceiling, and the assertion is that nothing widened it.
        debug_assert!(ceiling <= i64::from(i32::MAX));
        while candidate < ceiling {
            let fd = umbra_core::TracedFd(candidate as i32);
            if !context.fds.contains_key(&fd) {
                return Ok(Ok(fd));
            }
            candidate += 1;
        }
        // `EMFILE` is 24 on Darwin and on Linux alike, which is what makes it
        // safe to name here; see `Overlay::routed_binding` for the same argument
        // about `EBADF`.
        Ok(Err(umbra_core::Errno(24)))
    }

    /// Answer the stopped tracee with an errno and resume it, executing nothing.
    ///
    /// The backend must **skip** the trapped syscall, not merely rewrite its
    /// return registers. On Darwin arm64 the entry stop is the `svc` itself and
    /// `emulate_result` steps PC past it; a backend that cannot do this (the
    /// Linux stub's `emulate_result` returns an error) fails closed here rather
    /// than resuming the call it was told to refuse.
    ///
    /// One function rather than two copies, because there are two places a
    /// refusal is decided: `resolve` answering `Deny`, and the entry path
    /// finding that a binding the *tracee's own request* made impossible -- a
    /// null `struct stat` pointer -- must be answered rather than raised. Both
    /// reach the tracee the same way, and a second copy of this sequence is the
    /// shape that produced the `intercept()`/`install()` drift.
    fn deny_to_tracee(
        &mut self,
        thread: ThreadId,
        registers: &mut umbra_core::RegisterSet,
        errno: umbra_core::Errno,
    ) -> Result<()> {
        let result = EmulatedResult {
            outcome: OperationOutcome::Failure(errno),
            memory_writes: vec![],
        };
        self.platform.abi.emulate_result(registers, &result)?;
        self.service_renewal()?;
        self.platform
            .control
            .set_registers(thread, registers)
            .and_then(|()| self.resume_thread(thread))
    }

    /// The `struct stat` image a routed `fstat` must leave in the tracee, if
    /// this operation is one.
    ///
    /// **Two halves of one answer, and neither side can produce it alone.** The
    /// namespace resolved the descriptor to a logical object and knows its
    /// metadata; only the ABI knows what that metadata looks like in a tracee's
    /// memory. `EmulatedResult` carries bytes, so the join has to happen on this
    /// side of the namespace boundary -- which is also why `FsOp::Fstat`'s
    /// resolution deliberately returns no memory write of its own.
    ///
    /// It is the same shape as `record_routed_effect`: a supervisor-side step
    /// that completes a routed operation using something the namespace exposed
    /// beside the action rather than inside it.
    ///
    /// Every *remaining* failure is an `Err` rather than a skipped write, and
    /// the word remaining is load-bearing: the one failure the **tracee** can
    /// cause -- a bad output pointer -- was already bound as an errno and
    /// answered before `resolve` ran, so nothing that reaches here is the
    /// program's fault. What is left is umbra's own wiring: a resolution that
    /// answered `Emulate` for an `fstat` and then produced no metadata, an ABI
    /// with no stat layout, or an entry naming no output buffer. Each would
    /// leave the tracee's `struct stat` holding whatever was there before while
    /// the call reported success -- a wrong answer rather than a refusal.
    fn routed_stat_write(
        &mut self,
        operation: &FsOp,
        buffer: Option<umbra_core::RoutedInput<umbra_core::IoBuffer>>,
    ) -> Result<Option<umbra_core::MemoryWrite>> {
        if !matches!(operation, FsOp::Fstat { .. }) {
            return Ok(None);
        }
        let stat = self.namespace.routed_stat()?.ok_or_else(|| {
            error(
                ErrorKind::ProtocolMismatch,
                "supervisor.routed_stat",
                "the namespace emulated an fstat without resolving its metadata",
            )
        })?;
        let bytes = self.platform.abi.encode_stat(&stat)?;
        let buffer = match buffer {
            Some(Ok(buffer)) => buffer,
            // Answered to the tracee before `resolve`; an `Emulate` cannot be
            // reached with one outstanding.
            Some(Err(errno)) => {
                return Err(error(
                    ErrorKind::InvalidState,
                    "supervisor.routed_stat",
                    format!("a refused stat buffer ({errno:?}) reached the emulated answer"),
                ))
            }
            None => {
                return Err(error(
                    ErrorKind::ProtocolMismatch,
                    "supervisor.routed_stat",
                    "the ABI decoded an fstat but reports no output buffer for it",
                ))
            }
        };
        // The encoding and the buffer are produced by the same ABI from the same
        // entry, so a disagreement is umbra's own wiring rather than anything the
        // tracee did -- and writing the shorter of the two would leave a partly
        // stale `struct stat` behind.
        if bytes.len() != buffer.length as usize {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "supervisor.routed_stat",
                "the ABI's stat encoding and its output buffer disagree in length",
            ));
        }
        Ok(Some(umbra_core::MemoryWrite {
            address: buffer.address,
            bytes,
        }))
    }

    /// Record what this routed operation's observed success must do to the
    /// issuing process's descriptor table. Applied at the exit, never here.
    fn record_routed_effect(&mut self, thread: ThreadId, operation: &FsOp) -> Result<()> {
        if self
            .budget
            .as_ref()
            .and_then(|b| b.descriptor_floor)
            .is_none()
        {
            return Ok(());
        }
        let effect = match operation {
            FsOp::Open { .. } => match self.namespace.routed_descriptor()? {
                Some((fd, state)) => RoutedEffect::Opened(fd, state),
                // An `Open` the namespace answered without binding a descriptor:
                // a logical-symlink stat or another emulated answer on a run that
                // also routes. Nothing to record.
                None => return Ok(()),
            },
            FsOp::Read { fd, .. } | FsOp::Write { fd, .. } => RoutedEffect::Advanced(*fd),
            FsOp::Close { fd } => RoutedEffect::Closed(*fd),
            FsOp::Fchdir { fd } => RoutedEffect::ChangedCwd(*fd),
            _ => return Ok(()),
        };
        self.routed.insert(thread, effect);
        Ok(())
    }

    /// Apply a routed operation's descriptor effect, after its success.
    fn apply_routed_effect(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        outcome: &OperationOutcome,
    ) -> Result<()> {
        let Some(effect) = self.routed.remove(&thread) else {
            return Ok(());
        };
        let OperationOutcome::Success { return_value } = outcome else {
            // A refused routed operation changes nothing: no descriptor was
            // issued, no position moved, no binding was released.
            return Ok(());
        };
        let context = &mut self.process_mut(task)?.context;
        match effect {
            RoutedEffect::Opened(fd, state) => {
                if *return_value != fd.0 as u64 {
                    return Err(error(
                        ErrorKind::ProtocolMismatch,
                        "supervisor.routing",
                        "the observed open result is not the descriptor the namespace bound",
                    ));
                }
                context.fds.insert(fd, state);
            }
            RoutedEffect::Advanced(fd) => {
                let state = context.fds.get_mut(&fd).ok_or_else(|| {
                    error(
                        ErrorKind::InvalidState,
                        "supervisor.routing",
                        "a routed transfer succeeded on a descriptor that is no longer bound",
                    )
                })?;
                // The observed count, not the requested one. A short transfer
                // moves the position by what actually moved.
                state.offset = state.offset.saturating_add(*return_value);
            }
            RoutedEffect::Closed(fd) => {
                context.fds.remove(&fd);
            }
            // **The one thing that has ever moved `ProcessContext::cwd` after
            // launch.** It was populated once, from `command.cwd`, and read for
            // `DirRef::Cwd` resolution; `FsOp::Chdir`/`Fchdir`/`GetCwd` were
            // classified by `dispatch` and had no handler anywhere, so the
            // logical working directory could not move at all.
            //
            // The path comes from the descriptor's own binding, resolved now
            // rather than at the entry, and an absolute one is required: `cwd`
            // is what relative resolution is anchored against, so a relative
            // value here would anchor against itself.
            //
            // **Measured, and worth stating because it bounds what this buys:**
            // `/bin/ls` never moves anywhere. `fts` `fchdir`s only back to
            // descriptors it opened on the directory it was already in, and
            // calls neither `chdir`(12) nor `getcwd`(326) -- so for `ls` this
            // arm is an identity. It is here because `fchdir` had to be *routed*
            // for its virtual descriptor's sake, and a routed call that reported
            // success while silently declining to do the thing it names would be
            // a worse answer than the refusal it replaced.
            //
            // `Chdir` and `GetCwd` stay declared and inert: nothing decodes
            // 12 or 326, no measured caller on this path issues either, and a
            // handler for a call that cannot arrive is prose describing a path
            // nothing walks.
            RoutedEffect::ChangedCwd(fd) => {
                let state = context.fds.get(&fd).ok_or_else(|| {
                    error(
                        ErrorKind::InvalidState,
                        "supervisor.routing",
                        "a routed fchdir succeeded on a descriptor that is no longer bound",
                    )
                })?;
                let logical = state
                    .logical_path
                    .clone()
                    .filter(|p| p.is_absolute())
                    .ok_or_else(|| {
                        error(
                            ErrorKind::InvalidState,
                            "supervisor.routing",
                            "a routed fchdir succeeded on a descriptor with no absolute \
                             logical path to make the working directory",
                        )
                    })?;
                context.cwd = logical;
            }
        }
        Ok(())
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
                // After the commit, deliberately: the descriptor table must not
                // record a binding for a transaction that failed to commit, and a
                // failed commit stops the run before the tracee sees either.
                self.apply_routed_effect(task, thread, &outcome)?;
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
                self.apply_routed_effect(task, thread, &outcome)?;
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
    /// A platform double whose `directory_request` and tracee memory are the
    /// test's to choose, so `unserved_directory_request` can be driven over
    /// every exit it has.
    ///
    /// The shipped `Fake` cannot do this: its `read_memory` panics by design, to
    /// prove a renewal precedes it. This one serves bytes.
    struct DirectoryFake {
        block: Option<IoBuffer>,
        refusal: Option<UmbraError>,
        attrlist: [u8; umbra_platform::dirents::ATTRLIST_BYTES],
    }
    impl DirectoryFake {
        fn serving() -> Self {
            let mut attrlist = [0u8; umbra_platform::dirents::ATTRLIST_BYTES];
            attrlist[0..2]
                .copy_from_slice(&umbra_platform::dirents::ATTR_BIT_MAP_COUNT.to_le_bytes());
            attrlist[4..8]
                .copy_from_slice(&umbra_platform::dirents::SERVED_COMMONATTR.to_le_bytes());
            attrlist[16..20]
                .copy_from_slice(&umbra_platform::dirents::SERVED_FILEATTR.to_le_bytes());
            Self {
                block: Some(IoBuffer {
                    address: 4096,
                    length: umbra_platform::dirents::ATTRLIST_BYTES as u32,
                }),
                refusal: None,
                attrlist,
            }
        }
        fn into_session(self) -> PlatformSession {
            PlatformSession {
                control: Box::new(DirectoryControl {
                    attrlist: self.attrlist,
                }),
                abi: Box::new(self),
            }
        }
    }
    struct DirectoryControl {
        attrlist: [u8; umbra_platform::dirents::ATTRLIST_BYTES],
    }
    impl TraceBackend for DirectoryControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            unreachable!()
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            unreachable!()
        }
        fn read_memory(&mut self, _: TaskId, _: u64, out: &mut [u8]) -> Result<()> {
            out.copy_from_slice(&self.attrlist[..out.len()]);
            Ok(())
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
    impl TraceControl for DirectoryControl {
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
    impl SyscallAbi for DirectoryFake {
        fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> {
            unreachable!()
        }
        fn directory_request(&self, _: &RegisterSet) -> Result<Option<IoBuffer>> {
            match &self.refusal {
                Some(e) => Err(e.clone()),
                None => Ok(self.block),
            }
        }
        fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
            unreachable!()
        }
        fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> {
            unreachable!()
        }
    }

    /// **Every exit of `unserved_directory_request`, driven.**
    ///
    /// There was no supervisor-level coverage of this function at all -- no ABI
    /// double in the tree produced a directory read -- which is why its two
    /// refusal arms could disagree about carrying an errno and only a live
    /// routed run with a malformed header would have shown it.
    ///
    /// The distinction under test is not "refused or served" but **"answered to
    /// the tracee or fatal to the run"**, because that is the one the defect got
    /// wrong: `Ok(Some(errno))` is answered, `Err` ends the run.
    #[test]
    fn every_exit_of_the_directory_request_check_is_answered_or_deliberately_fatal() {
        let registers = RegisterSet::new(umbra_core::Architecture::Aarch64, vec![0; 272]).unwrap();
        let drive = |fake: DirectoryFake| {
            let mut s = supervisor();
            s.platform = fake.into_session();
            s.unserved_directory_request(task(1), &registers)
        };

        // 7. Served: no refusal, the read proceeds.
        assert_eq!(drive(DirectoryFake::serving()).unwrap(), None);

        // 6. Unserved bitmaps -> answered. The attribute-set arm.
        let mut wide = DirectoryFake::serving();
        wide.attrlist[4..8].copy_from_slice(
            &(umbra_platform::dirents::SERVED_COMMONATTR | 0x0002_0000).to_le_bytes(),
        );
        assert_eq!(drive(wide).unwrap(), Some(Errno(45)));

        // 6. The header arm, which is the defect this test exists for. A
        // `bitmapcount` umbra does not model is answered, never fatal.
        let mut counted = DirectoryFake::serving();
        counted.attrlist[0..2].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(drive(counted).unwrap(), Some(Errno(45)));

        // And `reserved` is a don't-care: garbage there must not refuse at all,
        // because the kernel serves it and a program can leave it uninitialised.
        let mut reserved = DirectoryFake::serving();
        reserved.attrlist[2..4].copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(drive(reserved).unwrap(), None);

        // 2. The ABI refuses with an errno -> answered.
        let mut refused = DirectoryFake::serving();
        refused.refusal = Some(
            UmbraError::new(ErrorKind::UnsupportedCapability, "abi", "options")
                .with_errno(Errno(45)),
        );
        assert_eq!(drive(refused).unwrap(), Some(Errno(45)));

        // 3. The ABI refuses without one -> fatal, correctly: interception broke.
        let mut broke = DirectoryFake::serving();
        broke.refusal = Some(UmbraError::new(
            ErrorKind::Io,
            "abi",
            "register read failed",
        ));
        assert!(drive(broke).is_err());

        // 1. No block at all -> fatal. A wiring fault, not a tracee's.
        let mut absent = DirectoryFake::serving();
        absent.block = None;
        assert!(drive(absent).is_err());

        // 4. A block of the wrong width -> fatal, and refused *before* the read,
        // so a provider cannot make the supervisor copy an arbitrary span.
        let mut wrong = DirectoryFake::serving();
        wrong.block = Some(IoBuffer {
            address: 4096,
            length: 8,
        });
        assert!(drive(wrong).is_err());
    }

    /// This file cites **items**, not lines, and this is what keeps it that way.
    ///
    /// The `Deny` taxonomy above carried twelve citations into
    /// `umbra-overlay`'s `engine.rs`. All twelve were correct when they were
    /// written and all twelve were wrong once this slice grew that file, and
    /// one of them landed on a *different* `Deny` -- which reads as a
    /// confirmation and is not. It happened in the same round that diagnosed
    /// the class elsewhere and abandoned line ranges for it, which is why a
    /// comment saying "name items here" was demonstrably not enough.
    ///
    /// **What this covers:** the reintroduction of a line-number citation
    /// anywhere in `events.rs`. That is one file -- the one where the defect
    /// occurred, twice.
    ///
    /// **What it does not cover, stated because the round-1 report overclaimed
    /// the remedy:**
    ///
    /// * **Renames.** A citation naming `routed_binding` breaks silently if
    ///   that function is renamed, exactly as a line range breaks on an
    ///   insertion. Naming makes the citation survive edits *above* it, not
    ///   edits *to* it. Nothing here checks that a named item exists;
    ///   `include_str!` across a crate boundary would, at the cost of a
    ///   build-time relative path between crates, which was judged the worse
    ///   trade.
    /// * **Other files.** Several in this slice still carry line citations
    ///   deliberately -- `run_fixtures.rs` quotes two stale ranges while
    ///   explaining why ranges were abandoned -- so a blanket ban is not
    ///   expressible as one test.
    /// * **Prose.** The largest stale-documentation class this slice produced
    ///   was claims *about behaviour*, not citations. No citation policy
    ///   reaches those; `memoria check` does, and cannot run in a jj workspace
    ///   with no colocated `.git`.
    #[test]
    fn citations_in_this_file_name_items_not_lines() {
        // Assembled rather than written out, so this test's own message cannot
        // match the pattern it bans.
        let marker = format!("{}{}", ".rs", ':');
        let source = include_str!("events.rs");
        let offenders: Vec<&str> = source
            .lines()
            .filter(|line| {
                line.match_indices(&marker).any(|(at, _)| {
                    line[at + marker.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
                })
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "this file cites a source line by number, which goes stale the next \
             time the cited file grows and gives no signal when it does. Name the \
             function, type or test instead. Offending line(s):\n{}",
            offenders.join("\n")
        );
    }

    /// Both branches of the descriptor fence, including the one no registry
    /// runnable without a live NFSv4 fixture can reach.
    ///
    /// The routed arm -- `floor.is_some()` and `fd >= floor` -- is only taken on
    /// a run whose storage advertises userspace routing, so a rewrite-backed
    /// fixture matrix exercises the *other* arm and nothing else. That asymmetry
    /// is why this is a unit test of the predicate rather than a claim resting
    /// on the end-to-end matrices: as shipped in round 0 the fence was not
    /// merely untested but unreachable, and being able to point at which branch
    /// is proven by what matters more than the total.
    ///
    /// `umbra_owns_descriptor` is the twin of `umbra_owns` in
    /// `umbra_interpose.c`; the C side is pinned by a test in
    /// `umbra-platform-macos`. The cases below are the four the C's three
    /// conjuncts produce, plus the negative-descriptor case that a `u32` cast
    /// would get wrong.
    #[test]
    fn the_descriptor_fence_owns_exactly_the_fenced_range() {
        // No fence: a rewrite-backed run has no virtual descriptors, so umbra
        // owns nothing and every `fstat` resumes to the kernel.
        for fd in [-1, 0, 1, 3, 4095, 4096, 8192, i32::MAX] {
            assert!(
                !umbra_owns_descriptor(None, TracedFd(fd)),
                "an unfenced run must own no descriptor, but claimed {fd}"
            );
        }
        // Fenced: below the floor is the kernel's, at or above it is umbra's.
        // 3 is not arbitrary -- libSystem's pre-`main` `fstat` is on fd 3, and
        // `cat` stats fd 1. Answering either from the namespace would break a
        // working program.
        for fd in [0, 1, 2, 3, 4094, 4095] {
            assert!(
                !umbra_owns_descriptor(Some(4096), TracedFd(fd)),
                "a kernel descriptor must pass through, but umbra claimed {fd}"
            );
        }
        for fd in [4096, 4097, 8191, 8192, i32::MAX] {
            assert!(
                umbra_owns_descriptor(Some(4096), TracedFd(fd)),
                "a fenced descriptor must be umbra's, but it disclaimed {fd}"
            );
        }
        // A negative descriptor is never umbra's, however high the `u32` cast
        // would put it. `-1` casts to `0xffff_ffff`, which is above every
        // plausible floor.
        for fd in [-1, -2, i32::MIN] {
            assert!(
                !umbra_owns_descriptor(Some(4096), TracedFd(fd)),
                "a negative descriptor must never be umbra's, but it claimed {fd}"
            );
        }
    }

    /// A routed budget with a deliberately tiny fence, so the range can be
    /// exhausted in a test rather than only in principle.
    fn routed_budget(fence: u32) -> RunBudget {
        RunBudget {
            renew_after: Duration::from_secs(1),
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
            architecture: Architecture::Aarch64,
            abi: "test-abi".to_owned(),
            descriptor_floor: Some(fence),
        }
    }

    fn bound_fd(path: &[u8]) -> FdState {
        FdState {
            object: ObjectId(Uuid::new_v4()),
            logical_path: Some(BytePath::new(path.to_vec()).unwrap()),
            directory: false,
            flags: OpenFlags::default(),
            offset: 0,
        }
    }

    /// Routed descriptors come from `[floor, floor + floor)` and nowhere else.
    ///
    /// The upper bound is what makes the fence mean anything. Without it the
    /// allocator scanned to `i32::MAX`, so the tracee was told
    /// `getrlimit = <floor>` while umbra would hand out far more, and the map it
    /// keeps per open grew unbounded.
    #[test]
    fn a_routed_descriptor_comes_from_the_fenced_range() {
        let mut s = supervisor();
        s.budget = Some(routed_budget(8));
        let mut context = s.process(task(1)).unwrap().context.clone();
        assert_eq!(
            s.allocate_descriptor(&context).unwrap(),
            Ok(TracedFd(8)),
            "the first routed descriptor is the floor itself"
        );
        // Never below the floor, however empty the table is: below it is the
        // kernel's range, and handing one out would be a wrong-object read.
        context.fds.insert(TracedFd(8), bound_fd(b"/held"));
        assert_eq!(s.allocate_descriptor(&context).unwrap(), Ok(TracedFd(9)));
    }

    /// Exhausting the range answers `EMFILE`, and the run survives it.
    ///
    /// `EMFILE` was converted from a run-stopping `Err` into a bound refusal in
    /// round 1, but the range it guarded was unbounded, so nothing could reach
    /// it: measured, a tracee held **400** routed descriptors with no refusal at
    /// all. With the range capped the refusal is reachable, which is what makes
    /// it worth having.
    #[test]
    fn exhausting_the_routed_range_answers_emfile_rather_than_stopping_the_run() {
        let mut s = supervisor();
        s.budget = Some(routed_budget(8));
        {
            let context = &mut s.process_mut(task(1)).unwrap().context;
            // The whole of `[8, 16)`.
            for number in 8..16 {
                context.fds.insert(TracedFd(number), bound_fd(b"/held"));
            }
        }
        let context = s.process(task(1)).unwrap().context.clone();
        assert_eq!(
            s.allocate_descriptor(&context).unwrap(),
            // 24 is `EMFILE` on Darwin and on Linux alike.
            Err(Errno(24)),
            "a full routed range must refuse rather than issue a number outside it"
        );

        // And releasing one makes exactly that number available again.
        let mut context = context;
        context.fds.remove(&TracedFd(11));
        assert_eq!(s.allocate_descriptor(&context).unwrap(), Ok(TracedFd(11)));
    }

    /// A run that routes nothing has no fence, and asking for a descriptor is a
    /// wiring fault rather than a refusal the tracee should see.
    #[test]
    fn allocating_a_routed_descriptor_without_a_fence_is_an_error_not_an_errno() {
        let s = supervisor();
        let context = s.process(task(1)).unwrap().context.clone();
        assert_eq!(
            s.allocate_descriptor(&context).unwrap_err().kind,
            ErrorKind::InvalidState
        );
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
                offset: 0,
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
    }

    // A namespace double for the syscall-exit path, reporting a **scripted
    // verdict**. It deliberately does not model `Overlay::abort`'s rule: the
    // supervisor only ever *reads* the namespace's verdict and never computes
    // it, so a verdict handed in per construction is the whole of what these
    // tests need from a namespace.
    //
    // The real rule is pinned against a real `Overlay` over real storage by
    // `crates/umbra-supervisor/tests/kernel_refusal.rs`
    // ([#57](https://github.com/invakid404/umbra/issues/57)). That file is the
    // authority; **deleting it un-pins `Overlay::abort`'s rule**, because this
    // double no longer carries a copy of it to fall back on. The copy it used to
    // carry drifted twice — the `created` gate in #53's review, the
    // error-kind flattening in #69's — which is why it is gone.
    //
    // What the tests below therefore cover is the supervisor's half alone: that
    // it sends the observed fact as the abort reason, that it propagates
    // whatever verdict it gets back, and that it fails closed on a refusal.
    struct Journaling {
        log: Arc<Mutex<Journaled>>,
        // What the namespace answers this abort with, decided by whoever built
        // the double rather than computed from the reason or the recorded
        // outcome. `Ok(())` is a reconciliation, `Err(..)` a refusal; the
        // supervisor branches on neither the kind nor the context.
        verdict: Result<()>,
    }
    // The refusal the poison test scripts. Its kind and context are the real
    // overlay's uncorroborated-abort refusal; its operation string is this
    // helper's own, because the engine stamps every error it raises with the
    // operation `"overlay"`. Nothing here is a claim about production — the
    // harness asserts the real error, including the two kinds this one cannot
    // be both of at once.
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
            panic!("an observed kernel failure must never commit")
        }
        fn abort(&mut self, _: OperationId, reason: &AbortReason) -> Result<()> {
            self.log.lock().unwrap().aborts.push(reason.clone());
            self.verdict.clone()
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
        verdict: Result<()>,
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
                verdict,
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
        let (mut s, log, journaled) = exit_supervisor(op, Ok(()));
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

    // The creating `Open` the real overlay reconciles, and the one the
    // resumed-with-its-errno test below drives. Its parent directory exists in
    // neither the shadow nor the base, so `prepare` materialises a directory
    // over nothing and records both it and the file for rollback.
    //
    // No longer argued here: which op the real overlay latches, and why this one
    // does not. `tests/kernel_refusal.rs` demonstrates the reconciliation
    // against a real `Overlay` on this very op, ancestor included. What is left
    // for this helper to say is only what the supervisor sees, which is a
    // verdict it reads and does not compute.
    fn creating_open_under_an_absent_parent() -> FsOp {
        FsOp::Open {
            dir: DirRef::Cwd,
            path: BytePath::new(b"/newdir/fresh".to_vec()).unwrap(),
            flags: OpenFlags {
                write: true,
                create: true,
                ..OpenFlags::default()
            },
            mode: 0o640,
        }
    }

    // An op whose undo the backend can refuse. A logical symlink is the richest
    // instance: `create_symlink` writes a target blob, a placeholder and a
    // backing index keyed on the placeholder's backend object ID, and the
    // reconciled abort removes all three plus any ancestor materialised for the
    // placeholder — four objects, any one of whose removals the storage backend
    // can refuse, at which point the overlay poisons rather than reporting a
    // reconciliation it did not perform. Pinned in `umbra-overlay` by the three
    // `a_symlink_rollback_that_cannot_unlink_…_poisons` siblings.
    //
    // (b) `resolve` answers `Emulate` for `FsOp::Symlink`, so the supervisor's
    //     own `syscall_exit` would not reach this refusal today. The property
    //     under test is "when the namespace refuses, the exit fails closed",
    //     which is the supervisor's behaviour and not the op's reachability.
    //     That caveat still stands; the one that said turning this into
    //     machinery needs an integration harness does not —
    //     `tests/kernel_refusal.rs` is that harness, and
    //     `a_rollback_the_backend_refuses_poisons_the_run_with_the_backends_own_kind`
    //     drives a refused undo through a real `Overlay` on an op that *is*
    //     reachable.
    fn a_symlink_whose_undo_the_backend_can_refuse() -> FsOp {
        FsOp::Symlink {
            target: BytePath::new(b"/target".to_vec()).unwrap(),
            link_dir: DirRef::Cwd,
            link_name: BytePath::new(b"/link".to_vec()).unwrap(),
        }
    }

    // Case (d) at supervisor level. The property is about the supervisor, not
    // about which op triggers it: reconciliation is the namespace's verdict to
    // give, not an error the supervisor may swallow, so when the namespace
    // refuses, the exit must fail closed -- the run poisons, the lifecycle
    // latches, and nothing is resumed. The supervisor still passes
    // `KernelRefused` down; what changes is the answer it gets back. Named for
    // the property rather than the trigger because the trigger keeps moving: a
    // plain creating `open` became reconcilable in #55, a creating `open` under
    // an absent parent in #64, and the op below is what is left.
    #[test]
    fn a_namespace_refusal_on_the_exit_path_poisons_the_run_and_resumes_nothing() {
        let thread = ThreadId(task(1).0);
        let (mut s, log, journaled) = exit_supervisor(
            a_symlink_whose_undo_the_backend_can_refuse(),
            Err(unreconcilable()),
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
            // The kind this test *scripted*, not a claim about production: the
            // supervisor propagates whatever the namespace returned and branches
            // on none of it, so what this pins is that the error reached the
            // caller. The kind a real `Overlay` produces for a refused undo is
            // the storage backend's own, and
            // `a_rollback_the_backend_refuses_poisons_the_run_with_the_backends_own_kind`
            // in `tests/kernel_refusal.rs` is the twin that asserts it.
            ErrorKind::InvalidState
        );
        assert!(s.is_poisoned());
        assert_eq!(s.state.lifecycle, RunLifecycle::RecoveryRequired);
        // The reason the supervisor sent is still the observed fact; the refusal
        // is the namespace's, not a different reason having been constructed.
        assert!(matches!(
            journaled.lock().unwrap().aborts.as_slice(),
            [AbortReason::KernelRefused(e)] if *e == Errno(1)
        ));
        assert!(
            log.lock().unwrap().order().is_empty(),
            "a run that could not reconcile must resume nothing"
        );
    }

    // The mirror, and the behaviour this PR exists to produce: the same exit
    // path, the same `KernelRefused` reason, but a namespace that rolled the
    // prepare-time creation back and reconciled. Without this the supervisor
    // suite would pin only the failure half and stay silent about the half #55
    // delivers.
    #[test]
    fn a_refused_creating_open_the_namespace_rolls_back_is_resumed_with_its_errno() {
        let errno = Errno(28);
        let thread = ThreadId(task(1).0);
        // A different op from the poisoning case above, and genuinely different:
        // the real overlay reconciles this one, while that one's four-object undo
        // gives a backend four chances to refuse. The supervisor still only reads
        // the answer -- it does not compute it from the op -- which is what makes
        // a scripted verdict the right knob and the op choice a fidelity claim.
        // `tests/kernel_refusal.rs` is where that fidelity claim is checked
        // rather than asserted in prose.
        let (mut s, log, journaled) =
            exit_supervisor(creating_open_under_an_absent_parent(), Ok(()));
        s.operations.insert(thread, OperationId(Uuid::new_v4()));
        s.handle_event(TraceEvent::SyscallExit {
            task: task(1),
            thread,
            outcome: OperationOutcome::Failure(errno),
        })
        .unwrap();
        assert!(!s.is_poisoned());
        assert_eq!(s.state.lifecycle, RunLifecycle::Running);
        assert!(s.operations.is_empty());
        let journaled = journaled.lock().unwrap();
        assert!(
            matches!(journaled.aborts.as_slice(), [AbortReason::KernelRefused(e)] if *e == errno),
            "the observed errno must reach the namespace as the abort reason"
        );
        assert_eq!(journaled.observed, vec![OperationOutcome::Failure(errno)]);
        let log = log.lock().unwrap();
        // The rewritten syscall already ran and already carries the kernel's
        // errno in the tracee's return register, so the exit resumes it and
        // installs nothing. A `set_registers` here would mean the supervisor had
        // synthesised a verdict of its own.
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
