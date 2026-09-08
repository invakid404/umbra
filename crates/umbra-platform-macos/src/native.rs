//! Thread-affine debugger controller. Unsafe code is limited to owned Mach rights,
//! local libSystem calls and bounded remote memory copies.
use crate::{
    abi::{self, get, set, CPSR, PC},
    cache, error,
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
    Open,
    Wait,
    Fork { restore: BTreeMap<u64, [u8; 4]> },
    Spawn { pid_pointer: u64, twin: PathBuf },
    Exec,
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
}
impl Session {
    fn attach(
        pid: i32,
        generation: u64,
        twin: PathBuf,
        parent: Option<usize>,
        options: &Options,
        deadline: Instant,
        watchdog: &Watchdog,
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
    fn return_stop(&mut self, kind: ReturnKind) -> Result<()> {
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
        // Shared cache addresses are system-wide on this native host. Verify every
        // remote instruction against local code before planting any breakpoint.
        for name in [
            "__open",
            "__open_nocancel",
            "__openat",
            "__openat_nocancel",
            "__execve",
            "__posix_spawn",
            "__fork",
            "__wait4",
            "__wait4_nocancel",
        ] {
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
            let offset = code
                .chunks_exact(4)
                .position(|b| b == [1, 16, 0, 212])
                .ok_or_else(|| error("symbol", format!("no svc in {name}")))?
                * 4;
            self.breakpoint(start + offset as u64, false)?;
        }
        self.install_raw()?;
        Ok(())
    }
    fn wait_for_dyld(&mut self) -> Result<()> {
        let address = rsp::number(&self.rsp.request("qShlibInfoAddr")?)?;
        let mut info = [0; 16];
        self.task.read(address, &mut info)?;
        if u32at(&info, 4)? != 0 {
            return Ok(());
        }
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
    fn install_raw(&mut self) -> Result<()> {
        let reply = self
            .rsp
            .request("jGetLoadedDynamicLibrariesInfos:{\"fetch_all_solibs\":true}")?;
        let json: serde_json::Value =
            serde_json::from_str(&reply).map_err(|e| error("images", format!("{e}: {reply}")))?;
        let images = json["images"]
            .as_array()
            .ok_or_else(|| error("images", "missing images"))?;
        let main = images
            .iter()
            .find(|v| v["mach_header"]["filetype"].as_u64() == Some(2))
            .ok_or_else(|| error("images", format!("no main image: {reply}")))?;
        let base = main["load_address"]
            .as_u64()
            .ok_or_else(|| error("images", "no main address"))?;
        let mut header = [0; 32];
        self.task.read(base, &mut header)?;
        if u32at(&header, 0)? != 0xfeedfacf || u32at(&header, 4)? != 0x100000c {
            return Err(unsupported("main image must be thin arm64 Mach-O"));
        }
        let size = u32at(&header, 20)? as usize;
        if size > 1024 * 1024 {
            return Err(error("Mach-O", "load command limit"));
        }
        let mut commands = vec![0; size];
        self.task.read(base + 32, &mut commands)?;
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
            for (i, insn) in code.chunks_exact(4).enumerate() {
                if insn == [1, 16, 0, 212] {
                    self.breakpoint(address + (i * 4) as u64, true)?;
                }
            }
        }
        Ok(())
    }
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
    /// Compatibility entry point for the same LocalDevelopment tracer as trait launch.
    pub fn launch_experimental(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        self.launch_traced(spec)
    }
    fn launch_traced(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        if self.deadline.is_some() {
            return Err(error("launch", "one run per backend"));
        }
        if self.options.timeout_ms == 0 || self.options.timeout_ms > 3_600_000 {
            return Err(error("launch", "timeout must be 1..3600000 ms"));
        }
        if spec.policy.persistence != PersistencePolicy::LocalDevelopment {
            return Err(unsupported(
                "prototype only supports explicit LocalDevelopment",
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
        if spec.argv.is_empty() || !spec.executable.is_absolute() || !spec.cwd.is_absolute() {
            return Err(error(
                "launch",
                "absolute executable/cwd and argv[0] required",
            ));
        }
        let args = spec
            .argv
            .iter()
            .map(|a| cstr(a))
            .collect::<Result<Vec<_>>>()?;
        let env = spec
            .environment
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
        let cwd = cstr(spec.cwd.as_bytes())?;
        let deadline = Instant::now() + Duration::from_millis(self.options.timeout_ms);
        self.deadline = Some(deadline);
        self.watchdog = Some(Watchdog::new(deadline));
        let twin = cache::resign(
            Path::new(OsStr::from_bytes(spec.executable.as_bytes())),
            &self.options,
            deadline,
        )?;
        let path = cstr(twin.as_os_str().as_bytes())?;
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
            twin,
            None,
            &self.options,
            deadline,
            self.watchdog.as_ref().unwrap(),
        );
        let session = match session {
            Ok(s) => s,
            Err(e) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
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
        Ok(handle)
    }
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
    pub fn prepare_rewrite(
        &mut self,
        thread: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite> {
        let i = self.thread_index(thread)?;
        let regs = self.sessions[i].regs()?;
        if path.as_bytes().len() >= abi::MAX_PATH {
            return Err(error("rewrite", "path exceeds 4 KiB"));
        }
        let address = self.sessions[i].allocate(&vec![0; path.as_bytes().len() + 1])?;
        abi::prepare_path(&regs, address, path, operation)
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
        let mut child = Session::attach(
            pid,
            self.generation,
            twin,
            Some(parent),
            &self.options,
            self.deadline.unwrap(),
            self.watchdog.as_ref().unwrap(),
        )?;
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
            ReturnKind::Open => {
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
            ReturnKind::Exec => {
                self.sessions[index].continue_run()?;
            }
        }
        Ok(())
    }
    fn intercept(&mut self, index: usize, mut regs: RegisterSet) -> Result<()> {
        let number = get(&regs, 16)?;
        let pc = get(&regs, PC)?;
        match number {
            5 | 398 | 463 | 464 => {
                let s = &mut self.sessions[index];
                s.entry = Some(pc);
                self.events.push_back(TraceEvent::SyscallEntry {
                    task: s.id,
                    thread: s.thread,
                    registers: regs,
                });
            }
            2 => {
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
            7 | 400 => {
                let wanted = get(&regs, 0)? as i32;
                let options = get(&regs, 2)?;
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
                if !children.is_empty() && (wanted <= 0 && wanted != -1 || options & !1 != 0) {
                    return Err(unsupported(
                        "wait process-group/stopped/continued selection",
                    ));
                }
                let ready = children.iter().any(|i| self.sessions[*i].done);
                let s = &mut self.sessions[index];
                if !children.is_empty() && !ready && options & 1 != 0 {
                    set(&mut regs, 0, 0)?;
                    let flags = get(&regs, CPSR)?;
                    set(&mut regs, CPSR, flags & !(1 << 29))?;
                    set(&mut regs, PC, pc + 4)?;
                    s.set_regs(&regs)?;
                    s.continue_run()?;
                } else if !children.is_empty() && !ready {
                    s.single_thread()?;
                    s.waiting = children;
                } else {
                    s.return_stop(ReturnKind::Wait)?;
                }
            }
            59 | 244 => {
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
                    let version = cache::command(
                        std::process::Command::new("/usr/bin/sw_vers").arg("-productVersion"),
                        self.deadline.unwrap(),
                    )?;
                    if version.stdout != b"26.5.1\n" {
                        return Err(unsupported("spawn descriptor is pinned to macOS 26.5.1"));
                    }
                    if get(&regs, 2)? != 0 {
                        return Err(unsupported("spawn requires null attributes/file actions"));
                    }
                    let data = spawn_attributes()?;
                    let attr = s.allocate(&data)?;
                    let mut descriptor = vec![0; 144];
                    descriptor[..8].copy_from_slice(&248u64.to_le_bytes());
                    descriptor[8..16].copy_from_slice(&attr.to_le_bytes());
                    set(&mut regs, 2, s.allocate(&descriptor)?)?;
                    let mut pid_pointer = get(&regs, 0)?;
                    if pid_pointer == 0 {
                        pid_pointer = s.allocate(&[0; 4])?;
                        set(&mut regs, 0, pid_pointer)?;
                    }
                    s.set_regs(&regs)?;
                    s.return_stop(ReturnKind::Spawn { pid_pointer, twin })?;
                } else {
                    s.twin = twin;
                    s.set_regs(&regs)?;
                    s.return_stop(ReturnKind::Exec)?;
                }
            }
            _ => return Err(unsupported(format!("intercepted raw syscall {number}"))),
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
                    libc::waitpid(s.id.0.native_id as i32, std::ptr::null_mut(), libc::WNOHANG);
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
            s.pending = None;
            s.breaks.clear();
            s.scratch.clear();
            s.entry = None;
            s.exec_generation += 1;
            let task = Arc::new(Task::acquire(s.id.0.native_id as i32)?);
            self.watchdog.as_ref().unwrap().add(task.clone());
            s.task = task;
            // Debugserver removes obsolete address breakpoints on exec; resolve the new image.
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
        if s.pending.as_ref().is_some_and(|p| p.gate == pc) {
            return self.finish_return(index, regs);
        }
        if s.breaks.contains_key(&pc) {
            return self.intercept(index, regs);
        }
        let signal = rsp::number(&reply[1..3])? as i32;
        // Continuing with `c` (no C signal) suppresses these attach transients.
        if [
            libc::SIGHUP,
            libc::SIGTRAP,
            libc::SIGSTOP,
            libc::SIGCHLD,
            libc::SIGCONT,
            libc::SIGWINCH,
            libc::SIGURG,
            libc::SIGIO,
        ]
        .contains(&signal)
            || (interrupted && signal == libc::SIGINT)
        {
            s.continue_run()?;
            return Ok(());
        }
        Err(error(
            "tracee stop",
            format!("fatal signal/exception: {reply}"),
        ))
    }
}
fn spawn_attributes() -> Result<Vec<u8>> {
    unsafe {
        let mut attr = std::mem::zeroed();
        errno(libc::posix_spawnattr_init(&mut attr), "spawnattr_init")?;
        let result = (|| {
            errno(
                libc::posix_spawnattr_setflags(&mut attr, 0x80),
                "spawnattr_setflags",
            )?;
            // This private size is validated only on 26.5.1, checked before calling.
            let bytes = std::slice::from_raw_parts(attr as *const u8, 248).to_vec();
            if bytes[..2] != [0x80, 0] || bytes[192..].iter().any(|b| *b != 0) {
                return Err(unsupported("unexpected spawn attribute layout"));
            }
            Ok(bytes)
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}
impl TraceBackend for MacosTraceBackend {
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
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
                s.return_stop(ReturnKind::Open)
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
            capabilities: ["darwin-arm64-abi-v1".to_owned()].into_iter().collect(),
        }
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
