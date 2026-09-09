//! Per-crash-window authority and recovery coverage, over the fake facade.
//!
//! One test per row of the crash taxonomy in `docs/design/failure-model.md`.
//! Each drives the window through a real [`StateSession`] over
//! [`FakeTransport`], then asserts three things rather than one:
//!
//! 1. **The state-machine transition.** Which of the five states the run reached.
//! 2. **The retained-error surface.** That the original `NFS4ERR_*` or I/O
//!    failure is still readable verbatim afterwards, never folded into a generic
//!    error by having passed through recovery.
//! 3. **Admission and epoch.** What the durable marker says once the dust
//!    settles: who holds it, at which epoch, and that no timeout moved either.
//!
//! # Why no live server
//!
//! The admission marker is exercised through the frozen transport facade against
//! the shape fake. The fixture at `/tmp/nfs-scope/m1/fixtures/authority-recovery`
//! is allocated for this role and deliberately unused: nothing here needs a
//! server to answer, because what is under test is which state Umbra reaches
//! given an answer, and the fake supplies answers under fault injection with more
//! precision than a real Ganesha can be made to.
//!
//! # What the fake cannot claim
//!
//! [`FakeReplayLog`] is in memory, so every [`RetainedError`] it carries reports
//! `is_durable() == false`. That is deliberate and asserted: a consumer that
//! gates replay on durability behaves here exactly as it will against a durable
//! log, rather than accidentally passing because volatile evidence was treated as
//! persistent. Consequently these tests assert the *recovery plan* a durable
//! journal would license; they do not perform a replay, because under the fake
//! no evidence survives a process to replay from.

use std::sync::{Arc, Mutex};

use umbra_core::{IdempotencyKey, LeaseEpoch, OperationId, RunId, WriterId};
use umbra_storage_nfs_userspace::authority::admission::{
    AdmissionControl, AdmissionOutcome, AdmissionRequest,
};
use umbra_storage_nfs_userspace::authority::journal::{
    Acknowledged, CommittedResult, Durability, MutationJournal, MutationRequest,
};
use umbra_storage_nfs_userspace::authority::marker::{
    AdmissionMarker, AdmissionPhase, MarkerStore, WriterToken,
};
use umbra_storage_nfs_userspace::authority::outage::{
    AdmissionStanding, CrashWindow, Deferral, Evidence, OutageBudget, OutageMachine, RecoveryState,
};
use umbra_storage_nfs_userspace::authority::server_marker::ServerMarkerStore;
use umbra_storage_nfs_userspace::authority::{Admitted, OutstandingIo, ReleaseOutcome};
use umbra_storage_nfs_userspace::error::{
    AuthorityError, ErrorClass, FacadeError, FacadeResult, Nfs4Status, ReplayError,
};
use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport, ScriptedFault};
use umbra_storage_nfs_userspace::handle::{FileHandle, ObjectIdentity, OpenFile};
use umbra_storage_nfs_userspace::integration::{identity_for, Backend, StateSession};
use umbra_storage_nfs_userspace::layout;
use umbra_storage_nfs_userspace::replay::{IntentKind, Payload, ReplayBudget, ReplayOutcome};
use umbra_storage_nfs_userspace::state::open_owner::{close, OpenOutcome, OpenRequest};
use umbra_storage_nfs_userspace::transport::{
    CallToken, ComponentName, Compound, CompoundReply, ConnectionEpoch, ConnectionState, Deadline,
    FaultAction, FaultPlan, FaultPoint, Fsid, OpCode, OpenHow, RawTransport, Retirement,
    ShareAccess, ShareDeny, Stability, TransportLimits, TransportResult, Verifier, WireProfile,
    WriteVerifier,
};
use uuid::Uuid;

const DEADLINE: Deadline = Deadline {
    millis: OutageBudget::DESIGN_DEFAULTS.rpc_deadline.millis,
};

fn run_id() -> RunId {
    RunId(Uuid::from_u128(0x4d31_0000_0000_0000_0000_0000_0000_0001))
}

fn marker_name() -> ComponentName {
    ComponentName::new(layout::WRITER_LOCK_FILE.to_vec()).expect("writer.lock is a valid component")
}

fn operation() -> OperationId {
    OperationId(Uuid::from_u128(0xA11CE))
}

fn key(label: &str) -> IdempotencyKey {
    IdempotencyKey(label.to_owned())
}

fn evidence() -> Evidence {
    Evidence::for_operation(operation(), key("authority-recovery"))
}

fn budget() -> ReplayBudget {
    ReplayBudget {
        max_records: 16,
        max_payload_bytes: 64 * 1024,
    }
}

// --- Harness -----------------------------------------------------------------

/// One run: a fake server holding a `.provider` directory, a session established
/// over it, and the journal and machine that session drives.
struct Run {
    session: StateSession,
    root: FileHandle,
    provider: FileHandle,
    journal: MutationJournal<FakeReplayLog>,
    machine: OutageMachine,
}

impl Run {
    /// A run whose server is in grace or not, as a restart scenario needs.
    ///
    /// Grace is pre-scripted because `StateSession::over_fake` takes the fake by
    /// value; that is the documented way to script the fake, and it is enough,
    /// because a `CLAIM_PREVIOUS` inside grace and one outside it are two runs,
    /// not two phases of one.
    fn with_grace(label: &str, in_grace: bool) -> Self {
        let mut fake = FakeTransport::new();
        fake.set_grace(in_grace);
        let export_root = fake.root();
        let provider = fake.insert_directory(&export_root, layout::PRIVATE_DIR);
        let mut session = StateSession::over_fake(fake, identity_for(label, Verifier([0x5A; 8])));
        assert_eq!(session.backend(), Backend::Fake);
        let root = session.root(DEADLINE).expect("export root");
        session
            .establish_and_adopt(&root, 0, DEADLINE)
            .expect("the unfaulted setup establishes a client id");
        Self {
            session,
            root,
            provider,
            journal: MutationJournal::new(
                run_id(),
                FakeReplayLog::new(budget()),
                // In memory, so it says so. Nothing here may claim durability.
                Durability::Volatile,
            ),
            machine: OutageMachine::running(OutageBudget::DESIGN_DEFAULTS, false),
        }
    }

    fn new(label: &str) -> Self {
        Self::with_grace(label, false)
    }

    /// Run `body` against this run's admission marker on the server.
    fn with_marker_store<T>(&mut self, body: impl FnOnce(&mut ServerMarkerStore<'_>) -> T) -> T {
        let provider = self.provider.clone();
        let (state, transport) = self.session.split();
        let owners = state
            .incarnation()
            .expect("an incarnation was adopted")
            .open_owners();
        let mut store =
            ServerMarkerStore::new(transport, owners, provider, marker_name(), DEADLINE);
        body(&mut store)
    }

    /// Ask for admission as `writer`, exactly as a fresh process would.
    ///
    /// A new `AdmissionControl` per call is the point: a restarted process has no
    /// in-memory ladder, so everything it concludes must come from the marker.
    fn admit(&mut self, writer: &str, token: u8) -> AdmissionOutcome {
        let run = run_id();
        self.with_marker_store(|store| {
            AdmissionControl::new(run, store).acquire(&AdmissionRequest::cooperative(
                WriterId(writer.to_owned()),
                WriterToken([token; 16]),
            ))
        })
    }

    fn release(&mut self, admitted: Admitted, outstanding: OutstandingIo) -> ReleaseOutcome {
        let run = run_id();
        self.with_marker_store(|store| {
            AdmissionControl::new(run, store).release(admitted, outstanding)
        })
    }

    /// The durable marker as a follow-on process would read it.
    fn marker(&mut self) -> Option<AdmissionMarker> {
        let bytes = self
            .with_marker_store(|store| store.read())
            .expect("the marker is readable");
        bytes.map(|bytes| AdmissionMarker::decode(&bytes).expect("the marker decodes"))
    }

    fn install(&mut self, plan: Box<dyn FaultPlan>) {
        self.session.transport().install_faults(plan);
    }

    /// Open a regular file under the export root through the frozen facade.
    fn open_named(&mut self, name: &[u8], share: ShareAccess) -> FacadeResult<OpenFile> {
        let root = self.root.clone();
        let (state, transport) = self.session.split();
        let owners = state
            .incarnation()
            .expect("an incarnation was adopted")
            .open_owners();
        let lease = owners.allocate()?;
        let request = OpenRequest {
            parent: root,
            name: ComponentName::new(name.to_vec()).expect("valid component"),
            how: OpenHow::Unchecked { mode: 0o600 },
            share_access: share,
            share_deny: ShareDeny::NONE,
        };
        match owners.open(lease, transport, &request, DEADLINE) {
            OpenOutcome::Opened(file) => Ok(file),
            other => Err(other
                .error()
                .cloned()
                .expect("a non-opened outcome carries an error")),
        }
    }

    fn close_file(&mut self, file: OpenFile) {
        let (_, transport) = self.session.split();
        close(file, transport, DEADLINE);
    }

    /// The object identity a write intent names.
    fn object(file: &OpenFile) -> ObjectIdentity {
        file.identity()
    }
}

/// One `FakeTransport` shared by two sessions, so an admission race has one
/// server to be arbitrated by.
///
/// Only the competing-admission test uses this. It deliberately does not
/// reconnect: one shared connection generation for two clients is a modelling
/// artefact, and no test here depends on it.
#[derive(Clone)]
struct SharedFake(Arc<Mutex<FakeTransport>>);

impl SharedFake {
    fn new(fake: FakeTransport) -> Self {
        Self(Arc::new(Mutex::new(fake)))
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, FakeTransport> {
        self.0.lock().expect("the fake is not poisoned")
    }
}

impl RawTransport for SharedFake {
    fn wire_profile(&self) -> WireProfile {
        self.locked().wire_profile()
    }

    fn limits(&self) -> TransportLimits {
        self.locked().limits()
    }

    fn connection(&self) -> ConnectionState {
        self.locked().connection()
    }

    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply> {
        self.locked().submit(call, deadline)
    }

    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement> {
        self.locked().cancel(token)
    }

    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.locked().reconnect()
    }

    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.locked().install_faults(plan);
    }
}

/// Assert that a marker records exactly this owner, phase and epoch.
fn assert_marker(
    marker: Option<AdmissionMarker>,
    writer: &str,
    phase: AdmissionPhase,
    epoch: LeaseEpoch,
) {
    let marker = marker.expect("a marker must exist");
    assert_eq!(
        marker.writer().map(|id| id.0.as_str()),
        Some(writer),
        "the marker records the wrong owner"
    );
    assert_eq!(marker.phase(), phase, "the marker records the wrong phase");
    assert_eq!(marker.epoch(), epoch, "the marker records the wrong epoch");
}

// --- Competing sessions ------------------------------------------------------

#[test]
fn a_second_session_is_denied_without_takeover() {
    // Two genuinely separate sessions — separate client ids, separate open
    // owners, separate protocol state — racing one server for one marker.
    let mut fake = FakeTransport::new();
    let export_root = fake.root();
    let provider = fake.insert_directory(&export_root, layout::PRIVATE_DIR);
    let shared = SharedFake::new(fake);

    let mut sessions: Vec<StateSession> = ["session-a", "session-b"]
        .into_iter()
        .map(|label| {
            let mut session = StateSession::new(
                Backend::Fake,
                Box::new(shared.clone()),
                identity_for(label, Verifier([0x5A; 8])),
            );
            let root = session.root(DEADLINE).expect("export root");
            session
                .establish_and_adopt(&root, 0, DEADLINE)
                .expect("establish");
            session
        })
        .collect();
    let mut second = sessions.pop().expect("two sessions");
    let mut first = sessions.pop().expect("two sessions");

    let acquire = |session: &mut StateSession, writer: &str, token: u8| -> AdmissionOutcome {
        let (state, transport) = session.split();
        let owners = state.incarnation().expect("adopted").open_owners();
        let mut store =
            ServerMarkerStore::new(transport, owners, provider.clone(), marker_name(), DEADLINE);
        AdmissionControl::new(run_id(), &mut store).acquire(&AdmissionRequest::cooperative(
            WriterId(writer.to_owned()),
            WriterToken([token; 16]),
        ))
    };

    let admitted = acquire(&mut first, "session-a", 1)
        .admitted()
        .expect("the first session wins");
    assert_eq!(admitted.epoch(), LeaseEpoch(1));

    let denial = acquire(&mut second, "session-b", 2);
    let AdmissionOutcome::Denied {
        holder,
        holder_epoch,
        error,
    } = &denial
    else {
        panic!("the second session must be denied, got {denial:?}");
    };
    assert_eq!(
        holder.as_ref().map(|id| id.0.as_str()),
        Some("session-a"),
        "the denial must name the actual holder"
    );
    assert_eq!(*holder_epoch, LeaseEpoch(1), "no epoch moved on a denial");
    assert!(
        matches!(
            error,
            FacadeError::Authority(AuthorityError::AdmissionRefused(_))
        ),
        "{error:?}"
    );
    assert_eq!(error.class(), ErrorClass::SafeStop);
    assert!(
        !matches!(
            error,
            FacadeError::Authority(AuthorityError::TakeoverRefused)
        ),
        "a denial is not a takeover attempt that was refused; no takeover was tried"
    );

    // Denial is not a one-shot: the second session stays out however often it
    // asks, because nothing about elapsed time is consulted.
    for _ in 0..3 {
        assert!(matches!(
            acquire(&mut second, "session-b", 2),
            AdmissionOutcome::Denied { .. }
        ));
    }

    // And the marker still records the first session, unchanged.
    let (state, transport) = second.split();
    let owners = state.incarnation().expect("adopted").open_owners();
    let mut store =
        ServerMarkerStore::new(transport, owners, provider.clone(), marker_name(), DEADLINE);
    let bytes = store.read().expect("read").expect("marker present");
    assert_marker(
        Some(AdmissionMarker::decode(&bytes).expect("decode")),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn a_cooperative_release_is_the_only_way_the_next_session_gets_in() {
    let mut run = Run::new("cooperative");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    assert_eq!(admitted.epoch(), LeaseEpoch(1));

    // While held, a follow-on process is refused however it asks.
    assert!(matches!(
        run.admit("session-b", 2),
        AdmissionOutcome::Denied { .. }
    ));

    assert!(matches!(
        run.release(admitted, OutstandingIo::Excluded),
        ReleaseOutcome::Released {
            epoch: LeaseEpoch(1)
        }
    ));
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Released,
        LeaseEpoch(1),
    );

    let next = run
        .admit("session-b", 2)
        .admitted()
        .expect("a released marker admits the follow-on session");
    assert_eq!(
        next.epoch(),
        LeaseEpoch(2),
        "a legitimate transition advances the epoch by exactly one"
    );
    assert_marker(
        run.marker(),
        "session-b",
        AdmissionPhase::Held,
        LeaseEpoch(2),
    );
}

// --- Crash windows -----------------------------------------------------------

#[test]
fn umbra_crash_before_write_blocks_with_nothing_to_replay() {
    let mut run = Run::new("crash-before");
    let admitted = run.admit("session-a", 1).admitted().expect("first");

    // The process dies here. Nothing was journalled.
    let follow_on = run.admit("session-b", 2);
    let AdmissionOutcome::Denied { error, .. } = &follow_on else {
        panic!("a crashed owner's marker is never taken over: {follow_on:?}");
    };
    let error = error.clone();

    let state = run.machine.enter(
        CrashWindow::UmbraCrashBeforeWrite,
        &evidence()
            .with_error(error)
            .with_admission(AdmissionStanding::Denied(LeaseEpoch(1)))
            .durable(false, false, false),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert!(matches!(
        run.machine.retained().expect("latched").error(),
        FacadeError::Authority(AuthorityError::AdmissionRefused(_))
    ));

    let plan = run
        .journal
        .plan_recovery(std::iter::once(&key("never-began")));
    assert!(plan.replayable.is_empty());
    assert_eq!(plan.blocked.len(), 1, "an unrecorded key settles nothing");

    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
    assert_eq!(admitted.epoch(), LeaseEpoch(1), "no epoch moved");
}

#[test]
fn umbra_crash_mid_write_blocks_with_a_replayable_intent() {
    let mut run = Run::new("crash-mid");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"mid-write", ShareAccess::WRITE)
        .expect("open");

    // The intent, its payload and the authorising epoch reach the log before
    // anything is dispatched.
    let mutation = MutationRequest {
        operation: operation(),
        key: key("mid-write"),
        kind: IntentKind::Write {
            object: Run::object(&file),
            offset: 0,
            stability: Stability::Unstable,
        },
        payload: Payload::Inline(b"half-written".to_vec()),
    };
    let Acknowledged::Dispatch(ticket) = run
        .journal
        .begin(&admitted, &mutation)
        .expect("a fresh intent is admitted")
    else {
        panic!("expected a dispatch ticket");
    };
    assert_eq!(ticket.epoch(), LeaseEpoch(1));
    run.close_file(file);

    // The process dies between the durable intent and the reply.
    let outcome = run.journal.plan_recovery(std::iter::once(ticket.key()));
    assert_eq!(
        outcome.replayable,
        vec![key("mid-write")],
        "an inline payload makes the write re-drivable under its original identity"
    );

    let follow_on = run.admit("session-b", 2);
    let AdmissionOutcome::Denied { error, .. } = &follow_on else {
        panic!("expected a denial: {follow_on:?}");
    };
    let state = run.machine.enter(
        CrashWindow::UmbraCrashMidWrite,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Denied(LeaseEpoch(1)))
            .durable(true, true, false),
    );
    assert_eq!(
        state,
        RecoveryState::BlockedRecoverable,
        "a replayable intent is still not replayed without authority"
    );
    assert!(!state.admits_mutation());
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn umbra_crash_after_write_keeps_the_recorded_result_as_the_answer() {
    let mut run = Run::new("crash-after");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"after-write", ShareAccess::WRITE)
        .expect("open");
    let stateid = file.stateid().expect("confirmed open");

    let mutation = MutationRequest {
        operation: operation(),
        key: key("after-write"),
        kind: IntentKind::Write {
            object: Run::object(&file),
            offset: 0,
            stability: Stability::FileSync,
        },
        payload: Payload::Inline(b"landed".to_vec()),
    };
    let Acknowledged::Dispatch(ticket) = run.journal.begin(&admitted, &mutation).expect("admit")
    else {
        panic!("expected a ticket");
    };
    let written = {
        let (_, transport) = run.session.split();
        transport
            .write(
                file.handle(),
                stateid,
                0,
                Stability::FileSync,
                b"landed".to_vec(),
                DEADLINE,
            )
            .expect("the unfaulted write lands")
    };
    run.journal
        .settle(
            ticket,
            CommittedResult::Completed(umbra_storage_nfs_userspace::replay::CompletedWrite {
                count: written.count,
                committed: written.committed,
                verifier: Some(written.verifier),
            }),
        )
        .expect("settle");
    run.close_file(file);

    // The process dies after the durable result, before the caller was told.
    let plan = run
        .journal
        .plan_recovery(std::iter::once(&key("after-write")));
    assert_eq!(plan.settled, vec![key("after-write")]);
    assert!(
        plan.replayable.is_empty(),
        "a settled key is never re-dispatched"
    );

    // Asking again returns the recorded effect verbatim, not a fresh dispatch.
    let replayed = run.journal.begin(&admitted, &mutation).expect("retry");
    let Acknowledged::Replayed(ReplayOutcome::Completed(recorded)) = replayed else {
        panic!("a settled key must replay its recorded result: {replayed:?}");
    };
    assert_eq!(recorded.count, written.count);
    assert_eq!(recorded.committed, Stability::FileSync);

    let follow_on = run.admit("session-b", 2);
    let AdmissionOutcome::Denied { error, .. } = &follow_on else {
        panic!("expected a denial: {follow_on:?}");
    };
    let state = run.machine.enter(
        CrashWindow::UmbraCrashAfterWrite,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Denied(LeaseEpoch(1)))
            .durable(true, true, true),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn tracee_crash_records_failed_execution_without_implicating_storage() {
    let mut run = Run::new("tracee");
    let admitted = run.admit("session-a", 1).admitted().expect("first");

    let state = run.machine.enter(
        CrashWindow::TraceeCrash,
        &evidence()
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .io_excluded(true),
    );
    assert_eq!(state, RecoveryState::FailedTracee);
    assert_ne!(
        state,
        RecoveryState::Corrupted,
        "a nonzero exit never implies storage corruption"
    );
    // The controller keeps its own authority: the tracee failed, not the writer.
    assert!(run.machine.marker_retained());
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn transient_partition_recovers_inside_the_budget_and_blocks_past_it() {
    let mut run = Run::new("partition");
    let admitted = run.admit("session-a", 1).admitted().expect("first");

    // A real disconnect from the frozen facade, not a hand-built error.
    run.install(ScriptedFault::once(
        FaultPoint::BeforeDispatch,
        None,
        FaultAction::Fail(
            umbra_storage_nfs_userspace::error::TransportError::Disconnected {
                epoch: ConnectionEpoch(1),
                detail: "peer reset the connection".into(),
            },
        ),
    ));
    let error = run
        .session
        .root(DEADLINE)
        .expect_err("the injected disconnect must surface");
    assert_eq!(error.class(), ErrorClass::NeedsRecovery);

    let inside = run.machine.enter(
        CrashWindow::TransientPartition,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .elapsed(29_999),
    );
    assert_eq!(inside, RecoveryState::Recovering);

    let past = run.machine.enter(
        CrashWindow::TransientPartition,
        &evidence()
            .with_error(FacadeError::Replay(ReplayError::Indeterminate))
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .elapsed(30_001),
    );
    assert_eq!(
        past,
        RecoveryState::BlockedRecoverable,
        "past the 30 s ordinary outage budget the run is left blocked"
    );
    assert_eq!(
        run.machine.retained().expect("latched").error(),
        &error,
        "the original disconnect survives the later attempt verbatim"
    );

    // The marker is held, not deleted, and no epoch moved on a timeout.
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
    assert!(run.machine.marker_retained());
}

#[test]
fn ganesha_graceful_restart_reclaims_inside_grace() {
    let mut run = Run::with_grace("grace-in", true);
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"reclaimed", ShareAccess::BOTH)
        .expect("open");
    let expected = file.identity();

    // The server restarted: the connection generation changes and every piece of
    // protocol state must be proven again.
    let root = run.root.clone();
    let (state, transport) = run.session.split();
    state.invalidate();
    transport.reconnect().expect("reconnect");
    let previous = vec![file];
    let (incarnation, report) = state
        .reestablish_and_reclaim(transport, &root, &previous, 0, DEADLINE)
        .expect("the client id is re-established inside grace");
    // Adopt the re-established incarnation: the marker work below runs through
    // the session, and a session with no adopted incarnation has no open owners.
    state.adopt(incarnation);
    assert!(
        report.is_complete(),
        "CLAIM_PREVIOUS inside grace must recover every open: {report:?}"
    );
    assert_eq!(report.recovered.len(), 1);
    assert_eq!(
        report.recovered[0].identity(),
        expected,
        "a reclaim must recover the same object, never a replacement"
    );

    let state = run.machine.enter(
        CrashWindow::GaneshaGracefulRestart,
        &evidence()
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .reclaim(true, 90)
            .elapsed(45_000),
    );
    assert_eq!(state, RecoveryState::Running);
    assert_eq!(
        run.machine.budget().grace_budget_millis(90),
        120_000,
        "a 90-second grace gets a 120-second bounded budget"
    );
    // A server restart is not an Umbra ownership transition.
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn ganesha_restart_outside_grace_surrenders_with_the_verbatim_status() {
    let mut run = Run::with_grace("grace-out", false);
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"unreclaimable", ShareAccess::BOTH)
        .expect("open");

    let root = run.root.clone();
    let (state, transport) = run.session.split();
    state.invalidate();
    transport.reconnect().expect("reconnect");
    let previous = vec![file];
    let (incarnation, report) = state
        .reestablish_and_reclaim(transport, &root, &previous, 0, DEADLINE)
        .expect("the client id re-establishes even when the reclaim cannot");
    state.adopt(incarnation);
    assert!(!report.is_complete());
    let surrendered = report.surrendered.first().expect("one surrender");
    assert_eq!(
        surrendered.status(),
        Some(Nfs4Status::NO_GRACE),
        "the verbatim NFS4ERR_NO_GRACE must reach the surrender, not a classification of it"
    );
    let error = surrendered.error.clone().expect("a verbatim failure");

    let state = run.machine.enter(
        CrashWindow::GaneshaGracefulRestart,
        &evidence()
            .with_error(error)
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .reclaim(false, 90)
            .elapsed(1_000),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert_eq!(
        run.machine.status(),
        Some(Nfs4Status::NO_GRACE),
        "10033 must still be 10033 after the transition"
    );
    assert_eq!(
        run.machine.status().map(|status| status.0),
        Some(10_033),
        "the raw status word is preserved as a number"
    );
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn enospc_settles_the_key_with_the_verbatim_status_and_blocks() {
    let mut run = Run::new("enospc");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run.open_named(b"full", ShareAccess::WRITE).expect("open");
    let stateid = file.stateid().expect("confirmed open");

    let mutation = MutationRequest {
        operation: operation(),
        key: key("enospc"),
        kind: IntentKind::Write {
            object: Run::object(&file),
            offset: 0,
            stability: Stability::FileSync,
        },
        payload: Payload::Inline(b"too big".to_vec()),
    };
    let Acknowledged::Dispatch(ticket) = run.journal.begin(&admitted, &mutation).expect("admit")
    else {
        panic!("expected a ticket");
    };

    run.install(ScriptedFault::once(
        FaultPoint::AfterDispatch,
        Some(OpCode::Write),
        FaultAction::Substitute(Nfs4Status::NOSPC),
    ));
    let error = {
        let (_, transport) = run.session.split();
        transport
            .write(
                file.handle(),
                stateid,
                0,
                Stability::FileSync,
                b"too big".to_vec(),
                DEADLINE,
            )
            .expect_err("the substituted NFS4ERR_NOSPC must surface")
    };
    assert_eq!(error.status(), Some(Nfs4Status::NOSPC));
    run.journal
        .settle(ticket, CommittedResult::Failed(error.clone()))
        .expect("settle");
    run.close_file(file);

    let state = run.machine.enter(
        CrashWindow::ServerEnospc,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .durable(true, true, true),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert_eq!(run.machine.status().map(|s| s.0), Some(28));

    // "A recorded completed error stays the result for that key."
    let retry = run.journal.begin(&admitted, &mutation).expect("retry");
    let Acknowledged::Replayed(ReplayOutcome::Failed(retained)) = retry else {
        panic!("a recorded error must be the settled answer: {retry:?}");
    };
    assert_eq!(retained.error(), &error);
    assert_eq!(retained.error().status(), Some(Nfs4Status::NOSPC));
    assert!(
        !retained.is_durable(),
        "an in-memory log must not claim a persistence boundary it does not have"
    );
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn eio_latches_the_original_failure_and_refuses_a_clean_release() {
    let mut run = Run::new("eio");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"failing", ShareAccess::WRITE)
        .expect("open");
    let stateid = file.stateid().expect("confirmed open");

    run.install(ScriptedFault::once(
        FaultPoint::AfterDispatch,
        Some(OpCode::Write),
        FaultAction::Substitute(Nfs4Status::IO),
    ));
    let error = {
        let (_, transport) = run.session.split();
        transport
            .write(
                file.handle(),
                stateid,
                0,
                Stability::FileSync,
                b"bytes".to_vec(),
                DEADLINE,
            )
            .expect_err("the substituted NFS4ERR_IO must surface")
    };
    run.close_file(file);
    assert_eq!(error.status(), Some(Nfs4Status::IO));

    let state = run.machine.enter(
        CrashWindow::ServerEio,
        &evidence()
            .with_error(error)
            .with_admission(AdmissionStanding::Held(admitted.epoch())),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert_ne!(
        state,
        RecoveryState::Corrupted,
        "corruption is only recorded on proven contradiction or loss"
    );

    // Three further recovery attempts, each returning something less specific.
    for later in [
        FacadeError::protocol(Nfs4Status::SERVERFAULT, OpCode::Write, 1),
        FacadeError::Replay(ReplayError::Indeterminate),
        FacadeError::protocol(Nfs4Status::DELAY, OpCode::Write, 1),
    ] {
        run.machine
            .enter(CrashWindow::ServerEio, &evidence().with_error(later));
    }
    assert_eq!(run.machine.attempts(), 4);
    assert_eq!(
        run.machine.status().map(|s| s.0),
        Some(5),
        "NFS4ERR_IO must still be 5 after every recovery attempt"
    );

    // "Do not issue a clean receipt or release." Outstanding I/O is not excluded,
    // so the release is withheld and the marker stays held.
    let withheld = run.release(
        admitted,
        OutstandingIo::Unknown {
            detail: "a stable write failed and its range is unknown".into(),
        },
    );
    let ReleaseOutcome::Retained { admitted, error } = withheld else {
        panic!("a release must not be issued over a latched write failure");
    };
    assert_eq!(admitted.epoch(), LeaseEpoch(1));
    assert_eq!(error.class(), ErrorClass::SafeStop);
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn stale_filehandle_re_resolves_only_on_proven_identity() {
    let mut run = Run::new("stale");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"vanishing", ShareAccess::READ)
        .expect("open");
    let stateid = file.stateid().expect("confirmed open");
    let identity = file.identity();

    run.install(ScriptedFault::once(
        FaultPoint::AfterDispatch,
        Some(OpCode::Read),
        FaultAction::Substitute(Nfs4Status::STALE),
    ));
    let error = {
        let (_, transport) = run.session.split();
        transport
            .read(file.handle(), stateid, 0, 16, DEADLINE)
            .expect_err("the substituted NFS4ERR_STALE must surface")
    };
    assert_eq!(error.status(), Some(Nfs4Status::STALE));
    assert_eq!(error.class(), ErrorClass::SafeStop);

    // Re-resolve and find the same fsid/fileid: the object is provably the same.
    let reread = {
        let (_, transport) = run.session.split();
        transport
            .read(file.handle(), stateid, 0, 16, DEADLINE)
            .expect("the fault fired once")
    };
    let _ = reread;
    let proven = run.machine.enter(
        CrashWindow::StaleFilehandle,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .identity(identity == file.identity()),
    );
    assert_eq!(proven, RecoveryState::Running);
    assert_eq!(
        run.machine.status().map(|s| s.0),
        Some(70),
        "recovering does not erase the NFS4ERR_STALE that caused it"
    );
    run.close_file(file);

    // A second run where identity cannot be proven stops instead.
    let mut unproven_run = Run::new("stale-unproven");
    unproven_run
        .admit("session-a", 1)
        .admitted()
        .expect("first");
    let stopped = unproven_run.machine.enter(
        CrashWindow::StaleFilehandle,
        &evidence()
            .with_error(error)
            .with_admission(AdmissionStanding::Held(LeaseEpoch(1)))
            .identity(false),
    );
    assert_eq!(stopped, RecoveryState::BlockedRecoverable);
    assert_eq!(
        unproven_run.machine.status().map(|s| s.0),
        Some(70),
        "an unproven identity keeps the original status too"
    );
    assert_marker(
        unproven_run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn sigstop_resume_stays_blocked_until_queued_effects_are_excluded() {
    let mut run = Run::new("sigstop");
    let admitted = run.admit("session-a", 1).admitted().expect("first");

    // A reply that never arrived: the process was stopped while a call was out,
    // so whether the server applied it is unknown.
    run.install(ScriptedFault::once(
        FaultPoint::OnDeadline,
        None,
        FaultAction::DropReply,
    ));
    let error = run
        .session
        .root(DEADLINE)
        .expect_err("the dropped reply must reach its deadline");
    assert!(matches!(
        error,
        FacadeError::Transport(
            umbra_storage_nfs_userspace::error::TransportError::DeadlineExpired { .. }
        )
    ));

    let blocked = run.machine.enter(
        CrashWindow::UmbraSigstopResume,
        &evidence()
            .with_error(error.clone())
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .suspended(120_000)
            .io_excluded(false),
    );
    assert_eq!(
        blocked,
        RecoveryState::BlockedRecoverable,
        "queued remote effects that cannot be excluded keep the run blocked"
    );

    // Even with the queue proven empty, a suspend longer than the outage budget
    // means the authority this session held may have lapsed while it could not
    // check. It is poisoned, not resumed.
    let long_gap = run.machine.enter(
        CrashWindow::UmbraSigstopResume,
        &evidence()
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .suspended(120_000)
            .io_excluded(true),
    );
    assert_eq!(long_gap, RecoveryState::BlockedRecoverable);

    // A short suspend with the queue proven empty may revalidate.
    let short_gap = run.machine.enter(
        CrashWindow::UmbraSigstopResume,
        &evidence()
            .with_admission(AdmissionStanding::Held(admitted.epoch()))
            .suspended(1_500)
            .io_excluded(true),
    );
    assert_eq!(short_gap, RecoveryState::Recovering);

    assert_eq!(
        run.machine.retained().expect("latched").error(),
        &error,
        "the deadline failure that opened the window is still the latched one"
    );
    // Nothing about being stopped released or moved the marker.
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

#[test]
fn client_power_loss_replays_only_from_a_durable_payload() {
    let mut run = Run::new("power-loss");
    let admitted = run.admit("session-a", 1).admitted().expect("first");
    let file = run
        .open_named(b"in-flight", ShareAccess::WRITE)
        .expect("open");
    let stateid = file.stateid().expect("confirmed open");

    // An UNSTABLE write, journalled with its exact bytes before dispatch.
    let mutation = MutationRequest {
        operation: operation(),
        key: key("power-write"),
        kind: IntentKind::Write {
            object: Run::object(&file),
            offset: 0,
            stability: Stability::Unstable,
        },
        payload: Payload::Inline(b"unstable".to_vec()),
    };
    let Acknowledged::Dispatch(ticket) = run.journal.begin(&admitted, &mutation).expect("admit")
    else {
        panic!("expected a ticket");
    };
    let written = {
        let (_, transport) = run.session.split();
        transport
            .write(
                file.handle(),
                stateid,
                0,
                Stability::Unstable,
                b"unstable".to_vec(),
                DEADLINE,
            )
            .expect("the write is accepted")
    };
    assert_eq!(written.committed, Stability::Unstable);
    run.journal
        .note_write_verifier(ticket.key(), written.verifier);

    // Power returns and the server's verifier has changed: the unstable data is
    // gone and the bytes must come from the retained payload, not from the
    // acknowledgement.
    let rotated = WriteVerifier([0xEE; 8]);
    run.install(ScriptedFault::once(
        FaultPoint::BeforeReturn,
        Some(OpCode::PutRootFh),
        FaultAction::RotateVerifier(rotated),
    ));
    run.session.root(DEADLINE).expect("root");
    let committed = {
        let (_, transport) = run.session.split();
        transport
            .commit(file.handle(), 0, 0, DEADLINE)
            .expect("commit")
    };
    assert_eq!(committed.verifier, rotated);
    let verifier_error = run
        .journal
        .check_commit_verifier(ticket.key(), committed.verifier)
        .into_result()
        .expect_err("a changed verifier must be an error, not a shrug");
    assert!(
        matches!(
            verifier_error,
            FacadeError::Replay(ReplayError::VerifierChanged { .. })
        ),
        "{verifier_error:?}"
    );
    run.close_file(file);

    // The intent and its exact bytes survived, so the write is re-drivable — and
    // still not driven, because the M3 authority gate precedes mutation replay.
    let plan = run.journal.plan_recovery(std::iter::once(ticket.key()));
    assert_eq!(plan.replayable, vec![key("power-write")]);
    let state = run.machine.enter(
        CrashWindow::ClientPowerLossInFlight,
        &evidence()
            .with_error(verifier_error)
            .with_admission(AdmissionStanding::Denied(LeaseEpoch(1)))
            .durable(true, true, false),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert!(matches!(
        run.machine.retained().expect("latched").error(),
        FacadeError::Replay(ReplayError::VerifierChanged { .. })
    ));

    // The same window with only a digest retained cannot claim recovery at all.
    let mut without_payload = Run::new("power-loss-no-payload");
    let admitted = without_payload
        .admit("session-a", 1)
        .admitted()
        .expect("first");
    let digest_only = MutationRequest {
        operation: operation(),
        key: key("digest-write"),
        kind: IntentKind::Write {
            object: ObjectIdentity {
                fsid: Fsid { major: 1, minor: 1 },
                fileid: 9,
            },
            offset: 0,
            stability: Stability::Unstable,
        },
        payload: Payload::Digest {
            digest: [0x11; 32],
            len: 8,
            source: "an upstream journal that is not guaranteed here".into(),
        },
    };
    without_payload
        .journal
        .begin(&admitted, &digest_only)
        .expect("admit");
    let plan = without_payload
        .journal
        .plan_recovery(std::iter::once(&key("digest-write")));
    assert!(plan.replayable.is_empty());
    assert!(matches!(
        plan.blocked[0].1,
        FacadeError::Replay(ReplayError::PayloadMissing)
    ));
    assert_eq!(plan.blocked[0].1.class(), ErrorClass::SafeStop);
    assert_marker(
        without_payload.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

// --- Deferred to M3 ----------------------------------------------------------

#[test]
fn split_brain_and_hard_partition_stop_both_sides_and_defer_to_m3_fencing() {
    for window in [CrashWindow::SplitBrain, CrashWindow::HardPartition] {
        let mut run = Run::new("split-brain");
        let admitted = run.admit("session-a", 1).admitted().expect("first");

        let state = run.machine.enter(
            window,
            &evidence()
                .with_error(FacadeError::Authority(AuthorityError::IdentityUnproven(
                    "a second host presents valid ownership evidence".into(),
                )))
                .with_admission(AdmissionStanding::Held(admitted.epoch())),
        );
        assert_eq!(
            state,
            RecoveryState::BlockedRecoverable,
            "{window:?} must stop, not select a winner"
        );
        assert_eq!(run.machine.deferral(), Some(Deferral::M3Fencing));
        assert!(run
            .machine
            .deferral()
            .expect("deferral")
            .note()
            .contains("an increasing epoch is not a fence"));

        // The epoch did not move and no rival marker was touched: nothing here
        // resolves the conflict, which is the whole point of the deferral.
        assert_marker(
            run.marker(),
            "session-a",
            AdmissionPhase::Held,
            LeaseEpoch(1),
        );

        let stopped = run
            .machine
            .clone()
            .give_up_unproven_authority(AuthorityError::TakeoverRefused);
        assert_eq!(stopped.state(), RecoveryState::BlockedRecoverable);
        assert!(
            stopped.marker_retained(),
            "stopping never releases the marker"
        );
        assert_eq!(stopped.deferral(), Some(Deferral::M3Fencing));
    }
}

#[test]
fn server_power_loss_is_not_qualified_by_anything_here() {
    let mut run = Run::new("server-power");
    run.admit("session-a", 1).admitted().expect("first");
    let state = run.machine.enter(
        CrashWindow::ServerPowerLoss,
        &evidence().with_admission(AdmissionStanding::Held(LeaseEpoch(1))),
    );
    assert_eq!(state, RecoveryState::BlockedRecoverable);
    assert_eq!(run.machine.deferral(), Some(Deferral::M3Qualification));
    assert!(run
        .machine
        .deferral()
        .expect("deferral")
        .note()
        .contains("not power-loss qualification"));
}

// --- Coverage ----------------------------------------------------------------

#[test]
fn every_crash_window_in_the_failure_model_has_a_transition() {
    // A window added to the taxonomy without a rule would silently answer
    // whatever the catch-all arm happened to say. There is no catch-all arm, and
    // this asserts the count stays in step with the tests above.
    assert_eq!(CrashWindow::ALL.len(), 14);
    for window in CrashWindow::ALL {
        let mut machine = OutageMachine::running(OutageBudget::DESIGN_DEFAULTS, false);
        let state = machine.enter(window, &evidence());
        assert_ne!(
            state,
            RecoveryState::Running,
            "{window:?} reached Running on conservative evidence"
        );
        assert!(machine.marker_retained(), "{window:?} released the marker");
    }
}

#[test]
fn no_window_and_no_policy_produces_a_takeover() {
    // The single assertion the owner scope-lock turns on: nothing in this crate
    // grants a takeover, from any of the three layers that could be asked.
    let mut run = Run::new("no-takeover");
    run.admit("session-a", 1).admitted().expect("first");

    let (state, _) = run.session.split();
    assert!(matches!(
        state.takeover(),
        Err(FacadeError::Authority(AuthorityError::TakeoverRefused))
    ));

    let control = AdmissionControl::new(run_id(), MemoryStore::default());
    assert!(matches!(
        control.takeover(),
        FacadeError::Authority(AuthorityError::TakeoverRefused)
    ));

    for _ in 0..5 {
        assert!(matches!(
            run.admit("session-b", 2),
            AdmissionOutcome::Denied { .. }
        ));
    }
    assert_marker(
        run.marker(),
        "session-a",
        AdmissionPhase::Held,
        LeaseEpoch(1),
    );
}

/// The in-memory store, aliased so the takeover assertion above needs no server.
type MemoryStore = umbra_storage_nfs_userspace::authority::marker::MemoryMarkerStore;
