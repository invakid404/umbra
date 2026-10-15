//! The relay: the original `umbra run` process in launchd's session (#193 D3).
//!
//! It stays in the caller's process group, spawns the guardian into a fresh
//! session S (`POSIX_SPAWN_SETSID`, so S = the guardian's pid), and reports the
//! run's status to the caller.
//!
//! **T1 (`t1-protocol.md`).** The relay never reaps the guardian until its
//! fallback sweep (if any) and the record's final line are done: it observes
//! the guardian only with `waitid(…WNOWAIT…)`. While the guardian is unreaped,
//! no process outside the original session can carry session id S, so a
//! numeric-SID census is an ownership proof (the zombie anchor). The single
//! reap is the last numeric-pid operation (R-10).

use crate::census::MacHost;
use crate::frame::{send, Msg, Read, Reader};
use crate::record::{self, FdSink, Record};
use crate::spawn;
use crate::sweep::{self, Kind};
use crate::{ENV_GUARDIAN, ENV_RUN_ISOLATED};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Bound for each startup handshake phase (O7: operational, monotonic).
const HANDSHAKE: Duration = Duration::from_secs(2);
/// Bound for a sweep (O7).
const SWEEP: Duration = Duration::from_secs(3);
/// Bound for waiting on a killed guardian to exit (O7).
const REAP: Duration = Duration::from_secs(1);

/// The guardian's state as the relay knows it (R-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guardian {
    /// Running. May be signalled by pid: it is our unreaped child.
    Live,
    /// Stopped. May be signalled by pid (SIGKILL acts on a stopped process).
    Stopped,
    /// Exited, not yet reaped: still the zombie anchor. Nothing to signal.
    Exited,
    /// No longer our child (`waitid` ECHILD). Never signalled by pid.
    Gone,
}

/// `waitid` without reaping (`WNOWAIT`). `None` = the guardian is no longer our
/// unreaped child (ECHILD), which the relay's own logic never causes.
fn observe(g: i32) -> Option<Guardian> {
    // SAFETY: siginfo_t is plain old data; zero is a valid initial value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: a valid out-pointer; `g` is our child.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                g as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
    if info.si_pid == 0 {
        return Some(Guardian::Live);
    }
    match info.si_code {
        libc::CLD_STOPPED => Some(Guardian::Stopped),
        libc::CLD_CONTINUED => Some(Guardian::Live),
        _ => Some(Guardian::Exited),
    }
}

/// The zombie anchor: the guardian is still our unreaped child.
fn anchor(g: i32) -> bool {
    observe(g).is_some()
}

/// Reap the guardian. The last numeric-pid operation of the relay (R-10).
fn reap(g: i32) {
    let mut st = 0;
    loop {
        // SAFETY: a valid out-pointer; `g` is our child.
        let r = unsafe { libc::waitpid(g, &mut st, 0) };
        if r >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// Re-raise the supervisor's status, or exit with its code.
pub(crate) fn mirror(status: i32) -> ! {
    if libc::WIFEXITED(status) {
        std::process::exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        // SAFETY: resetting one disposition to default, unblocking it, and
        // raising it in this single-threaded process.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(sig);
        }
        std::process::exit(128 + sig);
    }
    std::process::exit(1)
}

struct Relay {
    rec: Record<FdSink>,
    g: i32,
    state: Guardian,
}

impl Relay {
    /// SIGKILL the guardian by pid. Allowed only while it is our unreaped,
    /// running or stopped child (R-10); recorded either way.
    fn kill_guardian(&mut self) {
        let allowed = matches!(self.state, Guardian::Live | Guardian::Stopped);
        self.rec.line(
            "guardian-signal",
            serde_json::json!({ "pid": self.g, "state": format!("{:?}", self.state), "sent": allowed }),
        );
        if allowed {
            // SAFETY: `g` is our unreaped child; its pid cannot be reallocated
            // before we reap it (t1-protocol.md §2.2, §4).
            unsafe {
                libc::kill(self.g, libc::SIGKILL);
            }
        }
    }
    /// The single reap: the last numeric-pid operation (R-10).
    fn reap(&mut self) {
        self.rec
            .line("guardian-reaped", serde_json::json!({ "pid": self.g }));
        reap(self.g);
    }

    /// Fail closed: end the guardian if it is still ours to signal, sweep S
    /// under the anchor, write the final record line, reap, exit 1.
    fn fallback(mut self, reason: &str) -> ! {
        if matches!(self.state, Guardian::Live | Guardian::Stopped) {
            self.kill_guardian();
            let end = Instant::now() + REAP;
            while Instant::now() < end && observe(self.g) != Some(Guardian::Exited) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let g = self.g;
        let outcome = sweep::sweep(
            &mut MacHost {
                anchor: || anchor(g),
            },
            g,
            SWEEP,
        );
        self.rec.line(
            "relay-fallback",
            serde_json::json!({
                "reason": reason, "sid": g, "outcome": outcome.kind.as_str(),
                "owned_alive": outcome.owned_alive, "unverifiable": outcome.unverifiable,
                "sent": outcome.sent, "stale": outcome.stale, "passes": outcome.passes,
                "frozen_at": outcome.frozen_at, "error": outcome.error,
            }),
        );
        self.reap();
        let failed = self.rec.failed();
        eprintln!(
            "umbra: run isolation failed: {reason} (session {g}, relay sweep {}){}",
            outcome.kind.as_str(),
            failed
                .map(|e| format!("; isolation record write failed (errno {e})"))
                .unwrap_or_default()
        );
        std::process::exit(1)
    }
}

fn fail_before_launch(why: String) -> ! {
    eprintln!("umbra: run isolation failed: {why}");
    std::process::exit(1)
}

/// Run as the relay. Never returns.
pub fn run() -> ! {
    // R-1: the record exists, with its first line on disk, before anything is
    // launched.
    let dir = record::record_dir().unwrap_or_else(|e| {
        fail_before_launch(format!("cannot create isolation record directory: {e}"))
    });
    let exe = std::env::current_exe()
        .unwrap_or_else(|e| fail_before_launch(format!("current executable: {e}")));
    let args: Vec<_> = std::env::args_os().collect();
    run_with(exe, args, dir)
}

/// The relay with an explicit guardian executable, guardian argv and record
/// directory (tests point these at a scripted guardian and a temporary
/// directory). Never returns.
pub(crate) fn run_with(exe: PathBuf, args: Vec<std::ffi::OsString>, dir: PathBuf) -> ! {
    record::prune(&dir, record::KEEP);
    let unique = record::unique_id().unwrap_or_else(|e| {
        fail_before_launch(format!("cannot create isolation record id: errno {e}"))
    });
    let pending = dir.join(record::record_name(&unique, None));
    let sink = record::open(&pending).unwrap_or_else(|e| {
        fail_before_launch(format!(
            "cannot create isolation record {}: errno {e}",
            pending.display()
        ))
    });
    let mut rec = Record::new(sink);
    rec.line(
        "relay-start",
        serde_json::json!({ "pgid": unsafe_getpgrp(), "sid": 1 }),
    );
    if let Some(e) = rec.failed() {
        fail_before_launch(format!(
            "isolation record {} not writable: errno {e}",
            pending.display()
        ));
    }

    let pipes = (|| Ok::<_, i32>((spawn::pipe()?, spawn::pipe()?, spawn::pipe()?)))();
    let ((up_r, up_w), (down_r, down_w), (life_r, life_w)) =
        pipes.unwrap_or_else(|e| fail_before_launch(format!("pipe: errno {e}")));
    let env = spawn::environment(
        &[ENV_GUARDIAN, ENV_RUN_ISOLATED],
        &[(ENV_GUARDIAN, format!("{up_w},{down_r},{life_r}"))],
    );
    let g = match spawn::spawn(&exe, &args, &env, true, &[up_w, down_r, life_r]) {
        Ok(g) => g,
        Err(e) => {
            rec.line("spawn-failed", serde_json::json!({ "errno": e }));
            fail_before_launch(format!("cannot start the run guardian: errno {e}"));
        }
    };
    spawn::close(up_w);
    spawn::close(down_r);
    spawn::close(life_r);
    // `life_w` stays open, unwritten, for the relay's lifetime: its EOF tells the
    // guardian the relay is gone.
    let _life = life_w;
    let mut relay = Relay {
        rec,
        g,
        state: Guardian::Live,
    };
    let mut reader = Reader::new(up_r);

    // Handshake phase 1: the guardian proves it leads session S = g.
    match reader.next(Instant::now() + HANDSHAKE) {
        Read::Frame(f) => match Msg::parse(&f) {
            Ok(Msg::Isolated(s)) if s == g => {}
            Ok(other) => relay.fallback(&format!("guardian did not isolate: {other:?}")),
            Err(e) => relay.fallback(&format!("protocol: {e}")),
        },
        Read::Timeout => relay.fallback("guardian did not isolate in time"),
        Read::Lost(e) => relay.fallback(&format!("guardian did not isolate: {e}")),
    }
    let final_path: PathBuf = dir.join(record::record_name(&unique, Some(g)));
    if let Err(e) = std::fs::rename(&pending, &final_path) {
        relay.rec.line(
            "rename-failed",
            serde_json::json!({ "errno": e.raw_os_error() }),
        );
    }
    relay.rec.line(
        "isolated",
        serde_json::json!({ "sid": g, "record": final_path.display().to_string() }),
    );
    if let Err(e) = send(down_w, &Msg::Go(final_path)) {
        relay.fallback(&format!("cannot signal the guardian: {e}"));
    }
    // Handshake phase 2: the supervisor was spawned in S.
    let supervisor = match reader.next(Instant::now() + HANDSHAKE) {
        Read::Frame(f) => match Msg::parse(&f) {
            Ok(Msg::Started(p)) => p,
            Ok(other) => relay.fallback(&format!("supervisor did not start: {other:?}")),
            Err(e) => relay.fallback(&format!("protocol: {e}")),
        },
        Read::Timeout => relay.fallback("supervisor start not confirmed in time"),
        Read::Lost(e) => relay.fallback(&format!("supervisor did not start: {e}")),
    };
    relay.rec.line(
        "started",
        serde_json::json!({ "sid": g, "supervisor": supervisor, "guardian": g }),
    );

    // The run: wait for the guardian's OUTCOME and its exit, without reaping.
    let mut outcome: Option<(i32, Kind, i32)> = None;
    // When OUTCOME arrived: from then on the guardian has only to exit, within
    // REAP (R-6), whether or not it closes the channel first.
    let mut outcome_at: Option<Instant> = None;
    loop {
        match observe(g) {
            None | Some(Guardian::Gone) => {
                relay.state = Guardian::Gone;
                relay.fallback("guardian is no longer the relay's child");
            }
            Some(Guardian::Stopped) => {
                relay.state = Guardian::Stopped;
                relay.fallback("guardian stopped");
            }
            Some(Guardian::Exited) => {
                relay.state = Guardian::Exited;
                if outcome.is_none() {
                    if let Read::Frame(f) = reader.next(Instant::now() + Duration::from_millis(200))
                    {
                        if let Ok(Msg::Outcome {
                            status,
                            kind,
                            record_errno,
                        }) = Msg::parse(&f)
                        {
                            outcome = Some((status, kind, record_errno));
                        }
                    }
                }
                match outcome {
                    Some(_) => break,
                    None => relay.fallback("guardian ended without an outcome"),
                }
            }
            Some(Guardian::Live) => {}
        }
        match reader.next(Instant::now() + Duration::from_millis(50)) {
            Read::Frame(f) => match Msg::parse(&f) {
                Ok(Msg::Outcome {
                    status,
                    kind,
                    record_errno,
                }) if outcome.is_none() => {
                    outcome = Some((status, kind, record_errno));
                    outcome_at = Some(Instant::now());
                }
                Ok(other) => relay.fallback(&format!("protocol: unexpected {other:?}")),
                Err(e) => relay.fallback(&format!("protocol: {e}")),
            },
            Read::Timeout => {
                if outcome_at.is_some_and(|t| t.elapsed() >= REAP) {
                    relay.fallback("guardian did not exit after its outcome");
                }
            }
            Read::Lost(e) => {
                // EOF is expected right after OUTCOME; otherwise the channel is
                // gone while the guardian may still be live: fail closed.
                if outcome.is_none() {
                    let end = Instant::now() + Duration::from_millis(200);
                    while Instant::now() < end && observe(g) == Some(Guardian::Live) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if observe(g) == Some(Guardian::Live) {
                        relay.fallback(&format!("guardian channel lost: {e}"));
                    }
                } else {
                    let end = Instant::now() + REAP;
                    while Instant::now() < end && observe(g) == Some(Guardian::Live) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if observe(g) == Some(Guardian::Live) {
                        relay.fallback("guardian did not exit after its outcome");
                    }
                }
            }
        }
    }
    let (status, kind, guardian_errno) = outcome.expect("loop exits only with an outcome");
    relay.rec.line(
        "outcome",
        serde_json::json!({ "supervisor_status": status, "cleanup": kind.as_str(), "guardian_record_errno": guardian_errno }),
    );
    relay.reap();
    let relay_errno = relay.rec.failed();
    // O5: a cleanup other than COMPLETE, or any record failure, is exit 1.
    if kind == Kind::Complete && guardian_errno == 0 && relay_errno.is_none() {
        mirror(status);
    }
    if kind != Kind::Complete {
        eprintln!(
            "umbra: run isolation cleanup {} (session {g}); supervisor status {status}",
            kind.as_str()
        );
    }
    if guardian_errno != 0 || relay_errno.is_some() {
        eprintln!(
            "umbra: isolation record write failed (guardian errno {guardian_errno}, relay errno {}); supervisor status {status}",
            relay_errno.unwrap_or(0)
        );
    }
    std::process::exit(1)
}

fn unsafe_getpgrp() -> i32 {
    // SAFETY: getpgrp has no arguments and cannot fail.
    unsafe { libc::getpgrp() }
}
