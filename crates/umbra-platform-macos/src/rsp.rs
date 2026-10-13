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
pub struct Rsp {
    /// Debug aid: distinguishes concurrent connections in UMBRA_RSP_LOG output.
    pub id: u32,
    stream: TcpStream,
    child: Child,
    buffer: Vec<u8>,
    no_ack: bool,
    deadline: Instant,
    /// Set once the owning session's ordered teardown has killed and reaped
    /// the tracer, so `Drop` does not do it a second time.
    torn_down: bool,
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
        self.deadline
    }
    pub fn connect(options: &Options, deadline: Instant) -> Result<Self> {
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
        let mut child = Command::new(executable)
            .args(["--reverse-connect", &address.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| error("debugserver spawn", e))?;
        let limit = deadline.min(Instant::now() + Duration::from_secs(10));
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error("debugserver accept", e));
                }
            }
            if Instant::now() >= limit
                || child
                    .try_wait()
                    .map_err(|e| error("debugserver wait", e))?
                    .is_some()
            {
                let _ = child.kill();
                let _ = child.wait();
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
        let mut r = Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stream,
            child,
            buffer: Vec::new(),
            no_ack: false,
            deadline,
            torn_down: false,
        };
        r.stream
            .write_all(b"+")
            .map_err(|e| error("rsp handshake", e))?;
        r.ok("QStartNoAckMode")?;
        r.no_ack = true;
        r.request("qSupported:xmlRegisters=arm64")?;
        r.ok("QThreadSuffixSupported")?;
        r.request("QListThreadsInStopReply")?;
        r.request("QSetDetachOnError:0")?;
        Ok(r)
    }
    pub fn send(&mut self, body: &str) -> Result<()> {
        if std::env::var_os("UMBRA_RSP_LOG").is_some() {
            eprintln!("RSP[{}] > {body}", self.id);
        }
        if Instant::now() >= self.deadline {
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
        self.stream
            .write_all(&packet)
            .map_err(|e| error("rsp send", e))
    }
    #[allow(dead_code)]
    pub fn interrupt(&mut self) -> Result<()> {
        self.stream
            .write_all(&[3])
            .map_err(|e| error("rsp interrupt", e))
    }
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<String>> {
        self.poll_until((Instant::now() + timeout).min(self.deadline))
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
                            self.stream
                                .write_all(b"+")
                                .map_err(|e| error("rsp ack", e))?;
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
            if Instant::now() >= limit {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(
                    limit
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))
                .map_err(|e| error("rsp timeout", e))?;
            let mut bytes = [0; 8192];
            match self.stream.read(&mut bytes) {
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
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(error("rsp receive", e)),
            }
            if self.buffer.len() > 1024 * 1024 {
                return Err(error("rsp", "packet exceeds limit"));
            }
        }
    }
    pub fn request(&mut self, body: &str) -> Result<String> {
        self.send(body)?;
        let limit = self.deadline.min(Instant::now() + Duration::from_secs(5));
        loop {
            let reply = self
                .poll(limit.saturating_duration_since(Instant::now()))?
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
        self.stream
            .set_write_timeout(Some(bound.max(Duration::from_millis(1))))?;
        self.stream.write_all(b"$k#6b")
    }
    /// One bounded read of whatever the tracer sends during teardown.
    pub fn teardown_read(&mut self, wait: Duration) -> TeardownRead {
        match self.poll_until(Instant::now() + wait) {
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
        let limit = Instant::now() + bound;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= limit {
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
impl Drop for Rsp {
    /// The fallback for a connection no session tore down in order: a failed
    /// connect or a failed attach inside `Session::attach`, before umbra has
    /// terminated anything. Every `Session` runs `Session::ordered_teardown`
    /// first instead (#186).
    ///
    /// **Not shown safe.** Measured in the #186 arc: when a tracer dies while
    /// holding an attached, not-yet-terminated inferior, the inferior has been
    /// seen as a PPID-1 zombie within 5 ms. It was not handed back to its
    /// original parent, and the reason is not established. So in launchd's
    /// session this fallback may take the #186 route. It is a residual tracked
    /// in #193.
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
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        // Exercise a reply after negotiation, when debugserver sends #00.
        let host = rsp.request("qHostInfo").unwrap();
        assert_eq!(fields(&host).get("ostype"), Some(&"macosx"));
    }
}
