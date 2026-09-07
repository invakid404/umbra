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
        .chunks_exact(2)
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
    stream: TcpStream,
    child: Child,
    buffer: Vec<u8>,
    no_ack: bool,
    deadline: Instant,
}
impl Rsp {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn connect(options: &Options, deadline: Instant) -> Result<Self> {
        let executable = match &options.debugserver {
            Some(p) => p.clone(),
            None => {
                let out = Command::new("/usr/bin/xcode-select")
                    .arg("-p")
                    .output()
                    .map_err(|e| error("xcode-select", e))?;
                if !out.status.success() {
                    return Err(error("xcode-select", "developer tools unavailable"));
                }
                std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
                    .join("Library/PrivateFrameworks/LLDB.framework/Resources/debugserver")
            }
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
        let mut r = Self {
            stream,
            child,
            buffer: Vec::new(),
            no_ack: false,
            deadline,
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
            eprintln!("RSP > {body}");
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
        let limit = (Instant::now() + timeout).min(self.deadline);
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
                        eprintln!("RSP < {}", String::from_utf8_lossy(&bytes[..n]));
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
impl Drop for Rsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debugserver_reverse_connect_no_ack_handshake() {
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
