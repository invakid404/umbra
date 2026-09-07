use crate::{unsupported, Options};
use umbra_core::*;
use umbra_platform::{TraceBackend, TraceControl};
#[derive(Default)]
pub struct MacosTraceBackend;
impl MacosTraceBackend {
    pub fn new(_: Options) -> Self {
        Self
    }
    pub fn launch_experimental(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
        Err(unsupported("native macOS arm64 required"))
    }
}
impl TraceBackend for MacosTraceBackend {
    fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn resume(&mut self, _: ResumeCommand) -> Result<()> {
        Err(unsupported("native macOS arm64 required"))
    }
}
impl TraceControl for MacosTraceBackend {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }
    fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
        Err(unsupported("native macOS arm64 required"))
    }
    fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
        Err(unsupported("native macOS arm64 required"))
    }
}
