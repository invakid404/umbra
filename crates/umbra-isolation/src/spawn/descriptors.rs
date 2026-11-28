//! Parent-prepared descriptor exclusions for fork-to-exec children.
use std::{io, os::fd::RawFd};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: libc::dev_t,
    inode: libc::ino_t,
    mode: libc::mode_t,
}

fn identity(fd: RawFd) -> io::Result<Option<Identity>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to a complete writable object.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EBADF) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: successful fstat initialized the object.
    let stat = unsafe { stat.assume_init() };
    Ok(Some(Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
        mode: stat.st_mode,
    }))
}

/// Descriptors present in the parent that an exec child must not inherit.
///
/// Capture after preparing the command and before spawning it. The caller must
/// introduce no new managed endpoints across that boundary. Identity checks
/// protect descriptors reused by spawn itself, including its error pipe.
pub struct ClosePlan(Vec<(RawFd, Identity)>);

impl ClosePlan {
    /// Capture non-stdio descriptors, except an explicit inspection allowlist.
    /// Production debugger launches use an empty allowlist.
    pub fn capture(keep: &[RawFd]) -> io::Result<Self> {
        // SAFETY: getdtablesize takes no pointers and runs in the parent.
        let limit = unsafe { libc::getdtablesize() };
        if limit < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut descriptors = Vec::new();
        for fd in 3..limit {
            if !keep.contains(&fd) {
                if let Some(identity) = identity(fd)? {
                    descriptors.push((fd, identity));
                }
            }
        }
        Ok(Self(descriptors))
    }

    /// Close only descriptors that still match the parent snapshot.
    ///
    /// # Safety
    /// Call only in the forked child, before exec or its first ordinary
    /// operation. The child must not concurrently create descriptors. This
    /// method uses no allocation, locks, logging, or destructors.
    pub unsafe fn close_in_child(&self) -> io::Result<()> {
        for (fd, expected) in &self.0 {
            if identity(*fd)? == Some(*expected) {
                // SAFETY: the caller confines this operation to its forked
                // child; the descriptor still matches the parent's snapshot.
                if unsafe { libc::close(*fd) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd},
        os::unix::net::UnixStream,
    };

    #[test]
    fn fork_close_plan_preserves_spawn_descriptor_reuse() {
        let (old, _old_peer) = UnixStream::pair().unwrap();
        let (replacement, mut replacement_peer) = UnixStream::pair().unwrap();
        replacement.set_nonblocking(true).unwrap();
        let nonce = *uuid::Uuid::new_v4().as_bytes();
        replacement_peer.write_all(&nonce).unwrap();
        let mut inspection = [0; 2];
        assert_eq!(unsafe { libc::pipe(inspection.as_mut_ptr()) }, 0);
        let mut reader = unsafe { std::fs::File::from_raw_fd(inspection[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(inspection[1]) };
        let plan = ClosePlan::capture(&[writer.as_raw_fd()]).unwrap();
        let reused = old.as_raw_fd();
        // Simulate spawn replacing a captured descriptor with its own channel.
        // Only async-signal-safe operations run in this branch.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let mut result = [0u8; 17];
            let copied = unsafe { libc::dup2(replacement.as_raw_fd(), reused) } == reused;
            let closed = unsafe { plan.close_in_child() }.is_ok();
            let received = unsafe { libc::read(reused, result[1..].as_mut_ptr().cast(), 16) };
            result[0] = u8::from(copied && closed && received == 16);
            unsafe {
                libc::write(writer.as_raw_fd(), result.as_ptr().cast(), result.len());
                libc::_exit(0);
            }
        }
        drop(writer);
        let mut result = [0u8; 17];
        let read = reader.read_exact(&mut result);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        read.unwrap();
        assert_eq!(status, 0);
        assert_eq!(result[0], 1, "close plan destroyed a reused descriptor");
        assert_eq!(
            &result[1..],
            &nonce,
            "wrong endpoint survived the close plan"
        );
    }

    #[test]
    fn fork_close_plan_excludes_parent_endpoints() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut inspection = [0; 2];
        // SAFETY: writable descriptor pair; both are owned below.
        assert_eq!(unsafe { libc::pipe(inspection.as_mut_ptr()) }, 0);
        let mut reader = unsafe { std::fs::File::from_raw_fd(inspection[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(inspection[1]) };
        let plan = ClosePlan::capture(&[writer.as_raw_fd()]).unwrap();
        let endpoints = [left.as_raw_fd(), right.as_raw_fd()];
        // SAFETY: child only calls async-signal-safe operations and _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let mut result = [0u8; 3];
            result[0] = u8::from(unsafe { plan.close_in_child() }.is_ok());
            for (i, fd) in endpoints.iter().enumerate() {
                result[i + 1] = u8::from(
                    unsafe { libc::fcntl(*fd, libc::F_GETFD) } < 0
                        && io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
                );
            }
            unsafe {
                libc::write(writer.as_raw_fd(), result.as_ptr().cast(), result.len());
                libc::_exit(0);
            }
        }
        drop(writer);
        let mut result = [0; 3];
        let read = reader.read_exact(&mut result);
        let mut status = 0;
        // SAFETY: pid is this test's actual child; retain the real parent wait.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        read.unwrap();
        assert_eq!(status, 0);
        assert_eq!(result, [1, 1, 1], "fork child retained parent endpoints");
        assert!(identity(left.as_raw_fd()).unwrap().is_some());
        assert!(identity(right.as_raw_fd()).unwrap().is_some());
    }
}
