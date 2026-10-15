//! The macOS [`Host`] for the session sweep: process census, kernel-backed
//! identity, and signal by audit token (#193 design R-8, T1 §3).

use crate::sweep::{Claimant, Host, Signalled, Token};
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

/// The macOS process table, with a pluggable anchor check.
pub struct MacHost<A: FnMut() -> bool> {
    /// The ownership proof (`t1-protocol.md` §2): the relay's checks its
    /// unreaped guardian; the guardian's own sweep passes `|| true`.
    pub anchor: A,
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
                _ => continue,
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
        let mut t = *token;
        // SAFETY: `t` is a valid audit_token_t for the duration of the call.
        match unsafe { proc_signal_with_audittoken(&mut t, libc::SIGKILL) } {
            0 => Signalled::Sent,
            libc::ESRCH => Signalled::Stale,
            e => Signalled::Refused(e),
        }
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_millis(20));
    }
}
