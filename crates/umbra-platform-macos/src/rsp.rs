//! Bounded GDB Remote Serial Protocol transport to a private debugserver.
use crate::{error, Options};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use umbra_core::Result;
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(error("rsp", "odd hex length"));
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| {
            let s = std::str::from_utf8(b).map_err(|e| error("rsp", e))?;
            u8::from_str_radix(s, 16).map_err(|e| error("rsp", e))
        })
        .collect()
}
pub fn number(s: &str) -> Result<u64> {
    u64::from_str_radix(s, 16).map_err(|e| error("rsp number", e))
}
pub fn fields(s: &str) -> BTreeMap<&str, &str> {
    s.split(';').filter_map(|f| f.split_once(':')).collect()
}
pub(crate) struct Wakeup {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
    cancelled: std::sync::atomic::AtomicBool,
}
impl Wakeup {
    fn new() -> std::io::Result<std::sync::Arc<Self>> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(std::sync::Arc::new(Self {
            reader,
            writer,
            cancelled: false.into(),
        }))
    }
    pub(crate) fn signal(&self) {
        use std::sync::atomic::Ordering;
        self.cancelled.store(true, Ordering::Release);
        let _ = (&self.writer).write(&[1]);
    }
}
#[derive(Default)]
pub(crate) struct Wakeups(std::sync::Mutex<(bool, Vec<std::sync::Weak<Wakeup>>)>);
impl Wakeups {
    pub(crate) fn add(&self, wakeup: std::sync::Weak<Wakeup>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.1.retain(|w| w.strong_count() != 0);
        if state.0 {
            if let Some(w) = wakeup.upgrade() {
                w.signal();
            }
        }
        state.1.push(wakeup);
    }
    pub(crate) fn signal(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = true;
        for wake in state.1.iter().filter_map(std::sync::Weak::upgrade) {
            wake.signal();
        }
    }
}
pub struct Rsp {
    wakeup: std::sync::Arc<Wakeup>,
    teardown: bool,
    aligned: bool,
    ownership: Option<umbra_platform::shutdown::Reservation>,
    /// Debug aid: distinguishes concurrent connections in UMBRA_RSP_LOG output.
    pub id: u32,
    stream: TcpStream,
    child: Child,
    reaped: bool,
    buffer: Vec<u8>,
    no_ack: bool,
    deadline: Option<Instant>,
    setup_deadline: Option<Instant>,
    /// Set once the owning session's ordered teardown has killed and reaped
    /// the tracer, so `Drop` does not do it a second time.
    torn_down: bool,
    #[cfg(feature = "signal-witness")]
    signal_fault_fired: bool,
}

#[cfg(test)]
thread_local! {
    // A controlled transport can fail before, during or after accepting a
    // signal packet. Count attempts even after the one-shot fault is consumed.
    pub(crate) static SIGNAL_WRITE_FAULT: std::cell::Cell<Option<(usize, &'static str)>> = const { std::cell::Cell::new(None) };
    pub(crate) static SIGNAL_WRITE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Own the child immediately after spawn, before any fallible socket operation.
// Once Rsp takes it, the existing ordered teardown owns cleanup instead.
struct SetupChild {
    child: Option<std::process::Child>,
    ownership: Option<umbra_platform::shutdown::Reservation>,
}
impl std::ops::Deref for SetupChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.child.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for SetupChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().unwrap()
    }
}
impl Drop for SetupChild {
    fn drop(&mut self) {
        let Some(child) = &mut self.child else { return };
        let _ = child.kill();
        let deadline = crate::clock::now() + TRACER_REAP_BOUND;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(slot) = &self.ownership {
                        let _ = slot.confirm(umbra_core::shutdown::Proof::Collected {
                            status: status.to_string(),
                        });
                    }
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
                Ok(None) => {}
            }
            if crate::clock::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(slot) = &self.ownership {
            let _ = slot.unconfirmed("debugserver setup cleanup did not confirm collection");
        }
    }
}
/// How long a tracer is given to be reaped after its `SIGKILL`, by the ordered
/// teardown and by the `Drop` fallback alike. Every wait in teardown is bounded.
pub const TRACER_REAP_BOUND: Duration = Duration::from_millis(250);
/// What one teardown read of the connection produced.
pub enum TeardownRead {
    /// A whole packet, e.g. debugserver's `X09` after an RSP `k`.
    Packet(String),
    /// Nothing within the wait.
    Quiet,
    /// The connection is gone (EOF, receive error or an undecodable packet).
    Closed(String),
}
/// Debugserver discovery for callers that did not name one in `Options`:
/// `UMBRA_DEBUGSERVER` first, then the path below `xcode-select -p`. The
/// environment override exists because some Xcode layouts keep debugserver
/// under `SharedFrameworks`, where the probed path does not resolve.
///
/// `connect` and its handshake test share this so the test cannot silently skip
/// on a host where only the override names a usable binary.
pub fn discover_debugserver() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("UMBRA_DEBUGSERVER") {
        return Some(std::path::PathBuf::from(path));
    }
    Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
                .join("Library/PrivateFrameworks/LLDB.framework/Resources/debugserver")
        })
}
impl Rsp {
    pub fn deadline(&self) -> Instant {
        self.limit(Duration::from_secs(10))
    }
    fn limit(&self, budget: Duration) -> Instant {
        let mut limit = crate::clock::now() + budget;
        for d in [self.deadline, self.setup_deadline].into_iter().flatten() {
            limit = limit.min(d);
        }
        limit
    }
    pub fn begin_setup(&mut self) {
        if self.setup_deadline.is_none() {
            self.setup_deadline = Some(self.limit(Duration::from_secs(10)));
        }
    }
    pub fn finish_setup(&mut self) {
        self.setup_deadline = None;
    }
    #[cfg(test)]
    pub fn connect(options: &Options, deadline: Option<Instant>) -> Result<Self> {
        Self::connect_inner(options, deadline, None)
    }
    pub(crate) fn connect_owned(
        options: &Options,
        deadline: Option<Instant>,
        ownership: umbra_platform::shutdown::Reservation,
    ) -> Result<Self> {
        let result = Self::connect_inner(options, deadline, Some(ownership.clone()));
        if result.is_err() {
            ownership.creation_failed();
        }
        result
    }
    fn connect_inner(
        options: &Options,
        deadline: Option<Instant>,
        ownership: Option<umbra_platform::shutdown::Reservation>,
    ) -> Result<Self> {
        let executable = match &options.debugserver {
            Some(p) => p.clone(),
            None => discover_debugserver()
                .ok_or_else(|| error("xcode-select", "developer tools unavailable"))?,
        };
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| error("debugserver listen", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| error("debugserver listen", e))?;
        let address = listener
            .local_addr()
            .map_err(|e| error("debugserver listen", e))?;
        let mut command = Command::new(executable);
        command
            .args(["--reverse-connect", &address.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // #205 A2b: debugserver is a member of the terminal job's process
        // group, so ^C, ^\ and `kill -INT/-QUIT/-TERM -pgid` reach it, and it
        // ends on all three (it handles only SIGPIPE and SIGHUP). Ended while
        // its program is held at an intercepted call, it would leave the
        // program to the kernel, which hands it to launchd as a zombie nobody
        // collects. It starts with those three ignored: an ignored disposition
        // survives its `execve`, and nothing in debugserver re-enables the
        // default. The program is spawned by this provider, not by
        // debugserver, so it never inherits this. The platform still ends
        // debugserver with SIGKILL.
        //
        // `pre_exec` makes `std` fork and exec rather than `posix_spawn`. The
        // closure runs only `signal`, which is async-signal-safe.
        // SAFETY: only async-signal-safe libc between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, || {
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    if libc::signal(signal, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| error("debugserver spawn", e))?;
        let mut child = SetupChild {
            child: Some(child),
            ownership: ownership.clone(),
        };
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(
            serde_json::json!({"event": "acquired", "kind": "Tracer", "pid": child.id()}),
        );
        if let Some(slot) = &ownership {
            slot.acquired(umbra_core::shutdown::Identity {
                pid: Some(child.id() as i32),
                birth: None,
            })?;
        }
        let limit = deadline.map_or_else(
            || crate::clock::now() + Duration::from_secs(10),
            |d| d.min(crate::clock::now() + Duration::from_secs(10)),
        );
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    return Err(error("debugserver accept", e));
                }
            }
            if crate::clock::now() >= limit
                || child
                    .try_wait()
                    .map_err(|e| error("debugserver wait", e))?
                    .is_some()
            {
                return Err(error("debugserver connect", "timeout or server exited"));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Darwin inherits O_NONBLOCK from the listener. poll() uses blocking
        // reads with a timeout; an inherited nonblocking socket would return
        // WouldBlock immediately, before debugserver can answer the handshake.
        stream.set_nonblocking(false).map_err(|e| error("rsp", e))?;
        stream.set_nodelay(true).map_err(|e| error("rsp", e))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| error("rsp", e))?;
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        stream
            .set_nonblocking(true)
            .map_err(|e| error("rsp nonblocking", e))?;
        let mut r = Self {
            wakeup: Wakeup::new().map_err(|e| error("rsp wakeup", e))?,
            teardown: false,
            aligned: true,
            ownership,
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stream,
            child: child.child.take().unwrap(),
            reaped: false,
            buffer: Vec::new(),
            no_ack: false,
            deadline,
            setup_deadline: Some(limit),
            torn_down: false,
            #[cfg(feature = "signal-witness")]
            signal_fault_fired: false,
        };
        r.write_packet(b"+", r.limit(Duration::from_secs(2)))?;
        r.ok("QStartNoAckMode")?;
        r.no_ack = true;
        r.request("qSupported:xmlRegisters=arm64")?;
        r.ok("QThreadSuffixSupported")?;
        r.request("QListThreadsInStopReply")?;
        // Kill, never detach, the inferior if this connection is lost
        // (#193). debugserver's default is detach (`g_detach_on_error =
        // true`, debugserver.cpp:67); this packet clears it at runtime and the
        // handler answers `OK` (RNBRemote.cpp:2429-2447, swift-6.3.2), and on a
        // lost connection a running inferior is then killed rather than
        // detached (debugserver.cpp:589-606). Sent before any `vAttach`, so it
        // governs every inferior; any other reply fails `connect`, rather
        // than leaving a traced process able to outlive its tracer.
        r.ok("QSetDetachOnError:0")?;
        Ok(r)
    }
    pub(crate) fn wakeup(&self) -> std::sync::Weak<Wakeup> {
        std::sync::Arc::downgrade(&self.wakeup)
    }
    pub(crate) fn begin_teardown(&mut self) -> bool {
        self.teardown = true;
        self.aligned
    }
    fn ready(&self, event: i16, limit: Instant, finish_frame: bool) -> Result<bool> {
        use std::os::fd::AsRawFd;
        loop {
            if !self.teardown
                && !finish_frame
                && self
                    .wakeup
                    .cancelled
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(error(
                    "rsp",
                    "ordinary work cancelled; stream retained for teardown",
                ));
            }
            let remaining = limit.saturating_duration_since(crate::clock::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: event,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if self.teardown || finish_frame {
                        -1
                    } else {
                        self.wakeup.reader.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: poll receives the valid stack array for this call only.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error("rsp poll", e));
            }
            if fds[1].revents != 0 {
                continue;
            }
            if fds[0].revents != 0 {
                return Ok(true);
            }
        }
    }
    fn write_packet(&mut self, bytes: &[u8], limit: Instant) -> Result<()> {
        #[cfg(feature = "signal-witness")]
        if !self.signal_fault_fired
            && bytes.starts_with(b"$vCont;C")
            && std::env::var("UMBRA_FIX284_SEND_ERROR").ok().as_deref() == Some("completed")
        {
            // The live pairing deliberately makes transport success uncertain:
            // the owned debugger receives the packet, but its caller gets an
            // error. No normal build consults this test-only switch.
            self.signal_fault_fired = true;
            self.stream
                .write_all(bytes)
                .map_err(|e| error("rsp write", e))?;
            return Err(error("rsp write", "f284 possibly completed signal send"));
        }
        #[cfg(test)]
        if bytes.starts_with(b"$vCont;C") {
            SIGNAL_WRITE_ATTEMPTS.with(|count| count.set(count.get() + 1));
            if let Some((accepted, reason)) = SIGNAL_WRITE_FAULT.with(|fault| fault.take()) {
                self.stream
                    .write_all(&bytes[..accepted.min(bytes.len())])
                    .map_err(|e| error("rsp write", e))?;
                return Err(error("rsp write", reason));
            }
        }
        if !self.aligned {
            return Err(error("rsp write", "previous outgoing frame incomplete"));
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let remaining = limit
                .checked_duration_since(crate::clock::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| error("rsp write", "deadline expired"))?;
            self.stream
                .set_write_timeout(Some(read_timeout(remaining)))
                .map_err(|e| error("rsp write", e))?;
            if !self.ready(libc::POLLOUT, limit, offset > 0)? {
                self.aligned = offset == 0;
                return Err(error("rsp write", "deadline expired during frame"));
            }
            match self.stream.write(&bytes[offset..]) {
                Ok(0) => return Err(error("rsp write", "debugserver disconnected")),
                Ok(n) => {
                    offset += n;
                    self.aligned = offset == bytes.len();
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(error("rsp write", e)),
            }
        }
        Ok(())
    }
    pub fn send(&mut self, body: &str) -> Result<()> {
        if std::env::var_os("UMBRA_RSP_LOG").is_some() {
            eprintln!("RSP[{}] > {body}", self.id);
        }
        if self.deadline.is_some_and(|d| crate::clock::now() >= d) {
            return Err(error("watchdog", "session deadline expired"));
        }
        let mut escaped = Vec::new();
        for b in body.bytes() {
            if matches!(b, b'$' | b'#' | b'}' | b'*') {
                escaped.push(b'}');
                escaped.push(b ^ 0x20);
            } else {
                escaped.push(b);
            }
        }
        let sum = escaped.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        let mut packet = vec![b'$'];
        packet.extend(escaped);
        packet.extend(format!("#{sum:02x}").bytes());
        self.write_packet(&packet, self.limit(Duration::from_secs(2)))
    }
    #[allow(dead_code)]
    pub fn interrupt(&mut self) -> Result<()> {
        self.interrupt_until(self.limit(Duration::from_secs(2)))
    }
    pub fn interrupt_until(&mut self, limit: Instant) -> Result<()> {
        self.write_packet(&[3], limit.min(self.limit(Duration::from_secs(2))))
    }
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<String>> {
        self.poll_until(self.limit(timeout))
    }
    /// `poll` against an explicit limit. Teardown reads through this with its
    /// own bound, because the session deadline may already have expired (the
    /// watchdog path) and a teardown still has to read the tracer's reply.
    fn poll_until(&mut self, limit: Instant) -> Result<Option<String>> {
        loop {
            if let Some(start) = self.buffer.iter().position(|b| *b == b'$') {
                if let Some(end) = self.buffer[start + 1..]
                    .iter()
                    .position(|b| *b == b'#')
                    .map(|i| i + start + 1)
                {
                    if self.buffer.len() >= end + 3 {
                        let sum = self.buffer[start + 1..end]
                            .iter()
                            .fold(0u8, |a, b| a.wrapping_add(*b));
                        let expected = number(
                            std::str::from_utf8(&self.buffer[end + 1..end + 3])
                                .map_err(|e| error("rsp checksum", e))?,
                        )?;
                        // debugserver uses a placeholder checksum (#00) once
                        // QStartNoAckMode has disabled acknowledgements.
                        if !self.no_ack && sum as u64 != expected {
                            return Err(error("rsp", "checksum mismatch"));
                        }
                        let raw = self.buffer[start + 1..end].to_vec();
                        self.buffer.drain(..end + 3);
                        if !self.no_ack {
                            self.write_packet(b"+", limit)?;
                        }
                        let mut data = Vec::new();
                        let mut i = 0;
                        while i < raw.len() {
                            if raw[i] == b'}' {
                                i += 1;
                                data.push(
                                    *raw.get(i).ok_or_else(|| error("rsp", "bad escape"))? ^ 0x20,
                                );
                            } else if raw[i] == b'*' {
                                i += 1;
                                let count = *raw.get(i).ok_or_else(|| error("rsp", "bad RLE"))?;
                                if count < 29 || data.is_empty() {
                                    return Err(error("rsp", "bad RLE count"));
                                }
                                let b = *data.last().unwrap();
                                data.extend(std::iter::repeat_n(b, (count - 29) as usize));
                            } else {
                                data.push(raw[i]);
                            }
                            i += 1;
                            if data.len() > 1024 * 1024 {
                                return Err(error("rsp", "decoded packet too large"));
                            }
                        }
                        return String::from_utf8(data)
                            .map(Some)
                            .map_err(|e| error("rsp packet", e));
                    }
                }
            } else {
                self.buffer.clear();
            }
            if crate::clock::now() >= limit {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(read_timeout(
                    limit.saturating_duration_since(crate::clock::now()),
                )))
                .map_err(|e| error("rsp timeout", e))?;
            if !self.ready(libc::POLLIN, limit, false)? {
                return Ok(None);
            }
            let mut bytes = [0; 8192];
            match self.read_progress(&mut bytes) {
                Ok(0) => return Err(error("rsp", "debugserver disconnected")),
                Ok(n) => {
                    if std::env::var_os("UMBRA_RSP_LOG").is_some() {
                        eprintln!(
                            "RSP[{}] < {}",
                            self.id,
                            String::from_utf8_lossy(&bytes[..n])
                        );
                    }
                    self.buffer.extend_from_slice(&bytes[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(error("rsp receive", e)),
            }
            if self.buffer.len() > 1024 * 1024 {
                return Err(error("rsp", "packet exceeds limit"));
            }
        }
    }
    fn read_progress(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if EARLY_PACKET.with(|v| v.get()) && !self.buffer.is_empty() {
            EARLY_PACKET.with(|v| v.set(false));
            let awake = crate::clock::now();
            crate::clock::SCRIPT.with(|v| v.set(Some((awake, awake + Duration::from_secs(86400)))));
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
        } else {
            let len = if EARLY_PACKET.with(|v| v.get()) {
                3
            } else {
                bytes.len()
            };
            self.stream.read(&mut bytes[..len])
        }
        #[cfg(not(test))]
        self.stream.read(bytes)
    }
    pub fn request(&mut self, body: &str) -> Result<String> {
        self.send(body)?;
        let limit = self.limit(Duration::from_secs(5));
        loop {
            let reply = self
                .poll(limit.saturating_duration_since(crate::clock::now()))?
                .ok_or_else(|| error("rsp request", format!("timeout: {body}")))?;
            if reply.starts_with('O') && reply != "OK" {
                continue;
            }
            return Ok(reply);
        }
    }
    pub fn ok(&mut self, body: &str) -> Result<()> {
        let reply = self.request(body)?;
        if reply != "OK" {
            return Err(error("rsp", format!("{body}: {reply}")));
        }
        Ok(())
    }
}
/// The primitives of the per-session ordered teardown (#186), which
/// `native.rs` composes: RSP `k`, read the reply, and only after the tracee's
/// exit is confirmed kill and reap the tracer.
impl Rsp {
    /// The debugserver this connection talks to.
    pub fn tracer_pid(&self) -> i32 {
        self.child.id() as i32
    }
    /// Send the RSP kill packet (`$k#6b`; acknowledgements are off since the
    /// handshake). It bypasses the session deadline, because teardown also runs
    /// after a watchdog expiry, and it is bounded by its own write timeout.
    pub fn send_kill(&mut self, bound: Duration) -> std::io::Result<()> {
        self.teardown_write(b"$k#6b", crate::clock::now() + bound)
    }
    pub(crate) fn teardown_interrupt(&mut self, deadline: Instant) -> std::io::Result<()> {
        self.teardown_write(&[3], deadline)
    }
    fn teardown_write(&mut self, bytes: &[u8], deadline: Instant) -> std::io::Result<()> {
        // Deliberately independent of expired ordinary session/setup deadlines.
        self.write_packet(bytes, deadline)
            .map_err(std::io::Error::other)
    }

    /// One bounded read of whatever the tracer sends during teardown.
    pub fn teardown_read(&mut self, wait: Duration) -> TeardownRead {
        match self.poll_until(crate::clock::now() + wait) {
            Ok(Some(packet)) => TeardownRead::Packet(packet),
            Ok(None) => TeardownRead::Quiet,
            Err(e) => TeardownRead::Closed(e.to_string()),
        }
    }
    /// `SIGKILL` the tracer. Harmless if it has already exited.
    pub fn kill_tracer(&mut self) {
        let _ = self.child.kill();
    }
    /// Reap the tracer with `WNOHANG` polls until `bound`; true once reaped.
    pub fn reap_tracer(&mut self, bound: Duration) -> bool {
        let limit = crate::clock::now() + bound;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    if let Some(slot) = self.ownership.take() {
                        let _ = slot.confirm(umbra_core::shutdown::Proof::Collected {
                            status: status.to_string(),
                        });
                    }
                    return true;
                }
                Ok(None) => {}
                // A caught signal can interrupt the wait (#205 A2a): try again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
            if crate::clock::now() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// Record that the ordered teardown has killed and reaped (or tried to
    /// reap) the tracer.
    pub fn mark_torn_down(&mut self) {
        self.torn_down = true;
    }
}
#[cfg(test)]
impl Rsp {
    pub(crate) fn set_test_ownership(&mut self, slot: umbra_platform::shutdown::Reservation) {
        self.ownership = Some(slot);
    }
    pub(crate) fn retirement_test() -> Self {
        Self::retirement_test_pair().0
    }
    pub(crate) fn retirement_test_pair() -> (Self, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        (
            Self {
                wakeup: Wakeup::new().unwrap(),
                teardown: false,
                aligned: true,
                ownership: None,
                id: 0,
                stream,
                child,
                reaped: true,
                buffer: Vec::new(),
                no_ack: true,
                deadline: None,
                setup_deadline: None,
                torn_down: false,
                #[cfg(feature = "signal-witness")]
                signal_fault_fired: false,
            },
            peer,
        )
    }
}
#[cfg(test)]
thread_local! {
    static RETIREMENT_WAIT_GATES: std::cell::RefCell<std::collections::HashMap<u32, usize>> = Default::default();
}
#[cfg(test)]
impl Rsp {
    pub(crate) fn retirement_live_child(&mut self) {
        assert!(self.reaped);
        self.child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        self.reaped = false;
    }
    /// A tracer recorded as reaped whose pid now names another live process:
    /// the pid-reuse case the teardown prologue must never signal (#205).
    pub(crate) fn retirement_reused_pid(&mut self, other: Child) {
        assert!(self.reaped);
        self.child = other;
    }
    /// Delay visibility, never fabricate a successful owned wait status.
    pub(crate) fn retirement_wait_gate(&mut self, observations: usize) {
        self.reaped = false;
        RETIREMENT_WAIT_GATES.with(|gates| {
            gates.borrow_mut().insert(self.child.id(), observations);
        });
    }
}
impl crate::retirement::Tracer for Rsp {
    fn already_reaped(&self) -> bool {
        self.reaped
    }
    fn try_reap(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        #[cfg(test)]
        if RETIREMENT_WAIT_GATES.with(|gates| {
            let mut gates = gates.borrow_mut();
            let Some(remaining) = gates.get_mut(&self.child.id()) else {
                return false;
            };
            if *remaining == 0 {
                gates.remove(&self.child.id());
                return false;
            }
            if *remaining != usize::MAX {
                *remaining -= 1;
            }
            true
        }) {
            return Ok(None);
        }
        self.child.try_wait().inspect(|status| {
            self.reaped = status.is_some();
            if let Some(status) = status {
                if let Some(slot) = self.ownership.take() {
                    let _ = slot.confirm(umbra_core::shutdown::Proof::Collected {
                        status: status.to_string(),
                    });
                }
            }
        })
    }
    fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
}
impl Drop for Rsp {
    /// The fallback for a connection no session tore down in order: a failed
    /// connect or a failed attach inside `Session::attach`, before umbra has
    /// terminated anything. Every `Session` runs `Session::ordered_teardown`
    /// first instead (#186).
    ///
    /// **Not shown safe.** Measured in the #186 arc: when a tracer dies while
    /// holding an attached, not-yet-terminated inferior, the inferior has been
    /// seen as a PPID-1 zombie within 5 ms. #205 established why: the tracer's
    /// death releases its hold before the tracer's child loop runs, the held
    /// inferior (at a breakpoint, or with an exit pending) starts exiting
    /// first, and the loop hands it to launchd with its original parent still
    /// recorded, so it is never collected. This fallback can therefore leave
    /// such a zombie (#205 R1) and, in launchd's session, take the #186 route.
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        self.kill_tracer();
        let _ = self.reap_tracer(TRACER_REAP_BOUND);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cancellation_preserves_partial_reply_and_same_stream_for_teardown() {
        use std::io::{Read, Write};
        let (mut rsp, mut peer) = Rsp::retirement_test_pair();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        rsp.send("qC").unwrap();
        let mut request = [0; 6];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"$qC#b4");
        peer.write_all(b"$O").unwrap();
        assert!(rsp
            .poll(std::time::Duration::from_millis(10))
            .unwrap()
            .is_none());
        assert_eq!(&rsp.buffer, b"$O");
        rsp.wakeup.signal();
        assert!(rsp.poll(std::time::Duration::from_secs(1)).is_err());
        assert_eq!(&rsp.buffer, b"$O");
        assert!(rsp.begin_teardown());
        peer.write_all(b"K#00").unwrap();
        assert!(
            matches!(rsp.teardown_read(std::time::Duration::from_millis(20)), TeardownRead::Packet(p) if p == "OK")
        );
        rsp.teardown_interrupt(crate::clock::now() + std::time::Duration::from_millis(20))
            .unwrap();
        let mut interrupt = [0];
        peer.read_exact(&mut interrupt).unwrap();
        assert_eq!(interrupt, [3]);
        peer.write_all(b"$T02thread:1;#00").unwrap();
        assert!(
            matches!(rsp.teardown_read(std::time::Duration::from_millis(20)), TeardownRead::Packet(p) if p == "T02thread:1;")
        );
        rsp.send_kill(std::time::Duration::from_millis(20)).unwrap();
        let mut kill = [0; 5];
        peer.read_exact(&mut kill).unwrap();
        assert_eq!(&kill, b"$k#6b");
    }

    use super::*;

    #[test]
    fn debugserver_reverse_connect_no_ack_handshake() {
        // GitHub's macos-14 runner has `xcode-select -p` returning a valid SDK
        // path, but the LLDB.framework debugserver binary is not present in
        // that layout. Probe the actual binary path — not just the SDK — so
        // the test skips cleanly rather than panicking during connect().
        // Resolve it exactly as connect() does, so a host that only names a
        // usable binary through UMBRA_DEBUGSERVER still exercises the
        // handshake instead of skipping.
        let debugserver_path = discover_debugserver();
        let debugserver_present = debugserver_path.as_ref().is_some_and(|p| p.is_file());
        if !debugserver_present {
            let where_ = debugserver_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no UMBRA_DEBUGSERVER and xcode-select unavailable>".into());
            eprintln!(
                "SKIP debugserver_reverse_connect_no_ack_handshake: debugserver missing at {where_}"
            );
            return;
        }
        let mut rsp = Rsp::connect(
            &Options::default(),
            Some(crate::clock::now() + Duration::from_secs(10)),
        )
        .unwrap();
        // Exercise a reply after negotiation, when debugserver sends #00.
        let host = rsp.request("qHostInfo").unwrap();
        assert_eq!(fields(&host).get("ostype"), Some(&"macosx"));
    }

    /// `kp_proc.p_sigignore` of `pid`, from `sysctl(KERN_PROC_PID)`.
    /// `struct kinfo_proc` is 648 bytes on arm64 Darwin with `p_sigignore` at
    /// offset 232 (measured with `offsetof` against the SDK headers; the
    /// `libc` crate does not define `kinfo_proc` for Apple targets). The test
    /// validates the read on a known case before relying on it.
    fn sigignore_of(pid: i32) -> Option<u32> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
        let mut buf = [0u8; 648];
        let mut len = buf.len();
        // SAFETY: `buf` is writable for `len` bytes; the name has 4 components.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == buf.len()).then(|| u32::from_ne_bytes(buf[232..236].try_into().unwrap()))
    }

    fn sig_bit(signal: i32) -> u32 {
        1 << (signal - 1)
    }

    /// F11 (#205 A2b): debugserver is a member of the terminal job's group,
    /// so ^C, ^\ and `kill -INT/-QUIT/-TERM -pgid` reach it, and the shipped
    /// debugserver ends on all three (it handles only SIGPIPE and SIGHUP). If
    /// it ends while its program is held at an intercepted call, the kernel
    /// hands the program to launchd as a zombie nobody collects. umbra
    /// therefore spawns it with those three ignored. Nothing is traced here.
    #[test]
    fn debugserver_ignores_terminal_signals() {
        use std::os::unix::process::CommandExt;
        let path = discover_debugserver();
        if !path.as_ref().is_some_and(|p| p.is_file()) {
            eprintln!("SKIP debugserver_ignores_terminal_signals: debugserver missing at {path:?}");
            return;
        }
        // The instrument on a known case first: a child with SIGUSR1 ignored.
        let mut known = Command::new("/bin/sleep");
        known.arg("30");
        // SAFETY: only async-signal-safe libc between fork and exec.
        unsafe {
            known.pre_exec(|| {
                libc::signal(libc::SIGUSR1, libc::SIG_IGN);
                Ok(())
            });
        }
        let mut known = known.spawn().unwrap();
        let read = sigignore_of(known.id() as i32);
        let _ = known.kill();
        let _ = known.wait();
        let read = read.expect("p_sigignore of a known child");
        assert!(
            read & sig_bit(libc::SIGUSR1) != 0 && read & sig_bit(libc::SIGUSR2) == 0,
            "p_sigignore read {read:#x} does not show the known SIGUSR1 ignore"
        );
        let mut rsp = Rsp::connect(
            &Options::default(),
            Some(crate::clock::now() + Duration::from_secs(10)),
        )
        .unwrap();
        let pid = rsp.tracer_pid();
        let mask = sigignore_of(pid);
        let mut failures = Vec::new();
        for (name, signal) in [
            ("SIGINT", libc::SIGINT),
            ("SIGQUIT", libc::SIGQUIT),
            ("SIGTERM", libc::SIGTERM),
        ] {
            if mask.is_none_or(|m| m & sig_bit(signal) == 0) {
                failures.push(format!("{name} not ignored (p_sigignore {mask:x?})"));
            }
        }
        // Behaviourally too, so the test still holds if the field ever reads
        // empty: send each, and debugserver must still be running 1 s later.
        if failures.is_empty() {
            for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                // SAFETY: our own child, with no program attached.
                unsafe { libc::kill(pid, signal) };
            }
            std::thread::sleep(Duration::from_secs(1));
            if let Ok(Some(status)) = rsp.child.try_wait() {
                rsp.reaped = true;
                failures.push(format!("debugserver ended: {status}"));
            }
        }
        rsp.kill_tracer();
        let reaped = rsp.reaped || rsp.reap_tracer(Duration::from_secs(2));
        assert!(reaped, "debugserver {pid} not reaped");
        assert!(failures.is_empty(), "debugserver {pid}: {failures:?}");
    }
}

// SO_RCVTIMEO has an intentional 1ms floor. A tiny RPC budget can fail closed.
fn read_timeout(remaining: Duration) -> Duration {
    remaining.max(Duration::from_millis(1))
}

#[cfg(test)]
mod poll_bound_tests {
    use super::*;
    #[test]
    fn partial_packet_survives_idle() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let mut rsp = Rsp {
            wakeup: Wakeup::new().unwrap(),
            teardown: false,
            aligned: true,
            ownership: None,
            id: 0,
            stream,
            child,
            reaped: true,
            buffer: Vec::new(),
            no_ack: true,
            deadline: Some(crate::clock::now() + Duration::from_secs(2)),
            setup_deadline: None,
            torn_down: true,
            #[cfg(feature = "signal-witness")]
            signal_fault_fired: false,
        };
        peer.write_all(b"$T05thread:").unwrap();
        assert_eq!(rsp.poll(Duration::from_millis(2)).unwrap(), None);
        assert_eq!(rsp.buffer, b"$T05thread:");
        peer.write_all(b"1;#00").unwrap();
        assert_eq!(
            rsp.poll(Duration::from_millis(20)).unwrap().as_deref(),
            Some("T05thread:1;")
        );
        assert_eq!(rsp.poll(Duration::from_millis(2)).unwrap(), None);
    }

    #[test]
    fn effective_socket_read_floor() {
        for (input, expected) in [
            (0, 1000),
            (250, 1000),
            (999, 1000),
            (1000, 1000),
            (2000, 2000),
        ] {
            assert_eq!(
                read_timeout(Duration::from_micros(input)),
                Duration::from_micros(expected)
            );
        }
    }
}

#[cfg(test)]
thread_local! { static EARLY_PACKET:std::cell::Cell<bool>=const{std::cell::Cell::new(false)}; }
#[cfg(test)]
mod early_packet_test {
    use super::*;
    #[test]
    fn partial_packet_survives_early_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let mut rsp = Rsp {
            wakeup: Wakeup::new().unwrap(),
            teardown: false,
            aligned: true,
            ownership: None,
            id: 0,
            stream,
            child,
            reaped: true,
            buffer: vec![],
            no_ack: true,
            deadline: None,
            setup_deadline: None,
            torn_down: true,
            #[cfg(feature = "signal-witness")]
            signal_fault_fired: false,
        };
        peer.write_all(b"$T05thread:1;#00").unwrap();
        EARLY_PACKET.with(|v| v.set(true));
        assert_eq!(
            rsp.poll(Duration::from_secs(2)).unwrap().as_deref(),
            Some("T05thread:1;")
        );
        assert!(rsp.buffer.is_empty());
        crate::clock::SCRIPT.with(|v| v.set(None));
    }
}
