//! Fixture file changes wake the production guard wait without a polling timer.
use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};
use umbra_isolation::managed::guard::Guard;

pub struct Activity {
    queue: OwnedFd,
    _directories: Vec<File>,
}

impl Activity {
    pub fn new(root: &Path, clock: &Path, rescue: bool) -> io::Result<Self> {
        // SAFETY: kqueue returns a fresh owned descriptor without pointer arguments.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is newly owned by this fixture.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fcntl operates on this fixture's valid descriptor.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let directories = vec![File::open(root)?, File::open(clock.parent().unwrap())?];
        let mut changes: Vec<_> = directories
            .iter()
            .map(|directory| libc::kevent {
                ident: directory.as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE,
                data: 0,
                udata: std::ptr::null_mut(),
            })
            .collect();
        if rescue {
            // One invalidating observation wake after the existing three-second
            // harness bound. The callback records failure and advances only the
            // original ticket; this is never a production cleanup deadline.
            changes.push(libc::kevent {
                ident: 1,
                filter: libc::EVFILT_TIMER,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: 0,
                data: 3100,
                udata: std::ptr::null_mut(),
            });
        }
        // SAFETY: initialized change array, no output events requested.
        if unsafe {
            libc::kevent(
                raw,
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            queue,
            _directories: directories,
        })
    }

    pub fn wait(&self, guard: &mut Guard) -> io::Result<()> {
        guard.wait_for_activity(Some((self.queue.as_fd(), false, u64::MAX)))?;
        loop {
            // SAFETY: initialized event storage and a zero timeout for draining.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
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
            if count == 0 {
                return Ok(());
            }
            if event.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(event.data as i32));
            }
        }
    }
}
