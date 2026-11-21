//! The macOS [`Host`] for the session sweep: process census, kernel-backed
//! identity, and signal by audit token (#193 design R-8, T1 §3).

use crate::sweep::{token_pid, token_version, Claimant, Host, Member, Signalled, Token};
use std::collections::HashMap;
use std::time::{Duration, Instant};

extern "C" {
    // libproc (libSystem): signal the process an audit token names. Fails with
    // ESRCH unless the token's pid version matches the live process
    // (xnu-12377.121.6 kern_proc.c:526-547 via proc_info.c:3596-3638).
    // Returns 0 or an errno value.
    fn proc_signal_with_audittoken(token: *mut Token, sig: libc::c_int) -> libc::c_int;
}

/// The audit token of `pid`, read twice through one task **name** port, with
/// `getsid(pid)` between the reads. Both reads must name the same pid version,
/// so the session id returned belongs to the instance the token names: a pid
/// reused in between fails the second read (the old name port is dead) or
/// changes the version.
pub fn verified_token(pid: i32) -> Option<(Token, i32)> {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::port::{mach_port_t, MACH_PORT_NULL};
    use mach2::task_info::{task_info_t, TASK_AUDIT_TOKEN, TASK_AUDIT_TOKEN_COUNT};
    use mach2::traps::{mach_task_self, task_name_for_pid};
    let mut name: mach_port_t = MACH_PORT_NULL;
    // SAFETY: valid out-pointer; mach_task_self() is this task's port.
    if unsafe { task_name_for_pid(mach_task_self(), pid, &mut name) } != KERN_SUCCESS {
        return None;
    }
    let read = |out: &mut Token| -> bool {
        let mut count = TASK_AUDIT_TOKEN_COUNT;
        // SAFETY: `out` is an 8 x u32 audit_token_t buffer and `count` its size
        // in natural_t units, as TASK_AUDIT_TOKEN requires.
        unsafe {
            mach2::task::task_info(
                name,
                TASK_AUDIT_TOKEN,
                out.as_mut_ptr() as task_info_t,
                &mut count,
            ) == KERN_SUCCESS
        }
    };
    let mut a: Token = [0; 8];
    let mut b: Token = [0; 8];
    let first = read(&mut a);
    // SAFETY: getsid has no memory arguments.
    let sid = unsafe { libc::getsid(pid) };
    let second = read(&mut b);
    // SAFETY: deallocating the send right task_name_for_pid gave us.
    unsafe {
        mach2::mach_port::mach_port_deallocate(mach_task_self(), name);
    }
    (first && second && a[5] as i32 == pid && a[7] == b[7] && sid > 0).then_some((a, sid))
}

/// Every live pid, or `Err` when the list cannot be read completely.
fn all_pids() -> Result<Vec<i32>, String> {
    for _ in 0..4 {
        // SAFETY: a null buffer asks for the current count.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Err(format!("proc_listallpids: {n}"));
        }
        let cap = n as usize + 64;
        let mut buf = vec![0i32; cap];
        let bytes = (cap * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: the buffer holds `cap` pids, `bytes` bytes.
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return Err(format!("proc_listallpids: {got}"));
        }
        // A full buffer may be truncated: retry with a larger one.
        if (got as usize) < cap {
            buf.truncate(got as usize);
            return Ok(buf);
        }
    }
    Err("proc_listallpids: list kept growing".into())
}

/// Whether `pid` is a zombie (`SZOMB`), or `None` if it is gone.
fn is_zombie(pid: i32) -> Option<bool> {
    // SAFETY: proc_bsdinfo is plain old data.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    Some(info.pbi_status == libc::SZOMB)
}

/// `pbi_flags` bit for a traced process (`PROC_FLAG_TRACED`, `P_LTRACED`;
/// bsd/sys/proc_info.h).
const PROC_FLAG_TRACED: u32 = 2;

/// `proc_bsdinfo` of `pid`; `zombie` asks for the zombie fallback (`arg = 1`).
fn bsd_info(pid: i32, zombie: bool) -> Option<libc::proc_bsdinfo> {
    // SAFETY: proc_bsdinfo is plain old data.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            u64::from(zombie),
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info)
}

/// Whether `pid`'s image is a debugger process (`debugserver`), for waiting
/// only. `None` when the path cannot be read.
fn is_debugger(pid: i32) -> Option<bool> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| buf[..n as usize].ends_with(b"/debugserver"))
}

/// The only child of `parent` whose session is `sid`, if there is exactly
/// one (#205). The relay uses it, while the guardian is still its unreaped
/// child, to name the supervisor the guardian spawned before `STARTED`
/// arrived: the guardian spawns nothing else. Naming only: the supervisor is
/// then ended by its own audit token.
pub fn only_child_in_session(parent: i32, sid: i32) -> Option<i32> {
    let mut pids = [0i32; 64];
    // SAFETY: `pids` is writable for its size in bytes.
    let n = unsafe {
        libc::proc_listchildpids(
            parent,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    if n <= 0 {
        return None;
    }
    // libproc returns the number of pids it wrote.
    let children: Vec<i32> = pids[..(n as usize).min(pids.len())]
        .iter()
        .copied()
        // SAFETY: getsid has no memory arguments.
        .filter(|&pid| pid > 1 && unsafe { libc::getsid(pid) } == sid)
        .collect();
    match children[..] {
        [only] => Some(only),
        _ => None,
    }
}

/// The macOS process table, with a pluggable anchor check.
pub struct MacHost<A: FnMut() -> bool> {
    /// The ownership proof (the zombie anchor; README, Cleanup and
    /// ownership): the relay's checks its unreaped guardian; the guardian's
    /// own sweep passes `|| true`.
    pub anchor: A,
    /// Start identity of every member instance the drain saw, keyed by
    /// (pid, pid version), so a later zombie check compares the same instance.
    starts: HashMap<(i32, u32), (u64, u64)>,
}

impl<A: FnMut() -> bool> MacHost<A> {
    /// A host with the given anchor check.
    pub fn new(anchor: A) -> Self {
        MacHost {
            anchor,
            starts: HashMap::new(),
        }
    }
}

fn signal_token(token: &Token, signal: libc::c_int) -> Signalled {
    let mut t = *token;
    // SAFETY: `t` is a valid audit_token_t for the duration of the call.
    match unsafe { proc_signal_with_audittoken(&mut t, signal) } {
        0 => Signalled::Sent,
        libc::ESRCH => Signalled::Stale,
        e => Signalled::Refused(e),
    }
}

impl<A: FnMut() -> bool> Host for MacHost<A> {
    fn anchor_ok(&mut self) -> bool {
        (self.anchor)()
    }
    fn census(&mut self, sid: i32, deadline: Instant) -> Result<Vec<Claimant>, String> {
        let me = std::process::id() as i32;
        let mut out = vec![];
        for pid in all_pids()? {
            // The deadline is checked before every per-process step.
            if Instant::now() >= deadline {
                return Err("census cut at the deadline".into());
            }
            if pid <= 1 || pid == me {
                continue;
            }
            // SAFETY: getsid has no memory arguments.
            if unsafe { libc::getsid(pid) } != sid {
                continue;
            }
            match is_zombie(pid) {
                Some(false) => {}
                Some(true) => continue,
                // `proc_pidinfo` gave no answer. That is what a zombie or an
                // exited pid gives, but also a live member whose info cannot
                // be read. Never read that as "not a member": re-check the
                // session and report a member as a tokenless claimant, which
                // the sweep never enrolls or signals and which ends it
                // UNVERIFIABLE. A zombie or exited pid fails `getsid` (xnu
                // kern_exit.c:2269 marks the zombie P_REF_DEAD, so
                // kern_prot.c:269-275 `proc_find` refuses it), so neither is
                // added.
                None => {
                    // SAFETY: getsid has no memory arguments.
                    if unsafe { libc::getsid(pid) } == sid {
                        out.push(Claimant { pid, token: None });
                    }
                    continue;
                }
            }
            match verified_token(pid) {
                Some((t, s)) if s == sid => out.push(Claimant {
                    pid,
                    token: Some(t),
                }),
                Some(_) => {} // left the session in between: not a claimant
                // SAFETY: as above.
                None if unsafe { libc::getsid(pid) } == sid => {
                    out.push(Claimant { pid, token: None })
                }
                None => {}
            }
        }
        Ok(out)
    }
    fn kill(&mut self, token: &Token) -> Signalled {
        signal_token(token, libc::SIGKILL)
    }
    fn members(&mut self, sid: i32, deadline: Instant) -> Result<Vec<Member>, String> {
        let me = std::process::id() as i32;
        let mut out = vec![];
        for pid in all_pids()? {
            if Instant::now() >= deadline {
                return Err("member census cut at the deadline".into());
            }
            // SAFETY: getsid has no memory arguments.
            if pid <= 1 || pid == me || unsafe { libc::getsid(pid) } != sid {
                continue;
            }
            let Some(info) = bsd_info(pid, false) else {
                // Unreadable but still in S: the drain waits on it rather
                // than assuming it is not a debugger.
                // SAFETY: getsid has no memory arguments.
                if unsafe { libc::getsid(pid) } == sid {
                    out.push(Member {
                        claimant: Claimant { pid, token: None },
                        debugger: true,
                        traced: false,
                        stopped: false,
                    });
                }
                continue;
            };
            if info.pbi_status == libc::SZOMB {
                continue;
            }
            let token = match verified_token(pid) {
                Some((t, s)) if s == sid => Some(t),
                Some(_) => continue, // left the session in between
                None => None,
            };
            if let Some(t) = token {
                self.starts.insert(
                    (token_pid(&t), token_version(&t)),
                    (info.pbi_start_tvsec, info.pbi_start_tvusec),
                );
            }
            out.push(Member {
                claimant: Claimant { pid, token },
                debugger: is_debugger(pid).unwrap_or(true),
                traced: info.pbi_flags & PROC_FLAG_TRACED != 0,
                stopped: info.pbi_status == libc::SSTOP,
            });
        }
        Ok(out)
    }
    fn cont(&mut self, token: &Token) -> Signalled {
        signal_token(token, libc::SIGCONT)
    }
    fn end_supervisor(&mut self, pid: i32, sid: i32) -> Signalled {
        match verified_token(pid) {
            Some((t, s)) if s == sid => signal_token(&t, libc::SIGKILL),
            _ => Signalled::Stale,
        }
    }
    fn launchd_zombie(&mut self, token: &Token) -> bool {
        let pid = token_pid(token);
        let Some(start) = self.starts.get(&(pid, token_version(token))).copied() else {
            return false;
        };
        bsd_info(pid, true).is_some_and(|info| {
            info.pbi_status == libc::SZOMB
                && info.pbi_ppid == 1
                && (info.pbi_start_tvsec, info.pbi_start_tvusec) == start
        })
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_millis(20));
    }
}
