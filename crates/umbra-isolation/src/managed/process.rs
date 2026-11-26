//! Identity checks, independent exit watches and actual parent collection.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use umbra_core::managed::{Participant, Role};

/// Signal only the audit-token instance bracketed by matching birth reads.
/// A successful signal is never interpreted as collection.
pub fn signal_current(participant: Participant, signal: i32) -> io::Result<()> {
    if ![libc::SIGTERM, libc::SIGKILL, libc::SIGCONT].contains(&signal) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    signal_verified(participant, signal)
}

/// Native qualification hold, using the same birth and audit-token checks.
#[cfg(feature = "probe-cancellation-ticket")]
#[doc(hidden)]
pub fn probe_stop_current(participant: Participant) -> io::Result<()> {
    signal_verified(participant, libc::SIGSTOP)
}

fn signal_verified(participant: Participant, signal: i32) -> io::Result<()> {
    if identity(participant.pid, participant.role)? != participant {
        return Err(super::invalid("signal birth mismatch"));
    }
    let (token, _) = crate::census::verified_token(participant.pid as i32)
        .ok_or_else(|| super::invalid("signal audit identity unavailable"))?;
    if identity(participant.pid, participant.role)? != participant {
        return Err(super::invalid(
            "signal identity changed during verification",
        ));
    }
    match crate::census::signal_token(&token, signal) {
        crate::sweep::Signalled::Sent => Ok(()),
        crate::sweep::Signalled::Stale => Err(io::Error::from_raw_os_error(libc::ESRCH)),
        crate::sweep::Signalled::Refused(errno) => Err(io::Error::from_raw_os_error(errno)),
    }
}

/// Child created by the managed posix_spawn boundary. Only its actual parent
/// may construct this handle; an ECHILD result permanently revokes authority.
pub struct SpawnedChild {
    pid: i32,
    status: Option<i32>,
    lost: bool,
}

impl SpawnedChild {
    /// Force only after the caller's original-ticket opportunity expired.
    /// A prior ECHILD or an already collected child never reaches signalling.
    pub fn force_after(
        &mut self,
        participant: Participant,
        ticket: umbra_core::managed::Ticket,
        not_before: u64,
    ) -> io::Result<bool> {
        if participant.pid != self.pid as u32
            || not_before < ticket.started
            || not_before > ticket.deadline
        {
            return Err(super::invalid("invalid helper fallback identity or cutoff"));
        }
        if self.collect()?.is_some() {
            return Ok(false);
        }
        if super::boot_nanos()? < not_before {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        signal_current(participant, libc::SIGKILL)?;
        Ok(true)
    }
    /// Spawn the same registered executable with an explicit endpoint assignment.
    pub fn spawn(
        executable: &std::path::Path,
        args: &[std::ffi::OsString],
        environment: &[(std::ffi::OsString, std::ffi::OsString)],
        new_session: bool,
        endpoints: &[std::os::fd::RawFd],
    ) -> io::Result<Self> {
        let pid =
            crate::spawn::spawn_managed(executable, args, environment, new_session, endpoints)
                .map_err(io::Error::from_raw_os_error)?;
        Ok(Self {
            pid,
            status: None,
            lost: false,
        })
    }

    /// PID is used to establish birth identity before GO, never as signal proof.
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// Actual nonblocking parent wait; notification never fills this receipt.
    pub fn collect(&mut self) -> io::Result<Option<i32>> {
        if self.lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut status = 0;
        // SAFETY: pid is returned by our posix_spawn, and status is writable.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.status = Some(status);
        } else if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.lost = true;
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(self.status)
    }
}

/// Read a process birth identity without treating a PID as sufficient authority.
pub fn identity(pid: u32, role: Role) -> io::Result<Participant> {
    identity_with_parent(pid, role).map(|(participant, _)| participant)
}

/// Read birth and parent from the same kernel process record.
pub fn identity_with_parent(pid: u32, role: Role) -> io::Result<(Participant, u32)> {
    let pid = i32::try_from(pid).map_err(|_| io::ErrorKind::InvalidInput)?;
    if pid <= 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // SAFETY: proc_bsdinfo is a plain C output structure.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    // SAFETY: the output buffer has exactly the supplied size.
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if count != size {
        return Err(io::Error::last_os_error());
    }
    let participant = Participant {
        role,
        pid: pid as u32,
        birth: (info.pbi_start_tvsec, info.pbi_start_tvusec),
    };
    participant.validate().map_err(super::invalid)?;
    Ok((participant, info.pbi_ppid))
}

/// Private queue created after exec; never inherited from another role.
pub struct ProcessWatch {
    queue: OwnedFd,
    participants: Vec<Participant>,
}

impl ProcessWatch {
    /// Check that a transferred queue retains every required startup watch.
    pub fn covers(&self, participants: &[Participant]) -> bool {
        participants
            .iter()
            .all(|participant| self.participants.contains(participant))
    }

    /// Create one queue for this role's complete pre-GO participant inventory.
    pub fn new() -> io::Result<Self> {
        // SAFETY: kqueue takes no pointers and returns a new owned descriptor.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just created and is exclusively owned.
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: fcntl takes an owned descriptor and integer flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            queue,
            participants: Vec::new(),
        })
    }

    /// Validate identity before and after installation; a failure prohibits GO.
    pub fn install(&mut self, participant: Participant) -> io::Result<()> {
        if self.participants.iter().any(|p| p.pid == participant.pid) {
            return Err(super::invalid("duplicate watched participant"));
        }
        if identity(participant.pid, participant.role)? != participant {
            return Err(super::invalid("process identity changed before watch"));
        }
        let fd = self.queue.as_raw_fd();
        let change = libc::kevent {
            ident: participant.pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one initialized input event; no output array requested.
        if unsafe { libc::kevent(fd, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if identity(participant.pid, participant.role)? != participant {
            return Err(super::invalid("process identity changed during watch"));
        }
        self.participants.push(participant);
        Ok(())
    }

    /// Observe exit without claiming a parent wait or depending on channel EOF.
    pub fn exited(&self) -> io::Result<Option<Participant>> {
        // SAFETY: zero initialization is valid for this C output structure.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: one writable event slot and a valid zero timeout.
        let count = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Ok(None);
        }
        if event.flags & libc::EV_ERROR != 0
            || event.filter != libc::EVFILT_PROC
            || event.fflags & libc::NOTE_EXIT == 0
        {
            return Err(super::invalid("invalid process watch event"));
        }
        self.participants
            .iter()
            .find(|p| p.pid as usize == event.ident)
            .copied()
            .map(Some)
            .ok_or_else(|| super::invalid("unknown process watch event"))
    }
}

/// Parent-owned child with a sticky wait result. ECHILD never authorizes a signal.
pub struct Child {
    child: std::process::Child,
    participant: Participant,
    status: Option<std::process::ExitStatus>,
}

impl Child {
    /// Bind a real parent-owned handle to its pre-GO kernel identity.
    /// On validation failure, return ownership for bounded startup rollback.
    pub fn new(
        child: std::process::Child,
        participant: Participant,
    ) -> Result<Self, (io::Error, std::process::Child)> {
        let validation = identity(participant.pid, participant.role).and_then(|found| {
            if child.id() == participant.pid && found == participant {
                Ok(())
            } else {
                Err(super::invalid("owned child identity mismatch"))
            }
        });
        if let Err(error) = validation {
            return Err((error, child));
        }
        Ok(Self {
            child,
            participant,
            status: None,
        })
    }

    /// Actual parent wait receipt; absence or an exit watch is not a receipt.
    pub fn collect(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    /// Stable identity attached to any eventual wait receipt.
    pub fn participant(&self) -> Participant {
        self.participant
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn custody_watch_registration_race() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let setup = (|| -> io::Result<_> {
            let participant = identity(child.id(), Role::Worker)?;
            let mut watch = ProcessWatch::new()?;
            watch.install(participant)?;

            // Force a real kevent registration error while the identity is
            // still live. A regular descriptor cannot act as a process queue.
            let mut broken = ProcessWatch::new()?;
            broken.queue = std::fs::File::open("/dev/null")?.into();
            let registration_error = broken.install(participant).err();
            Ok((
                participant,
                watch,
                registration_error,
                broken.covers(&[participant]),
                child.try_wait()?.is_none(),
            ))
        })();
        // Complete the actual parent wait before asserting, including when
        // registration failed. An exit notification supplies no wait receipt.
        drop(input);
        let status = child.wait().unwrap();
        let (participant, watch, error, covered, alive) = setup.unwrap();
        assert!(status.success());
        assert!(alive, "failed registration was mistaken for collection");
        assert!(
            error.is_some(),
            "kernel watch registration failure was ignored"
        );
        assert!(!covered, "failed watch was advertised as ready");
        assert_eq!(watch.exited().unwrap(), Some(participant));

        // The opposite ordering also must refuse readiness: the process has
        // already exited and its actual parent has collected it.
        let mut late = ProcessWatch::new().unwrap();
        assert!(late.install(participant).is_err());
        assert!(!late.covers(&[participant]));
    }

    #[test]
    fn watch_exit_and_actual_parent_wait_are_distinct() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let setup = (|| -> io::Result<_> {
            let participant = identity(child.id(), Role::Worker)?;
            let mut watch = ProcessWatch::new()?;
            watch.install(participant)?;
            if watch.exited()?.is_some() || child.try_wait()?.is_some() {
                return Err(super::super::invalid("child exited before pipe release"));
            }
            Ok((participant, watch))
        })();
        drop(input);
        let status = child.wait().unwrap();
        let (participant, watch) = setup.unwrap();
        assert!(status.success());
        assert_eq!(watch.exited().unwrap(), Some(participant));
    }
}
