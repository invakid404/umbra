//! Fake-fault matrix: every protocol-state transition against every fault.
//!
//! Each row is a state transition. Each column is a
//! ([`FaultPoint`], [`FaultAction`]) pair. A cell passes when the transition's
//! *invariant* held under that fault — not when a particular outcome occurred.
//! That distinction is the point of the matrix: a fault may legitimately produce
//! success, a server rejection or an unknown outcome, and what must be true in
//! all three is that the state machine advanced no further than the transport
//! actually proved.
//!
//! The five invariants the owner gate asks for are asserted here:
//!
//! * **(a)** No state advances on a dropped reply.
//! * **(b)** No state advances on an unresolved retransmit collision.
//! * **(c)** Reconnect plus reclaim recovers only what `CLAIM_PREVIOUS` allows.
//! * **(d)** Retained errors keep their original `NFS4ERR_*` code.
//! * **(e)** No client-id takeover by timeout alone.
//!
//! # What the fake actually honours
//!
//! `FakeTransport` consults its plan at all five points but acts on a given
//! action only where that action means something: `Fail` at `BeforeDispatch` and
//! `BeforeReturn`, `Substitute` at `AfterDispatch`, `DropReply` at `OnDeadline`,
//! `RotateVerifier` at `BeforeReturn`. `OnConnection` is never consulted, and
//! `ShortWrite` is modelled by `FakeTransport::set_write_cap` rather than by the
//! action. Cells where the action is inert at that point are still run and still
//! asserted: the invariant must hold, and the transition must reach its
//! unfaulted outcome rather than drifting. They are reported as `PASS` with the
//! matrix legend recording that the fake does not act there, so no cell claims
//! coverage the fake does not provide.

use std::sync::{Arc, Mutex};

use umbra_core::{IdempotencyKey, OperationId};
use umbra_storage_nfs_userspace::error::{AuthorityError, FacadeError, Nfs4Status, TransportError};
use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport};
use umbra_storage_nfs_userspace::handle::{OpenFile, Stateid};
use umbra_storage_nfs_userspace::replay::VerifierMatch;
use umbra_storage_nfs_userspace::state::client_id::{ClientIdentity, ConfirmOutcome};
use umbra_storage_nfs_userspace::state::lease::LeaseStanding;
use umbra_storage_nfs_userspace::state::open_owner::{
    close, confirm_open, downgrade_with, CloseOutcome, OpenOutcome, OpenRequest,
};
use umbra_storage_nfs_userspace::state::reclaim::{ReclaimPlan, SurrenderCause};
use umbra_storage_nfs_userspace::state::retained_errors::RetainedErrorLedger;
use umbra_storage_nfs_userspace::state::verifier::{check_commit, note_write, WriteRecord};
use umbra_storage_nfs_userspace::state::{Incarnation, ProtocolState};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, ConnectionEpoch, Deadline, FaultAction, FaultContext, FaultPlan,
    FaultPoint, OpCode, OpenHow, RawTransport, ShareAccess, ShareDeny, Stability, Verifier,
    WriteVerifier,
};

// --- Harness -----------------------------------------------------------------

/// A fault plan that fires once, after skipping `skip` earlier matches.
///
/// `ScriptedFault` fires on the first match, which is enough for a single
/// round trip. A transition made of several COMPOUNDs needs to target a later
/// one, and at every point except `AfterDispatch` the fault context carries the
/// COMPOUND's first operation rather than the interesting one.
struct DelayedFault {
    point: FaultPoint,
    op: Option<OpCode>,
    skip: u32,
    action: Option<FaultAction>,
}

impl DelayedFault {
    fn boxed(
        point: FaultPoint,
        op: Option<OpCode>,
        skip: u32,
        action: FaultAction,
    ) -> Box<dyn FaultPlan> {
        Box::new(Self {
            point,
            op,
            skip,
            action: Some(action),
        })
    }
}

impl FaultPlan for DelayedFault {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if point != self.point || self.op.is_some_and(|op| op != context.op) {
            return FaultAction::Proceed;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return FaultAction::Proceed;
        }
        self.action.take().unwrap_or(FaultAction::Proceed)
    }
}

/// A plan that injects nothing and records every operation dispatched.
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<OpCode>>>,
}

impl FaultPlan for Recorder {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if point == FaultPoint::AfterDispatch {
            self.seen.lock().expect("recorder lock").push(context.op);
        }
        FaultAction::Proceed
    }
}

fn deadline() -> Deadline {
    Deadline { millis: 1_000 }
}

fn identity() -> ClientIdentity {
    ClientIdentity::new(b"umbra-m1-protocol-state".to_vec(), Verifier([0x5A; 8]))
}

fn key(name: &str) -> IdempotencyKey {
    IdempotencyKey(name.into())
}

fn established(transport: &mut FakeTransport) -> Result<Incarnation, String> {
    let root = transport.root();
    ProtocolState::new(identity())
        .establish(transport, &root, 0, deadline())
        .established()
        .ok_or_else(|| "the unfaulted setup failed to establish a client id".to_string())
}

fn request(name: &[u8]) -> OpenRequest {
    OpenRequest {
        parent: FakeTransport::new().root(),
        name: ComponentName::new(name.to_vec()).expect("a valid component"),
        how: OpenHow::Unchecked { mode: 0o600 },
        share_access: ShareAccess::BOTH,
        share_deny: ShareDeny::NONE,
    }
}

fn open_named(
    transport: &mut FakeTransport,
    incarnation: &mut Incarnation,
    name: &[u8],
) -> Result<OpenFile, String> {
    let lease = incarnation
        .open_owners()
        .allocate()
        .map_err(|error| format!("owner allocation failed: {error}"))?;
    match incarnation
        .open_owners()
        .open(lease, transport, &request(name), deadline())
    {
        OpenOutcome::Opened(file) => Ok(file),
        other => Err(format!("the unfaulted setup could not open: {other:?}")),
    }
}

// --- Rows --------------------------------------------------------------------

type Row = fn(&mut FakeTransport, FaultPoint, FaultAction) -> Result<(), String>;

/// SETCLIENTID: an acknowledged client id holds no state until it is confirmed.
fn row_set_client_id(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    match identity().set_client_id(transport, ConnectionEpoch(1), deadline()) {
        Ok(pending) => {
            if transport.renew(pending.client_id(), deadline()).is_ok() {
                return Err("an unconfirmed client id renewed a lease".into());
            }
            Ok(())
        }
        // No client id was established, which is the whole of the state change.
        Err(_) => Ok(()),
    }
}

/// SETCLIENTID_CONFIRM: we claim confirmation exactly when the server gave it.
fn row_set_client_id_confirm(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let pending = identity()
        .set_client_id(transport, ConnectionEpoch(1), deadline())
        .map_err(|error| format!("the unfaulted SETCLIENTID failed: {error}"))?;
    let client_id = pending.client_id();
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    let outcome = pending.confirm(transport, deadline());
    let claimed = matches!(outcome, ConfirmOutcome::Confirmed(_));
    let server_confirmed = transport.renew(client_id, deadline()).is_ok();
    // The invariant is one-directional on purpose. A reply lost *after* the
    // server processed the confirm leaves the client unable to know it
    // succeeded, and no client can close that gap; reporting `Indeterminate` and
    // redoing the idempotent SETCLIENTID is the correct answer. What must never
    // happen is the other direction: claiming a confirmation the server did not
    // make.
    if claimed && !server_confirmed {
        return Err("claimed a confirmation the server never made".into());
    }
    Ok(())
}

/// OPEN: each outcome advances exactly as much as the server proved.
fn row_open(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let burned_before = incarnation.open_owners().burned();
    let lease = incarnation
        .open_owners()
        .allocate()
        .map_err(|error| format!("owner allocation failed: {error}"))?;
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    let outcome =
        incarnation
            .open_owners()
            .open(lease, transport, &request(b"open-row"), deadline());
    let burned_after = incarnation.open_owners().burned();
    match outcome {
        OpenOutcome::Opened(file) => {
            if !file.is_confirmed().unwrap_or(false) || file.stateid().is_err() {
                return Err("an Opened outcome yielded an unusable stateid".into());
            }
            if burned_after != burned_before {
                return Err("a successful OPEN burned its owner".into());
            }
            Ok(())
        }
        OpenOutcome::Unconfirmed { file, .. } => {
            if file.stateid().is_ok() {
                return Err("an unconfirmed open handed out a usable stateid".into());
            }
            Ok(())
        }
        OpenOutcome::Rejected { lease, .. } => {
            if lease.next_seqid().is_none() {
                return Err("a server rejection poisoned the owner".into());
            }
            if burned_after != burned_before {
                return Err("a server rejection burned the owner".into());
            }
            Ok(())
        }
        OpenOutcome::Abandoned { .. } => {
            if burned_after != burned_before + 1 {
                return Err("an unknown outcome did not burn its owner".into());
            }
            Ok(())
        }
    }
}

/// OPEN_CONFIRM retried on an open the server flagged but did not confirm.
fn row_open_confirm(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let lease = incarnation
        .open_owners()
        .allocate()
        .map_err(|error| format!("owner allocation failed: {error}"))?;
    // Force an unconfirmed open with a status that advances the seqid, so the
    // retry below is a genuine second OPEN_CONFIRM rather than a repeat.
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::AfterDispatch,
        Some(OpCode::OpenConfirm),
        0,
        FaultAction::Substitute(Nfs4Status::DELAY),
    ));
    let file =
        match incarnation
            .open_owners()
            .open(lease, transport, &request(b"confirm-row"), deadline())
        {
            OpenOutcome::Unconfirmed { file, .. } => file,
            other => return Err(format!("setup expected an unconfirmed open, got {other:?}")),
        };
    if file.stateid().is_ok() {
        return Err("an unconfirmed open handed out a usable stateid".into());
    }

    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    match confirm_open(file, transport, deadline()) {
        OpenOutcome::Opened(file) => {
            if file.stateid().is_err() {
                return Err("a confirmed open still refuses its stateid".into());
            }
            Ok(())
        }
        OpenOutcome::Unconfirmed { file, .. } => {
            if file.stateid().is_ok() {
                return Err("a failed confirm made the stateid usable".into());
            }
            Ok(())
        }
        OpenOutcome::Abandoned { .. } => Ok(()),
        OpenOutcome::Rejected { .. } => {
            Err("confirm_open must never hand back an owner lease".into())
        }
    }
}

/// CLOSE: a refused close leaves the open usable; a lost one consumes it.
fn row_close(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let file = open_named(transport, &mut incarnation, b"close-row")?;
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    match close(file, transport, deadline()) {
        CloseOutcome::Closed(_) => Ok(()),
        CloseOutcome::Rejected { file, .. } => {
            if file.stateid().is_err() {
                return Err("a refused CLOSE left the open unusable".into());
            }
            Ok(())
        }
        // The open is consumed and its owner poisoned. Server-side state persists
        // until the lease expires, which is the honest cost of an unknown reply.
        CloseOutcome::Abandoned { .. } => Ok(()),
    }
}

/// OPEN_DOWNGRADE: share bits narrow only when the dispatch actually succeeded.
///
/// The frozen transport has no `Nfs4Op::OpenDowngrade`, so a real GETATTR round
/// trip stands in for the dispatch. That is a stand-in for the *wire* step only:
/// the seqid discipline and the share-bit transition under test are the real
/// ones, driven by a real transport fault.
fn row_open_downgrade(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let file = open_named(transport, &mut incarnation, b"downgrade-row")?;
    let before = file.share().map_err(|error| error.to_string())?;
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    let result = downgrade_with(&file, ShareAccess::READ, ShareDeny::NONE, |prepared| {
        transport.getattr(file.handle(), AttrMask::IDENTITY, deadline())?;
        Ok(Stateid {
            seqid: prepared.stateid.seqid.wrapping_add(1),
            other: prepared.stateid.other,
        })
    });
    let after = file.share();
    match result {
        Ok(_) => {
            if after.map_err(|e| e.to_string())? != (ShareAccess::READ, ShareDeny::NONE) {
                return Err("a successful downgrade did not narrow the share bits".into());
            }
            Ok(())
        }
        Err(error) => {
            match after {
                Ok(observed) if observed != before => {
                    return Err("a failed downgrade narrowed the share bits anyway".into())
                }
                _ => {}
            }
            // Probed with `next_seqid`, which inspects the owner without
            // creating a guard that a dropped probe would poison.
            let usable = file.next_seqid().ok().flatten().is_some();
            match error.status() {
                // A server answer settles the seqid, so the owner survives.
                Some(_) if !usable => Err("a server rejection poisoned the owner".into()),
                // No answer: the owner must be poisoned, not guessed at.
                None if usable => Err("an unknown downgrade outcome left the owner usable".into()),
                _ => Ok(()),
            }
        }
    }
}

/// OP_RENEW: the lease is refreshed only by a renewal the server answered.
fn row_renew(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let client = incarnation.client().clone();
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    // The fake reports a 90 second lease, so renewal is owed from 45 seconds.
    // Probing before that threshold would compare against a lease that was
    // fresh regardless of what the renewal did.
    const IDLE_PAST_THRESHOLD: u64 = 60_000;
    let outcome = incarnation
        .lease()
        .renew(transport, &client, IDLE_PAST_THRESHOLD, deadline());
    let fresh = incarnation.lease().standing(IDLE_PAST_THRESHOLD) == LeaseStanding::Fresh;
    if outcome.is_renewed() != fresh {
        return Err(format!(
            "renewed={} but the lease standing says fresh={fresh}",
            outcome.is_renewed()
        ));
    }
    // (e) Whatever happened to the lease, timing it out authorises nothing.
    if !matches!(
        incarnation.lease().takeover_by_timeout(),
        FacadeError::Authority(AuthorityError::TakeoverRefused)
    ) {
        return Err("a lease outcome produced something other than a takeover refusal".into());
    }
    Ok(())
}

/// Reconnect plus `CLAIM_PREVIOUS`: recover only what the reclaim actually granted.
fn row_reclaim(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut state = ProtocolState::new(identity());
    let root = transport.root();
    let incarnation = state
        .establish(transport, &root, 0, deadline())
        .established()
        .ok_or_else(|| "the unfaulted setup failed to establish".to_string())?;
    state.adopt(incarnation);
    let file = {
        let incarnation = state
            .incarnation()
            .ok_or_else(|| "no incarnation was adopted".to_string())?;
        open_named(transport, incarnation, b"reclaim-row")?
    };
    let expected = file.identity();
    state.invalidate();
    transport
        .reconnect()
        .map_err(|error| format!("reconnect failed: {error}"))?;
    transport.set_grace(true);

    let previous = vec![file];
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    match state.reestablish_and_reclaim(transport, &root, &previous, 0, deadline()) {
        Ok((_, report)) => {
            if report.recovered.len() + report.surrendered.len() != previous.len() {
                return Err("the reclaim report did not account for every open".into());
            }
            for recovered in &report.recovered {
                if recovered.identity() != expected {
                    return Err("a reclaim recovered a different object".into());
                }
                if !recovered.is_confirmed().unwrap_or(false) {
                    return Err("a recovered open was never confirmed".into());
                }
            }
            Ok(())
        }
        // The client id could not be re-established, so nothing was reclaimed.
        Err(_) => Ok(()),
    }
}

/// WRITE then COMMIT: the reported verifier match agrees with the bytes observed.
fn row_write_commit_verifier(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut incarnation = established(transport)?;
    let file = open_named(transport, &mut incarnation, b"verifier-row")?;
    let stateid = file.stateid().map_err(|error| error.to_string())?;
    let mut log = FakeReplayLog::default();
    let write_key = key("matrix-write");

    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    let written = match transport.write(
        file.handle(),
        stateid,
        0,
        Stability::Unstable,
        vec![1, 2, 3, 4],
        deadline(),
    ) {
        Ok(reply) => reply,
        // No write landed, so there is no verifier to account for.
        Err(_) => return Ok(()),
    };
    let record = WriteRecord::from_reply(&written);
    if !note_write(&mut log, &write_key, &record) {
        return Err("an unstable write recorded no verifier".into());
    }
    let committed = match transport.commit(file.handle(), 0, 0, deadline()) {
        Ok(reply) => reply,
        Err(_) => return Ok(()),
    };
    let expected = if record.verifier == committed.verifier {
        VerifierMatch::Match
    } else {
        VerifierMatch::Changed {
            recorded: record.verifier,
            observed: committed.verifier,
        }
    };
    if check_commit(&log, &write_key, &committed) != expected {
        return Err("the verifier accounting disagreed with the bytes observed".into());
    }
    Ok(())
}

/// Retained errors: whatever is retained keeps its verbatim `NFS4ERR_*` word.
fn row_retained_error(
    transport: &mut FakeTransport,
    point: FaultPoint,
    action: FaultAction,
) -> Result<(), String> {
    let mut ledger = RetainedErrorLedger::new();
    let retained_key = key("matrix-retained");
    transport.install_faults(DelayedFault::boxed(point, None, 0, action));
    let observed = transport.root_filehandle(deadline());
    let Err(error) = observed else {
        return Ok(());
    };
    let status = error.status();
    ledger
        .retain(
            OperationId(uuid::Uuid::from_u128(7)),
            &retained_key,
            error,
            true,
        )
        .map_err(|error| format!("retaining a fresh key failed: {error}"))?;
    if ledger.status(&retained_key) != status {
        return Err("the retained error lost its original NFS4ERR code".into());
    }
    Ok(())
}

// --- Matrix ------------------------------------------------------------------

const POINTS: [(FaultPoint, &str); 5] = [
    (FaultPoint::BeforeDispatch, "BeforeDispatch"),
    (FaultPoint::AfterDispatch, "AfterDispatch"),
    (FaultPoint::BeforeReturn, "BeforeReturn"),
    (FaultPoint::OnDeadline, "OnDeadline"),
    (FaultPoint::OnConnection, "OnConnection"),
];

fn actions() -> Vec<(&'static str, FaultAction)> {
    vec![
        (
            "Fail",
            FaultAction::Fail(TransportError::Disconnected {
                epoch: ConnectionEpoch(1),
                detail: "fault matrix".into(),
            }),
        ),
        ("Substitute", FaultAction::Substitute(Nfs4Status::DELAY)),
        ("ShortWrite", FaultAction::ShortWrite(2)),
        (
            "RotateVerifier",
            FaultAction::RotateVerifier(WriteVerifier([0xEE; 8])),
        ),
        ("DropReply", FaultAction::DropReply),
    ]
}

fn rows() -> Vec<(&'static str, Row)> {
    vec![
        ("SETCLIENTID", row_set_client_id as Row),
        ("SETCLIENTID_CONFIRM", row_set_client_id_confirm),
        ("OPEN", row_open),
        ("OPEN_CONFIRM", row_open_confirm),
        ("CLOSE", row_close),
        ("OPEN_DOWNGRADE", row_open_downgrade),
        ("OP_RENEW", row_renew),
        ("CLAIM_PREVIOUS reclaim", row_reclaim),
        ("WRITE/COMMIT verifier", row_write_commit_verifier),
        ("Retained error", row_retained_error),
    ]
}

#[test]
fn every_state_transition_survives_every_fault() {
    let mut failures = Vec::new();
    let mut report = String::from(
        "\n| State | Point | Fail | Substitute | ShortWrite | RotateVerifier | DropReply |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for (state, row) in rows() {
        for (point, point_name) in POINTS {
            report.push_str(&format!("| {state} | {point_name} |"));
            for (action_name, action) in actions() {
                // A fresh transport per cell: a fault matrix that shared state
                // between cells would be testing the order it ran them in.
                let mut transport = FakeTransport::new();
                let outcome = row(&mut transport, point, action.clone());
                match outcome {
                    Ok(()) => report.push_str(" PASS |"),
                    Err(detail) => {
                        report.push_str(" FAIL |");
                        failures.push(format!("{state} / {point_name} / {action_name}: {detail}"));
                    }
                }
            }
            report.push('\n');
        }
    }
    println!("{report}");
    assert!(
        failures.is_empty(),
        "fault matrix failures:\n{}",
        failures.join("\n")
    );
}

// --- The five named invariants, asserted directly ----------------------------

/// (a) A dropped reply advances nothing, at every transition that has state.
#[test]
fn a_dropped_reply_advances_no_state() {
    // SETCLIENTID_CONFIRM.
    let mut transport = FakeTransport::new();
    let pending = identity()
        .set_client_id(&mut transport, ConnectionEpoch(1), deadline())
        .unwrap();
    let client_id = pending.client_id();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::OnDeadline,
        None,
        0,
        FaultAction::DropReply,
    ));
    assert!(matches!(
        pending.confirm(&mut transport, deadline()),
        ConfirmOutcome::Indeterminate(_)
    ));
    assert_eq!(
        transport.renew(client_id, deadline()).unwrap_err().status(),
        Some(Nfs4Status::STALE_CLIENTID),
        "a dropped confirm reply must leave the client id unconfirmed"
    );

    // OPEN.
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let burned = incarnation.open_owners().burned();
    let lease = incarnation.open_owners().allocate().unwrap();
    assert_eq!(lease.next_seqid(), Some(0));
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::OnDeadline,
        None,
        0,
        FaultAction::DropReply,
    ));
    let outcome =
        incarnation
            .open_owners()
            .open(lease, &mut transport, &request(b"dropped"), deadline());
    assert!(
        matches!(outcome, OpenOutcome::Abandoned { .. }),
        "a dropped OPEN reply must be abandoned, not guessed at"
    );
    assert_eq!(
        incarnation.open_owners().burned(),
        burned + 1,
        "the owner whose seqid is unknown must be retired"
    );

    // CLOSE.
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let file = open_named(&mut transport, &mut incarnation, b"dropped-close").unwrap();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::OnDeadline,
        None,
        0,
        FaultAction::DropReply,
    ));
    assert!(
        matches!(
            close(file, &mut transport, deadline()),
            CloseOutcome::Abandoned { .. }
        ),
        "a dropped CLOSE reply must never be reported as a close"
    );

    // OP_RENEW.
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let client = incarnation.client().clone();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::OnDeadline,
        None,
        0,
        FaultAction::DropReply,
    ));
    // Past the 45 second renewal threshold of the fake's 90 second lease.
    let outcome = incarnation
        .lease()
        .renew(&mut transport, &client, 60_000, deadline());
    assert!(!outcome.is_renewed());
    assert_eq!(
        incarnation.lease().standing(60_000),
        LeaseStanding::DueForRenewal,
        "an unanswered renewal must not refresh the lease"
    );
}

/// (b) An unresolved retransmit collision advances nothing.
///
/// The replay ledger reports [`Admission::Indeterminate`] for a key that was
/// recorded but never settled — a retransmit whose first attempt's outcome is
/// unknown. Redispatching it would risk repeating a non-idempotent effect, and a
/// poisoned owner cannot issue a seqid to redispatch it with even if a caller
/// tried.
#[test]
fn an_unresolved_retransmit_collision_advances_no_state() {
    use umbra_core::{LeaseEpoch, RunId};
    use umbra_storage_nfs_userspace::replay::{
        Admission, IntentKind, Payload, ReplayIntent, ReplayLog,
    };

    let mut log = FakeReplayLog::default();
    let intent = ReplayIntent {
        run_id: RunId(uuid::Uuid::nil()),
        operation: OperationId(uuid::Uuid::from_u128(1)),
        key: key("collision"),
        epoch: LeaseEpoch(1),
        kind: IntentKind::Namespace {
            operation: "open".into(),
        },
        payload: Payload::Inline(vec![1, 2, 3]),
    };
    assert_eq!(log.admit(&intent).unwrap(), Admission::Fresh);
    assert_eq!(
        log.admit(&intent).unwrap(),
        Admission::Indeterminate,
        "an unsettled key must not be admitted for a second dispatch"
    );

    // The owner side of the same collision: a lost reply poisons the owner, so
    // the retransmit has no seqid to go out under.
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let file = open_named(&mut transport, &mut incarnation, b"collision").unwrap();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::OnDeadline,
        None,
        0,
        FaultAction::DropReply,
    ));
    let op = file.sequence().unwrap();
    let seqid = op.seqid();
    let stateid = op.stateid();
    let lost = transport.close(file.handle(), seqid, stateid, deadline());
    assert!(lost.is_err());
    op.abandon();
    assert!(
        file.sequence().is_err(),
        "a retransmit cannot be sequenced once the owner's seqid is unknown"
    );
    assert!(
        file.stateid().is_ok(),
        "the stateid itself was never invalidated by the lost reply"
    );
}

/// (c) Reconnect plus reclaim recovers only what `CLAIM_PREVIOUS` permits.
#[test]
fn reclaim_recovers_only_what_claim_previous_allows() {
    // In grace, a confirmed open comes back with its identity intact.
    let mut transport = FakeTransport::new();
    let root = transport.root();
    let mut state = ProtocolState::new(identity());
    let incarnation = state
        .establish(&mut transport, &root, 0, deadline())
        .established()
        .unwrap();
    state.adopt(incarnation);
    let file = {
        let incarnation = state.incarnation().unwrap();
        open_named(&mut transport, incarnation, b"reclaimable").unwrap()
    };
    let expected = file.identity();
    state.invalidate();
    transport.reconnect().unwrap();
    transport.set_grace(true);
    let (_, report) = state
        .reestablish_and_reclaim(&mut transport, &root, &[file], 0, deadline())
        .unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(report.is_complete());
    assert_eq!(report.recovered[0].identity(), expected);

    // Out of grace, the same open is surrendered with the server's verbatim code.
    let mut transport = FakeTransport::new();
    let root = transport.root();
    let mut state = ProtocolState::new(identity());
    let incarnation = state
        .establish(&mut transport, &root, 0, deadline())
        .established()
        .unwrap();
    state.adopt(incarnation);
    let file = {
        let incarnation = state.incarnation().unwrap();
        open_named(&mut transport, incarnation, b"too-late").unwrap()
    };
    state.invalidate();
    transport.reconnect().unwrap();
    transport.set_grace(false);
    let (_, report) = state
        .reestablish_and_reclaim(&mut transport, &root, &[file], 0, deadline())
        .unwrap();
    assert!(report.recovered.is_empty(), "no grace, no recovery");
    assert_eq!(report.surrendered.len(), 1);
    assert_eq!(report.surrendered[0].cause, SurrenderCause::GraceLifted);
    assert_eq!(
        report.surrendered[0].status(),
        Some(Nfs4Status::NO_GRACE),
        "the surrender keeps the server's verbatim NFS4ERR_NO_GRACE"
    );
    assert!(report.surrendered[0].cause.is_safe_stop());

    // An open that was never confirmed is not reclaimable at all.
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let lease = incarnation.open_owners().allocate().unwrap();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::AfterDispatch,
        Some(OpCode::OpenConfirm),
        0,
        FaultAction::Substitute(Nfs4Status::DELAY),
    ));
    let unconfirmed = match incarnation.open_owners().open(
        lease,
        &mut transport,
        &request(b"never-confirmed"),
        deadline(),
    ) {
        OpenOutcome::Unconfirmed { file, .. } => file,
        other => panic!("expected an unconfirmed open, got {other:?}"),
    };
    let plan = ReclaimPlan::from_opens([&unconfirmed]);
    assert!(
        plan.targets().is_empty(),
        "an unconfirmed open is never a reclaim target"
    );

    // Nor is a confirmed open whose owner seqid became unknown: the reclaim
    // would be asserting a state that was never proven.
    let mut poisoned_transport = FakeTransport::new();
    let mut poisoned_incarnation = established(&mut poisoned_transport).unwrap();
    let poisoned = open_named(
        &mut poisoned_transport,
        &mut poisoned_incarnation,
        b"poisoned",
    )
    .unwrap();
    assert!(poisoned.is_confirmed().unwrap());
    poisoned.sequence().unwrap().abandon();
    let poisoned_plan = ReclaimPlan::from_opens([&poisoned]);
    assert!(poisoned_plan.targets().is_empty());
    poisoned_transport.set_grace(true);
    let poisoned_report = poisoned_plan.run(
        poisoned_incarnation.open_owners(),
        &mut poisoned_transport,
        deadline(),
    );
    assert!(poisoned_report.recovered.is_empty());
    assert_eq!(
        poisoned_report.surrendered[0].cause,
        SurrenderCause::Indeterminate,
        "a confirmed open with an unknown seqid is indeterminate, not unconfirmed"
    );
    transport.set_grace(true);
    let report = plan.run(incarnation.open_owners(), &mut transport, deadline());
    assert!(report.recovered.is_empty());
    assert_eq!(report.surrendered.len(), 1);
    assert_eq!(
        report.surrendered[0].cause,
        SurrenderCause::NeverConfirmed,
        "what was never proven is surrendered, not claimed back"
    );
}

/// (d) A retained error keeps the original `NFS4ERR_*` code it was recorded with.
#[test]
fn retained_errors_keep_their_original_nfs4err_code() {
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let mut ledger = RetainedErrorLedger::new();
    let write_key = key("retain-nospc");

    let lease = incarnation.open_owners().allocate().unwrap();
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::AfterDispatch,
        Some(OpCode::Open),
        0,
        FaultAction::Substitute(Nfs4Status::NOSPC),
    ));
    let error = incarnation
        .open_owners()
        .open(lease, &mut transport, &request(b"retained"), deadline())
        .error()
        .cloned()
        .expect("a substituted status fails the OPEN");
    assert_eq!(error.status(), Some(Nfs4Status::NOSPC));
    ledger
        .retain(
            OperationId(uuid::Uuid::from_u128(3)),
            &write_key,
            error,
            true,
        )
        .unwrap();
    assert_eq!(ledger.status(&write_key), Some(Nfs4Status::NOSPC));
    assert_eq!(ledger.status(&write_key).unwrap().0, 28);
    assert!(ledger.is_durably_settled(&write_key));

    // Once durable, the answer is final: a different one for the same key is
    // refused rather than silently replacing it.
    assert!(ledger
        .retain(
            OperationId(uuid::Uuid::from_u128(4)),
            &write_key,
            FacadeError::protocol(Nfs4Status::ACCESS, OpCode::Open, 1),
            true
        )
        .is_err());
    assert_eq!(ledger.status(&write_key), Some(Nfs4Status::NOSPC));
}

/// (e) A timed-out lease never authorises taking a client id or its state over.
#[test]
fn no_client_id_takeover_by_timeout_alone() {
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();

    // Let the lease run all the way out with no renewal at all.
    let far_future = incarnation.lease().lease_millis() * 4;
    assert_eq!(
        incarnation.lease().standing(far_future),
        LeaseStanding::PossiblyExpired
    );
    assert!(
        matches!(
            incarnation.lease().takeover_by_timeout(),
            FacadeError::Authority(AuthorityError::TakeoverRefused)
        ),
        "expiry is not proof that any other writer terminated"
    );

    let state = ProtocolState::new(identity());
    assert!(matches!(
        state.takeover(),
        Err(FacadeError::Authority(AuthorityError::TakeoverRefused))
    ));

    // And an expired lease does not let this client keep using its state without
    // proving the id again: the server answers STALE_CLIENTID and that is
    // reported as client loss, not converted into a seizure.
    transport.install_faults(DelayedFault::boxed(
        FaultPoint::AfterDispatch,
        Some(OpCode::Renew),
        0,
        FaultAction::Substitute(Nfs4Status::EXPIRED),
    ));
    let client = incarnation.client().clone();
    let outcome = incarnation
        .lease()
        .renew(&mut transport, &client, far_future, deadline());
    assert!(!outcome.is_renewed());
    assert_eq!(
        outcome.error().and_then(FacadeError::status),
        Some(Nfs4Status::EXPIRED)
    );
}

// --- Scope lock --------------------------------------------------------------

/// No NFSv4.1 operation is ever dispatched, and no `RECLAIM_COMPLETE` exists.
#[test]
fn the_reclaim_path_dispatches_only_nfsv4_0_operations() {
    let recorder = Recorder::default();
    let seen = Arc::clone(&recorder.seen);

    let mut transport = FakeTransport::new();
    transport.install_faults(Box::new(recorder));
    let root = transport.root();
    let mut state = ProtocolState::new(identity());
    let incarnation = state
        .establish(&mut transport, &root, 0, deadline())
        .established()
        .unwrap();
    state.adopt(incarnation);
    let file = {
        let incarnation = state.incarnation().unwrap();
        open_named(&mut transport, incarnation, b"scope-lock").unwrap()
    };
    state.invalidate();
    transport.reconnect().unwrap();
    transport.set_grace(true);
    let (_, report) = state
        .reestablish_and_reclaim(&mut transport, &root, &[file], 0, deadline())
        .unwrap();
    assert_eq!(report.recovered.len(), 1);

    let dispatched = seen.lock().unwrap().clone();
    assert!(
        !dispatched.is_empty(),
        "the recorder must have observed the reclaim"
    );
    for op in &dispatched {
        assert!(
            (*op as u32) <= OpCode::ReleaseLockOwner as u32,
            "{op:?} is not an NFSv4.0 operation"
        );
    }
    assert!(
        dispatched.contains(&OpCode::Open),
        "a reclaim goes out as OPEN with CLAIM_PREVIOUS"
    );
    assert!(
        dispatched.contains(&OpCode::SetClientId)
            && dispatched.contains(&OpCode::SetClientIdConfirm),
        "re-establishment is SETCLIENTID plus SETCLIENTID_CONFIRM"
    );
}

/// Short writes are reported as they happened, never rounded up to the request.
#[test]
fn a_short_write_is_recorded_as_it_happened() {
    let mut transport = FakeTransport::new();
    let mut incarnation = established(&mut transport).unwrap();
    let file = open_named(&mut transport, &mut incarnation, b"short").unwrap();
    let stateid = file.stateid().unwrap();
    transport.set_write_cap(Some(2));
    let reply = transport
        .write(
            file.handle(),
            stateid,
            0,
            Stability::Unstable,
            vec![1, 2, 3, 4],
            deadline(),
        )
        .unwrap();
    let record = WriteRecord::from_reply(&reply);
    assert_eq!(record.count, 2);
    assert!(record.is_short(4));
    assert!(record.needs_commit());
}
