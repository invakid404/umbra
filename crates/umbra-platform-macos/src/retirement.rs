//! Nonblocking cleanup of a confirmed-finished inferior (#274).
//! A cleanup error is evidence for final teardown, never a live-session error.
use std::process::ExitStatus;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsError {
    pub kind: std::io::ErrorKind,
    pub raw: Option<i32>,
}
impl From<&std::io::Error> for OsError {
    fn from(error: &std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw: error.raw_os_error(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Pending,
    Reaped(ExitStatus),
    WaitFailed(OsError),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishFailure {
    pub error: Option<OsError>,
    pub elapsed: Duration,
    pub last_observation: Option<Observation>,
    pub later_wait_status: Option<ExitStatus>,
}

pub(crate) const BARRIER_BOUND: Duration = Duration::from_millis(1500);
pub(crate) const TRACER_BOUND: Duration = Duration::from_millis(250);
const NATURAL_GRACE: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishOutcome {
    AlreadyReaped(ExitStatus),
    ReapedNormally(ExitStatus),
    KilledThenReaped(ExitStatus),
    UnconfirmedBarrier(String),
    KillFailure(FinishFailure),
    WaitError(FinishFailure),
    BoundExhausted(FinishFailure),
}
impl FinishOutcome {
    pub(crate) fn success(&self) -> bool {
        matches!(
            self,
            Self::AlreadyReaped(_) | Self::ReapedNormally(_) | Self::KilledThenReaped(_)
        )
    }
}

pub(crate) trait Tracer {
    fn already_reaped(&self) -> bool {
        false
    }
    /// Return the actual owned wait status, including Child's cache.
    fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>>;
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
    pub(crate) last_observation: Option<Observation>,
    last_error: Option<OsError>,
}
impl Retirement {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            started: now,
            deadline: now + BARRIER_BOUND + TRACER_BOUND,
            phase: Phase::Barrier,
            overrun: None,
            last_observation: None,
            last_error: None,
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
    fn failure(&self, now: Instant) -> FinishFailure {
        FinishFailure {
            error: self.last_error.clone(),
            elapsed: self.elapsed(now),
            last_observation: self.last_observation.clone(),
            later_wait_status: None,
        }
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
            Ok(Some(status)) => {
                self.phase = Phase::Settled(if already_reaped {
                    FinishOutcome::AlreadyReaped(status)
                } else if killed {
                    FinishOutcome::KilledThenReaped(status)
                } else {
                    FinishOutcome::ReapedNormally(status)
                });
                return;
            }
            Ok(None) => self.last_observation = Some(Observation::Pending),
            Err(e) => {
                let error = OsError::from(&e);
                self.last_error = Some(error.clone());
                self.last_observation = Some(Observation::WaitFailed(error));
                if e.kind() != std::io::ErrorKind::Interrupted {
                    self.phase = Phase::Settled(FinishOutcome::WaitError(self.failure(now)));
                    return;
                }
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
                        let mut failure = self.failure(now);
                        failure.error = Some(OsError::from(&e));
                        // One additional owned wait records a possible race with exit;
                        // it never erases the signal failure or starts a fresh deadline.
                        failure.last_observation = Some(match tracer.try_reap() {
                            Ok(Some(status)) => {
                                failure.later_wait_status = Some(status);
                                Observation::Reaped(status)
                            }
                            Ok(None) => Observation::Pending,
                            Err(error) => Observation::WaitFailed(OsError::from(&error)),
                        });
                        self.phase = Phase::Settled(FinishOutcome::KillFailure(failure));
                        return;
                    }
                }
            }
            if now >= *deadline {
                self.overrun = Some(now.saturating_duration_since(*deadline));
                self.phase = if *killed {
                    Phase::KilledReapPending
                } else {
                    Phase::Settled(FinishOutcome::BoundExhausted(self.failure(now)))
                };
            }
        }
    }
    /// Called only when the final drain's single fixed deadline expires.
    pub(crate) fn finish_drain(&mut self, now: Instant) {
        if self.outcome().is_none() {
            self.phase = Phase::Settled(FinishOutcome::BoundExhausted(self.failure(now)));
        }
    }
}

/// Typed evidence from the one bounded emergency attempt. The original
/// retirement outcome remains in its report even if this attempt reaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TracerOperation {
    Wait(Result<Option<ExitStatus>, OsError>),
    Kill(Result<(), OsError>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencyFinish {
    pub outcome: FinishOutcome,
    pub operations: Vec<TracerOperation>,
    pub elapsed: Duration,
}
pub(crate) fn emergency_finish(tracer: &mut impl Tracer, limit: Instant) -> EmergencyFinish {
    struct Recorded<'a, T> {
        tracer: &'a mut T,
        operations: Vec<TracerOperation>,
    }
    impl<T: Tracer> Tracer for Recorded<'_, T> {
        fn already_reaped(&self) -> bool {
            self.tracer.already_reaped()
        }
        fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
            let result = self.tracer.try_reap();
            self.operations.push(TracerOperation::Wait(
                result.as_ref().map(|s| *s).map_err(OsError::from),
            ));
            result
        }
        fn kill(&mut self) -> std::io::Result<()> {
            let result = self.tracer.kill();
            self.operations.push(TracerOperation::Kill(
                result.as_ref().copied().map_err(OsError::from),
            ));
            result
        }
    }
    let started = crate::clock::now();
    let mut state = Retirement::new(started);
    let mut recorded = Recorded {
        tracer,
        operations: vec![],
    };
    loop {
        let now = crate::clock::now();
        state.service(now, Ok(()), &mut recorded);
        if state.outcome().is_some() {
            break;
        }
        if now >= limit {
            state.finish_drain(now);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    EmergencyFinish {
        outcome: state.outcome().unwrap().clone(),
        operations: recorded.operations,
        elapsed: crate::clock::now().saturating_duration_since(started),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    fn status() -> ExitStatus {
        ExitStatus::from_raw(7 << 8)
    }
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
        fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
            if self.wait_error {
                Err(std::io::Error::from_raw_os_error(10))
            } else {
                Ok(self.reaped.then(status))
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
        assert_eq!(
            r.outcome(),
            Some(&FinishOutcome::KilledThenReaped(status()))
        );
        assert_eq!(t.kills, 1);
    }
    #[test]
    fn retirement_killed_reap_pending_unreaped_at_terminate() {
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut t = Fake::default();
        r.service(now, Ok(()), &mut t);
        r.service(now + TRACER_BOUND, Ok(()), &mut t);
        r.finish_drain(now + Duration::from_secs(2));
        assert!(matches!(
            r.outcome(),
            Some(FinishOutcome::BoundExhausted(_))
        ));
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
        assert_eq!(r.outcome(), Some(&FinishOutcome::ReapedNormally(status())));
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
        assert!(matches!(&original, Some(FinishOutcome::WaitError(failure))
            if failure.error == Some(OsError { kind: std::io::Error::from_raw_os_error(10).kind(), raw: Some(10) })
                && failure.elapsed == Duration::ZERO
                && failure.last_observation == Some(Observation::WaitFailed(OsError { kind: std::io::Error::from_raw_os_error(10).kind(), raw: Some(10) }))));
        t.wait_error = false;
        t.reaped = true;
        r.service(now + Duration::from_secs(2), Ok(()), &mut t);
        r.finish_drain(now + Duration::from_secs(2));
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
        assert!(
            matches!(&original, Some(FinishOutcome::KillFailure(failure))
            if failure.error == Some(OsError { kind: std::io::ErrorKind::PermissionDenied, raw: Some(1) })
                && failure.elapsed == NATURAL_GRACE
                && failure.last_observation == Some(Observation::Pending)
                && failure.later_wait_status.is_none())
        );
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
        assert_eq!(r.outcome(), Some(&FinishOutcome::AlreadyReaped(status())));
        assert_eq!(t.kills, 0);
    }
    #[test]
    fn retirement_kill_failure_keeps_later_status() {
        struct Race {
            waits: usize,
        }
        impl Tracer for Race {
            fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
                self.waits += 1;
                Ok((self.waits == 3).then(status))
            }
            fn kill(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from_raw_os_error(libc::ESRCH))
            }
        }
        let now = Instant::now();
        let mut r = Retirement::new(now);
        let mut tracer = Race { waits: 0 };
        r.service(now, Ok(()), &mut tracer);
        r.service(now + NATURAL_GRACE, Ok(()), &mut tracer);
        let Some(FinishOutcome::KillFailure(failure)) = r.outcome() else {
            panic!("signal error must remain failure")
        };
        assert_eq!(
            failure.error,
            Some(OsError::from(&std::io::Error::from_raw_os_error(
                libc::ESRCH
            )))
        );
        assert_eq!(failure.later_wait_status, Some(status()));
        assert_eq!(
            failure.last_observation,
            Some(Observation::Reaped(status()))
        );
        assert_eq!(failure.elapsed, NATURAL_GRACE);
        assert!(!r.outcome().unwrap().success());
    }
    #[test]
    fn retirement_exhaustion_retains_interrupted_wait_evidence() {
        struct Interrupted;
        impl Tracer for Interrupted {
            fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
                Err(std::io::Error::from_raw_os_error(libc::EINTR))
            }
            fn kill(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let now = Instant::now();
        let mut r = Retirement::new(now);
        r.service(now, Ok(()), &mut Interrupted);
        r.service(now + TRACER_BOUND, Ok(()), &mut Interrupted);
        assert!(r.outcome().is_none());
        r.finish_drain(now + TRACER_BOUND);
        let Some(FinishOutcome::BoundExhausted(failure)) = r.outcome() else {
            panic!("missing exhaustion")
        };
        let error = OsError {
            kind: std::io::ErrorKind::Interrupted,
            raw: Some(libc::EINTR),
        };
        assert_eq!(failure.error, Some(error.clone()));
        assert_eq!(
            failure.last_observation,
            Some(Observation::WaitFailed(error))
        );
        assert_eq!(failure.elapsed, TRACER_BOUND);
    }
}
