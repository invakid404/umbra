//! The guardian: leader of the run's session S (#193 D3).
//!
//! Spawned by the relay with `POSIX_SPAWN_SETSID`. It proves isolation, spawns
//! the supervisor (the same `umbra` binary, arguments and environment, less
//! the guardian's own descriptors variable) only after the relay's `GO`, is
//! the only process
//! that waits on the supervisor, and sweeps S when the supervisor exits or the
//! relay dies. While it runs, S persists (its leader is a member), so its own
//! sweep needs no further ownership proof.

use crate::census::MacHost;
use crate::frame::{send, Msg, Read, Reader};
use crate::record::{self, Record};
use crate::spawn;
use crate::sweep::{self, Kind};
use crate::ENV_GUARDIAN;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

const HANDSHAKE: Duration = Duration::from_secs(2);
const SWEEP: Duration = Duration::from_secs(3);
const REAP: Duration = Duration::from_secs(1);

fn parse_fds(spec: &str) -> Option<(RawFd, RawFd, RawFd)> {
    let v: Vec<RawFd> = spec
        .split(',')
        .map(|s| s.parse().ok())
        .collect::<Option<_>>()?;
    match v.as_slice() {
        [a, b, c] if *a > 2 && *b > 2 && *c > 2 => Some((*a, *b, *c)),
        _ => None,
    }
}

/// Run as the guardian. Never returns.
pub fn run(spec: &std::ffi::OsStr) -> ! {
    let Some((up, down, life)) = spec.to_str().and_then(parse_fds) else {
        eprintln!("umbra: run guardian: malformed {ENV_GUARDIAN}");
        std::process::exit(70);
    };
    for fd in [up, down, life] {
        spawn::set_cloexec(fd);
    }
    // SAFETY: getpid/getsid take no memory arguments.
    let (me, sid) = unsafe { (libc::getpid(), libc::getsid(0)) };
    if sid != me || me <= 1 {
        let _ = send(up, &Msg::Fail("not-a-session-leader".into()));
        std::process::exit(70);
    }
    if send(up, &Msg::Isolated(me)).is_err() {
        std::process::exit(73);
    }
    let mut reader = Reader::new(down);
    let path = match reader.next(Instant::now() + HANDSHAKE) {
        Read::Frame(f) => match Msg::parse(&f) {
            Ok(Msg::Go(p)) => p,
            _ => std::process::exit(71),
        },
        _ => std::process::exit(71),
    };
    let mut rec = match record::open(&path) {
        Ok(sink) => Record::new(sink),
        Err(e) => {
            let _ = send(up, &Msg::Fail(format!("record-errno-{e}")));
            std::process::exit(74);
        }
    };
    rec.line("guardian-start", serde_json::json!({ "sid": me }));
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => {
            let _ = send(up, &Msg::Fail("current-exe".into()));
            std::process::exit(72);
        }
    };
    let args: Vec<_> = std::env::args_os().collect();
    let env = spawn::environment(&[ENV_GUARDIAN], &[]);
    let supervisor = match spawn::spawn(&exe, &args, &env, false, &[]) {
        Ok(p) => p,
        Err(e) => {
            let _ = send(up, &Msg::Fail(format!("spawn-errno-{e}")));
            std::process::exit(72);
        }
    };
    rec.line(
        "supervisor-started",
        serde_json::json!({ "sid": me, "supervisor": supervisor }),
    );
    let relay_alive = send(up, &Msg::Started(supervisor)).is_ok();

    let finish_relay_dead = |mut rec: Record<record::FdSink>, why: &str| -> ! {
        let outcome = sweep::sweep(&mut MacHost { anchor: || true }, me, SWEEP);
        let mut st = 0;
        let end = Instant::now() + REAP;
        let mut reaped = false;
        while Instant::now() < end {
            // SAFETY: a valid out-pointer; `supervisor` is our child.
            if unsafe { libc::waitpid(supervisor, &mut st, libc::WNOHANG) } == supervisor {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let kind = if outcome.kind == Kind::Complete && !reaped {
            Kind::Incomplete
        } else {
            outcome.kind
        };
        rec.line(
            "relay-death",
            serde_json::json!({ "why": why, "sid": me, "outcome": kind.as_str(), "supervisor_reaped": reaped,
                "owned_alive": outcome.owned_alive, "unverifiable": outcome.unverifiable, "error": outcome.error }),
        );
        // R-4: no caller left. The inherited stderr is the only report when
        // the record itself failed.
        if let Some(e) = rec.failed() {
            eprintln!("umbra: run guardian: relay gone, cleanup {}, isolation record write failed (errno {e})", kind.as_str());
        }
        std::process::exit(if kind == Kind::Complete && rec.failed().is_none() {
            0
        } else {
            1
        })
    };
    if !relay_alive {
        finish_relay_dead(rec, "relay channel closed at start");
    }

    loop {
        let mut st = 0;
        // SAFETY: a valid out-pointer; only the supervisor is waited on (C-3).
        let r = unsafe { libc::waitpid(supervisor, &mut st, libc::WNOHANG) };
        if r == supervisor {
            let outcome = sweep::sweep(&mut MacHost { anchor: || true }, me, SWEEP);
            rec.line(
                "sweep",
                serde_json::json!({ "why": "supervisor-exit", "sid": me, "supervisor_status": st,
                    "outcome": outcome.kind.as_str(), "owned_alive": outcome.owned_alive,
                    "unverifiable": outcome.unverifiable, "sent": outcome.sent, "stale": outcome.stale,
                    "passes": outcome.passes, "error": outcome.error }),
            );
            let record_errno = rec.failed().unwrap_or(0);
            // R-4: with the relay gone and the record failed, the inherited
            // stderr is the only report left.
            if send(
                up,
                &Msg::Outcome {
                    status: st,
                    kind: outcome.kind,
                    record_errno,
                },
            )
            .is_err()
                && record_errno != 0
            {
                eprintln!("umbra: run guardian: relay gone, cleanup {}, isolation record write failed (errno {record_errno})", outcome.kind.as_str());
            }
            std::process::exit(if outcome.kind == Kind::Complete && record_errno == 0 {
                0
            } else {
                1
            });
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            // R-7: a permanent wait error fails closed.
            let outcome = sweep::sweep(&mut MacHost { anchor: || true }, me, SWEEP);
            rec.line(
                "wait-error",
                serde_json::json!({ "sid": me, "outcome": outcome.kind.as_str() }),
            );
            let _ = send(
                up,
                &Msg::Outcome {
                    status: -1,
                    kind: Kind::Error,
                    record_errno: rec.failed().unwrap_or(0),
                },
            );
            std::process::exit(1);
        }
        let mut pfd = libc::pollfd {
            fd: life,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let p = unsafe { libc::poll(&mut pfd, 1, 10) };
        let lost = if p < 0 {
            std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
        } else if p > 0 {
            let mut b = [0u8; 1];
            // SAFETY: a valid one-byte buffer.
            let n = unsafe { libc::read(life, b.as_mut_ptr().cast(), 1) };
            n == 0
                || (n < 0
                    && !matches!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EINTR) | Some(libc::EAGAIN)
                    ))
        } else {
            false
        };
        if lost {
            finish_relay_dead(rec, "relay-death");
        }
    }
}
