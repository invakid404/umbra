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

// Instant on Darwin is CLOCK_UPTIME_RAW (sleep does not consume a budget).
fn transport_now() -> Instant {
    #[cfg(test)]
    if let Some((awake, _continuous)) = FIX264_CLOCK.with(|v| v.get()) {
        return awake;
    }
    Instant::now()
}

fn wait_for_connection(listener: &UnixListener, deadline: Instant) -> std::io::Result<()> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    // Readiness wakes immediately; the existing child-exit inspection cadence
    // remains a fallback when a child ends without connecting.
    let duration = remaining.min(Duration::from_millis(5));
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: duration.as_nanos() as _,
    };
    match poll(&mut [PollFd::new(listener, PollFlags::IN)], Some(&timeout)) {
        Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
#[cfg(test)]
thread_local! { static FIX264_CLOCK: std::cell::Cell<Option<(Instant, Instant)>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn early_wake() {
    let awake = transport_now();
    FIX264_CLOCK.with(|v| v.set(Some((awake, awake + Duration::from_secs(86400)))));
}

fn unavailable(error: impl std::fmt::Display) -> UmbraError {
    UmbraError::new(
        ErrorKind::StorageUnavailable,
        "provider.transport",
        error.to_string(),
    )
}

/// A read or write that expired against its `SO_{RCV,SND}TIMEO` deadline surfaces as
/// `WouldBlock` (EAGAIN, `os error 35`) or, on some platforms, `TimedOut`. Both are
/// wake indications: callers must recheck the original absolute uptime deadline,
/// retaining partial progress if it has not elapsed.
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
    cleanup_limit: Option<Instant>,
}
impl Connection {
    fn send_launch_stdio(&mut self, stdio: &crate::launch_stdio::LaunchStdio) -> Result<()> {
        let result = stdio.send_with_budget(&self.stream, || {
            self.stream
                .set_write_timeout(Some(self.remaining()?))
                .map_err(unavailable)
        });
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    /// Receive the platform's one-shot owned descriptor payload.
    pub fn receive_launch_stdio(&mut self) -> Result<crate::launch_stdio::LaunchStdio> {
        let result = crate::launch_stdio::LaunchStdio::receive_with_budget(&self.stream, || {
            self.stream
                .set_read_timeout(Some(self.remaining()?))
                .map_err(unavailable)
        });
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    /// Construct this value from the supplied configuration or fields.
    pub fn new(stream: UnixStream, timeout: Duration) -> Self {
        Self {
            stream,
            deadline: transport_now() + timeout,
            timeout,
            failed: false,
            cleanup_limit: None,
        }
    }
    /// Duplicate the provider endpoint for non-consuming liveness observation.
    /// Closing the duplicate alone does not close this connection.
    pub fn observation_stream(&self) -> Result<UnixStream> {
        let stream = self.stream.try_clone().map_err(unavailable)?;
        crate::cost_snapshot!("provider-observation-duplicate", None, &[]);
        Ok(stream)
    }
    /// Current absolute deadline, retained by nested callback requests.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Restore an outer request deadline after a nested control request.
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = self
            .cleanup_limit
            .map_or(deadline, |limit| limit.min(deadline));
    }
    /// Permanently cap this connection to an original managed cleanup ticket.
    /// Neither nested requests nor later notifications can extend the cap.
    pub fn cap_cleanup_deadline(&mut self, deadline: Instant) {
        let limit = self
            .cleanup_limit
            .map_or(deadline, |current| current.min(deadline));
        self.cleanup_limit = Some(limit);
        self.deadline = self.deadline.min(limit);
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
        self.set_deadline(transport_now() + self.timeout);
    }
    /// Remaining time in the current frame's original request deadline.
    pub fn remaining(&self) -> Result<Duration> {
        if self.failed {
            return Err(unavailable("connection is closed"));
        }
        self.deadline
            .checked_duration_since(transport_now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| unavailable("request deadline exceeded"))
    }
    /// Invalidate.
    pub fn invalidate(&mut self) {
        self.failed = true;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
    /// Awake-time clock used by request deadlines and shutdown envelopes.
    pub fn clock_now() -> Instant {
        transport_now()
    }
    /// Existing configured request budget, without starting a new deadline.
    pub fn request_timeout(&self) -> Duration {
        self.timeout
    }
    fn read_progress(&mut self, out: &mut [u8], offset: usize) -> std::io::Result<usize> {
        #[cfg(test)]
        if FIX264_EARLY.with(|v| v.get()) && offset > 0 {
            FIX264_EARLY.with(|v| v.set(false));
            early_wake();
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            let end = if FIX264_EARLY.with(|v| v.get()) {
                offset + 1
            } else {
                out.len()
            };
            self.stream.read(&mut out[offset..end])
        }
        #[cfg(not(test))]
        self.stream.read(&mut out[offset..])
    }
    fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut offset = 0;
        while offset < out.len() {
            let remaining = self.remaining()?;
            let count = match self.stream.set_read_timeout(Some(remaining)) {
                Ok(()) => match self.read_progress(out, offset) {
                    Ok(count) => count,
                    // Timeout errno can be an early wake (including across sleep).
                    // Retain the offset and let remaining() decide real expiry.
                    Err(ref e)
                        if is_deadline_timeout(e)
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue;
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
    fn write_progress(&mut self, bytes: &[u8], offset: usize) -> std::io::Result<usize> {
        #[cfg(test)]
        if FIX264_EARLY.with(|v| v.get()) && offset > 0 {
            FIX264_EARLY.with(|v| v.set(false));
            early_wake();
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            let end = if FIX264_EARLY.with(|v| v.get()) {
                offset + 1
            } else {
                bytes.len()
            };
            self.stream.write(&bytes[offset..end])
        }
        #[cfg(not(test))]
        self.stream.write(&bytes[offset..])
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.stream
                .set_write_timeout(Some(self.remaining()?))
                .map_err(unavailable)?;
            let count = match self.write_progress(bytes, offset) {
                Ok(count) => count,
                // Early wakes retain progress; the next iteration rechecks the same deadline.
                Err(ref e)
                    if is_deadline_timeout(e) || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    continue;
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
        let result = Child::try_wait(self);
        #[cfg(feature = "test-owned-exit")]
        if let Ok(Some(status)) = &result {
            observed_exit(self.id(), *status);
        }
        result
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let result = Child::wait(self);
        #[cfg(feature = "test-owned-exit")]
        if let Ok(status) = &result {
            observed_exit(self.id(), *status);
        }
        result
    }
}

#[cfg(feature = "test-owned-exit")]
type ExitObservation = std::sync::Arc<std::sync::Mutex<Option<std::process::ExitStatus>>>;
#[cfg(feature = "test-owned-exit")]
static EXIT_OBSERVATIONS: std::sync::Mutex<
    std::collections::BTreeMap<
        u32,
        std::sync::Weak<std::sync::Mutex<Option<std::process::ExitStatus>>>,
    >,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());
#[cfg(feature = "test-owned-exit")]
fn observed_exit(pid: u32, status: std::process::ExitStatus) {
    if let Some(observer) = EXIT_OBSERVATIONS
        .lock()
        .unwrap()
        .remove(&pid)
        .and_then(|value| value.upgrade())
    {
        *observer.lock().unwrap() = Some(status);
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
    /// The actual wait returned an error, so collection remains unconfirmed.
    WaitFailed,
}

/// SIGKILL a provider and reap it without blocking the caller past `grace`.
///
/// Check once without blocking, then wait for the actual owned child on a
/// reaper thread. The caller waits only to the original grace cutoff. A child
/// stuck in the kernel retains its reaper after the caller returns. An inspection
/// error follows the existing immediate-detach path; it is not collection proof.
fn reap(mut child: impl Reap, grace: Duration) -> Reaped {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    let await_result = match child.try_wait() {
        Ok(Some(_)) => return Reaped::Exited,
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(%error, "provider inspection failed; retaining detached wait");
            false
        }
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    match std::thread::Builder::new()
        .name("umbra-provider-reap".into())
        .spawn(move || {
            let result = child.wait();
            let _ = sender.send(result);
        }) {
        Ok(_handle) => {
            if !await_result {
                return Reaped::Detached;
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                // Receipt proves the owned wait completed. Dropping the handle
                // avoids an unbounded wait for the thread's final return.
                Ok(Ok(_)) => Reaped::Exited,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "provider wait failed; collection unconfirmed");
                    Reaped::WaitFailed
                }
                Err(_) => Reaped::Detached,
            }
        }
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
    /// Move a fresh managed connection to the worker without shutting down its
    /// socket. The cleanup owner retains the independently stored child handle.
    /// No requests may have been sent after the handshake.
    pub fn into_managed_handoff(mut self) -> Result<(UnixStream, Welcome)> {
        if self.child.is_some() || self.next_id != 1 || self.connection.failed {
            return Err(protocol_error(
                "connection is not eligible for managed handoff",
            ));
        }
        let (replacement, peer) = UnixStream::pair().map_err(unavailable)?;
        drop(peer);
        let timeout = self.connection.timeout;
        let connection =
            std::mem::replace(&mut self.connection, Connection::new(replacement, timeout));
        Ok((connection.stream, self.welcome.clone()))
    }

    /// Qualification receipt populated only by this transport's actual parent wait.
    #[cfg(feature = "test-owned-exit")]
    pub fn observe_owned_exit(pid: u32) -> ExitObservation {
        let observation = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut registry = EXIT_OBSERVATIONS.lock().unwrap();
        registry.retain(|_, value| value.strong_count() > 0);
        assert!(registry
            .insert(pid, std::sync::Arc::downgrade(&observation))
            .is_none());
        observation
    }
    /// Whether this client can independently confirm its provider's exit.
    /// Attached storage/journal clients remain valid for their own protocols.
    pub fn owns_provider(&self) -> bool {
        self.child.is_some()
    }

    /// After a validated Final, wait without signalling the owned provider.
    /// Failure leaves the child owned for the existing emergency Drop path.
    /// The returned duration is available to test-side Final-to-exit readings.
    pub fn confirm_provider_exit(&mut self) -> Result<Duration> {
        let started = transport_now();
        let deadline = started + REAP_GRACE;
        self.connection.invalidate();
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| protocol_error("platform provider has no owned exit waiter"))?;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    #[cfg(feature = "test-owned-exit")]
                    observed_exit(child.id(), status);
                    self.child.take();
                    if !status.success() {
                        return Err(protocol_error(format!("provider exited with {status}")));
                    }
                    return Ok(transport_now().saturating_duration_since(started));
                }
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(unavailable(error)),
            }
            if transport_now() >= deadline {
                return Err(protocol_error(
                    "provider exit was not confirmed within REAP_GRACE",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

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
    /// Explicit owned exit waiter for injected protocol contract tests.
    /// The caller must supply the child that owns the peer connection.
    #[cfg(feature = "test-owned-exit")]
    pub fn from_owned_connection(connection: Connection, welcome: Welcome, child: Child) -> Self {
        Self {
            connection,
            child: Some(child),
            next_id: 1,
            welcome,
        }
    }

    /// Validate provider registration and establish a private protocol session.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        Self::connect_with_custody(descriptor, timeout_ms, None, None, |_| Ok(()))
    }

    /// Connect from the cleanup-owner process, retaining its child on every error.
    /// The caller supplies an empty custody slot and must collect it under the
    /// original cleanup ticket, including connection and handshake failures.
    /// The returned client has no child and cannot kill the provider on drop.
    pub fn connect_managed(
        descriptor: &ProviderDescriptor,
        timeout_ms: u64,
        custody: &mut Option<Child>,
    ) -> Result<Self> {
        Self::connect_managed_configured(descriptor, timeout_ms, custody, |_| Ok(()))
    }

    /// Install private managed startup descriptors and environment before spawn.
    /// Configuration spends the same readiness allowance as the handshake.
    /// A configuration failure cannot leave a spawned child; after spawn the
    /// supplied custody slot retains the child even if the handshake fails.
    pub fn connect_managed_configured(
        descriptor: &ProviderDescriptor,
        timeout_ms: u64,
        custody: &mut Option<Child>,
        configure: impl FnOnce(&mut Command) -> std::io::Result<()>,
    ) -> Result<Self> {
        if custody.is_some() {
            return Err(protocol_error("provider custody slot already occupied"));
        }
        Self::connect_with_custody(descriptor, timeout_ms, Some(custody), None, configure)
    }

    /// Apply the inherited readiness cutoff without changing subsequent RPC budgets.
    pub fn connect_managed_configured_until(
        descriptor: &ProviderDescriptor,
        timeout_ms: u64,
        readiness_deadline: Instant,
        custody: &mut Option<Child>,
        configure: impl FnOnce(&mut Command) -> std::io::Result<()>,
    ) -> Result<Self> {
        if custody.is_some() {
            return Err(protocol_error("provider custody slot already occupied"));
        }
        Self::connect_with_custody(
            descriptor,
            timeout_ms,
            Some(custody),
            Some(readiness_deadline),
            configure,
        )
    }

    fn connect_with_custody(
        descriptor: &ProviderDescriptor,
        timeout_ms: u64,
        custody: Option<&mut Option<Child>>,
        readiness_deadline: Option<Instant>,
        configure: impl FnOnce(&mut Command) -> std::io::Result<()>,
    ) -> Result<Self> {
        descriptor.validate(&descriptor.role)?;
        if timeout_ms == 0 || timeout_ms > 60_000 {
            return Err(protocol_error("invalid timeout"));
        }
        let timeout = Duration::from_millis(timeout_ms);
        // Managed startup spends one allowance across preparation, spawn,
        // socket acceptance and Hello/Welcome. Ongoing RPCs retain their
        // configured timeout independently of this startup cutoff.
        let managed = custody.is_some();
        let managed_deadline =
            managed.then(|| readiness_deadline.unwrap_or_else(|| Instant::now() + timeout));
        let directory = tempfile::Builder::new()
            .prefix("umbra-provider-")
            .tempdir()
            .map_err(unavailable)?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(unavailable)?;
        let socket = directory.path().join("ipc");
        let listener = UnixListener::bind(&socket).map_err(unavailable)?;
        listener.set_nonblocking(true).map_err(unavailable)?;
        let mut command = Command::new(std::ffi::OsStr::from_bytes(
            descriptor.executable.as_bytes(),
        ));
        command
            .arg("--umbra-socket")
            .arg(&socket)
            .arg("--umbra-timeout-ms")
            .arg(timeout_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        configure(&mut command).map_err(unavailable)?;
        if managed_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(unavailable("provider readiness expired before spawn"));
        }
        crate::cost_snapshot!("provider-command-before", None, &[]);
        #[cfg(feature = "startup-witness")]
        let stage_origin = Instant::now();
        let child = command.spawn().map_err(unavailable)?;
        #[cfg(feature = "startup-witness")]
        let (spawn_duration, stage_child) = (stage_origin.elapsed(), child.id());
        crate::cost_snapshot!("provider-command-after", Some(child.id()), &[]);
        let mut local_child = None;
        let child_slot = custody.unwrap_or(&mut local_child);
        *child_slot = Some(child);
        let deadline = managed_deadline.unwrap_or_else(|| Instant::now() + timeout);
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break Ok(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break Err(unavailable("provider connection timeout"));
                    }
                    match child_slot.as_mut().expect("spawned child").try_wait() {
                        Ok(Some(status)) => {
                            break Err(unavailable(format!("provider exited: {status}")))
                        }
                        Err(e) => break Err(unavailable(e)),
                        _ => {}
                    }
                    if let Err(error) = wait_for_connection(&listener, deadline) {
                        break Err(unavailable(error));
                    }
                }
                Err(e) => break Err(unavailable(e)),
            }
        };
        let stream = match accepted {
            Ok(stream) => stream,
            Err(e) => {
                // A provider stuck exec-paging from a dead mount must not wedge this
                // failure path, so reap it through the same bounded helper as `Drop`.
                if !managed {
                    let _ = reap(child_slot.take().expect("spawned child"), REAP_GRACE);
                }
                return Err(e);
            }
        };
        // macOS may inherit O_NONBLOCK from the accepting listener.
        crate::cost_snapshot!("provider-connection-accepted", None, &[]);
        #[cfg(feature = "startup-witness")]
        let accept_duration = stage_origin.elapsed();
        if let Err(error) = stream.set_nonblocking(false) {
            if !managed {
                let _ = reap(child_slot.take().expect("spawned child"), REAP_GRACE);
            }
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
            child: if managed { None } else { child_slot.take() },
            next_id: 1,
            welcome,
        };
        if managed {
            client.connection.set_deadline(deadline);
        }
        client.connection.send(&Hello {
            id: descriptor.id.clone(),
            role: descriptor.role.clone(),
            version: descriptor.protocol_version,
            required_capabilities: descriptor.capabilities.clone(),
            options: descriptor.options.clone(),
        })?;
        client.welcome = client.connection.receive::<Result<Welcome>>()??;
        #[cfg(feature = "startup-witness")]
        if let Some(directory) = std::env::var_os("UMBRA_F299_PERF_EVENTS") {
            use std::io::Write;
            let row = serde_json::json!({
                "event": "provider-transport-stages", "pid": std::process::id(),
                "child": stage_child, "provider_role": descriptor.role,
                "run_nonce": std::env::var("UMBRA_F299_PERF_RUN_NONCE").expect("measurement nonce"),
                "spawn_ns": spawn_duration.as_nanos(),
                "accept_ns": accept_duration.as_nanos(),
                "welcome_ns": stage_origin.elapsed().as_nanos(),
            });
            let path = std::path::Path::new(&directory)
                .join(format!("{}-transport-stages.transport", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("transport stage file");
            writeln!(file, "{row}").expect("transport stage write");
        }
        if client.welcome.id != descriptor.id
            || client.welcome.role != descriptor.role
            || client.welcome.version != PROTOCOL_VERSION
            || !descriptor
                .capabilities
                .is_subset(&client.welcome.capabilities)
        {
            return Err(protocol_error("provider handshake mismatch"));
        }
        crate::cost_snapshot!(
            &format!("provider-ready-{}", descriptor.role),
            client.child.as_ref().or(child_slot.as_ref()).map(Child::id),
            &[]
        );
        drop(listener);
        drop(directory);
        crate::cost_snapshot!("provider-listener-closed", None, &[]);
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

    /// Send a platform-only request followed by its owned descriptor payload.
    /// The response is the ownership ACK; any uncertainty closes the transport.
    pub fn call_with_stdio<Q: Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        request: &Q,
        stdio: crate::launch_stdio::LaunchStdio,
    ) -> Result<R> {
        if self.welcome.role != "platform"
            || !self
                .welcome
                .capabilities
                .contains(crate::launch_stdio::CAPABILITY)
        {
            return Err(protocol_error("platform launch-stdio capability required"));
        }
        let result = (|| {
            let id = self.start(request)?;
            self.connection.send_launch_stdio(&stdio)?;
            match self.connection.receive()? {
                Frame::Response { id: got, result } if id == got => decode(&result?),
                _ => Err(protocol_error("stdio ownership ACK mismatch")),
            }
        })();
        if result.is_err() {
            self.connection.invalidate();
        }
        result
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
    fn queued_stdio(sender: &Connection) {
        let file = tempfile::tempfile().unwrap();
        crate::launch_stdio::LaunchStdio::new(std::array::from_fn(|_| {
            file.try_clone().unwrap().into()
        }))
        .unwrap()
        .send(&sender.stream)
        .unwrap();
    }
    #[test]
    fn ancillary_send_expired_budget_transfers_no_rights() {
        let (mut sender, receiver) = pair(Duration::from_secs(5));
        let file = tempfile::tempfile().unwrap();
        let stdio = crate::launch_stdio::LaunchStdio::new(std::array::from_fn(|_| {
            file.try_clone().unwrap().into()
        }))
        .unwrap();
        sender.set_deadline(transport_now() - Duration::from_secs(1));
        let result = sender.send_launch_stdio(&stdio);
        assert!(result.is_err(), "expired ancillary send transferred rights");
        assert_eq!(
            result.unwrap_err().to_string(),
            unavailable("request deadline exceeded").to_string()
        );
        assert!(sender.failed);
        // Shutdown must expose EOF, not an ancillary marker or rights.
        let mut peer = receiver.stream;
        let mut marker = [0];
        assert_eq!(peer.read(&mut marker).unwrap(), 0);
    }

    #[test]
    fn ancillary_send_no_progress_exhausts_remaining_budget() {
        let (mut sender, receiver) = pair(Duration::from_secs(5));
        sender
            .stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let now = transport_now();
        FIX264_CLOCK.with(|v| v.set(Some((now, now))));
        sender.set_deadline(now + Duration::from_millis(30));
        crate::launch_stdio::SEND_ATTEMPT.with(|v| {
            v.set(Some(|| {
                // A no-payload wake consumes the remaining awake-time budget.
                let expired = transport_now() + Duration::from_millis(30);
                FIX264_CLOCK.with(|v| v.set(Some((expired, expired))));
                rustix::io::Errno::AGAIN
            }))
        });
        let file = tempfile::tempfile().unwrap();
        let stdio = crate::launch_stdio::LaunchStdio::new(std::array::from_fn(|_| {
            file.try_clone().unwrap().into()
        }))
        .unwrap();
        let result = sender.send_launch_stdio(&stdio);
        crate::launch_stdio::SEND_ATTEMPT.with(|v| v.set(None));
        FIX264_CLOCK.with(|v| v.set(None));
        let error = result.unwrap_err();
        assert_eq!(
            error.to_string(),
            unavailable("request deadline exceeded").to_string()
        );
        assert!(sender.failed);
        assert!(sender.stream.write_timeout().unwrap().unwrap() <= Duration::from_millis(30));
        let mut peer = receiver.stream;
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn ancillary_send_refreshes_stale_timeout() {
        let (mut sender, receiver) = pair(Duration::from_secs(5));
        sender
            .stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        sender.set_deadline(transport_now() + Duration::from_secs(1));
        let file = tempfile::tempfile().unwrap();
        let stdio = crate::launch_stdio::LaunchStdio::new(std::array::from_fn(|_| {
            file.try_clone().unwrap().into()
        }))
        .unwrap();
        sender.send_launch_stdio(&stdio).unwrap();
        assert!(sender.stream.write_timeout().unwrap().unwrap() <= Duration::from_secs(1));
        crate::launch_stdio::LaunchStdio::receive(&receiver.stream).unwrap();
    }

    #[test]
    fn ancillary_transfer_rejects_an_expired_request_before_receiving() {
        let (mut receiver, sender) = pair(Duration::from_secs(5));
        queued_stdio(&sender);
        receiver.set_deadline(transport_now() - Duration::from_secs(1));
        assert!(receiver.receive_launch_stdio().is_err());
        assert!(receiver.failed, "expired request must invalidate transport");
    }
    #[test]
    fn ancillary_transfer_refreshes_timeout_from_the_remaining_budget() {
        let (mut receiver, sender) = pair(Duration::from_secs(5));
        receiver
            .stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        receiver.set_deadline(transport_now() + Duration::from_secs(1));
        queued_stdio(&sender);
        receiver.receive_launch_stdio().unwrap();
        assert!(
            receiver.stream.read_timeout().unwrap().unwrap() <= Duration::from_secs(1),
            "ancillary receive reused the earlier frame's timeout"
        );
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
    fn ambiguous_stdio_ack_closes_transport_and_all_writer_copies() {
        use crate::launch_stdio::{LaunchStdio, CAPABILITY};
        for wrong_id in [false, true] {
            let (connection, mut server) = pair(Duration::from_secs(2));
            let (mut reader, writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let stdio = LaunchStdio::new([
                writer.try_clone().unwrap().into(),
                writer.try_clone().unwrap().into(),
                writer.into(),
            ])
            .unwrap();
            let worker = std::thread::spawn(move || {
                let Frame::Request { id, .. } = server.receive().unwrap() else {
                    panic!("expected adoption request")
                };
                let adopted = LaunchStdio::receive(&server.stream).unwrap();
                if wrong_id {
                    server
                        .send(&Frame::Response {
                            id: id + 1,
                            result: Ok(encode(&()).unwrap()),
                        })
                        .unwrap();
                    assert!(
                        server.receive::<Frame>().is_err(),
                        "client retried after uncertain ACK"
                    );
                }
                // Missing ACK models provider loss after accepting the handles.
                drop(adopted);
            });
            let mut hello = welcome();
            hello.role = "platform".into();
            hello.capabilities.insert(CAPABILITY.into());
            let mut client = Client::from_connection(connection, hello);
            assert!(client.call_with_stdio::<_, ()>(&"adopt", stdio).is_err());
            assert!(client.call::<_, ()>(&"retry").is_err());
            worker.join().unwrap();
            assert_eq!(
                reader.read(&mut [0]).unwrap(),
                0,
                "uncertain ACK retained a writer"
            );
        }
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
        wait_error: bool,
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
            if self.wait_error {
                Err(std::io::Error::other("wait failed"))
            } else {
                Ok(exit_status())
            }
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
                wait_error: false,
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
    fn reap_collects_a_wait_ready_child_before_returning() {
        let (child, control) = fake_child(false, false);
        // The child becomes waitable after the initial nonblocking inspection.
        control.release.send(()).unwrap();
        assert_eq!(reap(child, Duration::from_millis(50)), Reaped::Exited);
        assert_eq!(control.wait_calls.load(Ordering::SeqCst), 1);
        control
            .finished
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn reap_does_not_confirm_a_failed_wait() {
        let (mut child, control) = fake_child(false, false);
        child.wait_error = true;
        control.release.send(()).unwrap();
        assert_eq!(reap(child, Duration::from_millis(50)), Reaped::WaitFailed);
        assert_eq!(control.wait_calls.load(Ordering::SeqCst), 1);
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
    fn managed_deadline_survives_request_begin_and_nested_restore() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(socket, Duration::from_secs(60));
        let original = Instant::now() + Duration::from_secs(1);
        connection.cap_cleanup_deadline(original);
        connection.begin();
        assert_eq!(connection.deadline(), original);
        connection.set_deadline(original + Duration::from_secs(60));
        assert_eq!(connection.deadline(), original);
        connection.cap_cleanup_deadline(original + Duration::from_secs(120));
        connection.begin();
        assert_eq!(connection.deadline(), original);
        let earlier = original - Duration::from_millis(100);
        connection.cap_cleanup_deadline(earlier);
        connection.begin();
        assert_eq!(connection.deadline(), earlier);
    }

    #[test]
    fn managed_readiness_does_not_replace_configured_rpc_timeout() {
        let descriptor = managed_stall_descriptor();
        let mut custody = None;
        let deadline = Instant::now() + Duration::from_millis(100);
        let result = Client::connect_managed_configured_until(
            &descriptor,
            20_000,
            deadline,
            &mut custody,
            |command| {
                let args: Vec<_> = command.get_args().collect();
                assert!(args
                    .windows(2)
                    .any(|pair| pair[0] == "--umbra-timeout-ms" && pair[1] == "20000"));
                Err(std::io::Error::other("checked RPC timeout before spawn"))
            },
        );
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("checked RPC timeout"));
        assert!(custody.is_none());
        let expired = Client::connect_managed_configured_until(
            &descriptor,
            20_000,
            Instant::now() - Duration::from_millis(1),
            &mut custody,
            |_| Ok(()),
        );
        assert!(expired
            .err()
            .unwrap()
            .to_string()
            .contains("readiness expired before spawn"));
        assert!(custody.is_none());
    }

    #[test]
    fn managed_configuration_failure_never_spawns_provider() {
        let descriptor = managed_stall_descriptor();
        let mut custody = None;
        let result = Client::connect_managed_configured(&descriptor, 1000, &mut custody, |_| {
            Err(std::io::Error::other("startup endpoint setup failed"))
        });
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("startup endpoint setup failed"));
        assert!(custody.is_none(), "configuration failure spawned a child");
    }

    #[test]
    fn managed_configuration_cannot_renew_readiness_before_spawn() {
        let descriptor = managed_stall_descriptor();
        let mut custody = None;
        let result = Client::connect_managed_configured(&descriptor, 1, &mut custody, |_| {
            std::thread::sleep(Duration::from_millis(5));
            Ok(())
        });
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("readiness expired before spawn"));
        assert!(custody.is_none(), "expired startup spawned a child");
    }

    fn managed_stall_descriptor() -> ProviderDescriptor {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stall-provider.sh");
        ProviderDescriptor {
            id: "fake".into(),
            role: "test".into(),
            protocol_version: PROTOCOL_VERSION,
            executable: crate::BytePath::new(script.as_os_str().as_bytes().to_vec()).unwrap(),
            capabilities: Default::default(),
            options: Default::default(),
        }
    }

    #[test]
    fn managed_connect_timeout_retains_live_child_for_owner_collection() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stall-provider.sh");
        let descriptor = ProviderDescriptor {
            id: "fake".into(),
            role: "test".into(),
            protocol_version: PROTOCOL_VERSION,
            executable: crate::BytePath::new(script.as_os_str().as_bytes().to_vec()).unwrap(),
            capabilities: Default::default(),
            options: Vec::new(),
        };
        let mut custody = None;
        let result = Client::connect_managed(&descriptor, 100, &mut custody);
        let mut child = custody.expect("spawned child remains in owner custody");
        let before = child.try_wait().unwrap();
        // This fixture never acquired resources; finish its deliberate stall
        // and retain the actual parent receipt before asserting the outcome.
        child.kill().unwrap();
        let collected = child.wait().unwrap();
        assert!(result.is_err());
        assert_eq!(
            before, None,
            "connect error must not kill the managed provider"
        );
        assert!(!collected.success());
    }

    #[test]
    fn connect_reaps_a_provider_that_never_completes_the_handshake() {
        // A script that ignores its args and sleeps: it stays alive but never dials
        // back the private socket, so `connect` must hit the accept timeout (D-4).
        //
        // The script is a checked-in fixture rather than written at test time: a
        // concurrent sibling test's `posix_spawn` copies the shared fd table
        // (glibc uses `CLONE_VM|CLONE_VFORK` without `CLONE_FILES`), so it can
        // inherit a still-open writable fd from `fs::write`. The child's
        // reference keeps `inode->i_writecount > 0` until its own `execve`
        // completes `close-on-exec`, and any `execve` of that script during
        // that window fails with `ETXTBSY` from `deny_write_access` (#146).
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stall-provider.sh");
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

#[cfg(test)]
thread_local! { static FIX264_EARLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
mod fix264_sleep_probe {
    use super::*;
    #[test]
    fn early_read_timeout_preserves_progress() {
        let (mut peer, socket) = UnixStream::pair().unwrap();
        peer.write_all(b"ab").unwrap();
        let mut conn = Connection::new(socket, Duration::from_secs(5));
        FIX264_EARLY.with(|v| v.set(true));
        let mut out = [0; 2];
        let result = conn.read_exact(&mut out);
        assert!(Instant::now() < conn.deadline);
        eprintln!("partial read={out:?}, early result={result:?}");
        assert!(
            result.is_ok(),
            "early timeout before uptime deadline must preserve progress"
        );
        assert_eq!(&out, b"ab");
    }
    #[test]
    fn early_write_timeout_preserves_progress() {
        let (mut peer, socket) = UnixStream::pair().unwrap();
        let mut conn = Connection::new(socket, Duration::from_secs(5));
        FIX264_EARLY.with(|v| v.set(true));
        let result = conn.write_all(b"ab");
        assert!(Instant::now() < conn.deadline);
        let mut first = [0; 1];
        peer.read_exact(&mut first).unwrap();
        eprintln!("first written={first:?}, early result={result:?}");
        assert!(
            result.is_ok(),
            "early timeout before uptime deadline must preserve progress"
        );
        let mut rest = [0; 1];
        peer.read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"b");
    }
}

#[cfg(test)]
mod fix264_real_expiry {
    use super::*;
    #[test]
    fn actual_uptime_expiry_is_terminal() {
        for write in [false, true] {
            let (_peer, socket) = UnixStream::pair().unwrap();
            FIX264_CLOCK.with(|v| v.set(None));
            let mut c = Connection::new(socket, Duration::from_secs(5));
            FIX264_CLOCK.with(|v| v.set(Some((c.deadline, c.deadline))));
            let e = if write {
                c.write_all(b"x")
            } else {
                c.read_exact(&mut [0])
            }
            .unwrap_err();
            assert!(e.to_string().contains("request deadline exceeded"));
        }
        FIX264_CLOCK.with(|v| v.set(None));
    }
}

#[cfg(test)]
mod startup_readiness_tests {
    use super::*;

    #[test]
    fn readiness_does_not_renew_expired_cutoff() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deadline");
        let listener = UnixListener::bind(&path).unwrap();
        let cutoff = Instant::now();
        for ready in [false, true] {
            let _peer = ready.then(|| UnixStream::connect(&path).unwrap());
            assert_eq!(
                wait_for_connection(&listener, cutoff).unwrap_err().kind(),
                std::io::ErrorKind::TimedOut
            );
        }
    }

    #[test]
    fn queued_connection_has_no_timer_floor() {
        if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut samples = Vec::new();
        for _ in 0..30 {
            let peer = UnixStream::connect(&path).unwrap();
            let start = Instant::now();
            wait_for_connection(&listener, start + Duration::from_secs(1)).unwrap();
            samples.push(start.elapsed());
            let accepted = listener.accept().unwrap();
            drop((peer, accepted));
        }
        samples.sort();
        eprintln!(
            "ready listener median={:?} p95={:?}",
            samples[15], samples[28]
        );
        assert!(samples[15] <= Duration::from_millis(1));
        assert!(samples[28] <= Duration::from_millis(5));
    }
}
