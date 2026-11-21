//! #205 per-test guard: launchd-parented zombies attributable to a test.
//!
//! When a debugger process ends while its program has started exiting (or is
//! held at a breakpoint), the kernel hands the program to launchd as a zombie
//! that launchd never collects and that its original parent can no longer
//! `waitpid` (ECHILD). Such a zombie stays until a reboot and counts against
//! the user's process limit.
//!
//! Attribution follows the owner scope's rule: the fixture's image name plus a
//! start time inside the test's window. A recorded (pid, start identity) is an
//! additional exact match, for the processes the test knows by pid. This only
//! counts; nothing is ever signalled on this basis.

use std::time::{SystemTime, UNIX_EPOCH};

const SZOMB: u32 = 5;

/// A wall-clock instant in the units `proc_bsdinfo` reports start times in.
pub fn now() -> (u64, u64) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    (d.as_secs(), u64::from(d.subsec_micros()))
}

fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Every launchd-parented zombie of this user whose image name begins with the
/// 16-character truncation of one of `names` and that started at or after
/// `since`, plus any that matches a `recorded` (pid, start identity) exactly.
/// Each is described as `pid name start=<secs>.<usecs>`.
pub fn launchd_zombies(
    names: &[&str],
    since: (u64, u64),
    recorded: &[(i32, Option<(u64, u64)>)],
) -> Vec<String> {
    // `ps` reads the process table through sysctl, zombies included.
    let out = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid=,stat="])
        .output()
        .expect("ps");
    // A failed census is no reading, never "no zombies": report it as an entry,
    // so a caller's emptiness assertion fails.
    if !out.status.success() {
        return vec![format!(
            "ps failed ({}): no reading: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )];
    }
    // SAFETY: getuid has no arguments.
    let uid = unsafe { libc::getuid() };
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [pid, ppid, stat] = fields[..] else {
            continue;
        };
        if ppid != "1" || !stat.starts_with('Z') {
            continue;
        }
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        // SAFETY: plain old data, filled by proc_pidinfo below.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: `info` is writable for `size` bytes; arg 1 asks for the
        // zombie fallback.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                1,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if got != size || info.pbi_status != SZOMB || info.pbi_ppid != 1 || info.pbi_uid != uid {
            continue;
        }
        let start = (info.pbi_start_tvsec, info.pbi_start_tvusec);
        let name = {
            let long = c_name(&info.pbi_name);
            if long.is_empty() {
                c_name(&info.pbi_comm)
            } else {
                long
            }
        };
        let by_name = start >= since
            && names.iter().any(|n| {
                let short: String = n.chars().take(16).collect();
                name.starts_with(&short)
            });
        let by_record = recorded
            .iter()
            .any(|(p, id)| *p == pid && id.is_none_or(|id| id == start));
        if by_name || by_record {
            found.push(format!("{pid} {name} start={}.{:06}", start.0, start.1));
        }
    }
    found
}
