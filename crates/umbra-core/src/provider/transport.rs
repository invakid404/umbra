//! Private Unix socket transport with bounded frames, deadlines and fail-closed disconnects.
use super::*;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::{
    ffi::OsStrExt,
    net::{UnixListener, UnixStream},
};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn unavailable(error: impl std::fmt::Display) -> UmbraError {
    UmbraError::new(
        ErrorKind::StorageUnavailable,
        "provider.transport",
        error.to_string(),
    )
}

/// Ordered stream; a deadline covers the complete request including callbacks.
pub struct Connection {
    stream: UnixStream,
    deadline: Instant,
    timeout: Duration,
    failed: bool,
}
impl Connection {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(stream: UnixStream, timeout: Duration) -> Self {
        Self {
            stream,
            deadline: Instant::now() + timeout,
            timeout,
            failed: false,
        }
    }
    /// Current absolute deadline, retained by nested callback requests.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Restore an outer request deadline after a nested control request.
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
    /// Wait for an idle server's next request, then bound the remainder of its frame.
    pub fn receive_request<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let result = (|| {
            if self.failed {
                return Err(unavailable("connection is closed"));
            }
            self.stream.set_read_timeout(None).map_err(unavailable)?;
            let mut header = [0; 4];
            self.stream
                .read_exact(&mut header[..1])
                .map_err(unavailable)?;
            self.begin();
            self.read_exact(&mut header[1..])?;
            let len = u32::from_be_bytes(header) as usize;
            if len == 0 || len > MAX_FRAME_BYTES {
                return Err(protocol_error("invalid frame length"));
            }
            let mut bytes = vec![0; len];
            self.read_exact(&mut bytes)?;
            decode(&bytes)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    /// Begin.
    pub fn begin(&mut self) {
        self.deadline = Instant::now() + self.timeout;
    }
    fn remaining(&self) -> Result<Duration> {
        if self.failed {
            return Err(unavailable("connection is closed"));
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| unavailable("request deadline exceeded"))
    }
    /// Invalidate.
    pub fn invalidate(&mut self) {
        self.failed = true;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
    fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut offset = 0;
        while offset < out.len() {
            let remaining = self.remaining()?;
            let count = match self.stream.set_read_timeout(Some(remaining)) {
                Ok(()) => self.stream.read(&mut out[offset..]).map_err(unavailable)?,
                Err(timeout_error) => {
                    // macOS can reject SO_RCVTIMEO after a peer closes, despite a
                    // complete buffered reply. Drain only immediately available data;
                    // never turn failed timeout setup into an unbounded blocking read.
                    self.stream.set_nonblocking(true).map_err(unavailable)?;
                    let read = self.stream.read(&mut out[offset..]);
                    self.stream.set_nonblocking(false).map_err(unavailable)?;
                    read.map_err(|_| unavailable(timeout_error))?
                }
            };
            if count == 0 {
                return Err(unavailable("provider disconnected"));
            }
            offset += count;
        }
        Ok(())
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.stream
                .set_write_timeout(Some(self.remaining()?))
                .map_err(unavailable)?;
            let count = self.stream.write(&bytes[offset..]).map_err(unavailable)?;
            if count == 0 {
                return Err(unavailable("provider disconnected"));
            }
            offset += count;
        }
        Ok(())
    }
    /// Receive.
    pub fn receive<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let result = (|| {
            let mut header = [0; 4];
            self.read_exact(&mut header)?;
            let len = u32::from_be_bytes(header) as usize;
            if len == 0 || len > MAX_FRAME_BYTES {
                return Err(protocol_error("invalid frame length"));
            }
            let mut bytes = vec![0; len];
            self.read_exact(&mut bytes)?;
            decode(&bytes)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    /// Send.
    pub fn send<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let result = (|| {
            let bytes = encode(value)?;
            self.write_all(&(bytes.len() as u32).to_be_bytes())?;
            self.write_all(&bytes)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
}

/// Owns a child provider; losing the session closes IPC and terminates/reaps the child.
pub struct Client {
    /// Connection.
    pub connection: Connection,
    child: Option<Child>,
    next_id: u64,
    /// Welcome.
    pub welcome: Welcome,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.connection.invalidate();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Client {
    /// Attach an already handshaken private stream; intended for injected contract transports.
    /// The caller must establish identity, role, version and capability agreement first.
    pub fn from_connection(connection: Connection, welcome: Welcome) -> Self {
        Self {
            connection,
            child: None,
            next_id: 1,
            welcome,
        }
    }
    /// Validate provider registration and establish a private protocol session.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        descriptor.validate(&descriptor.role)?;
        if timeout_ms == 0 || timeout_ms > 60_000 {
            return Err(protocol_error("invalid timeout"));
        }
        let timeout = Duration::from_millis(timeout_ms);
        let directory = tempfile::Builder::new()
            .prefix("umbra-provider-")
            .tempdir()
            .map_err(unavailable)?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(unavailable)?;
        let socket = directory.path().join("ipc");
        let listener = UnixListener::bind(&socket).map_err(unavailable)?;
        listener.set_nonblocking(true).map_err(unavailable)?;
        let mut child = Command::new(std::ffi::OsStr::from_bytes(
            descriptor.executable.as_bytes(),
        ))
        .arg("--umbra-socket")
        .arg(&socket)
        .arg("--umbra-timeout-ms")
        .arg(timeout_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(unavailable)?;
        let deadline = Instant::now() + timeout;
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break Ok(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break Err(unavailable("provider connection timeout"));
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            break Err(unavailable(format!("provider exited: {status}")))
                        }
                        Err(e) => break Err(unavailable(e)),
                        _ => {}
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => break Err(unavailable(e)),
            }
        };
        let stream = match accepted {
            Ok(stream) => stream,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        // macOS may inherit O_NONBLOCK from the accepting listener.
        if let Err(error) = stream.set_nonblocking(false) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable(error));
        }
        let welcome = Welcome {
            id: descriptor.id.clone(),
            role: descriptor.role.clone(),
            version: PROTOCOL_VERSION,
            capabilities: Default::default(),
        };
        let mut client = Self {
            connection: Connection::new(stream, timeout),
            child: Some(child),
            next_id: 1,
            welcome,
        };
        client.connection.send(&Hello {
            id: descriptor.id.clone(),
            role: descriptor.role.clone(),
            version: descriptor.protocol_version,
            required_capabilities: descriptor.capabilities.clone(),
            options: descriptor.options.clone(),
        })?;
        client.welcome = client.connection.receive::<Result<Welcome>>()??;
        if client.welcome.id != descriptor.id
            || client.welcome.role != descriptor.role
            || client.welcome.version != PROTOCOL_VERSION
            || !descriptor
                .capabilities
                .is_subset(&client.welcome.capabilities)
        {
            return Err(protocol_error("provider handshake mismatch"));
        }
        tracing::debug!(provider = %descriptor.id, role = %descriptor.role, "connected provider");
        Ok(client)
    }
    /// Begin one request; callers serialize access, except documented callback nesting.
    pub fn start<T: Serialize>(&mut self, request: &T) -> Result<u64> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| protocol_error("request id exhausted"))?;
        self.connection.begin();
        self.connection.send(&Frame::Request {
            id,
            payload: encode(request)?,
        })?;
        Ok(id)
    }
    /// Call.
    pub fn call<Q: Serialize, R: serde::de::DeserializeOwned>(&mut self, request: &Q) -> Result<R> {
        let id = self.start(request)?;
        match self.connection.receive()? {
            Frame::Response { id: got, result } if id == got => {
                let decoded = decode(&result?);
                if decoded.is_err() {
                    self.connection.invalidate();
                }
                decoded
            }
            _ => {
                self.connection.invalidate();
                Err(protocol_error("unexpected response or request ID"))
            }
        }
    }
}

/// Accept only the private endpoint and explicit timeout passed by the registry client.
/// Constructs a backend only after validating the initial provider identity and role.
pub fn accept<B>(
    id: &str,
    role: &str,
    factory: impl FnOnce(&[u8]) -> Result<(B, std::collections::BTreeSet<String>)>,
) -> Result<(Connection, B)> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 || args[0] != "--umbra-socket" || args[2] != "--umbra-timeout-ms" {
        return Err(protocol_error(
            "expected private provider socket and timeout arguments",
        ));
    }
    let timeout_ms: u64 = args[3]
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0 && *n <= 60_000)
        .ok_or_else(|| protocol_error("invalid timeout"))?;
    let stream = UnixStream::connect(&args[1]).map_err(unavailable)?;
    let mut connection = Connection::new(stream, Duration::from_millis(timeout_ms));
    let hello: Hello = connection.receive()?;
    let result = (|| {
        if hello.id != id || hello.role != role || hello.version != PROTOCOL_VERSION {
            return Err(protocol_error("provider identity/role/version mismatch"));
        }
        let (backend, capabilities) = factory(&hello.options)?;
        if !hello.required_capabilities.is_subset(&capabilities) {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "provider.handshake",
                "required capabilities unavailable",
            ));
        }
        Ok((
            backend,
            Welcome {
                id: id.into(),
                role: role.into(),
                version: PROTOCOL_VERSION,
                capabilities,
            },
        ))
    })();
    match result {
        Ok((backend, welcome)) => {
            connection.send(&Ok::<_, UmbraError>(welcome))?;
            Ok((connection, backend))
        }
        Err(e) => {
            connection.send(&Err::<Welcome, _>(e.clone()))?;
            Err(e)
        }
    }
}

/// Generic role loop. One outstanding request provides backpressure; malformed frames close IPC.
pub fn serve<Q: serde::de::DeserializeOwned, R: Serialize>(
    mut connection: Connection,
    mut dispatch: impl FnMut(Q) -> Result<R>,
) -> Result<()> {
    let mut previous = 0;
    loop {
        connection.begin();
        let Frame::Request { id, payload } = connection.receive_request()? else {
            return Err(protocol_error("expected request"));
        };
        if id <= previous {
            return Err(protocol_error("reused or unordered request ID"));
        }
        previous = id;
        let result = decode(&payload)
            .and_then(&mut dispatch)
            .and_then(|response| encode(&response));
        connection.send(&Frame::Response { id, result })?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(timeout: Duration) -> (Connection, Connection) {
        let (left, right) = UnixStream::pair().unwrap();
        (
            Connection::new(left, timeout),
            Connection::new(right, timeout),
        )
    }
    fn welcome() -> Welcome {
        Welcome {
            id: "fake".into(),
            role: "test".into(),
            version: PROTOCOL_VERSION,
            capabilities: Default::default(),
        }
    }
    #[test]
    fn oversize_truncated_and_timed_out_frames_invalidate_connection() {
        let (mut receiver, mut sender) = pair(Duration::from_millis(30));
        sender
            .stream
            .write_all(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes())
            .unwrap();
        assert_eq!(
            receiver.receive::<Frame>().unwrap_err().kind,
            ErrorKind::ProtocolMismatch
        );
        assert!(receiver.receive::<Frame>().is_err());
        let (mut receiver, mut sender) = pair(Duration::from_millis(30));
        sender.stream.write_all(&[0, 0, 0, 20, b'{']).unwrap();
        drop(sender);
        assert_eq!(
            receiver.receive::<Frame>().unwrap_err().kind,
            ErrorKind::StorageUnavailable
        );
        let (mut receiver, _sender) = pair(Duration::from_millis(30));
        assert_eq!(
            receiver.receive::<Frame>().unwrap_err().kind,
            ErrorKind::StorageUnavailable
        );
    }
    #[test]
    fn response_ids_are_checked_and_disconnects_are_not_retried() {
        let (connection, mut server) = pair(Duration::from_secs(2));
        let worker = std::thread::spawn(move || {
            let Frame::Request { id, .. } = server.receive().unwrap() else {
                panic!()
            };
            server
                .send(&Frame::Response {
                    id: id + 1,
                    result: Ok(encode(&7u32).unwrap()),
                })
                .unwrap();
        });
        let mut client = Client::from_connection(connection, welcome());
        let error = client.call::<_, u32>(&1u32).unwrap_err();
        assert_eq!(error.kind, ErrorKind::ProtocolMismatch, "{error}");
        assert!(client.call::<_, u32>(&2u32).is_err());
        worker.join().unwrap();
    }
    #[test]
    fn server_rejects_reused_request_ids_before_dispatch() {
        let (mut client, server) = pair(Duration::from_secs(2));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let worker = std::thread::spawn(move || {
            serve(server, |value: u32| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(value)
            })
        });
        let request = Frame::Request {
            id: 1,
            payload: encode(&1u32).unwrap(),
        };
        client.send(&request).unwrap();
        let _: Frame = client.receive().unwrap();
        client.send(&request).unwrap();
        assert!(client.receive::<Frame>().is_err());
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind,
            ErrorKind::ProtocolMismatch
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn idle_server_waits_without_expiring_the_next_request() {
        let (mut client, mut server) = pair(Duration::from_millis(50));
        let worker = std::thread::spawn(move || server.receive_request::<u32>());
        std::thread::sleep(Duration::from_millis(75));
        client.begin();
        client.send(&42u32).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), 42);
    }
}
