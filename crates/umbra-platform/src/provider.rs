//! Paired platform proxy with a multiplexed ABI memory callback lane.
//! While decode waits for memory, the server services nested read-only control
//! requests. The client releases its RefCell borrow before invoking TraceMemory,
//! so a callback may read through the paired control proxy without deadlocking.
use super::*;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};
use umbra_core::{
    provider::{self as wire, protocol_error, Client, Connection, Frame, ProviderDescriptor},
    MAX_IO_BYTES,
};

/// Platform control and ABI methods in protocol version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Capabilities.
    Capabilities,
    /// Launch.
    Launch(LaunchSpec),
    /// Next event.
    NextEvent,
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
        match call(&self.client, &Request::Launch(spec))? {
            Response::Process(v) => Ok(v),
            _ => Err(protocol_error("platform.launch response")),
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
        Request::Launch(spec) => control.launch(spec).map(Response::Process),
        Request::NextEvent => control.next_event().map(Response::Event),
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
        Request::Registers(thread) => control.registers(thread).map(Response::Registers),
        Request::SetRegisters { thread, regs } => control
            .set_registers(thread, &regs)
            .map(|()| Response::Unit),
        Request::Resume(command) => control.resume(command).map(|()| Response::Unit),
        Request::Quiesce(process) => control.quiesce(process).map(Response::Quiesced),
        Request::Terminate { process, policy } => {
            control.terminate(process, policy).map(|()| Response::Unit)
        }
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
    use umbra_core::{Architecture, TaskIdentity, UmbraError};
    struct FakeControl;
    impl TraceBackend for FakeControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            Err(UmbraError::not_implemented("fake.launch"))
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            Err(UmbraError::not_implemented("fake.event"))
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
            client: client.clone(),
            capabilities: Default::default(),
        };
        let abi = Abi {
            client: client.clone(),
        };
        let regs = RegisterSet::new(Architecture::Aarch64, vec![]).unwrap();
        let decoded = abi
            .decode_entry(&regs, &mut ReadThrough(&mut control))
            .unwrap();
        assert!(matches!(decoded, Some(FsOp::GetCwd)));
        assert!(control.next_event().is_err()); // A structured backend error does not poison the stream.
        drop(abi);
        drop(control);
        drop(client);
        assert!(worker.join().unwrap().is_err());
    }
}
