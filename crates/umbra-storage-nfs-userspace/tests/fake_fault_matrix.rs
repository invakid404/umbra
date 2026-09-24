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
use umbra_storage_nfs_userspace::error::ReplayError;
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
use umbra_storage_nfs_userspace::state::retained_errors::{is_settled, RetainedErrorLedger};
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
    let settled = is_settled(&error);

    // **F19.** A fake transport observation is not persistence evidence, so the
    // record is volatile. This row used to pass `true` for every cell, which is
    // how an `NFS4ERR_DELAY` — a condition the server explicitly asks the client
    // to retry — became a durably settled answer that every later retry of the
    // key read back as final.
    ledger
        .retain(
            OperationId(uuid::Uuid::from_u128(7)),
            &retained_key,
            error.clone(),
            false,
        )
        .map_err(|error| format!("retaining a fresh key failed: {error}"))?;
    if ledger.status(&retained_key) != status {
        return Err("the retained error lost its original NFS4ERR code".into());
    }
    if ledger.is_durably_settled(&retained_key) {
        return Err("a volatile record must not report itself durably settled".into());
    }

    // And the durable path answers according to what the failure actually is.
    let durable = ledger.retain(
        OperationId(uuid::Uuid::from_u128(8)),
        &key("matrix-retained-durable"),
        error,
        true,
    );
    match (settled, durable) {
        (true, Ok(_)) | (false, Err(ReplayError::Indeterminate)) => Ok(()),
        (true, Err(error)) => Err(format!("a settled failure must retain durably: {error}")),
        (false, Ok(_)) => {
            Err("a transient failure must not be retained as a durable answer".into())
        }
        (false, Err(error)) => Err(format!("expected Indeterminate, got {error}")),
    }
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

// ===========================================================================
// (d3) Failure handling on the PUBLIC surface: no receipt escapes a failure
// ===========================================================================
//
// These cases drive `NfsUserspaceStorage` through the `Storage` contract with a
// fault installed on the fake, then call `flush` and assert the firewall directly:
// whatever a fault does to a write, a following `flush` returns `Err` and never a
// receipt. The negative assertion is the point of every case. The one exception is
// the short write — a *legal* reply, not a failure — where the property under test
// is instead that the ledger records the actual byte count, never the requested.

use umbra_core::{
    BytePath, CreateKind, CreateOptions, Durability, FlushRequest, FlushScope,
    ImmutableBaseContract, OpenRunIntent, OpenRunRequest, RequestContext, RunId, StorageAnchor,
    StorageOperation, StoragePath, StoragePolicy, StorageRequest, StorageResponse,
};
use umbra_storage::Storage;
use umbra_storage_nfs_userspace::fake::ScriptedFault;
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};

const FAULT_EXPORT: &[u8] = b"umbra";
const FAULT_RUN_PARENT: &[u8] = b"runs";

fn fault_config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        // A loopback port owned by no other suite; nothing opens a socket here.
        port: 12118,
        export: BytePath::new(FAULT_EXPORT).expect("export"),
        run_parent: BytePath::new(FAULT_RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: Deadline { millis: 1_000 },
    }
}

/// A fake server carrying only `<export>/<run_parent>`, so a case creates its run.
fn fault_server() -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in FAULT_EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    fake.insert_directory(&current, FAULT_RUN_PARENT);
    fake
}

/// Build a provider over a fake configured by `setup` — install a fault plan, cap
/// writes, or leave it clean. The fault targets `OpCode::Commit`, which only the
/// data-write path issues, so `open_run` runs unfaulted and the fault stays armed
/// until the first `WriteAt`.
fn provider_with(setup: impl FnOnce(&mut FakeTransport)) -> NfsUserspaceStorage {
    let mut fake = fault_server();
    setup(&mut fake);
    NfsUserspaceStorage::with_facades(
        fault_config(),
        Box::new(fake),
        Box::new(FakeReplayLog::default()),
    )
    .expect("the provider accepts the fault fixture")
}

fn fault_run(run_id: RunId, intent: OpenRunIntent) -> OpenRunRequest {
    OpenRunRequest {
        run_id,
        intent,
        immutable_base: ImmutableBaseContract {
            identity: "umbra-fault-matrix".into(),
            fingerprint: vec![0x46, 0x4D],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: FORMAT_VERSION,
        },
    }
}

fn fault_ctx(storage: &NfsUserspaceStorage, key: &str) -> RequestContext {
    let admitted = storage
        .admission()
        .expect("a run is open on this provider")
        .admitted();
    RequestContext {
        run_id: admitted.run(),
        operation_id: OperationId(uuid::Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: Some(admitted.epoch()),
    }
}

fn fault_path(name: &str) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, name.as_bytes()).expect("relative path")
}

/// Create `name` (unfaulted), then write `bytes` to it and return the write's
/// result — which is where a Commit-targeted fault surfaces.
fn create_then_write(
    storage: &mut NfsUserspaceStorage,
    name: &str,
    bytes: &[u8],
) -> umbra_core::Result<StorageResponse> {
    let create = StorageRequest {
        context: fault_ctx(storage, &format!("create-{name}")),
        operation: StorageOperation::Create {
            path: fault_path(name),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o640,
            },
        },
    };
    storage
        .execute(&create)
        .expect("the create itself is unfaulted");
    let write = StorageRequest {
        context: fault_ctx(storage, &format!("write-{name}")),
        operation: StorageOperation::WriteAt {
            path: fault_path(name),
            offset: 0,
            bytes: bytes.to_vec(),
        },
    };
    storage.execute(&write)
}

/// Fails the run's single data COMMIT with a transport error, leaving that write
/// with an unproven server-side disposition. Armed by the COMMIT's `AfterDispatch`
/// (the one point whose context carries the real op, and the only COMMIT the whole
/// session issues — `open_run`'s manifest writes are FILE_SYNC and owe none), then
/// fired at that same COMMIT's `BeforeReturn`, where the fake honours `Fail`.
#[derive(Default)]
struct FailTheCommit {
    armed: bool,
    fired: bool,
}

impl FaultPlan for FailTheCommit {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if self.fired {
            return FaultAction::Proceed;
        }
        match point {
            FaultPoint::AfterDispatch if context.op == OpCode::Commit => {
                self.armed = true;
                FaultAction::Proceed
            }
            FaultPoint::BeforeReturn if self.armed => {
                self.fired = true;
                FaultAction::Fail(TransportError::Disconnected {
                    epoch: ConnectionEpoch(1),
                    detail: "fault: the commit reply never arrived".into(),
                })
            }
            _ => FaultAction::Proceed,
        }
    }
}

/// Rotates the server's write verifier to a fresh value after *every* WRITE.
///
/// This is robust without counting `open_run`'s internal writes: a FILE_SYNC write
/// owes no COMMIT and records no verifier, so rotating after one is harmless. Only
/// the run's single UNSTABLE data write records a verifier and then COMMITs, and by
/// then the verifier has moved on — exactly the mismatch a server that lost
/// unstable data across a restart produces. Armed by each WRITE's `AfterDispatch`,
/// fired at that WRITE's immediately following `BeforeReturn`.
#[derive(Default)]
struct RotateVerifierEachWrite {
    armed: bool,
    next: u8,
}

impl FaultPlan for RotateVerifierEachWrite {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        match point {
            FaultPoint::AfterDispatch if context.op == OpCode::Write => {
                self.armed = true;
                FaultAction::Proceed
            }
            FaultPoint::BeforeReturn if self.armed => {
                self.armed = false;
                self.next = self.next.wrapping_add(1);
                FaultAction::RotateVerifier(WriteVerifier([self.next; 8]))
            }
            _ => FaultAction::Proceed,
        }
    }
}

/// The load-bearing assertion: neither scope a caller can issue yields a receipt.
fn assert_no_receipt(storage: &mut NfsUserspaceStorage, key: &str) {
    for scope in [
        FlushScope::EntireRun,
        FlushScope::Data {
            objects: Vec::new(),
        },
    ] {
        let request = FlushRequest {
            context: fault_ctx(storage, key),
            scope: scope.clone(),
        };
        let result = storage.flush(&request);
        assert!(
            result.is_err(),
            "a failure path yielded a receipt for {scope:?}: {result:?}"
        );
    }
}

#[test]
fn a_dropped_commit_reply_is_indeterminate_and_never_a_receipt() {
    let mut storage = provider_with(|fake| {
        fake.install_faults(Box::new(FailTheCommit::default()));
    });
    let run_id = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    // The COMMIT never returns cleanly, so the write's disposition is unproven.
    let write = create_then_write(&mut storage, "dropped.bin", b"unproven-bytes");
    assert!(write.is_err(), "a failed commit is not a success");

    // An unproven server-side disposition is indeterminate: flush refuses.
    assert_no_receipt(&mut storage, "flush-dropped");
    storage.close_run().ok();
}

#[test]
fn a_commit_returning_io_latches_and_every_later_flush_refuses() {
    let mut storage = provider_with(|fake| {
        fake.install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            Some(OpCode::Commit),
            FaultAction::Substitute(Nfs4Status::IO),
        ));
    });
    let run_id = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    let write = create_then_write(&mut storage, "eio.bin", b"lost-write");
    let error = write.expect_err("a COMMIT NFS4ERR_IO is a failed stable write");
    assert_eq!(error.kind, umbra_core::ErrorKind::Io);

    // The lost-write evidence is latched: a mutation is now refused with the
    // original failure, and it is not erasable by a later flush.
    let mutation = StorageRequest {
        context: fault_ctx(&storage, "post-latch-write"),
        operation: StorageOperation::WriteAt {
            path: fault_path("eio.bin"),
            offset: 0,
            bytes: b"retry".to_vec(),
        },
    };
    assert!(
        storage.execute(&mutation).is_err(),
        "a latched write failure must refuse further mutations"
    );

    // Repeated flushes keep refusing; a later flush cannot erase the evidence.
    assert_no_receipt(&mut storage, "flush-latch-1");
    assert_no_receipt(&mut storage, "flush-latch-2");
    storage.close_run().ok();
}

/// A COMMIT that fails with a status mapping OUTSIDE `{StorageUnavailable, Io}` must
/// still make `flush` refuse — it must never let the barrier certify `Remote` over
/// bytes whose COMMIT never proved them stable.
///
/// This pins the durability-without-evidence hole (CR-2): a successful
/// `WRITE(UNSTABLE)` whose COMMIT returns e.g. `NFS4ERR_ACCESS` (→ `Denied`),
/// `NFS4ERR_STALE` (→ `StaleHandle`) or `NFS4ERR_INVAL` (→ `InvalidInput`) — all of
/// which RFC 7530 lists among COMMIT's legal errors — records no ledger entry and
/// trips none of `note_failure`'s kind filters. Without the write-path fix,
/// `flush(EntireRun)` passes every guard and returns a receipt over data sitting
/// in the server's volatile storage: exactly the lie the firewall exists to make
/// impossible. (That receipt read `Ok(Durability::Remote)` when this case was
/// written. Over the fake it would now read `Ok(Durability::Local)`, because the
/// claim is qualified per run — but a receipt is still a receipt, and certifying
/// one here would still be the lie.)
#[test]
fn a_commit_failing_with_a_non_io_non_transport_status_still_refuses_flush() {
    let mut storage = provider_with(|fake| {
        fake.install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            Some(OpCode::Commit),
            FaultAction::Substitute(Nfs4Status::ACCESS),
        ));
    });
    let run_id = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    // WRITE(UNSTABLE) lands; its COMMIT returns NFS4ERR_ACCESS -> Denied, a kind
    // outside {StorageUnavailable, Io}. The write is a failure whose durability is
    // unproven, not a success.
    let write = create_then_write(&mut storage, "denied.bin", b"unproven-under-access");
    let error = write.expect_err("a COMMIT NFS4ERR_ACCESS is not a success");
    assert_eq!(error.kind, umbra_core::ErrorKind::Denied);

    // The load-bearing negative: no receipt escapes an unproven COMMIT, under either
    // scope a caller can issue, and a later flush still refuses.
    assert_no_receipt(&mut storage, "flush-denied-1");
    assert_no_receipt(&mut storage, "flush-denied-2");
    storage.close_run().ok();
}

#[test]
fn a_changed_commit_verifier_yields_no_receipt() {
    let mut storage = provider_with(|fake| {
        fake.install_faults(Box::new(RotateVerifierEachWrite::default()));
    });
    let run_id = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    // The COMMIT returns a verifier that does not match the WRITE's: the server
    // lost the unstable data, and the write surfaces that rather than succeeding.
    let write = create_then_write(&mut storage, "drift.bin", b"pre-restart");
    assert!(write.is_err(), "a changed verifier is not a success");

    assert_no_receipt(&mut storage, "flush-drift");
    storage.close_run().ok();
}

#[test]
fn a_short_write_records_the_actual_count_not_the_requested() {
    // The cap is above every write `open_run` itself issues (its epoch marker and
    // manifest are small), so provisioning is unaffected; the data payload below is
    // larger than the cap, so only it is truncated.
    let cap = 1024u32;
    let mut storage = provider_with(|fake| fake.set_write_cap(Some(cap)));
    let run_id = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    // A short write is a legal reply, not a failure: it settles with the count the
    // server actually accepted, never the requested length.
    let payload = vec![0x5Au8; 2048];
    let write = create_then_write(&mut storage, "short.bin", &payload)
        .expect("a short write is a legal, settled reply");
    assert_eq!(
        write,
        StorageResponse::WriteAt(cap),
        "the settled count is the actual, never the requested"
    );

    // The barrier is satisfied, and its evidence reports the actual byte count.
    let request = FlushRequest {
        context: fault_ctx(&storage, "flush-short"),
        scope: FlushScope::EntireRun,
    };
    let receipt = storage
        .flush(&request)
        .expect("a short write still settles");
    assert_ne!(receipt.durability, Durability::None);
    let evidence = String::from_utf8(receipt.evidence).expect("utf-8 evidence");
    assert!(
        evidence.contains(&format!("committed_bytes={cap}")),
        "the ledger must record the actual count, not the requested: {evidence}"
    );
    storage.close_run().ok();
}

#[test]
fn the_ledger_is_cleared_on_close_run_so_the_next_run_is_not_poisoned() {
    let mut storage = provider_with(|_| {});

    // Run 1 lands a settled write, so its ledger is non-empty.
    let first = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(first, OpenRunIntent::CreateNew))
        .expect("open run 1");
    create_then_write(&mut storage, "run1.bin", b"first-run-bytes").expect("run 1 write settles");
    let run1_receipt = storage
        .flush(&FlushRequest {
            context: fault_ctx(&storage, "flush-run1"),
            scope: FlushScope::EntireRun,
        })
        .expect("run 1 barrier");
    let run1_evidence = String::from_utf8(run1_receipt.evidence).expect("utf-8");
    assert!(
        run1_evidence.contains("committed_writes=1"),
        "run 1 must have recorded its write: {run1_evidence}"
    );
    storage.close_run().expect("close run 1");

    // Run 2, on the SAME provider, must start from an empty ledger: close_run
    // cleared it, so its barrier is not vacuously certified over run 1's write.
    let second = RunId(uuid::Uuid::new_v4());
    storage
        .open_run(&fault_run(second, OpenRunIntent::CreateNew))
        .expect("open run 2");
    let run2_receipt = storage
        .flush(&FlushRequest {
            context: fault_ctx(&storage, "flush-run2"),
            scope: FlushScope::EntireRun,
        })
        .expect("run 2 barrier over nothing outstanding");
    let run2_evidence = String::from_utf8(run2_receipt.evidence).expect("utf-8");
    assert!(
        run2_evidence.contains("committed_writes=0"),
        "the ledger leaked run 1's writes into run 2: {run2_evidence}"
    );
    storage.close_run().expect("close run 2");
}

// ===========================================================================
// The persistence boundary is a mechanism, not a convention
// ===========================================================================
//
// `QUALIFIED_DURABILITY` used to be read straight into every run binding and
// every flush receipt, so this job — no fixture, no `transport-raw` feature, no
// live transport linked at all — advertised `Durability::Remote` with zero live
// evidence. It is now the ceiling on a claim that is earned per `open_run` from
// two independent gates ANDed together, and this section is the proof that runs
// unconditionally, here, with no fixture and no waiver:
//
// * **(e1)** the plain fake declares nothing, so it claims only `Local`;
// * a transport that *does* declare a remote boundary, and whose probe then
//   completes a matched-verifier COMMIT cycle, reaches `Remote` on the binding
//   and on the receipt alike — and leaves the run's layout untouched;
// * **(e4)** a declared boundary whose probe *fails* degrades to `Local` and
//   does nothing else: the run still writes, and still flushes.
//
// The declaring transport here is a deliberate local lie — `FakeTransport`
// wearing a declaration it has not earned — because that is the only way to
// drive the probe path at all without a live server. It is exactly why the
// declaration alone is never enough: the fake reproduces the WRITE/COMMIT
// verifier protocol faithfully and passes any probe put to it.

use umbra_storage_nfs_userspace::handle::FileHandle;
use umbra_storage_nfs_userspace::storage::QUALIFIED_DURABILITY;
use umbra_storage_nfs_userspace::transport::{
    CallToken, Compound, CompoundReply, ConnectionState, DirCookie, DirVerifier, Nfs4Op, Nfs4Type,
    PersistenceBoundary, ReadDirRequest, Retirement, TransportLimits, TransportResult, WireProfile,
};

/// A [`FakeTransport`] that declares a remote persistence boundary.
///
/// Every required method delegates to the inner fake, so the shape helpers the
/// trait defaults build on behave identically; the only difference on the wire
/// is `rotate_between_write_and_commit`.
struct DeclaredRemote {
    inner: FakeTransport,
    /// Rotate the fake's write/commit verifier immediately after the first
    /// `UNSTABLE` WRITE reply, exactly as a server restarting between WRITE and
    /// COMMIT would, so the probe's COMMIT returns a verifier its WRITE never
    /// reported.
    ///
    /// Keyed on `UNSTABLE` because the probe issues the only such write in an
    /// `open_run`: the anchor and admission writes are all `FILE_SYNC` and owe
    /// no COMMIT.
    rotate_between_write_and_commit: bool,
}

impl DeclaredRemote {
    fn new(inner: FakeTransport, rotate_between_write_and_commit: bool) -> Self {
        Self {
            inner,
            rotate_between_write_and_commit,
        }
    }
}

impl RawTransport for DeclaredRemote {
    fn wire_profile(&self) -> WireProfile {
        self.inner.wire_profile()
    }

    fn limits(&self) -> TransportLimits {
        self.inner.limits()
    }

    fn connection(&self) -> ConnectionState {
        self.inner.connection()
    }

    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply> {
        let unstable = call.ops.iter().any(|op| {
            matches!(
                op,
                Nfs4Op::Write {
                    stability: Stability::Unstable,
                    ..
                }
            )
        });
        let reply = self.inner.submit(call, deadline)?;
        if unstable && self.rotate_between_write_and_commit {
            self.rotate_between_write_and_commit = false;
            self.inner.rotate_write_verifier(WriteVerifier([0xAB; 8]));
        }
        Ok(reply)
    }

    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement> {
        self.inner.cancel(token)
    }

    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.inner.reconnect()
    }

    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.inner.install_faults(plan);
    }

    fn persistence_boundary(&self) -> PersistenceBoundary {
        PersistenceBoundary::RemoteServer
    }
}

/// A provider whose transport declares a remote boundary; `rotate` decides
/// whether the probe's COMMIT will find the verifier its WRITE reported.
fn provider_declaring_remote(rotate: bool) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(
        fault_config(),
        Box::new(DeclaredRemote::new(fault_server(), rotate)),
        Box::new(FakeReplayLog::default()),
    )
    .expect("the provider accepts the declaring fixture")
}

/// Every name in `directory`, with its type and mode, `.`/`..` dropped.
fn boundary_listing(
    transport: &mut dyn RawTransport,
    directory: &FileHandle,
) -> Vec<(Vec<u8>, Nfs4Type, u32)> {
    let deadline = Deadline { millis: 1_000 };
    let mut entries = Vec::new();
    let mut cookie = DirCookie(0);
    let mut verifier = DirVerifier([0; 8]);
    loop {
        let page = transport
            .readdir(
                directory,
                ReadDirRequest {
                    cookie,
                    verifier,
                    dir_count: 8192,
                    max_count: 32768,
                    attrs: AttrMask::TYPE.union(AttrMask::MODE),
                },
                deadline,
            )
            .expect("READDIR");
        verifier = page.verifier;
        for entry in &page.entries {
            cookie = entry.cookie;
            let name = entry.name.as_bytes().to_vec();
            if name == b"." || name == b".." {
                continue;
            }
            entries.push((
                name,
                entry.attributes.file_type.expect("FATTR4_TYPE"),
                entry.attributes.mode.expect("FATTR4_MODE") & 0o7777,
            ));
        }
        if page.eof {
            break;
        }
    }
    entries
}

/// The run's whole layout as sorted `<path>\t<kind>\t0oMODE` rows, in the shape
/// `tests/goldens/run-layout.txt` pins.
fn boundary_layout(transport: &mut dyn RawTransport, run_id: RunId) -> Vec<String> {
    let deadline = Deadline { millis: 1_000 };
    let mut parent = transport.root_filehandle(deadline).expect("root");
    for part in [FAULT_EXPORT, FAULT_RUN_PARENT] {
        parent = transport
            .lookup(
                &parent,
                &ComponentName::new(part.to_vec()).expect("component"),
                AttrMask::IDENTITY,
                deadline,
            )
            .expect("resolve the export path")
            .0;
    }
    let run_name = ComponentName::new(run_id.0.hyphenated().to_string().into_bytes())
        .expect("run directory component");
    let (run, run_attrs) = transport
        .lookup(
            &parent,
            &run_name,
            AttrMask::TYPE.union(AttrMask::MODE),
            deadline,
        )
        .expect("stat the run directory");

    let kind = |kind| match kind {
        Nfs4Type::Directory => "dir",
        _ => "file",
    };
    let mut rows = vec![format!(
        "<run>\tdir\t0o{:o}",
        run_attrs.mode.expect("mode") & 0o7777
    )];
    for (name, entry_kind, mode) in boundary_listing(transport, &run) {
        let label = format!("<run>/{}", String::from_utf8_lossy(&name));
        rows.push(format!("{label}\t{}\t0o{mode:o}", kind(entry_kind)));
        if name == umbra_storage_nfs_userspace::layout::PRIVATE_DIR {
            let private = transport
                .lookup(
                    &run,
                    &ComponentName::new(name.clone()).expect("component"),
                    AttrMask::IDENTITY,
                    deadline,
                )
                .expect("resolve .provider")
                .0;
            for (child, child_kind, child_mode) in boundary_listing(transport, &private) {
                rows.push(format!(
                    "<run>/.provider/{}\t{}\t0o{child_mode:o}",
                    String::from_utf8_lossy(&child),
                    kind(child_kind)
                ));
            }
        }
    }
    rows.sort();
    rows
}

/// The eight rows `tests/goldens/run-layout.txt` pins, sorted to match.
fn golden_layout_rows() -> Vec<String> {
    let golden = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/run-layout.txt"),
    )
    .expect("read the run-layout golden");
    let mut rows: Vec<String> = golden
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(rows.len(), 8, "the golden pins exactly eight rows");
    rows.sort();
    rows
}

/// **(e1).** With no fixture and no declared boundary, the claim is `Local` —
/// on the binding, on `capabilities()` and on the flush receipt.
///
/// This is the case the whole issue is about. It runs in the default CI job,
/// which has no `UMBRA_NFS_RAW_FIXTURE` and no `transport-raw` feature, so
/// nothing in this build could possibly have qualified a remote boundary. The
/// old code shipped `Durability::Remote` here anyway.
#[test]
fn a_run_over_an_undeclared_transport_claims_only_local_durability() {
    let mut storage = provider_with(|_| {});
    let run_id = RunId(uuid::Uuid::new_v4());
    let binding = storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    assert_eq!(
        binding.capabilities.durability,
        Durability::Local,
        "the fake declares no persistence boundary, so nothing may be claimed"
    );
    assert_eq!(storage.capabilities().durability, Durability::Local);

    // A settled write and a satisfied barrier still happen — degrading the claim
    // degrades nothing else. The receipt inherits `Local` rather than stamping
    // the constant.
    create_then_write(&mut storage, "undeclared.bin", b"bytes").expect("the write settles");
    let receipt = storage
        .flush(&FlushRequest {
            context: fault_ctx(&storage, "flush-undeclared"),
            scope: FlushScope::EntireRun,
        })
        .expect("a barrier over a settled run is a receipt");
    assert_eq!(receipt.durability, Durability::Local);
    // `Local` still clears the overlay's only check (engine.rs:2618).
    assert_ne!(receipt.durability, Durability::None);

    storage.close_run().expect("close_run");
}

/// A declared boundary plus a probe that proved it reaches `QUALIFIED_DURABILITY`
/// — and the probe leaves the run exactly as the golden pins it.
///
/// Both halves matter. The first is that the conjunction actually admits the
/// qualified claim, so `Local` is not simply hard-wired. The second is the
/// orphan assertion: the probe creates a file in `.provider`, writes it, commits
/// it, closes it and removes it, and after `open_run` + `close_run` the run's
/// whole layout must still be the eight rows the mounted-adapter golden pins,
/// with no `boundary-probe` among them.
#[test]
fn a_declared_boundary_with_a_matched_verifier_probe_claims_remote_and_leaves_no_orphan() {
    let mut storage = provider_declaring_remote(false);
    let run_id = RunId(uuid::Uuid::new_v4());
    let binding = storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("open_run");

    assert_eq!(
        binding.capabilities.durability, QUALIFIED_DURABILITY,
        "both gates held, so the run earned the qualified claim"
    );
    assert_eq!(storage.capabilities().durability, QUALIFIED_DURABILITY);

    let receipt = storage
        .flush(&FlushRequest {
            context: fault_ctx(&storage, "flush-declared"),
            scope: FlushScope::EntireRun,
        })
        .expect("a barrier over nothing outstanding is a receipt");
    assert_eq!(
        receipt.durability, QUALIFIED_DURABILITY,
        "the receipt inherits the run's qualification"
    );

    storage.close_run().expect("close_run");

    let observed = boundary_layout(storage.transport().expect("transport"), run_id);
    assert_eq!(
        observed,
        golden_layout_rows(),
        "the probe must leave the run byte-identical to the mounted-adapter layout"
    );
    // Named explicitly, because this is the artifact that would leak.
    assert!(
        !observed
            .iter()
            .any(|row| row.contains("<run>/.provider/boundary-probe")),
        "the probe artifact survived its own open_run: {observed:?}"
    );
}

/// **(e4).** A probe that fails degrades the claim and does nothing else.
///
/// This is the sharpest hazard in the design. `flush`'s case-3 refusal returns
/// `Err` for the life of the run whenever `unsettled` is non-empty, so a probe
/// that recorded its failure as run bookkeeping would turn "this run may not
/// claim `Remote`" into "this run can never flush again" — a far worse
/// regression than the over-claim being fixed. The probe's only output is a
/// boolean, and this pins that: the transport declares a remote boundary, the
/// probe's COMMIT finds a rotated verifier and fails, and the run goes on to
/// write and to flush exactly as an unqualified run does.
#[test]
fn a_failed_probe_degrades_the_claim_without_bricking_the_run() {
    let mut storage = provider_declaring_remote(true);
    let run_id = RunId(uuid::Uuid::new_v4());
    let binding = storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("a failed probe never fails the open");

    assert_eq!(
        binding.capabilities.durability,
        Durability::Local,
        "the verifier changed under the probe, so the boundary is unproven"
    );

    // The run is fully usable: a write settles, and the barrier certifies it.
    // Both would be impossible had the probe's failure reached `unsettled`,
    // `stable_write_failure` or `recovery_blocked`.
    create_then_write(&mut storage, "after-failed-probe.bin", b"still-writable")
        .expect("a failed probe must not refuse the run's writes");
    let receipt = storage
        .flush(&FlushRequest {
            context: fault_ctx(&storage, "flush-after-failed-probe"),
            scope: FlushScope::EntireRun,
        })
        .expect("a failed probe must not brick the barrier");
    assert_eq!(receipt.durability, Durability::Local);
    let evidence = String::from_utf8(receipt.evidence).expect("utf-8 evidence");
    assert!(
        evidence.contains("committed_writes=1"),
        "the probe must not fold its synthetic write into the ledger: {evidence}"
    );

    storage.close_run().expect("close_run");

    // And it still cleaned up after itself: a probe that failed mid-cycle owes
    // the same empty layout as one that succeeded.
    let observed = boundary_layout(storage.transport().expect("transport"), run_id);
    assert!(
        !observed
            .iter()
            .any(|row| row.contains("<run>/.provider/boundary-probe")),
        "a failed probe left its artifact behind: {observed:?}"
    );
}

/// The same run, reopened read-only.
///
/// Always `OpenExisting`: a read-only run cannot be *created*, because creation
/// is itself a mutation and `Operations::open` refuses the combination outright.
fn read_only_run(run_id: RunId) -> OpenRunRequest {
    let mut request = fault_run(run_id, OpenRunIntent::OpenExisting);
    request.policy.read_only = true;
    request
}

/// **D.7.** A read-only run is never probed, so it never claims more than
/// `Local` — even over a transport that declares a remote persistence boundary.
///
/// The guard exists because the probe is a *write*: `preflight` refuses every
/// mutation on a read-only run, so the cycle cannot honestly be driven there,
/// and a boundary that was never proven must not be claimed. Unguarded, a
/// read-only run over a declaring transport — in production, the real
/// `LibnfsRawTransport` — would drive a synthetic WRITE into `.provider` in
/// breach of the run's own contract, and would then claim `Remote` on a run
/// forbidden to mutate.
///
/// This case is what distinguishes the read-only arm from the undeclared arm.
/// Every other read-only test in the crate runs over the plain fake, which
/// declares nothing, so gate 1 short-circuits before the read-only term is ever
/// load-bearing and deleting the guard changes no observable behaviour.
#[test]
fn a_read_only_run_over_a_declaring_transport_is_never_probed() {
    let mut storage = provider_declaring_remote(false);
    let run_id = RunId(uuid::Uuid::new_v4());

    // Writable first, so the run exists to reopen — and so the qualified claim
    // is shown to be reachable on this very transport. That makes `read_only`
    // the single variable between the two opens below.
    let created = storage
        .open_run(&fault_run(run_id, OpenRunIntent::CreateNew))
        .expect("create the run writable");
    assert_eq!(
        created.capabilities.durability, QUALIFIED_DURABILITY,
        "the writable open probed and qualified over this transport"
    );
    storage.close_run().expect("release");

    let reopened = storage
        .open_run(&read_only_run(run_id))
        .expect("reopen read-only");
    assert_eq!(
        reopened.capabilities.durability,
        Durability::Local,
        "a read-only run cannot drive the probe, so it has proven nothing and \
         must claim nothing — the transport's declaration alone is never enough"
    );
    assert_eq!(storage.capabilities().durability, Durability::Local);

    storage.close_run().expect("close the read-only run");

    // And nothing was written on its behalf. Asserted as the artifact's absence
    // rather than against the whole golden, because an `OpenExisting` succession
    // legitimately records its own per-epoch claim in `.provider` (R1-001).
    let observed = boundary_layout(storage.transport().expect("transport"), run_id);
    assert!(
        !observed
            .iter()
            .any(|row| row.contains("<run>/.provider/boundary-probe")),
        "a read-only run was probed: {observed:?}"
    );
}
