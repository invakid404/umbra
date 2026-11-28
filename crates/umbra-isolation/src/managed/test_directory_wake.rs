//! File-change wake source for the native setup cancellation test hook.
use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    path::Path,
};

/// Keeps directory registration alive until the test's guard returns.
pub struct DirectoryWake {
    queue: OwnedFd,
    _directory: File,
}

impl DirectoryWake {
    /// Register before checking the trigger, so its creation cannot be missed.
    pub fn new(path: &Path) -> io::Result<Self> {
        let directory = File::open(path)?;
        // SAFETY: kqueue returns a fresh owned descriptor.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is newly owned by this test hook.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: the descriptor is valid and owned here.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: directory.as_raw_fd() as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: initialized change, no output events requested.
        if unsafe { libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            queue,
            _directory: directory,
        })
    }

    /// Descriptor included alongside the existing guard and observer sources.
    pub fn descriptor(&self) -> BorrowedFd<'_> {
        self.queue.as_fd()
    }

    /// Consume the wake after waiting and before rechecking the trigger.
    pub fn drain(&self) -> io::Result<()> {
        // SAFETY: zero is valid output storage and a nonblocking timeout.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid queue, initialized output and timeout, no changes.
        let count = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &zero,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count > 0 && event.flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(event.data as i32));
        }
        Ok(())
    }
}
