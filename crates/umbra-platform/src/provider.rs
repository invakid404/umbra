//! Paired platform proxy with a multiplexed ABI memory callback lane.
//! While decode waits for memory, the server services nested read-only control
//! requests. The client releases its RefCell borrow before invoking TraceMemory,
//! so a callback may read through the paired control proxy without deadlocking.
use super::*;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};
use umbra_core::{
    provider::{self as wire, protocol_error, Client, Connection, Frame, ProviderDescriptor},
    ErrorKind, SandboxRequirement, UmbraError, MAX_IO_BYTES,
};

/// Platform control and ABI methods in protocol version 3.
/// Bounded polling is negotiated by capability; legacy messages remain supported.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Capabilities.
    Capabilities,
    /// Launch.
    Launch(LaunchSpec),
    /// Next event.
    NextEvent,
    /// Bounded event wait, in microseconds.
    PollEvent {
        /// Maximum wait in microseconds, from 1 through 100000.
        max_wait_us: u64,
    },
    /// Read memory.
    ReadMemory {
        /// Task.
        task: TaskId,
        /// Address.
        address: u64,
        /// Length in bytes.
        len: u32,
    },
    /// Write memory.
    WriteMemory {
        /// Task.
        task: TaskId,
        /// Address.
        address: u64,
        /// Owned bytes; no UTF-8 conversion is implied.
        bytes: Vec<u8>,
    },
    /// Registers.
    Registers(ThreadId),
    /// Syscall result copyout, respecting memory protection.
    CopyoutMemory {
        /// Stopped task.
        task: TaskId,
        /// Output address.
        address: u64,
        /// Result bytes.
        bytes: Vec<u8>,
    },
    /// Set registers.
    SetRegisters {
        /// Thread.
        thread: ThreadId,
        /// Regs.
        regs: RegisterSet,
    },
    /// Resume.
    Resume(ResumeCommand),
    /// Quiesce.
    Quiesce(ProcessHandle),
    /// Terminate.
    Terminate {
        /// Process.
        process: ProcessHandle,
        /// Policy.
        policy: TerminationPolicy,
    },
    /// Decode.
    Decode(RegisterSet),
    /// Rewrite.
    Rewrite {
        /// Regs.
        regs: RegisterSet,
        /// Rewrite.
        rewrite: PreparedRewrite,
    },
    /// Emulate.
    Emulate {
        /// Regs.
        regs: RegisterSet,
        /// Result.
        result: EmulatedResult,
    },
    /// Prepare rewrite.
    PrepareRewrite {
        /// Thread.
        thread: ThreadId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Operation.
        operation: FsOp,
    },
    /// Io buffer.
    IoBuffer(RegisterSet),
    /// Directory request block.
    DirectoryRequest(RegisterSet),
    /// Encode stat.
    EncodeStat(umbra_core::BlobStat),
    /// Encode logical filesystem metadata.
    EncodeFilesystemMetadata(umbra_core::FilesystemMetadata),
}
/// Owned method results, including buffers copied only after response validation.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Capabilities.
    Capabilities(PlatformCapabilities),
    /// Process.
    Process(ProcessHandle),
    /// Event.
    Event(TraceEvent),
    /// Bounded event outcome.
    EventPoll(EventPoll),
    /// Bytes.
    Bytes(Vec<u8>),
    /// Registers.
    Registers(RegisterSet),
    /// Unit.
    Unit,
    /// Quiesced.
    Quiesced(QuiescedTree),
    /// Decoded.
    Decoded(Option<FsOp>),
    /// Io buffer.
    IoBuffer(Option<umbra_core::IoBuffer>),
    /// Directory request block.
    DirectoryRequest(Option<umbra_core::IoBuffer>),
    /// Encoded stat.
    ///
    /// Distinct from [`Response::Bytes`], which is a memory read's reply: these
    /// bytes are an ABI encoding of metadata and were never in the tracee, and
    /// a shared variant would let one answer the other's request.
    EncodedStat(Vec<u8>),
    /// Encoded filesystem metadata, distinct from file-stat and memory replies.
    EncodedFilesystemMetadata(Vec<u8>),
    /// Prepared.
    Prepared(PreparedRewrite),
}
/// Callback bound to the stopped task by the caller's TraceMemory adapter.
#[derive(Serialize, Deserialize)]
pub struct MemoryRead {
    /// Address.
    pub address: u64,
    /// Length in bytes.
    pub len: u32,
}
struct Control {
    client: Rc<RefCell<Client>>,
    capabilities: PlatformCapabilities,
    timeout_ms: u64,
}
struct Abi {
    client: Rc<RefCell<Client>>,
}

/// Negotiate both proxies on one provider session; no architecture-based backend selection.
pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<PlatformSession> {
    descriptor.validate("platform")?;
    let mut client = Client::connect(descriptor, timeout_ms)?;
    let capabilities = match client.call(&Request::Capabilities)? {
        Response::Capabilities(v) => v,
        _ => return Err(protocol_error("platform.capabilities response")),
    };
    let client = Rc::new(RefCell::new(client));
    Ok(PlatformSession {
        control: Box::new(Control {
            client: client.clone(),
            capabilities,
            timeout_ms,
        }),
        abi: Box::new(Abi { client }),
    })
}
fn call(client: &RefCell<Client>, request: &Request) -> Result<Response> {
    client
        .try_borrow_mut()
        .map_err(|_| protocol_error("concurrent platform request"))?
        .call(request)
}
fn bounds(address: u64, len: usize) -> Result<()> {
    if len > MAX_IO_BYTES || address.checked_add(len as u64).is_none() {
        return Err(protocol_error("platform memory bounds"));
    }
    Ok(())
}
fn unit(response: Response) -> Result<()> {
    match response {
        Response::Unit => Ok(()),
        _ => Err(protocol_error("platform unit response")),
    }
}
impl TraceBackend for Control {
    fn launch(&mut self, spec: LaunchSpec) -> Result<ProcessHandle> {
        require_bounded_poll(&self.capabilities)?;
        match call(&self.client, &Request::Launch(spec))? {
            Response::Process(v) => Ok(v),
            _ => Err(protocol_error("platform.launch response")),
        }
    }
    fn poll_event(&mut self, max_wait: std::time::Duration) -> Result<EventPoll> {
        if !self
            .capabilities
            .capabilities
            .contains(umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1)
        {
            return Err(protocol_error("platform lacks bounded-event-poll-v1"));
        }
        let transport_us = self
            .timeout_ms
            .checked_mul(1000)
            .ok_or_else(|| protocol_error("poll timeout overflow"))?
            / 4;
        let micros = max_wait
            .as_micros()
            .min(100_000)
            .min(u128::from(transport_us));
        let max_wait_us =
            u64::try_from(micros).map_err(|_| protocol_error("poll timeout overflow"))?;
        validate_poll_wait(max_wait_us)?;
        match call(&self.client, &Request::PollEvent { max_wait_us })? {
            Response::EventPoll(outcome) => Ok(outcome),
            _ => Err(protocol_error("platform.poll_event response")),
        }
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        match call(&self.client, &Request::NextEvent)? {
            Response::Event(v) => Ok(v),
            _ => Err(protocol_error("platform.event response")),
        }
    }
    fn read_memory(&mut self, task: TaskId, address: u64, out: &mut [u8]) -> Result<()> {
        bounds(address, out.len())?;
        match call(
            &self.client,
            &Request::ReadMemory {
                task,
                address,
                len: out.len() as u32,
            },
        )? {
            Response::Bytes(bytes) if bytes.len() == out.len() => {
                out.copy_from_slice(&bytes);
                Ok(())
            }
            _ => Err(protocol_error("platform.read_memory response")),
        }
    }
    fn write_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()> {
        bounds(address, bytes.len())?;
        unit(call(
            &self.client,
            &Request::WriteMemory {
                task,
                address,
                bytes: bytes.to_vec(),
            },
        )?)
    }
    fn registers(&mut self, thread: ThreadId) -> Result<RegisterSet> {
        match call(&self.client, &Request::Registers(thread))? {
            Response::Registers(v) => Ok(v),
            _ => Err(protocol_error("platform.registers response")),
        }
    }
    fn copyout_memory(&mut self, task: TaskId, address: u64, bytes: &[u8]) -> Result<()> {
        bounds(address, bytes.len())?;
        unit(call(
            &self.client,
            &Request::CopyoutMemory {
                task,
                address,
                bytes: bytes.to_vec(),
            },
        )?)
    }
    fn set_registers(&mut self, thread: ThreadId, regs: &RegisterSet) -> Result<()> {
        unit(call(
            &self.client,
            &Request::SetRegisters {
                thread,
                regs: regs.clone(),
            },
        )?)
    }
    fn resume(&mut self, command: ResumeCommand) -> Result<()> {
        unit(call(&self.client, &Request::Resume(command))?)
    }
}
impl TraceControl for Control {
    fn capabilities(&self) -> PlatformCapabilities {
        self.capabilities.clone()
    }
    fn quiesce(&mut self, process: ProcessHandle) -> Result<QuiescedTree> {
        match call(&self.client, &Request::Quiesce(process))? {
            Response::Quiesced(v) => Ok(v),
            _ => Err(protocol_error("platform.quiesce response")),
        }
    }
    fn terminate(&mut self, process: ProcessHandle, policy: TerminationPolicy) -> Result<()> {
        unit(call(&self.client, &Request::Terminate { process, policy })?)
    }
    fn prepare_rewrite(
        &mut self,
        thread: ThreadId,
        path: &BytePath,
        operation: FsOp,
    ) -> Result<PreparedRewrite> {
        match call(
            &self.client,
            &Request::PrepareRewrite {
                thread,
                path: path.clone(),
                operation: operation.clone(),
            },
        )? {
            Response::Prepared(v)
                if v.operation.operation == operation
                    && v.operation.paths.as_slice()
                        == [umbra_core::PathRewrite {
                            operand: umbra_core::PathOperand::Path,
                            path: umbra_core::PhysicalPath(path.clone()),
                        }] =>
            {
                Ok(v)
            }
            _ => Err(protocol_error("platform.prepare_rewrite response")),
        }
    }
}
impl SyscallAbi for Abi {
    fn decode_entry(
        &self,
        regs: &RegisterSet,
        memory: &mut dyn TraceMemory,
    ) -> Result<Option<FsOp>> {
        let id = self
            .client
            .borrow_mut()
            .start(&Request::Decode(regs.clone()))?;
        let deadline = self.client.borrow().connection.deadline();
        let mut expected_token = 1;
        loop {
            self.client.borrow_mut().connection.set_deadline(deadline);
            let frame = { self.client.borrow_mut().connection.receive()? };
            match frame {
                Frame::Response { id: got, result } if got == id => {
                    return match wire::decode(&result?)? {
                        Response::Decoded(v) => Ok(v),
                        _ => Err(protocol_error("platform.decode response")),
                    };
                }
                Frame::Callback {
                    id: got,
                    token,
                    payload,
                } if got == id && token == expected_token && token <= 256 => {
                    expected_token += 1;
                    let result = (|| {
                        let read: MemoryRead = wire::decode(&payload)?;
                        bounds(read.address, read.len as usize)?;
                        let mut out = vec![0; read.len as usize];
                        // No connection borrow or mutex is held during user memory access.
                        memory.read(read.address, &mut out)?;
                        Ok(out)
                    })();
                    self.client
                        .borrow_mut()
                        .connection
                        .send(&Frame::CallbackResult { id, token, result })?;
                }
                _ => {
                    self.client.borrow_mut().connection.invalidate();
                    return Err(protocol_error("invalid ABI callback frame"));
                }
            }
        }
    }
    fn io_buffer(&self, regs: &RegisterSet) -> Result<Option<umbra_core::IoBuffer>> {
        // A plain call, not `decode_entry`'s callback loop: the buffer address is
        // in the registers the caller already holds, so answering needs no read
        // of the stopped task's memory.
        match call(&self.client, &Request::IoBuffer(regs.clone()))? {
            Response::IoBuffer(binding) => Ok(binding),
            _ => Err(protocol_error("platform.io_buffer response")),
        }
    }
    fn directory_request(&self, regs: &RegisterSet) -> Result<Option<umbra_core::IoBuffer>> {
        // A plain call for `io_buffer`'s reason: the block's address is in the
        // registers, so reporting where it is needs no read of the stopped task.
        // The *contents* are read by the caller, after its own descriptor test.
        match call(&self.client, &Request::DirectoryRequest(regs.clone()))? {
            Response::DirectoryRequest(binding) => Ok(binding),
            _ => Err(protocol_error("platform.directory_request response")),
        }
    }
    fn encode_stat(&self, stat: &umbra_core::BlobStat) -> Result<Vec<u8>> {
        // A plain call for the same reason `io_buffer` is one: the encoding is a
        // pure function of the metadata and touches no stopped task.
        match call(&self.client, &Request::EncodeStat(stat.clone()))? {
            Response::EncodedStat(bytes) if !bytes.is_empty() && bytes.len() <= MAX_IO_BYTES => {
                Ok(bytes)
            }
            Response::EncodedStat(_) => Err(protocol_error("platform.encode_stat length")),
            _ => Err(protocol_error("platform.encode_stat response")),
        }
    }
    fn encode_filesystem_metadata(
        &self,
        metadata: &umbra_core::FilesystemMetadata,
    ) -> Result<Vec<u8>> {
        match call(
            &self.client,
            &Request::EncodeFilesystemMetadata(metadata.clone()),
        )? {
            Response::EncodedFilesystemMetadata(bytes) if bytes.len() == 2168 => Ok(bytes),
            _ => Err(protocol_error(
                "platform.encode_filesystem_metadata response",
            )),
        }
    }
    fn apply_rewrite(&self, regs: &mut RegisterSet, rewrite: &PreparedRewrite) -> Result<()> {
        match call(
            &self.client,
            &Request::Rewrite {
                regs: regs.clone(),
                rewrite: rewrite.clone(),
            },
        )? {
            Response::Registers(updated) if updated.architecture() == regs.architecture() => {
                *regs = updated;
                Ok(())
            }
            _ => Err(protocol_error("platform.rewrite response")),
        }
    }
    fn emulate_result(&self, regs: &mut RegisterSet, result: &EmulatedResult) -> Result<()> {
        match call(
            &self.client,
            &Request::Emulate {
                regs: regs.clone(),
                result: result.clone(),
            },
        )? {
            Response::Registers(updated) if updated.architecture() == regs.architecture() => {
                *regs = updated;
                Ok(())
            }
            _ => Err(protocol_error("platform.emulate response")),
        }
    }
}
fn control_request(control: &mut dyn TraceControl, request: Request) -> Result<Response> {
    match request {
        Request::Capabilities => Ok(Response::Capabilities(control.capabilities())),
        Request::Launch(spec) => {
            if matches!(spec.sandbox, SandboxRequirement::UnsandboxedExperiment) {
                return Err(UmbraError::new(
                    ErrorKind::UnsupportedCapability,
                    "platform.launch",
                    "supervised launch requires an installed sandbox",
                ));
            }
            control.launch(spec).map(Response::Process)
        }
        Request::NextEvent => control.next_event().map(Response::Event),
        Request::PollEvent { max_wait_us } => control
            .poll_event(validate_poll_wait(max_wait_us)?)
            .map(Response::EventPoll),
        Request::ReadMemory { task, address, len } => {
            bounds(address, len as usize)?;
            let mut bytes = vec![0; len as usize];
            control.read_memory(task, address, &mut bytes)?;
            Ok(Response::Bytes(bytes))
        }
        Request::WriteMemory {
            task,
            address,
            bytes,
        } => {
            bounds(address, bytes.len())?;
            control
                .write_memory(task, address, &bytes)
                .map(|()| Response::Unit)
        }
        Request::CopyoutMemory {
            task,
            address,
            bytes,
        } => {
            bounds(address, bytes.len())?;
            control
                .copyout_memory(task, address, &bytes)
                .map(|()| Response::Unit)
        }
        Request::Registers(thread) => control.registers(thread).map(Response::Registers),
        Request::SetRegisters { thread, regs } => control
            .set_registers(thread, &regs)
            .map(|()| Response::Unit),
        Request::Resume(command) => control.resume(command).map(|()| Response::Unit),
        Request::Quiesce(process) => control.quiesce(process).map(Response::Quiesced),
        Request::Terminate { process, policy } => {
            control.terminate(process, policy).map(|()| Response::Unit)
        }
        Request::PrepareRewrite {
            thread,
            path,
            operation,
        } => control
            .prepare_rewrite(thread, &path, operation)
            .map(Response::Prepared),
        _ => Err(protocol_error("ABI request in control lane")),
    }
}
struct Memory<'a> {
    connection: &'a mut Connection,
    control: &'a mut dyn TraceControl,
    previous: &'a mut u64,
    id: u64,
    token: u64,
}
impl TraceMemory for Memory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        bounds(address, out.len())?;
        self.token += 1;
        if self.token > 256 {
            return Err(protocol_error("ABI callback budget exceeded"));
        }
        self.connection.send(&Frame::Callback {
            id: self.id,
            token: self.token,
            payload: wire::encode(&MemoryRead {
                address,
                len: out.len() as u32,
            })?,
        })?;
        loop {
            match self.connection.receive()? {
                Frame::CallbackResult { id, token, result }
                    if id == self.id && token == self.token =>
                {
                    let bytes = result?;
                    if bytes.len() != out.len() {
                        return Err(protocol_error("callback read length mismatch"));
                    }
                    out.copy_from_slice(&bytes);
                    return Ok(());
                }
                Frame::Request { id, payload } if id > *self.previous => {
                    *self.previous = id;
                    let request = wire::decode(&payload)?;
                    // Only reads may interleave with decoding a stopped task.
                    if !matches!(
                        request,
                        Request::ReadMemory { .. } | Request::Registers(_) | Request::Capabilities
                    ) {
                        return Err(protocol_error("mutating request during ABI callback"));
                    }
                    let result =
                        control_request(self.control, request).and_then(|r| wire::encode(&r));
                    self.connection.send(&Frame::Response { id, result })?;
                }
                _ => return Err(protocol_error("invalid callback reply")),
            }
        }
    }
}
/// Own tracing and ABI on this thread. On connection loss, request immediate tree termination.
/// Backends must independently qualify enforcement on controller/provider death.
pub fn serve_provider(
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<PlatformSession>,
) -> Result<()> {
    let (connection, platform) = wire::accept(id, "platform", |options| {
        let platform = factory(options)?;
        let capabilities = platform.control.capabilities().capabilities;
        Ok((platform, capabilities))
    })?;
    serve_session(connection, platform)
}

/// Run the provider handshake and dispatcher on an injected private connection.
/// This is the socketpair equivalent of `serve_provider`'s process-argument transport.
pub fn serve_provider_on(
    connection: Connection,
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<PlatformSession>,
) -> Result<()> {
    let (connection, platform) = wire::accept_connection(connection, id, "platform", |options| {
        let platform = factory(options)?;
        let capabilities = platform.control.capabilities().capabilities;
        Ok((platform, capabilities))
    })?;
    serve_session(connection, platform)
}

fn serve_session(mut connection: Connection, mut platform: PlatformSession) -> Result<()> {
    let mut roots = Vec::new();
    let result = (|| {
        let mut previous = 0;
        loop {
            connection.begin();
            let Frame::Request { id, payload } = connection.receive_request()? else {
                return Err(protocol_error("expected platform request"));
            };
            if id <= previous {
                return Err(protocol_error("unordered platform request"));
            }
            previous = id;
            let result = match wire::decode(&payload)? {
                Request::Decode(regs) => {
                    let mut memory = Memory {
                        connection: &mut connection,
                        control: &mut *platform.control,
                        previous: &mut previous,
                        id,
                        token: 0,
                    };
                    platform
                        .abi
                        .decode_entry(&regs, &mut memory)
                        .map(Response::Decoded)
                }
                Request::IoBuffer(regs) => platform.abi.io_buffer(&regs).map(Response::IoBuffer),
                Request::DirectoryRequest(regs) => platform
                    .abi
                    .directory_request(&regs)
                    .map(Response::DirectoryRequest),
                Request::EncodeStat(stat) => {
                    platform.abi.encode_stat(&stat).map(Response::EncodedStat)
                }
                Request::EncodeFilesystemMetadata(metadata) => platform
                    .abi
                    .encode_filesystem_metadata(&metadata)
                    .map(Response::EncodedFilesystemMetadata),
                Request::Rewrite { mut regs, rewrite } => platform
                    .abi
                    .apply_rewrite(&mut regs, &rewrite)
                    .map(|()| Response::Registers(regs)),
                Request::Emulate { mut regs, result } => platform
                    .abi
                    .emulate_result(&mut regs, &result)
                    .map(|()| Response::Registers(regs)),
                request => control_request(&mut *platform.control, request),
            };
            if let Ok(Response::Process(process)) = &result {
                roots.push(*process);
            }
            connection.send(&Frame::Response {
                id,
                result: result.and_then(|r| wire::encode(&r)),
            })?;
        }
    })();
    for process in roots {
        let _ = platform
            .control
            .terminate(process, TerminationPolicy::Immediate);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};
    use umbra_core::{Architecture, TaskIdentity};

    #[test]
    fn prepared_response_must_match_the_requested_operation_and_single_path() {
        use umbra_core::{PathOperand, PathRewrite, PhysicalOperation, PhysicalPath};
        for mismatch in 0..6 {
            let path = BytePath::new(b"/shadow/file".to_vec()).unwrap();
            let operation = FsOp::GetCwd;
            let requested_path = path.clone();
            let requested_operation = operation.clone();
            let (left, right) = UnixStream::pair().unwrap();
            let worker = std::thread::spawn(move || {
                let mut connection = Connection::new(right, Duration::from_secs(2));
                connection.begin();
                let Frame::Request { id, payload } = connection.receive_request().unwrap() else {
                    panic!("request");
                };
                let Request::PrepareRewrite {
                    path, operation, ..
                } = wire::decode(&payload).unwrap()
                else {
                    panic!("prepare");
                };
                assert_eq!(path, requested_path);
                assert_eq!(operation, requested_operation);
                let mut plan = PreparedRewrite {
                    operation: PhysicalOperation {
                        operation,
                        paths: vec![PathRewrite {
                            operand: PathOperand::Path,
                            path: PhysicalPath(path),
                        }],
                    },
                    arguments: vec![],
                    memory_writes: vec![],
                };
                match mismatch {
                    0 => {}
                    1 => {
                        plan.operation.operation = FsOp::Close {
                            fd: umbra_core::TracedFd(9),
                        }
                    }
                    2 => {
                        plan.operation.paths[0].path =
                            PhysicalPath(BytePath::new(b"/wrong".to_vec()).unwrap())
                    }
                    3 => plan.operation.paths.clear(),
                    4 => plan.operation.paths.push(plan.operation.paths[0].clone()),
                    5 => plan.operation.paths[0].operand = PathOperand::Source,
                    _ => unreachable!(),
                }
                connection
                    .send(&Frame::Response {
                        id,
                        result: wire::encode(&Response::Prepared(plan)),
                    })
                    .unwrap();
            });
            let client = Rc::new(RefCell::new(Client::from_connection(
                Connection::new(left, Duration::from_secs(2)),
                wire::Welcome {
                    id: "fake".into(),
                    role: "platform".into(),
                    version: wire::PROTOCOL_VERSION,
                    capabilities: Default::default(),
                },
            )));
            let mut control = Control {
                timeout_ms: 5000,
                client,
                capabilities: Default::default(),
            };
            let result = control.prepare_rewrite(
                ThreadId(TaskIdentity {
                    native_id: 1,
                    generation: 1,
                }),
                &path,
                operation,
            );
            if mismatch == 0 {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().kind, ErrorKind::ProtocolMismatch);
            }
            worker.join().unwrap();
        }
    }
    struct FakeControl;
    impl TraceBackend for FakeControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            Err(UmbraError::not_implemented("fake.launch"))
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            Err(UmbraError::not_implemented("fake.event"))
        }
        fn poll_event(&mut self, _: std::time::Duration) -> Result<crate::EventPoll> {
            Err(umbra_core::UmbraError::not_implemented("poll_event"))
        }
        fn read_memory(&mut self, _: TaskId, address: u64, out: &mut [u8]) -> Result<()> {
            assert_eq!(address, 100);
            out.copy_from_slice(b"test");
            Ok(())
        }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
            panic!("callback must not mutate")
        }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
            Err(UmbraError::not_implemented("fake.registers"))
        }
        fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
            panic!("callback must not mutate")
        }
        fn resume(&mut self, _: ResumeCommand) -> Result<()> {
            panic!("callback must not resume")
        }
    }
    impl TraceControl for FakeControl {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities::default()
        }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
            Err(UmbraError::not_implemented("fake.quiesce"))
        }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
            Ok(())
        }
    }
    struct FakeAbi;
    impl SyscallAbi for FakeAbi {
        fn decode_entry(
            &self,
            _: &RegisterSet,
            memory: &mut dyn TraceMemory,
        ) -> Result<Option<FsOp>> {
            let mut bytes = [0; 4];
            memory.read(100, &mut bytes)?;
            assert_eq!(&bytes, b"test");
            // Exercise multiple callbacks and nested request ID ordering.
            memory.read(100, &mut bytes)?;
            Ok(Some(FsOp::GetCwd))
        }
        fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
            Err(UmbraError::not_implemented("fake.rewrite"))
        }
        fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> {
            Err(UmbraError::not_implemented("fake.emulate"))
        }
    }
    struct ReadThrough<'a>(&'a mut dyn TraceControl);
    impl TraceMemory for ReadThrough<'_> {
        fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
            self.0.read_memory(
                TaskId(TaskIdentity {
                    native_id: 1,
                    generation: 1,
                }),
                address,
                out,
            )
        }
    }
    #[test]
    fn abi_callbacks_can_use_the_paired_control_proxy_without_deadlock() {
        let (left, right) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            serve_session(
                Connection::new(right, Duration::from_secs(2)),
                PlatformSession {
                    control: Box::new(FakeControl),
                    abi: Box::new(FakeAbi),
                },
            )
        });
        let client = Rc::new(RefCell::new(Client::from_connection(
            Connection::new(left, Duration::from_secs(2)),
            wire::Welcome {
                id: "fake".into(),
                role: "platform".into(),
                version: wire::PROTOCOL_VERSION,
                capabilities: Default::default(),
            },
        )));
        let mut control = Control {
            timeout_ms: 5000,
            client: client.clone(),
            capabilities: PlatformCapabilities {
                capabilities: [umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.into()]
                    .into(),
                ..Default::default()
            },
        };
        let abi = Abi {
            client: client.clone(),
        };
        let regs = RegisterSet::new(Architecture::Aarch64, vec![]).unwrap();
        let decoded = abi
            .decode_entry(&regs, &mut ReadThrough(&mut control))
            .unwrap();
        assert!(matches!(decoded, Some(FsOp::GetCwd)));
        let refused = control
            .launch(LaunchSpec {
                executable: umbra_core::BytePath::new(b"/bin/true".to_vec()).unwrap(),
                argv: vec![b"/bin/true".to_vec()],
                environment: vec![],
                cwd: umbra_core::BytePath::new(b"/".to_vec()).unwrap(),
                policy: umbra_core::LaunchPolicy {
                    persistence: umbra_core::PersistencePolicy::LocalDevelopment,
                    inherited_fds: vec![],
                    // Not a routed launch: no interposer, no descriptor fence.
                    interpose: false,
                    descriptor_limit: None,
                },
                sandbox: SandboxRequirement::UnsandboxedExperiment,
            })
            .unwrap_err();
        // FakeControl would return NotImplemented if dispatch reached it.
        assert_eq!(refused.kind, ErrorKind::UnsupportedCapability);
        assert!(control.next_event().is_err()); // A structured backend error does not poison the stream.
        drop(abi);
        drop(control);
        drop(client);
        assert!(worker.join().unwrap().is_err());
    }
}

fn require_bounded_poll(capabilities: &PlatformCapabilities) -> Result<()> {
    if capabilities
        .capabilities
        .contains(umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1)
    {
        Ok(())
    } else {
        Err(protocol_error("platform lacks bounded-event-poll-v1"))
    }
}

fn validate_poll_wait(micros: u64) -> Result<std::time::Duration> {
    if !(1..=100_000).contains(&micros) {
        return Err(protocol_error(
            "poll wait must be in 1..=100000 microseconds",
        ));
    }
    Ok(std::time::Duration::from_micros(micros))
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};

    struct PollControl(bool);
    impl TraceBackend for PollControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            Err(UmbraError::not_implemented("fake.launch"))
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            Ok(TraceEvent::Exit {
                task: TaskId(umbra_core::TaskIdentity {
                    native_id: 7,
                    generation: 2,
                }),
                status: umbra_core::ExitStatus::Code(0),
            })
        }
        fn poll_event(&mut self, _: std::time::Duration) -> Result<crate::EventPoll> {
            Ok(if self.0 {
                self.0 = false;
                EventPoll::Idle
            } else {
                EventPoll::Event(self.next_event()?)
            })
        }
        fn read_memory(&mut self, _: TaskId, address: u64, out: &mut [u8]) -> Result<()> {
            assert_eq!(address, 100);
            out.copy_from_slice(b"test");
            Ok(())
        }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
            panic!("callback must not mutate")
        }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
            Err(UmbraError::not_implemented("fake.registers"))
        }
        fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
            panic!("callback must not mutate")
        }
        fn resume(&mut self, _: ResumeCommand) -> Result<()> {
            panic!("callback must not resume")
        }
    }
    impl TraceControl for PollControl {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities::default()
        }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
            Err(UmbraError::not_implemented("fake.quiesce"))
        }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn dispatch_preserves_legacy_event_and_new_idle_then_event() {
        let mut control = PollControl(true);
        let legacy: Request = wire::decode(br#""NextEvent""#).unwrap();
        let response = control_request(&mut control, legacy).unwrap();
        assert!(matches!(response, Response::Event(TraceEvent::Exit { .. })));
        assert!(String::from_utf8(wire::encode(&response).unwrap())
            .unwrap()
            .starts_with("{\"Event\":"));
        assert!(matches!(
            control_request(&mut control, Request::PollEvent { max_wait_us: 1 }).unwrap(),
            Response::EventPoll(EventPoll::Idle)
        ));
        assert!(matches!(
            control_request(&mut control, Request::PollEvent { max_wait_us: 1 }).unwrap(),
            Response::EventPoll(EventPoll::Event(TraceEvent::Exit { .. }))
        ));
        assert!(control_request(&mut control, Request::PollEvent { max_wait_us: 0 }).is_err());
    }

    fn proxy(stream: UnixStream, supported: bool, timeout_ms: u64) -> Control {
        let capabilities = if supported {
            [umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.to_owned()].into()
        } else {
            Default::default()
        };
        Control {
            client: Rc::new(RefCell::new(Client::from_connection(
                Connection::new(stream, Duration::from_millis(timeout_ms)),
                wire::Welcome {
                    id: "test".into(),
                    role: "platform".into(),
                    version: wire::PROTOCOL_VERSION,
                    capabilities: Default::default(),
                },
            ))),
            capabilities: PlatformCapabilities {
                capabilities,
                ..Default::default()
            },
            timeout_ms,
        }
    }

    #[test]
    fn launch_refuses_missing_runtime_support_even_if_handshake_advertised_it() {
        use std::io::Read;
        for advertised in [false, true] {
            let (left, mut right) = UnixStream::pair().unwrap();
            let mut control = proxy(left, false, 1000);
            if advertised {
                control
                    .client
                    .borrow_mut()
                    .welcome
                    .capabilities
                    .insert(umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.into());
            }
            let spec = LaunchSpec {
                executable: BytePath::new(b"/usr/bin/true".to_vec()).unwrap(),
                argv: vec![],
                environment: vec![],
                cwd: BytePath::new(b"/".to_vec()).unwrap(),
                policy: umbra_core::LaunchPolicy {
                    persistence: umbra_core::PersistencePolicy::LocalDevelopment,
                    inherited_fds: vec![],
                    interpose: false,
                    descriptor_limit: None,
                },
                sandbox: umbra_core::SandboxRequirement::UnsandboxedExperiment,
            };
            assert!(control
                .launch(spec)
                .unwrap_err()
                .to_string()
                .contains("bounded-event-poll-v1"));
            right.set_nonblocking(true).unwrap();
            assert_eq!(
                right.read(&mut [0u8]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn poll_wire_preserves_legacy_and_rejects_invalid_budgets() {
        assert_eq!(
            wire::encode(&Request::NextEvent).unwrap(),
            br#""NextEvent""#
        );
        for invalid in [0, 100001, u64::MAX] {
            assert!(validate_poll_wait(invalid).is_err());
        }
        for valid in [1, 250, 100000] {
            assert_eq!(
                validate_poll_wait(valid).unwrap(),
                Duration::from_micros(valid)
            );
        }
        assert!(wire::decode::<Response>(br#"{"EventPoll":"unknown"}"#).is_err());
    }

    #[test]
    fn poll_capability_and_concurrent_access_fail_without_sending() {
        let (left, mut right) = UnixStream::pair().unwrap();
        let mut control = proxy(left, false, 1000);
        assert_eq!(
            control
                .poll_event(Duration::from_millis(1))
                .unwrap_err()
                .kind,
            ErrorKind::ProtocolMismatch
        );
        control
            .capabilities
            .capabilities
            .insert(umbra_core::capabilities::PLATFORM_BOUNDED_EVENT_POLL_V1.into());
        let shared = control.client.clone();
        let _borrow = shared.borrow_mut();
        assert_eq!(
            control
                .poll_event(Duration::from_millis(1))
                .unwrap_err()
                .kind,
            ErrorKind::ProtocolMismatch
        );
        right.set_nonblocking(true).unwrap();
        use std::io::Read;
        assert_eq!(
            right.read(&mut [0u8]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn poll_proxy_bounds_wait_and_receives_idle() {
        let (left, right) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut connection = Connection::new(right, Duration::from_secs(2));
            let Frame::Request { id, payload } = connection.receive_request().unwrap() else {
                panic!("request")
            };
            assert!(matches!(
                wire::decode::<Request>(&payload).unwrap(),
                Request::PollEvent { max_wait_us: 50000 }
            ));
            connection
                .send(&Frame::Response {
                    id,
                    result: wire::encode(&Response::EventPoll(EventPoll::Idle)),
                })
                .unwrap();
        });
        assert!(matches!(
            proxy(left, true, 200)
                .poll_event(Duration::from_secs(1))
                .unwrap(),
            EventPoll::Idle
        ));
        worker.join().unwrap();
    }

    #[test]
    fn silent_and_disconnecting_poll_peers_fail_closed() {
        for disconnect in [false, true] {
            let (left, right) = UnixStream::pair().unwrap();
            let (received_tx, received_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut connection = Connection::new(right, Duration::from_secs(2));
                assert!(matches!(
                    connection.receive_request().unwrap(),
                    Frame::Request { .. }
                ));
                received_tx.send(()).unwrap();
                if !disconnect {
                    release_rx.recv().unwrap();
                }
            });
            let mut control = proxy(left, true, 100);
            let error = control.poll_event(Duration::from_millis(5)).unwrap_err();
            received_rx.recv().unwrap();
            assert_eq!(error.kind, ErrorKind::StorageUnavailable);
            if !disconnect {
                assert!(error.to_string().contains("request deadline exceeded"));
                release_tx.send(()).unwrap();
            }
            worker.join().unwrap();
            assert!(control.poll_event(Duration::from_millis(5)).is_err());
        }
    }
}
