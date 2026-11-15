//! Nonblocking cleanup of a confirmed-finished inferior (#274).
//! A cleanup error is evidence for final teardown, never a live-session error.
use std::time::{Duration, Instant};

pub(crate) const BARRIER_BOUND: Duration = Duration::from_millis(1500);
pub(crate) const TRACER_BOUND: Duration = Duration::from_millis(250);
const NATURAL_GRACE: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishOutcome {
    AlreadyReaped,
    ReapedNormally,
    KilledThenReaped,
    UnconfirmedBarrier(String),
    KillFailure(String),
    WaitError(String),
    BoundExhausted,
}
impl FinishOutcome {
    pub(crate) fn success(&self) -> bool {
        matches!(
            self,
            Self::AlreadyReaped | Self::ReapedNormally | Self::KilledThenReaped
        )
    }
}

pub(crate) trait Tracer {
    fn already_reaped(&self) -> bool {
        false
    }
    /// true means an actual successful owned wait, including Child's cache.
    fn try_reap(&mut self) -> std::io::Result<bool>;
    fn kill(&mut self) -> std::io::Result<()>;
}

#[derive(Debug)]
enum Phase {
    Barrier,
    Finishing {
        started: Instant,
        deadline: Instant,
        killed: bool,
    },
    KilledReapPending,
    Settled(FinishOutcome),
}

#[derive(Debug)]
pub(crate) struct Retirement {
    started: Instant,
    deadline: Instant,
    phase: Phase,
    pub(crate) overrun: Option<Duration>,
    pub(crate) last_observation: Option<String>,
}
impl Retirement {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            started: now,
            deadline: now + BARRIER_BOUND + TRACER_BOUND,
            phase: Phase::Barrier,
            overrun: None,
            last_observation: None,
        }
    }
    pub(crate) fn outcome(&self) -> Option<&FinishOutcome> {
        match &self.phase {
            Phase::Settled(result) => Some(result),
            _ => None,
        }
    }
    pub(crate) fn needs_barrier(&self) -> bool {
        matches!(self.phase, Phase::Barrier)
    }
    pub(crate) fn hold_root(&self, now: Instant) -> bool {
        self.outcome().is_none() && now < self.deadline
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
    /// No sleeping, no event-loop Result and at most one signal per obligation.
    pub(crate) fn service(
        &mut self,
        now: Instant,
        barrier: Result<(), String>,
        tracer: &mut impl Tracer,
    ) {
        if self.outcome().is_some() {
            return;
        }
        if self.needs_barrier() {
            match barrier {
                Ok(()) => {
                    self.phase = Phase::Finishing {
                        started: now,
                        deadline: self.deadline.min(now + TRACER_BOUND),
                        killed: false,
                    }
                }
                Err(why) => {
                    if now >= self.started + BARRIER_BOUND {
                        self.phase = Phase::Settled(FinishOutcome::UnconfirmedBarrier(why));
                    }
                    return;
                }
            }
        }
        let killed = matches!(
            self.phase,
            Phase::KilledReapPending | Phase::Finishing { killed: true, .. }
        );
        let already_reaped = tracer.already_reaped();
        match tracer.try_reap() {
            Ok(true) => {
                self.phase = Phase::Settled(if already_reaped {
                    FinishOutcome::AlreadyReaped
                } else if killed {
                    FinishOutcome::KilledThenReaped
                } else {
                    FinishOutcome::ReapedNormally
                });
                return;
            }
            Ok(false) => self.last_observation = Some("wait returned no status".into()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                self.last_observation = Some(format!("wait interrupted: {e}"));
            }
            Err(e) => {
                self.phase = Phase::Settled(FinishOutcome::WaitError(format!(
                    "{e}; errno={:?}",
                    e.raw_os_error()
                )));
                return;
            }
        }
        if let Phase::Finishing {
            started,
            deadline,
            killed,
        } = &mut self.phase
        {
            if !*killed && now >= *started + NATURAL_GRACE {
                match tracer.kill() {
                    Ok(()) => *killed = true,
                    Err(e) => {
                        self.phase = Phase::Settled(FinishOutcome::KillFailure(format!(
                            "{e}; errno={:?}",
                            e.raw_os_error()
                        )));
                        return;
                    }
                }
            }
            if now >= *deadline {
                self.overrun = Some(now.saturating_duration_since(*deadline));
                self.phase = if *killed {
                    Phase::KilledReapPending
                } else {
                    Phase::Settled(FinishOutcome::BoundExhausted)
                };
            }
        }
    }
    /// Called only when the final drain's single fixed deadline expires.
    pub(crate) fn finish_drain(&mut self) {
        if self.outcome().is_none() {
            self.phase = Phase::Settled(FinishOutcome::BoundExhausted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        reaped: bool,
        kills: usize,
        wait_error: bool,
        kill_error: bool,
        cached: bool,
    }
    impl Tracer for Fake {
        fn already_reaped(&self) -> bool {
            self.cached
        }
        fn try_reap(&mut self) -> std::io::Result<bool> {
            if self.wait_error {
                Err(std::io::Error::from_raw_os_error(10))
            } else {
                Ok(self.reaped)
            }
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills += 1;
            if self.kill_error {
                Err(std::io::Error::from_raw_os_error(1))
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn retirement_killed_reap_pending_completes_later() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake::default();
        r.service(now, Ok(()), &mut t);
        r.service(now + NATURAL_GRACE, Ok(()), &mut t);
        r.service(now + TRACER_BOUND, Ok(()), &mut t);
        assert!(r.outcome().is_none());
        assert!(r.overrun.is_some());
        t.reaped = true;
        r.service(now + Duration::from_secs(2), Ok(()), &mut t);
        assert_eq!(r.outcome(), Some(&FinishOutcome::KilledThenReaped));
        assert_eq!(t.kills, 1);
    }
    #[test]
    fn retirement_killed_reap_pending_unreaped_at_terminate() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake::default();
        r.service(now, Ok(()), &mut t);
        r.service(now + TRACER_BOUND, Ok(()), &mut t);
        r.finish_drain();
        assert_eq!(r.outcome(), Some(&FinishOutcome::BoundExhausted));
        assert!(!t.reaped);
        assert_eq!(t.kills, 1);
    }
    #[test]
    fn retirement_unconfirmed_then_confirmed() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake {
            reaped: true,
            ..Fake::default()
        };
        r.service(now, Err("lookup unavailable".into()), &mut t);
        assert!(r.needs_barrier());
        r.service(now + NATURAL_GRACE, Ok(()), &mut t);
        assert_eq!(r.outcome(), Some(&FinishOutcome::ReapedNormally));
        assert_eq!(t.kills, 0);
    }
    #[test]
    fn retirement_permanent_wait_error() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake {
            wait_error: true,
            ..Fake::default()
        };
        r.service(now, Ok(()), &mut t);
        let original = r.outcome().cloned();
        assert!(matches!(original, Some(FinishOutcome::WaitError(_))));
        t.wait_error = false;
        t.reaped = true;
        r.service(now + Duration::from_secs(2), Ok(()), &mut t);
        r.finish_drain();
        assert_eq!(r.outcome(), original.as_ref());
    }
    #[test]
    fn retirement_kill_error_is_preserved() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake {
            kill_error: true,
            ..Fake::default()
        };
        r.service(now, Ok(()), &mut t);
        r.service(now + NATURAL_GRACE, Ok(()), &mut t);
        let original = r.outcome().cloned();
        assert!(matches!(original, Some(FinishOutcome::KillFailure(_))));
        t.kill_error = false;
        t.reaped = true;
        r.service(now + Duration::from_secs(2), Ok(()), &mut t);
        assert_eq!(r.outcome(), original.as_ref());
        assert_eq!(t.kills, 1);
    }
    #[test]
    fn retirement_barrier_expiry_never_kills_tracer() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake::default();
        r.service(now + BARRIER_BOUND, Err("unconfirmed".into()), &mut t);
        assert_eq!(
            r.outcome(),
            Some(&FinishOutcome::UnconfirmedBarrier("unconfirmed".into()))
        );
        assert_eq!(t.kills, 0);
        assert!(!r.hold_root(now + BARRIER_BOUND));
    }
    #[test]
    fn retirement_root_hold_has_original_deadline() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake::default();
        r.service(now, Ok(()), &mut t);
        r.service(now + TRACER_BOUND, Ok(()), &mut t);
        assert!(r.hold_root(now + TRACER_BOUND));
        assert!(!r.hold_root(now + BARRIER_BOUND + TRACER_BOUND));
        assert!(r.outcome().is_none()); // ownership remains pending beyond the hold
    }
    #[test]
    fn retirement_owned_cached_wait_is_distinct() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake {
            cached: true,
            reaped: true,
            ..Fake::default()
        };
        r.service(now, Ok(()), &mut t);
        assert_eq!(r.outcome(), Some(&FinishOutcome::AlreadyReaped));
        assert_eq!(t.kills, 0);
    }
}
