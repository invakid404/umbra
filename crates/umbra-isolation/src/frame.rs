//! The relay <-> guardian protocol: newline-terminated ASCII frames, strictly
//! validated (#193 design R-7).
//!
//! A frame is at most [`MAX_FRAME`] bytes of printable ASCII and ends in `\n`.
//! A partial frame survives a read timeout (the v2 prototype lost one;
//! `experiments.r8.md`, D3v2-ESCP3). Anything malformed or oversized is a
//! protocol error, which both sides treat as losing the peer: fail closed.

use crate::sweep::Kind;
use std::path::PathBuf;

/// The largest frame either side accepts, newline excluded.
pub const MAX_FRAME: usize = 1024;

/// One protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// Guardian -> relay: it leads session `sid` (its own pid).
    Isolated(i32),
    /// Relay -> guardian: proceed; the isolation record's final path.
    Go(PathBuf),
    /// Guardian -> relay: the supervisor was spawned with this pid.
    Started(i32),
    /// Guardian -> relay: the supervisor's raw wait status, how the sweep
    /// ended, and the guardian's first record errno (0 = none).
    Outcome {
        /// Raw `waitpid` status of the supervisor.
        status: i32,
        /// The sweep outcome.
        kind: Kind,
        /// First record write/fsync errno seen by the guardian, 0 if none.
        record_errno: i32,
    },
    /// Guardian -> relay: startup failed (short reason, printable ASCII).
    Fail(String),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.is_empty() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}
fn int(text: &str) -> Option<i32> {
    // Only canonical decimal: no sign games, no leading '+', no whitespace.
    if text.is_empty() || text.len() > 11 {
        return None;
    }
    let v: i32 = text.parse().ok()?;
    (v.to_string() == text).then_some(v)
}

impl Msg {
    /// Encode as one frame, newline included.
    pub fn encode(&self) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        let s = match self {
            Msg::Isolated(s) => format!("ISOLATED {s}"),
            Msg::Go(p) => format!("GO {}", hex(p.as_os_str().as_bytes())),
            Msg::Started(p) => format!("STARTED {p}"),
            Msg::Outcome {
                status,
                kind,
                record_errno,
            } => format!("OUTCOME {status} {} {record_errno}", kind.as_str()),
            Msg::Fail(r) => format!("FAIL {}", r.replace(|c: char| !c.is_ascii_graphic(), "_")),
        };
        let mut v = s.into_bytes();
        v.push(b'\n');
        v
    }

    /// Parse one frame (newline already removed). Strict: any other shape is
    /// `Err`, which callers treat as a lost peer.
    pub fn parse(frame: &[u8]) -> Result<Msg, String> {
        use std::os::unix::ffi::OsStringExt;
        if frame.len() > MAX_FRAME {
            return Err("oversized frame".into());
        }
        if !frame.iter().all(|b| (0x20..0x7f).contains(b)) {
            return Err("malformed frame: non-printable byte".into());
        }
        let text = std::str::from_utf8(frame).map_err(|_| "malformed frame: utf-8")?;
        let parts: Vec<&str> = text.split(' ').collect();
        let bad = || format!("malformed frame: {:?}", &text[..text.len().min(64)]);
        match parts.as_slice() {
            ["ISOLATED", s] => int(s).filter(|v| *v > 1).map(Msg::Isolated).ok_or_else(bad),
            ["STARTED", p] => int(p).filter(|v| *v > 1).map(Msg::Started).ok_or_else(bad),
            ["GO", h] => unhex(h)
                .filter(|b| !b.contains(&0))
                .map(|b| Msg::Go(PathBuf::from(std::ffi::OsString::from_vec(b))))
                .ok_or_else(bad),
            ["OUTCOME", st, k, e] => match (int(st), Kind::parse(k), int(e)) {
                (Some(status), Some(kind), Some(record_errno)) if record_errno >= 0 => {
                    Ok(Msg::Outcome {
                        status,
                        kind,
                        record_errno,
                    })
                }
                _ => Err(bad()),
            },
            ["FAIL", r] if !r.is_empty() => Ok(Msg::Fail(r.to_string())),
            _ => Err(bad()),
        }
    }
}

/// What one bounded read produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// A complete frame (newline removed).
    Frame(Vec<u8>),
    /// The deadline passed with no complete frame; any partial frame is kept.
    Timeout,
    /// EOF, a permanent read/poll error, or an oversized partial frame.
    Lost(String),
}

/// A buffered frame reader over a raw descriptor. Partial input is kept across
/// calls and timeouts.
#[cfg(target_os = "macos")]
pub struct Reader {
    fd: std::os::fd::RawFd,
    buf: Vec<u8>,
}

#[cfg(target_os = "macos")]
impl Reader {
    /// Read frames from `fd` (owned by the caller).
    pub fn new(fd: std::os::fd::RawFd) -> Self {
        Reader { fd, buf: vec![] }
    }

    /// One frame, or `Timeout` at the monotonic `deadline`, or `Lost`.
    pub fn next(&mut self, deadline: std::time::Instant) -> Read {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let frame = self.buf[..i].to_vec();
                self.buf.drain(..=i);
                return Read::Frame(frame);
            }
            if self.buf.len() > MAX_FRAME {
                return Read::Lost("oversized frame".into());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Read::Timeout;
            }
            let ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32 + 1;
            let mut pfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid, initialized pollfd; the count matches.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Read::Lost(format!("poll: {e}"));
            }
            if r == 0 {
                continue;
            }
            let mut chunk = [0u8; 512];
            // SAFETY: the buffer is valid for `chunk.len()` bytes.
            let n = unsafe { libc::read(self.fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n == 0 {
                return Read::Lost("peer closed the channel".into());
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                    _ => return Read::Lost(format!("read: {e}")),
                }
            }
            self.buf.extend_from_slice(&chunk[..n as usize]);
        }
    }
}

/// Write one whole frame, or report the peer lost (EPIPE, short write, error).
#[cfg(target_os = "macos")]
pub fn send(fd: std::os::fd::RawFd, msg: &Msg) -> Result<(), String> {
    let bytes = msg.encode();
    let mut off = 0;
    while off < bytes.len() {
        // SAFETY: the slice is valid for the remaining length.
        let n = unsafe { libc::write(fd, bytes[off..].as_ptr().cast(), bytes.len() - off) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("write: {e}"));
        }
        off += n as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let msgs = [
            Msg::Isolated(4242),
            Msg::Go(PathBuf::from(
                "/Users/a b/Library/Logs/umbra/isolation/x.jsonl",
            )),
            Msg::Started(77),
            Msg::Outcome {
                status: 256,
                kind: Kind::Incomplete,
                record_errno: 28,
            },
            Msg::Fail("setsid".into()),
        ];
        for m in msgs {
            let enc = m.encode();
            assert_eq!(enc.last(), Some(&b'\n'));
            assert_eq!(Msg::parse(&enc[..enc.len() - 1]).unwrap(), m);
        }
    }

    #[test]
    fn malformed_frames_are_rejected() {
        for bad in [
            &b"ISOLATED"[..],
            b"ISOLATED  12",
            b"ISOLATED 1",
            b"ISOLATED +12",
            b"ISOLATED 012",
            b"STARTED -5",
            b"OUTCOME 0 COMPLETE",
            b"OUTCOME 0 complete 0",
            b"OUTCOME 0 COMPLETE -1",
            b"OUTCOME x COMPLETE 0",
            b"GO zz",
            b"GO 00",
            b"HELLO 1",
            b"ISOLATED 12\x00",
            b"FAIL ",
            b"",
        ] {
            assert!(
                Msg::parse(bad).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let big = vec![b'A'; MAX_FRAME + 1];
        assert_eq!(Msg::parse(&big).unwrap_err(), "oversized frame");
    }

    /// Close-on-exec, as production's: a child another test spawns
    /// concurrently must not inherit the write end and hold EOF off.
    #[cfg(target_os = "macos")]
    fn pipe() -> (std::os::fd::RawFd, std::os::fd::RawFd) {
        crate::spawn::pipe().unwrap()
    }

    /// A frame split across a read timeout is not lost (the v2 defect).
    #[cfg(target_os = "macos")]
    #[test]
    fn partial_frame_survives_a_timeout() {
        use std::time::{Duration, Instant};
        let (r, w) = pipe();
        let mut reader = Reader::new(r);
        // SAFETY: a valid buffer of the stated length.
        unsafe { libc::write(w, b"OUTCOME 0 COMP".as_ptr().cast(), 14) };
        assert_eq!(
            reader.next(Instant::now() + Duration::from_millis(30)),
            Read::Timeout
        );
        // SAFETY: as above.
        unsafe { libc::write(w, b"LETE 0\n".as_ptr().cast(), 7) };
        match reader.next(Instant::now() + Duration::from_millis(500)) {
            Read::Frame(f) => assert_eq!(
                Msg::parse(&f).unwrap(),
                Msg::Outcome {
                    status: 0,
                    kind: Kind::Complete,
                    record_errno: 0
                }
            ),
            other => panic!("{other:?}"),
        }
        // SAFETY: closing descriptors this test owns.
        unsafe {
            libc::close(w);
        }
        assert!(matches!(
            reader.next(Instant::now() + Duration::from_millis(200)),
            Read::Lost(_)
        ));
        // SAFETY: as above.
        unsafe {
            libc::close(r);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_oversized_partial_frame_is_a_lost_peer() {
        use std::time::{Duration, Instant};
        let (r, w) = pipe();
        let mut reader = Reader::new(r);
        let big = vec![b'A'; MAX_FRAME + 64];
        // SAFETY: a valid buffer of the stated length (below the pipe buffer).
        unsafe { libc::write(w, big.as_ptr().cast(), big.len()) };
        assert!(matches!(
            reader.next(Instant::now() + Duration::from_millis(500)),
            Read::Lost(_)
        ));
        // SAFETY: closing descriptors this test owns.
        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }
}
