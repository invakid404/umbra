//! Process-generation-bound owner observation. No protocol bytes are consumed.
use crate::{error, unsupported};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    time::Instant,
};
use umbra_core::Result;
#[repr(C)]
#[derive(Default)]
struct Unique {
    uuid: [u8; 16],
    id: u64,
    parent: u64,
    version: i32,
    original: i32,
    reserved: [u64; 2],
}
/// Identity captured from the provider's actual parent before accepting requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerIdentity {
    pid: i32,
    id: u64,
    version: i32,
}
impl OwnerIdentity {
    /// Capture the parent process generation. PID 1 is never an adopted supervisor.
    pub fn parent() -> Result<Self> {
        let pid = unsafe { libc::getppid() };
        if pid <= 1 {
            return Err(unsupported("owner-observation: supervisor already gone"));
        }
        Self::read(pid)
    }
    fn read(pid: i32) -> Result<Self> {
        let mut u = Unique::default();
        // XNU PROC_PIDUNIQIDENTIFIERINFO, including the process generation.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                17,
                0,
                (&mut u as *mut Unique).cast(),
                std::mem::size_of::<Unique>() as i32,
            )
        };
        if n as usize != std::mem::size_of::<Unique>() {
            return Err(unsupported(
                "owner-observation: process generation unavailable",
            ));
        }
        Ok(Self {
            pid,
            id: u.id,
            version: u.version,
        })
    }
}
fn event(id: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident: id,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}
fn queue() -> Result<OwnedFd> {
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(error("owner-observation", std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(error("owner-observation", std::io::Error::last_os_error()));
    }
    Ok(fd)
}
fn register(k: &OwnedFd, mut change: libc::kevent) -> Result<()> {
    change.flags |= libc::EV_RECEIPT;
    let mut receipt = event(0, 0, 0, 0);
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let n = unsafe { libc::kevent(k.as_raw_fd(), &change, 1, &mut receipt, 1, &zero) };
    if n != 1 || receipt.flags & libc::EV_ERROR == 0 || receipt.data != 0 {
        return Err(unsupported(
            "owner-observation: event registration unavailable",
        ));
    }
    Ok(())
}
pub(crate) struct OwnerWatch {
    queue: OwnedFd,
    _stream: UnixStream,
}
impl OwnerWatch {
    pub fn new(identity: OwnerIdentity, stream: UnixStream) -> Result<Self> {
        let mut peer = 0i32;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        let mut uid = 0;
        let mut gid = 0;
        let fd = stream.as_raw_fd();
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut peer as *mut i32).cast(),
                &mut len,
            )
        } != 0
            || peer != identity.pid
            || unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0
            || uid != unsafe { libc::geteuid() }
            || OwnerIdentity::read(peer)? != identity
        {
            return Err(unsupported(
                "owner-observation: supervisor identity mismatch",
            ));
        }
        let queue = queue()?;
        register(
            &queue,
            event(
                peer as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_CLEAR,
                libc::NOTE_EXIT,
            ),
        )?;
        register(
            &queue,
            event(
                fd as usize,
                libc::EVFILT_READ,
                libc::EV_ADD | libc::EV_CLEAR,
                0,
            ),
        )?;
        if OwnerIdentity::read(peer)? != identity {
            return Err(unsupported(
                "owner-observation: supervisor changed during registration",
            ));
        }
        Ok(Self {
            queue,
            _stream: stream,
        })
    }
}
pub(crate) enum Wake {
    Disarm,
    Timeout,
    OwnerGone,
}
pub(crate) struct Waiter {
    queue: OwnedFd,
    _owner: Option<UnixStream>,
    disarm: UnixStream,
}
impl Waiter {
    pub fn new(owner: Option<OwnerWatch>) -> Result<(Self, UnixStream)> {
        let (queue, stream) = match owner {
            Some(o) => (o.queue, Some(o._stream)),
            None => (queue()?, None),
        };
        let (rx, tx) = UnixStream::pair().map_err(|e| error("watchdog", e))?;
        register(
            &queue,
            event(
                rx.as_raw_fd() as usize,
                libc::EVFILT_READ,
                libc::EV_ADD | libc::EV_CLEAR,
                0,
            ),
        )?;
        Ok((
            Self {
                queue,
                _owner: stream,
                disarm: rx,
            },
            tx,
        ))
    }
    pub fn disconnect(&self) {
        if let Some(stream) = &self._owner {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
    pub fn wait(&self, deadline: Option<Instant>) -> Result<Wake> {
        loop {
            let remaining = deadline.map(|d| d.saturating_duration_since(crate::clock::now()));
            let timeout = remaining.map(|d| libc::timespec {
                tv_sec: d.as_secs() as _,
                tv_nsec: d.subsec_nanos() as _,
            });
            let mut events = [event(0, 0, 0, 0); 4];
            let n = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    4,
                    timeout.as_ref().map_or(std::ptr::null(), |t| t),
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error("owner-observation", e));
            }
            if n == 0 {
                return Ok(Wake::Timeout);
            }
            let events = &events[..n as usize];
            if events.iter().any(|e| {
                e.filter == libc::EVFILT_READ && e.ident == self.disarm.as_raw_fd() as usize
            }) {
                return Ok(Wake::Disarm);
            }
            if events.iter().any(|e| e.flags & libc::EV_ERROR != 0) {
                return Err(error("owner-observation", "monitor event failed"));
            }
            if events.iter().any(|e| {
                (e.filter == libc::EVFILT_PROC && e.fflags & libc::NOTE_EXIT != 0)
                    || (e.filter == libc::EVFILT_READ && e.flags & libc::EV_EOF != 0)
            }) {
                return Ok(Wake::OwnerGone);
            }
            // Ordinary readability is not owner loss. EV_CLEAR prevents a spin
            // while the dispatcher, not this observer, consumes protocol bytes.
        }
    }
}
