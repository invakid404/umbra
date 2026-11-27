//! Descriptor readiness and explicit wakeups under an absolute boot-clock cutoff.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(super) struct Readiness(OwnedFd);

impl Readiness {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: kqueue has no pointer arguments and returns a new owned descriptor.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh descriptor, owned only here.
        let queue = Self(unsafe { OwnedFd::from_raw_fd(raw) });
        // SAFETY: fcntl operates on our valid descriptor.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        queue.change(1, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0)?;
        Ok(queue)
    }

    fn change(&self, ident: usize, filter: i16, flags: u16, fflags: u32) -> io::Result<()> {
        let change = libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one initialized input, no output array.
        if unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn wake(&self) -> io::Result<()> {
        self.change(1, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER)
    }

    pub(super) fn wait(&self, fd: RawFd, writing: bool, deadline: u64) -> io::Result<()> {
        self.change(
            fd as usize,
            if writing {
                libc::EVFILT_WRITE
            } else {
                libc::EVFILT_READ
            },
            libc::EV_ADD | libc::EV_ONESHOT,
            0,
        )?;
        loop {
            let remaining = deadline
                .checked_sub(super::boot_nanos()?)
                .filter(|n| *n != 0)
                .ok_or(io::ErrorKind::TimedOut)?;
            let timeout = libc::timespec {
                tv_sec: (remaining / 1_000_000_000) as libc::time_t,
                tv_nsec: (remaining % 1_000_000_000) as libc::c_long,
            };
            // SAFETY: initialized output storage and finite timeout, no input changes.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::kevent(
                    self.0.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            if result > 0 {
                if event.flags & libc::EV_ERROR != 0 {
                    return Err(io::Error::from_raw_os_error(event.data as i32));
                }
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            // Recheck the original absolute cutoff after interruptions or timer expiry.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn explicit_wake_cannot_renew_original_cutoff() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let wait = Readiness::new().unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 20_000_000;
        wait.wake().unwrap();
        wait.wait(stream.as_raw_fd(), false, deadline).unwrap();
        assert_eq!(
            wait.wait(stream.as_raw_fd(), false, deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        // A subsequent wake cannot authorize a wait after the original cutoff.
        wait.wake().unwrap();
        assert_eq!(
            wait.wait(stream.as_raw_fd(), false, deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn full_output_wait_keeps_original_cutoff() {
        use std::io::Write;
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let bytes = [0u8; 8192];
        loop {
            match stream.write(&bytes) {
                Ok(n) => assert!(n > 0),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let wait = Readiness::new().unwrap();
        let deadline = super::super::boot_nanos().unwrap() + 20_000_000;
        assert_eq!(
            wait.wait(stream.as_raw_fd(), true, deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn peer_closure_wakes_read_wait() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let wait = Readiness::new().unwrap();
        drop(peer);
        wait.wait(
            stream.as_raw_fd(),
            false,
            super::super::boot_nanos().unwrap() + 1_000_000_000,
        )
        .unwrap();
        let mut byte = [0];
        assert_eq!(std::io::Read::read(&mut &stream, &mut byte).unwrap(), 0);
    }
}

/// Wait for any selected route; expiry returns to its state machine.
pub(super) fn wait_descriptors(descriptors: &mut [libc::pollfd], deadline: u64) -> io::Result<()> {
    if descriptors.is_empty() {
        return Ok(());
    }
    loop {
        let now = super::boot_nanos()?;
        if now >= deadline {
            return Ok(());
        }
        let millis = (deadline - now).div_ceil(1_000_000).min(i32::MAX as u64) as i32;
        // SAFETY: live exclusively borrowed route descriptors and initialized array.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                millis,
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        // An interruption or timeout never creates a new deadline.
    }
}
