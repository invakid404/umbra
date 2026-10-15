//! The session sweep: the one place that decides which processes a run's
//! cleanup may signal (#193 T1 protocol, `t1-protocol.md` §3).
//!
//! Pure logic over a [`Host`], so every interleaving the protocol cares about
//! can be scripted exactly in tests. The macOS host lives in `census.rs`.
//!
//! **Rule.** A process is signalled only if its (pid, pid version) was
//! *enrolled*, and enrollment by numeric session id happens only for a census
//! pass bracketed by two successful anchor checks. From the first failed check
//! the owned set is frozen: later claimants of the session id are reported as
//! unverifiable and are never enrolled or signalled.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// An audit token, as `TASK_AUDIT_TOKEN` returns it (`audit_token_t`).
pub type Token = [u32; 8];

/// The pid an audit token names (`audit_token_to_pid`: `val[5]`).
pub fn token_pid(token: &Token) -> i32 {
    token[5] as i32
}

/// The pid version an audit token names (`audit_token_to_pidversion`: `val[7]`).
/// It changes on every pid allocation, so a token never names a later process
/// that reuses the pid.
pub fn token_version(token: &Token) -> u32 {
    token[7]
}

/// One process that claims the swept session id in a census pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimant {
    /// Its pid.
    pub pid: i32,
    /// Its audit token, taken so that the session check and the token describe
    /// one instance; `None` when no token could be taken. A claimant without a
    /// token is never signalled.
    pub token: Option<Token>,
}

/// What a signal by audit token did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signalled {
    /// The kernel accepted it.
    Sent,
    /// The token no longer names a live process (`ESRCH`).
    Stale,
    /// Any other refusal (errno).
    Refused(i32),
}

/// The process table and signal primitive the sweep runs against.
pub trait Host {
    /// Whether ownership of the numeric session id is still proved (the zombie
    /// anchor, `t1-protocol.md` §2). The guardian's own sweep answers `true`.
    fn anchor_ok(&mut self) -> bool;
    /// Every live, non-zombie process whose session id is `sid`, or `Err` when
    /// the census could not be taken. Must not run past `deadline`.
    fn census(&mut self, sid: i32, deadline: Instant) -> Result<Vec<Claimant>, String>;
    /// SIGKILL by audit token. Never by pid.
    fn kill(&mut self, token: &Token) -> Signalled;
    /// The monotonic clock.
    fn now(&self) -> Instant;
    /// Wait between passes.
    fn pause(&mut self);
}

/// How a sweep ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A successful census found no claimant of the session.
    Complete,
    /// The budget ran out with owned processes still alive.
    Incomplete,
    /// No census succeeded within the budget. Unavailable is never read as
    /// empty.
    Error,
    /// A claimant of the session could not be proved owned, so it was not
    /// signalled. Dominates `Incomplete`.
    Unverifiable,
}

impl Kind {
    /// The wire and record spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Complete => "COMPLETE",
            Kind::Incomplete => "INCOMPLETE",
            Kind::Error => "ERROR",
            Kind::Unverifiable => "UNVERIFIABLE",
        }
    }
    /// The inverse of [`Kind::as_str`]; anything else is malformed.
    pub fn parse(text: &str) -> Option<Kind> {
        match text {
            "COMPLETE" => Some(Kind::Complete),
            "INCOMPLETE" => Some(Kind::Incomplete),
            "ERROR" => Some(Kind::Error),
            "UNVERIFIABLE" => Some(Kind::Unverifiable),
            _ => None,
        }
    }
}

/// The result of one sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// How it ended.
    pub kind: Kind,
    /// Owned pids still alive at the last successful census.
    pub owned_alive: Vec<i32>,
    /// Claimants never enrolled (and never signalled).
    pub unverifiable: Vec<i32>,
    /// Signals the kernel accepted.
    pub sent: u32,
    /// Signals refused because the token was stale.
    pub stale: u32,
    /// Census passes taken (successful or not).
    pub passes: u32,
    /// The pass at which the owned set froze, if it did.
    pub frozen_at: Option<u32>,
    /// The last census error, if any.
    pub error: Option<String>,
}

/// Sweep session `sid` within `budget` (a monotonic, operational deadline:
/// checked before every pass and passed into each census; see the README's
/// bounds section).
pub fn sweep(host: &mut impl Host, sid: i32, budget: Duration) -> Outcome {
    let deadline = host.now() + budget;
    let mut owned: BTreeMap<(i32, u32), Token> = BTreeMap::new();
    let mut frozen_at: Option<u32> = None;
    let mut outcome = Outcome {
        kind: Kind::Error,
        owned_alive: vec![],
        unverifiable: vec![],
        sent: 0,
        stale: 0,
        passes: 0,
        frozen_at: None,
        error: None,
    };
    let mut any_census = false;
    let mut last_owned_alive: Vec<i32> = vec![];
    let mut last_unverifiable: Vec<i32> = vec![];
    while host.now() < deadline {
        let pass = outcome.passes;
        outcome.passes += 1;
        if frozen_at.is_none() && !host.anchor_ok() {
            frozen_at = Some(pass);
        }
        let census = match host.census(sid, deadline) {
            Ok(c) => c,
            Err(e) => {
                outcome.error = Some(e);
                host.pause();
                continue;
            }
        };
        // Enrollment needs a pass bracketed by two successful anchor checks.
        if frozen_at.is_none() && !host.anchor_ok() {
            frozen_at = Some(pass);
        }
        any_census = true;
        if census.is_empty() {
            outcome.kind = Kind::Complete;
            outcome.owned_alive.clear();
            outcome.unverifiable.clear();
            outcome.frozen_at = frozen_at;
            return outcome;
        }
        if frozen_at.is_none() {
            for c in &census {
                if let Some(t) = c.token {
                    owned.insert((t[5] as i32, token_version(&t)), t);
                }
            }
        }
        let mut owned_alive = vec![];
        let mut unverifiable = vec![];
        for c in &census {
            let known = c
                .token
                .and_then(|t| owned.get(&(token_pid(&t), token_version(&t))).copied());
            match known {
                Some(t) if token_pid(&t) == c.pid => {
                    owned_alive.push(c.pid);
                    match host.kill(&t) {
                        Signalled::Sent => outcome.sent += 1,
                        Signalled::Stale => outcome.stale += 1,
                        Signalled::Refused(_) => {}
                    }
                }
                _ => unverifiable.push(c.pid),
            }
        }
        last_owned_alive = owned_alive;
        last_unverifiable = unverifiable;
        // Frozen and nothing left that we may signal: waiting cannot help.
        if frozen_at.is_some() && last_owned_alive.is_empty() {
            break;
        }
        host.pause();
    }
    outcome.frozen_at = frozen_at;
    outcome.owned_alive = last_owned_alive;
    outcome.unverifiable = last_unverifiable;
    outcome.kind = if !any_census {
        Kind::Error
    } else if !outcome.unverifiable.is_empty() {
        Kind::Unverifiable
    } else {
        Kind::Incomplete
    };
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn tok(pid: i32, version: u32) -> Token {
        let mut t = [0u32; 8];
        t[5] = pid as u32;
        t[7] = version;
        t
    }
    fn claim(pid: i32, version: u32) -> Claimant {
        Claimant {
            pid,
            token: Some(tok(pid, version)),
        }
    }

    /// A scripted host: one entry per call, in order, so each interleaving is
    /// exact. Every signal is recorded.
    struct Script {
        anchors: VecDeque<bool>,
        censuses: VecDeque<Result<Vec<Claimant>, String>>,
        killed: Vec<Token>,
        clock: Instant,
        alive: std::collections::BTreeSet<(i32, u32)>,
    }
    impl Script {
        fn new(anchors: &[bool], censuses: Vec<Result<Vec<Claimant>, String>>) -> Self {
            Script {
                anchors: anchors.iter().copied().collect(),
                censuses: censuses.into(),
                killed: vec![],
                clock: Instant::now(),
                alive: Default::default(),
            }
        }
    }
    impl Host for Script {
        fn anchor_ok(&mut self) -> bool {
            self.anchors.pop_front().unwrap_or(false)
        }
        fn census(&mut self, _sid: i32, _deadline: Instant) -> Result<Vec<Claimant>, String> {
            let c = self.censuses.pop_front().unwrap_or_else(|| Ok(vec![]));
            if let Ok(list) = &c {
                for m in list {
                    if let Some(t) = m.token {
                        self.alive.insert((m.pid, token_version(&t)));
                    }
                }
            }
            c
        }
        fn kill(&mut self, token: &Token) -> Signalled {
            self.killed.push(*token);
            Signalled::Sent
        }
        fn now(&self) -> Instant {
            self.clock
        }
        fn pause(&mut self) {
            self.clock += Duration::from_millis(20);
        }
    }

    #[test]
    fn anchor_held_enrolls_and_completes() {
        let mut h = Script::new(
            &[true, true, true, true],
            vec![Ok(vec![claim(10, 1), claim(11, 1)]), Ok(vec![])],
        );
        let o = sweep(&mut h, 99, Duration::from_secs(3));
        assert_eq!(o.kind, Kind::Complete);
        assert_eq!(o.sent, 2);
        assert_eq!(o.frozen_at, None);
    }

    /// T1-a: the last authentic witness is lost mid-sweep. Pass 1 enrolls A and
    /// B; pass 2's anchor check fails *after* its census, which shows B and a new
    /// X. X must never be signalled, and pass 2's enrollments are discarded.
    #[test]
    fn t1_a_witness_lost_mid_sweep_never_signals_the_newcomer() {
        let mut h = Script::new(
            &[true, true, true, false],
            vec![
                Ok(vec![claim(10, 1), claim(11, 1)]),
                Ok(vec![claim(11, 1), claim(77, 1)]),
                Ok(vec![claim(77, 1)]),
            ],
        );
        let o = sweep(&mut h, 99, Duration::from_secs(3));
        assert!(
            h.killed.iter().all(|t| token_pid(t) != 77),
            "X was signalled: {:?}",
            h.killed
        );
        assert!(
            h.killed.iter().any(|t| token_pid(t) == 11),
            "owned B not signalled"
        );
        assert_eq!(o.frozen_at, Some(1));
        assert_eq!(o.kind, Kind::Unverifiable);
        assert_eq!(o.unverifiable, vec![77]);
    }

    /// T1-b: registry refresh after loss. The anchor fails before the first
    /// census; every later pass only observes. Nothing is enrolled or signalled.
    #[test]
    fn t1_b_refresh_after_loss_never_enrolls() {
        let mut h = Script::new(
            &[false],
            vec![
                Ok(vec![claim(10, 1), claim(12, 3)]),
                Ok(vec![claim(10, 1), claim(12, 3), claim(13, 1)]),
            ],
        );
        let o = sweep(&mut h, 99, Duration::from_secs(3));
        assert!(h.killed.is_empty(), "signalled after loss: {:?}", h.killed);
        assert_eq!(o.kind, Kind::Unverifiable);
        assert_eq!(o.frozen_at, Some(0));
    }

    /// T1-c(ii), scripted: anchor forced off, multi-round sweep in which a
    /// claimant reuses a *pid* we enrolled but with a different pid version (a
    /// reused pid), and an unknown claimant appears. Neither is signalled; the
    /// enrolled instance is.
    #[test]
    fn t1_c_reused_pid_and_unknown_claimant_are_never_signalled() {
        let mut h = Script::new(
            &[true, true, false],
            vec![
                Ok(vec![claim(10, 1)]),
                Ok(vec![claim(10, 2), claim(55, 9)]),
                Ok(vec![claim(10, 2), claim(55, 9)]),
            ],
        );
        let o = sweep(&mut h, 99, Duration::from_secs(3));
        assert_eq!(
            h.killed,
            vec![tok(10, 1)],
            "only the enrolled instance may be signalled"
        );
        assert_eq!(o.kind, Kind::Unverifiable);
        assert!(o.unverifiable.contains(&10) && o.unverifiable.contains(&55));
    }

    #[test]
    fn a_claimant_without_a_token_is_never_signalled() {
        let mut h = Script::new(
            &[true, true, true, true, true, true],
            vec![
                Ok(vec![Claimant {
                    pid: 40,
                    token: None,
                }]),
                Ok(vec![Claimant {
                    pid: 40,
                    token: None,
                }]),
            ],
        );
        let o = sweep(&mut h, 99, Duration::from_millis(30));
        assert!(h.killed.is_empty());
        assert_eq!(o.kind, Kind::Unverifiable);
    }

    #[test]
    fn unavailable_census_is_error_not_complete() {
        let mut h = Script::new(
            &[true; 64],
            (0..64).map(|_| Err("sysctl".to_string())).collect(),
        );
        let o = sweep(&mut h, 99, Duration::from_millis(100));
        assert_eq!(o.kind, Kind::Error);
        assert!(h.killed.is_empty());
    }

    #[test]
    fn owned_survivor_at_deadline_is_incomplete() {
        let mut h = Script::new(
            &[true; 64],
            (0..64).map(|_| Ok(vec![claim(10, 1)])).collect(),
        );
        let o = sweep(&mut h, 99, Duration::from_millis(100));
        assert_eq!(o.kind, Kind::Incomplete);
        assert_eq!(o.owned_alive, vec![10]);
    }

    #[test]
    fn kind_round_trips_and_rejects_anything_else() {
        for k in [
            Kind::Complete,
            Kind::Incomplete,
            Kind::Error,
            Kind::Unverifiable,
        ] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kind::parse("complete"), None);
        assert_eq!(Kind::parse(""), None);
    }
}
