//! Synchronous, platform-independent tracing and syscall ABI contracts.
//!
//! Shared value types belong to [`umbra_core`]. Native bindings, enforcement,
//! descendant capture, and instruction repair belong to backend crates. None of
//! these traits requires `Send`: construct thread-affine backends on the dedicated
//! debug-control thread, and require `+ Send` only at actual thread boundaries.
//!
//! A provider factory supplies [`PlatformSession`] with control and ABI objects
//! negotiated for the same session and architecture. Constructors are outside the
//! runtime trait surface. Consumers of control should use `T: TraceControl + ?Sized`
//! when they also need tracing methods, without relying on trait-object upcasting.
//!
//! All runtime contracts are dyn-compatible:
//!
//! ```
//! use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};
//!
//! fn accept_contracts(
//!     _: &mut dyn TraceBackend,
//!     _: &mut dyn TraceControl,
//!     _: &dyn SyscallAbi,
//!     _: &mut dyn TraceMemory,
//! ) {}
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use umbra_core::{
    BytePath, EmulatedResult, FsOp, LaunchSpec, PlatformCapabilities, PreparedRewrite,
    ProcessHandle, QuiescedTree, RegisterSet, Result, ResumeCommand, TaskId, TerminationPolicy,
    ThreadId, TraceEvent,
};

/// Owns the tracing transport and process-tree interception boundary.
///
/// Implementations capture descendants before their first mutation, re-arm tracing
/// after exec, and own sandbox installation, signal forwarding, and instruction
/// repair. Failure must never permit an uncontrolled host-filesystem mutation.
pub trait TraceBackend {
    /// Launch under tracing, with enforcement and inherited-fd sanitation installed
    /// before unrestricted execution. Apply the policy in `spec` before resuming.
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle>;

    /// Report normalized syscall entry/exit and child/fork/exec/exit notifications.
    ///
    /// Platform-specific return registers must become normalized syscall outcomes.
    /// Use bounded polling or provider timeouts so lease renewal and control work
    /// cannot be starved by an indefinitely blocked event read.
    fn next_event(&mut self) -> Result<TraceEvent>;

    /// Read exactly `out.len()` bytes from a stopped task, or return an error.
    /// Check address/length bounds and preserve bytes without UTF-8 conversion.
    fn read_memory(&mut self, task: TaskId, address: u64, out: &mut [u8]) -> Result<()>;

    /// Write bytes to a stopped task after validating address/length bounds.
    /// An error must leave the task stopped; it need not imply an atomic write.
    fn write_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()>;

    /// Read bounded, architecture-tagged registers from a stopped thread.
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet>;

    /// Set registers on a stopped thread, rejecting unsupported architectures.
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()>;

    /// Resume according to the requested disposition and signal policy.
    fn resume(&mut self, command: ResumeCommand) -> Result<()>;
}

/// Read-only memory access bound by an adapter to the stopped task being decoded.
///
/// The ABI does not need the task identity or tracing implementation. The adapter
/// must not outlive the stop that makes its memory reads valid. Over IPC, owned
/// reply bytes are copied into `out`; no Rust references or OS pointers cross it.
pub trait TraceMemory {
    /// Fill `out` with exactly the requested bytes, or return an error.
    /// Validate pointer arithmetic and length bounds, preserving non-UTF-8 paths.
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()>;
}

/// Decode and modify the negotiated syscall ABI without owning tracing transport.
pub trait SyscallAbi {
    /// Decode an entry, using task-bound memory for bounded pointer reads.
    ///
    /// `None` means a positively classified non-filesystem operation, never an
    /// unknown syscall. Unclassified potential mutations and unsupported
    /// architectures must return a structured unsupported or denied error.
    fn decode_entry(
        &self,
        regs: &RegisterSet,
        memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>>;

    /// Apply an already prepared rewrite to registers for this ABI.
    fn apply_rewrite(&self, regs: &mut RegisterSet, rewrite: &PreparedRewrite) -> Result<()>;

    /// Encode the normalized emulated result using this ABI's return conventions.
    fn emulate_result(&self, regs: &mut RegisterSet, result: &EmulatedResult) -> Result<()>;
}

/// Process-tree lifecycle operations required for checkpointing and safe shutdown.
pub trait TraceControl: TraceBackend {
    /// Report only capabilities qualified on the actual OS/architecture setup.
    fn capabilities(&self) -> PlatformCapabilities;

    /// Reject new children/mutations and stop the entire tree at known transaction
    /// boundaries. Return only once that quiescence has been established.
    fn quiesce(&mut self, process: ProcessHandle) -> Result<QuiescedTree>;

    /// Terminate the supervised tree according to policy while retaining enforcement.
    /// Controller loss must keep tracees stopped or terminate them safely.
    fn terminate(&mut self, process: ProcessHandle, policy: TerminationPolicy) -> Result<()>;

    /// Prepare an executable path rewrite for a stopped thread's current syscall.
    ///
    /// The namespace has already journaled preparation and, where required, has
    /// created or copied up the target; `operation` is therefore the *prepared*
    /// operation, whose flags may differ from the ones the tracee issued. The
    /// backend allocates bounded scratch memory in the tracee, validates the path
    /// operand slots for this ABI, and returns the argument and memory writes that
    /// would redirect the call. It performs none of those writes and resumes
    /// nothing: this is a plan, not an applied rewrite.
    ///
    /// Reject multi-path operations and operand slots the backend cannot address.
    /// The default refuses, so a backend without a qualified scratch mechanism
    /// cannot be mistaken for one that has it.
    fn prepare_rewrite(
        &mut self,
        _thread: ThreadId,
        _path: &BytePath,
        _operation: FsOp,
    ) -> Result<PreparedRewrite> {
        Err(umbra_core::UmbraError::new(
            umbra_core::ErrorKind::UnsupportedCapability,
            "platform.prepare_rewrite",
            "backend cannot prepare syscall path rewrites",
        ))
    }
}

/// Paired objects returned by a platform factory after session/ABI negotiation.
///
/// The factory must bind both objects to the same provider session; this container
/// alone cannot validate that relationship. Native constructors and provider
/// connection establishment live outside the runtime traits.
pub struct PlatformSession {
    /// Control.
    pub control: Box<dyn TraceControl>,
    /// Abi.
    pub abi: Box<dyn SyscallAbi>,
}

const _: Option<&dyn TraceBackend> = None;
const _: Option<&dyn TraceControl> = None;
const _: Option<&dyn SyscallAbi> = None;
const _: Option<&dyn TraceMemory> = None;

/// Versioned provider protocol, server harness and trait proxies.
#[cfg(unix)]
pub mod provider;
