//! Thread-affine debugger controller. Unsafe code is limited to owned Mach rights,
//! local libSystem calls and bounded remote memory copies.
use crate::{
    abi::{self, get, set, CPSR, PC},
    cache, error, interpose,
    rsp::{self, Rsp},
    unsupported, Options,
};
use mach2::{
    traps::{mach_task_self, task_for_pid},
    vm::mach_vm_read_overwrite,
};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CString, OsStr},
    marker::PhantomData,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{mpsc, Arc, Mutex},
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
struct Task(u32);
impl Task {
    fn acquire(pid: i32) -> Result<Self> {
        let mut port = 0;
        // SAFETY: valid out pointer; the returned send right is owned by Task.
        unsafe {
            mach(
                task_for_pid(mach_task_self(), pid, &mut port),
                "task_for_pid",
            )?;
        }
        Ok(Self(port))
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
    fn kill(&self) {
        unsafe {
            libc::task_terminate(self.0);
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        unsafe {
            mach2::mach_port::mach_port_deallocate(mach_task_self(), self.0);
        }
    }
}
impl TraceMemory for &Task {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        Task::read(self, address, out)
    }
}
struct Watchdog {
    tasks: Arc<Mutex<Vec<Arc<Task>>>>,
    cancel: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new(deadline: Instant) -> Self {
        let tasks = Arc::new(Mutex::new(Vec::<Arc<Task>>::new()));
        let owned = tasks.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            if rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err()
            {
                for task in owned.lock().unwrap().iter().rev() {
                    task.kill();
                }
            }
        });
        Self {
            tasks,
            cancel: Some(tx),
            worker: Some(worker),
        }
    }
    fn add(&self, task: Arc<Task>) {
        self.tasks.lock().unwrap().push(task);
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().iter().rev() {
            task.kill();
        }
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
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
struct Pending {
    entry: u64,
    entry_breakpoint: Breakpoint,
    gate: u64,
    kind: ReturnKind,
    hardware: bool,
}
struct Session {
    task: Arc<Task>,
    rsp: Rsp,
    id: TaskId,
    thread: ThreadId,
    parent: Option<usize>,
    twin: PathBuf,
    // Only software breakpoints installed on this session's RSP connection.
    // Disabled entry sites live in Pending; inherited bytes are not ownership.
    breaks: BTreeMap<u64, Breakpoint>,
    pending: Option<Pending>,
    waiting: Vec<usize>,
    entry: Option<u64>,
    stopped: bool,
    done: bool,
    reaped: bool,
    exec_generation: u64,
    registers: Vec<(usize, usize)>,
    scratch: Vec<(u64, usize, usize)>,
    initial: bool,
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
impl Session {
    #[allow(clippy::too_many_arguments)]
    fn attach(
        pid: i32,
        generation: u64,
        twin: PathBuf,
        parent: Option<usize>,
        options: &Options,
        deadline: Instant,
        watchdog: &Watchdog,
        interposer: Option<(PathBuf, PathBuf, u32)>,
    ) -> Result<Self> {
        let task = Arc::new(Task::acquire(pid)?);
        watchdog.add(task.clone());
        let mut rsp = Rsp::connect(options, deadline)?;
        let stop = rsp.request(&format!("vAttach;{pid:x}"))?;
        if !stop.starts_with('T') {
            return Err(error("attach", stop));
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
            task,
            rsp,
            id: TaskId(TaskIdentity {
                native_id: pid as u64,
                generation,
            }),
            thread: tid(thread, generation),
            parent,
            twin,
            breaks: BTreeMap::new(),
            pending: None,
            waiting: vec![],
            entry: None,
            stopped: true,
            done: false,
            reaped: false,
            exec_generation: 0,
            registers,
            scratch: vec![],
            initial: true,
            interposer,
        })
    }
    fn regs(&mut self) -> Result<RegisterSet> {
        if !self.stopped || self.done {
            return Err(error("registers", "task is not stopped"));
        }
        let raw = rsp::unhex(
            &self
                .rsp
                .request(&format!("g;thread:{:x};", self.thread.0.native_id))?,
        )?;
        let mut regs = RegisterSet::new(Architecture::Aarch64, vec![0; 272])?;
        for (index, (offset, len)) in self.registers.iter().enumerate() {
            // Current Apple debugserver does not implement the bulk g packet.
            // An empty reply means unsupported; use individual register reads.
            let individual;
            let b = if raw.is_empty() {
                individual = rsp::unhex(
                    &self
                        .rsp
                        .request(&format!("p{index:x};thread:{:x};", self.thread.0.native_id))?,
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
    fn set_regs(&mut self, regs: &RegisterSet) -> Result<()> {
        let old = self.regs()?;
        for i in 0..34 {
            let v = get(regs, i)?;
            if v != get(&old, i)? {
                self.rsp.ok(&format!(
                    "P{i:x}={};thread:{:x};",
                    rsp::hex(&v.to_le_bytes()[..self.registers[i].1]),
                    self.thread.0.native_id
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
    fn continue_run(&mut self) -> Result<()> {
        self.rsp.send("c")?;
        self.stopped = false;
        Ok(())
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
    fn return_stop(&mut self, kind: ReturnKind) -> Result<()> {
        // **One intercepted syscall in flight per session, asserted where it can
        // be violated rather than left implicit.**
        //
        // `pending` is a single slot and the assignment below is unconditional,
        // so a second `return_stop` before the first is consumed overwrites it.
        // Three things ride on that slot and all three are lost by such an
        // overwrite, not only the newest: `entry_breakpoint`, which `finish_return`
        // hands back to `install_breakpoint` -- the site was already released
        // with `z0` a few lines down, so losing the value means it is never
        // re-armed and that stub stops being intercepted for the rest of the
        // run; `gate`, so the first return gate stays registered and is never
        // retired; and, since the R1 fix, the `Exec` candidate image -- losing which leaves `twin` naming the previous
        // image, `install()` matching nothing, and the exec'd image
        // half-mediated with its stubs breakpointed and its interposer inert.
        //
        // **An assertion rather than a refusal, deliberately.** This is a
        // pre-existing invariant of the single `Pending` slot, not something the
        // exec candidate introduced -- the breakpoint corruption above predates
        // it and is worse than the lost candidate. A run that reaches this state
        // is already broken by other means, so repairing one symptom of it (by
        // moving the candidate to a field of its own) would hide the violation
        // rather than surface it. What is wanted is to catch the violation at
        // the point it happens, which is here.
        //
        // The only way to reach it is a multithreaded tracee -- `continue_run`
        // resumes every thread, and `single_thread()` gates `Delivery::Fork` and
        // `WaitPlan::Park` but **not** `Delivery::Exec` or a plain `Namespace`
        // call -- and multithreaded tracees are the ratified next arc, which will
        // have to revisit this slot wholesale. This is the tripwire that arc
        // wants, and it is live in every `cargo test` run because the test
        // profile is a debug profile. In release it costs nothing and changes
        // nothing, which is why it is not a behaviour change to a shipped path.
        debug_assert!(
            self.pending.is_none(),
            "a second intercepted syscall entered while one was still in flight: \
             this session's pending entry breakpoint, return gate and exec \
             candidate would all be overwritten"
        );
        let regs = self.regs()?;
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
        self.pending = Some(Pending {
            entry: pc,
            entry_breakpoint,
            gate,
            kind,
            hardware,
        });
        self.continue_run()
    }
    fn install(&mut self) -> Result<()> {
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
            let name_c = cstr(name.as_bytes())?;
            let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name_c.as_ptr()) };
            if ptr.is_null() {
                return Err(error("symbol", format!("missing {name}")));
            }
            // Strip arm64e function pointer authentication; only inspect local executable memory.
            let start = (ptr as u64) & 0x0000_ffff_ffff_ffff;
            let mut code = [0; 96];
            let me = Task(unsafe { mach_task_self() });
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
            if carried != *number {
                return Err(error(
                    "symbol",
                    format!("{name} issues syscall {carried}, not the {number} umbra traces it as"),
                ));
            }
            self.breakpoint(start + offset as u64 * 4, false)?;
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
        loop {
            let stop = self.rsp.request("c")?;
            if !stop.starts_with('T') {
                return Err(error("run_to", stop));
            }
            let fields = rsp::fields(&stop[3..]);
            self.thread = tid(
                rsp::number(
                    fields
                        .get("thread")
                        .ok_or_else(|| error("run_to", "no thread"))?,
                )?,
                self.id.0.generation,
            );
            let regs = self.regs()?;
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
            let stop = self.rsp.request("c")?;
            if !stop.starts_with('T') {
                return Err(error("dyld startup", stop));
            }
            let fields = rsp::fields(&stop[3..]);
            self.thread = tid(
                rsp::number(
                    fields
                        .get("thread")
                        .ok_or_else(|| error("dyld", "no thread"))?,
                )?,
                self.id.0.generation,
            );
            let regs = self.regs()?;
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
impl Drop for Session {
    fn drop(&mut self) {
        if !self.done {
            self.task.kill();
        }
    }
}

/// Native prototype control, deliberately !Send/!Sync. Construct on the provider thread.
pub struct MacosTraceBackend {
    options: Options,
    sessions: Vec<Session>,
    events: VecDeque<TraceEvent>,
    // Stop replies consumed by quiesce still belong to the normal event loop.
    quiesced_stops: VecDeque<(usize, String)>,
    deadline: Option<Instant>,
    watchdog: Option<Watchdog>,
    generation: u64,
    /// The routing interposer this run loads and the image it routes in, if any.
    ///
    /// Set by `launch_traced` before the spawn and handed to every `Session` in
    /// the tree, so a descendant execing the same image breakpoints the same trap
    /// sites. See `Session::interposer`.
    interposer: Option<(PathBuf, PathBuf, u32)>,
    _affine: PhantomData<Rc<()>>,
}
impl Default for MacosTraceBackend {
    fn default() -> Self {
        Self::new(Options::default())
    }
}
impl MacosTraceBackend {
    pub fn new(options: Options) -> Self {
        Self {
            options,
            sessions: vec![],
            events: VecDeque::new(),
            quiesced_stops: VecDeque::new(),
            deadline: None,
            watchdog: None,
            generation: 0,
            interposer: None,
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
    fn thread_index(&self, thread: ThreadId) -> Result<usize> {
        self.sessions
            .iter()
            .position(|s| s.thread == thread && !s.done)
            .ok_or_else(|| {
                UmbraError::new(ErrorKind::StaleHandle, "thread", "unknown or exited thread")
            })
    }
    fn check_deadline(&mut self) -> Result<()> {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            for s in &self.sessions {
                s.task.kill();
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
    fn launch_traced(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        Self::check_sandbox_shape(&spec)?;
        if self.deadline.is_some() {
            return Err(error("launch", "one run per backend"));
        }
        if self.options.timeout_ms == 0 || self.options.timeout_ms > 3_600_000 {
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
        let deadline = Instant::now() + Duration::from_millis(self.options.timeout_ms);
        self.deadline = Some(deadline);
        self.watchdog = Some(Watchdog::new(deadline));
        let twin = cache::resign(
            Path::new(OsStr::from_bytes(spec.executable.as_bytes())),
            &self.options,
            deadline,
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
            let published = interpose::publish(&self.options, deadline)?;
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
                let bootstrap =
                    cache::resign(Path::new(SANDBOX_BOOTSTRAP), &self.options, deadline)?;
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
                errno(
                    libc::posix_spawnattr_setflags(
                        &mut attr,
                        (libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_CLOEXEC_DEFAULT)
                            as i16,
                    ),
                    "spawn flags",
                )?;
                errno(
                    posix_spawn_file_actions_addchdir_np(&mut actions, cwd.as_ptr()),
                    "spawn cwd",
                )?;
                for fd in &spec.policy.inherited_fds {
                    errno(
                        libc::posix_spawn_file_actions_adddup2(&mut actions, fd.0, fd.0),
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
            result?;
        }
        self.generation += 1;
        let session = Session::attach(
            pid,
            self.generation,
            image,
            None,
            &self.options,
            deadline,
            self.watchdog.as_ref().unwrap(),
            self.interposer.clone(),
        );
        let session = match session {
            Ok(s) => s,
            Err(mut e) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    e.launch_tree_terminated = reap_launched_child(pid);
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
                    session.task.kill();
                }
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
                TraceEvent::Child { .. } => {
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
        let regs = self.sessions[i].regs()?;
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
    fn attach_child(
        &mut self,
        parent: usize,
        pid: i32,
        twin: PathBuf,
        kind: ChildKind,
        restore: Option<BTreeMap<u64, [u8; 4]>>,
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
        let mut child = Session::attach(
            pid,
            self.generation,
            twin,
            Some(parent),
            &self.options,
            self.deadline.unwrap(),
            self.watchdog.as_ref().unwrap(),
            self.interposer.clone(),
        )?;
        child.retarget_interposer(&image)?;
        // The parent supplied memory repairs, not breakpoints owned by this RSP.
        debug_assert!(child.breaks.is_empty());
        if let Some(restore) = restore {
            for (address, bytes) in restore {
                child.write(address, &bytes)?;
            }
        }
        child.install()?;
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
        self.events.push_back(event);
        self.events.push_back(thread_event);
        Ok(())
    }
    fn finish_return(&mut self, index: usize, regs: RegisterSet) -> Result<()> {
        let pending = self.sessions[index].pending.take().unwrap();
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
            ReturnKind::Syscall => {
                self.events.push_back(TraceEvent::SyscallExit {
                    task: s.id,
                    thread: s.thread,
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
                    let twin = s.twin.clone();
                    self.attach_child(
                        index,
                        return_value as i32,
                        twin,
                        ChildKind::Fork,
                        Some(restore),
                    )?;
                }
                self.sessions[index].continue_run()?;
            }
            ReturnKind::Spawn { pid_pointer, twin } => {
                if let OperationOutcome::Success { .. } = abi::outcome(&regs)? {
                    let mut bytes = [0; 4];
                    s.task.read(pid_pointer, &mut bytes)?;
                    self.attach_child(
                        index,
                        i32::from_le_bytes(bytes),
                        twin,
                        ChildKind::Spawn,
                        None,
                    )?;
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
    fn intercept(&mut self, index: usize, mut regs: RegisterSet) -> Result<()> {
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
                s.entry = Some(pc);
                self.events.push_back(TraceEvent::SyscallEntry {
                    task: s.id,
                    thread: s.thread,
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
                s.return_stop(ReturnKind::Fork { restore })?;
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
                        s.set_regs(&regs)?;
                        s.continue_run()?;
                    }
                    WaitPlan::Park => {
                        s.single_thread()?;
                        s.waiting = children;
                    }
                    WaitPlan::Native => s.return_stop(ReturnKind::Wait)?,
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
                    self.deadline.unwrap(),
                )?;
                let s = &mut self.sessions[index];
                let mut bytes = twin.as_os_str().as_bytes().to_vec();
                bytes.push(0);
                set(&mut regs, slot, s.allocate(&bytes)?)?;
                if number == 244 {
                    if get(&regs, 2)? != 0 {
                        return Err(unsupported("spawn requires null attributes/file actions"));
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
                    s.set_regs(&regs)?;
                    s.return_stop(ReturnKind::Spawn { pid_pointer, twin })?;
                } else {
                    // Not `s.twin = twin` -- see `ReturnKind::Exec`. The image
                    // this session runs may not move until an exec has actually
                    // happened, and at this entry it has not.
                    s.set_regs(&regs)?;
                    s.return_stop(ReturnKind::Exec { twin })?;
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
        if reply.starts_with('W') || reply.starts_with('X') {
            let status =
                rsp::number(reply.get(1..3).ok_or_else(|| error("exit", "bad packet"))?)? as i32;
            let s = &mut self.sessions[index];
            s.done = true;
            s.stopped = false;
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
                unsafe {
                    s.reaped =
                        libc::waitpid(s.id.0.native_id as i32, std::ptr::null_mut(), libc::WNOHANG)
                            == s.id.0.native_id as i32;
                }
            }
            return Ok(());
        }
        if !reply.starts_with('T') {
            return Err(error("stop", reply));
        }
        let fields = rsp::fields(&reply[3..]);
        let thread = rsp::number(
            fields
                .get("thread")
                .ok_or_else(|| error("stop", "no thread"))?,
        )?;
        let s = &mut self.sessions[index];
        s.stopped = true;
        s.thread = tid(thread, s.id.0.generation);
        if fields.get("reason") == Some(&"exec") {
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
            let adopted = match s.pending.take() {
                Some(Pending {
                    kind: ReturnKind::Exec { twin },
                    ..
                }) => Some(twin),
                _ => None,
            };
            if let Some(twin) = adopted {
                s.twin = twin;
            }
            s.scratch.clear();
            s.entry = None;
            s.exec_generation += 1;
            let task = Arc::new(Task::acquire(s.id.0.native_id as i32)?);
            self.watchdog.as_ref().unwrap().add(task.clone());
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
        let regs = s.regs()?;
        let pc = get(&regs, PC)?;
        let signal = rsp::number(&reply[1..3])? as i32;
        if s.pending.as_ref().is_some_and(|p| p.gate == pc) {
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
                    s.continue_run()?;
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
            return self.finish_return(index, regs);
        }
        if s.breaks.contains_key(&pc) {
            return self.intercept(index, regs);
        }
        // Continuing with `c` (no C signal) suppresses these attach transients.
        if TRANSIENT_SIGNALS.contains(&signal) || (interrupted && signal == libc::SIGINT) {
            s.continue_run()?;
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
fn reap_launched_child(pid: i32) -> bool {
    loop {
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if result == pid {
            return true;
        }
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return false;
    }
}

impl TraceBackend for MacosTraceBackend {
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        if matches!(spec.sandbox, SandboxRequirement::UnsandboxedExperiment) {
            return Err(unsupported(
                "supervised launch requires an installed sandbox",
            ));
        }
        self.launch_traced(spec)
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        loop {
            self.check_deadline()?;
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            if let Some((index, reply)) = self.quiesced_stops.pop_front() {
                self.stop(index, reply, true)?;
                continue;
            }
            if self.sessions.is_empty() || self.sessions.iter().all(|s| s.done) {
                return Err(error("next_event", "no live processes"));
            }
            for i in 0..self.sessions.len() {
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
                        self.sessions[i].return_stop(ReturnKind::Wait)?;
                    }
                    continue;
                }
                if self.sessions[i].stopped {
                    continue;
                }
                if let Some(reply) = self.sessions[i].rsp.poll(Duration::from_millis(2))? {
                    self.stop(i, reply, false)?;
                }
                if !self.events.is_empty() {
                    break;
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
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet> {
        let i = self.thread_index(thread)?;
        self.sessions[i].regs()
    }
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()> {
        let i = self.thread_index(thread)?;
        self.sessions[i].set_regs(regs)
    }
    fn resume(&mut self, command: ResumeCommand) -> Result<()> {
        self.check_deadline()?;
        let i = self.thread_index(command.thread)?;
        if self.quiesced_stops.iter().any(|(index, _)| *index == i) {
            return Err(error(
                "resume",
                "consume quiesced stop with next_event first",
            ));
        }
        let s = &mut self.sessions[i];
        if !s.stopped || !s.waiting.is_empty() {
            return Err(error("resume", "not externally stopped"));
        }
        if command.signal.is_some() {
            return Err(unsupported("explicit signal forwarding is not qualified"));
        }
        if command.mode == ResumeMode::SingleStep {
            return Err(unsupported("external single stepping"));
        }
        if let Some(pc) = s.entry.take() {
            let regs = s.regs()?;
            if get(&regs, PC)? == pc {
                s.return_stop(ReturnKind::Syscall)
            } else {
                self.events.push_back(TraceEvent::SyscallExit {
                    task: s.id,
                    thread: s.thread,
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
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            architectures: vec![Architecture::Aarch64],
            // Both are implemented and measured on this host: scratch-backed
            // open/openat rewriting by the direct fixture tests, and the traced
            // installer handoff by tests/sandbox_launch.rs, which also proves an
            // unrewritten write outside the run root is denied by the kernel.
            capabilities: [
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
        if self.sessions.first().is_none_or(|s| s.id != process.0) {
            return Err(UmbraError::new(
                ErrorKind::StaleHandle,
                "quiesce",
                "unknown root",
            ));
        }
        self.check_deadline()?;
        let result = (|| {
            // Interrupt all live connections before waiting on any one of them.
            for s in &mut self.sessions {
                if !s.done && !s.stopped {
                    s.rsp.interrupt()?;
                }
            }
            for i in 0..self.sessions.len() {
                while !self.sessions[i].done && !self.sessions[i].stopped {
                    self.check_deadline()?;
                    let Some(reply) = self.sessions[i].rsp.poll(Duration::from_millis(10))? else {
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
                s.task.kill();
            }
            return Err(e);
        }
        if self.sessions.iter().any(|s| !s.done && s.pending.is_some()) {
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
    }
    fn terminate(&mut self, process: ProcessHandle, policy: TerminationPolicy) -> Result<()> {
        if self.sessions.first().is_none_or(|s| s.id != process.0) {
            return Err(error("terminate", "unknown root"));
        }
        if !matches!(policy, TerminationPolicy::Immediate) {
            return Err(unsupported("graceful tree termination"));
        }
        for s in self.sessions.iter().rev() {
            if !s.done {
                s.task.kill();
            }
        }
        self.sessions.clear();
        self.events.clear();
        self.quiesced_stops.clear();
        self.watchdog.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
}
