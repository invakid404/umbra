//! Stub macOS tracing transport and Darwin arm64 syscall ABI.
//!
//! These types reserve the Rust implementation boundary for the Python v2 tracer
//! in `experiments/tracer/umbra_tracer.py`. No tracing, syscall decoding, descendant
//! capture, instruction repair, or sandbox enforcement is implemented yet.
//! Runtime operations return explicit errors without touching their arguments;
//! capability reporting advertises no supported architectures or capabilities.
//! The stubs are available on all targets; native Mach types are macOS-only.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_core::{
    EmulatedResult, FsOp, LaunchSpec, PlatformCapabilities, PreparedRewrite, ProcessHandle,
    QuiescedTree, RegisterSet, Result, ResumeCommand, TaskId, TerminationPolicy, ThreadId,
    TraceEvent, UmbraError,
};
use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};

/// Native Mach port name reserved for the future tracing transport.
/// This alias conveys no ownership of a port right and invokes no Mach APIs.
#[cfg(target_os = "macos")]
pub type MachPort = mach2::port::mach_port_t;

/// Placeholder for the macOS tracing transport; construction acquires no resources.
#[derive(Debug, Default)]
pub struct MacosTraceBackend;

impl TraceBackend for MacosTraceBackend {
    fn launch(&mut self, _spec: LaunchSpec) -> Result<ProcessHandle> {
        Err(UmbraError::not_implemented("macos: launch"))
    }

    fn next_event(&mut self) -> Result<TraceEvent> {
        Err(UmbraError::not_implemented("macos: next_event"))
    }

    fn read_memory(&mut self, _task: TaskId, _address: u64, _out: &mut [u8]) -> Result<()> {
        Err(UmbraError::not_implemented("macos: read_memory"))
    }

    fn write_memory(&mut self, _task: TaskId, _address: u64, _bytes: &[u8]) -> Result<()> {
        Err(UmbraError::not_implemented("macos: write_memory"))
    }

    fn registers(&mut self, _thread: ThreadId) -> Result<RegisterSet> {
        Err(UmbraError::not_implemented("macos: registers"))
    }

    fn set_registers(&mut self, _thread: ThreadId, _regs: &RegisterSet) -> Result<()> {
        Err(UmbraError::not_implemented("macos: set_registers"))
    }

    fn resume(&mut self, _command: ResumeCommand) -> Result<()> {
        Err(UmbraError::not_implemented("macos: resume"))
    }
}

impl TraceControl for MacosTraceBackend {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }

    fn quiesce(&mut self, _process: ProcessHandle) -> Result<QuiescedTree> {
        Err(UmbraError::not_implemented("macos: quiesce"))
    }

    fn terminate(&mut self, _process: ProcessHandle, _policy: TerminationPolicy) -> Result<()> {
        Err(UmbraError::not_implemented("macos: terminate"))
    }
}

/// Placeholder for Darwin arm64 decoding, argument rewriting, and result encoding.
#[derive(Debug, Default)]
pub struct DarwinArm64Abi;

impl SyscallAbi for DarwinArm64Abi {
    fn decode_entry(
        &self,
        _regs: &RegisterSet,
        _memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>> {
        Err(UmbraError::not_implemented("macos: decode_entry"))
    }

    fn apply_rewrite(&self, _regs: &mut RegisterSet, _rewrite: &PreparedRewrite) -> Result<()> {
        Err(UmbraError::not_implemented("macos: apply_rewrite"))
    }

    fn emulate_result(&self, _regs: &mut RegisterSet, _result: &EmulatedResult) -> Result<()> {
        Err(UmbraError::not_implemented("macos: emulate_result"))
    }
}
