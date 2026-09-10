//! The finite outage state machine.
//!
//! Five states, taken verbatim from the table in `docs/design/failure-model.md`,
//! and one transition per crash window that document enumerates. Every window
//! either resolves to [`RecoveryState::Running`] or stops. None of them
//! fabricates a success, a release or a clean checkpoint, and none of them
//! reaches a state by consulting a clock for *authority* — the clocks here bound
//! how long recovery may try, never who owns the run.
//!
//! # The error latch
//!
//! The first failure a window produces is latched and kept. Later attempts
//! record that they happened but never overwrite it, so a `NFS4ERR_NOSPC` that
//! opened an outage is still `NFS4ERR_NOSPC` after three reconnects rather than
//! decaying into whatever the last attempt happened to return. The failure model
//! calls this the "original error latch" and requires it as persistent evidence;
//! [`RetainedError`] from the frozen error facade is the value that carries it,
//! so the verbatim status word survives into the durable record.
//!
//! # Stopping is a value, not a flag
//!
//! [`OutageMachine::stop`] consumes the machine and returns [`StoppedSafely`].
//! A caller holding one has no machine left to drive, which is what "give up
//! safely" has to mean if it is to be more than a comment.
//!
//! # Split brain and hard partition
//!
//! Not implemented here, and deliberately so. Two hosts holding conflicting
//! *valid* ownership evidence needs a fence receipt and a cutoff before a winner
//! may be selected; the failure model says so and `docs/design/fencing-survey.md`
//! is where that authority is being designed. M1 answers by stopping **both**
//! sides and recording [`Deferral::M3Fencing`]. An increasing epoch is not a
//! fence, a client-side marker check is not a fence, and nothing in this module
//! pretends otherwise.

use umbra_core::{IdempotencyKey, LeaseEpoch, OperationId};

use crate::error::{
    AuthorityError, FacadeError, Nfs4Status, ReplayError, RetainedError, TransportError,
};
use crate::transport::Deadline;

/// The five states the failure model defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryState {
    /// Authority, connection and required evidence are valid.
    Running,
    /// New mutations are stopped and the tree quiesced while bounded outstanding
    /// operations are resolved. No success, release or checkpoint is fabricated.
    Recovering,
    /// Timeout, missing authority or unavailable evidence. The writer marker,
    /// the replay data and the diagnostic are retained. Not corrupted.
    BlockedRecoverable,
    /// Contradictory identities, invalid frames, or demonstrated loss of data
    /// that was previously promised durable. Remaining bytes are preserved and
    /// no automatic repair or relaunch is attempted.
    Corrupted,
    /// An agent or child failed. Storage may be entirely intact; no clean
    /// checkpoint is inferred from an exit status.
    FailedTracee,
}

impl RecoveryState {
    /// Whether the run has stopped in this state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::BlockedRecoverable | Self::Corrupted | Self::FailedTracee
        )
    }

    /// Whether a mutation may be admitted in this state.
    pub fn admits_mutation(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Work this milestone does not own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deferral {
    /// Needs the M3 fencing authority: a fence receipt and a cutoff that
    /// excludes the old writer's mutations before a winner is selected.
    M3Fencing,
    /// Needs M3's remote-persistence qualification: independent digests,
    /// before/after wire traces and cold observations.
    M3Qualification,
}

impl Deferral {
    /// Why M1 stops instead of resolving.
    pub fn note(self) -> &'static str {
        match self {
            Self::M3Fencing => {
                "conflicting valid ownership evidence: both sides stop until M3 fencing authority \
                 supplies a fence receipt and a cutoff; an increasing epoch is not a fence"
            }
            Self::M3Qualification => {
                "server-side persistence assumptions are not qualified in M1; a process restart \
                 is not power-loss qualification"
            }
        }
    }
}

/// The crash windows M1 must cover, one per row of the failure model's taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrashWindow {
    /// Umbra died before any intent reached the log.
    UmbraCrashBeforeWrite,
    /// Umbra died with a durable intent recorded and no outcome.
    UmbraCrashMidWrite,
    /// Umbra died after the result was durably recorded, before replying.
    UmbraCrashAfterWrite,
    /// A traced child failed. Storage is not implicated by an exit status.
    TraceeCrash,
    /// The connection dropped and may come back within the outage budget.
    TransientPartition,
    /// Ganesha restarted gracefully; v4.0 `CLAIM_PREVIOUS` inside grace applies.
    GaneshaGracefulRestart,
    /// The server answered `NFS4ERR_NOSPC` or `NFS4ERR_DQUOT`.
    ServerEnospc,
    /// The server answered `NFS4ERR_IO`, or a stable write failed.
    ServerEio,
    /// `NFS4ERR_STALE` or `NFS4ERR_FHEXPIRED` on an existing handle.
    StaleFilehandle,
    /// Umbra was stopped with SIGSTOP and continued with SIGCONT.
    UmbraSigstopResume,
    /// The client lost power with a WRITE or COMMIT in flight.
    ClientPowerLossInFlight,
    /// The server or its storage lost power with a WRITE or COMMIT in flight.
    ServerPowerLoss,
    /// Two hosts hold conflicting valid ownership evidence.
    SplitBrain,
    /// The partition outlasted every budget and both sides may still be live.
    HardPartition,
}

impl CrashWindow {
    /// Every window this module answers, for a matrix that must stay exhaustive.
    pub const ALL: [Self; 14] = [
        Self::UmbraCrashBeforeWrite,
        Self::UmbraCrashMidWrite,
        Self::UmbraCrashAfterWrite,
        Self::TraceeCrash,
        Self::TransientPartition,
        Self::GaneshaGracefulRestart,
        Self::ServerEnospc,
        Self::ServerEio,
        Self::StaleFilehandle,
        Self::UmbraSigstopResume,
        Self::ClientPowerLossInFlight,
        Self::ServerPowerLoss,
        Self::SplitBrain,
        Self::HardPartition,
    ];

    /// The deferral this window carries, when M1 does not own its resolution.
    pub fn deferral(self) -> Option<Deferral> {
        match self {
            Self::SplitBrain | Self::HardPartition => Some(Deferral::M3Fencing),
            Self::ServerPowerLoss => Some(Deferral::M3Qualification),
            _ => None,
        }
    }
}

/// Where this session stands on admission when a window opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionStanding {
    /// This session holds admission at this epoch.
    Held(LeaseEpoch),
    /// This session was denied; a marker held by someone else stands.
    Denied(LeaseEpoch),
    /// Admission could not be proven at all.
    Unproven,
}

impl AdmissionStanding {
    /// The epoch, when one was observed.
    pub fn epoch(self) -> Option<LeaseEpoch> {
        match self {
            Self::Held(epoch) | Self::Denied(epoch) => Some(epoch),
            Self::Unproven => None,
        }
    }

    /// Whether this session may still mutate.
    pub fn is_held(self) -> bool {
        matches!(self, Self::Held(_))
    }
}

/// What was observed when a window opened.
///
/// Every field is something the failure model names as evidence for at least one
/// row, and every field defaults to the conservative reading — nothing durable,
/// nothing proven, nothing excluded — so a caller that forgets to set one gets a
/// stop rather than an optimistic transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    /// Operation identity the latched error is bound to.
    pub operation: OperationId,
    /// Key the latched error settles, if it settles one.
    pub key: IdempotencyKey,
    /// The verbatim failure that opened the window.
    pub error: Option<FacadeError>,
    /// Where this session stands on admission.
    pub admission: AdmissionStanding,
    /// Monotonic milliseconds the outage has lasted.
    pub elapsed_millis: u64,
    /// Whether a durable intent for the in-flight mutation exists.
    pub intent_durable: bool,
    /// Whether the payload needed to rebuild the mutation survived durably.
    pub payload_durable: bool,
    /// Whether the committed result was durably recorded before the crash.
    pub result_durable: bool,
    /// Whether object identity was re-proven after the interruption.
    pub identity_proven: Option<bool>,
    /// Whether every call this session issued is proven withdrawn or settled.
    pub outstanding_io_excluded: bool,
    /// Whether a v4.0 `CLAIM_PREVIOUS` pass recovered every target.
    ///
    /// Named for what it reports rather than mirroring
    /// `ReclaimReport::is_complete`, so nothing here reads as the v4.1
    /// `RECLAIM_COMPLETE` operation, which is out of scope and unrepresentable
    /// in the frozen transport.
    pub reclaim_recovered_all: Option<bool>,
    /// The server's advertised grace, in seconds.
    pub grace_seconds: u32,
    /// Monotonic milliseconds the process was suspended.
    pub suspend_gap_millis: u64,
    /// Whether loss of previously promised durable data was actually proven.
    pub proven_loss: bool,
}

impl Evidence {
    /// Conservative evidence bound to one operation identity.
    pub fn for_operation(operation: OperationId, key: IdempotencyKey) -> Self {
        Self {
            operation,
            key,
            error: None,
            admission: AdmissionStanding::Unproven,
            elapsed_millis: 0,
            intent_durable: false,
            payload_durable: false,
            result_durable: false,
            identity_proven: None,
            outstanding_io_excluded: false,
            reclaim_recovered_all: None,
            grace_seconds: 0,
            suspend_gap_millis: 0,
            proven_loss: false,
        }
    }

    /// The verbatim failure that opened the window.
    pub fn with_error(mut self, error: FacadeError) -> Self {
        self.error = Some(error);
        self
    }

    /// Where this session stands on admission.
    pub fn with_admission(mut self, admission: AdmissionStanding) -> Self {
        self.admission = admission;
        self
    }

    /// How long the outage has lasted.
    pub fn elapsed(mut self, millis: u64) -> Self {
        self.elapsed_millis = millis;
        self
    }

    /// What survived durably: intent, payload, result.
    pub fn durable(mut self, intent: bool, payload: bool, result: bool) -> Self {
        self.intent_durable = intent;
        self.payload_durable = payload;
        self.result_durable = result;
        self
    }

    /// Whether object identity was re-proven.
    pub fn identity(mut self, proven: bool) -> Self {
        self.identity_proven = Some(proven);
        self
    }

    /// Whether outstanding I/O is proven excluded.
    pub fn io_excluded(mut self, excluded: bool) -> Self {
        self.outstanding_io_excluded = excluded;
        self
    }

    /// The result of a `CLAIM_PREVIOUS` pass, and the grace it ran inside.
    pub fn reclaim(mut self, recovered_all: bool, grace_seconds: u32) -> Self {
        self.reclaim_recovered_all = Some(recovered_all);
        self.grace_seconds = grace_seconds;
        self
    }

    /// How long the process was suspended.
    pub fn suspended(mut self, millis: u64) -> Self {
        self.suspend_gap_millis = millis;
        self
    }

    /// Loss of previously promised durable data was actually demonstrated.
    pub fn proven_loss(mut self) -> Self {
        self.proven_loss = true;
        self
    }
}

/// The finite budgets from the failure model's "Finite budgets and replay
/// acceptance" section.
///
/// These bound how long recovery may keep trying. None of them bounds ownership:
/// no elapsed value anywhere in this module releases a marker or admits a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutageBudget {
    /// Deadline applied to one RPC attempt.
    pub rpc_deadline: Deadline,
    /// First reconnect backoff.
    pub reconnect_backoff_initial_millis: u64,
    /// Ceiling the reconnect backoff doubles up to.
    pub reconnect_backoff_max_millis: u64,
    /// Budget for an ordinary outage.
    pub ordinary_outage_millis: u64,
    /// Extra budget granted on top of the server's grace during reclaim.
    pub grace_extra_millis: u64,
}

impl OutageBudget {
    /// The design defaults: 5 s per attempt, 250 ms doubling to 5 s, a 30 s
    /// ordinary outage budget and grace plus 30 s for reclaim.
    pub const DESIGN_DEFAULTS: Self = Self {
        rpc_deadline: Deadline { millis: 5_000 },
        reconnect_backoff_initial_millis: 250,
        reconnect_backoff_max_millis: 5_000,
        ordinary_outage_millis: 30_000,
        grace_extra_millis: 30_000,
    };

    /// The bounded budget a grace recovery gets: the server's grace plus the
    /// extra. A 90-second grace yields 120 seconds.
    pub fn grace_budget_millis(&self, grace_seconds: u32) -> u64 {
        u64::from(grace_seconds)
            .saturating_mul(1_000)
            .saturating_add(self.grace_extra_millis)
    }

    /// The backoff for reconnect attempt `attempt`, zero-based, doubling from the
    /// initial value and saturating at the ceiling.
    pub fn backoff_millis(&self, attempt: u32) -> u64 {
        self.reconnect_backoff_initial_millis
            .saturating_mul(1u64 << attempt.min(63))
            .min(self.reconnect_backoff_max_millis)
    }
}

impl Default for OutageBudget {
    fn default() -> Self {
        Self::DESIGN_DEFAULTS
    }
}

/// A run that has stopped, with everything the failure model requires retained.
///
/// Produced by consuming the machine, so a caller holding one cannot carry on
/// driving transitions.
#[derive(Clone, Debug)]
pub struct StoppedSafely {
    state: RecoveryState,
    retained: Option<RetainedError>,
    deferral: Option<Deferral>,
    marker_retained: bool,
    attempts: u32,
    diagnostic: String,
}

impl StoppedSafely {
    /// The state the run stopped in.
    pub fn state(&self) -> RecoveryState {
        self.state
    }

    /// The latched original failure.
    pub fn retained(&self) -> Option<&RetainedError> {
        self.retained.as_ref()
    }

    /// The verbatim `NFS4ERR_*` the outage started with, when it came from the
    /// server.
    pub fn status(&self) -> Option<Nfs4Status> {
        self.retained.as_ref().and_then(|e| e.error().status())
    }

    /// The milestone that owns the resolution, when M1 does not.
    pub fn deferral(&self) -> Option<Deferral> {
        self.deferral
    }

    /// Whether the writer marker is still held. Always true for a stop: the
    /// failure model forbids releasing on the way down.
    pub fn marker_retained(&self) -> bool {
        self.marker_retained
    }

    /// Recovery attempts made before stopping.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The human-readable reason, retained alongside the typed evidence.
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

/// Drives one run through the crash windows.
#[derive(Clone, Debug)]
pub struct OutageMachine {
    state: RecoveryState,
    budget: OutageBudget,
    retained: Option<RetainedError>,
    deferral: Option<Deferral>,
    marker_retained: bool,
    attempts: u32,
    durable_evidence: bool,
}

impl OutageMachine {
    /// A running machine under `budget`.
    ///
    /// `durable_evidence` says whether the log behind this run reaches a
    /// persistence boundary; it is what a latched [`RetainedError`] reports for
    /// [`RetainedError::is_durable`], so a volatile run cannot claim otherwise.
    pub fn running(budget: OutageBudget, durable_evidence: bool) -> Self {
        Self {
            state: RecoveryState::Running,
            budget,
            retained: None,
            deferral: None,
            marker_retained: true,
            attempts: 0,
            durable_evidence,
        }
    }

    /// The current state.
    pub fn state(&self) -> RecoveryState {
        self.state
    }

    /// The budgets in force.
    pub fn budget(&self) -> &OutageBudget {
        &self.budget
    }

    /// The latched original failure, preserved across every attempt.
    pub fn retained(&self) -> Option<&RetainedError> {
        self.retained.as_ref()
    }

    /// The verbatim `NFS4ERR_*` the latch holds, when it came from the server.
    pub fn status(&self) -> Option<Nfs4Status> {
        self.retained.as_ref().and_then(|e| e.error().status())
    }

    /// The milestone that owns this window's resolution, when M1 does not.
    pub fn deferral(&self) -> Option<Deferral> {
        self.deferral
    }

    /// Recovery attempts made so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the writer marker is still held.
    pub fn marker_retained(&self) -> bool {
        self.marker_retained
    }

    /// Drive one crash window and return the state it settled in.
    ///
    /// **F06.** A terminal state is terminal. `decide` answers each window on the
    /// window's own evidence, with no memory of where the run already stopped, so
    /// driving a fresh window into a stopped machine used to overwrite the stop:
    /// a run put into [`RecoveryState::Corrupted`] by a proven durable loss went
    /// back to [`RecoveryState::Running`] on a later
    /// [`CrashWindow::StaleFilehandle`] whose identity happened to prove, and
    /// [`RecoveryState::admits_mutation`] then said yes. That is the opposite of
    /// what [`RecoveryState::is_terminal`] promises, and it is the failure model's
    /// "no automatic repair or relaunch" read backwards.
    ///
    /// The stop is returned before anything can move: the attempt is not counted,
    /// no later error displaces the latched one, and no window's deferral is
    /// recorded against a run that is no longer being driven. Everything a stopped
    /// machine already holds — diagnosis, marker retention, counters — is exactly
    /// what it held when it stopped. Nothing here reopens a terminal state; only
    /// building a new machine does, which is the operator intervention the state
    /// is for.
    pub fn enter(&mut self, window: CrashWindow, evidence: &Evidence) -> RecoveryState {
        if self.state.is_terminal() {
            return self.state;
        }
        self.attempts = self.attempts.saturating_add(1);
        if let Some(error) = &evidence.error {
            self.latch(evidence.operation, &evidence.key, error.clone());
        }
        if let Some(deferral) = window.deferral() {
            self.deferral = Some(deferral);
        }
        self.state = self.decide(window, evidence);
        self.state
    }

    /// Latch the first failure. Later ones are counted, never substituted.
    ///
    /// This is the whole of retained-error preservation: the original status word
    /// is what a caller reads after any number of recovery attempts, because
    /// there is no path in this type that replaces a latched value.
    fn latch(&mut self, operation: OperationId, key: &IdempotencyKey, error: FacadeError) {
        if self.retained.is_none() {
            self.retained = Some(RetainedError::record(
                operation,
                key.clone(),
                error,
                self.durable_evidence,
            ));
        }
    }

    fn decide(&self, window: CrashWindow, evidence: &Evidence) -> RecoveryState {
        match window {
            // "Recover journal and primitives without assuming owner release."
            // A crashed controller leaves an abandoned session; M1 never steals
            // its marker, so a follow-on process is blocked whatever the journal
            // holds. What differs between the three windows is what recovery will
            // be *able* to do once authority is granted, not whether it may run.
            CrashWindow::UmbraCrashBeforeWrite
            | CrashWindow::UmbraCrashMidWrite
            | CrashWindow::UmbraCrashAfterWrite => {
                if evidence.admission.is_held() && evidence.outstanding_io_excluded {
                    // Same session, same admission, old I/O ruled out: this is the
                    // in-process recovery the failure model does allow.
                    RecoveryState::Recovering
                } else {
                    RecoveryState::BlockedRecoverable
                }
            }

            // "Record failed execution; do not treat agent buffers as persisted."
            // Storage corruption is never inferred from a nonzero exit.
            CrashWindow::TraceeCrash => RecoveryState::FailedTracee,

            // Bounded reconnect inside the budget; past it the prolonged-partition
            // row applies and the run is left blocked with the marker held.
            CrashWindow::TransientPartition => {
                if evidence.elapsed_millis > self.budget.ordinary_outage_millis {
                    RecoveryState::BlockedRecoverable
                } else {
                    RecoveryState::Recovering
                }
            }

            // v4.0 CLAIM_PREVIOUS inside a bounded grace. "Stop if reclaim cannot
            // establish valid object state."
            CrashWindow::GaneshaGracefulRestart => {
                let budget = self.budget.grace_budget_millis(evidence.grace_seconds);
                match evidence.reclaim_recovered_all {
                    _ if evidence.elapsed_millis > budget => RecoveryState::BlockedRecoverable,
                    Some(true) => RecoveryState::Running,
                    Some(false) => RecoveryState::BlockedRecoverable,
                    None => RecoveryState::Recovering,
                }
            }

            // "Report actual error/short count, retain intent and partial effects;
            // block uncertain mutations."
            CrashWindow::ServerEnospc => RecoveryState::BlockedRecoverable,

            // "Latch original write/barrier failure. Do not issue a clean receipt
            // or release. Mark corrupted only on proven contradiction/loss."
            CrashWindow::ServerEio => {
                if evidence.proven_loss {
                    RecoveryState::Corrupted
                } else {
                    RecoveryState::BlockedRecoverable
                }
            }

            // "Re-resolve only if persistent object identity proves the same
            // object. If identity cannot be proven, stop."
            CrashWindow::StaleFilehandle => match evidence.identity_proven {
                Some(true) => RecoveryState::Running,
                _ => RecoveryState::BlockedRecoverable,
            },

            // "On resume, recheck local deadline and remote state before any
            // queued mutation. If queued remote effects cannot be excluded, stay
            // blocked." Silence during the stop proves nothing either way.
            CrashWindow::UmbraSigstopResume => {
                if !evidence.outstanding_io_excluded || !evidence.admission.is_held() {
                    RecoveryState::BlockedRecoverable
                } else if evidence.suspend_gap_millis > self.budget.ordinary_outage_millis {
                    // The authority this session held may have lapsed while it
                    // could not check. Poisoned, not silently resumed.
                    RecoveryState::BlockedRecoverable
                } else {
                    RecoveryState::Recovering
                }
            }

            // "Rebuild using server-retained intent/payload... If payload was not
            // durable, do not claim recovery of those bytes." The M3 authority
            // gate precedes mutation replay, so a follow-on process stays blocked
            // even when the bytes did survive.
            CrashWindow::ClientPowerLossInFlight => RecoveryState::BlockedRecoverable,

            // M3 qualifies server-side persistence. A stop here is the honest
            // answer; a proven loss after a durable receipt is a qualification
            // failure, which is corruption.
            CrashWindow::ServerPowerLoss => {
                if evidence.proven_loss {
                    RecoveryState::Corrupted
                } else {
                    RecoveryState::BlockedRecoverable
                }
            }

            // Both sides stop. No winner is selected here, ever.
            CrashWindow::SplitBrain | CrashWindow::HardPartition => {
                RecoveryState::BlockedRecoverable
            }
        }
    }

    /// Give up safely because writer authority could not be proven.
    ///
    /// Consumes the machine. The marker is retained, the latch is preserved, and
    /// the reason is recorded verbatim.
    pub fn give_up_unproven_authority(mut self, error: AuthorityError) -> StoppedSafely {
        let diagnostic = format!("authority could not be proven: {error}");
        if self.retained.is_none() {
            // Nothing had failed yet on the wire; the authority refusal itself is
            // the original error, so it is what gets latched.
            self.retained = Some(RetainedError::record(
                OperationId(uuid::Uuid::nil()),
                IdempotencyKey(String::new()),
                FacadeError::Authority(error),
                self.durable_evidence,
            ));
        }
        self.state = RecoveryState::BlockedRecoverable;
        self.stop(diagnostic)
    }

    /// Stop, retaining everything the failure model requires.
    pub fn stop(self, diagnostic: impl Into<String>) -> StoppedSafely {
        StoppedSafely {
            state: self.state,
            retained: self.retained,
            deferral: self.deferral,
            // Never released on the way down: the failure model forbids a release
            // that could let uncertain old I/O overlap a new owner.
            marker_retained: true,
            attempts: self.attempts,
            diagnostic: diagnostic.into(),
        }
    }
}

/// The verbatim failure a transport disconnect produces, for callers building
/// evidence for [`CrashWindow::TransientPartition`].
pub fn disconnected(epoch: crate::transport::ConnectionEpoch, detail: &str) -> FacadeError {
    FacadeError::Transport(TransportError::Disconnected {
        epoch,
        detail: detail.to_owned(),
    })
}

/// The verbatim failure a lost payload produces, for callers building evidence
/// for [`CrashWindow::ClientPowerLossInFlight`].
pub fn payload_missing() -> FacadeError {
    FacadeError::Replay(ReplayError::PayloadMissing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorClass;
    use crate::transport::{ConnectionEpoch, OpCode};
    use uuid::Uuid;

    fn evidence() -> Evidence {
        Evidence::for_operation(
            OperationId(Uuid::from_u128(0x1234)),
            IdempotencyKey("outage-test".into()),
        )
    }

    fn machine() -> OutageMachine {
        OutageMachine::running(OutageBudget::DESIGN_DEFAULTS, false)
    }

    #[test]
    fn the_design_budgets_are_the_documented_numbers() {
        let budget = OutageBudget::DESIGN_DEFAULTS;
        assert_eq!(budget.rpc_deadline.millis, 5_000);
        assert_eq!(budget.ordinary_outage_millis, 30_000);
        assert_eq!(
            budget.grace_budget_millis(90),
            120_000,
            "a 90-second grace gets a 120-second budget"
        );
        assert_eq!(budget.backoff_millis(0), 250);
        assert_eq!(budget.backoff_millis(1), 500);
        assert_eq!(
            budget.backoff_millis(20),
            5_000,
            "backoff saturates at the ceiling"
        );
    }

    #[test]
    fn the_first_error_is_latched_and_later_attempts_never_replace_it() {
        // Driven through a *non-terminal* window on purpose. A terminal state
        // stops the machine outright (F06), so repeating a window that stops it
        // would prove the latch survives only because nothing ran at all. A
        // bounded transient partition keeps returning `Recovering`, so every one
        // of these really is another attempt against a live machine.
        let mut machine = machine();
        let original = FacadeError::protocol(Nfs4Status::NOSPC, OpCode::Write, 1);
        assert_eq!(
            machine.enter(
                CrashWindow::TransientPartition,
                &evidence().with_error(original.clone()),
            ),
            RecoveryState::Recovering
        );
        for generic in [
            FacadeError::Transport(TransportError::Connect("later attempt".into())),
            FacadeError::Replay(ReplayError::Indeterminate),
            FacadeError::protocol(Nfs4Status::SERVERFAULT, OpCode::Write, 1),
        ] {
            assert_eq!(
                machine.enter(
                    CrashWindow::TransientPartition,
                    &evidence().with_error(generic)
                ),
                RecoveryState::Recovering
            );
        }
        assert_eq!(machine.attempts(), 4);
        assert_eq!(
            machine.status(),
            Some(Nfs4Status::NOSPC),
            "the original NFS4ERR_NOSPC must survive every later attempt"
        );
        assert_eq!(machine.retained().expect("latched").error(), &original);
    }

    /// **F06.** Every terminal state stays terminal, whatever window arrives next.
    ///
    /// `decide` answers each window on that window's own evidence, with no memory
    /// of where the run already stopped. So a machine that had stopped went back
    /// to `Running` on the next window whose evidence happened to be good — a
    /// proven durable loss became `Corrupted`, and one `StaleFilehandle` with a
    /// proven identity reopened it for mutations.
    #[test]
    fn f06_a_terminal_state_never_returns_to_running() {
        // Each of the three actual terminal variants, reached the way the failure
        // model reaches it, then driven with the window that used to reopen it.
        for (window, expected) in [
            (CrashWindow::ServerEio, RecoveryState::Corrupted),
            (CrashWindow::ServerEnospc, RecoveryState::BlockedRecoverable),
            (CrashWindow::TraceeCrash, RecoveryState::FailedTracee),
        ] {
            let mut machine = machine();
            // Only the EIO window needs proven loss to reach its stop; the other
            // two stop on the window alone, and the extra evidence is inert.
            machine.enter(window, &evidence().proven_loss());
            assert_eq!(machine.state(), expected);
            assert!(machine.state().is_terminal());
            let attempts = machine.attempts();
            let retained = machine.retained().cloned();

            // The window whose `decide` arm answers `Running` outright.
            assert_eq!(
                machine.enter(CrashWindow::StaleFilehandle, &evidence().identity(true),),
                expected,
                "a proven identity must not reopen a {expected:?} run"
            );
            // And the other one, which needs a full reclaim.
            assert_eq!(
                machine.enter(
                    CrashWindow::GaneshaGracefulRestart,
                    &evidence().reclaim(true, 90),
                ),
                expected,
                "a complete reclaim must not reopen a {expected:?} run either"
            );
            assert!(
                !machine.state().admits_mutation(),
                "a stopped run admits no mutation"
            );

            // Everything the stop retained is exactly what it retained.
            assert_eq!(
                machine.attempts(),
                attempts,
                "a stopped machine counts no further attempts"
            );
            assert_eq!(machine.retained().cloned(), retained);
            assert!(machine.marker_retained());
        }
    }

    /// **F06.** A later window's deferral is not recorded against a run that has
    /// already stopped, and a later error does not displace the latched one.
    #[test]
    fn f06_a_stopped_run_records_no_later_deferral_or_error() {
        let mut machine = machine();
        let original = FacadeError::protocol(Nfs4Status::IO, OpCode::Write, 1);
        machine.enter(
            CrashWindow::ServerEio,
            &evidence().proven_loss().with_error(original.clone()),
        );
        assert_eq!(machine.state(), RecoveryState::Corrupted);
        assert_eq!(machine.deferral(), None);

        // SplitBrain defers to M3 and would latch its own error.
        machine.enter(
            CrashWindow::SplitBrain,
            &evidence().with_error(FacadeError::Replay(ReplayError::Indeterminate)),
        );
        assert_eq!(machine.state(), RecoveryState::Corrupted);
        assert_eq!(
            machine.deferral(),
            None,
            "a stopped run is not being driven, so no window's deferral applies to it"
        );
        assert_eq!(machine.retained().expect("latched").error(), &original);
    }

    /// **F06.** A non-terminal state is still driven normally: the guard stops
    /// stopped runs, not recovery.
    #[test]
    fn f06_a_recovering_run_is_still_driven() {
        let mut machine = machine();
        assert_eq!(
            machine.enter(CrashWindow::TransientPartition, &evidence()),
            RecoveryState::Recovering
        );
        assert_eq!(
            machine.enter(CrashWindow::StaleFilehandle, &evidence().identity(true),),
            RecoveryState::Running,
            "recovery may still conclude the run is healthy"
        );
        assert_eq!(machine.attempts(), 2);
    }

    #[test]
    fn a_tracee_exit_never_implies_storage_corruption() {
        let mut machine = machine();
        let state = machine.enter(CrashWindow::TraceeCrash, &evidence());
        assert_eq!(state, RecoveryState::FailedTracee);
        assert_ne!(state, RecoveryState::Corrupted);
        assert!(machine.marker_retained());
    }

    #[test]
    fn a_partition_recovers_inside_the_budget_and_blocks_past_it() {
        let mut machine = machine();
        let error = disconnected(ConnectionEpoch(3), "peer reset");
        assert_eq!(
            machine.enter(
                CrashWindow::TransientPartition,
                &evidence().with_error(error.clone()).elapsed(20_000)
            ),
            RecoveryState::Recovering
        );
        assert_eq!(
            machine.enter(
                CrashWindow::TransientPartition,
                &evidence().with_error(error).elapsed(30_001)
            ),
            RecoveryState::BlockedRecoverable
        );
        assert!(
            machine.marker_retained(),
            "a prolonged partition holds the marker rather than deleting it"
        );
    }

    #[test]
    fn eio_blocks_unless_loss_was_actually_proven() {
        let eio = FacadeError::protocol(Nfs4Status::IO, OpCode::Commit, 1);
        let mut blocked = machine();
        assert_eq!(
            blocked.enter(CrashWindow::ServerEio, &evidence().with_error(eio.clone())),
            RecoveryState::BlockedRecoverable
        );
        assert_eq!(blocked.status(), Some(Nfs4Status::IO));

        let mut corrupted = machine();
        assert_eq!(
            corrupted.enter(
                CrashWindow::ServerEio,
                &evidence().with_error(eio).proven_loss()
            ),
            RecoveryState::Corrupted
        );
    }

    #[test]
    fn a_stale_handle_is_only_re_resolved_on_proven_identity() {
        let stale = FacadeError::protocol(Nfs4Status::STALE, OpCode::Read, 1);
        assert_eq!(stale.class(), ErrorClass::SafeStop);
        let mut proven = machine();
        assert_eq!(
            proven.enter(
                CrashWindow::StaleFilehandle,
                &evidence().with_error(stale.clone()).identity(true)
            ),
            RecoveryState::Running
        );
        assert_eq!(
            proven.status(),
            Some(Nfs4Status::STALE),
            "recovering does not erase the error that caused it"
        );

        let mut unproven = machine();
        assert_eq!(
            unproven.enter(
                CrashWindow::StaleFilehandle,
                &evidence().with_error(stale).identity(false)
            ),
            RecoveryState::BlockedRecoverable
        );
    }

    #[test]
    fn split_brain_stops_and_records_the_m3_deferral() {
        for window in [CrashWindow::SplitBrain, CrashWindow::HardPartition] {
            let mut machine = machine();
            let state = machine.enter(
                window,
                &evidence().with_admission(AdmissionStanding::Held(LeaseEpoch(4))),
            );
            assert_eq!(state, RecoveryState::BlockedRecoverable);
            assert_eq!(machine.deferral(), Some(Deferral::M3Fencing));
            assert!(machine
                .deferral()
                .expect("deferral")
                .note()
                .contains("fence"));
        }
    }

    #[test]
    fn giving_up_consumes_the_machine_and_keeps_the_marker() {
        let stopped = machine().give_up_unproven_authority(AuthorityError::TakeoverRefused);
        assert_eq!(stopped.state(), RecoveryState::BlockedRecoverable);
        assert!(stopped.marker_retained());
        assert!(stopped
            .diagnostic()
            .contains("authority could not be proven"));
        assert!(matches!(
            stopped.retained().expect("latched").error(),
            FacadeError::Authority(AuthorityError::TakeoverRefused)
        ));
    }

    #[test]
    fn every_window_is_answered() {
        assert_eq!(CrashWindow::ALL.len(), 14);
        for window in CrashWindow::ALL {
            let mut machine = machine();
            let state = machine.enter(window, &evidence());
            // Conservative evidence must never leave a window Running.
            assert_ne!(
                state,
                RecoveryState::Running,
                "{window:?} ran on no evidence"
            );
        }
    }
}
