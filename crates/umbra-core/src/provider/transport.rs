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

/// A read or write that expired against its `SO_{RCV,SND}TIMEO` deadline surfaces as
/// `WouldBlock` (EAGAIN, `os error 35`) or, on some platforms, `TimedOut`. Both are
/// the request deadline being missed, not a distinct failure, so callers report them
/// as such rather than leaking the raw errno.
fn is_deadline_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Ordered stream; the per-request deadline is the authoritative bound on the whole
/// request, including callbacks. Storage backends size their in-backend bounds to fit
/// inside the registry default (`DEFAULT_TIMEOUT_MS`) so their own outcome surfaces
/// before this deadline; a miss invalidates the connection.
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
                Ok(()) => match self.stream.read(&mut out[offset..]) {
                    Ok(count) => count,
                    // The read hit its deadline: report the miss, not the raw errno.
                    Err(ref e) if is_deadline_timeout(e) => {
                        return Err(unavailable("request deadline exceeded"))
                    }
                    Err(e) => return Err(unavailable(e)),
                },
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
            let count = match self.stream.write(&bytes[offset..]) {
                Ok(count) => count,
                // The write hit its deadline: report the miss, not the raw errno.
                Err(ref e) if is_deadline_timeout(e) => {
                    return Err(unavailable("request deadline exceeded"))
                }
                Err(e) => return Err(unavailable(e)),
            };
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

/// How long a caller waits on its own thread for a killed provider to be reaped
/// before handing the wait to a detached thread. A healthy provider exits in well
/// under 5ms; past this the reap moves off the caller so an uninterruptible kernel
/// wait can never block a drop or a connect-failure path.
const REAP_GRACE: Duration = Duration::from_millis(100);

/// Seam over the parts of [`std::process::Child`] the reaper uses, so tests can
/// supply a child whose `wait` never returns without spawning a real process.
trait Reap: Send + 'static {
    fn kill(&mut self) -> std::io::Result<()>;
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus>;
}
impl Reap for Child {
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        Child::try_wait(self)
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        Child::wait(self)
    }
}

/// Outcome of [`reap`], observable by tests. The product callers ignore it.
#[derive(Debug, PartialEq, Eq)]
enum Reaped {
    /// The child was reaped synchronously within the grace window.
    Exited,
    /// The child outlived the grace window; its `wait` moved to a detached thread.
    Detached,
    /// The reaper thread could not be spawned; the child was dropped unreaped.
    Abandoned,
}

/// SIGKILL a provider and reap it without blocking the caller past `grace`.
///
/// Poll `try_wait` for up to `grace` (the same 5ms idiom as [`Client::connect`]'s
/// accept loop). A healthy provider exits at once, so the reap stays synchronous and
/// spawns nothing. A provider stuck on an uninterruptible kernel wait is moved into a
/// detached named thread that blocks on `wait`, so the caller returns in about
/// `grace`, leaking at most one parked thread and one zombie until the kernel releases
/// the wait (both cleaned up when the process exits and the provider is reparented to
/// init). A `try_wait` error does *not* prove the child was reaped -- std's Unix
/// `try_wait` propagates the `waitpid(WNOHANG)` error, so a transient EINTR leaves the
/// child alive -- so it too routes to the detached wait, which actually reaps a live
/// child and returns immediately (ECHILD) for one already reaped elsewhere.
fn reap(mut child: impl Reap, grace: Duration) -> Reaped {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Reaped::Exited,
            Err(error) => {
                // An error is not proof of reaping (only ECHILD is): hand the wait to
                // the detached thread, which reaps a still-live child and resolves an
                // already-reaped one immediately.
                tracing::warn!(%error, "provider try_wait failed; reaping on a detached thread");
                break;
            }
            Ok(None) if Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    tracing::warn!("provider did not exit after SIGKILL; reaping on a detached thread");
    match std::thread::Builder::new()
        .name("umbra-provider-reap".into())
        .spawn(move || {
            let _ = child.wait();
        }) {
        // Dropping the `JoinHandle` detaches the thread; std's `Child::drop` never
        // waits, so the `Abandoned` path leaves only a zombie until process exit.
        Ok(_) => Reaped::Detached,
        Err(_) => Reaped::Abandoned,
    }
}

/// Owns a child provider; losing the session closes IPC and reaps the child.
///
/// `Drop` invalidates the connection first (so the provider sees EOF), then SIGKILLs
/// and reaps the child through [`reap`]: a short synchronous grace on the dropping
/// thread, then a detached reaper for a child stuck on an uninterruptible wait. A
/// zombie may outlive the drop in that case, but the caller never blocks on the
/// kernel wait.
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
        if let Some(child) = self.child.take() {
            let _ = reap(child, REAP_GRACE);
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
                // A provider stuck exec-paging from a dead mount must not wedge this
                // failure path, so reap it through the same bounded helper as `Drop`.
                let _ = reap(child, REAP_GRACE);
                return Err(e);
            }
        };
        // macOS may inherit O_NONBLOCK from the accepting listener.
        if let Err(error) = stream.set_nonblocking(false) {
            let _ = reap(child, REAP_GRACE);
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
    accept_connection(
        Connection::new(stream, Duration::from_millis(timeout_ms)),
        id,
        role,
        factory,
    )
}

/// Negotiate the same provider handshake on an injected private connection.
pub fn accept_connection<B>(
    mut connection: Connection,
    id: &str,
    role: &str,
    factory: impl FnOnce(&[u8]) -> Result<(B, std::collections::BTreeSet<String>)>,
) -> Result<(Connection, B)> {
    let hello: Hello = connection.receive()?;
    let result = (|| {
        if hello.id != id || hello.role != role || hello.version != PROTOCOL_VERSION {
            return Err(protocol_error("provider identity/role/version mismatch"));
        }
        let (backend, capabilities) = factory(&hello.options)?;
        if !hello.required_capabilities.is_subset(&capabilities) {
            // Name what is missing and what is on offer: these errors are the
            // whole configuration interface, since nothing here may prompt.
            let missing: Vec<&str> = hello
                .required_capabilities
                .difference(&capabilities)
                .map(String::as_str)
                .collect();
            let advertised: Vec<&str> = capabilities.iter().map(String::as_str).collect();
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "provider.handshake",
                format!(
                    "provider '{id}' (role {role}) does not advertise required \
                     capabilities [{}]; it advertises [{}]",
                    missing.join(", "),
                    advertised.join(", ")
                ),
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

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    fn exit_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    // A `Reap` double whose `wait` blocks until the test releases it, so the detached
    // hand-off can be observed without a real process.
    struct FakeChild {
        killed: Arc<AtomicBool>,
        exits: bool,
        errs: bool,
        wait_calls: Arc<AtomicUsize>,
        entered_wait: mpsc::Sender<std::thread::ThreadId>,
        release: mpsc::Receiver<()>,
        finished: mpsc::Sender<()>,
    }
    impl Reap for FakeChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            if self.errs {
                // A try_wait error (e.g. a transient EINTR): not proof of reaping.
                return Err(std::io::Error::other("try_wait failed"));
            }
            Ok(self.exits.then(exit_status))
        }
        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered_wait.send(std::thread::current().id());
            let _ = self.release.recv();
            let _ = self.finished.send(());
            Ok(exit_status())
        }
    }
    struct FakeControl {
        killed: Arc<AtomicBool>,
        wait_calls: Arc<AtomicUsize>,
        entered_wait: mpsc::Receiver<std::thread::ThreadId>,
        release: mpsc::Sender<()>,
        finished: mpsc::Receiver<()>,
    }
    fn fake_child(exits: bool, errs: bool) -> (FakeChild, FakeControl) {
        let killed = Arc::new(AtomicBool::new(false));
        let wait_calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        (
            FakeChild {
                killed: killed.clone(),
                exits,
                errs,
                wait_calls: wait_calls.clone(),
                entered_wait: entered_tx,
                release: release_rx,
                finished: finished_tx,
            },
            FakeControl {
                killed,
                wait_calls,
                entered_wait: entered_rx,
                release: release_tx,
                finished: finished_rx,
            },
        )
    }

    #[test]
    fn reap_detaches_a_child_whose_wait_never_returns() {
        let (child, control) = fake_child(false, false); // try_wait: Ok(None) forever
        let start = Instant::now();
        let outcome = reap(child, Duration::from_millis(50));
        assert_eq!(outcome, Reaped::Detached);
        assert!(
            control.killed.load(Ordering::SeqCst),
            "the child was killed"
        );
        assert!(
            start.elapsed() < Duration::from_millis(50) + Duration::from_millis(500),
            "reap returned within the grace window plus slack, not after wait finished"
        );
        // `wait` ran on the detached reaper thread, not the caller's.
        let wait_thread = control
            .entered_wait
            .recv_timeout(Duration::from_secs(1))
            .expect("wait() was entered on the detached reaper thread");
        assert_ne!(wait_thread, std::thread::current().id());
        // Release the blocked wait and observe the reaper thread finish.
        control.release.send(()).unwrap();
        control
            .finished
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reaper finished after wait returned");
        assert_eq!(control.wait_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reap_is_synchronous_for_a_child_that_exits_on_kill() {
        let (child, control) = fake_child(true, false); // try_wait: Ok(Some(_))
        let outcome = reap(child, Duration::from_secs(5));
        assert_eq!(outcome, Reaped::Exited);
        assert!(control.killed.load(Ordering::SeqCst));
        assert_eq!(
            control.wait_calls.load(Ordering::SeqCst),
            0,
            "no detached wait for a child reaped synchronously"
        );
        assert!(
            control.entered_wait.try_recv().is_err(),
            "no reaper thread was spawned"
        );
    }

    #[test]
    fn reap_routes_a_try_wait_error_to_the_detached_wait() {
        // A try_wait error is not proof of reaping, so the wait must move to the
        // detached thread, which reaps a still-live child (or resolves immediately for
        // an already-reaped one).
        let (child, control) = fake_child(false, true); // try_wait: Err every poll
        let outcome = reap(child, Duration::from_secs(5));
        assert_eq!(outcome, Reaped::Detached);
        // The detached `wait` actually runs and completes.
        control.release.send(()).unwrap();
        let start = Instant::now();
        while control.wait_calls.load(Ordering::SeqCst) == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "the detached reaper ran wait() to completion"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        control
            .finished
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reaper finished after wait returned");
    }

    fn process_is_alive(pid: u32) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn client_drop_kills_and_reaps_a_real_provider_promptly() {
        let (connection, _peer) = pair(Duration::from_secs(1));
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id();
        let client = Client {
            connection,
            child: Some(child),
            next_id: 1,
            welcome: welcome(),
        };
        let start = Instant::now();
        drop(client);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "dropping a Client kills and reaps its provider promptly"
        );
        // A beat in case this host detached rather than reaping synchronously.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!process_is_alive(pid), "the provider process was reaped");
    }

    #[test]
    fn connect_reaps_a_provider_that_never_completes_the_handshake() {
        // A script that ignores its args and sleeps: it stays alive but never dials
        // back the private socket, so `connect` must hit the accept timeout (D-4).
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stall");
        std::fs::write(&script, b"#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let descriptor = ProviderDescriptor {
            id: "fake".into(),
            role: "test".into(),
            protocol_version: PROTOCOL_VERSION,
            executable: crate::BytePath::new(script.as_os_str().as_bytes().to_vec()).unwrap(),
            capabilities: Default::default(),
            options: Vec::new(),
        };
        let start = Instant::now();
        // `Client` is not `Debug`, so match rather than `unwrap_err`.
        let error = match Client::connect(&descriptor, 200) {
            Ok(_) => panic!("connect must not succeed against a provider that never dials back"),
            Err(e) => e,
        };
        let elapsed = start.elapsed();
        assert!(
            error.to_string().contains("provider connection timeout"),
            "the accept timeout is reported: {error}"
        );
        assert!(
            elapsed < Duration::from_millis(200) + REAP_GRACE + Duration::from_secs(1),
            "connect returned about at the timeout plus the reap grace, not blocked: {elapsed:?}"
        );
    }

    #[test]
    fn a_read_timeout_reports_the_deadline_rather_than_the_raw_errno() {
        // The peer never answers, so the receive read expires against its deadline.
        let (mut receiver, _sender) = pair(Duration::from_millis(30));
        let error = receiver.receive::<Frame>().unwrap_err();
        assert_eq!(error.kind, ErrorKind::StorageUnavailable);
        assert!(
            error.to_string().contains("request deadline exceeded"),
            "a timed-out read reports the deadline miss, not `os error 35`: {error}"
        );
    }
}
