//! Spawning the guardian and the supervisor: `posix_spawn` of the `umbra`
//! binary itself (#193 ruling O2, fork safety). No `fork()` anywhere, so
//! neither the relay nor the guardian ever runs non-async-signal-safe code in
//! a forked copy of a possibly multi-threaded process.
//!
//! Signal state is passed through as today's `umbra` process would have it,
//! with one exception: SIGPIPE is reset to its default action in the child
//! (`POSIX_SPAWN_SETSIGDEF`). The Rust runtime ignores SIGPIPE in this
//! process; that suppression must never reach the supervisor or anything it
//! starts (ruling, record correctness). Every other disposition and the
//! signal mask are inherited exactly as `execve` would inherit them.

use std::ffi::{CString, OsStr, OsString};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `POSIX_SPAWN_SETSID` (`<sys/spawn.h>`, 0x0400): the child leads a new
/// session (xnu-12377.121.6 kern_exec.c:5140-5145 -> setsid_internal).
pub const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;

extern "C" {
    // <spawn.h> (Apple): pass `fd` to the child even though it is close-on-exec.
    // Not bound by the pinned `libc`. Measured on this host: `adddup2(fd, fd)`
    // does NOT clear close-on-exec, this does.
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

fn cstring(bytes: &[u8]) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| "argument contains a NUL byte".to_string())
}

/// Spawn `exe` with `args` (argv\[0\] included) and exactly `env`.
///
/// `new_session`: lead a new session. `keep`: descriptors to pass even though
/// they are close-on-exec (`posix_spawn_file_actions_addinherit_np`). Every
/// other close-on-exec descriptor stays closed, and every other descriptor is
/// inherited exactly as `execve` would. Returns the child's pid, or the errno.
pub fn spawn(
    exe: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
    new_session: bool,
    keep: &[RawFd],
) -> Result<i32, i32> {
    let path = cstring(exe.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    let argv: Vec<CString> = args
        .iter()
        .map(|a| cstring(a.as_bytes()))
        .collect::<Result<_, _>>()
        .map_err(|_| libc::EINVAL)?;
    let envv: Vec<CString> = env
        .iter()
        .map(|(k, v)| {
            let mut b = k.as_bytes().to_vec();
            b.push(b'=');
            b.extend_from_slice(v.as_bytes());
            cstring(&b)
        })
        .collect::<Result<_, _>>()
        .map_err(|_| libc::EINVAL)?;
    let mut argp: Vec<*mut libc::c_char> = argv.iter().map(|c| c.as_ptr() as *mut _).collect();
    argp.push(std::ptr::null_mut());
    let mut envp: Vec<*mut libc::c_char> = envv.iter().map(|c| c.as_ptr() as *mut _).collect();
    envp.push(std::ptr::null_mut());

    // SAFETY: the attribute and file-action objects are initialized before
    // use and destroyed on every path; every pointer passed to posix_spawn
    // (path, argv, envp) is a NUL-terminated array or string that outlives the
    // call; `pid` is a valid out-pointer.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return Err(libc::ENOMEM);
        }
        if libc::posix_spawn_file_actions_init(&mut actions) != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return Err(libc::ENOMEM);
        }
        let result = (|| {
            let mut defaults: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut defaults);
            libc::sigaddset(&mut defaults, libc::SIGPIPE);
            let r = libc::posix_spawnattr_setsigdefault(&mut attr, &defaults);
            if r != 0 {
                return Err(r);
            }
            let mut flags = libc::POSIX_SPAWN_SETSIGDEF as libc::c_short;
            if new_session {
                flags |= POSIX_SPAWN_SETSID;
            }
            let r = libc::posix_spawnattr_setflags(&mut attr, flags);
            if r != 0 {
                return Err(r);
            }
            for fd in keep {
                let r = posix_spawn_file_actions_addinherit_np(&mut actions, *fd);
                if r != 0 {
                    return Err(r);
                }
            }
            let mut pid: libc::pid_t = 0;
            let r = libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions,
                &attr,
                argp.as_ptr(),
                envp.as_ptr(),
            );
            if r != 0 {
                return Err(r);
            }
            Ok(pid)
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}

/// This process's environment, minus `drop`, plus `add`.
pub fn environment(drop: &[&str], add: &[(&str, String)]) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !drop.iter().any(|d| OsStr::new(d) == k.as_os_str()))
        .filter(|(k, _)| !add.iter().any(|(a, _)| OsStr::new(a) == k.as_os_str()))
        .collect();
    for (k, v) in add {
        env.push((OsString::from(k), OsString::from(v)));
    }
    env
}

/// A close-on-exec pipe: (read end, write end).
pub fn pipe() -> Result<(RawFd, RawFd), i32> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: a valid two-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    for fd in fds {
        set_cloexec(fd);
    }
    Ok((fds[0], fds[1]))
}

/// Mark `fd` close-on-exec.
pub fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor this process owns; no memory arguments.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// Close `fd`, ignoring errors.
pub fn close(fd: RawFd) {
    // SAFETY: closing a descriptor this process owns.
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(pid: i32) -> i32 {
        let mut st = 0;
        // SAFETY: valid out-pointer; `pid` is our child.
        unsafe { libc::waitpid(pid, &mut st, 0) };
        st
    }

    /// The test process ignores SIGPIPE (the Rust runtime does). The spawned
    /// child must not inherit that: a shell that sends itself SIGPIPE dies of it.
    #[test]
    fn sigpipe_suppression_is_not_inherited() {
        let args: Vec<OsString> = ["/bin/sh", "-c", "kill -PIPE $$; echo survived"]
            .iter()
            .map(OsString::from)
            .collect();
        let pid = spawn(
            Path::new("/bin/sh"),
            &args,
            &environment(&[], &[]),
            false,
            &[],
        )
        .unwrap();
        let st = wait(pid);
        assert!(
            libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGPIPE,
            "status {st:#x}"
        );
    }

    #[test]
    fn new_session_leads_its_own_session() {
        let args: Vec<OsString> = [
            "/bin/sh",
            "-c",
            "test \"$(ps -o sess= -p $$)\" != '' && sleep 0.2",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let pid = spawn(
            Path::new("/bin/sh"),
            &args,
            &environment(&[], &[]),
            true,
            &[],
        )
        .unwrap();
        // SAFETY: getsid has no memory arguments.
        let sid = unsafe { libc::getsid(pid) };
        assert_eq!(sid, pid, "the child leads its session");
        wait(pid);
    }

    /// A close-on-exec descriptor named in `keep` reaches the child; one not
    /// named does not.
    #[test]
    fn keep_passes_exactly_the_named_close_on_exec_descriptors() {
        let (r1, w1) = pipe().unwrap();
        let (r2, w2) = pipe().unwrap();
        let script = format!("ls /dev/fd/{w1} >/dev/null 2>&1 || exit 3; ls /dev/fd/{w2} >/dev/null 2>&1 && exit 4; exit 0");
        let args: Vec<OsString> = ["/bin/sh", "-c", &script]
            .iter()
            .map(OsString::from)
            .collect();
        let pid = spawn(
            Path::new("/bin/sh"),
            &args,
            &environment(&[], &[]),
            false,
            &[w1],
        )
        .unwrap();
        let st = wait(pid);
        for fd in [r1, w1, r2, w2] {
            close(fd);
        }
        assert!(
            libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0,
            "status {st:#x}"
        );
    }

    #[test]
    fn environment_drops_and_adds() {
        // SAFETY: test-local variable; tests in this module do not race on it.
        std::env::set_var("UMBRA_ISOLATION_TEST_DROP", "x");
        let env = environment(
            &["UMBRA_ISOLATION_TEST_DROP"],
            &[("UMBRA_ISOLATION_TEST_ADD", "7".into())],
        );
        assert!(!env.iter().any(|(k, _)| k == "UMBRA_ISOLATION_TEST_DROP"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "UMBRA_ISOLATION_TEST_ADD" && v == "7"));
    }
}
