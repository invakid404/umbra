//! Managed role control transport. Each role owns its endpoints and clock state.
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use umbra_core::managed::{decode_frame, encode_frame, MAX_FRAME};
pub mod admission;
pub mod arming;
pub mod bootstrap;
pub mod caller_startup;
pub mod cancellation;
pub mod channels;
pub mod executable;
pub mod fail_stop;
pub mod guard;
pub mod guardian_startup;
pub mod inventory;
pub mod owner;
pub mod owner_startup;
pub mod process;
pub mod provider_startup;
pub mod publication;
mod readiness;
pub mod relay_startup;
pub mod rollback;
pub mod startup;
#[cfg(feature = "test-clock")]
pub mod test_clock;
pub mod topology;
pub mod worker;
pub mod worker_startup;

/// Checked boot-clock timestamp shared by every participant on this machine.
pub fn boot_nanos() -> io::Result<u64> {
    #[cfg(feature = "test-clock")]
    if let Some(value) = test_clock::read() {
        return value;
    }
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec; the clock has no process-local epoch.
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds = u64::try_from(time.tv_sec).map_err(|_| invalid("negative clock"))?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| invalid("negative clock"))?;
    if nanos >= 1_000_000_000 {
        return Err(invalid("invalid clock fraction"));
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|s| s.checked_add(nanos))
        .ok_or_else(|| invalid("clock overflow"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// One nonblocking control endpoint. Partial I/O never renews its deadline.
pub struct Channel {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
}

impl Channel {
    /// Wait for pending output or incoming bytes under the original cutoff.
    pub fn wait_until(&self, deadline: u64) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let (fd, writing) = self.wait_interest();
        readiness::wait_descriptors(
            &mut [libc::pollfd {
                fd: fd.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            }],
            deadline,
        )
    }

    /// Borrow readiness interest without transferring or duplicating ownership.
    /// Pending output must drain before the next input frame is read.
    pub fn wait_interest(&self) -> (std::os::fd::BorrowedFd<'_>, bool) {
        use std::os::fd::AsFd;
        (self.stream.as_fd(), self.written != self.output.len())
    }

    /// No incomplete incoming frame or pending outgoing bytes remain.
    pub fn at_frame_boundary(&self) -> bool {
        self.input.is_empty() && self.written == self.output.len()
    }

    /// Borrow the stream only at a complete frame boundary for the one-shot
    /// startup descriptor transfer. Partial framed I/O forbids changing modes.
    pub fn handoff_stream(&self) -> io::Result<&UnixStream> {
        if !self.input.is_empty() || self.written != self.output.len() {
            return Err(invalid("descriptor handoff during partial control frame"));
        }
        Ok(&self.stream)
    }

    /// Take exclusive ownership and enable nonblocking operation.
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
        })
    }

    /// Queue one frame; refusing a second prevents unbounded producer buffering.
    pub fn queue<T: serde::Serialize>(&mut self, value: &T) -> io::Result<()> {
        if self.written != self.output.len() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "control frame pending",
            ));
        }
        self.output = encode_frame(value).map_err(invalid)?;
        self.written = 0;
        Ok(())
    }

    /// Attempt progress without blocking other participants' parallel publication.
    pub fn flush(&mut self, deadline: u64) -> io::Result<bool> {
        while self.written < self.output.len() {
            check_deadline(deadline)?;
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => self.written += count,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Return only a complete bounded frame. EOF and malformed lengths are errors.
    pub fn receive<T: serde::de::DeserializeOwned>(
        &mut self,
        deadline: u64,
    ) -> io::Result<Option<T>> {
        loop {
            check_deadline(deadline)?;
            let target = if self.input.len() < 4 {
                4
            } else {
                let size = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_FRAME {
                    return Err(invalid("invalid control frame length"));
                }
                4 + size
            };
            if self.input.len() == target && target > 4 {
                let value = decode_frame(&self.input).map_err(invalid)?;
                self.input.clear();
                return Ok(Some(value));
            }
            let mut buffer = [0u8; 8192];
            let count = buffer.len().min(target - self.input.len());
            match self.stream.read(&mut buffer[..count]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => self.input.extend_from_slice(&buffer[..count]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

fn check_deadline(deadline: u64) -> io::Result<()> {
    if boot_nanos()? >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_wait_retains_cutoff_and_endpoint() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(reader).unwrap();
        let deadline = boot_nanos().unwrap() + 20_000_000;
        channel.wait_until(deadline).unwrap();
        assert!(boot_nanos().unwrap() >= deadline);
        writer.write_all(&encode_frame(&42u64).unwrap()).unwrap();
        channel.wait_until(deadline).unwrap();
        assert_eq!(
            channel.receive::<u64>(deadline).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // Timeout preserves the endpoint and unread bytes for owned rollback.
        assert_eq!(channel.receive::<u64>(u64::MAX).unwrap(), Some(42));
    }

    #[test]
    fn startup_wait_wakes_for_peer_closure() {
        let (writer, reader) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(reader).unwrap();
        drop(writer);
        let deadline = boot_nanos().unwrap() + 1_000_000_000;
        channel.wait_until(deadline).unwrap();
        assert!(boot_nanos().unwrap() < deadline);
        assert!(channel.receive::<u64>(deadline).is_err());
    }

    #[test]
    fn partial_control_frame_does_not_publish_or_renew_deadline() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(reader).unwrap();
        let encoded = encode_frame(&42u64).unwrap();
        let deadline = boot_nanos().unwrap().checked_add(1_000_000_000).unwrap();
        writer.write_all(&encoded[..5]).unwrap();
        assert_eq!(channel.receive::<u64>(deadline).unwrap(), None);
        assert_eq!(
            channel.receive::<u64>(0).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        writer.write_all(&encoded[5..]).unwrap();
        assert_eq!(channel.receive::<u64>(deadline).unwrap(), Some(42));
    }

    #[test]
    fn oversized_control_header_fails_before_payload_allocation() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(reader).unwrap();
        writer.write_all(&u32::MAX.to_be_bytes()).unwrap();
        assert_eq!(
            channel.receive::<u64>(u64::MAX).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(channel.input.len(), 4);
    }
}
