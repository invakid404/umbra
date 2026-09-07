//! Empty Linux backend precedent for the platform contracts.
//!
//! Capabilities report no support. Every runtime operation returns an explicit not-implemented error on every host.
//! Future Linux tracing and syscall ABI support belongs here (handoff §7).
//! This crate uses no native bindings and can be checked on macOS.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_core::{
    EmulatedResult, FsOp, LaunchSpec, PlatformCapabilities, PreparedRewrite, ProcessHandle,
    QuiescedTree, RegisterSet, Result, ResumeCommand, TaskId, TerminationPolicy, ThreadId,
    TraceEvent, UmbraError,
};
use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};

/// Placeholder for the future Linux tracing transport; launches no processes.
#[derive(Debug, Default)]
pub struct LinuxTraceBackend;

impl TraceBackend for LinuxTraceBackend {
    fn launch(&mut self, _spec: LaunchSpec) -> Result<ProcessHandle> {
        Err(UmbraError::not_implemented("linux: launch"))
    }

    fn next_event(&mut self) -> Result<TraceEvent> {
        Err(UmbraError::not_implemented("linux: next_event"))
    }

    fn read_memory(&mut self, _task: TaskId, _address: u64, _out: &mut [u8]) -> Result<()> {
        Err(UmbraError::not_implemented("linux: read_memory"))
    }

    fn write_memory(&mut self, _task: TaskId, _address: u64, _bytes: &[u8]) -> Result<()> {
        Err(UmbraError::not_implemented("linux: write_memory"))
    }

    fn registers(&mut self, _thread: ThreadId) -> Result<RegisterSet> {
        Err(UmbraError::not_implemented("linux: registers"))
    }

    fn set_registers(&mut self, _thread: ThreadId, _regs: &RegisterSet) -> Result<()> {
        Err(UmbraError::not_implemented("linux: set_registers"))
    }

    fn resume(&mut self, _command: ResumeCommand) -> Result<()> {
        Err(UmbraError::not_implemented("linux: resume"))
    }
}

impl TraceControl for LinuxTraceBackend {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }

    fn quiesce(&mut self, _process: ProcessHandle) -> Result<QuiescedTree> {
        Err(UmbraError::not_implemented("linux: quiesce"))
    }

    fn terminate(&mut self, _process: ProcessHandle, _policy: TerminationPolicy) -> Result<()> {
        Err(UmbraError::not_implemented("linux: terminate"))
    }
}

/// Placeholder for future Linux aarch64/x86-64 syscall decoding and register edits.
#[derive(Debug, Default)]
pub struct LinuxSyscallAbi;

impl SyscallAbi for LinuxSyscallAbi {
    fn decode_entry(
        &self,
        _regs: &RegisterSet,
        _memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>> {
        Err(UmbraError::not_implemented("linux: decode_entry"))
    }

    fn apply_rewrite(&self, _regs: &mut RegisterSet, _rewrite: &PreparedRewrite) -> Result<()> {
        Err(UmbraError::not_implemented("linux: apply_rewrite"))
    }

    fn emulate_result(&self, _regs: &mut RegisterSet, _result: &EmulatedResult) -> Result<()> {
        Err(UmbraError::not_implemented("linux: emulate_result"))
    }
}
