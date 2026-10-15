//! #193 T1 protocol on the real kernel: the frozen owned set with the anchor
//! forced off, the audit-token kill, and a sanity check of a zombie leader.
//! The scripted interleavings (T1-a, T1-b, scripted T1-c) are unit tests in
//! `src/sweep.rs`.
//!
//! The anchor-held half of T1-c (no process can obtain a zombie leader's id)
//! is not tested here: XNU allocates pids sequentially (`kern_fork.c:955-974`),
//! so only a full wrap of the pid space could attempt the reuse. It rests on
//! the XNU source (README, Cleanup and ownership) and on one recorded
//! wraparound reading of the #193 arc, which is not part of CI.
#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use umbra_isolation::census::verified_token;
use umbra_isolation::census::MacHost;
use umbra_isolation::spawn::{environment, spawn};
use umbra_isolation::sweep::{sweep, Host, Kind, Signalled};

fn args(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

fn stat(pid: i32) -> String {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sid(pid: i32) -> i32 {
    // SAFETY: getsid takes no memory arguments.
    unsafe { libc::getsid(pid) }
}

fn reap(pid: i32) -> i32 {
    let mut st = 0;
    // SAFETY: a valid out-pointer; `pid` is our child.
    unsafe { libc::waitpid(pid, &mut st, 0) };
    st
}

/// Sanity check only; it attempts no pid reuse. A session leader is left an
/// unreaped zombie while 200 new sessions are created; none gets its pid or
/// session id, and it stays a zombie until the reap. Sequential allocation
/// means 200 spawns cannot come back to its pid, so this cannot show the
/// anchor-held property (module docs).
#[test]
fn zombie_leader_sanity_check_attempts_no_reuse() {
    let env = environment(&[], &[]);
    let s = spawn(
        Path::new("/usr/bin/true"),
        &args(&["true"]),
        &env,
        true,
        &[],
    )
    .unwrap();
    // Wait (without reaping) until it is a zombie.
    for _ in 0..200 {
        if stat(s).starts_with('Z') {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        stat(s).starts_with('Z'),
        "anchor is not a zombie: {:?}",
        stat(s)
    );
    for _ in 0..200 {
        let p = spawn(
            Path::new("/usr/bin/true"),
            &args(&["true"]),
            &env,
            true,
            &[],
        )
        .unwrap();
        assert_ne!(
            p, s,
            "pid {s} was reallocated while its zombie was unreaped"
        );
        assert_ne!(sid(p), s, "a new session carried the anchored id {s}");
        reap(p);
    }
    assert!(
        stat(s).starts_with('Z'),
        "the anchor changed before its reap: {:?}",
        stat(s)
    );
    reap(s);
}

/// T1-c(ii), real kernel: with the anchor forced off (a test build of the
/// relay's fallback), every claimant of the session is reported unverifiable
/// and **no signal is sent**; the processes are still alive afterwards. Then
/// the same session is swept with the anchor held (as the guardian does),
/// which enrolls and kills both by audit token.
#[test]
fn t1_c_anchor_forced_off_never_signals_then_anchor_held_completes() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("member");
    let env = environment(&[], &[]);
    let leader = spawn(
        Path::new("/bin/sh"),
        &[
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("sleep 30 & echo $! > \"$0\"; exec sleep 30"),
            pidfile.clone().into_os_string(),
        ],
        &env,
        true,
        &[],
    )
    .unwrap();
    let mut member = 0;
    for _ in 0..400 {
        if let Ok(t) = std::fs::read_to_string(&pidfile) {
            if let Ok(p) = t.trim().parse::<i32>() {
                member = p;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(member > 0, "member did not start");
    assert_eq!(sid(leader), leader);
    assert_eq!(sid(member), leader);

    let off = sweep(
        &mut MacHost { anchor: || false },
        leader,
        Duration::from_millis(500),
    );
    assert_eq!(off.kind, Kind::Unverifiable, "{off:?}");
    assert_eq!(
        off.sent, 0,
        "a signal was sent without an ownership proof: {off:?}"
    );
    let mut seen = off.unverifiable.clone();
    seen.sort();
    let mut want = vec![leader, member];
    want.sort();
    assert_eq!(seen, want);
    assert!(
        !stat(leader).starts_with('Z') && !stat(member).is_empty(),
        "claimants were harmed"
    );

    let on = sweep(
        &mut MacHost { anchor: || true },
        leader,
        Duration::from_secs(3),
    );
    assert_eq!(on.kind, Kind::Complete, "{on:?}");
    assert!(on.sent >= 2);
    let st = reap(leader);
    assert!(
        libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGKILL,
        "leader status {st:#x}"
    );
}

/// R-8 (P-TOK): kills are by audit token. A token of a process that has been
/// reaped is refused (stale), even though the pid number may be reused; a
/// live process's token kills it.
#[test]
fn a_stale_token_is_refused_and_a_live_one_kills() {
    let env = environment(&[], &[]);
    let gone = spawn(
        Path::new("/bin/sleep"),
        &args(&["sleep", "30"]),
        &env,
        false,
        &[],
    )
    .unwrap();
    // SAFETY: getsid has no memory arguments.
    let (token, s) = verified_token(gone).expect("a token for a live child");
    assert_eq!(s, unsafe { libc::getsid(0) });
    // SAFETY: our own child.
    unsafe { libc::kill(gone, libc::SIGKILL) };
    reap(gone);
    let mut host = MacHost { anchor: || true };
    assert_eq!(host.kill(&token), Signalled::Stale);

    let live = spawn(
        Path::new("/bin/sleep"),
        &args(&["sleep", "30"]),
        &env,
        false,
        &[],
    )
    .unwrap();
    let (token, _) = verified_token(live).expect("a token for a live child");
    assert_eq!(host.kill(&token), Signalled::Sent);
    let st = reap(live);
    assert!(
        libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGKILL,
        "status {st:#x}"
    );
}

/// CR-1: the census reports a member it cannot read as a tokenless claimant,
/// but never a zombie. Here session S's only process is its leader, left an
/// unreaped zombie (as the relay's exited guardian is during the relay's
/// fallback sweep). `proc_pidinfo` gives no answer for a zombie and `getsid`
/// refuses it, so the census is empty and the sweep is COMPLETE with nothing
/// sent, not UNVERIFIABLE.
#[test]
fn a_zombie_leader_is_not_a_claimant() {
    let env = environment(&[], &[]);
    let s = spawn(
        Path::new("/usr/bin/true"),
        &args(&["true"]),
        &env,
        true,
        &[],
    )
    .unwrap();
    for _ in 0..200 {
        if stat(s).starts_with('Z') {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(stat(s).starts_with('Z'), "not a zombie: {:?}", stat(s));
    let mut host = MacHost { anchor: || true };
    let census = host
        .census(s, std::time::Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(census.is_empty(), "a zombie was reported: {census:?}");
    let o = sweep(&mut host, s, Duration::from_secs(1));
    assert_eq!(o.kind, Kind::Complete, "{o:?}");
    assert_eq!(o.sent, 0);
    reap(s);
}
