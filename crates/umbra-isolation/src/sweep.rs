//! The session sweep: the one place that decides which processes a run's
//! cleanup may signal (#193's T1 protocol; README, Cleanup and ownership).
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

/// One live member of the swept session as the run's ending sees it (#205).
///
/// The classification is for **waiting and counting only**: whether the drain
/// may stop waiting, and which zombies to report. Nothing is ever signalled on
/// its basis; signals still go only to tokens enrolled under the anchor rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Its pid and audit token, as a census claimant.
    pub claimant: Claimant,
    /// Its image is a debugger process (`debugserver`), or it could not be
    /// read: the drain waits while such a member lives.
    pub debugger: bool,
    /// It is traced (`P_LTRACED`): the drain waits while such a member lives.
    pub traced: bool,
    /// It is stopped (`SSTOP`).
    pub stopped: bool,
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
    /// anchor; README, Cleanup and ownership). The guardian's own sweep
    /// answers `true`.
    fn anchor_ok(&mut self) -> bool;
    /// Every live, non-zombie process whose session id is `sid`, or `Err` when
    /// the census could not be taken. Must not run past `deadline`.
    fn census(&mut self, sid: i32, deadline: Instant) -> Result<Vec<Claimant>, String>;
    /// SIGKILL by audit token. Never by pid.
    fn kill(&mut self, token: &Token) -> Signalled;
    /// Every live member of `sid`, classified for the drain, or `Err` when the
    /// census could not be taken. Must not run past `deadline` (#205).
    fn members(&mut self, sid: i32, deadline: Instant) -> Result<Vec<Member>, String>;
    /// SIGCONT by audit token: it wakes a stopped member and cannot end one.
    fn cont(&mut self, token: &Token) -> Signalled;
    /// End the run's supervisor: SIGKILL by the audit token of `pid`, taken
    /// so that its session is checked to be `sid` for that same instance, and
    /// only between two successful anchor checks (the sweep's own rule); with
    /// no ownership proof it signals nothing and answers `Stale`.
    fn end_supervisor(&mut self, pid: i32, sid: i32) -> Signalled;
    /// Whether the instance `token` names is now a zombie under launchd, which
    /// nobody will collect (#205).
    fn launchd_zombie(&mut self, token: &Token) -> bool;
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
    /// Members seen while the run was ended that are now zombies under
    /// launchd, which nobody collects (#205). `Complete` requires none.
    pub launchd_zombies: Vec<i32>,
    /// The drain reached its cap with a debugger or a traced member still
    /// live, and the sweep went ahead (#205).
    pub drain_exceeded: bool,
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
        launchd_zombies: vec![],
        drain_exceeded: false,
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

/// How long the drain waits for a session holding `debuggers` live debugger
/// processes (#205): the platform's own worst case, 2 s for its retirements
/// plus up to 4 s per traced session (its ordered teardown, then the emergency
/// attempt for a failed retirement), and a margin: 3 s + 4 s per debugger, at
/// least one.
pub fn drain_cap(debuggers: usize) -> Duration {
    Duration::from_secs(3) + Duration::from_secs(4) * debuggers.max(1) as u32
}

/// End a run's session `sid` debugger-last (#205).
///
/// 1. End the run's `supervisor`, the platform provider's owner, if it is
///    still alive. The provider then tears its sessions down in order: only
///    debugserver ends a held program, through RSP `k`, and debugserver is
///    ended only once the program is gone.
/// 2. Drain: wait, capped by [`drain_cap`], until no live member of `sid` is a
///    debugger and none is traced. Those are the processes whose ending order
///    decides whether the kernel hands a half-exited program to launchd as a
///    zombie nobody collects. Stopped members that are not traced are sent
///    SIGCONT first, by enrolled token, so a stopped provider can finish.
/// 3. [`sweep`] what is left within `budget`.
///
/// The outcome reports every member seen that is now a zombie under launchd,
/// and whether the drain reached its cap.
pub fn end_run(
    host: &mut impl Host,
    sid: i32,
    supervisor: Option<i32>,
    budget: Duration,
) -> Outcome {
    if let Some(pid) = supervisor {
        host.end_supervisor(pid, sid);
    }
    let (seen, drain_exceeded) = drain(host, sid);
    let mut outcome = sweep(host, sid, budget);
    outcome.drain_exceeded = drain_exceeded;
    let mut zombies: Vec<i32> = seen
        .iter()
        .filter(|t| host.launchd_zombie(t))
        .map(token_pid)
        .collect();
    zombies.sort_unstable();
    zombies.dedup();
    if !zombies.is_empty() && outcome.kind == Kind::Complete {
        outcome.kind = Kind::Incomplete;
    }
    outcome.launchd_zombies = zombies;
    outcome
}

/// The drain of [`end_run`]: wait until no live member of `sid` is a debugger
/// and none is traced, capped by [`drain_cap`] for the debuggers of its first
/// readable reading (and by `drain_cap(1)` until there is one). Returns every
/// member token it saw, and whether it reached the cap.
///
/// It sends no terminating signal. Its one signal is SIGCONT, at the first
/// readable reading, to stopped members that are not traced, by tokens
/// enrolled in a reading bracketed by two successful anchor checks (the
/// sweep's own rule); a failed anchor check ends those attempts.
fn drain(host: &mut impl Host, sid: i32) -> (Vec<Token>, bool) {
    let start = host.now();
    let mut seen: BTreeMap<(i32, u32), Token> = BTreeMap::new();
    let mut cap: Option<Instant> = None;
    // The cap is sized by the first readable reading, and the SIGCONT pass
    // runs on the first readable reading bracketed by two successful anchor
    // checks. A failed reading delays both, it never skips them; a failed
    // anchor check ends the SIGCONT attempts (the owned set is frozen).
    let mut sized = false;
    let mut continued = false;
    let mut anchor_lost = false;
    loop {
        let bound = cap.unwrap_or(start + drain_cap(1));
        let try_cont = !continued && !anchor_lost;
        let anchored_before = try_cont && host.anchor_ok();
        if try_cont && !anchored_before {
            anchor_lost = true;
        }
        let reading = host.members(sid, bound);
        let anchored = anchored_before && host.anchor_ok();
        if anchored_before && !anchored {
            anchor_lost = true;
        }
        match reading {
            Ok(members) => {
                for m in &members {
                    if let Some(t) = m.claimant.token {
                        seen.insert((token_pid(&t), token_version(&t)), t);
                    }
                }
                if !sized {
                    sized = true;
                    let debuggers = members.iter().filter(|m| m.debugger).count();
                    cap = Some(start + drain_cap(debuggers));
                }
                if try_cont && anchored {
                    continued = true;
                    for m in members.iter().filter(|m| m.stopped && !m.traced) {
                        if let Some(t) = m.claimant.token {
                            host.cont(&t);
                        }
                    }
                }
                if !members.iter().any(|m| m.debugger || m.traced) {
                    return (seen.into_values().collect(), false);
                }
            }
            // No reading: keep reading until the cap. Unavailable is never
            // read as "nothing left to wait for".
            Err(_) => {
                cap.get_or_insert(start + drain_cap(1));
            }
        }
        if host.now() >= cap.expect("set by the first pass") {
            return (seen.into_values().collect(), true);
        }
        host.pause();
    }
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
        /// The drain's member readings, one per call; the last one repeats.
        members: VecDeque<Result<Vec<Member>, String>>,
        /// Every host action in order: `members`, `kill <pid>`, `cont <pid>`,
        /// `end-supervisor <pid>`, `pause`.
        log: Vec<String>,
        /// Instances that ended as zombies under launchd.
        zombies: std::collections::BTreeSet<(i32, u32)>,
    }
    impl Script {
        fn new(anchors: &[bool], censuses: Vec<Result<Vec<Claimant>, String>>) -> Self {
            Script {
                anchors: anchors.iter().copied().collect(),
                censuses: censuses.into(),
                killed: vec![],
                clock: Instant::now(),
                alive: Default::default(),
                members: VecDeque::new(),
                log: vec![],
                zombies: Default::default(),
            }
        }
        fn with_members(mut self, members: Vec<Vec<Member>>) -> Self {
            self.members = members.into_iter().map(Ok).collect();
            self
        }
        fn first(&self, prefix: &str) -> Option<usize> {
            self.log.iter().position(|l| l.starts_with(prefix))
        }
        fn last(&self, prefix: &str) -> Option<usize> {
            self.log.iter().rposition(|l| l.starts_with(prefix))
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
            self.log.push(format!("kill {}", token_pid(token)));
            Signalled::Sent
        }
        fn members(&mut self, _sid: i32, _deadline: Instant) -> Result<Vec<Member>, String> {
            self.log.push("members".into());
            if self.members.len() > 1 {
                self.members.pop_front().unwrap()
            } else {
                self.members.front().cloned().unwrap_or_else(|| Ok(vec![]))
            }
        }
        fn cont(&mut self, token: &Token) -> Signalled {
            self.log.push(format!("cont {}", token_pid(token)));
            Signalled::Sent
        }
        fn end_supervisor(&mut self, pid: i32, _sid: i32) -> Signalled {
            self.log.push(format!("end-supervisor {pid}"));
            Signalled::Sent
        }
        fn launchd_zombie(&mut self, token: &Token) -> bool {
            self.zombies
                .contains(&(token_pid(token), token_version(token)))
        }
        fn now(&self) -> Instant {
            self.clock
        }
        fn pause(&mut self) {
            self.log.push("pause".into());
            self.clock += Duration::from_millis(20);
        }
    }

    fn member(pid: i32, debugger: bool, traced: bool, stopped: bool) -> Member {
        Member {
            claimant: claim(pid, 1),
            debugger,
            traced,
            stopped,
        }
    }

    /// F5(a) (#205): the run's ending ends the supervisor first, then drains
    /// while a debugger or a traced member lives, and signals nothing until the
    /// drain is over. Ending debugserver and its held program together (the
    /// one-pass sweep) is how the kernel comes to hand the program to launchd.
    #[test]
    fn the_supervisor_ends_first_and_the_session_drains_before_any_sweep() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![claim(30, 1)]), Ok(vec![])])
            .with_members(vec![
                vec![
                    member(10, false, false, false),
                    member(20, true, false, false),
                    member(21, false, true, false),
                ],
                vec![
                    member(20, true, false, false),
                    member(21, false, true, false),
                ],
                vec![],
            ]);
        let o = end_run(&mut h, 99, Some(10), Duration::from_secs(3));
        assert_eq!(
            h.log.first().map(String::as_str),
            Some("end-supervisor 10"),
            "{:?}",
            h.log
        );
        let drained = h.last("members").expect("a drain");
        let first_kill = h.first("kill").expect("a sweep");
        assert!(
            first_kill > drained,
            "signalled before the drain ended: {:?}",
            h.log
        );
        assert!(!o.drain_exceeded);
        assert_eq!(o.kind, Kind::Complete);
    }

    /// F5(b) (#205): no platform image is needed for the drain to wait. With a
    /// debugger live and nothing ending it (the platform already gone, or a
    /// substitute provider), the drain waits to its cap, reports it, and only
    /// then sweeps; it never ends early for want of a platform member.
    #[test]
    fn the_drain_waits_to_its_cap_while_a_debugger_lives_without_a_platform() {
        let mut h = Script::new(&[true; 1024], vec![Ok(vec![claim(20, 1)]), Ok(vec![])])
            .with_members(vec![vec![member(20, true, false, false)]]);
        let start = h.clock;
        let o = end_run(&mut h, 99, None, Duration::from_secs(3));
        assert!(o.drain_exceeded, "{:?}", h.log);
        let first_kill = h.first("kill").expect("the sweep ran after the cap");
        let pauses_before_kill = h.log[..first_kill].iter().filter(|l| *l == "pause").count();
        let waited = Duration::from_millis(20) * pauses_before_kill as u32;
        assert!(waited >= drain_cap(1), "waited {waited:?}");
        assert!(
            waited < drain_cap(1) + Duration::from_millis(100),
            "waited {waited:?}"
        );
        let _ = start;
    }

    /// F5(c) (#205): with no debugger and no traced member there is nothing to
    /// wait for; the drain ends at once and a clean end costs nothing.
    #[test]
    fn the_drain_ends_at_once_with_nothing_to_wait_for() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![claim(30, 1)]), Ok(vec![])])
            .with_members(vec![vec![member(30, false, false, false)]]);
        let o = end_run(&mut h, 99, None, Duration::from_secs(3));
        assert_eq!(
            h.log.iter().filter(|l| *l == "members").count(),
            1,
            "{:?}",
            h.log
        );
        let first_kill = h.first("kill").expect("a sweep");
        assert!(
            !h.log[..first_kill].iter().any(|l| l == "pause"),
            "{:?}",
            h.log
        );
        assert!(!o.drain_exceeded);
    }

    /// F5(d) (#205 N-c): a stopped member that is not traced (a stopped
    /// provider) is continued at drain start, by its enrolled token; a stopped
    /// traced member's stop belongs to its debugger and is left alone.
    #[test]
    fn the_drain_continues_stopped_untraced_members_only() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![])]).with_members(vec![
            vec![
                member(40, false, false, true),
                member(41, false, true, true),
            ],
            vec![],
        ]);
        end_run(&mut h, 99, None, Duration::from_secs(3));
        assert!(h.log.iter().any(|l| l == "cont 40"), "{:?}", h.log);
        assert!(!h.log.iter().any(|l| l == "cont 41"), "{:?}", h.log);
    }

    /// #205: a failed first reading delays the drain's SIGCONT pass, it never
    /// skips it. A stopped, untraced member found on the next readable reading
    /// is still continued, so a stopped provider can finish its teardown.
    #[test]
    fn a_failed_first_reading_still_continues_a_stopped_provider() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![])])
            .with_members(vec![vec![member(40, false, false, true)], vec![]]);
        h.members
            .push_front(Err("member census cut at the deadline".into()));
        end_run(&mut h, 99, None, Duration::from_secs(3));
        assert!(h.log.iter().any(|l| l == "cont 40"), "{:?}", h.log);
    }

    /// F5(e) (#205): the cap is 3 s + 4 s per debugger enrolled at the drain's
    /// first reading.
    #[test]
    fn the_drain_cap_follows_the_debugger_count() {
        assert_eq!(drain_cap(0), Duration::from_secs(7));
        assert_eq!(drain_cap(1), Duration::from_secs(7));
        assert_eq!(drain_cap(2), Duration::from_secs(11));
        let mut h = Script::new(&[true; 2048], vec![Ok(vec![])]).with_members(vec![vec![
            member(20, true, false, false),
            member(22, true, false, false),
        ]]);
        let o = end_run(&mut h, 99, None, Duration::from_secs(3));
        assert!(o.drain_exceeded);
        let first_census = h.log.iter().rposition(|l| l == "members").unwrap();
        let pauses = h.log[..first_census]
            .iter()
            .filter(|l| *l == "pause")
            .count();
        let waited = Duration::from_millis(20) * pauses as u32;
        assert!(
            waited >= drain_cap(2) - Duration::from_millis(20),
            "waited {waited:?}"
        );
        assert!(
            waited < drain_cap(2) + Duration::from_millis(100),
            "waited {waited:?}"
        );
    }

    /// F5b (#205 A-6): the relay-death path, where the supervisor is still
    /// alive. It is ended first; the drain then waits on the debugger it
    /// leaves, and only then does the sweep signal anything.
    #[test]
    fn the_relay_death_path_ends_the_live_supervisor_first() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![claim(30, 1)]), Ok(vec![])])
            .with_members(vec![
                vec![
                    member(10, false, false, false),
                    member(20, true, false, false),
                ],
                vec![],
            ]);
        let o = end_run(&mut h, 99, Some(10), Duration::from_secs(3));
        assert_eq!(h.first("end-supervisor 10"), Some(0), "{:?}", h.log);
        assert!(
            h.first("kill").unwrap() > h.last("members").unwrap(),
            "{:?}",
            h.log
        );
        assert_eq!(o.kind, Kind::Complete);
    }

    /// F6 (#205): a member seen during the run's ending that is now a zombie
    /// under launchd is reported, and the outcome is not `Complete`: nobody
    /// will ever collect it, so the cleanup did not complete.
    #[test]
    fn a_member_left_as_a_launchd_zombie_is_reported() {
        let mut h = Script::new(&[true; 64], vec![Ok(vec![])])
            .with_members(vec![vec![member(21, false, true, false)], vec![]]);
        h.zombies.insert((21, 1));
        let o = end_run(&mut h, 99, None, Duration::from_secs(3));
        assert_eq!(o.launchd_zombies, vec![21]);
        assert_ne!(o.kind, Kind::Complete);
        assert!(!o.drain_exceeded);
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
