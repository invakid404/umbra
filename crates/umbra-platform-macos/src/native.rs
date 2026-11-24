//! Thread-affine debugger controller. Unsafe code is limited to owned Mach rights,
//! local libSystem calls and bounded remote memory copies.
use crate::{
    abi::{self, get, set, CPSR, PC},
    cache, error, interpose,
    rsp::{self, Rsp},
    spawn, spawn_host, unsupported, Options,
};
use mach2::{
    traps::{mach_task_self, task_for_pid},
    vm::mach_vm_read_overwrite,
};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CString, OsStr},
    marker::PhantomData,
    os::fd::AsRawFd,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use umbra_core::*;
use umbra_platform::{TraceBackend, TraceControl, TraceMemory};

fn mach(code: i32, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(error(operation, format!("Mach status {code}")))
    }
}
fn cstr(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|e| error("launch string", e))
}
fn tid(id: u64, generation: u64) -> ThreadId {
    ThreadId(TaskIdentity {
        native_id: id,
        generation,
    })
}
struct Task(u32, Option<umbra_platform::shutdown::Reservation>);
impl Task {
    fn acquire_owned(pid: i32, slot: umbra_platform::shutdown::Reservation) -> Result<Self> {
        let mut task = match Self::acquire(pid) {
            Ok(task) => task,
            Err(error) => {
                slot.creation_failed();
                return Err(error);
            }
        };
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(
            serde_json::json!({"event": "acquired", "kind": "TaskRight", "pid": pid}),
        );
        slot.acquired(umbra_core::shutdown::Identity {
            pid: Some(pid),
            birth: None,
        })?;
        task.1 = Some(slot.clone());
        slot.capture_birth(
            bsd_info(pid).map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec)),
        )?;
        Ok(task)
    }
    fn writable_pid_slot(&self, pointer: u64) -> bool {
        use mach2::vm_region::{vm_region_basic_info_64, VM_REGION_BASIC_INFO_64};
        let Some(end) = pointer.checked_add(4) else {
            return false;
        };
        let mut address = pointer;
        let mut size = 0;
        let mut object = 0;
        // SAFETY: all output pointers are valid and info has the advertised size.
        unsafe {
            let mut info: vm_region_basic_info_64 = std::mem::zeroed();
            let mut count = vm_region_basic_info_64::count();
            let result = mach2::vm::mach_vm_region(
                self.0,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                (&mut info as *mut vm_region_basic_info_64).cast(),
                &mut count,
                &mut object,
            );
            if object != 0 {
                mach2::mach_port::mach_port_deallocate(mach_task_self(), object);
            }
            result == 0
                && address <= pointer
                && address.checked_add(size).is_some_and(|limit| end <= limit)
                && info.protection & mach2::vm_prot::VM_PROT_WRITE != 0
        }
    }
    fn acquire(pid: i32) -> Result<Self> {
        let mut port = 0;
        // SAFETY: valid out pointer; the returned send right is owned by Task.
        unsafe {
            mach(
                task_for_pid(mach_task_self(), pid, &mut port),
                "task_for_pid",
            )?;
        }
        Ok(Self(port, None))
    }
    fn read(&self, address: u64, out: &mut [u8]) -> Result<()> {
        if out.len() > MAX_IO_BYTES || address.checked_add(out.len() as u64).is_none() {
            return Err(error("memory", "invalid read bounds"));
        }
        let mut size = 0;
        // SAFETY: out owns exactly the requested writable bytes; Mach validates remote addresses.
        unsafe {
            mach(
                mach_vm_read_overwrite(
                    self.0,
                    address,
                    out.len() as u64,
                    out.as_mut_ptr() as u64,
                    &mut size,
                ),
                "mach_vm_read",
            )
            .map_err(|e| error("mach_vm_read", format!("{address:#x}/{}: {e}", out.len())))?;
        }
        if size != out.len() as u64 {
            return Err(error("mach_vm_read", "short read"));
        }
        Ok(())
    }
    /// `task_terminate`, returning its result. Measured on macOS 27.2.1
    /// (#205): it does not end a tracee its debugger holds at an exception
    /// (it returns `KERN_FAILURE`, 5, and the tracee stays traced and not
    /// exiting); only the debugger's release lets an exit run.
    #[must_use]
    fn kill(&self) -> i32 {
        // SAFETY: an owned task port; no memory is passed.
        unsafe { libc::task_terminate(self.0) }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        if cfg!(test) && self.0 == 0 && self.1.is_none() {
            return; // Synthetic task fixture owns no Mach send right.
        }
        let result = unsafe { mach2::mach_port::mach_port_deallocate(mach_task_self(), self.0) };
        if let Some(slot) = self.1.take() {
            if result == 0 {
                let _ = slot.confirm(umbra_core::shutdown::Proof::Released);
            } else {
                let _ = slot.unconfirmed(format!("mach_port_deallocate returned {result}"));
            }
        }
    }
}
impl TraceMemory for &Task {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        Task::read(self, address, out)
    }
}
#[cfg(feature = "mutation-probe-spawn-lifecycle")]
fn probe_unregistered(task: &Task, slot: u64) -> Result<bool> {
    if std::env::var("UMBRA_FIX_243_PROBE_PHASE").as_deref() != Ok("unregistered") {
        return Ok(false);
    }
    // Instrument-only observation. No child Task is acquired or registered.
    let mut bytes = [0; 4];
    task.read(slot, &mut bytes)?;
    let pid = i32::from_le_bytes(bytes);
    let birth = bsd_info(pid).map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec));
    crate::spawn_probe::rendezvous(
        "unregistered",
        pid,
        birth,
        serde_json::json!({"watchdog_registered": false, "session_inserted": false}),
    )
}

trait KillOwned {
    fn kill_owned(&self);
}
impl KillOwned for Arc<Task> {
    fn kill_owned(&self) {
        let _ = self.kill();
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCause {
    Deadline,
    OwnerGone,
    MonitorFailed,
    MonitorPanicked,
}
impl TerminalCause {
    fn message(self) -> &'static str {
        match self {
            Self::Deadline => "session timed out",
            Self::OwnerGone => "supervisor gone",
            Self::MonitorFailed => "owner monitor failed",
            Self::MonitorPanicked => "owner monitor panicked",
        }
    }
    fn operation(self) -> &'static str {
        match self {
            Self::Deadline => "watchdog",
            _ => "owner-observation",
        }
    }
}
struct WatchedTasks<T = Arc<Task>> {
    wakeups: Arc<rsp::Wakeups>,
    tasks: Vec<T>,
    reason: Option<TerminalCause>,
}
impl<T> Default for WatchedTasks<T> {
    fn default() -> Self {
        Self {
            wakeups: Arc::new(rsp::Wakeups::default()),
            tasks: Vec::new(),
            reason: None,
        }
    }
}
impl<T: KillOwned> WatchedTasks<T> {
    fn add(&mut self, task: T) {
        if self.reason.is_some() {
            task.kill_owned();
        }
        self.tasks.push(task);
    }
    fn latch(&mut self, reason: TerminalCause) {
        if self.reason.is_none() {
            self.reason = Some(reason);
            for task in self.tasks.iter().rev() {
                task.kill_owned();
            }
        }
    }
}
impl<T> WatchedTasks<Arc<T>>
where
    Arc<T>: KillOwned,
{
    fn retire(&mut self, old: &Arc<T>) -> Option<Arc<T>> {
        self.tasks
            .iter()
            .position(|task| Arc::ptr_eq(task, old))
            .map(|index| self.tasks.remove(index))
    }
    fn replace(&mut self, old: &Arc<T>, new: Arc<T>) -> Option<Arc<T>> {
        let retired = self.tasks.iter().position(|task| Arc::ptr_eq(task, old));
        // Register (and, if latched, kill) the replacement before retiring anything.
        self.add(new);
        retired.map(|index| self.tasks.remove(index))
    }
}
struct Watchdog {
    wakeups: Arc<rsp::Wakeups>,
    tasks: Arc<Mutex<WatchedTasks>>,
    cancel: Option<std::os::unix::net::UnixStream>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn new(
        deadline: Option<Instant>,
        owner: Option<crate::owner::OwnerWatch>,
        tasks: Arc<Mutex<WatchedTasks>>,
    ) -> Result<Self> {
        let owned = tasks.clone();
        // Owner and deadline monitors share cancellation with their task latch.
        let wakeups = tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wakeups
            .clone();
        let wake_worker = wakeups.clone();
        let (waiter, cancel) = crate::owner::Waiter::new(owner)?;
        let worker = std::thread::Builder::new()
            .name("umbra-owner".into())
            .spawn(move || {
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(reason) = monitor_reason(deadline, || waiter.wait(deadline)) {
                        owned
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .latch(reason);
                        wake_worker.signal();
                        if reason != TerminalCause::Deadline {
                            waiter.disconnect();
                        }
                    }
                }));
                if run.is_err() {
                    owned
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .latch(TerminalCause::MonitorPanicked);
                    wake_worker.signal();
                    waiter.disconnect();
                }
            })
            .map_err(|e| error("owner-observation", e))?;
        Ok(Self {
            wakeups,
            tasks,
            cancel: Some(cancel),
            worker: Some(worker),
        })
    }
    fn add(&self, task: Arc<Task>) {
        let mut state = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        state.add(task);
    }
    fn terminate_registered(&self) {
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tasks
            .iter()
            .rev()
        {
            let _ = task.kill();
        }
    }
    fn replace(&self, old: &Arc<Task>, new: Arc<Task>) {
        let retired = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.replace(old, new)
        };
        // Mach send-right release must not hold the registry lock.
        drop(retired);
    }
    fn reason(&self) -> Option<TerminalCause> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).reason
    }
}
fn monitor_reason(
    deadline: Option<Instant>,
    mut wait: impl FnMut() -> Result<crate::owner::Wake>,
) -> Option<TerminalCause> {
    loop {
        match wait() {
            Ok(crate::owner::Wake::Disarm) => return None,
            Ok(crate::owner::Wake::Timeout)
                if !deadline.is_some_and(|d| crate::clock::now() >= d) =>
            {
                continue
            }
            Ok(crate::owner::Wake::Timeout) => return Some(TerminalCause::Deadline),
            Ok(crate::owner::Wake::OwnerGone) => return Some(TerminalCause::OwnerGone),
            Err(_) => return Some(TerminalCause::MonitorFailed),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        use std::io::Write;
        if let Some(mut tx) = self.cancel.take() {
            let _ = tx.write_all(b"x");
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[derive(Clone)]
struct Breakpoint {
    original: [u8; 4],
    #[allow(dead_code)]
    raw: bool,
}
#[derive(Clone)]
enum ReturnKind {
    /// A filesystem syscall whose result the caller observes as a SyscallExit.
    Syscall,
    Wait,
    Fork {
        restore: BTreeMap<u64, [u8; 4]>,
    },
    Spawn {
        pid_pointer: u64,
        twin: PathBuf,
    },
    PackedSpawn,
    /// An `execve` in flight, carrying the resigned image it was rewritten to
    /// name -- as a *candidate*, not as a commitment.
    ///
    /// **The candidate is here rather than assigned to `Session::twin` at the
    /// entry, and that is the whole point of the field.** A successful `execve`
    /// never returns, so the only stop that can adopt it is the exec stop, which
    /// takes it out of the pending value. A *failed* `execve` does return, and
    /// this arm of `finish_return` is that return by construction -- so dropping
    /// the pending value is exactly the right thing, and `twin` keeps naming the
    /// image the session is still running.
    ///
    /// Assigning at the entry left `twin` naming an image the tracee never
    /// reached. That was harmless until the interposer requirement began
    /// following `twin`: after that, the next `fork` handed `attach_child` the
    /// wrong image, `install()` found no match, and the child was left with an
    /// *armed* control block -- inherited through `fork` -- and no breakpoint on
    /// any trap site. Its first routed call then issued `svc #0x80` with
    /// `x16 = UMBRA_TRAP_NUMBER` that nothing covered, and the run died on an
    /// undecoded `SIGSYS`. That is the #116 shape, reached from a new direction.
    Exec {
        twin: PathBuf,
    },
}
/// SDK `sys/wait.h` spells `WNOHANG` as mask value 1 — the least significant
/// bit, bit index 0 — not `1 << 1`.
const WNOHANG: u32 = libc::WNOHANG as u32;
/// What an intercepted `wait4`/`wait4_nocancel` should do, decided from the
/// tracee's arguments and the tracer's view of the caller's children alone.
#[derive(Debug, PartialEq, Eq)]
enum WaitPlan {
    /// WNOHANG over live but unfinished children: report "nothing reaped"
    /// without running the syscall, parking, or touching status/rusage.
    Poll,
    /// A blocking wait over live but unfinished children: park the thread.
    Park,
    /// Nothing tracked matches, or a match has finished: let the kernel
    /// answer, so ECHILD and the real status/rusage writes are preserved.
    Native,
    /// Selections the tracer refuses to virtualize rather than mis-emulate.
    Unsupported,
}
/// `wanted` is the pid selector the callee receives; `x2` is the raw option
/// register, truncated here because arm64 leaves the upper 32 bits of a
/// register holding an `int` argument unspecified and the kernel's argument
/// munger drops them — reading all 64 bits rejects legal WNOHANG callers.
fn wait_plan(wanted: i32, x2: u64, children: bool, ready: bool) -> WaitPlan {
    let options = x2 as u32;
    if !children {
        return WaitPlan::Native;
    }
    if wanted <= 0 && wanted != -1 || options & !WNOHANG != 0 {
        return WaitPlan::Unsupported;
    }
    match (ready, options & WNOHANG != 0) {
        (true, _) => WaitPlan::Native,
        (false, true) => WaitPlan::Poll,
        (false, false) => WaitPlan::Park,
    }
}
/// One intercepted syscall between its entry `svc` and its return gate.
///
/// **The window is per thread and names its own thread.** `return_stop` releases
/// the entry breakpoint with `z0` before letting the `svc` run, so for as long as
/// this value lives the stub carries no breakpoint *for the whole process* -- a
/// debugserver `Z0` is process-wide. `thread` is who is inside that window; every
/// sibling is held stopped until `finish_return` re-arms the entry.
struct Pending {
    creation: Option<TraceOwnership>,
    /// The thread that trapped at `entry`, recorded when the window opened.
    ///
    /// **This is the attribution source for the return, and [`Session::thread`]
    /// is never it.** That slot is overwritten by *every* stop on this
    /// connection, so a sibling stop landing inside the window would retarget
    /// the `SyscallExit` this return emits. The caller keys its in-flight
    /// operations by thread and removes them on the exit
    /// (`umbra-supervisor/src/events.rs`, `self.operations.remove(&thread)`); a
    /// miss there is not an error, it is the "an exit for a call we never
    /// intercepted" branch, which lets the kernel result stand **and abandons
    /// the rewrite without a word**. Closing the sibling window while attributing
    /// from the session slot would therefore convert a loud escape into a silent
    /// one, which is the whole reason this field exists rather than a second read
    /// of `s.thread`.
    ///
    /// Today the two agree in every reachable interleaving, because the window
    /// freezes the siblings that could have made them differ. That is an
    /// emergent property of the freeze, not an invariant of this code, and it
    /// stops holding the moment anything resumes a sibling inside a window --
    /// which is exactly the class of change a reader would not expect to be
    /// silent.
    thread: ThreadId,
    entry: u64,
    entry_breakpoint: Breakpoint,
    gate: u64,
    kind: ReturnKind,
    hardware: bool,
}
/// Owned until explicit deallocation succeeds or this task dies. Never dropped
/// as a side effect of taking the pending return out of the thread map.
struct SpawnInFlight {
    token: SpawnToken,
    snapshot: spawn::Snapshot,
    plan: SpawnPlan,
    authorization: Option<SpawnAuthorization>,
    original: RegisterSet,
    returned: Option<RegisterSet>,
    caller_pid: u64,
    region: Option<u64>,
    twin: PathBuf,
    child: Option<usize>,
}

/// Pure view of the production barrier; contains no task rights or ownership.
struct SpawnBarrier {
    token: SpawnToken,
    digest: [u8; 32],
    authorization: Option<SpawnAuthorization>,
    returned: bool,
    child: bool,
}
impl SpawnInFlight {
    fn barrier(&self) -> SpawnBarrier {
        SpawnBarrier {
            token: self.token,
            digest: self.plan.digest,
            authorization: self.authorization,
            returned: self.returned.is_some(),
            child: self.child.is_some(),
        }
    }
}
impl SpawnBarrier {
    fn current(&self, parent: TaskId, generation: u64, auth: SpawnAuthorization) -> bool {
        self.token == auth.token
            && self.digest == auth.digest
            && auth.token.parent == parent
            && auth.token.exec_generation == generation
    }
    fn authorize(&self, parent: TaskId, generation: u64, auth: SpawnAuthorization) -> Result<()> {
        if !self.current(parent, generation, auth) || self.authorization.is_some() {
            return Err(error("spawn", "stale authorization"));
        }
        Ok(())
    }
    fn complete(
        &self,
        parent: TaskId,
        generation: u64,
        auth: SpawnAuthorization,
        success: bool,
    ) -> Result<()> {
        if !self.current(parent, generation, auth)
            || self.authorization != Some(auth)
            || !self.returned
            || self.child != success
        {
            return Err(error("spawn", "stale or premature Ready"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct TraceOwnership {
    ledger: umbra_platform::shutdown::Ledger,
    tracee: umbra_platform::shutdown::Reservation,
}
struct Session {
    ownership: Option<TraceOwnership>,
    id: TaskId,
    thread: ThreadId,
    parent: Option<usize>,
    stopped: bool,
    done: bool,
    reaped: bool,
    /// The tracee's start time (`pbi_start_tvsec`, `pbi_start_tvusec`), read
    /// at attach while it is alive. The ordered teardown's SZOMB evidence only
    /// counts when a later lookup returns this same identity (#186).
    birth: Option<(u64, u64)>,
    /// Set once [`Session::ordered_teardown`] has run, so `Drop` does not
    /// run it a second time.
    torn_down: bool,
    /// Barrier evidence already collected, so it is not lost. A tracee's exit
    /// is checked the moment its tracer reports it (W/X), while the zombie
    /// still exists. Its own parent may reap it before teardown, and absence is
    /// never evidence (ruling R2), but proof taken earlier stays proof.
    exit_evidence: Option<BarrierEvidence>,
    /// The tracer's own exit report for this tracee (its `W`/`X` packet),
    /// kept for [`BarrierEvidence::TracerReportedExit`].
    exit_report: Option<String>,
    exec_generation: u64,
    #[cfg(feature = "signal-witness")]
    signal_probe_fired: bool,
    signals: crate::signals::Delivery,
    /// Generation visible in the supervisor's logical context. The trusted
    /// bootstrap's exec is consumed before that context exists, and child
    /// contexts inherit the parent's generation until a reported exec.
    policy_exec_generation: u64,
    initial: bool,
    live: Option<Box<LiveSession>>,
    retirement: Option<crate::retirement::Retirement>,
    retirement_report: Option<TeardownReport>,
}

struct LiveSession {
    _witness: crate::ownership::Witness,
    spawn: Option<SpawnInFlight>,
    task: Arc<Task>,
    rsp: Rsp,
    twin: PathBuf,
    // Only software breakpoints installed on this session's RSP connection.
    // Disabled entry sites live in Pending; inherited bytes are not ownership.
    breaks: crate::ownership::Owned<BTreeMap<u64, Breakpoint>>,
    /// Open return windows, keyed by the thread inside each one.
    ///
    /// **At most one is ever open, and the freeze is what makes that true** --
    /// `return_stop` resumes only the trapping thread and leaves every sibling
    /// stopped, so no other thread is running to reach `return_stop` and open a
    /// second. It is a structural consequence of which threads run, not of any
    /// check. `return_stop` asserts it (see there); the assertion is a tripwire
    /// over the freeze, not the thing that establishes it.
    ///
    /// **A map rather than a slot, for what it makes expressible rather than for
    /// what it answers.** A single slot answers "which thread owns this window"
    /// perfectly well through [`Pending::thread`], and that was measured: keyed
    /// back onto one shared slot, `finish_return` kept attributing correctly,
    /// `thread_index` kept resolving, and both multithreaded fixtures stayed
    /// green. What the map buys is that the per-thread invariant can be *stated*
    /// -- `return_stop`'s tripwire is per thread because this is -- and that the
    /// shape stays honest for the deferred `single_thread()` removal, at which
    /// point a second concurrent window becomes reachable and the map becomes
    /// load-bearing rather than expressive.
    pending: crate::ownership::Owned<BTreeMap<ThreadId, Pending>>,
    waiting: crate::ownership::Owned<Vec<usize>>,
    /// Entry `svc` sites stopped and reported to the caller, keyed by thread.
    ///
    /// A `Delivery::Namespace` entry parks here between the `SyscallEntry` this
    /// backend emits and the `resume` the caller answers it with, so the caller
    /// can rewrite registers in between. Per thread for the same reason as
    /// `pending`: the caller names the thread it is answering for, and
    /// `Session::thread` names whoever stopped last.
    entry: crate::ownership::Owned<BTreeMap<ThreadId, u64>>,
    registers: crate::ownership::Owned<Vec<(usize, usize)>>,
    scratch: crate::ownership::Owned<Vec<(u64, usize, usize)>>,
    /// The routing interposer this run loads, and **the image this session is
    /// now running**.
    ///
    /// `(published dylib, current image, descriptor floor)`. The second element
    /// is per-session state that follows this session's `exec`s: it is seeded
    /// from the backend's launch-time value and reassigned by
    /// [`Self::retarget_interposer`] at every exec stop and for every attached
    /// child, from that session's own twin.
    ///
    /// **It used to be the launch target, assigned once and never reassigned,
    /// and that was the exec gap.** `exec` replaces the address space, so the
    /// interposer arrives in the new image mapped and inert -- dyld re-loads it,
    /// because `DYLD_INSERT_LIBRARIES` rides in `envp` and survives the exec,
    /// and re-runs its constructor, which leaves `__DATA,__umbra_arm` back at
    /// zeroes. umbra must re-arm it. Comparing against a fixed launch target
    /// meant it re-armed only when the exec'd image *was* that target, so a
    /// child that exec'd anything else took the "this run routes nothing" arm
    /// below: no trap sites breakpointed, no control block written, and its
    /// file operations passing through to libc silently. Every tool call a shell
    /// makes execs a different binary, so that was the common case rather than
    /// the exotic one.
    ///
    /// Both path halves are still needed because `install` runs once per exec
    /// and the requirement is *not* uniform across them: `DYLD_INSERT_LIBRARIES` loads the dylib into the
    /// sandbox installer as well as the target, and the installer is stopped and
    /// installed before dyld has mapped anything at all.
    ///
    /// The installer's copy stays dormant because **nothing arms it**, which is
    /// the mechanism since round 1 (B1) and is not the one this doc used to
    /// describe: the library once compared its own `_NSGetExecutablePath`
    /// against the target from a constructor, and that constructor made it live
    /// and un-breakpointed through every library initializer. It has no
    /// constructor now and ships inert; `arm_interposer` writes the control
    /// block, and it is called for the one image named here and no other. So
    /// there is nothing to breakpoint in the installer and nothing to refuse,
    /// and requiring the image in *every* `install` failed the launch there,
    /// before the target existed.
    ///
    /// Carried per session rather than read from the backend so the requirement
    /// travels with the tree: a descendant execing the same target image is
    /// loaded with the same interposer and must have its trap sites breakpointed
    /// too, or its routed calls would reach the kernel unmediated.
    interposer: Option<(PathBuf, PathBuf, u32)>,
}

impl std::ops::Deref for Session {
    type Target = LiveSession;
    fn deref(&self) -> &LiveSession {
        self.live.as_deref().expect("active session resources")
    }
}
impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut LiveSession {
        self.live.as_deref_mut().expect("active session resources")
    }
}

impl LiveSession {
    fn attached(
        pid: i32,
        task: Arc<Task>,
        rsp: Rsp,
        twin: PathBuf,
        registers: Vec<(usize, usize)>,
        interposer: Option<(PathBuf, PathBuf, u32)>,
    ) -> Self {
        Self {
            _witness: crate::ownership::Witness::new(pid as u64, "body"),
            spawn: None,
            task,
            rsp,
            twin,
            breaks: crate::ownership::Owned::new(BTreeMap::new(), pid as u64, "breaks"),
            pending: crate::ownership::Owned::new(BTreeMap::new(), pid as u64, "pending"),
            waiting: crate::ownership::Owned::new(vec![], pid as u64, "waiting"),
            entry: crate::ownership::Owned::new(BTreeMap::new(), pid as u64, "entry"),
            registers: crate::ownership::Owned::new(registers, pid as u64, "registers"),
            scratch: crate::ownership::Owned::new(vec![], pid as u64, "scratch"),
            interposer,
        }
    }
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn attach(
        pid: i32,
        generation: u64,
        twin: PathBuf,
        parent: Option<usize>,
        options: &Options,
        deadline: Option<Instant>,
        watchdog: &Watchdog,
        interposer: Option<(PathBuf, PathBuf, u32)>,
        ownership: TraceOwnership,
    ) -> Result<Self> {
        let right = ownership.ledger.reserve(
            umbra_core::shutdown::ResourceKind::TaskRight,
            Some(ownership.tracee.id()),
            generation,
            0,
        )?;
        let task = Arc::new(Task::acquire_owned(pid, right)?);
        watchdog.add(task.clone());
        #[cfg(any(test, feature = "retirement-witness"))]
        crate::ownership::task(pid as u64, &task);
        let birth = bsd_info(pid).map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec));
        ownership.tracee.capture_birth(birth)?;
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if parent.is_some() {
            let registered = watchdog
                .tasks
                .lock()
                .unwrap()
                .tasks
                .iter()
                .any(|owned| Arc::ptr_eq(owned, &task));
            crate::spawn_probe::acquired(pid, birth, registered)?;
        }
        let tracer = ownership.ledger.reserve(
            umbra_core::shutdown::ResourceKind::Tracer,
            Some(ownership.tracee.id()),
            generation,
            0,
        )?;
        let mut rsp = Rsp::connect_owned(options, deadline, tracer)?;
        watchdog.wakeups.add(rsp.wakeup());
        let actions = rsp.request("vCont?")?;
        if !actions.split(';').any(|action| action == "C") {
            return Err(unsupported(
                "signal-resume-capability: debugserver lacks vCont C",
            ));
        }
        let stop = rsp.request(&format!("vAttach;{pid:x}"))?;
        if !stop.starts_with('T') {
            return Err(error("attach", stop));
        }
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if parent.is_none() && std::env::var_os("UMBRA_FIX_243_PROBE_ROOT").is_some() {
            // Plain-run comparison: root is acquired and debugger attached;
            // no spawn request has executed.
            crate::spawn_probe::acquired(pid, birth, true)?;
        }
        let fields = rsp::fields(&stop[3..]);
        let thread = rsp::number(
            fields
                .get("thread")
                .ok_or_else(|| error("attach", "no thread"))?,
        )?;
        let info = rsp.request("qProcessInfo")?;
        if rsp::fields(&info).get("cputype") != Some(&"100000c") {
            return Err(unsupported(format!("not native arm64: {info}")));
        }
        let mut registers = Vec::new();
        for index in 0..34 {
            let info = rsp.request(&format!("qRegisterInfo{index:x}"))?;
            let fields = rsp::fields(&info);
            let name = if index < 29 {
                format!("x{index}")
            } else {
                ["fp", "lr", "sp", "pc", "cpsr"][index - 29].into()
            };
            if fields.get("name") != Some(&name.as_str()) {
                return Err(unsupported(format!("unexpected register layout: {info}")));
            }
            let bits = fields
                .get("bitsize")
                .ok_or_else(|| error("register", "bitsize"))?
                .parse::<usize>()
                .map_err(|e| error("register", e))?;
            let offset = fields
                .get("offset")
                .ok_or_else(|| error("register", "offset"))?
                .parse::<usize>()
                .map_err(|e| error("register", e))?;
            registers.push((offset, bits / 8));
        }
        Ok(Self {
            ownership: Some(ownership),
            id: TaskId(TaskIdentity {
                native_id: pid as u64,
                generation,
            }),
            thread: tid(thread, generation),
            parent,
            stopped: true,
            done: false,
            reaped: false,
            birth,
            torn_down: false,
            exit_evidence: None,
            exit_report: None,
            exec_generation: 0,
            #[cfg(feature = "signal-witness")]
            signal_probe_fired: false,
            signals: crate::signals::Delivery::default(),
            policy_exec_generation: 0,
            initial: true,
            retirement: None,
            retirement_report: None,
            live: Some(Box::new(LiveSession::attached(
                pid, task, rsp, twin, registers, interposer,
            ))),
        })
    }
    /// Read one thread's registers.
    ///
    /// **The thread is named by the caller, not taken from [`Self::thread`].**
    /// That slot holds whoever stopped last on this connection, which in a
    /// multithreaded tracee need not be the thread being asked about:
    /// `registers`/`set_registers` are reached with a `ThreadId` the caller read
    /// off a `SyscallEntry`, and answering them from the session slot would hand
    /// back a sibling's context under the right thread's name.
    fn regs(&mut self, thread: ThreadId) -> Result<RegisterSet> {
        if !self.stopped || self.done {
            return Err(error("registers", "task is not stopped"));
        }
        let live = self.live.as_mut().expect("stopped session resources");
        let raw = rsp::unhex(
            &live
                .rsp
                .request(&format!("g;thread:{:x};", thread.0.native_id))?,
        )?;
        let mut regs = RegisterSet::new(Architecture::Aarch64, vec![0; 272])?;
        for (index, (offset, len)) in live.registers.iter().enumerate() {
            // Current Apple debugserver does not implement the bulk g packet.
            // An empty reply means unsupported; use individual register reads.
            let individual;
            let b = if raw.is_empty() {
                individual = rsp::unhex(
                    &live
                        .rsp
                        .request(&format!("p{index:x};thread:{:x};", thread.0.native_id))?,
                )?;
                if individual.len() != *len {
                    return Err(error("registers", "short register value"));
                }
                &individual
            } else {
                raw.get(*offset..offset + len)
                    .ok_or_else(|| error("registers", "short register context"))?
            };
            let mut value = [0; 8];
            value[..*len].copy_from_slice(b);
            set(&mut regs, index, u64::from_le_bytes(value))?;
        }
        Ok(regs)
    }
    fn set_regs(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()> {
        let old = self.regs(thread)?;
        let live = self.live.as_mut().expect("stopped session resources");
        for i in 0..34 {
            let v = get(regs, i)?;
            if v != get(&old, i)? {
                live.rsp.ok(&format!(
                    "P{i:x}={};thread:{:x};",
                    rsp::hex(&v.to_le_bytes()[..live.registers[i].1]),
                    thread.0.native_id
                ))?;
            }
        }
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        if !self.stopped
            || self.done
            || bytes.len() > MAX_IO_BYTES
            || address.checked_add(bytes.len() as u64).is_none()
        {
            return Err(error("write memory", "state or bounds"));
        }
        for (i, chunk) in bytes.chunks(2048).enumerate() {
            self.rsp.ok(&format!(
                "M{:x},{:x}:{}",
                address + (i * 2048) as u64,
                chunk.len(),
                rsp::hex(chunk)
            ))?;
        }
        Ok(())
    }
    fn allocate(&mut self, bytes: &[u8]) -> Result<u64> {
        let aligned = (bytes.len() + 15) & !15;
        if self.scratch.last().is_none_or(|b| b.2 + aligned > b.1) {
            let size = 16384usize.max(aligned);
            let address = rsp::number(&self.rsp.request(&format!("_M{size:x},rw"))?)?;
            if address == 0 {
                return Err(error("scratch", "null allocation"));
            }
            self.scratch.push((address, size, 0));
        }
        let block = self.scratch.last_mut().unwrap();
        let address = block.0 + block.2 as u64;
        block.2 += aligned;
        self.write(address, bytes)?;
        Ok(address)
    }
    fn breakpoint(&mut self, address: u64, raw: bool) -> Result<()> {
        if self.breaks.contains_key(&address) {
            return Ok(());
        }
        let mut original = [0; 4];
        self.task.read(address, &mut original)?;
        if original != [1, 16, 0, 212] {
            return Err(error(
                "breakpoint",
                format!("expected svc #0x80 at {address:x}, got {original:?}"),
            ));
        }
        self.install_breakpoint(address, Breakpoint { original, raw })
    }
    fn install_breakpoint(&mut self, address: u64, breakpoint: Breakpoint) -> Result<()> {
        if self.breaks.contains_key(&address) {
            return Err(error(
                "breakpoint",
                format!("session already owns breakpoint at {address:x}"),
            ));
        }
        self.rsp.ok(&format!("Z0,{address:x},4"))?;
        self.breaks.insert(address, breakpoint);
        Ok(())
    }
    fn remove_breakpoint(&mut self, address: u64) -> Result<Breakpoint> {
        let breakpoint = self
            .breaks
            .get(&address)
            .cloned()
            .ok_or_else(|| error("breakpoint", format!("session does not own {address:x}")))?;
        self.rsp.ok(&format!("z0,{address:x},4"))?;
        self.breaks.remove(&address);
        Ok(breakpoint)
    }
    fn temporary_breakpoint(&mut self, address: u64) -> Result<()> {
        let mut original = [0; 4];
        self.task.read(address, &mut original)?;
        self.install_breakpoint(
            address,
            Breakpoint {
                original,
                raw: false,
            },
        )
    }
    #[cfg(feature = "signal-witness")]
    fn signal_probe(&mut self, phase: &str) -> Result<bool> {
        if self.signal_probe_fired
            || self.parent.is_some()
            || std::env::var("UMBRA_FIX284_PHASE").ok().as_deref() != Some(phase)
        {
            return Ok(false);
        }
        let path = std::env::var_os("UMBRA_FIX284_RECORD")
            .ok_or_else(|| error("signal witness", "missing record"))?;
        let regs = self.regs(self.thread)?;
        let record = serde_json::json!({"phase": phase, "task": self.id,
            "thread": self.thread, "exec_generation": self.exec_generation,
            "pc": get(&regs, PC)?, "pending": self.pending.len(),
            "waiting": self.waiting.len(), "stopped": self.stopped});
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| error("signal witness", e))?;
        writeln!(file, "{record}").map_err(|e| error("signal witness", e))?;
        file.sync_all().map_err(|e| error("signal witness", e))?;
        self.signal_probe_fired = true;
        // This is the owned stopped task, never a process-name census.
        if unsafe { libc::kill(self.id.0.native_id as i32, libc::SIGUSR1) } != 0 {
            return Err(error("signal witness", std::io::Error::last_os_error()));
        }
        Ok(true)
    }
    /// Resume every thread when no return window is open.
    fn continue_run(&mut self) -> Result<()> {
        // A failed write may still have resumed the peer; its stop is outstanding.
        self.stopped = false;
        self.rsp.send("c")?;
        Ok(())
    }
    /// Resume exactly one thread and leave every sibling stopped.
    ///
    /// **This one line is the closure.** A debugserver `Z0` is process-wide, so
    /// the `z0` `return_stop` issues to let the trapping thread execute the `svc`
    /// it sits on un-arms that stub for *every* thread until `finish_return` puts
    /// it back. A bare `c` across that gap resumes the siblings too, and a
    /// sibling reaching the same stub inside it walks straight through and issues
    /// the real, unrewritten call -- measured as the `mt-write` escape, where the
    /// second worker's write reached the host filesystem and the tracee exited 0.
    ///
    /// `vCont;c:<tid>` resumes that one thread and nothing else: measured against
    /// a sibling spinning on a counter, a per-thread continue of the *other*
    /// thread advanced it not at all, where a bare `c` advanced it by billions.
    /// The packet is new to this backend, per-thread addressing is not -- `g`,
    /// `p` and `P` already carry `;thread:<tid>;` under `QThreadSuffixSupported`,
    /// and debugserver advertises `vCont;c;C;s;S`.
    ///
    /// **The freeze can hold a blocking call indefinitely by default (#242).**
    /// The interposer routes `read`/`write`/`close`, so a routed call that blocks
    /// -- a `read` on a pipe whose only writer is a frozen sibling, for example --
    /// waits with those siblings held. An opt-in lifetime watchdog kills the tree
    /// at its deadline. Without that opt-in (the default), the wait lasts until
    /// the call returns, supervising umbra goes away, or the user stops umbra;
    /// supervisor loss triggers owner-loss teardown. Safe sibling progress is
    /// the known residual tracked in #242. A bare `c` is still unsafe: it would
    /// release siblings into the unmediated window described above.
    fn continue_thread(&mut self, thread: ThreadId) -> Result<()> {
        self.stopped = false;
        self.rsp
            .send(&format!("vCont;c:{:x}", thread.0.native_id))?;
        Ok(())
    }
    /// Resume from a stop this backend absorbed without opening a transaction,
    /// leaving an open return window's freeze intact.
    ///
    /// A transient signal can arrive while a return is in flight -- `SIGCHLD`
    /// from a forked child is the one that actually happens, and it lands at the
    /// return gate because `fork` and `wait4` are themselves gated syscalls.
    /// Continuing with a bare `c` there would release the siblings into the very
    /// window [`Self::continue_thread`] exists to keep them out of, so the resume
    /// goes to the window's owner instead and every other thread stays where
    /// debugserver stopped it.
    ///
    /// **With no window open this is the bare `c`** -- what every such stop did
    /// before, and what they all still do for the whole of a run that never
    /// intercepts anything.
    ///
    /// **`values().next()` is well-defined because at most one window is open,
    /// and it is the freeze that makes that true.** A window freezes every
    /// sibling, so no other thread is running to reach `return_stop` and open a
    /// second. `return_stop` asserts the property as a tripwire over that
    /// reasoning -- it does not create it, and an earlier revision of this
    /// comment credited the assertion with establishing it, which was wrong.
    /// Were two windows ever open, this resumes one and leaves the other frozen
    /// -- never a released sibling. An opt-in lifetime still bounds the wait;
    /// by default it lasts until return or supervisor loss (including the user
    /// stopping umbra), as described above and tracked in #242.
    fn continue_absorbed(&mut self) -> Result<()> {
        match self.pending.values().next().map(|p| p.thread) {
            Some(owner) => self.continue_thread(owner),
            None => self.continue_run(),
        }
    }
    fn single_thread(&mut self) -> Result<()> {
        let threads = self.rsp.request("qfThreadInfo")?;
        if !threads.starts_with('m') || threads.contains(',') {
            return Err(unsupported("fork/deferred wait requires one thread"));
        }
        Ok(())
    }
    /// Point this session's interposer requirement at the image it is now
    /// running, so `install` arms the library where it was actually loaded.
    ///
    /// Called from exactly two places, and they are the two ways a session comes
    /// to be running an image umbra did not name at launch: the exec stop, after
    /// `intercept` has already set `twin` to the resigned image the tracee is
    /// execing, and `attach_child`, which is handed the same value for a fork
    /// and the spawned twin for a `posix_spawn`.
    ///
    /// **Not called for a freshly attached root**, which is what keeps the
    /// sandbox installer inert. The installer is a `sandbox-exec` twin with the
    /// interposer loaded into it by `DYLD_INSERT_LIBRARIES`, and it is stopped
    /// and installed before dyld has mapped anything at all; retargeting it to
    /// its own image would make `install` match, look for a library that is not
    /// mapped yet, and fail the launch in trusted bootstrap code. Its copy stays
    /// dormant because nothing arms it, which is the mechanism since round 1.
    ///
    /// Canonicalized because the comparison is against `image_path`, which
    /// answers from `proc_pidpath` and is already fully resolved. A `None`
    /// interposer -- a run that routes nothing -- has nothing to retarget and
    /// says so by doing nothing.
    fn retarget_interposer(&mut self, image: &Path) -> Result<()> {
        let Some((_, current, _)) = self.interposer.as_mut() else {
            return Ok(());
        };
        *current = std::fs::canonicalize(image).map_err(|e| {
            error(
                "interposer",
                format!("resolving the image this session now runs: {e}"),
            )
        })?;
        Ok(())
    }
    /// Open a return window on `thread`: release its entry breakpoint so the
    /// `svc` it is sitting on can run, plant the return gate at `pc + 4`, and
    /// resume that thread **alone**.
    fn return_stop(&mut self, thread: ThreadId, kind: ReturnKind) -> Result<()> {
        // **One intercepted syscall in flight per thread, asserted where it can
        // be violated rather than left implicit.**
        //
        // The slot this guards is now keyed by the thread inside the window, so
        // the collision it was planted for -- two threads on *different* stubs
        // sharing one session slot, which is what `mt_spawn` measured -- can no
        // longer happen. What it still catches is a second window opened on a
        // thread that already has one, and the three things such an overwrite
        // loses are unchanged: `entry_breakpoint`, which `finish_return` hands
        // back to `install_breakpoint` -- the site was already released with
        // `z0` a few lines down, so losing the value means it is never re-armed
        // and that stub stops being intercepted for the rest of the run;
        // `gate`, so the first return gate stays registered and is never
        // retired; and, since the R1 fix, the `Exec` candidate image -- losing
        // which leaves `twin` naming the previous image, `install()` matching
        // nothing, and the exec'd image half-mediated with its stubs
        // breakpointed and its interposer inert.
        //
        // **An assertion rather than a refusal, deliberately.** A run that
        // reaches this state is already broken by other means, so repairing one
        // symptom of it (by moving the candidate to a field of its own) would
        // hide the violation rather than surface it. What is wanted is to catch
        // the violation at the point it happens, which is here. It is live in
        // every `cargo test` run because the test profile is a debug profile; in
        // release it costs nothing and changes nothing, which is why it is not a
        // behaviour change to a shipped path.
        //
        // **The second assertion is a different property, and it is the one
        // `continue_absorbed` leans on.** That helper picks a window owner with
        // `pending.values().next()`, which is only well-defined if at most one
        // window is open *process-wide*. The per-thread check above does not
        // establish that -- two windows on two *different* threads pass it
        // untouched, and that is exactly the state that would make the pick
        // ambiguous. What establishes it is the **freeze**: the thread resumed at
        // the end of this function is the only one running, so nothing is left to
        // reach `return_stop` and open a second window.
        //
        // So the property is structural, and the assertion below is a tripwire
        // over that structure rather than its source. It is additive on purpose:
        // the per-thread form above is the ratified conversion of master's
        // single-slot tripwire and names the per-thread consequence, and this one
        // names what `continue_absorbed` needs. The stronger condition subsumes
        // the weaker, so only the first to fail is reported -- which is the right
        // order, because a same-thread double-open has the specific diagnosis and
        // a cross-thread one means the freeze itself has been broken.
        debug_assert!(
            !self.pending.contains_key(&thread),
            "a second intercepted syscall entered on a thread that was still \
             inside one: this thread's pending entry breakpoint, return gate and \
             exec candidate would all be overwritten"
        );
        debug_assert!(
            self.pending.is_empty(),
            "a second return window opened while another thread's was still \
             open: only the thread inside a window runs, so reaching this means \
             the per-thread resume let a sibling go -- and `continue_absorbed` \
             can no longer tell which window it should be resuming"
        );
        let regs = self.regs(thread)?;
        let pc = get(&regs, PC)?;
        let gate = pc + 4;
        let hardware = matches!(kind, ReturnKind::Fork { .. });
        let entry_breakpoint = self.remove_breakpoint(pc)?;
        if hardware {
            self.write(gate, &[0, 0, 0, 0x14])?;
            self.rsp.ok(&format!("Z1,{gate:x},4"))?;
        } else {
            self.temporary_breakpoint(gate)?;
        }
        let creation = if matches!(
            kind,
            ReturnKind::Fork { .. } | ReturnKind::Spawn { .. } | ReturnKind::PackedSpawn
        ) {
            let owner = self
                .ownership
                .as_ref()
                .ok_or_else(|| error("shutdown.ledger", "missing parent ownership"))?;
            Some(TraceOwnership {
                ledger: owner.ledger.clone(),
                tracee: owner.ledger.reserve(
                    umbra_core::shutdown::ResourceKind::Tracee,
                    Some(owner.tracee.id()),
                    self.id.0.generation,
                    self.exec_generation,
                )?,
            })
        } else {
            None
        };
        self.pending.insert(
            thread,
            Pending {
                creation,
                thread,
                entry: pc,
                entry_breakpoint,
                gate,
                kind,
                hardware,
            },
        );
        // **The trapping thread and nothing else**, and the `z0`/`Z0` sequence
        // above is untouched by that -- only which threads run changes. The
        // release a few lines up un-armed the entry stub process-wide; the bare
        // `c` that used to stand here is what let a sibling reach it unmediated.
        // Siblings resume at the first unqualified continue after
        // `finish_return` has re-armed the entry.
        #[cfg(feature = "signal-witness")]
        if !self.initial {
            let phase = if self
                .pending
                .values()
                .any(|p| matches!(p.kind, ReturnKind::Exec { .. }))
            {
                "exec"
            } else {
                "return-window"
            };
            self.signal_probe(phase)?;
        }
        self.continue_thread(thread)
    }
    fn install(&mut self) -> Result<()> {
        self.rsp.begin_setup();
        let result = self.install_inner();
        if result.is_ok() {
            self.rsp.finish_setup();
        }
        result
    }
    fn install_inner(&mut self) -> Result<()> {
        self.wait_for_dyld()?;
        // Before any breakpoint is planted, and that order is forced.
        //
        // At the target's exec stop dyld has recorded only the main image and
        // itself -- measured: `[<target>, /usr/lib/dyld]` -- so an inserted dylib
        // is not mapped yet and its trap sites have no address to plant on. The
        // only way to reach the moment it *is* mapped is to let dyld run to its
        // next image notification, and that means continuing the tracee. Doing
        // that after the libc stubs were breakpointed would resume into a tree of
        // planted breakpoints with no event loop to service them: a stub trap
        // during the remaining image loading would leave the PC sitting on a
        // `brk` this function does not know how to step over, and it would
        // re-trap forever. Running to the notification first costs nothing --
        // dyld's own loading was never intercepted anyway -- and leaves the
        // tracee stopped before any initializer, which is still before the
        // target's first instruction.
        // The gate is "is the image this session is running the one whose
        // interposer umbra is responsible for arming", and `current` is
        // maintained to answer exactly that -- see `Session::interposer` and
        // `retarget_interposer`. The comparison itself is unchanged; what moved
        // is that the right-hand side now follows the session's execs instead of
        // naming the launch target forever.
        let interposer = match self.interposer.clone() {
            Some((dylib, current, floor)) if image_path(self.id.0.native_id as i32)? == current => {
                // **A forked child is the one case that must not run to `main`.**
                //
                // `fork` copies the whole address space, so a child of a routed
                // tracee starts with the interposer already mapped *and* already
                // armed, sitting at the instruction after the `fork` -- long
                // past `main`, which it will never reach again. Running
                // `wait_for_image` here therefore resumed it with no breakpoint
                // planted on anything: it executed unmediated until its first
                // routed call became an `svc` the kernel does not know, took
                // `SIGSYS`, and ended the run. Both halves were wrong, and the
                // unmediated window was the dangerous half.
                //
                // Neither step is needed, because a forked child already has
                // what both produce. The address comes out of the image list
                // that fork copied -- complete, and readable without resuming
                // anything, which is what keeps the child stopped until its
                // traps are planted -- and the control block is armed already.
                //
                // Restricted to sessions with a parent so the target's own
                // `install` is provably unchanged: at its stop dyld has
                // registered only the main image and itself, so the lookup
                // would miss anyway, but this says so rather than relying on it.
                let inherited = if self.parent.is_some() {
                    image_base(&self.loaded_images()?, &dylib)
                } else {
                    None
                };
                match inherited {
                    Some(base) => Some((base, floor, self.interposer_armed(base)?)),
                    // A fresh image: `execve`/`posix_spawn` reset the address
                    // space, so dyld maps and this run arms, exactly as for the
                    // target itself.
                    None => Some((self.wait_for_image(&dylib)?, floor, false)),
                }
            }
            // Either this run routes nothing, or this is the sandbox installer,
            // where `DYLD_INSERT_LIBRARIES` also loaded the interposer. umbra
            // arms only the image it meant to route, so the installer's copy
            // stays inert and there is nothing here to intercept.
            _ => None,
        };
        // Shared cache addresses are system-wide on this native host. Verify every
        // remote instruction against local code before planting any breakpoint.
        //
        // The names come from `abi::TRACED_STUBS`, which `intercept()` also
        // reads to decide what to do when one of these breakpoints fires. One
        // table, two gates -- see that table for why it exists.
        for (name, number, _) in abi::TRACED_STUBS {
            for site in traced_stub_sites(name, *number)? {
                self.breakpoint(site, false)?;
            }
        }
        let main = main_image_base(&self.loaded_images()?)?;
        self.install_image(main)?;
        // The interposer's trap is an `svc` in *its* text, so its sites have to
        // be breakpointed explicitly: `install_image` covers one image at a time,
        // and the main image is the only other one scanned. That is also why a
        // tracee's self-injected `svc` is not intercepted -- it is in neither.
        if let Some((base, floor, inherited_arming)) = interposer {
            self.install_image(base)?;
            // Last, and the order is the mechanism rather than a preference: the
            // library is inert until this call, so every instruction between
            // dyld mapping it and this poke -- the whole library-initializer
            // window -- passes through to libc instead of trapping into
            // breakpoints that do not exist yet.
            //
            // A forked child skips it because fork already copied an armed
            // control block. Re-arming would fail anyway -- `arm_interposer`
            // requires the block to read back as zeroes, which is what proves
            // nothing else armed it -- and that check stays strict rather than
            // being relaxed to accommodate this path. A child forked *before*
            // its parent armed carries zeroes, so it is armed here like any
            // other image and routes from this instruction on.
            if !inherited_arming {
                self.arm_interposer(base, floor)?;
            }
        }
        Ok(())
    }

    /// Run the tracee forward until dyld reports the named image as mapped.
    ///
    /// Bounded by notifications rather than by time: dyld emits one per load
    /// batch, and a target that has not mapped the interposer after this many has
    /// not been given it. Failing here fails the launch, which is the right
    /// outcome -- an interposed target whose trap sites are not breakpointed
    /// turns every routed `open` into an `svc` the kernel answers with SIGSYS, so
    /// the tracee would die with no diagnosis at all.
    /// Find the interposer's load address, once dyld has actually mapped it.
    ///
    /// **Why this runs the tracee to its entry point rather than watching dyld.**
    /// At the target's exec stop dyld has registered only the main image and
    /// itself -- measured: `[<target>, /usr/lib/dyld]` -- so the interposer has
    /// no address yet. Two ways of waiting for one were tried and are recorded
    /// here so neither is tried again:
    ///
    /// * Re-querying `jGetLoadedDynamicLibrariesInfos` after each stop at
    ///   `_lldb_image_notifier` returns the same two images however many
    ///   notifications are consumed -- the list answers from
    ///   `dyld_all_image_infos`, which the notifier precedes.
    /// * Reading the batch out of the notifier's own arguments reports the main
    ///   image and nothing else, because on this dyld the initial image set --
    ///   libSystem and every inserted dylib -- is not announced through a second
    ///   notification at all.
    ///
    /// What *is* guaranteed is the ordering dyld promises: every image is mapped
    /// and every initializer has run before the main image's entry point. So the
    /// tracee is run to that entry, where the image list is complete, and nothing
    /// of the program itself has executed yet -- `LC_MAIN`'s `entryoff` is the
    /// address of `main`, and the breakpoint is retired before it runs.
    ///
    /// The cost is that library initializers run before umbra's syscall sites are
    /// planted, so a file operation issued by one is neither routed nor
    /// intercepted. That window is dyld's own image loading either way: the
    /// tracer has never intercepted it (`wait_for_dyld` exists precisely because
    /// breakpoints cannot be planted before it), and enforcement is already in
    /// force throughout, because the sandbox was installed before the exec.
    fn wait_for_image(&mut self, path: &Path) -> Result<u64> {
        let main = main_image_base(&self.loaded_images()?)?;
        let entry = self.main_entry_point(main)?;
        self.run_to(entry)?;
        let images = self.loaded_images()?;
        image_base(&images, path).ok_or_else(|| {
            // What dyld actually mapped is the diagnosis. The two ways this fails
            // -- dyld refused the insert, or the path umbra published and the path
            // dyld loaded are not the same file -- are indistinguishable from the
            // absence alone, and both leave a tracee that dies at its first
            // routed call with SIGSYS and no explanation.
            let loaded = images
                .iter()
                .filter_map(|image| image["pathname"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            error(
                "interposer",
                format!(
                    "{} was never mapped into the tracee, so its routing traps cannot be \
                     intercepted; loaded images: [{loaded}]",
                    path.display()
                ),
            )
        })
    }

    /// The address of the main image's `main`, from its `LC_MAIN` load command.
    ///
    /// `entryoff` is an offset from the start of the Mach header, which for every
    /// image this backend accepts -- thin arm64, `__TEXT` first -- is the load
    /// address. A `LC_UNIXTHREAD` image (a static executable) carries no
    /// `LC_MAIN` and is refused rather than guessed at.
    fn main_entry_point(&mut self, base: u64) -> Result<u64> {
        const LC_MAIN: u32 = 0x8000_0028;
        let (header, commands) = self.load_commands(base)?;
        let mut cursor = 0;
        for _ in 0..u32at(&header, 16)? {
            let command = u32at(&commands, cursor)?;
            let length = u32at(&commands, cursor + 4)? as usize;
            if length < 8 {
                return Err(error("Mach-O", "invalid load command"));
            }
            if command == LC_MAIN {
                let entry = u64at(
                    commands
                        .get(cursor..cursor + length)
                        .ok_or_else(|| error("Mach-O", "load command bounds"))?,
                    8,
                )?;
                return base
                    .checked_add(entry)
                    .ok_or_else(|| error("Mach-O", "entry point overflow"));
            }
            cursor += length;
        }
        Err(unsupported(
            "an interposed target must carry LC_MAIN; a static image's entry point \
             cannot be located this way",
        ))
    }

    /// Continue until the tracee stops at exactly this address, then retire the
    /// breakpoint that stopped it.
    ///
    /// Plants no breakpoint but its own, so a caller must not have planted syscall
    /// sites yet: a stub trap during the run would leave the PC on a `brk` this
    /// loop does not know how to step over, and it would re-trap forever.
    fn run_to(&mut self, address: u64) -> Result<()> {
        self.temporary_breakpoint(address)?;
        #[cfg(feature = "signal-witness")]
        self.signal_probe("internal")?;
        loop {
            self.stopped = false;
            let stop = self.rsp.request("c")?;
            if !stop.starts_with('T') {
                return Err(error("run_to", stop));
            }
            self.stopped = true;
            let fields = rsp::fields(&stop[3..]);
            self.thread = tid(
                rsp::number(
                    fields
                        .get("thread")
                        .ok_or_else(|| error("run_to", "no thread"))?,
                )?,
                self.id.0.generation,
            );
            // The tracee is stopped at its first instruction with one thread, so
            // the slot just assigned is the only thread there is to read.
            let regs = self.regs(self.thread)?;
            let signal = rsp::number(&stop[1..3])? as i32;
            if deliverable_signal(signal) {
                return Err(unsupported(format!(
                    "signal-during-internal-operation: signal={signal} stop={stop}"
                )));
            }
            if get(&regs, PC)? == address {
                self.remove_breakpoint(address)?;
                return Ok(());
            }
            let signal = rsp::number(&stop[1..3])? as i32;
            if ![libc::SIGSTOP, libc::SIGTRAP, libc::SIGCONT].contains(&signal) {
                return Err(error("run_to", stop));
            }
        }
    }

    fn wait_for_dyld(&mut self) -> Result<()> {
        let address = rsp::number(&self.rsp.request("qShlibInfoAddr")?)?;
        let mut info = [0; 16];
        self.task.read(address, &mut info)?;
        if u32at(&info, 4)? != 0 {
            return Ok(());
        }
        self.run_to_dyld_notification()
    }

    /// Continue until dyld's image notifier fires once, then stop there.
    ///
    /// **Plants no breakpoint but its own, and retires it before returning.** The
    /// tracee is resumed here, so a caller must not have planted syscall sites
    /// yet: a stub trap during dyld's remaining loading would leave the PC on a
    /// `brk` this loop does not know how to step over, and it would re-trap
    /// forever.
    fn run_to_dyld_notification(&mut self) -> Result<()> {
        let address = rsp::number(&self.rsp.request("qShlibInfoAddr")?)?;
        // START_SUSPENDED stops before dyld maps libSystem. Stop at its image
        // notification before installing syscall sites, rather than reading
        // shared-cache addresses that are not mapped yet. Initial dyld pointers
        // contain unapplied chained fixups, so resolve exported file symbols.
        let symbols = cache::command(
            std::process::Command::new("/usr/bin/xcrun").args([
                "nm",
                "-arch",
                "arm64e",
                "-g",
                "/usr/lib/dyld",
            ]),
            self.rsp.deadline(),
        )?;
        let symbols = String::from_utf8_lossy(&symbols.stdout);
        let symbol = |name: &str| -> Result<u64> {
            symbols
                .lines()
                .find_map(|line| {
                    let parts = line.split_whitespace().collect::<Vec<_>>();
                    (parts.len() == 3 && parts[2] == name).then(|| rsp::number(parts[0]))
                })
                .unwrap_or_else(|| Err(error("dyld", format!("missing {name}"))))
        };
        let notify = address
            .checked_sub(symbol("_dyld_all_image_infos")?)
            .and_then(|base| base.checked_add(symbol("_lldb_image_notifier").ok()?))
            .ok_or_else(|| error("dyld", "invalid notification address"))?;
        // Already parked on the notifier from a previous call, so planting a
        // breakpoint here and continuing would execute *that* breakpoint and stop
        // at the same instruction forever. Measured: without this step, every
        // notification after the first reports the same one-image batch, and the
        // wait never advances. One instruction is enough to get off the site; the
        // breakpoint below then catches the next call to it.
        self.temporary_breakpoint(notify)?;
        loop {
            self.stopped = false;
            let stop = self.rsp.request("c")?;
            if !stop.starts_with('T') {
                return Err(error("dyld startup", stop));
            }
            self.stopped = true;
            let fields = rsp::fields(&stop[3..]);
            self.thread = tid(
                rsp::number(
                    fields
                        .get("thread")
                        .ok_or_else(|| error("dyld", "no thread"))?,
                )?,
                self.id.0.generation,
            );
            // Pre-`main` dyld startup: one thread, just named by this stop.
            let regs = self.regs(self.thread)?;
            let signal = rsp::number(&stop[1..3])? as i32;
            if deliverable_signal(signal) {
                return Err(unsupported(format!(
                    "signal-during-internal-operation: signal={signal} stop={stop}"
                )));
            }
            if get(&regs, PC)? == notify {
                self.remove_breakpoint(notify)?;
                return Ok(());
            }
            let signal = rsp::number(&stop[1..3])? as i32;
            if ![libc::SIGSTOP, libc::SIGTRAP, libc::SIGCONT].contains(&signal) {
                return Err(error("dyld startup", stop));
            }
        }
    }
    /// Every image the tracee currently has mapped, as debugserver reports them.
    fn loaded_images(&mut self) -> Result<Vec<serde_json::Value>> {
        let reply = self
            .rsp
            .request("jGetLoadedDynamicLibrariesInfos:{\"fetch_all_solibs\":true}")?;
        let json: serde_json::Value =
            serde_json::from_str(&reply).map_err(|e| error("images", format!("{e}: {reply}")))?;
        json["images"]
            .as_array()
            .cloned()
            .ok_or_else(|| error("images", "missing images"))
    }

    /// One mapped image's Mach header and load-command block.
    ///
    /// Shared by the three readers that need them -- the `svc` scan, the entry
    /// point, and the interposer's arming section -- so a thin-arm64 check or a
    /// bound is written once rather than three times.
    fn load_commands(&mut self, base: u64) -> Result<([u8; 32], Vec<u8>)> {
        let mut header = [0; 32];
        self.task.read(base, &mut header)?;
        if u32at(&header, 0)? != 0xfeedfacf || u32at(&header, 4)? != 0x100000c {
            return Err(unsupported("image must be thin arm64 Mach-O"));
        }
        let size = u32at(&header, 20)? as usize;
        if size > 1024 * 1024 {
            return Err(error("Mach-O", "load command limit"));
        }
        let mut commands = vec![0; size];
        self.task.read(base + 32, &mut commands)?;
        Ok((header, commands))
    }

    /// The runtime address and size of one named section in a mapped image.
    ///
    /// `None` when the image carries no such section. The slide is taken from
    /// `__TEXT`'s `vmaddr`, exactly as `install_image` does: for every image this
    /// backend accepts, the load address is that segment's runtime address.
    fn image_section(
        &mut self,
        base: u64,
        segment: &[u8; 16],
        section: &[u8; 16],
    ) -> Result<Option<(u64, u64)>> {
        let (header, commands) = self.load_commands(base)?;
        let mut cursor = 0;
        let mut text_vm = None;
        let mut found = None;
        for _ in 0..u32at(&header, 16)? {
            let cmd = u32at(&commands, cursor)?;
            let size = u32at(&commands, cursor + 4)? as usize;
            if size < 8 {
                return Err(error("Mach-O", "invalid load command"));
            }
            let command = commands
                .get(cursor..cursor + size)
                .ok_or_else(|| error("Mach-O", "load command bounds"))?;
            if cmd == 0x19 {
                if command.get(8..14) == Some(b"__TEXT") {
                    text_vm = Some(u64at(command, 24)?);
                }
                for index in 0..u32at(command, 64)? as usize {
                    let at = 72 + index * 80;
                    if command.get(at..at + 16) == Some(section.as_slice())
                        && command.get(at + 16..at + 32) == Some(segment.as_slice())
                    {
                        found = Some((u64at(command, at + 32)?, u64at(command, at + 40)?));
                    }
                }
            }
            cursor += size;
        }
        let Some((address, length)) = found else {
            return Ok(None);
        };
        let vm = text_vm.ok_or_else(|| error("Mach-O", "missing __TEXT"))?;
        let address = base
            .checked_add(
                address
                    .checked_sub(vm)
                    .ok_or_else(|| error("Mach-O", "invalid section address"))?,
            )
            .ok_or_else(|| error("Mach-O", "address overflow"))?;
        Ok(Some((address, length)))
    }

    /// Arm the interposer, by writing its control block through the debugger port.
    ///
    /// **Called only after `install_image` has planted breakpoints on that
    /// image's `svc` sites, and that order is the whole mechanism.** umbra cannot
    /// learn the interposer's load address until dyld has mapped it, which means
    /// running the tracee to its entry point, which means every library
    /// initializer has already run. A library that armed itself from its own
    /// constructor was therefore live, with no breakpoint on its trap, for that
    /// whole window: the first `open` from any file-touching initializer became
    /// an `svc` the kernel does not know, took SIGSYS, and killed the launch
    /// before `main` with an undecoded debugger packet for a diagnosis.
    ///
    /// So umbra owns the arming instant, and it is one 16-byte poke into
    /// `__DATA,__umbra_arm`: the magic that switches the library on, and the
    /// descriptor floor it routes above. The block is required to read back as
    /// zeroes first, which is what says this is the library this build shipped
    /// and that nothing has armed it already.
    /// Is this mapped interposer already armed?
    ///
    /// True only for an address space a routed process forked: `fork` copies
    /// `__DATA,__umbra_arm` along with everything else, so the child inherits
    /// the magic its parent was given. Anything else -- a fresh `exec`, the
    /// sandbox installer's inert copy -- reads zeroes.
    ///
    /// A section that is missing or short is *not* reported as armed: that is
    /// not this library, and `arm_interposer` is the one that gets to say so,
    /// with the diagnosis it already carries.
    fn interposer_armed(&mut self, base: u64) -> Result<bool> {
        let Some((address, length)) = self.image_section(base, ARM_SEGMENT, ARM_SECTION)? else {
            return Ok(false);
        };
        if length < 16 {
            return Ok(false);
        }
        let mut current = [0u8; 16];
        self.task.read(address, &mut current)?;
        Ok(u64::from_le_bytes(current[..8].try_into().unwrap()) == interpose::ARM_MAGIC)
    }

    fn arm_interposer(&mut self, base: u64, floor: u32) -> Result<()> {
        let (address, length) = self
            .image_section(base, ARM_SEGMENT, ARM_SECTION)?
            .ok_or_else(|| {
                error(
                    "interposer",
                    "the loaded interposer carries no __DATA,__umbra_arm section, so it \
                     is not the library this build shipped",
                )
            })?;
        if length < 16 {
            return Err(error(
                "interposer",
                format!("the interposer's control block is {length} bytes, not 16"),
            ));
        }
        let mut current = [0u8; 16];
        self.task.read(address, &mut current)?;
        if current != [0u8; 16] {
            return Err(error(
                "interposer",
                format!(
                    "the interposer's control block is not zero before arming; something \
                     else wrote it: pid {} parent {:?} base {base:x} block {current:02x?}",
                    self.id.0.native_id, self.parent
                ),
            ));
        }
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&interpose::ARM_MAGIC.to_le_bytes());
        block[8..].copy_from_slice(&u64::from(floor).to_le_bytes());
        self.write(address, &block)
    }

    /// Breakpoint every `svc #0x80` in one mapped image's executable sections.
    ///
    /// Called for the main image, and for umbra's routing interposer when the run
    /// loads one. It is the whole of what "the tracer intercepts raw syscalls"
    /// means, and its bound is exactly these images: a tracee that writes an
    /// `svc` into memory it allocated itself has no site here to breakpoint, and
    /// that syscall is not intercepted. That limit is architectural rather than an
    /// oversight -- see `README.md` -- and what fail-closes host writes under it is
    /// the kernel-enforced Seatbelt profile, never this scan.
    fn install_image(&mut self, base: u64) -> Result<()> {
        let (header, commands) = self.load_commands(base)?;
        let mut cursor = 0;
        let mut text_vm = None;
        let mut sections = Vec::new();
        for _ in 0..u32at(&header, 16)? {
            let cmd = u32at(&commands, cursor)?;
            let size = u32at(&commands, cursor + 4)? as usize;
            if size < 8 {
                return Err(error("Mach-O", "invalid load command"));
            }
            let command = commands
                .get(cursor..cursor + size)
                .ok_or_else(|| error("Mach-O", "load command bounds"))?;
            if cmd == 0x19 {
                if command.get(8..14) == Some(b"__TEXT") {
                    text_vm = Some(u64at(command, 24)?);
                }
                for s in 0..u32at(command, 64)? as usize {
                    let off = 72 + s * 80;
                    let flags = u32at(command, off + 64)?;
                    if flags & 0x80000400 != 0 {
                        sections.push((u64at(command, off + 32)?, u64at(command, off + 40)?));
                    }
                }
            }
            cursor += size;
        }
        let vm = text_vm.ok_or_else(|| error("Mach-O", "missing __TEXT"))?;
        for (address, size) in sections {
            if size > 16 * 1024 * 1024 {
                return Err(unsupported("code section exceeds 16 MiB scan limit"));
            }
            let address = base
                .checked_add(
                    address
                        .checked_sub(vm)
                        .ok_or_else(|| error("Mach-O", "invalid section address"))?,
                )
                .ok_or_else(|| error("Mach-O", "address overflow"))?;
            let mut code = vec![0; size as usize];
            for (i, chunk) in code.chunks_mut(MAX_IO_BYTES).enumerate() {
                self.task.read(address + (i * MAX_IO_BYTES) as u64, chunk)?;
            }
            for (i, insn) in code.as_chunks::<4>().0.iter().enumerate() {
                if *insn == [1, 16, 0, 212] {
                    self.breakpoint(address + (i * 4) as u64, true)?;
                }
            }
        }
        Ok(())
    }
}
/// The address of the `svc` each [`abi::TRACED_STUBS`] row breakpoints, read
/// from this process's own copy of libsystem_kernel (the shared cache is
/// system-wide on this native host, so the address is the tracee's too).
///
/// A row with [`abi::TRACED_STUB_ALIASES`] resolves every candidate name whose
/// scan verifies and breakpoints each; it fails only if none verifies, and
/// then reports every candidate's failure.
fn traced_stub_sites(name: &str, number: u64) -> Result<Vec<u64>> {
    let aliases = abi::TRACED_STUB_ALIASES
        .iter()
        .filter(|(row, _)| *row == name)
        .map(|(_, alias)| *alias);
    let mut sites = Vec::new();
    let mut failures = Vec::new();
    for candidate in std::iter::once(name).chain(aliases) {
        match traced_stub_site(candidate, number) {
            Ok(site) if !sites.contains(&site) => sites.push(site),
            Ok(_) => {}
            Err(e) => failures.push(e),
        }
    }
    match (sites.is_empty(), failures.len()) {
        (false, _) => Ok(sites),
        (true, 1) => Err(failures.remove(0)),
        (true, _) => Err(error(
            "symbol",
            failures
                .iter()
                .map(|e| e.context.clone())
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

/// Scan one symbol for its `svc` and verify the syscall number it issues.
fn traced_stub_site(name: &str, number: u64) -> Result<u64> {
    let name_c = cstr(name.as_bytes())?;
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name_c.as_ptr()) };
    if ptr.is_null() {
        return Err(error("symbol", format!("missing {name}")));
    }
    // Strip arm64e function pointer authentication; only inspect local executable memory.
    let start = (ptr as u64) & 0x0000_ffff_ffff_ffff;
    let mut code = [0; 96];
    let me = Task(unsafe { mach_task_self() }, None);
    let result = me.read(start, &mut code);
    std::mem::forget(me);
    result?;
    let words = code.as_chunks::<4>().0;
    let offset = words
        .iter()
        .position(|b| *b == [1, 16, 0, 212])
        .ok_or_else(|| error("symbol", format!("no svc in {name}")))?;
    // **Verify the table's number against this host before planting the
    // breakpoint.** The stub's `svc` is preceded by a `movz x16, #imm`
    // that sets the syscall number, and that immediate -- not this
    // table -- is what the kernel will act on. `intercept()` dispatches
    // on the number, so a row whose number is wrong for this libc plants
    // a breakpoint whose firing is dispatched as something else, or
    // refused outright. Checking it here turns that into a launch
    // failure naming the symbol, rather than a run that dies at an
    // arbitrary later instruction.
    //
    // The *last* `movz x16` before the `svc` is the operative one:
    // `__fork` carries its `svc` at +16 and `unlinkat` at +48, with
    // setup instructions in between.
    let carried = words[..offset]
        .iter()
        .rev()
        .find_map(|word| {
            let instruction = u32::from_le_bytes(*word);
            // `movz x16, #imm16, lsl #0`: sf=1 opc=10 100101 hw=00, Rd=16.
            (instruction & 0xffe0_001f == 0xd280_0010)
                .then(|| u64::from((instruction >> 5) & 0xffff))
        })
        .ok_or_else(|| error("symbol", format!("no syscall number in {name}")))?;
    if carried != number {
        return Err(error(
            "symbol",
            format!("{name} issues syscall {carried}, not the {number} umbra traces it as"),
        ));
    }
    Ok(start + offset as u64 * 4)
}

/// Load address of the tracee's main executable (`MH_EXECUTE`).
fn main_image_base(images: &[serde_json::Value]) -> Result<u64> {
    images
        .iter()
        .find(|v| v["mach_header"]["filetype"].as_u64() == Some(2))
        .and_then(|main| main["load_address"].as_u64())
        .ok_or_else(|| error("images", "no main image"))
}

/// Load address of one mapped image, matched by the path it was loaded from.
///
/// Matched on the resolved path debugserver reports, compared against the path
/// umbra published and named in `DYLD_INSERT_LIBRARIES`. Both sides canonicalise
/// through the same cache directory, so an equality test is exact rather than
/// approximate -- and an inexact match here would breakpoint the wrong image's
/// `svc` sites, which is a reason to return `None` and fail the launch rather
/// than to widen the comparison.
fn image_base(images: &[serde_json::Value], path: &Path) -> Option<u64> {
    let wanted = std::fs::canonicalize(path).ok()?;
    images.iter().find_map(|image| {
        let pathname = image["pathname"].as_str()?;
        (std::fs::canonicalize(pathname).ok()? == wanted).then(|| image["load_address"].as_u64())?
    })
}

fn u32at(b: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(o..o + 4)
            .ok_or_else(|| error("Mach-O", "truncated u32"))?
            .try_into()
            .unwrap(),
    ))
}
fn u64at(b: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        b.get(o..o + 8)
            .ok_or_else(|| error("Mach-O", "truncated u64"))?
            .try_into()
            .unwrap(),
    ))
}
impl Session {
    fn settle_tracee(&mut self) {
        let Some(owner) = self.ownership.take() else {
            return;
        };
        if let Some(evidence) = &self.exit_evidence {
            if self.parent.is_some() || self.reaped {
                let _ = owner
                    .tracee
                    .confirm(umbra_core::shutdown::Proof::Accounted {
                        evidence: format!("{evidence:?}"),
                        root_collected: self.reaped && self.parent.is_none(),
                        root_owned: self.parent.is_none(),
                    });
                return;
            }
        }
        if self.parent.is_none() && self.reaped {
            let _ = owner
                .tracee
                .confirm(umbra_core::shutdown::Proof::Accounted {
                    evidence: "exact root collection".into(),
                    root_collected: true,
                    root_owned: true,
                });
        } else {
            let _ = owner
                .tracee
                .unconfirmed("tracee exit accounting or owned root collection missing");
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.done && self.live.is_some() {
            let _ = self.task.kill();
        }
        // Every backend path tears its sessions down in order first, children
        // before parents (`MacosTraceBackend::tear_down_sessions`). This only
        // catches a session dropped some other way, so even then the tracer is
        // never killed ahead of the barrier.
        if self.retirement_report.is_some() {
            self.finalize_failed_retirement();
        } else if !self.torn_down {
            let report = self.ordered_teardown();
            log_teardown(&report);
        }
        self.settle_tracee();
    }
}

/// Each provider retains both successful and failed host observations. Tracee
/// lifecycle transitions never invalidate this process-local library witness.
#[derive(Default)]
struct SpawnQualification {
    observed: Option<Option<&'static spawn::Layout>>,
}
impl SpawnQualification {
    fn get(
        &mut self,
        observe: impl FnOnce() -> Option<&'static spawn::Layout>,
    ) -> Option<&'static spawn::Layout> {
        *self.observed.get_or_insert_with(observe)
    }
}

/// Native prototype control, deliberately !Send/!Sync. Construct on the provider thread.
pub struct MacosTraceBackend {
    launch_stdio: Option<umbra_core::launch_stdio::LaunchStdio>,
    stdio_adopted: bool,
    stdio_consumed: bool,
    ledger: umbra_platform::shutdown::Ledger,
    poll_cursor: usize,
    spawn_sequence: u64,
    spawn_profile: SpawnQualification,
    options: Options,
    sessions: Vec<Session>,
    failed_retirements: Vec<Session>,
    /// Every ordered teardown this backend ran, in order. It is the teardown's
    /// diagnostic record: read-only, and never consulted by the backend itself.
    teardown_reports: Vec<TeardownReport>,
    events: VecDeque<TraceEvent>,
    // Stop replies consumed by quiesce still belong to the normal event loop.
    quiesced_stops: VecDeque<(usize, String)>,
    deadline: Option<Instant>,
    launched: bool,
    watchdog: Option<Watchdog>,
    // Provider-lifetime monitor, independent of launch/terminate.
    owner: Option<Watchdog>,
    generation: u64,
    /// The routing interposer this run loads and the image it routes in, if any.
    ///
    /// Set by `launch_traced` before the spawn and handed to every `Session` in
    /// the tree, so a descendant execing the same image breakpoints the same trap
    /// sites. See `Session::interposer`.
    interposer: Option<(PathBuf, PathBuf, u32)>,
    /// Test-only (#205): a session whose stops are not polled until the
    /// instant, so a fork it opened stays in its window across a teardown.
    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
    fork_window_hold: Option<(usize, Instant)>,
    _affine: PhantomData<Rc<()>>,
}
impl Default for MacosTraceBackend {
    fn default() -> Self {
        Self::new(Options::default())
    }
}
impl MacosTraceBackend {
    pub fn new(options: Options) -> Self {
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(serde_json::json!({"event": "owner-constructed"}));
        Self {
            launch_stdio: None,
            stdio_adopted: false,
            stdio_consumed: false,
            ledger: umbra_platform::shutdown::Ledger::new(),
            poll_cursor: 0,
            spawn_sequence: 0,
            spawn_profile: SpawnQualification::default(),
            options,
            sessions: vec![],
            failed_retirements: vec![],
            teardown_reports: vec![],
            events: VecDeque::new(),
            quiesced_stops: VecDeque::new(),
            deadline: None,
            launched: false,
            watchdog: None,
            owner: None,
            generation: 0,
            interposer: None,
            #[cfg(feature = "mutation-probe-spawn-lifecycle")]
            fork_window_hold: None,
            _affine: PhantomData,
        }
    }
    fn task_index(&self, task: TaskId) -> Result<usize> {
        self.sessions
            .iter()
            .position(|s| s.id == task && !s.done)
            .ok_or_else(|| {
                UmbraError::new(ErrorKind::StaleHandle, "task", "unknown or exited task")
            })
    }
    fn capture_spawn(
        &mut self,
        index: usize,
        thread: ThreadId,
        regs: RegisterSet,
        twin: PathBuf,
    ) -> Result<()> {
        let layout = self.spawn_profile.get(|| spawn_host::qualify().ok());
        let layout = layout.ok_or_else(|| unsupported("spawn ABI profile mismatch"))?;
        if std::env::var_os("UMBRA_SPAWN_LOG").is_some() {
            eprintln!("spawn-capture descriptor=non-null profile={}", layout.id);
        }
        if self.sessions[index].spawn.is_some()
            || self
                .sessions
                .iter()
                .filter(|s| s.live.as_ref().is_some_and(|live| live.spawn.is_some()))
                .count()
                >= 16
        {
            return Err(unsupported("spawn snapshot capacity exceeded"));
        }
        let s = &self.sessions[index];
        let mut descriptor = [0; spawn::DESCRIPTOR_BYTES];
        s.task
            .read(get(&regs, 2)?, &mut descriptor)
            .map_err(|_| unsupported("spawn malformed packed descriptor"))?;
        let snapshot = spawn::decode(layout, &descriptor, |address, output| {
            s.task.read(address, output)
        })?;
        if std::env::var_os("UMBRA_SPAWN_LOG").is_some() {
            eprintln!(
                "spawn-policy flags={:#06x} actions={}",
                u16::from_le_bytes(snapshot.attributes[..2].try_into().unwrap()) & !0x80,
                snapshot.actions.len()
            );
        }
        let plan = SpawnPlan {
            digest: spawn_host::digest(&snapshot.canonical_plan()?)?,
            actions: snapshot.actions.clone(),
            close_on_exec_default: snapshot.attributes[1] & 0x40 != 0,
        };
        let caller_pid = get(&regs, 0)?;
        if caller_pid != 0 && !s.task.writable_pid_slot(caller_pid) {
            return Err(unsupported("spawn invalid caller PID pointer"));
        }
        // Reserve queue and child-session capacity before the kernel can
        // create a child; attach adds no allocation for new token bookkeeping.
        self.sessions
            .try_reserve(1)
            .map_err(|e| error("spawn reserve", e))?;
        self.events
            .try_reserve(3)
            .map_err(|e| error("spawn reserve", e))?;
        self.spawn_sequence = self
            .spawn_sequence
            .checked_add(1)
            .ok_or_else(|| unsupported("spawn sequence exhausted"))?;
        let run_generation = self.sessions[0].id.0.generation;
        let s = &mut self.sessions[index];
        let token = SpawnToken {
            run_generation,
            parent: s.id,
            exec_generation: s.policy_exec_generation,
            thread,
            sequence: self.spawn_sequence,
        };
        s.spawn = Some(SpawnInFlight {
            token,
            snapshot,
            plan: plan.clone(),
            authorization: None,
            original: regs,
            returned: None,
            caller_pid,
            region: None,
            twin,
            child: None,
        });
        self.events
            .push_back(TraceEvent::SpawnPrepare { token, plan });
        Ok(())
    }

    fn arm_spawn(&mut self, index: usize, authorization: SpawnAuthorization) -> Result<()> {
        let s = &mut self.sessions[index];
        let allocation_task = s.task.0;
        let parent = s.id;
        let exec_generation = s.policy_exec_generation;
        let pending = s
            .spawn
            .as_mut()
            .ok_or_else(|| error("spawn", "no captured plan"))?;
        pending
            .barrier()
            .authorize(parent, exec_generation, authorization)?;
        pending.snapshot.require_supported()?;
        pending.authorization = Some(authorization);
        let mut address = 0;
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        let allocation_task =
            if std::env::var("UMBRA_FIX_243_REGION_FAULT").as_deref() == Ok("allocate") {
                mach2::port::MACH_PORT_NULL
            } else {
                allocation_task
            };
        // SAFETY: valid out pointer, this session owns the remote task right.
        unsafe {
            mach(
                mach2::vm::mach_vm_allocate(
                    allocation_task,
                    &mut address,
                    pending.snapshot.backing_plan()?.allocation_bytes as u64,
                    mach2::vm_statistics::VM_FLAGS_ANYWHERE,
                ),
                "spawn snapshot allocation",
            )?;
        }
        pending.region = Some(address);
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        crate::spawn_probe::region("allocated", pending.token, address, 0);
        // Store ownership before any write can fail.
        let backing = pending.snapshot.backing(address)?;
        let pid_offset = pending.snapshot.layout().pid_offset;
        let mut regs = pending.original.clone();
        let mut path = pending.twin.as_os_str().as_bytes().to_vec();
        path.push(0);
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if std::env::var("UMBRA_FIX_243_REGION_FAULT").as_deref() == Ok("write") {
            let result = s.write(0, &backing);
            let owned = s.spawn.as_ref().unwrap().region;
            eprintln!(
                "spawn-write-fault {}",
                serde_json::json!({
                    "failed": result.is_err(), "region": owned,
                    "parent_stopped": s.stopped, "child_created": s.spawn.as_ref().unwrap().child.is_some(),
                })
            );
            return result;
        }
        s.write(address, &backing)?;
        set(&mut regs, 1, s.allocate(&path)?)?;
        set(&mut regs, 0, address + pid_offset as u64)?;
        set(&mut regs, 2, address)?;
        s.set_regs(authorization.token.thread, &regs)?;
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if let Ok(count) = std::env::var("UMBRA_FIX_243_OVERLAP_PARENTS") {
            let count: usize = count
                .parse()
                .map_err(|_| error("spawn probe", "bad overlap count"))?;
            if !(2..=17).contains(&count) {
                return Err(error("spawn probe", "overlap count outside test bound"));
            }
            // Keep these parents stopped before kernel execution, while the
            // ordinary event loop continues other independently running tasks.
            let held: Vec<_> = self
                .sessions
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let p = s.live.as_ref()?.spawn.as_ref()?;
                    (p.region.is_some() && p.returned.is_none() && s.pending.is_empty())
                        .then_some((i, p.token.thread))
                })
                .collect();
            if held.len() < count {
                return Ok(());
            }
            for (i, thread) in held {
                let p = self.sessions[i].spawn.as_ref().unwrap();
                let address = p.region.unwrap();
                let expected = p.snapshot.backing(address)?;
                let mut actual = vec![0; expected.len()];
                self.sessions[i].task.read(address, &mut actual)?;
                if actual != expected {
                    return Err(error("spawn probe", "overlapping backing changed"));
                }
                crate::spawn_probe::region("overlap-verified", p.token, address, 0);
                self.sessions[i].return_stop(thread, ReturnKind::PackedSpawn)?;
            }
            return Ok(());
        }
        self.sessions[index].return_stop(authorization.token.thread, ReturnKind::PackedSpawn)
    }

    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
    fn probe_spawn_completion(&self, index: usize, phase: &str) -> Result<bool> {
        let parent = &self.sessions[index];
        let pending = parent.spawn.as_ref().expect("pending completion");
        let Some(child_index) = pending.child else {
            if std::env::var("UMBRA_FIX_243_PROBE_PHASE").as_deref() != Ok(phase) {
                return Ok(false);
            }
            // Observe an early completion call without acquiring or registering
            // a Task merely for the test. Unknown ownership is not reported as
            // successful registration.
            let Some(region) = pending.region else {
                return Err(error("spawn probe", "no region before child insertion"));
            };
            let mut bytes = [0; 4];
            parent.task.read(region + 400, &mut bytes)?;
            let pid = i32::from_le_bytes(bytes);
            let birth = bsd_info(pid).map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec));
            return crate::spawn_probe::rendezvous(
                phase,
                pid,
                birth,
                serde_json::json!({"watchdog_registered": null, "session_inserted": false,
                    "parent_stopped": parent.stopped, "child_stopped": null,
                    "region_owned": true}),
            );
        };
        let child = &self.sessions[child_index];
        let registered = self
            .watchdog
            .as_ref()
            .expect("watchdog")
            .tasks
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|task| Arc::ptr_eq(task, &child.task));
        crate::spawn_probe::rendezvous(
            phase,
            child.id.0.native_id as i32,
            child.birth,
            serde_json::json!({"watchdog_registered": registered, "session_inserted": true,
                "parent_stopped": parent.stopped, "child_stopped": child.stopped,
                "region_owned": pending.region.is_some()}),
        )
    }

    fn complete_spawn(
        &mut self,
        index: usize,
        authorization: SpawnAuthorization,
        success: bool,
    ) -> Result<()> {
        let pending = self.sessions[index]
            .spawn
            .as_ref()
            .ok_or_else(|| error("spawn", "no pending completion"))?;
        pending.barrier().complete(
            self.sessions[index].id,
            self.sessions[index].policy_exec_generation,
            authorization,
            success,
        )?;
        let child = pending.child;
        let caller = pending.caller_pid;
        let region = pending.region.expect("completion owns region");
        if let Some(child_index) = child {
            if caller != 0 {
                let pid = self.sessions[child_index].id.0.native_id as i32;
                #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                let caller = if self.probe_spawn_completion(index, "caller-copyout")? {
                    0
                } else {
                    caller
                };
                if self.sessions[index]
                    .write(caller, &pid.to_le_bytes())
                    .is_err()
                {
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    self.probe_spawn_completion(index, "caller-copyout-failed")?;
                    return Err(unsupported("spawn caller PID delivery failed"));
                }
            }
        }
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        let release_fault = self.probe_spawn_completion(index, "remote-free")?;
        let s = &mut self.sessions[index];
        // SAFETY: this task owns the dedicated allocation, with its original
        // extent. Failure retains the accounting and stops the run.
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        let task_port = if release_fault {
            mach2::port::MACH_PORT_NULL
        } else {
            s.task.0
        };
        #[cfg(not(feature = "mutation-probe-spawn-lifecycle"))]
        let task_port = s.task.0;
        let result =
            unsafe { mach2::vm::mach_vm_deallocate(task_port, region, spawn::REGION_BYTES as u64) };
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        crate::spawn_probe::region("release-result", authorization.token, region, result);
        if result != 0 {
            #[cfg(feature = "mutation-probe-spawn-lifecycle")]
            self.probe_spawn_completion(index, "remote-free-failed")?;
            return Err(unsupported("spawn snapshot release failed"));
        }
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        crate::spawn_probe::released_mapping(s.task.0, authorization.token, region);
        let pending = s.spawn.as_mut().unwrap();
        pending.region = None;
        let mut regs = pending.returned.take().unwrap();
        // Preserve return x0, CPSR and post-syscall PC. Only rewritten input
        // operands that are not return slots are restored.
        set(&mut regs, 1, get(&pending.original, 1)?)?;
        set(&mut regs, 2, get(&pending.original, 2)?)?;
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if self.probe_spawn_completion(index, "register-commit")? {
            return Err(error("spawn", "injected final register commit failure"));
        }
        let s = &mut self.sessions[index];
        s.set_regs(authorization.token.thread, &regs)?;
        s.spawn = None;
        if let Some(child_index) = child {
            let child = &self.sessions[child_index];
            self.events.push_back(TraceEvent::ThreadStarted {
                task: child.id,
                thread: child.thread,
            });
        }
        self.sessions[index].continue_run()
    }
    /// Resolve the session a caller-named thread belongs to.
    ///
    /// **The per-thread slots are consulted first, and `Session::thread` is only
    /// the fallback.** That slot holds whoever stopped last, so in a
    /// multithreaded tracee it aliases: every thread of a session that is not
    /// the most recent to stop would fail to resolve, or -- worse for
    /// `registers`/`set_registers`/`resume`, which all route through here --
    /// resolve to a session whose slot happens to name it while a different
    /// thread is the one the caller means. A thread the caller can legitimately
    /// name is one it was told about, and being told means either an entry it
    /// has not answered yet, a return in flight, or the stop it was just handed.
    ///
    /// Thread ids are unique across the machine, so consulting several sessions'
    /// slots cannot cross-match: the only failure this can produce is "not
    /// found", which is the refusal below.
    fn thread_index(&self, thread: ThreadId) -> Result<usize> {
        self.sessions
            .iter()
            .position(|s| {
                !s.done
                    && (s.entry.contains_key(&thread)
                        || s.pending.contains_key(&thread)
                        || s.spawn
                            .as_ref()
                            .is_some_and(|spawn| spawn.token.thread == thread)
                        || s.thread == thread)
            })
            .ok_or_else(|| {
                UmbraError::new(ErrorKind::StaleHandle, "thread", "unknown or exited thread")
            })
    }
    /// Bind a remote provider to its authenticated parent before launch.
    pub fn observe_owner(
        &mut self,
        identity: crate::OwnerIdentity,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<()> {
        if self.launched || self.owner.is_some() {
            return Err(unsupported("owner-observation: already configured"));
        }
        let watch = crate::owner::OwnerWatch::new(identity, stream)?;
        self.owner = Some(Watchdog::new(
            None,
            Some(watch),
            Arc::new(Mutex::new(WatchedTasks::default())),
        )?);
        Ok(())
    }
    fn check_deadline(&mut self) -> Result<()> {
        if let Some(reason) = self.owner.as_ref().and_then(Watchdog::reason) {
            return Err(error(reason.operation(), reason.message()));
        }
        if let Some(reason) = self.watchdog.as_ref().and_then(Watchdog::reason) {
            return Err(error(reason.operation(), reason.message()));
        }
        if self.deadline.is_some_and(|d| crate::clock::now() >= d) {
            for s in &self.sessions {
                if let Some(live) = &s.live {
                    let _ = live.task.kill();
                }
            }
            return Err(error("watchdog", "session timed out"));
        }
        Ok(())
    }
    /// Direct, deliberately unenforced tracer entry point for lower-level tests.
    ///
    /// It accepts only [`SandboxRequirement::UnsandboxedExperiment`], so a caller
    /// holding a rendered profile cannot reach an unenforced launch through it.
    pub fn launch_experimental(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        if self.stdio_adopted {
            self.consume_launch_stdio()?;
            return Err(unsupported("adopted stdio requires a supervised launch"));
        }
        if !matches!(spec.sandbox, SandboxRequirement::UnsandboxedExperiment) {
            return Err(unsupported(
                "launch_experimental installs no policy; use the supervised launch",
            ));
        }
        self.launch_traced(spec)
    }
    /// Validate the *shape* of an enforcement requirement before anything spawns.
    ///
    /// Whether the policy is actually installed is decided by the launch path
    /// below, which returns only after the target is stopped with the profile in
    /// force. This check exists so a malformed profile fails before a process
    /// is created rather than after.
    fn check_sandbox_shape(spec: &LaunchSpec) -> Result<()> {
        let profile = match &spec.sandbox {
            SandboxRequirement::UnsandboxedExperiment => return Ok(()),
            SandboxRequirement::Required(profile) => profile,
        };
        if profile.format() != SEATBELT_PROFILE_FORMAT {
            return Err(unsupported(format!(
                "unsupported sandbox profile format: {}",
                profile.format()
            )));
        }
        if profile.source().is_empty() || profile.source().len() > MAX_SANDBOX_PROFILE_BYTES {
            return Err(error("launch", "sandbox profile exceeds its size bounds"));
        }
        if profile.source().contains(&0) {
            return Err(error("launch", "sandbox profile contains a NUL byte"));
        }
        if !profile.write_root().is_absolute() || profile.write_root().as_bytes() == b"/" {
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                "launch",
                "sandbox write root must be an absolute path below the filesystem root",
            ));
        }
        Ok(())
    }
    fn consume_launch_stdio(&mut self) -> Result<Option<umbra_core::launch_stdio::LaunchStdio>> {
        if !self.stdio_adopted {
            // Legacy direct callers have never transferred launch endpoints.
            return Ok(None);
        }
        if self.stdio_consumed {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "platform.stdio",
                "launch stdio already consumed",
            ));
        }
        self.stdio_consumed = true;
        Ok(self.launch_stdio.take())
    }
    fn launch_traced(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        let launch_stdio = self.consume_launch_stdio()?;
        Self::check_sandbox_shape(&spec)?;
        if self.launched {
            return Err(error("launch", "one run per backend"));
        }
        if self
            .options
            .timeout_ms
            .is_some_and(|ms| ms == 0 || ms > 3_600_000)
        {
            return Err(error("launch", "timeout must be 1..3600000 ms"));
        }
        if !matches!(
            spec.policy.persistence,
            PersistencePolicy::LocalDevelopment | PersistencePolicy::NfsClientFsync
        ) {
            return Err(unsupported(
                "this backend supports explicit LocalDevelopment or NfsClientFsync; \
                 strict remote persistence is not qualified",
            ));
        }
        if spec
            .policy
            .inherited_fds
            .iter()
            .any(|fd| !(0..=2).contains(&fd.0))
        {
            return Err(unsupported("only explicit standard descriptors supported"));
        }
        if let Some(limit) = spec.policy.descriptor_limit {
            // Three standard descriptors plus room for the images dyld maps. A
            // limit this low is a configuration mistake, and the failure it would
            // produce -- `EMFILE` during image loading, before the target's first
            // instruction -- names nothing useful.
            if limit < 64 {
                return Err(error(
                    "launch",
                    "a descriptor limit below 64 cannot start a dynamically linked image",
                ));
            }
        }
        if spec.argv.is_empty() || !spec.executable.is_absolute() || !spec.cwd.is_absolute() {
            return Err(error(
                "launch",
                "absolute executable/cwd and argv[0] required",
            ));
        }
        // On the UnsandboxedExperiment branch, the tracee runs from a signed
        // twin but sees its own vendor identity: argv[0] is the absolute executable
        // the caller asked for, whatever argv[0] they supplied. argv[1..] and the
        // launched image (the twin) are passed through byte for byte. The caller's argv[0] is
        // still validated before it is dropped, so a malformed one is an error
        // rather than a silent substitution.
        let mut args = spec
            .argv
            .iter()
            .map(|a| cstr(a))
            .collect::<Result<Vec<_>>>()?;
        args[0] = cstr(spec.executable.as_bytes())?;
        let mut environment = spec.environment.clone();
        let cwd = cstr(spec.cwd.as_bytes())?;
        let deadline = self
            .options
            .timeout_ms
            .map(|ms| {
                crate::clock::now()
                    .checked_add(Duration::from_millis(ms))
                    .ok_or_else(|| error("launch", "lifetime overflow"))
            })
            .transpose()?;
        self.launched = true;
        self.deadline = deadline;
        self.check_deadline()?;
        let tasks = self
            .owner
            .as_ref()
            .map(|owner| owner.tasks.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(WatchedTasks::default())));
        self.watchdog = Some(Watchdog::new(deadline, None, tasks)?);
        let twin = cache::resign(
            Path::new(OsStr::from_bytes(spec.executable.as_bytes())),
            &self.options,
            operation_deadline(deadline, Duration::from_secs(10)),
        )?;
        // Enforcement is installed before the target's first instruction and the
        // interposer is loaded into that same image, so both are arranged here,
        // before the spawn, rather than injected afterwards.
        if spec.policy.interpose {
            let limit = spec.policy.descriptor_limit.ok_or_else(|| {
                error(
                    "launch",
                    "an interposed launch requires a descriptor limit: without the \
                     RLIMIT_NOFILE fence a virtual descriptor can collide with one the \
                     kernel later hands the tracee, which is a wrong-object read rather \
                     than a refusal",
                )
            })?;
            let published = interpose::publish(
                &self.options,
                operation_deadline(deadline, Duration::from_secs(10)),
            )?;
            // The target runs as its resigned twin, so this is the image umbra
            // will arm -- not the caller's `spec.executable`. Getting it wrong
            // leaves the interposer unarmed in the very image it was loaded for,
            // and every call it would have routed reaches the host instead. That
            // is why `install` *fails the launch* when it cannot find and arm the
            // library, rather than continuing with an inert one.
            let image = std::fs::canonicalize(&twin)
                .map_err(|e| error("launch", format!("resolving the target twin: {e}")))?;
            // One variable, and only dyld's. The interposer used to take its
            // image and its descriptor floor from two more, which made "the
            // library is live" something the environment decided -- and it
            // decided it during the library-initializer window, before the traps
            // were breakpointed. umbra now writes both into the library's own
            // `__DATA` after planting them; see `Session::arm_interposer`.
            let name = interpose::INSERT_VARIABLE;
            if environment.iter().any(|v| v.name == name.as_bytes()) {
                return Err(error(
                    "launch",
                    format!("the caller's environment already sets {name}"),
                ));
            }
            environment.push(EnvironmentVariable {
                name: name.as_bytes().to_vec(),
                value: published.as_os_str().as_bytes().to_vec(),
            });
            self.interposer = Some((published, image, limit));
        }
        let env = environment
            .iter()
            .map(|v| {
                if v.name.is_empty() || v.name.contains(&b'=') {
                    return Err(error("launch", "invalid environment name"));
                }
                let mut bytes = v.name.clone();
                bytes.push(b'=');
                bytes.extend(&v.value);
                cstr(&bytes)
            })
            .collect::<Result<Vec<_>>>()?;
        // Enforcement is installed by a trusted bootstrap that applies the
        // profile and then execs the target, so the image actually spawned is
        // the bootstrap and the target becomes its exec. The bootstrap is
        // addressed explicitly and never reached through a shell or PATH.
        let (image, args) = match &spec.sandbox {
            SandboxRequirement::UnsandboxedExperiment => (twin.clone(), args),
            SandboxRequirement::Required(profile) => {
                let bootstrap = cache::resign(
                    Path::new(SANDBOX_BOOTSTRAP),
                    &self.options,
                    operation_deadline(deadline, Duration::from_secs(10)),
                )?;
                // The target runs as its resigned twin, exactly as on the
                // unenforced path, so image identity and `_NSGetExecutablePath`
                // agree across both. The bootstrap sets the target's argv[0] to
                // the path it was handed, so argv[0] is the twin path here.
                let mut bytes = vec![
                    b"sandbox-exec".to_vec(),
                    b"-p".to_vec(),
                    profile.source().to_vec(),
                    twin.as_os_str().as_bytes().to_vec(),
                ];
                bytes.extend(spec.argv.iter().skip(1).cloned());
                let args = bytes.iter().map(|a| cstr(a)).collect::<Result<Vec<_>>>()?;
                (bootstrap, args)
            }
        };
        let path = cstr(image.as_os_str().as_bytes())?;
        let mut argv = args
            .iter()
            .map(|v| v.as_ptr() as *mut libc::c_char)
            .collect::<Vec<_>>();
        argv.push(std::ptr::null_mut());
        let mut envp = env
            .iter()
            .map(|v| v.as_ptr() as *mut libc::c_char)
            .collect::<Vec<_>>();
        envp.push(std::ptr::null_mut());
        let root_slot = self.ledger.reserve(
            umbra_core::shutdown::ResourceKind::Tracee,
            None,
            self.generation + 1,
            0,
        )?;
        let mut pid = 0;
        // SAFETY: initialized spawn objects, owned NUL-terminated arrays and valid out pointers.
        unsafe {
            let mut attr = std::mem::zeroed();
            let mut actions = std::mem::zeroed();
            errno(libc::posix_spawnattr_init(&mut attr), "spawnattr_init")?;
            if let Err(e) = errno(
                libc::posix_spawn_file_actions_init(&mut actions),
                "spawn actions",
            ) {
                libc::posix_spawnattr_destroy(&mut attr);
                return Err(e);
            }
            let result = (|| {
                let mut defaults: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut defaults);
                libc::sigaddset(&mut defaults, libc::SIGPIPE);
                errno(
                    libc::posix_spawnattr_setsigdefault(&mut attr, &defaults),
                    "spawn SIGPIPE default",
                )?;
                errno(
                    libc::posix_spawnattr_setflags(
                        &mut attr,
                        (libc::POSIX_SPAWN_START_SUSPENDED
                            | libc::POSIX_SPAWN_CLOEXEC_DEFAULT
                            | libc::POSIX_SPAWN_SETSIGDEF) as i16,
                    ),
                    "spawn flags",
                )?;
                errno(
                    posix_spawn_file_actions_addchdir_np(&mut actions, cwd.as_ptr()),
                    "spawn cwd",
                )?;
                for fd in &spec.policy.inherited_fds {
                    let source = launch_stdio
                        .as_ref()
                        .map_or(fd.0, |stdio| stdio.descriptors()[fd.0 as usize].as_raw_fd());
                    errno(
                        libc::posix_spawn_file_actions_adddup2(&mut actions, source, fd.0),
                        "spawn fd",
                    )?;
                }
                // The descriptor fence, applied here because `posix_spawn` has no
                // rlimit action and the child inherits this process's limits.
                //
                // `RLIMIT_NOFILE` is lowered in *this* process, soft and hard,
                // immediately before the spawn. Lowering the hard limit is what
                // makes the bound hold for the tracee's whole lifetime -- with only
                // the soft limit lowered the tracee raises it back and walks into
                // umbra's virtual descriptor range -- and it is irreversible for
                // an unprivileged process, which is why it happens last and why
                // `launch_traced` refuses a second run per backend. This provider
                // process holds a handful of descriptors, far below the limit, and
                // its own ceiling for the rest of its life is that same limit.
                if let Some(limit) = spec.policy.descriptor_limit {
                    let fence = libc::rlimit {
                        rlim_cur: limit as libc::rlim_t,
                        rlim_max: limit as libc::rlim_t,
                    };
                    errno(
                        libc::setrlimit(libc::RLIMIT_NOFILE, &fence),
                        "descriptor fence",
                    )?;
                }
                errno(
                    libc::posix_spawn(
                        &mut pid,
                        path.as_ptr(),
                        &actions,
                        &attr,
                        argv.as_ptr(),
                        envp.as_ptr(),
                    ),
                    "posix_spawn",
                )
            })();
            libc::posix_spawn_file_actions_destroy(&mut actions);
            libc::posix_spawnattr_destroy(&mut attr);
            if let Err(error) = result {
                root_slot.creation_failed();
                return Err(error);
            }
        }
        drop(launch_stdio);
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(
            serde_json::json!({"event": "acquired", "kind": "Tracee", "pid": pid}),
        );
        self.generation += 1;
        let session = root_slot
            .acquired(umbra_core::shutdown::Identity {
                pid: Some(pid),
                birth: None,
            })
            .and_then(|()| {
                Session::attach(
                    pid,
                    self.generation,
                    image,
                    None,
                    &self.options,
                    deadline,
                    self.watchdog.as_ref().unwrap(),
                    self.interposer.clone(),
                    TraceOwnership {
                        ledger: self.ledger.clone(),
                        tracee: root_slot.clone(),
                    },
                )
            });
        let session = match session {
            Ok(s) => s,
            Err(mut e) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    e.launch_tree_terminated = reap_launched_child(pid);
                    if e.launch_tree_terminated {
                        let _ = root_slot.confirm(umbra_core::shutdown::Proof::Accounted {
                            evidence: "exact root collection after failed attach".into(),
                            root_collected: true,
                            root_owned: true,
                        });
                    }
                }
                self.deadline = None;
                self.watchdog = None;
                return Err(e);
            }
        };
        let handle = ProcessHandle(session.id);
        self.sessions.push(session);
        self.sessions[0].install()?;
        self.events.push_back(TraceEvent::ThreadStarted {
            task: handle.0,
            thread: self.sessions[0].thread,
        });
        if matches!(spec.sandbox, SandboxRequirement::Required(_)) {
            if let Err(mut e) = self.complete_sandboxed_handoff(handle, &twin) {
                // Never return a live tree from a launch that could not prove the
                // boundary: the caller would have no handle to terminate it with.
                for session in &self.sessions {
                    if let Some(live) = &session.live {
                        let _ = live.task.kill();
                    }
                }
                self.tear_down_sessions();
                let mut reaped = !self.sessions.is_empty();
                for session in &self.sessions {
                    reaped &= (session.parent.is_none() && session.reaped)
                        || reap_launched_child(session.id.0.native_id as i32);
                }
                e.launch_tree_terminated = reaped;
                self.sessions.clear();
                self.events.clear();
                self.quiesced_stops.clear();
                self.deadline = None;
                self.watchdog = None;
                return Err(e);
            }
        }
        Ok(handle)
    }

    /// Drive the trusted bootstrap to the target's exec and stop there.
    ///
    /// Everything the installer does before that exec is fixed trusted launch
    /// code: its events are consumed here rather than exposed as workspace
    /// syscalls to rewrite. The handoff boundary is the exec stop itself, at
    /// which the profile is already in force and the target has not run an
    /// instruction. Anything else — the installer exiting, forking, or execing
    /// something other than the target — fails the launch.
    fn complete_sandboxed_handoff(&mut self, handle: ProcessHandle, target: &Path) -> Result<()> {
        // Bounded independently of the watchdog so a bootstrap that loops without
        // blocking still fails rather than spinning to the deadline.
        const MAX_BOOTSTRAP_EVENTS: usize = 8192;
        for _ in 0..MAX_BOOTSTRAP_EVENTS {
            let resume = match self.next_event()? {
                TraceEvent::Exec { task, thread, .. } if task == handle.0 => {
                    let image = image_path(task.0.native_id as i32)?;
                    let target = std::fs::canonicalize(target)
                        .map_err(|e| error("sandbox handoff target", e.to_string()))?;
                    if image != target {
                        return Err(error(
                            "sandbox.launch",
                            format!(
                                "installer exec'd {} instead of the target {}",
                                image.display(),
                                target.display()
                            ),
                        ));
                    }
                    // Report the stopped target the way an ordinary launch does.
                    self.sessions
                        .iter_mut()
                        .find(|session| session.id == task)
                        .unwrap()
                        .policy_exec_generation = 0;
                    self.events
                        .push_back(TraceEvent::ThreadStarted { task, thread });
                    return Ok(());
                }
                TraceEvent::Exit { status, .. } => {
                    return Err(error(
                        "sandbox.launch",
                        format!(
                            "sandbox installer exited with {status:?} before applying the \
                             policy and starting the target"
                        ),
                    ))
                }
                TraceEvent::Child { .. }
                | TraceEvent::SpawnPrepare { .. }
                | TraceEvent::SpawnChild { .. }
                | TraceEvent::SpawnAbort { .. } => {
                    return Err(error(
                        "sandbox.launch",
                        "sandbox installer created a child before installing the policy",
                    ))
                }
                TraceEvent::ThreadExited { .. } => continue,
                TraceEvent::ThreadStarted { thread, .. }
                | TraceEvent::SyscallEntry { thread, .. }
                | TraceEvent::SyscallExit { thread, .. }
                | TraceEvent::Exec { thread, .. }
                | TraceEvent::Signal { thread, .. } => thread,
            };
            self.resume(ResumeCommand {
                thread: resume,
                mode: ResumeMode::Syscall,
                signal: None,
            })?;
        }
        Err(error(
            "sandbox.launch",
            "sandbox installer did not reach the target exec within its event budget",
        ))
    }
}

/// The explicitly addressed system installer. Never resolved through PATH.
const SANDBOX_BOOTSTRAP: &str = "/usr/bin/sandbox-exec";

/// The running image of a process, used as evidence that the installer handed off
/// to the intended target rather than to something else.
fn image_path(pid: i32) -> Result<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: owned buffer with its true length; the call writes at most that many bytes.
    let written = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        return Err(error(
            "sandbox.launch",
            "cannot read the target's image path",
        ));
    }
    buffer.truncate(written as usize);
    Ok(PathBuf::from(OsStr::from_bytes(&buffer).to_owned()))
}
fn errno(code: i32, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(error(operation, "native call failed").with_errno(Errno(code)))
    }
}
extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> i32;
}

impl MacosTraceBackend {
    /// Allocate a task-owned, non-reused scratch path and prepare the ABI rewrite.
    /// Allocate one scratch buffer per resolved path operand and build the
    /// rewrite. Each operand gets its own bounded, NUL-terminated allocation,
    /// so a two-path syscall cannot alias its source and destination buffers.
    pub fn prepare_physical(
        &mut self,
        thread: ThreadId,
        physical: PhysicalOperation,
    ) -> Result<PreparedRewrite> {
        let i = self.thread_index(thread)?;
        let regs = self.sessions[i].regs(thread)?;
        let mut addresses = Vec::new();
        for rewrite in &physical.paths {
            let len = rewrite.path.0.as_bytes().len();
            if len >= abi::MAX_PATH {
                return Err(error("rewrite", "path exceeds 4 KiB"));
            }
            addresses.push(self.sessions[i].allocate(&vec![0; len + 1])?);
        }
        abi::prepare_paths(&regs, physical, &addresses)
    }
    /// Single-path compatibility wrapper over [`Self::prepare_physical`].
    pub fn prepare_rewrite(
        &mut self,
        thread: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite> {
        self.prepare_physical(
            thread,
            PhysicalOperation {
                operation,
                paths: vec![PathRewrite {
                    operand: PathOperand::Path,
                    path: PhysicalPath(path.clone()),
                }],
            },
        )
    }
    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
    fn probe_teardown_trigger(&mut self, phase: &str) -> Result<()> {
        let trigger = std::env::var("UMBRA_FIX_243_PROBE_TRIGGER").unwrap_or_default();
        self.probe_teardown(phase, &trigger)
    }
    #[cfg(any(test, feature = "mutation-probe-spawn-lifecycle"))]
    fn probe_teardown(&mut self, phase: &str, trigger: &str) -> Result<()> {
        match trigger {
            "deadline" => {
                self.deadline = Some(crate::clock::now());
                self.check_deadline()?;
            }
            "session-drop" => {
                // Before child insertion this drops the existing parent Session;
                // the acquired child remains in the watchdog task list.
                drop(self.sessions.pop().expect("registered Session"));
            }
            "backend-drop" => {
                // The probe replaces backend state within the same provider.
                let ledger = self.ledger.clone();
                drop(std::mem::take(self));
                self.ledger = ledger;
            }
            "terminate" => {
                self.terminate(
                    ProcessHandle(self.sessions[0].id),
                    TerminationPolicy::Immediate,
                )?;
            }
            "watchdog" => {
                let remaining = self
                    .deadline
                    .unwrap()
                    .saturating_duration_since(crate::clock::now());
                std::thread::sleep(remaining + Duration::from_millis(50));
            }
            _ => {}
        }
        Err(error(
            "spawn probe",
            format!("injected {phase} {trigger} failure"),
        ))
    }

    fn attach_child(
        &mut self,
        parent: usize,
        pid: i32,
        twin: PathBuf,
        kind: ChildKind,
        restore: Option<BTreeMap<u64, [u8; 4]>>,
        ownership: TraceOwnership,
    ) -> Result<()> {
        if pid <= 0 {
            return Err(error("child", "invalid child PID"));
        }
        self.generation += 1;
        // The child's own image, kept back from the move so the interposer
        // requirement can be pointed at it below. For a `fork` it is the
        // parent's -- the address space is a copy, so the running image is the
        // same one -- and for a `posix_spawn` it is the twin the spawn was
        // rewritten to name, which is a different binary whenever the tracee
        // asked for one.
        let image = twin.clone();
        let child_result = Session::attach(
            pid,
            self.generation,
            twin,
            Some(parent),
            &self.options,
            self.deadline,
            self.watchdog.as_ref().unwrap(),
            self.interposer.clone(),
            ownership,
        );
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if child_result.as_ref().err().is_some_and(|error| {
            error
                .to_string()
                .contains("injected attach failure after registration")
        }) && std::env::var_os("UMBRA_FIX_243_PROBE_TRIGGER").is_some()
        {
            return self.probe_teardown_trigger("acquired");
        }
        let mut child = child_result?;
        child.retarget_interposer(&image)?;
        // The parent supplied memory repairs, not breakpoints owned by this RSP.
        debug_assert!(child.breaks.is_empty());
        if let Some(restore) = restore {
            for (address, bytes) in restore {
                child.write(address, &bytes)?;
            }
        }
        child.install()?;
        child.policy_exec_generation = self.sessions[parent].policy_exec_generation;
        let event = TraceEvent::Child {
            parent: self.sessions[parent].id,
            child: ProcessHandle(child.id),
            kind,
        };
        let thread_event = TraceEvent::ThreadStarted {
            task: child.id,
            thread: child.thread,
        };
        self.sessions.push(child);
        if kind == ChildKind::Spawn {
            if let Some(spawn) = self.sessions[parent].spawn.as_ref() {
                let authorization = spawn.authorization.expect("armed spawn");
                let child_index = self.sessions.len() - 1;
                self.sessions[parent].spawn.as_mut().unwrap().child = Some(child_index);
                #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                if self.probe_spawn_completion(parent, "inserted")? {
                    return self.probe_teardown_trigger("inserted");
                }
                self.events.push_back(TraceEvent::SpawnChild {
                    authorization,
                    child: ProcessHandle(self.sessions[child_index].id),
                });
                return Ok(());
            }
        }
        self.events.push_back(event);
        self.events.push_back(thread_event);
        Ok(())
    }
    /// Close the return window `thread` is inside: retire the gate, re-arm the
    /// entry stub, and report the exit.
    ///
    /// `thread` names the window to close. The exit is attributed from that
    /// window's own [`Pending::thread`] and **never** from `Session::thread` --
    /// see that field for what reading the session slot here would cost.
    fn finish_return(&mut self, index: usize, thread: ThreadId, regs: RegisterSet) -> Result<()> {
        let pending = self.sessions[index].pending.remove(&thread).unwrap();
        let creation = pending.creation;
        let s = &mut self.sessions[index];
        if pending.hardware {
            s.rsp.ok(&format!("z1,{:x},4", pending.gate))?;
        } else {
            s.remove_breakpoint(pending.gate)?;
        }
        if let ReturnKind::Fork { restore } = &pending.kind {
            s.write(pending.gate, &restore[&pending.gate])?;
        }
        s.install_breakpoint(pending.entry, pending.entry_breakpoint)?;
        match pending.kind {
            ReturnKind::PackedSpawn => {
                let outcome = abi::outcome(&regs)?;
                let live = s.live.as_mut().expect("armed session resources");
                let spawn = live.spawn.as_mut().expect("armed spawn owns its snapshot");
                spawn.returned = Some(regs);
                let authorization = spawn.authorization.expect("armed authorization");
                if matches!(outcome, OperationOutcome::Success { .. }) {
                    let mut bytes = [0; 4];
                    let slot = spawn.region.expect("armed region") + 400;
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    let slot = if probe_unregistered(&live.task, slot)? {
                        0
                    } else {
                        slot
                    };
                    live.task.read(slot, &mut bytes)?;
                    let ownership =
                        creation.ok_or_else(|| error("shutdown.ledger", "unreserved spawn"))?;
                    #[cfg(feature = "shutdown-witness")]
                    umbra_platform::shutdown::witness(
                        serde_json::json!({"event": "acquired", "kind": "Tracee", "pid": i32::from_le_bytes(bytes)}),
                    );
                    ownership.tracee.acquired(umbra_core::shutdown::Identity {
                        pid: Some(i32::from_le_bytes(bytes)),
                        birth: None,
                    })?;
                    let twin = std::mem::take(&mut spawn.twin);
                    self.attach_child(
                        index,
                        i32::from_le_bytes(bytes),
                        twin,
                        ChildKind::Spawn,
                        None,
                        ownership,
                    )?;
                } else {
                    if let Some(owner) = creation {
                        owner.tracee.creation_failed();
                    }
                    self.events
                        .push_back(TraceEvent::SpawnAbort { authorization });
                }
            }
            ReturnKind::Syscall => {
                self.events.push_back(TraceEvent::SyscallExit {
                    task: s.id,
                    thread: pending.thread,
                    outcome: abi::outcome(&regs)?,
                });
            }
            ReturnKind::Wait => {
                if let OperationOutcome::Success { return_value } = abi::outcome(&regs)? {
                    for child in &mut self.sessions {
                        if child.parent == Some(index) && child.id.0.native_id == return_value {
                            child.reaped = true;
                        }
                    }
                }
                self.sessions[index].continue_run()?;
            }
            ReturnKind::Fork { restore } => {
                if let OperationOutcome::Success { return_value } = abi::outcome(&regs)? {
                    let ownership =
                        creation.ok_or_else(|| error("shutdown.ledger", "unreserved fork"))?;
                    #[cfg(feature = "shutdown-witness")]
                    umbra_platform::shutdown::witness(
                        serde_json::json!({"event": "acquired", "kind": "Tracee", "pid": return_value as i32}),
                    );
                    ownership.tracee.acquired(umbra_core::shutdown::Identity {
                        pid: Some(return_value as i32),
                        birth: None,
                    })?;
                    let twin = s.twin.clone();
                    self.attach_child(
                        index,
                        return_value as i32,
                        twin,
                        ChildKind::Fork,
                        Some(restore),
                        ownership,
                    )?;
                } else if let Some(owner) = creation {
                    owner.tracee.creation_failed();
                }
                self.sessions[index].continue_run()?;
            }
            ReturnKind::Spawn { pid_pointer, twin } => {
                if let OperationOutcome::Success { .. } = abi::outcome(&regs)? {
                    let mut bytes = [0; 4];
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    let pid_pointer = if probe_unregistered(&s.task, pid_pointer)? {
                        0
                    } else {
                        pid_pointer
                    };
                    s.task.read(pid_pointer, &mut bytes)?;
                    let ownership =
                        creation.ok_or_else(|| error("shutdown.ledger", "unreserved spawn"))?;
                    #[cfg(feature = "shutdown-witness")]
                    umbra_platform::shutdown::witness(
                        serde_json::json!({"event": "acquired", "kind": "Tracee", "pid": i32::from_le_bytes(bytes)}),
                    );
                    ownership.tracee.acquired(umbra_core::shutdown::Identity {
                        pid: Some(i32::from_le_bytes(bytes)),
                        birth: None,
                    })?;
                    self.attach_child(
                        index,
                        i32::from_le_bytes(bytes),
                        twin,
                        ChildKind::Spawn,
                        None,
                        ownership,
                    )?;
                } else if let Some(owner) = creation {
                    owner.tracee.creation_failed();
                }
                self.sessions[index].continue_run()?;
            }
            // **This arm is the failed-exec return, by construction**: a
            // successful `execve` never comes back here, it stops with reason
            // `exec`. So the candidate image is dropped rather than adopted, and
            // `twin` still names what this session is running -- which is what
            // the next `fork` hands `attach_child`.
            //
            // The candidate is bound and discarded rather than ignored with
            // `..`, so that a later edit adding a use for it has to decide what
            // a *failed* exec means for it rather than inheriting an answer.
            ReturnKind::Exec { twin: _candidate } => {
                self.sessions[index].continue_run()?;
            }
        }
        Ok(())
    }
    /// Dispatch a breakpoint stop taken by `thread` at one of this session's
    /// entry stubs.
    fn intercept(&mut self, index: usize, thread: ThreadId, mut regs: RegisterSet) -> Result<()> {
        let number = get(&regs, 16)?;
        let pc = get(&regs, PC)?;
        // **The disposition comes from `abi::TRACED_STUBS`, not from a list
        // written here.** This match used to carry its own set of syscall
        // numbers, which made it a second admission gate over the same decision
        // as `install()`'s stub list -- and the two drifted, exactly as #116's
        // `admit_run` lesson says a re-derived admission set does. It is now
        // exhaustive over `abi::Delivery`, so a stub row added to that table is
        // routed without touching this function, and a row whose disposition
        // nobody chose does not compile.
        //
        // `INTERPOSE_TRAP` keeps its own arm because it is not a libc stub: its
        // sites are breakpointed by `install_image` on the interposer's own
        // text, which is a different mechanism. Reaching here with it means the
        // breakpoint that fired was planted there, because no other image is
        // scanned. That is the containment: a trap number issued from anywhere
        // else was never breakpointed, so the kernel sees it and refuses it.
        // The trap is delivered on the namespace path deliberately: it *is* a
        // filesystem operation, it is answered by the same decode / resolve /
        // prepare / emulate / observe / commit sequence, and `resume` already
        // synthesises the exit stop for an entry whose PC moved past the `svc`.
        let delivery = if number == abi::INTERPOSE_TRAP {
            Some(abi::Delivery::Namespace)
        } else {
            abi::delivery(number)
        };
        match delivery {
            // Filesystem calls the caller decodes, rewrites and observes, plus
            // umbra's own routing trap.
            Some(abi::Delivery::Namespace) => {
                let s = &mut self.sessions[index];
                s.entry.insert(thread, pc);
                self.events.push_back(TraceEvent::SyscallEntry {
                    task: s.id,
                    thread,
                    registers: regs,
                });
            }
            Some(abi::Delivery::Fork) => {
                let s = &mut self.sessions[index];
                s.single_thread()?;
                let mut restore = s
                    .breaks
                    .iter()
                    .map(|(a, b)| (*a, b.original))
                    .collect::<BTreeMap<_, _>>();
                let mut original = [0; 4];
                s.task.read(pc + 4, &mut original)?;
                restore.insert(pc + 4, original);
                s.return_stop(thread, ReturnKind::Fork { restore })?;
                // Test-only (#205): hold a fork opened while another session
                // exists in its window, so a sibling's refusal tears the run
                // down while this fork's child is still unenrolled.
                #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                if self.sessions.len() > 1
                    && std::env::var_os("UMBRA_FIX_205_FORK_WINDOW_HOLD").is_some()
                {
                    let pid = self.sessions[index].id.0.native_id;
                    eprintln!("fork-window-hold parent={pid}");
                    self.fork_window_hold =
                        Some((index, crate::clock::now() + Duration::from_secs(10)));
                }
            }
            Some(abi::Delivery::Wait) => {
                let wanted = get(&regs, 0)? as i32;
                let children = self
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        s.parent == Some(index)
                            && !s.reaped
                            && (wanted <= 0 || s.id.0.native_id == wanted as u64)
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                let ready = children.iter().any(|i| self.sessions[*i].done);
                let plan = wait_plan(wanted, get(&regs, 2)?, !children.is_empty(), ready);
                let s = &mut self.sessions[index];
                match plan {
                    WaitPlan::Unsupported => {
                        return Err(unsupported(
                            "wait process-group/stopped/continued selection",
                        ))
                    }
                    // Success zero with the carry flag cleared, past the svc,
                    // leaving status/rusage and every child's reaped state
                    // exactly as the tracee left them.
                    WaitPlan::Poll => {
                        set(&mut regs, 0, 0)?;
                        let flags = get(&regs, CPSR)?;
                        set(&mut regs, CPSR, flags & !(1 << 29))?;
                        set(&mut regs, PC, pc + 4)?;
                        s.set_regs(thread, &regs)?;
                        s.continue_run()?;
                    }
                    WaitPlan::Park => {
                        s.single_thread()?;
                        s.waiting =
                            crate::ownership::Owned::new(children, s.id.0.native_id, "waiting");
                        #[cfg(feature = "signal-witness")]
                        s.signal_probe("park")?;
                    }
                    WaitPlan::Native => s.return_stop(thread, ReturnKind::Wait)?,
                }
            }
            Some(abi::Delivery::Exec) => {
                let slot = abi::path_slot(number)?;
                let old = abi::read_path(&mut &*self.sessions[index].task, get(&regs, slot)?)?;
                if !old.is_absolute() {
                    return Err(unsupported("relative exec/spawn path"));
                }
                let twin = cache::resign(
                    Path::new(OsStr::from_bytes(old.as_bytes())),
                    &self.options,
                    operation_deadline(self.deadline, Duration::from_secs(10)),
                )?;
                if number == 244 && get(&regs, 2)? != 0 {
                    return self.capture_spawn(index, thread, regs, twin);
                }
                let s = &mut self.sessions[index];
                let mut bytes = twin.as_os_str().as_bytes().to_vec();
                bytes.push(0);
                set(&mut regs, slot, s.allocate(&bytes)?)?;
                if number == 244 {
                    if std::env::var_os("UMBRA_SPAWN_LOG").is_some() {
                        eprintln!("spawn-capture descriptor=null");
                    }
                    let attributes = spawn_attributes()?;
                    let attr = s.allocate(&attributes.block)?;
                    let descriptor = spawn_descriptor(attr, attributes.len);
                    set(&mut regs, 2, s.allocate(&descriptor)?)?;
                    let mut pid_pointer = get(&regs, 0)?;
                    if pid_pointer == 0 {
                        pid_pointer = s.allocate(&[0; 4])?;
                        set(&mut regs, 0, pid_pointer)?;
                    }
                    s.set_regs(thread, &regs)?;
                    s.return_stop(thread, ReturnKind::Spawn { pid_pointer, twin })?;
                } else {
                    // Not `s.twin = twin` -- see `ReturnKind::Exec`. The image
                    // this session runs may not move until an exec has actually
                    // happened, and at this entry it has not.
                    s.set_regs(thread, &regs)?;
                    s.return_stop(thread, ReturnKind::Exec { twin })?;
                }
            }
            // A breakpoint fired for a number no row in `abi::TRACED_STUBS`
            // issues, and that is umbra's own wiring fault rather than anything
            // the tracee did: every breakpoint on this path was planted from
            // that table or on the interposer's text. Refusing stops the run
            // with a diagnosis naming the number, which is the right outcome --
            // guessing a disposition would resume or refuse a call nobody chose
            // to intercept.
            None => return Err(unsupported(format!("intercepted raw syscall {number}"))),
        }
        Ok(())
    }
    fn stop(&mut self, index: usize, reply: String, interrupted: bool) -> Result<()> {
        if reply.starts_with('O') {
            return Ok(());
        }
        if self.sessions[index].done {
            if self.sessions[index].exit_report.as_deref() == Some(reply.as_str()) {
                return Ok(());
            }
            return Err(error("stop", "unexpected stop for exited task"));
        }
        if reply.starts_with('W') || reply.starts_with('X') {
            let status =
                rsp::number(reply.get(1..3).ok_or_else(|| error("exit", "bad packet"))?)? as i32;
            let s = &mut self.sessions[index];
            s.signals.invalidate();
            s.done = true;
            s.stopped = false;
            s.exit_report = Some(reply.clone());
            self.events.push_back(TraceEvent::Exit {
                task: s.id,
                status: if reply.starts_with('W') {
                    ExitStatus::Code(status)
                } else {
                    ExitStatus::Signal(status)
                },
            });
            // Root was spawned by this process. Debugserver hands it back on exit.
            if s.parent.is_none() {
                s.reaped =
                    wait_owned_terminal(s.id.0.native_id as i32, libc::WNOHANG).unwrap_or(false);
            }
            // #186: take the teardown barrier's evidence now, while the exited
            // tracee is still a zombie someone has yet to reap.
            let _ = s.barrier();
            if s.parent.is_some() {
                s.retirement = Some(crate::retirement::Retirement::new(crate::clock::now()));
            }
            self.service_retirements();
            return Ok(());
        }
        if !reply.starts_with('T') {
            return Err(error("stop", reply));
        }
        // Test-only transport fault injection, after a genuine signal stop.
        #[cfg(feature = "signal-witness")]
        let (reply, interrupted) = {
            let phase = std::env::var("UMBRA_FIX284_PHASE").unwrap_or_default();
            if reply.starts_with("T1e") && matches!(phase.as_str(), "identity" | "ambiguous") {
                let original = reply.clone();
                let reply = if phase == "identity" {
                    format!(
                        "{}{}",
                        &reply[..3],
                        reply[3..]
                            .split(';')
                            .filter(|field| !field.starts_with("thread:"))
                            .collect::<Vec<_>>()
                            .join(";")
                    )
                } else {
                    reply
                };
                let interrupted = interrupted || phase == "ambiguous";
                let path =
                    std::env::var("UMBRA_FIX284_RECORD").map_err(|e| error("signal witness", e))?;
                let record = serde_json::json!({"original": original, "dispatched": reply,
                    "interrupted": interrupted, "fault": phase});
                std::fs::write(format!("{path}.stop.json"), record.to_string())
                    .map_err(|e| error("signal witness", e))?;
                (reply, interrupted)
            } else {
                (reply, interrupted)
            }
        };
        let fields = rsp::fields(&reply[3..]);
        let stopped_signal = rsp::number(&reply[1..3])? as i32;
        if deliverable_signal(stopped_signal) && !fields.contains_key("thread") {
            return Err(unsupported(format!(
                "signal-stop-identity: signal={stopped_signal} stop={reply}"
            )));
        }
        let thread = rsp::number(
            fields
                .get("thread")
                .ok_or_else(|| error("stop", "no thread"))?,
        )?;
        let s = &mut self.sessions[index];
        s.stopped = true;
        s.thread = tid(thread, s.id.0.generation);
        // The thread this stop is about, bound once. `Session::thread` is still
        // assigned above -- `thread_index` falls back to it for a thread that
        // holds no slot, and the `Exec` event reports it -- but every decision
        // below names this value rather than re-reading a field the next stop
        // overwrites.
        let thread = s.thread;
        if fields.get("reason") == Some(&"exec") {
            s.signals.invalidate();
            if deliverable_signal(stopped_signal) {
                return Err(unsupported(format!(
                    "signal-exec-transition: signal={stopped_signal} stop={reply}"
                )));
            }
            // **The exec happened, so this is where the candidate is adopted.**
            // `intercept` recorded the resigned image on the pending value
            // rather than writing it into `twin`, precisely so a *failed*
            // `execve` -- which returns to `finish_return` instead of stopping
            // here -- leaves `twin` naming the image still running.
            //
            // `None`, or a pending value of another kind, leaves `twin` alone.
            // An exec stop with nothing pending means the exec was not one umbra
            // intercepted, and umbra has no resigned image of its own to name for
            // it; keeping the previous value is what this did before the
            // candidate existed.
            //
            // **Drained whole rather than looked up by the stopping thread.** An
            // `execve` keeps one thread and discards the rest, and the survivor
            // is reported under a thread id belonging to the new image -- so the
            // thread stopping here need not be the one that entered the
            // `execve`, and keying the lookup on it would drop the candidate and
            // leave the exec'd image half-mediated. At most one window is open,
            // and none of them could survive the address space being replaced.
            let adopted = s.pending.take().into_values().find_map(|p| match p.kind {
                ReturnKind::Exec { twin } => Some(twin),
                _ => None,
            });
            if let Some(twin) = adopted {
                s.twin = twin;
            }
            s.scratch.clear();
            s.entry.clear();
            s.exec_generation += 1;
            s.policy_exec_generation = s.exec_generation;
            let owner = s
                .ownership
                .as_ref()
                .ok_or_else(|| error("shutdown.ledger", "missing exec ownership"))?;
            let previous = s
                .task
                .1
                .as_ref()
                .ok_or_else(|| error("shutdown.ledger", "unowned exec source right"))?
                .id();
            let right = owner.ledger.reserve_replacement(
                owner.tracee.id(),
                s.id.0.generation,
                s.exec_generation,
                previous,
            )?;
            let task = Arc::new(Task::acquire_owned(s.id.0.native_id as i32, right)?);
            #[cfg(any(test, feature = "retirement-witness"))]
            crate::ownership::task(s.id.0.native_id, &task);
            self.watchdog
                .as_ref()
                .unwrap()
                .replace(&s.task, task.clone());
            s.task = task;
            // Debugserver keeps its breakpoint registrations across exec on this
            // connection, and its Z0 handler is reference counted. Dropping our
            // own registry without releasing them would let install() below
            // re-register the same shared-cache addresses at count two, so the
            // single z0 that retires an entry site would decrement without
            // restoring the instruction and the tracee would re-trap forever.
            // Release every address this session still owns first.
            for address in s.breaks.keys().copied().collect::<Vec<_>>() {
                // The new image need not map an address the old one did. A
                // refused release is not fatal: it means the registration is
                // already gone, which is the state being asked for.
                let _ = s.rsp.request(&format!("z0,{address:x},4"))?;
            }
            s.breaks.clear();
            // **Before `install`, and that order is forced.** `twin` was set
            // from the pending candidate a few lines above, at the first moment
            // the exec is known to have happened; this is the stop where that
            // image is the one running, so it is where the interposer
            // requirement follows it. Do it after `install` and `install` would
            // have already decided, from the previous image's name, that this
            // run routes nothing here.
            let image = s.twin.clone();
            s.retarget_interposer(&image)?;
            // Resolve the new image.
            s.install()?;
            self.events.push_back(TraceEvent::Exec {
                task: s.id,
                thread: s.thread,
                exec_generation: s.exec_generation,
            });
            return Ok(());
        }
        let regs = s.regs(thread)?;
        let pc = get(&regs, PC)?;
        let signal = rsp::number(&reply[1..3])? as i32;
        if deliverable_signal(signal) {
            let refusal = if s.torn_down || s.signals.closed() {
                Some("signal-during-teardown")
            } else if interrupted {
                Some("ambiguous-interrupt-signal")
            } else if s
                .pending
                .values()
                .any(|p| matches!(p.kind, ReturnKind::Exec { .. }))
            {
                Some("signal-exec-transition")
            } else if !s.pending.is_empty() {
                Some("signal-during-return-window")
            } else if s.breaks.contains_key(&pc) || !s.entry.is_empty() {
                Some("signal-at-syscall-entry")
            } else if s.spawn.is_some() || s.initial {
                Some("signal-during-internal-operation")
            } else if s.signals.pending() || thread.0.native_id == 0 {
                Some("signal-stop-identity")
            } else {
                None
            };
            if let Some(reason) = refusal {
                return Err(unsupported(format!(
                    "{reason}: signal={signal} stop={reply}"
                )));
            }
            let token = s.signals.issue(s.id, thread, s.exec_generation, signal)?;
            self.events.push_back(TraceEvent::Signal {
                task: s.id,
                thread,
                signal,
                token,
            });
            return Ok(());
        }
        if s.pending.get(&thread).is_some_and(|p| p.gate == pc) {
            // A stop *at* the return gate is only a return when the breakpoint
            // planted there is what produced it. Accepting any signal here ate
            // one: a resumed umbra routing trap reaches Darwin's `nosys`, which
            // returns ENOSYS *and* posts `SIGSYS`, and the signal stop arrives
            // with `PC == gate` -- so it was classified as the syscall return,
            // the signal was dropped, and the tracee carried on with `-1/ENOSYS`
            // where it should have seen the namespace's own answer. The
            // supervisor no longer resumes a routing trap (see
            // `events.rs::syscall_entry`), and this refuses to hide it if
            // anything ever does again.
            if signal != libc::SIGTRAP {
                // **But a stop at the gate is not always about the gate.** A
                // transient signal can be delivered while the tracee happens to
                // sit on this PC -- `SIGCHLD` when a forked child exits is the
                // one that actually happens, because `fork` and `wait4` are
                // themselves gated syscalls, so their gate is exactly where the
                // child's death lands. Erroring on those took a `wait4` run out
                // over an ordinary event; the hardening above was aimed at
                // `SIGSYS`, and this keeps it aimed there.
                //
                // Continuing with `c` and no signal number forwards nothing,
                // and the gate breakpoint stays planted, so the real return is
                // still classified by the `SIGTRAP` that produced it.
                if TRANSIENT_SIGNALS.contains(&signal) {
                    s.continue_absorbed()?;
                    return Ok(());
                }
                return Err(error(
                    "return gate",
                    format!(
                        "fatal signal {signal} at the return gate for a syscall umbra \
                         rewrote; a return is a breakpoint trap and nothing else"
                    ),
                ));
            }
            return self.finish_return(index, thread, regs);
        }
        if s.breaks.contains_key(&pc) {
            #[cfg(feature = "signal-witness")]
            if !s.initial {
                let phase = std::env::var("UMBRA_FIX284_PHASE").unwrap_or_default();
                if matches!(phase.as_str(), "entry" | "identity" | "ambiguous")
                    && s.signal_probe(&phase)?
                {
                    s.continue_thread(thread)?;
                    return Ok(());
                }
            }
            return self.intercept(index, thread, regs);
        }
        // Continuing with no signal number suppresses these attach transients.
        // Through `continue_absorbed` rather than `continue_run`, so absorbing a
        // stop here cannot release the siblings into an open return window.
        if TRANSIENT_SIGNALS.contains(&signal) || (interrupted && signal == libc::SIGINT) {
            s.continue_absorbed()?;
            return Ok(());
        }
        Err(error(
            "tracee stop",
            format!("fatal signal/exception: {reply}"),
        ))
    }
}
/// The interposer's control-block segment and section, named exactly as Mach-O
/// stores them.
///
/// **Padded with NULs, not spaces.** A `section_64` name is 16 raw bytes and the
/// lookup is a byte comparison, so a space-padded copy of these literals matches
/// nothing -- and "found nothing" is indistinguishable from "this image has no
/// control block", which is exactly the answer that makes a forked child look
/// unarmed and sends umbra on to re-arm an already-armed block. Defined once so
/// the writer (`arm_interposer`) and the reader (`interposer_armed`) cannot
/// drift; they were written twice first, and the copy was wrong.
const ARM_SEGMENT: &[u8; 16] = b"__DATA\0\0\0\0\0\0\0\0\0\0";
const ARM_SECTION: &[u8; 16] = b"__umbra_arm\0\0\0\0\0";

fn deliverable_signal(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGPIPE | libc::SIGTERM | libc::SIGUSR1 | libc::SIGUSR2 | libc::SIGALRM
    )
}

/// Signals a supervised stop absorbs rather than forwards or fails on.
///
/// Attach transients (`SIGSTOP`, `SIGTRAP`, `SIGCONT`), terminal and I/O
/// notifications the tracee did not ask to be stopped for, and `SIGCHLD` --
/// which a run that forks delivers on its own schedule, including while the
/// tracee is parked on a return gate. Shared by the gate and the general stop
/// path so the two cannot drift: a signal that is merely noise in one place
/// must not be fatal in the other.
///
/// `SIGSYS` is deliberately absent. It is what an unintercepted routing trap
/// produces, and letting it pass is the exact failure the return gate exists to
/// refuse.
const TRANSIENT_SIGNALS: [i32; 8] = [
    libc::SIGHUP,
    libc::SIGTRAP,
    libc::SIGSTOP,
    libc::SIGCHLD,
    libc::SIGCONT,
    libc::SIGWINCH,
    libc::SIGURG,
    libc::SIGIO,
];

/// Upper bound for both spawn buffers handed to the tracee. The kernel copies
/// its own `sizeof`/`offsetof` out of them and never reports what it wants, so
/// each buffer is padded well past any released layout. Every field beyond the
/// content written here stays zero, which the descriptor reads as "not
/// provided" and the attribute block as a null extension pointer.
const SPAWN_BUFFER_BYTES: usize = 512;

/// Snapshot a libc-initialised attribute block requesting only a suspended
/// start.
///
/// `posix_spawnattr_t` is an opaque pointer to a private, per-release struct,
/// so the length comes from the allocator rather than a pinned constant:
/// `malloc_size` reports at least `sizeof` for a block libc allocated, and
/// reading it stays inside that allocation. The kernel copies
/// `offsetof(_posix_spawnattr, psa_ports)` bytes, always less than `sizeof`,
/// so a full-length snapshot covers whatever this release's kernel reads
/// without this crate knowing either offset.
/// A libc-built attribute block, padded for the tracee.
struct SpawnAttributes {
    /// Padded buffer to place in the tracee.
    block: Vec<u8>,
    /// Leading bytes of `block` that libc wrote; the descriptor's `attr_size`.
    len: usize,
}

/// Lay out the argument descriptor pointing at an already-placed attribute
/// block. `attr_size` only has to be non-zero: the kernel copies its own
/// `offsetof(_posix_spawnattr, psa_ports)` and never validates this value.
fn spawn_descriptor(attr_address: u64, attr_len: usize) -> Vec<u8> {
    let mut descriptor = vec![0; SPAWN_BUFFER_BYTES];
    descriptor[..8].copy_from_slice(&(attr_len as u64).to_le_bytes());
    descriptor[8..16].copy_from_slice(&attr_address.to_le_bytes());
    descriptor
}

fn spawn_attributes() -> Result<SpawnAttributes> {
    unsafe {
        let mut attr = std::mem::zeroed();
        errno(libc::posix_spawnattr_init(&mut attr), "spawnattr_init")?;
        let result = (|| {
            errno(
                libc::posix_spawnattr_setflags(&mut attr, 0x80),
                "spawnattr_setflags",
            )?;
            let size = libc::malloc_size(attr as *const libc::c_void);
            // A block outside this range is not the struct this expects; refuse
            // rather than hand the kernel a truncated or oversized attribute.
            if !(16..=SPAWN_BUFFER_BYTES).contains(&size) {
                return Err(unsupported("unexpected spawn attribute allocation"));
            }
            let bytes = std::slice::from_raw_parts(attr as *const u8, size);
            // psa_flags is the leading short in every released layout, so this
            // confirms the snapshot is the block that was just configured.
            if bytes[..2] != [0x80, 0] {
                return Err(unsupported("unexpected spawn attribute layout"));
            }
            // Pad: the kernel copies a length this crate never learns, so
            // anything it reads past these bytes must be zero rather than
            // neighbouring scratch.
            let mut block = vec![0; SPAWN_BUFFER_BYTES];
            block[..size].copy_from_slice(bytes);
            Ok(SpawnAttributes { block, len: size })
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}
// Only a successful waitpid is evidence that this launch's child was reaped.
fn wait_owned_terminal(pid: i32, options: i32) -> std::io::Result<bool> {
    let mut status = 0;
    // SAFETY: owned PID and valid status output; no status is inferred from ECHILD.
    let result = unsafe { libc::waitpid(pid, &mut status, options) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(result == pid && (libc::WIFEXITED(status) || libc::WIFSIGNALED(status)))
}
fn reap_launched_child(pid: i32) -> bool {
    loop {
        match wait_owned_terminal(pid, 0) {
            Ok(terminal) => return terminal,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

// ---- Ordered teardown (#186; cause-gate ruling F1-revised, R2, R3) ----
//
// **Why the order matters.** A debugger-held tracee does not start exiting while
// its debugserver holds it: a SIGKILL, another thread's `_exit` and
// `task_terminate` all wait, and on macOS 27.2.1 `task_terminate` returns
// KERN_FAILURE for it (#205, measured natively). Killing debugserver releases
// the hold before debugserver's own child loop runs. If the tracee then starts
// exiting (an exit was pending, or it was held at a breakpoint and traps again)
// before that loop reaches it, its `proc_prepareexit` has cleared `P_LTRACED`
// (`kern_exit.c:2278`) and the loop takes the untraced branch: launchd becomes
// its parent with `p_oppid` still set (`kern_exit.c:2610-2614`). Two things
// follow:
// - **#205:** the tracee's exit takes the notify-the-parent branch because of
//   `p_oppid` (`kern_exit.c:2748`, SZOMB `:2773`, SIGCHLD to launchd `:2779`),
//   not the kernel's system reap (`:2784-2817`). launchd never waits for it and
//   its original parent's `waitpid` gets ECHILD, so it stays a zombie until a
//   reboot. `barrier_decision` therefore never counts a zombie under launchd.
// - **#186:** the untraced branch does no job-control accounting
//   (`proc_reparentlocked` `:3549-3594`). The tracee's own `fixjobc`
//   (`kern_exit.c:2505`) then counts launchd as a qualifying parent whenever the
//   run's process group is in launchd's session, takes `pg_jobc` from 1 to 0,
//   and `orphanpg` sends SIGHUP+SIGCONT to the whole group, supervisor included
//   (`kern_proc.c:3203-3211`, `:3321-3383`).
//
// The teardown therefore opens by continuing its debugserver (a stopped one
// never answers `k`), and only debugserver ends the held tracee.
//
// **The order, per session.** RSP `k` over the existing connection (re-sent
// once after a stale stop reply), then a state barrier that proves the tracee
// has passed its exit accounting while the tracer is still alive, and only then
// `SIGKILL` and reap the tracer, and reap the handed-back root tracee.
//
// **The timeout path (ruling R3 option (i), review ruling 1 V3).** If the
// barrier does not confirm within the bound, the warning goes to stderr, the
// tracer is killed anyway, and one post-kill barrier decides: confirmed means
// `terminate` succeeds, still unconfirmed means it fails, and a tracee handed
// to launchd is never confirmed. The tracer kill on this path is the last
// resort for a hung debugserver (#205 R3): it can hand the tracee to launchd,
// and `terminate` then fails and names it. What this code never reaches stays
// residual (#205 R1, R2): debugserver ended by SIGKILL while its tracee is held,
// the provider ended by a fatal signal, any cancellation that bypasses
// `terminate`/`Drop`, and `Rsp`'s attach/connect failure fallback.

/// Record a teardown's `task_terminate` result in the teardown log
/// (`UMBRA_TEARDOWN_LOG`). Anything but success is expected for a tracee its
/// debugger holds; the ordered teardown that follows is what ends it.
fn record_task_terminate(pid: i32, result: i32) {
    if result != 0 && std::env::var_os("UMBRA_TEARDOWN_LOG").is_some() {
        eprintln!("umbra-platform-macos: task_terminate(pid={pid}) returned {result}");
    }
}

/// Bound on the RSP `k` send and reply plus the barrier, per session. Measured
/// replies took 16-91 ms (#186 arc, E4 K arm). With the two reap bounds, one
/// session's teardown takes at most 2 s, and a tree of N sessions at most 2N s.
/// Sessions are torn down one after another and each gets its own full bound,
/// so one session's timeout never shortens another's.
const TEARDOWN_BOUND: Duration = Duration::from_millis(1500);
/// Bound on reaping the root tracee once it is platform's child again.
const TRACEE_REAP_BOUND: Duration = Duration::from_millis(250);
/// `p_stat` value of a process awaiting collection (`bsd/sys/proc.h:159`).
const SZOMB: u32 = 5;
/// How long a zombie under launchd is given before it counts as handed to
/// launchd uncollected (#205 A1(a)): a kernel-reaped orphan passes through
/// SZOMB with ppid 1 on its way to the system reap.
const LAUNCHD_ZOMBIE_SETTLE: Duration = Duration::from_millis(50);

/// One barrier lookup of a pid.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Lookup {
    /// The process, with its `p_stat`, start identity and parent pid.
    Found {
        status: u32,
        start: (u64, u64),
        ppid: i32,
    },
    /// `ESRCH`: no process holds this pid.
    Absent,
    /// Any other failure, or a short or unreadable record.
    Error(String),
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)` with the zombie fallback enabled
/// (`arg = 1`), the only lookup the barrier uses.
///
/// **It finds a process from fork until the process is reaped, with no gap**
/// (xnu-12377.121.6, the source proof `TracerReportedExit` rests on):
/// - A process enters the pid hash at fork (`bsd/kern/kern_fork.c:988`) and
///   leaves it only when reaped (`bsd/kern/kern_exit.c:2983`, in
///   `reap_child_locked`). Both reap routes run after SZOMB is set
///   (`kern_exit.c:2773` before a parent's `wait4` at `:3100`; `:2803` before
///   the system reap at `:2815`), which is after `fixjobc` (`:2505`).
/// - The lookup tries `proc_find` and then `proc_find_zombref`
///   (`bsd/kern/proc_info.c:2198-2214`). `proc_find` fails only once
///   `P_REF_DEAD` (or `P_REF_NEW`) is set (`kern_proc.c:886-899`,
///   `:2520-2574`). `proc_find_zombref` finds the same pid-hash entry exactly
///   when `P_REF_DEAD` is set (`kern_proc.c:1121-1143`, `:814-817`). It waits,
///   rather than failing, while another thread holds `P_LIST_WAITING`.
/// - `P_REF_DEAD` is set in `proc_prepareexit` together with the move from
///   allproc to zombproc, under one `proc_list_lock` hold
///   (`kern_exit.c:2260-2269`). So the allproc→zombproc / `P_REF_DEAD`
///   transition opens no window in which this lookup misses the process. The
///   only window in which it misses begins with `phash_remove_locked`, at the
///   reap.
///
/// Hence `Absent` means the process was reaped, which means it is past
/// `fixjobc`. A pid is reallocated only when `phash_find_locked(pid)` finds
/// nothing (`kern_fork.c:972`), so a different start identity at the same pid
/// also means the original holder was reaped. `p_start` survives `exec`
/// (`kern_fork.c:1131-1135`, `in_exec`), so an exec cannot fake a reuse.
///
/// **An `exec` opens no window either.** It is the one lookup-relevant change a
/// live process makes to its pid-hash entry:
/// - The exec copy is never hashed beside the original: its `in_exec` branch
///   copies the pid and sets `P_REF_SHADOW` without `phash_insert_locked`
///   (`kern_fork.c:995-1004`).
/// - Its `P_REF_NEW` is cleared in `pinsertchild` (`kern_proc.c:2931-2932`),
///   before the swap.
/// - The swap is `phash_replace_locked` under `proc_list_lock`
///   (`kern_exec.c:5903`, `:5909`; `kern_proc.c:2483-2488`), one published
///   store (`osfmk/kern/smr.h:1412-1432`), so a reader sees the old proc or
///   the new one, never neither.
/// - `proc_find` retries on a shadow proc (`kern_proc.c:2546-2552`) and waits
///   out an exec in progress rather than failing (`:2558-2570`).
///
/// **The proof does not depend on any tracer report.** Every step above is a
/// property of the pid hash and of `proc_exit`'s order, and holds for any
/// process this backend attached to. That process was alive and hashed at
/// attach (`task_for_pid` and `vAttach` of that pid), and its start identity
/// was recorded then. So `Absent` and a reuse are positive for every session,
/// live or done (review ruling 1 V1). A session whose tracer also reported the
/// exit is labelled `TracerReportedExit`, and one without a report
/// `ReapedProven`.
fn lookup(pid: i32) -> Lookup {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if got == size {
        return Lookup::Found {
            status: info.pbi_status,
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            ppid: info.pbi_ppid as i32,
        };
    }
    let error = std::io::Error::last_os_error();
    if got <= 0 && error.raw_os_error() == Some(libc::ESRCH) {
        Lookup::Absent
    } else if got <= 0 {
        Lookup::Error(error.to_string())
    } else {
        Lookup::Error(format!("short record: {got} of {size} bytes"))
    }
}

/// The barrier's lookup after the launchd-zombie settle (#205 A1(a)): a
/// zombie under launchd is read once more, after `reread` waits
/// [`LAUNCHD_ZOMBIE_SETTLE`]. A kernel-reaped orphan passes through SZOMB with
/// ppid 1 on its way to the system reap (`kern_exit.c:2803` before `:2815`) and
/// is then gone; one still there was handed to launchd uncollected. The
/// settle is defensive: the system reap holds `proc_list_lock` across that
/// window, so the transient should be all but invisible to a lookup.
fn settled_lookup(first: Lookup, reread: impl FnOnce() -> Lookup) -> Lookup {
    match first {
        Lookup::Found {
            status: SZOMB,
            ppid: 1,
            ..
        } => reread(),
        other => other,
    }
}

/// How long the unenrolled-fork sweep waits for a fork still in the kernel to
/// produce its child, and for a killed child to be gone or a zombie (#205).
const UNENROLLED_FORK_SETTLE: Duration = Duration::from_millis(100);
const UNENROLLED_FORK_BOUND: Duration = Duration::from_secs(1);

/// The frozen shutdown allowance: the retirement windows already running,
/// plus one teardown per live session, and the unenrolled fork-child sweep
/// for `fork_windows` open fork windows (#205). No reservation is admitted
/// after the freeze, so no window can open later.
fn shutdown_allowance(
    retirement: Duration,
    live_sessions: usize,
    fork_windows: usize,
) -> Result<Duration> {
    let overflow = || error("shutdown", "cleanup schedule overflow");
    let mut remaining = retirement;
    for _ in 0..live_sessions {
        remaining = remaining
            .checked_add(TEARDOWN_BOUND + rsp::TRACER_REAP_BOUND + TRACEE_REAP_BOUND)
            .ok_or_else(overflow)?;
    }
    if fork_windows > 0 {
        // One shared settle and one shared wait, whatever the count.
        remaining = remaining
            .checked_add(UNENROLLED_FORK_SETTLE + UNENROLLED_FORK_BOUND)
            .ok_or_else(overflow)?;
    }
    Ok(remaining)
}

/// The fork windows of one live session that are still open: a fork return
/// not yet processed, whose child's slot is reserved (#205).
fn open_fork_windows(live: &LiveSession) -> Vec<ThreadId> {
    live.pending
        .iter()
        .filter(|(_, p)| matches!(p.kind, ReturnKind::Fork { .. }) && p.creation.is_some())
        .map(|(thread, _)| *thread)
        .collect()
}

/// `PROC_FLAG_TRACED` in `proc_bsdinfo::pbi_flags` (`sys/proc_info.h`).
const PROC_FLAG_TRACED: u32 = 0x2;

/// The live (not zombie) children of `parent`, read-only.
fn live_children(parent: i32) -> Vec<i32> {
    let mut pids = [0i32; 256];
    // SAFETY: `pids` is writable for its size in bytes.
    let count = unsafe {
        libc::proc_listchildpids(
            parent,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as i32,
        )
    };
    if count <= 0 {
        return Vec::new();
    }
    pids[..(count as usize).min(pids.len())]
        .iter()
        .copied()
        .filter(|&pid| pid > 0 && bsd_info(pid).is_some_and(|info| info.pbi_status != SZOMB))
        .collect()
}

/// Settle an unenrolled fork child's slot as unconfirmed, naming why, and say
/// so on stderr beside the other teardown lines (#205). Returns the
/// diagnostic, which `terminate` also reports.
fn unenrolled_unconfirmed(
    tracee: &umbra_platform::shutdown::Reservation,
    diagnostic: String,
) -> String {
    eprintln!("umbra-platform-macos: unconfirmed {diagnostic}");
    let _ = tracee.unconfirmed(diagnostic.clone());
    diagnostic
}

/// Begin ending one fork child the platform never enrolled (#205): record its
/// identity on its ledger reservation and SIGKILL it. The parent cannot reap
/// it meanwhile: a fork needs a single-threaded parent (`single_thread`), and
/// that one thread is in the fork or held at its return gate while the window
/// is open; the child itself spins at that gate. So the pid can name another
/// process only if something else ended the child, the parent died and
/// launchd reaped the child, and the pid was reused. The read that takes its
/// birth right before the signal therefore must still show `parent`'s child
/// with the birth the census read ([`unenrolled_identity`]), or nothing is
/// signalled and the slot is unconfirmed. It is untraced unless a debugger is
/// attaching to it; then it is
/// left to that debugger (debugger-last: a debugger's program is ended only
/// through it) and its slot names why it stays unconfirmed. Returns the
/// child's birth when it was signalled; its end is awaited by
/// [`await_unenrolled_ends`], shared with every other signalled child.
/// Otherwise returns the diagnostic of the unconfirmed slot.
fn signal_unenrolled_child(
    parent: i32,
    pid: i32,
    census_birth: Option<(u64, u64)>,
    tracee: &umbra_platform::shutdown::Reservation,
) -> std::result::Result<(u64, u64), String> {
    let Some(info) = bsd_info(pid) else {
        return Err(unenrolled_unconfirmed(
            tracee,
            format!("unenrolled fork child pid={pid} of pid={parent}: gone before its identity was read"),
        ));
    };
    let seen = (info.pbi_ppid, (info.pbi_start_tvsec, info.pbi_start_tvusec));
    let birth = match unenrolled_identity(parent, census_birth, seen) {
        Ok(birth) => birth,
        Err(why) => {
            return Err(unenrolled_unconfirmed(
                tracee,
                format!("unenrolled fork child pid={pid} of pid={parent}: {why}; not signalled"),
            ))
        }
    };
    if let Err(refused) = tracee.acquired(umbra_core::shutdown::Identity {
        pid: Some(pid),
        birth: Some(birth),
    }) {
        return Err(unenrolled_unconfirmed(
            tracee,
            format!(
                "unenrolled fork child pid={pid} of pid={parent}: its identity was not recorded ({refused}); not signalled"
            ),
        ));
    }
    if info.pbi_flags & PROC_FLAG_TRACED != 0 {
        return Err(unenrolled_unconfirmed(
            tracee,
            format!(
                "unenrolled fork child pid={pid} of pid={parent}: traced but not enrolled; left to its debugger"
            ),
        ));
    }
    // SAFETY: plain libSystem call on an untraced, unreaped child of a live
    // session; see above for why the pid names this child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    Ok(birth)
}

/// Whether one read of a census-found fork child, `(ppid, birth)`, still names
/// that child (#205), and its birth if so; otherwise why not.
fn unenrolled_identity(
    parent: i32,
    census_birth: Option<(u64, u64)>,
    seen: (u32, (u64, u64)),
) -> std::result::Result<(u64, u64), String> {
    let (ppid, birth) = seen;
    if i64::from(ppid) != i64::from(parent) {
        return Err(format!(
            "no longer a child of pid={parent} (parent pid={ppid})"
        ));
    }
    if census_birth.is_some_and(|census| census != birth) {
        return Err(format!(
            "another birth at its pid ({}.{:06}) than the census read",
            birth.0, birth.1
        ));
    }
    Ok(birth)
}

/// The fork-child sweep's one wait (#205): re-read every signalled child
/// `(pid, birth)` until each has ended or `limit` passes, whatever the number
/// of children, so the sweep's worst case is one bound, not one per child.
/// Returns, per child, its ending state ([`unenrolled_child_state`]) or the
/// last lookup taken by the limit.
fn await_unenrolled_ends(
    children: &[(i32, (u64, u64))],
    limit: Instant,
    mut lookup: impl FnMut(i32) -> Lookup,
    now: impl Fn() -> Instant,
    mut pause: impl FnMut(),
) -> Vec<std::result::Result<&'static str, Lookup>> {
    let mut ended: Vec<Option<&'static str>> = vec![None; children.len()];
    let mut last: Vec<Lookup> = vec![Lookup::Absent; children.len()];
    loop {
        for (index, (pid, birth)) in children.iter().enumerate() {
            if ended[index].is_none() {
                let found = lookup(*pid);
                ended[index] = unenrolled_child_state(&found, *birth);
                last[index] = found;
            }
        }
        if ended.iter().all(Option::is_some) || now() >= limit {
            break;
        }
        pause();
    }
    ended
        .into_iter()
        .zip(last)
        .map(|(state, last)| state.ok_or(last))
        .collect()
}

/// Whether one lookup proves a SIGKILLed unenrolled fork child ended (#205):
/// no process at its pid, a different process there (its birth replaced), or
/// a zombie of the same birth. A failed lookup proves nothing: it is never
/// read as disappearance.
fn unenrolled_child_state(found: &Lookup, birth: (u64, u64)) -> Option<&'static str> {
    match found {
        Lookup::Absent => Some("gone"),
        Lookup::Found { start, .. } if *start != birth => Some("gone"),
        Lookup::Found { status, .. } if *status == SZOMB => Some("a zombie under its parent"),
        Lookup::Found { .. } | Lookup::Error(_) => None,
    }
}

/// A candidate unenrolled fork child: its pid and the birth the census read
/// (`None` when unread).
type ForkCandidate = (i32, Option<(u64, u64)>);

/// A session's identity for the fork-child sweep: pid, recorded birth, and
/// whether it still has live resources.
type SessionIdentity = (i32, Option<(u64, u64)>, bool);

/// Whether a live child of a session, with pid `pid` and birth `birth`, is an
/// enrolled session (#205): a session with the same pid and the same birth,
/// or, when a session recorded no birth, one that still has live resources.
/// A retired session's pid alone never excludes a child, since that pid can
/// have been reused by a new, unenrolled fork child.
fn enrolled_identity(sessions: &[SessionIdentity], pid: i32, birth: Option<(u64, u64)>) -> bool {
    sessions.iter().any(|(session, recorded, live)| {
        *session == pid
            && match recorded {
                Some(recorded) => birth == Some(*recorded),
                None => *live,
            }
    })
}

fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (got == size).then_some(info)
}

/// The barrier's decision from one lookup, for a session that is not yet
/// confirmed by a reap. Pure, so it is unit-tested directly.
///
/// `report` is the tracer's `W`/`X` packet if it reported this tracee's exit
/// (implement ruling 1). Absence and pid reuse are positive with or without it,
/// by the report-independent proof on [`lookup`] (review ruling 1 V1). Only
/// the label differs. A present process that is not in SZOMB, a lookup error
/// and a missing identity stay `Unconfirmed`.
fn barrier_decision(
    report: Option<&str>,
    birth: Option<(u64, u64)>,
    lookup: &Lookup,
) -> BarrierReading {
    match (lookup, report) {
        (Lookup::Absent, Some(report)) => {
            BarrierReading::Confirmed(BarrierEvidence::TracerReportedExit {
                report: report.to_string(),
                lookup: ReportedExitLookup::Absent,
                admitted_by: ADMITTED_ABSENT,
            })
        }
        (Lookup::Absent, None) => BarrierReading::Confirmed(BarrierEvidence::ReapedProven {
            lookup: ReportedExitLookup::Absent,
            admitted_by: ADMITTED_ABSENT,
        }),
        (Lookup::Error(why), _) => BarrierReading::Unconfirmed(format!("lookup failed: {why}")),
        (Lookup::Found { .. }, _) if birth.is_none() => {
            BarrierReading::Unconfirmed("no attach-time identity".into())
        }
        (Lookup::Found { start, .. }, Some(report)) if Some(*start) != birth => {
            BarrierReading::Confirmed(BarrierEvidence::TracerReportedExit {
                report: report.to_string(),
                lookup: ReportedExitLookup::PidReused { start: *start },
                admitted_by: ADMITTED_REUSED,
            })
        }
        (Lookup::Found { start, .. }, None) if Some(*start) != birth => {
            BarrierReading::Confirmed(BarrierEvidence::ReapedProven {
                lookup: ReportedExitLookup::PidReused { start: *start },
                admitted_by: ADMITTED_REUSED,
            })
        }
        // #205: a zombie under launchd is never exit evidence. The kernel hands
        // a traced program to launchd, uncollected and with its original
        // parent left set, when its debugger finishes exiting after the
        // program started exiting (kern_exit.c:2610-2614, `:2748`); launchd
        // never waits for it and the original parent's `waitpid` gets ECHILD.
        // `settled_lookup` has already re-read it once, so a kernel-reaped
        // orphan passing through SZOMB on its way to the system reap is not
        // judged here.
        (Lookup::Found { status, ppid, .. }, _) if *status == SZOMB && *ppid == 1 => {
            BarrierReading::Unconfirmed(
                "handed to launchd uncollected: a zombie under launchd with the attach-time \
                 identity"
                    .into(),
            )
        }
        (Lookup::Found { status, .. }, _) if *status == SZOMB => {
            BarrierReading::Confirmed(BarrierEvidence::Zombie)
        }
        (Lookup::Found { status, .. }, _) => {
            BarrierReading::Unconfirmed(format!("p_stat {status}"))
        }
    }
}

/// What proved that a tracee has passed its exit-path job-control accounting
/// (`fixjobc`, `kern_exit.c:2505`). Each is established from the
/// xnu-12377.121.6 source, and nothing else counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarrierEvidence {
    /// A lookup returned the process in SZOMB with its attach-time start
    /// identity. SZOMB is assigned only at `kern_exit.c:2773` and `:2803`,
    /// both later in `proc_exit` than `fixjobc` at `:2505`.
    Zombie,
    /// This process reaped it with `waitpid`. `wait4` reaps only a child in
    /// SZOMB (`kern_exit.c:3100`), so this implies the above.
    ReapedByPlatform,
    /// Its traced parent's intercepted `wait4` returned its pid
    /// (`ReturnKind::Wait`): the kernel reaped it on the parent's behalf,
    /// again only from SZOMB (`kern_exit.c:3100`).
    ReapedByTracedParent,
    /// Implement ruling 1 (done sessions only). The tracer reported the
    /// tracee's exit (`report`, its `W`/`X` packet), and a lookup taken after
    /// that report finds no process at the pid, or a different process.
    /// Admitted by the xnu source proof on [`lookup`]: either way the original
    /// was reaped, which is after SZOMB and so after `fixjobc`. This is the
    /// orphaned child that its own tracer reaped with no handback
    /// (`kern_exit.c:2863-2918`). A post-report lookup that still finds the
    /// tracee, not in SZOMB, is `Unconfirmed`.
    TracerReportedExit {
        report: String,
        lookup: ReportedExitLookup,
        /// Which source-proof branch admitted it (logged with the outcome).
        admitted_by: &'static str,
    },
    /// Review ruling 1 V1: the same proven absence or pid reuse for a session
    /// with no tracer report, e.g. a live orphaned child that its own tracer
    /// reaped with no handback. The proof on [`lookup`] needs no report.
    ReapedProven {
        lookup: ReportedExitLookup,
        admitted_by: &'static str,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum TeardownReply {
    Stop(u8),
    Exit,
    Other,
}
fn teardown_reply(packet: &str, pid: i32) -> std::result::Result<TeardownReply, &'static str> {
    let bytes = packet.as_bytes();
    if !matches!(bytes.first(), Some(b'T' | b'S' | b'W' | b'X')) {
        return Ok(TeardownReply::Other);
    }
    if bytes.len() < 3 || !bytes[1..3].iter().all(u8::is_ascii_hexdigit) {
        return Err("malformed teardown stop/exit signal");
    }
    let signal = u8::from_str_radix(&packet[1..3], 16).map_err(|_| "invalid signal")?;
    if bytes[0] == b'S' && bytes.len() != 3 {
        return Err("malformed S reply");
    }
    if bytes.len() > 3 {
        let fields = packet[3..].trim_start_matches(';');
        if !fields.ends_with(';')
            || fields[..fields.len() - 1].split(';').any(|field| {
                field
                    .split_once(':')
                    .is_none_or(|(key, value)| key.is_empty() || value.is_empty())
            })
        {
            return Err("malformed teardown reply fields");
        }
        for (key, value) in rsp::fields(fields) {
            if key == "process" {
                if u64::from_str_radix(value, 16).ok() != Some(pid as u64) {
                    return Err("wrong teardown process identity");
                }
            } else if key == "thread" {
                let thread = if let Some(identity) = value.strip_prefix('p') {
                    let (process, thread) = identity
                        .split_once('.')
                        .ok_or("malformed teardown thread identity")?;
                    if u64::from_str_radix(process, 16).ok() != Some(pid as u64) {
                        return Err("wrong teardown thread process");
                    }
                    thread
                } else {
                    value
                };
                if u64::from_str_radix(thread, 16)
                    .ok()
                    .filter(|n| *n != 0)
                    .is_none()
                {
                    return Err("malformed teardown thread");
                }
            }
        }
    }
    Ok(if matches!(bytes[0], b'T' | b'S') {
        TeardownReply::Stop(signal)
    } else {
        TeardownReply::Exit
    })
}

/// Ruling-1 branch (b): absence after the report.
const ADMITTED_ABSENT: &str =
    "xnu proof (b): pid leaves the hash only at reap (kern_exit.c:2983), after fixjobc (:2505)";
/// Ruling-1 branch (c): pid reuse after the report.
const ADMITTED_REUSED: &str =
    "xnu proof (c): a pid is reallocated only once out of the hash (kern_fork.c:972)";

/// What the post-report lookup behind [`BarrierEvidence::TracerReportedExit`]
/// found. Both cases are admitted by the source proof on `lookup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportedExitLookup {
    /// `ESRCH`: the pid is no longer in the pid hash.
    Absent,
    /// The pid now belongs to a different process (this start identity).
    PidReused { start: (u64, u64) },
}

/// One barrier evaluation. Anything short of [`BarrierEvidence`] is
/// "not yet confirmed", never "done": a failed lookup, absence without one of
/// the reaps above, an identity mismatch, or a live state (ruling R2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarrierReading {
    Confirmed(BarrierEvidence),
    Unconfirmed(String),
}

/// One step of a session's ordered teardown, in the order it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeardownStep {
    /// The tracer was stopped when its teardown began, and was continued
    /// before `k` (#205 A1(b)). The continue is sent whether or not it was
    /// stopped; this step records only that it was.
    TracerContinued,
    /// The teardown-only interrupt was written on the existing stream.
    InterruptSent,
    /// RSP `k` was written.
    KillSent,
    /// No `k`: the tracer already reported the tracee's exit.
    KillSkipped,
    /// `k` could not be written; straight to the timeout path (ruling R3.4).
    KillSendFailed(String),
    /// A packet the tracer sent during teardown.
    Reply(String),
    /// The connection closed during teardown.
    ConnectionClosed(String),
    /// The barrier's final reading.
    Barrier(BarrierReading),
    /// The termination-unconfirmed warning, written to stderr before the kill.
    Warning(String),
    Retirement(crate::retirement::FinishOutcome),
    EmergencyTracer(crate::retirement::EmergencyFinish),
    RetirementOverrun {
        elapsed: Duration,
        last_observation: Option<crate::retirement::Observation>,
    },
    TracerKilled,
    TracerReaped(bool),
    /// `k` re-sent once after a stop reply (review ruling 1 V2).
    KillResent,
    /// After a timeout and the tracer kill, the post-kill barrier confirmed the
    /// exit (review ruling 1 V3). `terminate` succeeds; the warning stands.
    ConfirmedAfterTracerKill(BarrierEvidence),
    /// The post-kill barrier did not confirm either; `terminate` fails.
    UnconfirmedAfterTracerKill(String),
    /// The root tracee, once it is platform's child again.
    TraceeReaped(bool),
}

/// The diagnostic record of one session's ordered teardown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeardownReport {
    pub pid: i32,
    /// The identity the barrier checked against (`None`: unreadable at attach).
    pub identity: Option<(u64, u64)>,
    pub tracer: i32,
    pub steps: Vec<TeardownStep>,
    /// The barrier did not confirm before the bound, so the timeout path ran:
    /// the warning, then the tracer kill.
    pub timed_out: bool,
    /// The ordered attempt failed, or disposal still lacks collection proof.
    /// Post-kill collection cannot clear an earlier ordered failure. Cleanup
    /// makes terminate fail for this or any retained failed retirement outcome.
    /// Invalid root handles and unsupported termination policies also return Err.
    pub unconfirmed: bool,
    pub elapsed: Duration,
}
impl TeardownReport {
    fn summary(&self) -> String {
        let barrier = self.steps.iter().rev().find_map(|step| match step {
            TeardownStep::Barrier(reading) => Some(reading),
            _ => None,
        });
        format!(
            "pid={} identity={:?} tracer={} barrier={barrier:?} steps={:?}",
            self.pid, self.identity, self.tracer, self.steps
        )
    }
}

/// The teardown's diagnostic path to stderr. It is opt-in, like
/// `UMBRA_RSP_LOG`, so routine runs keep their stderr exact. Timeout warnings
/// are not opt-in: [`Session::ordered_teardown`] always writes them.
fn log_teardown(report: &TeardownReport) {
    #[cfg(feature = "shutdown-witness")]
    umbra_platform::shutdown::witness(serde_json::json!({
        "event": "teardown-report",
        "pid": report.pid,
        "tracer": report.tracer,
        "unconfirmed": report.unconfirmed,
        "steps": format!("{:?}", report.steps),
    }));
    if std::env::var_os("UMBRA_TEARDOWN_LOG").is_some() {
        eprintln!("umbra-platform-macos: teardown {report:?}");
    }
}

/// Whether a teardown ends unconfirmed once the tracer is gone: the one
/// post-kill reading, and for a root (the platform's own child) whether it was
/// collected (#205 A1(a)).
///
/// A root not collected once its tracer is gone was not handed back: that is
/// the shape in which the kernel gives it to launchd, and the post-kill
/// barrier's `Zombie` reading of it was the false `Ok` #205 found.
fn post_kill_unconfirmed(after_kill: Option<&BarrierReading>, root: bool, reaped: bool) -> bool {
    matches!(after_kill, Some(BarrierReading::Unconfirmed(_))) || (root && !reaped)
}

/// Continue a tracer before its teardown (#205 A1(b)). `true` when it was
/// stopped, for the record only: the continue itself is unconditional while
/// the tracer is unreaped.
trait ContinueTracer {
    fn continue_tracer(&mut self) -> bool;
}

/// The teardown's first step: continue the tracer, unconditionally (harmless
/// when it runs; no check-then-act gap), and report `TracerContinued` when it
/// was stopped. A stopped debugserver never answers `k`, and killing it while
/// its tracee is held at a breakpoint hands the tracee to launchd (#205).
fn teardown_prologue(tracer: &mut impl ContinueTracer) -> Option<TeardownStep> {
    tracer
        .continue_tracer()
        .then_some(TeardownStep::TracerContinued)
}

impl ContinueTracer for Rsp {
    fn continue_tracer(&mut self) -> bool {
        // Once reaped, the tracer's pid may already belong to another process;
        // while unreaped, its zombie keeps the pid from being reused.
        if crate::retirement::Tracer::already_reaped(self) {
            return false;
        }
        let pid = self.tracer_pid();
        let stopped = bsd_info(pid).is_some_and(|info| info.pbi_status == libc::SSTOP);
        // SAFETY: plain libSystem call on our own child; SIGCONT ends nothing.
        unsafe {
            libc::kill(pid, libc::SIGCONT);
        }
        stopped
    }
}

impl Session {
    /// Keep the original failure and append one explicit bounded emergency attempt.
    fn finalize_failed_retirement(&mut self) {
        if self.torn_down || self.live.is_none() {
            return;
        }
        let Some(mut retained) = self.retirement_report.clone() else {
            return;
        };
        let attempt = self.ordered_teardown_inner(true);
        retained.steps.extend(attempt.steps);
        retained.unconfirmed |= attempt.unconfirmed;
        retained.timed_out |= attempt.timed_out;
        retained.elapsed += attempt.elapsed;
        log_teardown(&retained);
        eprintln!(
            "umbra-platform-macos: retained retirement failure after emergency cleanup: {}",
            retained.summary()
        );
        self.retirement_report = Some(retained);
    }
    /// Evaluate the barrier once. It never blocks: the `waitpid` is `WNOHANG`.
    fn barrier(&mut self) -> BarrierReading {
        let reading = self.barrier_now();
        if let BarrierReading::Confirmed(evidence) = &reading {
            self.exit_evidence.get_or_insert(evidence.clone());
        }
        reading
    }
    fn barrier_now(&mut self) -> BarrierReading {
        let pid = self.id.0.native_id as i32;
        if let Some(evidence) = &self.exit_evidence {
            return BarrierReading::Confirmed(evidence.clone());
        }
        if self.reaped {
            return BarrierReading::Confirmed(if self.parent.is_none() {
                BarrierEvidence::ReapedByPlatform
            } else {
                BarrierEvidence::ReapedByTracedParent
            });
        }
        // Platform spawned the root, so it is the root's original parent and
        // the root comes back to it once the tracer lets go
        // (`kern_exit.c:2863-2918`). Before that, it is not our child.
        if self.parent.is_none()
            // SAFETY: plain libSystem call on a pid; no memory is passed.
            && wait_owned_terminal(pid, libc::WNOHANG).unwrap_or(false)
        {
            self.reaped = true;
            return BarrierReading::Confirmed(BarrierEvidence::ReapedByPlatform);
        }
        let found = settled_lookup(lookup(pid), || {
            std::thread::sleep(LAUNCHD_ZOMBIE_SETTLE);
            lookup(pid)
        });
        barrier_decision(self.exit_report.as_deref(), self.birth, &found)
    }

    /// Tear this session down in the #186 order: RSP `k`, the barrier, and only
    /// then kill and reap the tracer, and reap the root tracee once it is
    /// platform's child again. Every wait is bounded. On a timeout, the warning
    /// goes to stderr **before** the tracer is killed (ruling R3.2).
    fn ordered_teardown(&mut self) -> TeardownReport {
        self.ordered_teardown_inner(false)
    }
    fn ordered_teardown_inner(&mut self, emergency_retirement: bool) -> TeardownReport {
        self.torn_down = true;
        let started = crate::clock::now();
        let deadline = started + TEARDOWN_BOUND;
        let pid = self.id.0.native_id as i32;
        let tracer = self.rsp.tracer_pid();
        let mut steps = Vec::new();
        // #205 A1(b): before `k`, and before the emergency-retirement path,
        // which comes through here too.
        steps.extend(teardown_prologue(&mut self.rsp));
        // A running peer must observe an interrupt before k is dispatched.
        // 250ms stop + 1250ms kill/confirmation share the original 1500ms cap.
        let stop_deadline = started + Duration::from_millis(250);
        let mut failure = (!self.rsp.begin_teardown())
            .then(|| "ordinary RSP frame incomplete at teardown handover".to_string());
        let mut closed = false;
        let mut exit_seen = self.done;
        let mut interrupt_sent = false;
        let mut extra_stop_drained = false;
        if failure.is_none() && !self.done {
            // A buffered stop answers the outstanding resume, regardless of signal.
            // Keep the incremental decoder and the same socket throughout.
            while crate::clock::now() < stop_deadline {
                match self.rsp.teardown_read_buffered() {
                    rsp::TeardownRead::Quiet => break,
                    rsp::TeardownRead::Closed(why) => {
                        steps.push(TeardownStep::ConnectionClosed(why.clone()));
                        failure = Some(format!("RSP closed before interrupt: {why}"));
                        closed = true;
                        break;
                    }
                    rsp::TeardownRead::Packet(packet) => {
                        let kind = teardown_reply(&packet, pid);
                        steps.push(TeardownStep::Reply(packet));
                        match kind {
                            Ok(TeardownReply::Exit) => {
                                exit_seen = true;
                                break;
                            }
                            Ok(TeardownReply::Stop(_)) if !self.stopped => {
                                self.stopped = true;
                            }
                            Ok(TeardownReply::Stop(_)) => {
                                failure = Some("unsolicited or duplicate stop".into());
                                break;
                            }
                            Ok(_) => {}
                            Err(why) => {
                                failure = Some(why.into());
                                break;
                            }
                        }
                    }
                }
            }
            if failure.is_none() && !exit_seen && !self.stopped {
                match self.rsp.teardown_interrupt(stop_deadline) {
                    Ok(()) => {
                        interrupt_sent = true;
                        steps.push(TeardownStep::InterruptSent);
                    }
                    Err(e) => failure = Some(format!("RSP interrupt failed: {e}")),
                }
                let mut stopped = false;
                while failure.is_none() && crate::clock::now() < stop_deadline {
                    let wait = stop_deadline
                        .saturating_duration_since(crate::clock::now())
                        .min(Duration::from_millis(5));
                    match self.rsp.teardown_read(wait) {
                        rsp::TeardownRead::Quiet => {}
                        rsp::TeardownRead::Closed(why) => {
                            steps.push(TeardownStep::ConnectionClosed(why.clone()));
                            closed = true;
                            failure = Some(format!("RSP closed waiting for stop: {why}"));
                        }
                        rsp::TeardownRead::Packet(packet) => {
                            let kind = teardown_reply(&packet, pid);
                            steps.push(TeardownStep::Reply(packet));
                            match kind {
                                Ok(TeardownReply::Stop(_)) => {
                                    self.stopped = true;
                                    stopped = true;
                                    break;
                                }
                                Ok(TeardownReply::Exit) => {
                                    exit_seen = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(why) => failure = Some(why.into()),
                            }
                        }
                    }
                }
                if !stopped && !exit_seen && failure.is_none() {
                    failure = Some("interrupt stop phase expired".into());
                }
            }
        }
        // A fast stop must not lend its unused allocation to termination.
        let deadline = deadline.min(crate::clock::now() + Duration::from_millis(1250));
        if exit_seen {
            steps.push(TeardownStep::KillSkipped);
        } else if failure.is_none() {
            let bound = deadline
                .saturating_duration_since(crate::clock::now())
                .min(Duration::from_millis(250));
            match self.rsp.send_kill(bound) {
                Ok(()) => steps.push(TeardownStep::KillSent),
                Err(e) => {
                    steps.push(TeardownStep::KillSendFailed(e.to_string()));
                    failure = Some(format!("RSP k failed: {e}"));
                }
            }
        }
        let reading = loop {
            if let Some(why) = &failure {
                break BarrierReading::Unconfirmed(why.clone());
            }
            if !exit_seen && !closed {
                let wait = deadline
                    .saturating_duration_since(crate::clock::now())
                    .min(Duration::from_millis(5));
                match self.rsp.teardown_read(wait) {
                    rsp::TeardownRead::Quiet => {}
                    rsp::TeardownRead::Closed(why) => {
                        steps.push(TeardownStep::ConnectionClosed(why.clone()));
                        failure = Some(format!("RSP closed during kill: {why}"));
                    }
                    rsp::TeardownRead::Packet(packet) => {
                        let kind = teardown_reply(&packet, pid);
                        steps.push(TeardownStep::Reply(packet));
                        match kind {
                            Ok(TeardownReply::Stop(_)) if interrupt_sent && !extra_stop_drained => {
                                // One interrupt race reply is ignored, never a new authorization.
                                extra_stop_drained = true;
                            }
                            Ok(TeardownReply::Stop(_)) => {
                                failure = Some("unsolicited or duplicate stop during kill".into());
                            }
                            Ok(TeardownReply::Exit) => exit_seen = true,
                            Ok(_) => {}
                            Err(why) => failure = Some(why.into()),
                        }
                    }
                }
            }
            if failure.is_some() {
                continue;
            }
            let reading = self.barrier();
            if matches!(reading, BarrierReading::Confirmed(_)) || crate::clock::now() >= deadline {
                break reading;
            }
            if exit_seen {
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        if let BarrierReading::Confirmed(evidence) = &reading {
            eprintln!("umbra-platform-macos: tracee exit confirmed: pid={pid} identity={:?} evidence={evidence:?}", self.birth);
        }
        let timed_out = !matches!(reading, BarrierReading::Confirmed(_));
        steps.push(TeardownStep::Barrier(reading.clone()));
        if timed_out {
            let warning = format!(
                "umbra-platform-macos: tracee termination unconfirmed; tracer killed: \
                 pid={pid} identity={:?} tracer={tracer} barrier={reading:?}",
                self.birth
            );
            eprintln!("{warning}");
            steps.push(TeardownStep::Warning(warning));
        }
        let emergency_limit = crate::clock::now() + rsp::TRACER_REAP_BOUND;
        let emergency = emergency_retirement
            .then(|| crate::retirement::emergency_finish(&mut self.rsp, emergency_limit));
        if let Some(attempt) = &emergency {
            steps.push(TeardownStep::EmergencyTracer(attempt.clone()));
        } else {
            self.rsp.kill_tracer();
            steps.push(TeardownStep::TracerKilled);
        }
        // Review ruling 1 V3: after a timeout, ONE post-kill barrier, run inside
        // the tracer-reap window (so the per-session total stays at most 2 s).
        // Measured: a released tracee is a zombie within 5 ms of its tracer's
        // death. The same positives apply (SZOMB with identity under any
        // parent, or proven absence or reuse).
        let limit = if emergency_retirement {
            emergency_limit
        } else {
            crate::clock::now() + rsp::TRACER_REAP_BOUND
        };
        let mut tracer_reaped = emergency.as_ref().is_some_and(|attempt| {
            attempt.operations.iter().any(|operation| {
                matches!(
                    operation,
                    crate::retirement::TracerOperation::Wait(Ok(Some(_)))
                )
            })
        });
        let mut after_kill: Option<BarrierReading> = None;
        loop {
            if !tracer_reaped && !emergency_retirement {
                tracer_reaped = self.rsp.reap_tracer(Duration::ZERO);
            }
            if timed_out && !matches!(after_kill, Some(BarrierReading::Confirmed(_))) {
                after_kill = Some(self.barrier());
            }
            let settled = !timed_out || matches!(after_kill, Some(BarrierReading::Confirmed(_)));
            if (tracer_reaped && settled) || crate::clock::now() >= limit {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        steps.push(TeardownStep::TracerReaped(tracer_reaped));
        self.rsp.mark_torn_down();
        let post_kill = after_kill.clone();
        match after_kill {
            Some(BarrierReading::Confirmed(evidence)) => {
                eprintln!(
                    "umbra-platform-macos: tracee exit confirmed after tracer kill: \
                     pid={pid} identity={:?} evidence={evidence:?}",
                    self.birth
                );
                steps.push(TeardownStep::ConfirmedAfterTracerKill(evidence));
            }
            Some(BarrierReading::Unconfirmed(why)) => {
                steps.push(TeardownStep::UnconfirmedAfterTracerKill(why));
            }
            None => {}
        }
        if self.parent.is_none() {
            let limit = crate::clock::now() + TRACEE_REAP_BOUND;
            while !self.reaped {
                // SAFETY: plain libSystem call on a pid; no memory is passed.
                if wait_owned_terminal(pid, libc::WNOHANG).unwrap_or(false) {
                    self.reaped = true;
                } else if crate::clock::now() >= limit {
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            steps.push(TeardownStep::TraceeReaped(self.reaped));
        }
        let unconfirmed = timed_out
            || post_kill_unconfirmed(post_kill.as_ref(), self.parent.is_none(), self.reaped);
        TeardownReport {
            pid,
            identity: self.birth,
            tracer,
            steps,
            timed_out,
            unconfirmed,
            elapsed: started.elapsed(),
        }
    }
}

impl MacosTraceBackend {
    /// Service exited children without turning cleanup failure into a session error.
    fn service_retirements(&mut self) {
        let now = crate::clock::now();
        for s in &mut self.sessions {
            if s.retirement_report.is_some() || s.retirement.is_none() {
                continue;
            }
            let needs_barrier = s.retirement.as_ref().unwrap().needs_barrier();
            let barrier = if needs_barrier {
                match s.barrier() {
                    BarrierReading::Confirmed(_) => Ok(()),
                    BarrierReading::Unconfirmed(why) => Err(why),
                }
            } else {
                Ok(())
            };
            let retirement = s.retirement.as_mut().unwrap();
            let live = s.live.as_mut().expect("pending retirement owns resources");
            retirement.service(now, barrier, &mut live.rsp);
            let Some(outcome) = retirement.outcome().cloned() else {
                continue;
            };
            let success = outcome.success();
            let reading = s
                .exit_evidence
                .clone()
                .map(BarrierReading::Confirmed)
                .unwrap_or_else(|| {
                    BarrierReading::Unconfirmed("retirement barrier unconfirmed".into())
                });
            let mut report = TeardownReport {
                pid: s.id.0.native_id as i32,
                identity: s.birth,
                tracer: live.rsp.tracer_pid(),
                steps: vec![
                    TeardownStep::KillSkipped,
                    TeardownStep::Barrier(reading),
                    TeardownStep::Retirement(outcome.clone()),
                    TeardownStep::TracerReaped(success),
                ],
                timed_out: !success,
                unconfirmed: s.exit_evidence.is_none(),
                elapsed: retirement.elapsed(now),
            };
            if let Some(elapsed) = retirement.overrun {
                report.steps.push(TeardownStep::RetirementOverrun {
                    elapsed,
                    last_observation: retirement.last_observation.clone(),
                });
            }
            if success {
                live.rsp.mark_torn_down();
                let removed = self
                    .watchdog
                    .as_ref()
                    .and_then(|watchdog| watchdog.tasks.lock().unwrap().retire(&live.task));
                drop(removed);
                s.torn_down = true;
                s.live.take();
                #[cfg(feature = "retirement-witness")]
                eprintln!(
                    "retirement-ownership pid={} remaining={:?}",
                    s.id.0.native_id,
                    crate::ownership::remaining(s.id.0.native_id)
                );
            } else {
                eprintln!("umbra-platform-macos: child retirement failed: pid={} tracer={} outcome={outcome:?}", report.pid, report.tracer);
            }
            log_teardown(&report);
            self.teardown_reports.push(report.clone());
            s.retirement_report = Some(report);
        }
    }
    fn drain_retirements(&mut self) {
        let started = crate::clock::now();
        // A final reap window is independent of (and does not rewrite) each
        // obligation's original deadline. KilledReapPending only try_waits.
        let limit = self
            .sessions
            .iter()
            .filter_map(|s| s.retirement.as_ref())
            .filter(|r| r.outcome().is_none())
            .map(|r| r.deadline())
            .max()
            .unwrap_or(started)
            .max(started + rsp::TRACER_REAP_BOUND);
        loop {
            self.service_retirements();
            if self
                .sessions
                .iter()
                .all(|s| s.retirement.as_ref().is_none_or(|r| r.outcome().is_some()))
            {
                break;
            }
            if crate::clock::now() >= limit {
                for s in &mut self.sessions {
                    if let Some(r) = &mut s.retirement {
                        r.finish_drain(crate::clock::now());
                    }
                }
                self.service_retirements();
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// End every fork child the platform has not enrolled yet (#205), before
    /// any root is ended. A fork entry opens a return window: it writes a
    /// branch-to-self at the return gate, reserves the child's ledger slot and
    /// resumes the parent. The child spins at that gate, running and untraced,
    /// until the parent's fork return is processed and enrolls it. A teardown
    /// that comes first, such as another session's refused spawn, would leave
    /// it running after umbra and its slot reserved. Each open window's child
    /// is found by parentage while its parent still lives (after one short
    /// settle shared by every parent, for a fork still in the kernel), then
    /// every child is signalled ([`signal_unenrolled_child`]) and all of them
    /// are awaited by one shared wait ([`await_unenrolled_ends`]), so the
    /// sweep's worst case is one settle plus one wait, which
    /// [`shutdown_allowance`] budgets. A window whose child cannot be found
    /// names itself as unconfirmed, never as nothing.
    #[must_use = "an unconfirmed fork child is a cleanup failure the caller must report"]
    fn end_unenrolled_fork_children(&mut self) -> Vec<String> {
        let enrolled: Vec<SessionIdentity> = self
            .sessions
            .iter()
            .map(|s| (s.id.0.native_id as i32, s.birth, s.live.is_some()))
            .collect();
        // Every session with an open window. A retired session has no live
        // resources, so no open window.
        let windows: Vec<(usize, i32, Vec<ThreadId>)> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, s)| {
                let open = open_fork_windows(s.live.as_deref()?);
                (!open.is_empty()).then_some((index, s.id.0.native_id as i32, open))
            })
            .collect();
        let mut unconfirmed = Vec::new();
        if windows.is_empty() {
            return unconfirmed;
        }
        // One settle, shared by every parent, for forks still in the kernel.
        // Each candidate with the birth the census read (`None` if unread).
        let unenrolled = |parent: i32| -> Vec<ForkCandidate> {
            live_children(parent)
                .into_iter()
                .map(|pid| {
                    let birth =
                        bsd_info(pid).map(|info| (info.pbi_start_tvsec, info.pbi_start_tvusec));
                    (pid, birth)
                })
                .filter(|&(pid, birth)| !enrolled_identity(&enrolled, pid, birth))
                .collect()
        };
        let settle = crate::clock::now() + UNENROLLED_FORK_SETTLE;
        let found = loop {
            let found: Vec<Vec<ForkCandidate>> = windows
                .iter()
                .map(|(_, parent, _)| unenrolled(*parent))
                .collect();
            let complete = found
                .iter()
                .zip(&windows)
                .all(|(children, (_, _, open))| children.len() >= open.len());
            if complete || crate::clock::now() >= settle {
                break found;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        // Signal every child, keeping each reservation until it is settled.
        let mut signalled = Vec::new();
        for ((index, parent, open), children) in windows.into_iter().zip(found) {
            let mut children = children.into_iter();
            for thread in open {
                let Some(owner) = self.sessions[index]
                    .live
                    .as_deref_mut()
                    .and_then(|live| live.pending.get_mut(&thread))
                    .and_then(|pending| pending.creation.take())
                else {
                    continue;
                };
                match children.next() {
                    Some((pid, census_birth)) => {
                        match signal_unenrolled_child(parent, pid, census_birth, &owner.tracee) {
                            Ok(birth) => signalled.push((parent, pid, birth, owner)),
                            Err(diagnostic) => unconfirmed.push(diagnostic),
                        }
                    }
                    None => unconfirmed.push(unenrolled_unconfirmed(
                        &owner.tracee,
                        format!(
                            "fork child of pid={parent}: none found while its fork window was open \
                             (reserved, no identity)"
                        ),
                    )),
                }
            }
        }
        if signalled.is_empty() {
            return unconfirmed;
        }
        // One wait, shared by every signalled child.
        let identities: Vec<(i32, (u64, u64))> = signalled
            .iter()
            .map(|(_, pid, birth, _)| (*pid, *birth))
            .collect();
        let ends = await_unenrolled_ends(
            &identities,
            crate::clock::now() + UNENROLLED_FORK_BOUND,
            lookup,
            crate::clock::now,
            || std::thread::sleep(Duration::from_millis(2)),
        );
        for ((parent, pid, birth, owner), end) in signalled.into_iter().zip(ends) {
            match end {
                Ok(state) => {
                    eprintln!(
                        "umbra-platform-macos: unenrolled fork child ended: pid={pid} of pid={parent} birth={}.{:06} ({state})",
                        birth.0, birth.1
                    );
                    let _ = owner
                        .tracee
                        .confirm(umbra_core::shutdown::Proof::Accounted {
                            evidence: format!("unenrolled fork child SIGKILLed: {state}"),
                            root_owned: false,
                            root_collected: false,
                        });
                }
                Err(last) => {
                    let state = match &last {
                        Lookup::Error(error) => format!("unreadable ({error})"),
                        _ => "still live".into(),
                    };
                    unconfirmed.push(unenrolled_unconfirmed(
                        &owner.tracee,
                        format!(
                            "unenrolled fork child pid={pid} of pid={parent}: {state} {} ms after SIGKILL",
                            UNENROLLED_FORK_BOUND.as_millis()
                        ),
                    ));
                }
            }
        }
        unconfirmed
    }
    /// Run [`Session::ordered_teardown`] for every session not yet torn down,
    /// children before parents (the reverse of creation order), so a child's
    /// zombie still has its original parent to be handed back to. It does not
    /// remove the sessions; callers clear them afterwards.
    fn tear_down_sessions(&mut self) -> Vec<TeardownReport> {
        // Paths without a result of their own settle an unconfirmed fork
        // child through its reservation; `terminate` runs the sweep first and
        // reports it.
        let _ = self.end_unenrolled_fork_children();
        self.drain_retirements();
        #[cfg(feature = "mutation-probe-spawn-lifecycle")]
        if std::env::var_os("UMBRA_FIX_243_OWNER_TRACE").is_some() {
            let owners: Vec<_> = self
                .sessions
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "pid": s.id.0.native_id, "birth": s.birth, "stopped": s.stopped,
                    })
                })
                .collect();
            eprintln!("spawn-teardown-owners {}", serde_json::json!(owners));
        }
        let mut reports = Vec::new();
        for index in (0..self.sessions.len()).rev() {
            if let Some(report) = self.sessions[index].retirement_report.clone() {
                reports.push(report);
                // Keep unresolved owners for the backend's emergency Drop;
                // normal terminate must not hide a failed wait in legacy cleanup.
                continue;
            }
            if !self.sessions[index].torn_down {
                let report = self.sessions[index].ordered_teardown();
                log_teardown(&report);
                reports.push(report);
            }
        }
        for report in &reports {
            if !self
                .teardown_reports
                .iter()
                .any(|old| old.pid == report.pid && old.identity == report.identity)
            {
                self.teardown_reports.push(report.clone());
            }
        }
        reports
    }
    fn finalize_failed_retirements(&mut self) {
        for session in self
            .sessions
            .iter_mut()
            .rev()
            .chain(self.failed_retirements.iter_mut().rev())
        {
            session.finalize_failed_retirement();
            if let Some(report) = &session.retirement_report {
                if let Some(stored) = self
                    .teardown_reports
                    .iter_mut()
                    .find(|stored| stored.pid == report.pid && stored.identity == report.identity)
                {
                    *stored = report.clone();
                }
            }
        }
    }
    /// The tracer (debugserver) pid of each live session. Read-only test seam
    /// for the timeout-path acceptance tests.
    #[doc(hidden)]
    pub fn tracer_pids(&self) -> Vec<i32> {
        self.sessions
            .iter()
            .filter_map(|s| s.live.as_ref().map(|live| live.rsp.tracer_pid()))
            .collect()
    }
    /// Every ordered-teardown report so far. Read-only test seam and the
    /// teardown's in-process diagnostic record.
    #[doc(hidden)]
    pub fn teardown_reports(&self) -> &[TeardownReport] {
        &self.teardown_reports
    }
}

impl Drop for MacosTraceBackend {
    /// A dropped backend tears its sessions down in the same order `terminate`
    /// does, rather than leaving that to the `Vec`'s front-to-back drop.
    fn drop(&mut self) {
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(serde_json::json!({"event": "owner-drop"}));
        // #205: an unenrolled fork child is found by parentage, so before any
        // root is ended. A drop has no result: an unconfirmed child is settled
        // through its reservation (and named on stderr).
        let _ = self.end_unenrolled_fork_children();
        if let Some(watchdog) = &self.watchdog {
            watchdog.terminate_registered();
        }
        for s in self.sessions.iter().rev() {
            if !s.done {
                record_task_terminate(s.id.0.native_id as i32, s.task.kill());
            }
        }
        self.tear_down_sessions();
        self.finalize_failed_retirements();
        #[cfg(test)]
        retirement_controls::capture_drop(&self.teardown_reports);
    }
}

impl TraceBackend for MacosTraceBackend {
    fn adopt_stdio(&mut self, stdio: umbra_core::launch_stdio::LaunchStdio) -> Result<()> {
        if self.launched || self.stdio_adopted {
            return Err(unsupported("stdio was already adopted or launch attempted"));
        }
        self.stdio_adopted = true;
        self.launch_stdio = Some(stdio);
        Ok(())
    }
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        if matches!(spec.sandbox, SandboxRequirement::UnsandboxedExperiment) {
            self.consume_launch_stdio()?;
            return Err(unsupported(
                "supervised launch requires an installed sandbox",
            ));
        }
        self.launch_traced(spec)
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        loop {
            if let umbra_platform::EventPoll::Event(event) =
                self.poll_event(Duration::from_millis(100))?
            {
                return Ok(event);
            }
        }
    }
    fn poll_event(&mut self, max_wait: Duration) -> Result<umbra_platform::EventPoll> {
        if max_wait.is_zero() || max_wait > Duration::from_millis(100) {
            return Err(error("poll_event", "invalid wait budget"));
        }
        let limit = crate::clock::now()
            .checked_add(max_wait)
            .ok_or_else(|| error("poll_event", "wait overflow"))?;
        loop {
            self.check_deadline()?;
            self.service_retirements();
            let hold_root = self.sessions.iter().any(|s| {
                s.retirement
                    .as_ref()
                    .is_some_and(|r| r.hold_root(crate::clock::now()))
            });
            let root = self.sessions.first().map(|s| s.id);
            let deliver = self.events.iter().position(|event| {
                !hold_root || !matches!(event, TraceEvent::Exit { task, .. } if Some(*task) == root)
            });
            if let Some(index) = deliver {
                return Ok(umbra_platform::EventPoll::Event(
                    self.events.remove(index).unwrap(),
                ));
            }
            if let Some((index, reply)) = self.quiesced_stops.pop_front() {
                self.stop(index, reply, true)?;
                continue;
            }
            let pending_cleanup = self
                .sessions
                .iter()
                .any(|s| s.retirement.as_ref().is_some_and(|r| r.outcome().is_none()));
            if !pending_cleanup
                && (self.sessions.is_empty() || self.sessions.iter().all(|s| s.done))
            {
                return Err(error("next_event", "no live processes"));
            }
            if crate::clock::now() >= limit {
                return Ok(umbra_platform::EventPoll::Idle);
            }
            let start = crate::clock::now();
            let count = self.sessions.len();
            for _ in 0..count {
                if crate::clock::now() >= limit {
                    break;
                }
                let i = self.poll_cursor % self.sessions.len();
                self.poll_cursor = (i + 1) % self.sessions.len();
                if self.sessions[i].done {
                    continue;
                }
                if !self.sessions[i].waiting.is_empty() {
                    if self.sessions[i]
                        .waiting
                        .iter()
                        .any(|j| self.sessions[*j].done)
                    {
                        self.sessions[i].waiting.clear();
                        // `WaitPlan::Park` is gated by `single_thread()`, so the
                        // parked thread is the only thread this session has and
                        // the slot still names it.
                        let parked = self.sessions[i].thread;
                        self.sessions[i].return_stop(parked, ReturnKind::Wait)?;
                    }
                    continue;
                }
                if self.sessions[i].stopped {
                    continue;
                }
                #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                if self
                    .fork_window_hold
                    .is_some_and(|(held, until)| held == i && crate::clock::now() < until)
                {
                    continue;
                }
                if let Some(reply) = self.sessions[i].rsp.poll(
                    Duration::from_millis(2)
                        .min(limit.saturating_duration_since(crate::clock::now())),
                )? {
                    self.stop(i, reply, false)?;
                }
                if !self.events.is_empty() {
                    break;
                }
            }
            // A parked/stopped tree must not busy-spin between idle replies.
            if self.events.is_empty() && crate::clock::now() < limit {
                let pause = Duration::from_millis(1)
                    .saturating_sub(start.elapsed())
                    .min(limit.saturating_duration_since(crate::clock::now()));
                if !pause.is_zero() {
                    std::thread::sleep(pause);
                }
            }
        }
    }
    fn read_memory(&mut self, task: TaskId, address: u64, out: &mut [u8]) -> Result<()> {
        let i = self.task_index(task)?;
        if !self.sessions[i].stopped {
            return Err(error("read_memory", "task running"));
        }
        let mut bytes = vec![0; out.len()];
        self.sessions[i].task.read(address, &mut bytes)?;
        out.copy_from_slice(&bytes);
        Ok(())
    }
    fn write_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()> {
        let i = self.task_index(task)?;
        self.sessions[i].write(address, bytes)
    }
    fn copyout_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()> {
        let i = self.task_index(task)?;
        let session = &self.sessions[i];
        if !session.stopped || session.done || bytes.len() > MAX_IO_BYTES {
            return Err(error("copyout", "invalid task state or byte count"));
        }
        if address == 0 || address.checked_add(bytes.len() as u64).is_none() {
            return Err(
                error("copyout", "invalid output address").with_errno(umbra_core::Errno(14))
            );
        }
        // SAFETY: bytes owns the source buffer; the task send right is live.
        // Mach validates the destination and its protection. Unlike debugger
        // writes, this must not make a read-only page writable.
        let result = unsafe {
            mach2::vm::mach_vm_write(
                session.task.0,
                address,
                bytes.as_ptr() as _,
                bytes.len() as _,
            )
        };
        if result == mach2::kern_return::KERN_INVALID_ADDRESS
            || result == mach2::kern_return::KERN_PROTECTION_FAILURE
        {
            return Err(error("copyout", "inaccessible output").with_errno(umbra_core::Errno(14)));
        }
        mach(result, "copyout")
    }
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet> {
        let i = self.thread_index(thread)?;
        self.sessions[i].regs(thread)
    }
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()> {
        let i = self.thread_index(thread)?;
        self.sessions[i].set_regs(thread, regs)
    }
    fn resume(&mut self, command: ResumeCommand) -> Result<()> {
        self.ledger.admit()?;
        self.service_retirements();
        self.check_deadline()?;
        let i = self.thread_index(command.thread)?;
        if command.signal.is_none() && self.sessions[i].stopped {
            match command.mode {
                ResumeMode::SpawnAuthorize(auth) if auth.token.thread == command.thread => {
                    return self.arm_spawn(i, auth);
                }
                ResumeMode::SpawnChildReady(auth) if auth.token.thread == command.thread => {
                    #[cfg(feature = "mutation-probe-spawn-lifecycle")]
                    if std::env::var("UMBRA_FIX_243_PROBE_PHASE").as_deref() == Ok("policy-ready")
                        && matches!(
                            std::env::var("UMBRA_FIX_243_PROBE_TRIGGER").as_deref(),
                            Ok("deadline" | "session-drop" | "backend-drop" | "terminate")
                        )
                    {
                        return self.probe_teardown_trigger("policy-ready");
                    }
                    return self.complete_spawn(i, auth, true);
                }
                ResumeMode::SpawnAbortReady(auth) if auth.token.thread == command.thread => {
                    return self.complete_spawn(i, auth, false);
                }
                _ => {}
            }
        }
        if self.sessions[i].spawn.is_some()
            || matches!(
                command.mode,
                ResumeMode::SpawnAuthorize(_)
                    | ResumeMode::SpawnChildReady(_)
                    | ResumeMode::SpawnAbortReady(_)
            )
        {
            return Err(error("spawn", "resume before commit barrier"));
        }
        if self.quiesced_stops.iter().any(|(index, _)| *index == i) {
            return Err(error(
                "resume",
                "consume quiesced stop with next_event first",
            ));
        }
        let s = &mut self.sessions[i];
        if s.signals.closed() {
            return Err(unsupported(
                "signal-delivery-closed: no resume after ambiguous send or teardown",
            ));
        }
        if !s.stopped || !s.waiting.is_empty() {
            return Err(error("resume", "not externally stopped"));
        }
        if let ResumeMode::Signal(token) = command.mode {
            if s.torn_down
                || s.done
                || !deliverable_signal(token.signal)
                || !s.pending.is_empty()
                || !s.entry.is_empty()
                || s.spawn.is_some()
            {
                return Err(unsupported("signal-stop-identity: unsafe delivery state"));
            }
            let live = s
                .live
                .as_mut()
                .ok_or_else(|| unsupported("signal-stop-identity: retired task"))?;
            s.signals.send(
                token,
                s.id,
                command.thread,
                s.exec_generation,
                command.signal,
                |packet| {
                    // Validation has succeeded; even an error can mean delivery.
                    s.stopped = false;
                    live.rsp.send(packet)
                },
            )?;
            return Ok(());
        }
        if command.signal.is_some() || s.signals.pending() {
            return Err(unsupported(
                "signal-stop-identity: missing delivery authority",
            ));
        }
        if command.mode == ResumeMode::SingleStep {
            return Err(unsupported("external single stepping"));
        }
        if let Some(pc) = s.entry.remove(&command.thread) {
            let regs = s.regs(command.thread)?;
            if get(&regs, PC)? == pc {
                s.return_stop(command.thread, ReturnKind::Syscall)
            } else {
                // Attributed to the thread the caller is answering for, not to
                // whoever stopped last, for the reason [`Pending::thread`]
                // records: the caller keys its in-flight operation by this
                // thread and a miss on it abandons the rewrite in silence.
                self.events.push_back(TraceEvent::SyscallExit {
                    task: s.id,
                    thread: command.thread,
                    outcome: abi::outcome(&regs)?,
                });
                Ok(())
            }
        } else {
            s.initial = false;
            s.continue_run()
        }
    }
}
impl TraceControl for MacosTraceBackend {
    fn begin_shutdown(
        &mut self,
        nonce: umbra_core::shutdown::Nonce,
    ) -> Result<umbra_platform::shutdown::Start> {
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(serde_json::json!({"event": "owner-begin"}));
        for session in &mut self.sessions {
            session.signals.close();
        }
        // Serial dispatch leaves no setup operation in flight. Existing retirement
        // deadlines are carried forward; no poll renews an obligation's window.
        let now = crate::clock::now();
        let retirement = self
            .sessions
            .iter()
            .filter_map(|s| s.retirement.as_ref())
            .filter(|r| r.outcome().is_none())
            .map(|r| r.deadline().saturating_duration_since(now))
            .max()
            .unwrap_or_default()
            .max(rsp::TRACER_REAP_BOUND);
        let live_sessions = self
            .sessions
            .iter()
            .chain(self.failed_retirements.iter())
            .filter(|session| session.live.is_some() && !session.torn_down)
            .count();
        let fork_windows = self
            .sessions
            .iter()
            .filter_map(|session| session.live.as_deref())
            .map(|live| open_fork_windows(live).len())
            .sum();
        let remaining = shutdown_allowance(retirement, live_sessions, fork_windows)?;
        let accepted = self.ledger.freeze(nonce, remaining)?;
        Ok(umbra_platform::shutdown::Start::Local {
            accepted,
            ledger: self.ledger.clone(),
        })
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            architectures: vec![Architecture::Aarch64],
            // Both are implemented and measured on this host: scratch-backed
            // open/openat rewriting by the direct fixture tests, and the traced
            // installer handoff by tests/sandbox_launch.rs, which also proves an
            // unrewritten write outside the run root is denied by the kernel.
            capabilities: [
                umbra_core::shutdown::CAPABILITY.to_owned(),
                umbra_core::launch_stdio::CAPABILITY.to_owned(),
                umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.to_owned(),
                umbra_platform::dirents::DARWIN_ARM64_ABI.to_owned(),
                umbra_core::capabilities::PLATFORM_SANDBOXED_LAUNCH_V1.to_owned(),
                umbra_core::capabilities::PLATFORM_SYSCALL_REWRITE_V1.to_owned(),
                // The three halves the name asserts are all present in this
                // build, and none of them is optional at run time: the interposer
                // is compiled from source by `build.rs` and embedded, so it
                // cannot be absent; `launch_traced` refuses an interposed launch
                // without a descriptor limit; and `install` refuses one whose
                // interposer image it cannot find to breakpoint.
                umbra_core::capabilities::PLATFORM_INTERPOSE_V1.to_owned(),
            ]
            .into_iter()
            .collect(),
        }
    }
    fn prepare_rewrite(
        &mut self,
        thread: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite> {
        MacosTraceBackend::prepare_rewrite(self, thread, path, operation)
    }
    fn quiesce(&mut self, process: ProcessHandle) -> Result<QuiescedTree> {
        self.service_retirements();
        let result = (|| {
            if self.sessions.first().is_none_or(|s| s.id != process.0) {
                return Err(UmbraError::new(
                    ErrorKind::StaleHandle,
                    "quiesce",
                    "unknown root",
                ));
            }
            self.check_deadline()?;
            let quiesce_limit = operation_deadline(self.deadline, Duration::from_secs(5));
            let result = (|| {
                // Interrupt all live connections before waiting on any one of them.
                for s in &mut self.sessions {
                    if !s.done && !s.stopped {
                        s.rsp.interrupt_until(quiesce_limit)?;
                    }
                }
                for i in 0..self.sessions.len() {
                    while !self.sessions[i].done && !self.sessions[i].stopped {
                        self.check_deadline()?;
                        if crate::clock::now() >= quiesce_limit {
                            return Err(error("quiesce", "operation deadline expired"));
                        }
                        let Some(reply) = self.sessions[i].rsp.poll(Duration::from_millis(10))?
                        else {
                            continue;
                        };
                        if reply.starts_with('O') {
                            continue;
                        }
                        if reply.starts_with('W') || reply.starts_with('X') {
                            self.stop(i, reply, false)?;
                            continue;
                        }
                        if !reply.starts_with('T') || reply.len() < 3 {
                            return Err(error(
                                "quiesce",
                                format!("expected stopped acknowledgement: {reply}"),
                            ));
                        }
                        let fields = rsp::fields(&reply[3..]);
                        let thread = rsp::number(
                            fields
                                .get("thread")
                                .ok_or_else(|| error("quiesce", "no stopped thread"))?,
                        )?;
                        let s = &mut self.sessions[i];
                        s.thread = tid(thread, s.id.0.generation);
                        s.stopped = true;
                        // Do not dispatch here: process-control stops can resume or attach
                        // children. Preserve the reply so no syscall trap is skipped.
                        self.quiesced_stops.push_back((i, reply));
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                // A failed interrupt/ack must not leave part of the tree running.
                for s in &self.sessions {
                    if let Some(live) = &s.live {
                        let _ = live.task.kill();
                    }
                }
                return Err(e);
            }
            if self
                .sessions
                .iter()
                .any(|s| !s.done && !s.pending.is_empty())
            {
                return Err(error(
                    "quiesce",
                    "syscall return in flight; drain next_event before retrying",
                ));
            }
            Ok(QuiescedTree {
                process,
                tasks: self
                    .sessions
                    .iter()
                    .filter(|s| !s.done)
                    .map(|s| s.id)
                    .collect(),
            })
        })();
        self.service_retirements();
        result
    }
    fn terminate(&mut self, process: ProcessHandle, policy: TerminationPolicy) -> Result<()> {
        if self.sessions.first().is_none_or(|s| s.id != process.0) {
            return Err(error("terminate", "unknown root"));
        }
        if !matches!(policy, TerminationPolicy::Immediate) {
            return Err(unsupported("graceful tree termination"));
        }
        // #205: an unenrolled fork child is found by parentage, so before any
        // root is ended. One it cannot confirm ended fails the termination.
        let unenrolled = self.end_unenrolled_fork_children();
        if let Some(watchdog) = &self.watchdog {
            watchdog.terminate_registered();
        }
        for s in self.sessions.iter().rev() {
            if !s.done {
                record_task_terminate(s.id.0.native_id as i32, s.task.kill());
            }
        }
        let reports = self.tear_down_sessions();
        for session in self.sessions.drain(..) {
            if session.retirement_report.as_ref().is_some_and(|report| {
                report.steps.iter().any(
                    |step| matches!(step, TeardownStep::Retirement(outcome) if !outcome.success()),
                )
            }) {
                self.failed_retirements.push(session);
            }
        }
        self.events.clear();
        self.quiesced_stops.clear();
        self.watchdog.take();
        // Preserve both unconfirmed tracee termination and failed retirement
        // outcomes, even when a later emergency attempt may reap the tracer.
        let unconfirmed: Vec<String> = reports
            .iter()
            .filter(|r| r.unconfirmed || r.steps.iter().any(|step| matches!(step, TeardownStep::Retirement(outcome) if !outcome.success())))
            .map(TeardownReport::summary)
            .collect();
        termination_result(unconfirmed, unenrolled)
    }
}

/// `terminate`'s result (#205): `Ok` only when nothing is left unconfirmed:
/// no session report (tracee or retired tracer), and no unenrolled fork child
/// the sweep could not confirm ended. Callers treat `Ok` as confirmed
/// termination (the provider forgets the root; `terminate_tree` reports it).
fn termination_result(sessions: Vec<String>, unenrolled: Vec<String>) -> Result<()> {
    if sessions.is_empty() && unenrolled.is_empty() {
        return Ok(());
    }
    let failures: Vec<String> = sessions.into_iter().chain(unenrolled).collect();
    Err(error(
        "terminate",
        format!(
            "tree cleanup failed (tracee, retired tracer or unenrolled fork child): {}",
            failures.join("; ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn f247_qualification_cache_is_provider_scoped() {
        use super::*;
        use std::cell::Cell;
        let observations = Cell::new(0);
        for outcome in [Some(&spawn::CURRENT), None] {
            let mut provider = SpawnQualification::default();
            let before = observations.get();
            for _tracee_exec_generation in 0..3 {
                let actual = provider.get(|| {
                    observations.set(observations.get() + 1);
                    outcome
                });
                assert_eq!(
                    actual.map(|layout| layout.id),
                    outcome.map(|layout| layout.id)
                );
            }
            assert_eq!(observations.get(), before + 1);
            // A later available observation cannot upgrade a refused provider.
            assert_eq!(
                provider
                    .get(|| Some(&spawn::LEGACY))
                    .map(|layout| layout.id),
                outcome.map(|layout| layout.id)
            );
        }
        let mut restarted = SpawnQualification::default();
        assert_eq!(
            restarted
                .get(|| {
                    observations.set(observations.get() + 1);
                    Some(&spawn::CURRENT)
                })
                .unwrap()
                .id,
            spawn::CURRENT.id
        );
        assert_eq!(observations.get(), 3);
    }

    fn spawn_barrier_fixture() -> (super::SpawnBarrier, umbra_core::SpawnAuthorization) {
        use super::*;
        let parent = TaskId(TaskIdentity {
            native_id: 10,
            generation: 2,
        });
        let token = SpawnToken {
            run_generation: 2,
            parent,
            exec_generation: 3,
            thread: tid(11, 2),
            sequence: 4,
        };
        (
            SpawnBarrier {
                token,
                digest: [5; 32],
                authorization: None,
                returned: false,
                child: false,
            },
            SpawnAuthorization {
                token,
                digest: [5; 32],
                table_generation: 6,
            },
        )
    }
    #[test]
    fn f247_child_has_no_inherited_pending_snapshot() {
        use super::*;
        let pid = 247;
        let task = Arc::new(Task(0, None));
        let make = || {
            LiveSession::attached(
                pid,
                task.clone(),
                Rsp::retirement_test(),
                PathBuf::new(),
                vec![],
                None,
            )
        };
        let (barrier, auth) = spawn_barrier_fixture();
        let snapshot = spawn::decode(
            &spawn::CURRENT,
            &[0; spawn::DESCRIPTOR_BYTES],
            |_, _| unreachable!(),
        )
        .unwrap();
        let mut parent = make();
        parent.spawn = Some(SpawnInFlight {
            token: barrier.token,
            snapshot,
            plan: SpawnPlan {
                digest: auth.digest,
                actions: vec![],
                close_on_exec_default: false,
            },
            authorization: Some(auth),
            original: RegisterSet::new(Architecture::Aarch64, vec![0; 272]).unwrap(),
            returned: None,
            caller_pid: pid as u64,
            region: None,
            twin: PathBuf::new(),
            child: None,
        });
        let before = parent.spawn.as_ref().unwrap() as *const SpawnInFlight;
        let child = make();
        for fixture in [&parent, &child] {
            let pid = fixture.rsp.tracer_pid();
            let mut status = 0;
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            let wait_error = std::io::Error::last_os_error().raw_os_error();
            assert_eq!(waited, -1, "fixture child {pid} was not already waited for");
            assert_eq!(wait_error, Some(libc::ECHILD));
            let exists = unsafe { libc::kill(pid, 0) };
            let existence_error = std::io::Error::last_os_error().raw_os_error();
            assert_eq!(exists, -1, "fixture child {pid} survives");
            assert_eq!(existence_error, Some(libc::ESRCH));
        }
        assert!(child.spawn.is_none());
        let retained = parent.spawn.as_ref().unwrap();
        assert_eq!(before, retained as *const SpawnInFlight);
        assert_eq!(retained.authorization, Some(auth));
        assert!(std::ptr::eq(retained.snapshot.layout(), &spawn::CURRENT));
    }

    #[test]
    fn f247_stale_and_repeated_authorization() {
        let (mut barrier, auth) = spawn_barrier_fixture();
        let parent = auth.token.parent;
        assert!(barrier.authorize(parent, 3, auth).is_ok());
        for field in 0..7 {
            let mut bad = auth;
            match field {
                0 => bad.digest[0] ^= 1,
                1 => bad.token.run_generation += 1,
                2 => bad.token.parent.0.native_id += 1,
                3 => bad.token.exec_generation += 1,
                4 => bad.token.thread = super::tid(99, 2),
                5 => bad.token.sequence += 1,
                _ => bad.token.parent.0.generation += 1,
            }
            assert!(barrier.authorize(parent, 3, bad).is_err());
        }
        barrier.authorization = Some(auth);
        assert!(barrier.authorize(parent, 3, auth).is_err());
    }
    #[test]
    fn f247_exec_generation_invalidates_authorization() {
        let (mut barrier, auth) = spawn_barrier_fixture();
        assert!(barrier.authorize(auth.token.parent, 4, auth).is_err());
        barrier.authorization = Some(auth);
        barrier.returned = true;
        barrier.child = true;
        assert!(barrier.complete(auth.token.parent, 4, auth, true).is_err());
        let mut next = auth;
        next.token.exec_generation = 4;
        next.token.sequence += 1;
        barrier.token = next.token;
        barrier.authorization = None;
        assert!(barrier.authorize(next.token.parent, 4, next).is_ok());
    }
    #[test]
    fn f247_resume_and_replay_preserve_snapshot() {
        let (mut barrier, auth) = spawn_barrier_fixture();
        assert!(barrier.complete(auth.token.parent, 3, auth, false).is_err());
        barrier.authorization = Some(auth);
        assert!(barrier.complete(auth.token.parent, 3, auth, false).is_err());
        barrier.returned = true;
        assert!(barrier.complete(auth.token.parent, 3, auth, true).is_err());
        assert!(barrier.complete(auth.token.parent, 3, auth, false).is_ok());
        barrier.child = true;
        assert!(barrier.complete(auth.token.parent, 3, auth, false).is_err());
        assert!(barrier.complete(auth.token.parent, 3, auth, true).is_ok());
        barrier.authorization = None;
        assert!(barrier.complete(auth.token.parent, 3, auth, true).is_err());
        let mut other = auth;
        other.table_generation += 1;
        barrier.authorization = Some(other);
        assert!(barrier.complete(auth.token.parent, 3, auth, true).is_err());
        barrier.authorization = Some(auth);
        // Completion removes the pending plan; a new capture never accepts the
        // old sequence. No replay reconstructs a plan from its digest.
        barrier.token.sequence += 1;
        barrier.authorization = None;
        assert!(barrier.authorize(auth.token.parent, 3, auth).is_err());
        assert!(barrier.complete(auth.token.parent, 3, auth, true).is_err());
    }

    #[test]
    fn owner_loss_wakes_a_pending_rsp_request() {
        use std::io::Read;
        let tasks = Arc::new(Mutex::new(WatchedTasks::default()));
        let (stream, owner_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let owner = Watchdog::new(
            None,
            Some(crate::owner::OwnerWatch::test_stream(stream).unwrap()),
            tasks.clone(),
        )
        .unwrap();
        let deadline = Watchdog::new(None, None, tasks).unwrap();
        let (mut rsp, mut peer) = Rsp::retirement_test_pair();
        deadline.wakeups.add(rsp.wakeup());
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (completed, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = rsp.request("qC");
            completed.send((result, rsp)).unwrap();
        });
        let mut request = [0; 6];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"$qC#b4");
        drop(owner_peer);
        let limit = Instant::now() + Duration::from_secs(2);
        while owner.reason().is_none() && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert_eq!(owner.reason(), Some(TerminalCause::OwnerGone));
        // Synchronize with the monitor itself before checking delivery; no race
        // between latching the reason and signalling its wakeup registry.
        drop(owner);
        let response = completion.recv_timeout(Duration::from_secs(1));
        // Always release the worker even when the wiring is broken.
        deadline.wakeups.signal();
        worker.join().unwrap();
        let (result, mut rsp) = response.expect("owner loss must wake the pending request");
        assert!(result.is_err());
        assert!(
            rsp.begin_teardown(),
            "cancellation must preserve the RSP stream"
        );
    }

    /// F2 (#205): every `TRACED_STUBS` row resolves to at least one verified
    /// `svc` site in this process's own libsystem_kernel. This reads local
    /// memory only and traces nothing, so it runs on hosted runners too.
    ///
    /// Failing before the macOS 27 fix: on macOS 27.2.1 `fchmodat` is a C
    /// wrapper that calls `__fchmodat`, with no `svc` in its first 96 bytes,
    /// so the row fails with `no svc in fchmodat` and every traced launch
    /// fails with it.
    #[test]
    fn every_traced_stub_resolves_on_this_host() {
        use super::*;
        let mut failures = Vec::new();
        for (name, number, _) in abi::TRACED_STUBS {
            match traced_stub_sites(name, *number) {
                Ok(sites) if !sites.is_empty() => {}
                Ok(_) => failures.push(format!("{name}({number}): no site")),
                Err(e) => failures.push(format!("{name}({number}): {e}")),
            }
        }
        assert!(failures.is_empty(), "unresolved stubs: {failures:?}");
    }

    /// F1 (#205 A1(a)): a tracee found as a zombie whose parent is launchd is
    /// never exit evidence, whatever the tracer reported. The kernel leaves
    /// such a zombie to launchd, which never collects it, and its original
    /// parent's `waitpid` gets ECHILD (measured natively during the design).
    /// A zombie still under its debugger stays `Zombie` evidence.
    ///
    /// The settle re-reads a zombie under launchd once: a kernel-reaped orphan
    /// passes through SZOMB with ppid 1 on its way to the system reap, and is
    /// then `Absent`, which is the existing reaped-proven evidence.
    ///
    /// After the tracer kill, a root the platform did not collect is
    /// unconfirmed, whatever the post-kill reading said (the false `Ok` the
    /// base returned with `ConfirmedAfterTracerKill(Zombie)` then
    /// `TraceeReaped(false)`). A child session is not the platform's to collect.
    #[test]
    fn a_tracee_handed_to_launchd_is_never_confirmed() {
        use super::*;
        let birth = Some((1_000, 7));
        let at = |status: u32, ppid: i32| Lookup::Found {
            status,
            start: (1_000, 7),
            ppid,
        };
        assert_eq!(
            barrier_decision(None, birth, &at(SZOMB, 4242)),
            BarrierReading::Confirmed(BarrierEvidence::Zombie)
        );
        for report in [None, Some("X09")] {
            let reading = barrier_decision(report, birth, &at(SZOMB, 1));
            assert!(
                matches!(&reading, BarrierReading::Unconfirmed(why) if why.contains("handed to launchd")),
                "report {report:?}: {reading:?}"
            );
        }
        // A pid reused by another process is still proof of the reap.
        assert!(matches!(
            barrier_decision(
                None,
                birth,
                &Lookup::Found {
                    status: SZOMB,
                    start: (2_000, 1),
                    ppid: 1,
                }
            ),
            BarrierReading::Confirmed(BarrierEvidence::ReapedProven { .. })
        ));
        // The settle: a zombie under launchd is read again; one that is gone
        // by then was a kernel-reaped orphan.
        assert_eq!(
            settled_lookup(at(SZOMB, 1), || Lookup::Absent),
            Lookup::Absent
        );
        assert_eq!(
            settled_lookup(at(SZOMB, 1), || at(SZOMB, 1)),
            at(SZOMB, 1),
            "a zombie still under launchd after the settle stays one"
        );
        assert_eq!(
            settled_lookup(at(SZOMB, 4242), || panic!(
                "only a zombie under launchd is re-read"
            )),
            at(SZOMB, 4242)
        );
        assert_eq!(
            settled_lookup(at(2, 1), || panic!("only a zombie is re-read")),
            at(2, 1)
        );
        // After the tracer kill.
        let zombie = BarrierReading::Confirmed(BarrierEvidence::Zombie);
        let unconfirmed = BarrierReading::Unconfirmed("p_stat 2".into());
        assert!(post_kill_unconfirmed(Some(&zombie), true, false));
        assert!(post_kill_unconfirmed(None, true, false));
        assert!(!post_kill_unconfirmed(Some(&zombie), true, true));
        assert!(!post_kill_unconfirmed(None, true, true));
        assert!(!post_kill_unconfirmed(Some(&zombie), false, false));
        assert!(post_kill_unconfirmed(Some(&unconfirmed), false, false));
        assert!(post_kill_unconfirmed(Some(&unconfirmed), true, true));
    }

    /// The mutation discriminator for #205 A1(b): the teardown opens by
    /// continuing its tracer, unconditionally, and records `TracerContinued`
    /// only when it was stopped. A stopped debugserver never answers `k`; a
    /// teardown that then kills it while its tracee is held at a breakpoint
    /// hands the tracee to launchd.
    #[test]
    fn the_teardown_opens_by_continuing_its_tracer() {
        use super::*;
        struct Fake {
            stopped: bool,
            continued: u32,
        }
        impl ContinueTracer for Fake {
            fn continue_tracer(&mut self) -> bool {
                self.continued += 1;
                std::mem::replace(&mut self.stopped, false)
            }
        }
        let mut stopped = Fake {
            stopped: true,
            continued: 0,
        };
        assert_eq!(
            teardown_prologue(&mut stopped),
            Some(TeardownStep::TracerContinued)
        );
        assert_eq!(stopped.continued, 1);
        let mut running = Fake {
            stopped: false,
            continued: 0,
        };
        assert_eq!(teardown_prologue(&mut running), None);
        assert_eq!(running.continued, 1, "the continue is unconditional");
    }

    /// #205: the prologue never signals a reaped tracer's pid, which may
    /// already name another process; an unreaped stopped tracer is continued.
    #[test]
    fn a_reaped_tracer_is_never_continued() {
        use super::*;
        fn stop(pid: i32) {
            // SAFETY: plain libSystem call on this test's own child.
            unsafe { libc::kill(pid, libc::SIGSTOP) };
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while bsd_info(pid).map(|i| i.pbi_status) != Some(libc::SSTOP) {
                assert!(std::time::Instant::now() < end, "{pid} never stopped");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let other = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        stop(other.id() as i32);
        let mut reaped = Rsp::retirement_test();
        reaped.retirement_reused_pid(other);
        let pid = reaped.tracer_pid();
        assert_eq!(teardown_prologue(&mut reaped), None);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            bsd_info(pid).map(|i| i.pbi_status),
            Some(libc::SSTOP),
            "the prologue signalled a reaped tracer's pid"
        );
        let mut live = Rsp::retirement_test();
        live.retirement_live_child();
        let pid = live.tracer_pid();
        stop(pid);
        assert_eq!(
            teardown_prologue(&mut live),
            Some(TeardownStep::TracerContinued)
        );
        let end = std::time::Instant::now() + Duration::from_secs(1);
        while bsd_info(pid).map(|i| i.pbi_status) == Some(libc::SSTOP) {
            assert!(
                std::time::Instant::now() < end,
                "a live stopped tracer was not continued"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_fork_child_is_signalled_only_while_it_is_still_the_census_child() {
        use super::*;
        // #205 (U-2): the read that takes the child's birth right before
        // SIGKILL must still show the census child: `parent`'s child, with the
        // birth the census read. Anything else is not signalled.
        let birth = (100, 7);
        assert_eq!(unenrolled_identity(42, Some(birth), (42, birth)), Ok(birth));
        assert_eq!(unenrolled_identity(42, None, (42, birth)), Ok(birth));
        let reparented = unenrolled_identity(42, Some(birth), (1, birth));
        assert!(
            reparented
                .as_ref()
                .is_err_and(|why| why.contains("no longer a child of pid=42")),
            "{reparented:?}"
        );
        let replaced = unenrolled_identity(42, Some(birth), (42, (101, 0)));
        assert!(
            replaced
                .as_ref()
                .is_err_and(|why| why.contains("another birth")),
            "{replaced:?}"
        );
    }

    #[test]
    fn an_unconfirmed_fork_child_fails_termination() {
        use super::*;
        // #205 (CR-21): an unenrolled fork child the sweep could not confirm
        // ended is a cleanup failure, even when every session tore down: the
        // provider and `terminate_tree` treat `Ok` as confirmed termination.
        let child = "unenrolled fork child pid=77 of pid=42: still live 1000 ms after SIGKILL";
        let failed = termination_result(Vec::new(), vec![child.to_owned()])
            .expect_err("an unconfirmed fork child is not a confirmed termination");
        assert!(failed.to_string().contains(child), "{failed}");
        let session = "Tracee pid=42: unconfirmed";
        let both = termination_result(vec![session.to_owned()], vec![child.to_owned()])
            .expect_err("both kinds fail");
        assert!(
            both.to_string().contains(session) && both.to_string().contains(child),
            "{both}"
        );
        assert!(termination_result(Vec::new(), Vec::new()).is_ok());
    }

    #[test]
    fn an_open_fork_window_is_budgeted_in_the_shutdown_allowance() {
        use super::*;
        // #205 (CR-11): the fork-child sweep runs inside the frozen shutdown
        // envelope. Its worst case is one settle and one shared wait, once,
        // whatever the number of open windows; with none open it adds nothing.
        let retirement = Duration::from_millis(1500);
        let per_session = TEARDOWN_BOUND + rsp::TRACER_REAP_BOUND + TRACEE_REAP_BOUND;
        let none = shutdown_allowance(retirement, 3, 0).unwrap();
        assert_eq!(
            none,
            retirement + per_session * 3,
            "no window: nothing added"
        );
        let sweep = UNENROLLED_FORK_SETTLE + UNENROLLED_FORK_BOUND;
        for windows in [1, 2, 17] {
            assert_eq!(
                shutdown_allowance(retirement, 3, windows).unwrap(),
                none + sweep,
                "{windows} open fork window(s): the sweep's worst case, once"
            );
        }
    }

    #[test]
    fn the_fork_sweep_waits_once_for_every_child() {
        use super::*;
        use std::cell::{Cell, RefCell};
        // #205 (CR-11): 17 signalled children (the fix-243-cap shape's parent
        // count) share one wait. Three end along the way, the rest never do:
        // the wait still ends at one UNENROLLED_FORK_BOUND, not one per child.
        let birth = (100, 7);
        let children: Vec<(i32, (u64, u64))> = (1000..1017).map(|pid| (pid, birth)).collect();
        let start = Instant::now();
        let clock = Cell::new(start);
        let reads = RefCell::new(std::collections::HashMap::<i32, u32>::new());
        let lookup = |pid: i32| {
            let mut reads = reads.borrow_mut();
            let n = reads.entry(pid).or_default();
            *n += 1;
            match (pid, *n) {
                (1000, _) => Lookup::Absent,
                (1001, n) if n > 5 => Lookup::Found {
                    status: SZOMB,
                    start: birth,
                    ppid: 42,
                },
                (1002, n) if n > 50 => Lookup::Error("transient".into()),
                _ => Lookup::Found {
                    status: 2,
                    start: birth,
                    ppid: 42,
                },
            }
        };
        let ends = await_unenrolled_ends(
            &children,
            start + UNENROLLED_FORK_BOUND,
            lookup,
            || clock.get(),
            || clock.set(clock.get() + Duration::from_millis(2)),
        );
        let waited = clock.get() - start;
        assert!(
            waited >= UNENROLLED_FORK_BOUND
                && waited < UNENROLLED_FORK_BOUND + Duration::from_millis(5),
            "one shared wait: {waited:?}"
        );
        assert_eq!(ends[0], Ok("gone"));
        assert_eq!(ends[1], Ok("a zombie under its parent"));
        assert_eq!(ends[2], Err(Lookup::Error("transient".into())));
        assert!(ends[3..]
            .iter()
            .all(|end| matches!(end, Err(Lookup::Found { .. }))));
        // Every child ends early: the wait returns at once.
        let clock = Cell::new(start);
        let ends = await_unenrolled_ends(
            &children,
            start + UNENROLLED_FORK_BOUND,
            |_| Lookup::Absent,
            || clock.get(),
            || clock.set(clock.get() + Duration::from_millis(2)),
        );
        assert_eq!(clock.get(), start, "nothing left to wait for");
        assert!(ends.iter().all(|end| *end == Ok("gone")));
    }

    /// #205: a SIGKILLed unenrolled fork child is confirmed ended only by an
    /// absent pid, a replaced birth, or a zombie of its birth; a failed lookup
    /// is never read as disappearance.
    #[test]
    fn an_unreadable_fork_child_is_never_confirmed_gone() {
        use super::*;
        let birth = (100, 7);
        let at = |status: u32, start: (u64, u64)| Lookup::Found {
            status,
            start,
            ppid: 42,
        };
        assert_eq!(unenrolled_child_state(&Lookup::Absent, birth), Some("gone"));
        assert_eq!(
            unenrolled_child_state(&at(2, (101, 0)), birth),
            Some("gone")
        );
        assert_eq!(
            unenrolled_child_state(&at(SZOMB, birth), birth),
            Some("a zombie under its parent")
        );
        assert_eq!(unenrolled_child_state(&at(2, birth), birth), None);
        assert_eq!(
            unenrolled_child_state(&Lookup::Error("EPERM".into()), birth),
            None,
            "a failed lookup is not disappearance"
        );
    }

    /// #205: the fork-child sweep excludes a child only when an enrolled
    /// session has its pid and birth (or, with no recorded birth, still has
    /// live resources). A retired session's reused pid never hides a child.
    #[test]
    fn a_retired_sessions_pid_never_hides_a_fork_child() {
        use super::*;
        let child = Some((200, 3));
        // A retired session (no live resources) whose pid a new child reused.
        assert!(!enrolled_identity(
            &[(77, Some((150, 1)), false)],
            77,
            child
        ));
        assert!(!enrolled_identity(&[(77, None, false)], 77, child));
        // The same process, enrolled.
        assert!(enrolled_identity(&[(77, Some((200, 3)), true)], 77, child));
        assert!(enrolled_identity(&[(77, None, true)], 77, child));
        // Another pid.
        assert!(!enrolled_identity(&[(78, None, true)], 77, child));
    }

    #[test]
    fn retirement_failure_does_not_end_the_session() {
        use super::*;
        for outcome in [
            crate::FinishOutcome::UnconfirmedBarrier("scripted lookup".into()),
            crate::FinishOutcome::WaitError(crate::retirement::FinishFailure {
                error: Some(crate::retirement::OsError::from(
                    &std::io::Error::from_raw_os_error(libc::ECHILD),
                )),
                elapsed: Duration::ZERO,
                last_observation: None,
                later_wait_status: None,
            }),
        ] {
            let id = TaskId(TaskIdentity {
                native_id: 0,
                generation: 1,
            });
            let report = TeardownReport {
                pid: 0,
                identity: None,
                tracer: 0,
                steps: vec![TeardownStep::Retirement(outcome.clone())],
                timed_out: true,
                unconfirmed: false,
                elapsed: Duration::ZERO,
            };
            let mut backend = MacosTraceBackend::default();
            // Resource-free recorded slots let this control exercise actual
            // poll/terminate interfaces without launching or signalling a task.
            backend.sessions.push(Session {
                ownership: None,
                id,
                thread: tid(0, 1),
                parent: None,
                stopped: false,
                done: true,
                reaped: true,
                birth: None,
                torn_down: true,
                exit_evidence: Some(BarrierEvidence::ReapedByPlatform),
                exit_report: Some("W00".into()),
                exec_generation: 0,
                #[cfg(feature = "signal-witness")]
                signal_probe_fired: false,
                signals: crate::signals::Delivery::default(),
                policy_exec_generation: 0,
                initial: false,
                live: None,
                retirement: None,
                retirement_report: Some(report.clone()),
            });
            backend.teardown_reports.push(report.clone());
            backend.stop(0, "W00".into(), false).unwrap();
            assert!(
                backend.events.is_empty(),
                "duplicate exit must not redeliver"
            );
            assert!(backend.stop(0, "W01".into(), false).is_err());
            for _ in 0..2 {
                backend.events.push_back(TraceEvent::Exit {
                    task: id,
                    status: ExitStatus::Code(0),
                });
                assert!(matches!(
                    backend.poll_event(Duration::from_millis(1)),
                    Ok(umbra_platform::EventPoll::Event(TraceEvent::Exit { .. }))
                ));
            }
            // An externally stopped sibling keeps polling valid. With a one-nanosecond
            // max_wait, poll must return Idle before touching any live body.
            backend.sessions[0].done = false;
            let idle = backend.poll_event(Duration::from_nanos(1));
            backend.sessions[0].done = true;
            assert!(matches!(idle, Ok(umbra_platform::EventPoll::Idle)));
            let later_id = TaskId(TaskIdentity {
                native_id: std::process::id() as u64,
                generation: 99,
            });
            let task = Arc::new(Task::acquire(std::process::id() as i32).unwrap());
            let weak = Arc::downgrade(&task);
            backend.sessions.push(Session {
                ownership: None,
                id: later_id,
                thread: tid(later_id.0.native_id, 1),
                parent: Some(0),
                stopped: false,
                done: true,
                reaped: false,
                birth: None,
                torn_down: false,
                exit_evidence: Some(BarrierEvidence::ReapedByPlatform),
                exit_report: Some("W00".into()),
                exec_generation: 0,
                #[cfg(feature = "signal-witness")]
                signal_probe_fired: false,
                signals: crate::signals::Delivery::default(),
                policy_exec_generation: 0,
                initial: false,
                retirement: Some(crate::retirement::Retirement::new(crate::clock::now())),
                retirement_report: None,
                live: Some(Box::new(LiveSession {
                    _witness: crate::ownership::Witness::new(later_id.0.native_id, "body"),
                    task,
                    rsp: Rsp::retirement_test(),
                    twin: PathBuf::new(),
                    interposer: None,
                    spawn: None,
                    breaks: crate::ownership::Owned::new(
                        BTreeMap::new(),
                        later_id.0.native_id,
                        "breaks",
                    ),
                    pending: crate::ownership::Owned::new(
                        BTreeMap::new(),
                        later_id.0.native_id,
                        "pending",
                    ),
                    waiting: crate::ownership::Owned::new(vec![], later_id.0.native_id, "waiting"),
                    entry: crate::ownership::Owned::new(
                        BTreeMap::new(),
                        later_id.0.native_id,
                        "entry",
                    ),
                    registers: crate::ownership::Owned::new(
                        vec![],
                        later_id.0.native_id,
                        "registers",
                    ),
                    scratch: crate::ownership::Owned::new(vec![], later_id.0.native_id, "scratch"),
                })),
            });
            backend.drain_retirements();
            assert!(
                backend.sessions[1].live.is_none(),
                "later child must retire normally"
            );
            assert!(weak.upgrade().is_none(), "actual Task owner released");
            let failure = backend
                .terminate(ProcessHandle(id), TerminationPolicy::Immediate)
                .expect_err("retirement failure must surface at final cleanup");
            assert!(failure.to_string().contains(&format!("{outcome:?}")));
            assert_eq!(backend.teardown_reports()[0], report);
            assert_eq!(backend.teardown_reports().len(), 2);
            assert_eq!(backend.failed_retirements.len(), 1);
        }
    }

    /// This file's own text, for the three pins below.
    ///
    /// Reading the source is the same device `abi.rs` uses to pin the
    /// interposer's descriptor test against its Rust twin, and it is worth the
    /// same as that one: it pins a *decision*, not the wiring, and it fires on
    /// cosmetic edits. Each pin says in its own message what a reader should do
    /// instead of mechanically updating the literal.
    const NATIVE_RS: &str = include_str!("native.rs");

    /// The production half of this file -- everything above this module.
    ///
    /// **A pin that searched the whole file would find its own message.** The
    /// assertions below quote the code they pin, so the quotes are themselves
    /// occurrences of it; the first revision of the `single_thread` pin counted
    /// three call sites, and the third was the sentence complaining about it.
    fn production() -> &'static str {
        NATIVE_RS
            .split_once("#[cfg(test)]")
            .expect("this module is introduced by a #[cfg(test)] attribute")
            .0
    }

    /// The text of one method of this file, from its signature to the next item
    /// at the same indentation.
    fn body_of(signature: &str) -> &'static str {
        let after = production()
            .split_once(signature)
            .unwrap_or_else(|| {
                panic!("`{signature}` is no longer in native.rs; this pin needs rewriting, not deleting")
            })
            .1;
        let end = ["\n    ///", "\n    fn ", "\n}"]
            .iter()
            .filter_map(|marker| after.find(marker))
            .min()
            .unwrap_or(after.len());
        &after[..end]
    }

    /// **A completed return is attributed from its own window, never from the
    /// session's last-stopped slot.** This is mutation probe M3.
    ///
    /// `Session::thread` is overwritten by every stop on the connection.
    /// `finish_return` emits the `SyscallExit` the caller pairs with its
    /// in-flight operation, and the caller keys those by thread and removes them
    /// on the exit. A miss there is **not** an error: it is the "an exit for a
    /// call we never intercepted" branch, which lets the kernel result stand and
    /// abandons the rewrite in silence. So attributing from the session slot
    /// would turn the loud escape this arc closed into a quiet one.
    ///
    /// **Why this is a source pin and not a live test, stated exactly.** The
    /// mutation was run: `finish_return` was edited to read `s.thread`, the
    /// crate rebuilt, and `mt_write` and `mt_spawn` both still passed. They
    /// cannot catch it, and neither can any other test on this backend, because
    /// the per-thread resume freezes every sibling for the whole of the window —
    /// so nothing is running that could make the two values differ. The equality
    /// is a *consequence of the freeze*, not a property of `finish_return`, and
    /// it stops holding the moment anything resumes a sibling inside a window:
    /// the deferred `single_thread()` removal, a `vCont` that names more than one
    /// thread, or a future sibling-hold. That is the class of change this exists
    /// to fail in front of, and it is precisely the class no runtime test on this
    /// backend can be written for today.
    ///
    /// It is therefore worth exactly one thing: it makes the swap impossible to
    /// make *silently*. It does not prove the attribution is right, and a reader
    /// who satisfies it by renaming a variable has defeated it.
    #[test]
    fn a_return_is_attributed_from_its_window_and_never_from_the_session_slot() {
        let body = body_of(
            "fn finish_return(&mut self, index: usize, thread: ThreadId, regs: RegisterSet)",
        );
        assert!(
            body.contains("thread: pending.thread"),
            "`finish_return` must attribute its `SyscallExit` from `pending.thread`, \
             the thread recorded when the window opened"
        );
        assert!(
            !body.contains("s.thread"),
            "`finish_return` read `Session::thread`. That slot names whoever stopped \
             last on this connection, not the thread whose return this is. The \
             caller removes its in-flight operation by thread and treats a miss as \
             a call it never intercepted -- so a wrong thread here does not fail, \
             it drops the rewrite and lets the kernel's own result stand. Read \
             `pending.thread`; see `Pending::thread` for the full account."
        );
    }

    /// **A stop absorbed inside a return window must not release the siblings.**
    ///
    /// Between `return_stop`'s `z0` and `finish_return`'s re-arm, the entry stub
    /// carries no breakpoint for the whole process, and a sibling that reaches it
    /// there walks straight through unmediated -- the `mt-write` escape. `stop`
    /// absorbs two kinds of stop without opening a transaction: a transient
    /// signal at the return gate, and a transient or attach signal anywhere else.
    /// Both resume, and a bare `c` from either is that escape.
    ///
    /// Like `the_gate_absorbs_sigchld_and_never_absorbs_sigsys`, this pins the
    /// decision and not the wiring, for the same reason: reaching the gate arm
    /// needs an asynchronous signal delivered at one exact PC, and nothing can
    /// make that happen on demand. `mt_write` covers the resume that *is*
    /// schedulable -- reverting `return_stop` to a bare `c` fails it -- but
    /// neither absorb path is reachable from a test.
    ///
    /// **Both halves are asserted, because the call sites alone were not enough.**
    /// An earlier revision of this test checked only that `stop` routes through
    /// `continue_absorbed`, and nothing checked what `continue_absorbed` does.
    /// Measured: gutting the helper to an unconditional `self.continue_run()` --
    /// reintroducing the exact escape it exists to prevent, on a path this suite
    /// takes several times per run -- left all thirteen fixtures capturing and
    /// every unit test passing, **including this one under its own name**. A pin
    /// that cannot fail is worse than no pin, because it reads as coverage. The
    /// helper's body is asserted below, and the mutation was re-run against it.
    #[test]
    fn a_stop_absorbed_inside_a_return_window_does_not_release_the_siblings() {
        let helper = body_of("fn continue_absorbed(&mut self");
        assert!(
            helper.contains("self.pending"),
            "`continue_absorbed` must consult `pending` to find out whether a \
             return window is open; a body that does not look is a bare `c` \
             wearing this function's name"
        );
        assert!(
            helper.contains("self.continue_thread("),
            "`continue_absorbed` must resume the open window's owner through \
             `continue_thread`. Without it every absorbed stop releases the \
             siblings into a stub this session has un-armed process-wide -- the \
             `mt-write` escape with a signal in front of it, which no fixture in \
             this crate catches."
        );
        let body = body_of("fn stop(&mut self");
        assert_eq!(
            body.matches("continue_absorbed()").count(),
            2,
            "`stop` absorbs a transient signal at the return gate and one away \
             from it, and both must resume through `continue_absorbed`, which \
             keeps an open window's freeze"
        );
        assert_eq!(
            body.matches("continue_run()").count(),
            0,
            "`stop` resumed with an unqualified `c`. If a return window is open \
             that releases every sibling into a stub this session has un-armed \
             process-wide, which is the `mt-write` escape with a signal in front \
             of it. Use `continue_absorbed`, which is the bare `c` when no window \
             is open and the window's owner alone when one is."
        );
    }

    /// **The exec stop drains every window; it does not look one up by the
    /// thread that stopped.**
    ///
    /// An `execve` keeps one thread and discards the rest, and the survivor is
    /// reported under a thread id belonging to the new image -- so the thread
    /// stopping at the exec need not be the one that entered the `execve`.
    /// Keying the `ReturnKind::Exec` candidate lookup on it would drop the
    /// candidate, leaving `twin` naming the previous image and the exec'd image
    /// half-mediated.
    ///
    /// **This is a source pin because no fixture discriminates it, and that is a
    /// gap this arc did not create and does not close.** Measured: keying the
    /// lookup back on the stopping thread leaves all thirteen fixtures
    /// capturing, because the only case that reaches an exec stop re-execs the
    /// *same* twin -- the candidate it drops is equal to the value already
    /// there, so dropping it is a no-op. A case that execs a *different* image
    /// would discriminate it; adding one closes a pre-existing #117/#134 gap and
    /// belongs to its own arc, not to this ratification.
    ///
    /// So this asserts the shape and says plainly what it is worth: it stops the
    /// natural-looking wrong edit that the per-thread rekeying made available,
    /// and it proves nothing about twin adoption itself.
    #[test]
    fn the_exec_stop_adopts_its_candidate_without_keying_on_the_stopping_thread() {
        let body = body_of("fn stop(&mut self");
        assert!(
            body.contains("s.pending.take()"),
            "the exec stop must drain every window and scan the drained values \
             for the `ReturnKind::Exec` candidate"
        );
        assert!(
            !body.contains("s.pending.remove("),
            "the exec stop removed a window by key. The thread that stops at an \
             `execve` need not be the one that entered it -- the address space \
             and its threads have been replaced -- so a keyed lookup drops the \
             resigned-image candidate and leaves `twin` naming the image that is \
             no longer running. Drain the map instead."
        );
    }

    /// **`single_thread()` stays, and both of its call sites with it.**
    ///
    /// #135's sub-item 3 -- removing this guard from `Delivery::Fork` and
    /// `WaitPlan::Park` -- is deferred, and the two-thread-parent-forking case
    /// that depends on it is an explicit non-goal of this arc. Deferred by test
    /// rather than by intention: the per-thread slots and the per-thread resume
    /// make the guard *look* removable, and removing it widens the blast radius
    /// to the whole fork lifecycle, where `attach_child`, the hardware gate and
    /// the memory-restore map all assume one thread.
    ///
    /// Failing here is not a reason to edit this test. It is a reason to check
    /// that the removal was ratified.
    #[test]
    fn the_fork_and_park_paths_still_refuse_a_multithreaded_tracee() {
        assert!(
            production().contains("fn single_thread(&mut self) -> Result<()> {"),
            "`single_thread` is the guard that keeps `fork` and a blocking `wait4` \
             off multithreaded tracees; its removal is deferred to its own arc"
        );
        assert_eq!(
            production().matches("s.single_thread()?;").count(),
            2,
            "`single_thread` guards exactly two sites -- `Delivery::Fork` and \
             `WaitPlan::Park` -- and both are still deferred. A third site is as \
             much a surprise as a missing one."
        );
    }

    /// `SIGCHLD` is absorbed at a stop and `SIGSYS` is not, and the return gate
    /// reads the same list the general stop path does.
    ///
    /// **What this pins and what it does not.** It pins the *decision*: the two
    /// signals whose classification is load-bearing, on the one list both sites
    /// consult. It does not exercise the wiring, because a stop at the return
    /// gate carrying `SIGCHLD` needs an asynchronous signal to be delivered at
    /// one exact PC -- the gate -- and nothing can make that happen on demand.
    /// A live test for it would be a race dressed as a gate, so there is none;
    /// `a_forked_child_of_a_routed_tracee_is_mediated_and_the_run_finishes`
    /// covers fork end to end but does not reliably land a signal there.
    ///
    /// Both halves matter in opposite directions. Dropping `SIGCHLD` fails a
    /// `wait4` run over an ordinary event (CodeRabbit, PR #116 item 4). Adding
    /// `SIGSYS` would re-open the B2 hole the gate exists to close: an
    /// unintercepted routing trap would be silently continued over and the
    /// tracee would carry on with `-1/ENOSYS`.
    #[test]
    fn the_gate_absorbs_sigchld_and_never_absorbs_sigsys() {
        assert!(
            super::TRANSIENT_SIGNALS.contains(&libc::SIGCHLD),
            "a forked child's exit must not be fatal at a return gate"
        );
        assert!(
            !super::TRANSIENT_SIGNALS.contains(&libc::SIGSYS),
            "SIGSYS is an unintercepted routing trap; absorbing it is the B2 defect"
        );
    }

    use super::*;
    use std::os::unix::ffi::OsStrExt;

    /// Truth table for the `wait4`/`wait4_nocancel` decision. Needs no
    /// debugger, fixture or environment variable; the live behaviour it
    /// describes is qualified by the `wnohang-wait` fixture case.
    #[test]
    fn wait_plan_decides_from_the_callee_visible_arguments() {
        // The SDK spells WNOHANG as mask 1 — bit index 0, not `1 << 1`.
        assert_eq!(WNOHANG, 1);
        // Live children, none finished: WNOHANG polls, a blocking wait parks.
        assert_eq!(wait_plan(-1, WNOHANG as u64, true, false), WaitPlan::Poll);
        assert_eq!(wait_plan(-1, 0, true, false), WaitPlan::Park);
        assert_eq!(wait_plan(4321, WNOHANG as u64, true, false), WaitPlan::Poll);
        // A finished child, or nothing tracked, is the kernel's to answer: it
        // owns the real status/rusage writes and ECHILD.
        assert_eq!(wait_plan(-1, WNOHANG as u64, true, true), WaitPlan::Native);
        assert_eq!(
            wait_plan(-1, WNOHANG as u64, false, false),
            WaitPlan::Native
        );
        assert_eq!(wait_plan(0, WNOHANG as u64, false, false), WaitPlan::Native);
        assert_eq!(wait_plan(-1, u64::MAX, false, false), WaitPlan::Native);
        // Selections the tracer refuses rather than mis-emulating.
        for (wanted, options) in [
            (0, WNOHANG as u64),                              // caller's group
            (-7, WNOHANG as u64),                             // group 7
            (-1, libc::WUNTRACED as u64),                     // stopped
            (-1, (WNOHANG | libc::WCONTINUED as u32) as u64), // continued
        ] {
            assert_eq!(
                wait_plan(wanted, options, true, false),
                WaitPlan::Unsupported,
                "wanted {wanted} options {options:#x}"
            );
        }
        // Arm64 leaves the upper half of a register holding an `int` argument
        // unspecified and the kernel's munger drops it, so those bits must not
        // read as reserved options. Truncation must not lose real ones either.
        assert_eq!(
            wait_plan(-1, 0xdead_beef_0000_0001, true, false),
            WaitPlan::Poll
        );
        assert_eq!(
            wait_plan(-1, 0x0000_0000_0000_0002, true, false),
            WaitPlan::Unsupported
        );
    }

    /// Drift detector for the private `posix_spawn` structures.
    ///
    /// `struct _posix_spawnattr` and `struct _posix_spawn_args_desc` are
    /// private and move between releases, and the kernel reports neither the
    /// `offsetof` nor the `sizeof` it copies out of the buffers handed to it.
    /// The attribute side is sound by construction — libc ships with the
    /// kernel, so a `malloc_size` snapshot always covers the prefix the kernel
    /// reads — but the descriptor's length is not measurable from userspace and
    /// is only assumed to fit in `SPAWN_BUFFER_BYTES`.
    ///
    /// So let the live kernel judge instead of asserting remembered offsets:
    /// build the buffers through the same helpers the tracer uses and invoke
    /// `posix_spawn` directly. A layout that outgrew the padding, or an
    /// attribute block the kernel no longer accepts, fails here rather than
    /// inside a traced process on a user's machine.
    #[test]
    fn live_kernel_accepts_the_spawn_buffers() {
        let attributes = spawn_attributes().expect("build attribute block");
        assert_eq!(attributes.block.len(), SPAWN_BUFFER_BYTES);
        assert!(
            attributes.len <= SPAWN_BUFFER_BYTES,
            "libc attribute block ({}) outgrew SPAWN_BUFFER_BYTES ({SPAWN_BUFFER_BYTES})",
            attributes.len
        );

        // The kernel reads the block at this address, so the descriptor points
        // into the local buffer rather than into tracee scratch.
        let descriptor = spawn_descriptor(attributes.block.as_ptr() as u64, attributes.len);

        // A child that would leave an observable mark if it were not suspended.
        let directory =
            std::env::temp_dir().join(format!("umbra-spawn-probe-{}", unsafe { libc::getpid() }));
        std::fs::create_dir_all(&directory).expect("probe directory");
        let marker = directory.join("ran");
        // Exec the marker command directly. Going through a shell would put
        // the path into shell source, where a temporary directory containing
        // whitespace splits into separate words: the child then fails to
        // create the marker and this test passes without detecting anything.
        // A direct exec also needs no PATH, which the empty environment below
        // does not provide.
        let touch = c"/usr/bin/touch";
        let marker_arg = std::ffi::CString::new(marker.as_os_str().as_bytes())
            .expect("temporary paths contain no NUL");
        let argv = [touch.as_ptr(), marker_arg.as_ptr(), std::ptr::null()];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];

        let mut pid: libc::pid_t = 0;
        // Syscall 244 is posix_spawn; this is the call the tracer rewrites.
        let rc = unsafe {
            libc::syscall(
                244,
                &mut pid as *mut libc::pid_t,
                touch.as_ptr(),
                descriptor.as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        assert_eq!(
            rc,
            0,
            "kernel rejected the spawn buffers: {} (attr_size {}, buffers {SPAWN_BUFFER_BYTES}); \
             the private layout likely outgrew the padding",
            std::io::Error::last_os_error(),
            attributes.len
        );
        assert!(pid > 0, "spawn reported success without a pid");

        // POSIX_SPAWN_START_SUSPENDED holds the task before its first
        // instruction, so the marker must not appear. Absence cannot fail
        // spuriously on a slow machine: a child that never ran leaves nothing
        // either way, and only a child that ran can create the file.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let ran = marker.exists();

        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        let _ = std::fs::remove_dir_all(&directory);

        assert!(
            !ran,
            "child was not suspended; POSIX_SPAWN_START_SUSPENDED did not take effect"
        );
    }

    /// Decision table for the ordered teardown's barrier (#186; cause-gate
    /// ruling R2, implement ruling 1 I3.3). No debugger, fixture or hook: the
    /// decision is a pure function of the tracer's report, the attach-time
    /// identity and one lookup.
    #[test]
    fn the_teardown_barrier_decides_from_report_identity_and_lookup() {
        let birth = Some((1_000, 7));
        // Under its debugger (any parent but launchd), as the barrier sees a
        // tracee before the hand-back.
        let found = |status: u32, start: (u64, u64)| Lookup::Found {
            status,
            start,
            ppid: 4242,
        };
        let reported = Some("X09");
        let unconfirmed = |r: &BarrierReading| matches!(r, BarrierReading::Unconfirmed(_));
        // A done session (the tracer reported W/X).
        assert!(unconfirmed(&barrier_decision(
            reported,
            birth,
            &found(2, (1_000, 7))
        )));
        assert!(unconfirmed(&barrier_decision(
            reported,
            birth,
            &found(4, (1_000, 7))
        )));
        assert_eq!(
            barrier_decision(reported, birth, &found(SZOMB, (1_000, 7))),
            BarrierReading::Confirmed(BarrierEvidence::Zombie)
        );
        assert_eq!(
            barrier_decision(reported, birth, &Lookup::Absent),
            BarrierReading::Confirmed(BarrierEvidence::TracerReportedExit {
                report: "X09".into(),
                lookup: ReportedExitLookup::Absent,
                admitted_by: ADMITTED_ABSENT,
            })
        );
        assert_eq!(
            barrier_decision(reported, birth, &found(2, (2_000, 1))),
            BarrierReading::Confirmed(BarrierEvidence::TracerReportedExit {
                report: "X09".into(),
                lookup: ReportedExitLookup::PidReused { start: (2_000, 1) },
                admitted_by: ADMITTED_REUSED,
            })
        );
        assert!(unconfirmed(&barrier_decision(
            reported,
            birth,
            &Lookup::Error("Operation not permitted".into())
        )));
        assert!(unconfirmed(&barrier_decision(
            reported,
            None,
            &found(SZOMB, (1_000, 7))
        )));
        // A live session (no report): SZOMB with the attach-time identity, or
        // proven absence or reuse (review ruling 1 V1); nothing else.
        assert_eq!(
            barrier_decision(None, birth, &found(SZOMB, (1_000, 7))),
            BarrierReading::Confirmed(BarrierEvidence::Zombie)
        );
        assert_eq!(
            barrier_decision(None, birth, &Lookup::Absent),
            BarrierReading::Confirmed(BarrierEvidence::ReapedProven {
                lookup: ReportedExitLookup::Absent,
                admitted_by: ADMITTED_ABSENT,
            })
        );
        assert_eq!(
            barrier_decision(None, birth, &found(SZOMB, (2_000, 1))),
            BarrierReading::Confirmed(BarrierEvidence::ReapedProven {
                lookup: ReportedExitLookup::PidReused { start: (2_000, 1) },
                admitted_by: ADMITTED_REUSED,
            })
        );
        assert!(unconfirmed(&barrier_decision(
            None,
            None,
            &found(SZOMB, (1_000, 7))
        )));
        assert!(unconfirmed(&barrier_decision(
            None,
            birth,
            &found(2, (1_000, 7))
        )));
        assert!(unconfirmed(&barrier_decision(
            None,
            birth,
            &Lookup::Error("Operation not permitted".into())
        )));
    }
}

#[cfg(test)]
mod bounded_poll_tests {
    use super::*;
    fn exit_event() -> TraceEvent {
        TraceEvent::Exit {
            task: TaskId(umbra_core::TaskIdentity {
                native_id: 1,
                generation: 1,
            }),
            status: umbra_core::ExitStatus::Code(0),
        }
    }
    #[test]
    fn queued_event_precedes_expiry_but_not_watchdog() {
        let mut backend = MacosTraceBackend::default();
        backend.events.push_back(exit_event());
        assert!(matches!(
            backend.poll_event(Duration::from_nanos(1)).unwrap(),
            umbra_platform::EventPoll::Event(_)
        ));
        backend.events.push_back(exit_event());
        backend.deadline = Some(crate::clock::now());
        assert!(backend
            .poll_event(Duration::from_millis(1))
            .unwrap_err()
            .to_string()
            .contains("session timed out"));
        assert_eq!(backend.events.len(), 1);
    }
    #[test]
    fn no_live_process_and_invalid_budget_are_not_idle() {
        let mut backend = MacosTraceBackend::default();
        assert!(backend
            .poll_event(Duration::from_nanos(1))
            .unwrap_err()
            .to_string()
            .contains("no live processes"));
        for wait in [Duration::ZERO, Duration::MAX] {
            assert!(backend.poll_event(wait).is_err());
        }
    }
}

fn operation_deadline(lifetime: Option<Instant>, budget: Duration) -> Instant {
    let limit = crate::clock::now() + budget;
    lifetime.map_or(limit, |d| d.min(limit))
}

#[cfg(test)]
mod monitor_sleep_tests {
    use super::*;
    use crate::{clock::SCRIPT, owner::Wake};
    #[test]
    fn early_timeout_then_disarm_does_not_terminate() {
        let start = Instant::now();
        let mut calls = 0;
        SCRIPT.with(|v| v.set(Some((start, start + Duration::from_secs(86400)))));
        assert_eq!(
            monitor_reason(Some(start + Duration::from_secs(1)), || {
                calls += 1;
                Ok(if calls == 1 {
                    Wake::Timeout
                } else {
                    Wake::Disarm
                })
            }),
            None
        );
        assert_eq!(calls, 2);
        SCRIPT.with(|v| v.set(None));
    }
    struct Count(std::rc::Rc<std::cell::Cell<usize>>);
    impl KillOwned for Count {
        fn kill_owned(&self) {
            self.0.set(self.0.get() + 1);
        }
    }
    impl KillOwned for Arc<std::sync::atomic::AtomicUsize> {
        fn kill_owned(&self) {
            self.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn counted() -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }
    #[test]
    fn retirement_registry_preserves_live_and_late_tasks() {
        for latch_first in [false, true] {
            let mut registry = WatchedTasks::default();
            let finished = counted();
            let live = counted();
            let late = counted();
            let weak = Arc::downgrade(&finished);
            registry.add(finished.clone());
            registry.add(live.clone());
            if latch_first {
                registry.latch(TerminalCause::OwnerGone);
            }
            let retired = registry.retire(&finished).expect("exact owner");
            assert!(Arc::ptr_eq(&retired, &finished));
            drop(finished);
            assert!(weak.upgrade().is_some());
            drop(retired);
            assert!(weak.upgrade().is_none());
            if !latch_first {
                registry.latch(TerminalCause::OwnerGone);
            }
            registry.add(late.clone());
            assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(late.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(registry.tasks.len(), 2);
        }
    }

    #[test]
    fn exec_replacements_keep_registry_bounded() {
        let mut registry = WatchedTasks::default();
        let mut current = counted();
        registry.add(current.clone());
        for _ in 0..128 {
            let next = counted();
            let retired = registry.replace(&current, next.clone());
            assert_eq!(registry.tasks.len(), 1);
            assert!(Arc::ptr_eq(retired.as_ref().unwrap(), &current));
            drop(retired);
            assert_eq!(Arc::strong_count(&current), 1);
            current = next;
        }
    }
    #[test]
    fn exec_replacements_latch_kills_only_current_entries_once() {
        let mut registry = WatchedTasks::default();
        let sibling = counted();
        let old = counted();
        let new = counted();
        registry.add(sibling.clone());
        registry.add(old.clone());
        drop(registry.replace(&old, new.clone()));
        registry.latch(TerminalCause::OwnerGone);
        registry.latch(TerminalCause::OwnerGone);
        assert_eq!(old.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(sibling.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(new.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn exec_replacement_after_latch_kills_at_registration() {
        let mut registry = WatchedTasks::default();
        let old = counted();
        let new = counted();
        registry.add(old.clone());
        registry.latch(TerminalCause::OwnerGone);
        drop(registry.replace(&old, new.clone()));
        assert_eq!(old.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(new.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(registry.tasks.len(), 1);
        assert!(Arc::ptr_eq(&registry.tasks[0], &new));
        registry.latch(TerminalCause::OwnerGone);
        assert_eq!(new.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn exec_replacement_of_absent_entry_preserves_existing_entries() {
        let mut registry = WatchedTasks::default();
        let existing = counted();
        let absent = counted();
        let new = counted();
        registry.add(existing.clone());
        assert!(registry.replace(&absent, new.clone()).is_none());
        assert_eq!(registry.tasks.len(), 2);
        assert!(registry
            .tasks
            .iter()
            .any(|entry| Arc::ptr_eq(entry, &existing)));
        assert!(registry.tasks.iter().any(|entry| Arc::ptr_eq(entry, &new)));
        registry.latch(TerminalCause::OwnerGone);
        assert_eq!(existing.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(new.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(absent.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[test]
    fn latched_reason_kills_every_task_once_including_late_registration() {
        for reason in [
            TerminalCause::Deadline,
            TerminalCause::OwnerGone,
            TerminalCause::MonitorFailed,
        ] {
            let count = std::rc::Rc::new(std::cell::Cell::new(0));
            let mut tasks = WatchedTasks::default();
            tasks.add(Count(count.clone()));
            tasks.add(Count(count.clone()));
            tasks.latch(reason);
            tasks.latch(reason);
            assert_eq!(count.get(), 2);
            tasks.add(Count(count.clone()));
            assert_eq!(count.get(), 3);
            assert_eq!(tasks.reason, Some(reason));
        }
    }
    #[test]
    fn genuine_expiry_and_owner_loss_are_terminal() {
        let start = Instant::now();
        SCRIPT.with(|v| v.set(Some((start, start))));
        assert_eq!(
            monitor_reason(Some(start), || Ok(Wake::Timeout)),
            Some(TerminalCause::Deadline)
        );
        assert_eq!(
            monitor_reason(None, || Ok(Wake::OwnerGone)),
            Some(TerminalCause::OwnerGone)
        );
        SCRIPT.with(|v| v.set(None));
    }
}

#[cfg(test)]
#[path = "retirement_controls.rs"]
mod retirement_controls;

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod shutdown_tests;

#[cfg(test)]
#[path = "teardown_tests.rs"]
mod teardown_tests;
