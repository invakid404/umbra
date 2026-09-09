//! Durable intent, payload identity and committed result — recorded before the
//! caller is told anything.
//!
//! This is a thin, opinionated driver over the frozen
//! [`ReplayLog`](crate::replay::ReplayLog). The facade already fixes the record
//! shapes and the idempotency rules; what this module adds is the ordering the
//! failure model demands and a type that makes it unskippable.
//!
//! # The ticket
//!
//! [`DispatchTicket`] has no public constructor. The only way to obtain one is
//! [`MutationJournal::begin`] returning [`Acknowledged::Dispatch`], which happens
//! only after the intent, the writer epoch and the payload have reached the log.
//! A caller that wants to dispatch therefore cannot avoid recording first — not
//! by convention, but because it has nothing to dispatch *with*.
//!
//! Settling consumes the ticket, so a result cannot be recorded twice and a
//! dispatch cannot be silently left unsettled while the caller moves on.
//!
//! # Backpressure before dispatch
//!
//! [`MutationJournal::begin`] checks occupancy against the budget *before* it
//! admits, so an exhausted buffer refuses the mutation rather than letting it
//! reach the wire and then discovering there is nowhere to record its outcome.
//! The failure model calls for exactly this ordering.
//!
//! # What recovery can and cannot rebuild
//!
//! [`MutationJournal::plan_recovery`] sorts a run's keys into settled, replayable
//! and blocked. A digest-only payload for a write is blocked with
//! [`ReplayError::PayloadMissing`], never reconstructed — a digest proves what
//! the bytes *were*, not where they still are. A namespace intent whose reply was
//! lost is blocked too: its record carries an operation name, which is not the
//! before/after proof the failure model requires, and guessing is worse than
//! stopping. Turning that into implemented recovery needs a staging protocol in
//! the operations node; authority does not get to approximate it.

use std::collections::BTreeSet;

use umbra_core::{IdempotencyKey, LeaseEpoch, OperationId, RunId};

use crate::error::{AuthorityError, FacadeError, ReplayError, RetainedError};
use crate::replay::{
    Admission, Backpressure, CompletedWrite, IntentKind, Payload, ReplayIntent, ReplayLog,
    ReplayOutcome, ReplayRecord, VerifierMatch,
};
use crate::transport::WriteVerifier;

use super::admission::Admitted;

/// Whether the log behind a journal actually reaches a persistence boundary.
///
/// Carried explicitly because a [`RetainedError`] must not claim durability the
/// log does not provide. A journal over the in-memory fake is
/// [`Durability::Volatile`], so a consumer gating replay on
/// [`RetainedError::is_durable`] behaves under the fake exactly as it will
/// against a durable log instead of accidentally depending on volatile evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// The log persists records across a process restart.
    Durable,
    /// The log is in memory. Records do not survive the process.
    Volatile,
}

impl Durability {
    fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// One mutation, as it is described to the journal before dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationRequest {
    /// Operation identity. Reusing it under a different key is refused.
    pub operation: OperationId,
    /// Idempotency key. The identity a retry is matched on.
    pub key: IdempotencyKey,
    /// What is being attempted.
    pub kind: IntentKind,
    /// Payload retention. A digest is not a payload.
    pub payload: Payload,
}

/// Proof that one mutation's intent reached the log before it was dispatched.
///
/// No public constructor: see the module documentation.
#[derive(Debug, PartialEq, Eq)]
pub struct DispatchTicket {
    run: RunId,
    operation: OperationId,
    key: IdempotencyKey,
    epoch: LeaseEpoch,
}

impl DispatchTicket {
    /// The run this dispatch belongs to.
    pub fn run(&self) -> RunId {
        self.run
    }

    /// The operation identity recorded with the intent.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// The key the outcome will be settled under.
    pub fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The writer epoch that authorised the mutation.
    pub fn epoch(&self) -> LeaseEpoch {
        self.epoch
    }
}

/// What [`MutationJournal::begin`] settled.
#[derive(Debug)]
pub enum Acknowledged {
    /// Intent recorded. Dispatch may proceed with this ticket.
    Dispatch(DispatchTicket),
    /// This key already has a settled outcome; it is the answer forever.
    Replayed(ReplayOutcome),
    /// The key is recorded with no outcome. Re-dispatching could repeat a
    /// non-idempotent effect, so recovery settles it before anything else runs.
    Indeterminate,
}

/// The committed result identity a dispatch produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittedResult {
    /// What the server actually achieved: exact count, actual stability, verifier.
    Completed(CompletedWrite),
    /// A failure, retained verbatim as the answer for this key.
    Failed(FacadeError),
}

/// How one key stands after a crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStanding {
    /// A settled outcome exists; return it and do not dispatch.
    Settled(ReplayOutcome),
    /// Intent and payload both survived, so the mutation can be re-driven under
    /// its original identity.
    Replayable,
    /// Durable evidence does not settle this key and cannot rebuild it.
    Blocked(FacadeError),
}

/// What a bounded post-crash replay pass may do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// Keys whose recorded outcome is the answer. Never re-dispatched.
    pub settled: Vec<IdempotencyKey>,
    /// Keys that can be re-driven under their original identity.
    pub replayable: Vec<IdempotencyKey>,
    /// Keys that stop, each with its verbatim reason.
    pub blocked: Vec<(IdempotencyKey, FacadeError)>,
}

impl RecoveryPlan {
    /// Total keys the plan accounts for.
    pub fn len(&self) -> usize {
        self.settled.len() + self.replayable.len() + self.blocked.len()
    }

    /// Whether the plan accounts for nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether every key is either settled or replayable.
    pub fn is_clean(&self) -> bool {
        self.blocked.is_empty()
    }
}

/// Records every mutation's intent, payload and committed result.
#[derive(Debug)]
pub struct MutationJournal<L: ReplayLog> {
    log: L,
    run: RunId,
    durability: Durability,
    admitted_keys: BTreeSet<IdempotencyKey>,
}

impl<L: ReplayLog> MutationJournal<L> {
    /// A journal for `run` over `log`.
    pub fn new(run: RunId, log: L, durability: Durability) -> Self {
        Self {
            log,
            run,
            durability,
            admitted_keys: BTreeSet::new(),
        }
    }

    /// The underlying log.
    pub fn log(&self) -> &L {
        &self.log
    }

    /// The underlying log, mutably.
    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// Whether records here reach a persistence boundary.
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Current occupancy of the bounded buffer.
    pub fn pressure(&self) -> Backpressure {
        self.log.pressure()
    }

    /// Keys this journal admitted, in key order.
    ///
    /// A durable log enumerates its own records; this in-process set is what a
    /// test uses to stand in for that enumeration.
    pub fn admitted_keys(&self) -> impl Iterator<Item = &IdempotencyKey> {
        self.admitted_keys.iter()
    }

    /// Record a mutation's intent, payload and epoch durably, then authorise
    /// dispatch.
    ///
    /// The writer epoch comes from the [`Admitted`] proof rather than from the
    /// caller, so a mutation cannot be journalled under an epoch its session was
    /// never granted.
    pub fn begin(
        &mut self,
        admitted: &Admitted,
        request: &MutationRequest,
    ) -> Result<Acknowledged, FacadeError> {
        if admitted.run() != self.run {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                format!(
                    "admission is for run {} but this journal records run {}",
                    admitted.run().0,
                    self.run.0
                ),
            )));
        }

        // Backpressure is applied before the intent is admitted, so an exhausted
        // buffer never lets a mutation reach the wire with nowhere to record it.
        let cost = request.payload.retained_len();
        let pressure = self.log.pressure();
        if pressure.would_exceed(cost) {
            return Err(FacadeError::Replay(ReplayError::CapacityExhausted {
                records: pressure.records,
                bytes: pressure.payload_bytes,
            }));
        }

        let intent = ReplayIntent {
            run_id: self.run,
            operation: request.operation,
            key: request.key.clone(),
            epoch: admitted.epoch(),
            kind: request.kind.clone(),
            payload: request.payload.clone(),
        };
        match self.log.admit(&intent).map_err(FacadeError::Replay)? {
            Admission::Fresh => {
                self.admitted_keys.insert(request.key.clone());
                Ok(Acknowledged::Dispatch(DispatchTicket {
                    run: self.run,
                    operation: request.operation,
                    key: request.key.clone(),
                    epoch: admitted.epoch(),
                }))
            }
            Admission::Recorded(outcome) => Ok(Acknowledged::Replayed(outcome)),
            Admission::Indeterminate => Ok(Acknowledged::Indeterminate),
        }
    }

    /// Settle the committed result for a dispatch.
    ///
    /// Consumes the ticket: one dispatch settles once, and a settled outcome is
    /// never rewritten.
    pub fn settle(
        &mut self,
        ticket: DispatchTicket,
        result: CommittedResult,
    ) -> Result<(), FacadeError> {
        let outcome = match result {
            CommittedResult::Completed(write) => ReplayOutcome::Completed(write),
            CommittedResult::Failed(error) => ReplayOutcome::Failed(RetainedError::record(
                ticket.operation,
                ticket.key.clone(),
                error,
                self.durability.is_durable(),
            )),
        };
        self.log
            .record(&ticket.key, outcome)
            .map_err(FacadeError::Replay)
    }

    /// Read one record back.
    pub fn record(&self, key: &IdempotencyKey) -> Option<&ReplayRecord> {
        self.log.get(key)
    }

    /// Note the verifier a WRITE returned, for later COMMIT comparison.
    pub fn note_write_verifier(&mut self, key: &IdempotencyKey, verifier: WriteVerifier) {
        self.log.note_write_verifier(key, verifier);
    }

    /// Compare a COMMIT verifier against the one recorded for `key`.
    pub fn check_commit_verifier(
        &self,
        key: &IdempotencyKey,
        observed: WriteVerifier,
    ) -> VerifierMatch {
        self.log.check_commit_verifier(key, observed)
    }

    /// Release a settled, durable record and its budget.
    pub fn retire(&mut self, key: &IdempotencyKey) -> Result<(), FacadeError> {
        self.log.retire(key).map_err(FacadeError::Replay)?;
        self.admitted_keys.remove(key);
        Ok(())
    }

    /// How one key stands against the durable evidence.
    pub fn standing(&self, key: &IdempotencyKey) -> KeyStanding {
        let Some(record) = self.log.get(key) else {
            // No intent at all. Whether anything happened is genuinely unknown.
            return KeyStanding::Blocked(FacadeError::Replay(ReplayError::Indeterminate));
        };
        if let Some(outcome) = &record.outcome {
            return KeyStanding::Settled(outcome.clone());
        }
        match (&record.intent.kind, &record.intent.payload) {
            // A write needs its exact bytes. A digest says where they were, not
            // that they are still there.
            (IntentKind::Write { .. }, Payload::Inline(_)) => KeyStanding::Replayable,
            (IntentKind::Write { .. }, _) => {
                KeyStanding::Blocked(FacadeError::Replay(ReplayError::PayloadMissing))
            }
            // A commit and an exclusive create are rebuildable from identity
            // alone: the range and the verifier are both in the intent.
            (IntentKind::Commit { .. } | IntentKind::ExclusiveCreate { .. }, _) => {
                KeyStanding::Replayable
            }
            // A namespace intent records a name, not before/after state. The
            // failure model requires proof or a staging protocol; authority has
            // neither, so it stops instead of approximating one.
            (IntentKind::Namespace { .. }, _) => {
                KeyStanding::Blocked(FacadeError::Replay(ReplayError::Indeterminate))
            }
        }
    }

    /// Sort `keys` into what a bounded replay pass may do.
    ///
    /// The pass is bounded by the log's own record budget: a key set larger than
    /// the buffer could ever have held is not something this journal recorded.
    pub fn plan_recovery<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a IdempotencyKey>,
    ) -> RecoveryPlan {
        let mut plan = RecoveryPlan::default();
        let bound = self.log.pressure().budget.max_records as usize;
        for key in keys {
            if plan.len() >= bound {
                plan.blocked.push((
                    key.clone(),
                    FacadeError::Replay(ReplayError::CapacityExhausted {
                        records: plan.len() as u32,
                        bytes: self.log.pressure().payload_bytes,
                    }),
                ));
                continue;
            }
            match self.standing(key) {
                KeyStanding::Settled(_) => plan.settled.push(key.clone()),
                KeyStanding::Replayable => plan.replayable.push(key.clone()),
                KeyStanding::Blocked(error) => plan.blocked.push((key.clone(), error)),
            }
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::admission::{AdmissionControl, AdmissionRequest};
    use crate::authority::marker::{MemoryMarkerStore, WriterToken};
    use crate::error::ErrorClass;
    use crate::fake::FakeReplayLog;
    use crate::handle::ObjectIdentity;
    use crate::replay::ReplayBudget;
    use crate::transport::{Fsid, Stability};
    use umbra_core::WriterId;
    use uuid::Uuid;

    fn run() -> RunId {
        RunId(Uuid::from_u128(0x11))
    }

    fn admitted() -> Admitted {
        let mut control = AdmissionControl::new(run(), MemoryMarkerStore::empty());
        control
            .acquire(&AdmissionRequest::cooperative(
                WriterId("journal-test".into()),
                WriterToken([1; 16]),
            ))
            .admitted()
            .expect("first acquirer")
    }

    fn object() -> ObjectIdentity {
        ObjectIdentity {
            fsid: Fsid { major: 1, minor: 2 },
            fileid: 7,
        }
    }

    fn write_request(key: &str, payload: Payload) -> MutationRequest {
        MutationRequest {
            operation: OperationId(Uuid::from_u128(0xABCD)),
            key: IdempotencyKey(key.into()),
            kind: IntentKind::Write {
                object: object(),
                offset: 0,
                stability: Stability::Unstable,
            },
            payload,
        }
    }

    fn journal(budget: ReplayBudget) -> MutationJournal<FakeReplayLog> {
        MutationJournal::new(run(), FakeReplayLog::new(budget), Durability::Volatile)
    }

    fn default_budget() -> ReplayBudget {
        ReplayBudget {
            max_records: 8,
            max_payload_bytes: 4096,
        }
    }

    #[test]
    fn the_intent_is_recorded_before_the_ticket_exists() {
        let mut journal = journal(default_budget());
        let admitted = admitted();
        let request = write_request("k1", Payload::Inline(b"bytes".to_vec()));
        let Acknowledged::Dispatch(ticket) = journal
            .begin(&admitted, &request)
            .expect("fresh intent is admitted")
        else {
            panic!("expected a dispatch ticket");
        };
        let record = journal.record(ticket.key()).expect("intent is recorded");
        assert_eq!(record.intent.epoch, admitted.epoch());
        assert_eq!(record.intent.payload.bytes(), Some(&b"bytes"[..]));
        assert!(record.outcome.is_none(), "nothing settled before dispatch");
    }

    #[test]
    fn a_settled_key_replays_its_recorded_answer_including_a_failure() {
        let mut journal = journal(default_budget());
        let admitted = admitted();
        let request = write_request("k1", Payload::Inline(b"bytes".to_vec()));
        let Acknowledged::Dispatch(ticket) = journal.begin(&admitted, &request).expect("admit")
        else {
            panic!("expected a ticket");
        };
        let enospc = FacadeError::protocol(
            crate::error::Nfs4Status::NOSPC,
            crate::transport::OpCode::Write,
            1,
        );
        journal
            .settle(ticket, CommittedResult::Failed(enospc.clone()))
            .expect("settle");

        let Acknowledged::Replayed(ReplayOutcome::Failed(retained)) =
            journal.begin(&admitted, &request).expect("retry")
        else {
            panic!("a settled key must replay its recorded answer");
        };
        assert_eq!(retained.error(), &enospc);
        assert_eq!(
            retained.error().status(),
            Some(crate::error::Nfs4Status::NOSPC),
            "the verbatim status survives the round trip"
        );
        assert!(
            !retained.is_durable(),
            "an in-memory log must not claim durability"
        );
    }

    #[test]
    fn backpressure_refuses_before_the_intent_is_admitted() {
        let mut journal = journal(ReplayBudget {
            max_records: 8,
            max_payload_bytes: 4,
        });
        let admitted = admitted();
        let error = journal
            .begin(
                &admitted,
                &write_request("big", Payload::Inline(vec![0; 5])),
            )
            .expect_err("an oversized payload must be refused");
        assert!(matches!(
            error,
            FacadeError::Replay(ReplayError::CapacityExhausted { .. })
        ));
        assert_eq!(error.class(), ErrorClass::Retriable);
        assert_eq!(
            journal.record(&IdempotencyKey("big".into())),
            None,
            "the refused intent must not have been recorded"
        );
    }

    #[test]
    fn a_digest_only_write_is_blocked_and_never_reconstructed() {
        let mut journal = journal(default_budget());
        let admitted = admitted();
        let request = write_request(
            "digest",
            Payload::Digest {
                digest: [0; 32],
                len: 64,
                source: "somewhere else".into(),
            },
        );
        journal.begin(&admitted, &request).expect("admit");
        let plan = journal.plan_recovery(journal.admitted_keys.iter());
        assert_eq!(plan.replayable, Vec::new());
        assert_eq!(plan.blocked.len(), 1);
        assert!(matches!(
            plan.blocked[0].1,
            FacadeError::Replay(ReplayError::PayloadMissing)
        ));
        assert_eq!(plan.blocked[0].1.class(), ErrorClass::SafeStop);
    }

    #[test]
    fn an_inline_write_is_replayable_after_a_lost_reply() {
        let mut journal = journal(default_budget());
        let admitted = admitted();
        journal
            .begin(
                &admitted,
                &write_request("k", Payload::Inline(vec![1, 2, 3])),
            )
            .expect("admit");
        let plan = journal.plan_recovery(journal.admitted_keys.iter());
        assert_eq!(plan.replayable, vec![IdempotencyKey("k".into())]);
        assert!(plan.is_clean());
    }

    #[test]
    fn an_admission_for_another_run_cannot_journal_here() {
        let mut journal = MutationJournal::new(
            RunId(Uuid::from_u128(0x22)),
            FakeReplayLog::new(default_budget()),
            Durability::Volatile,
        );
        let error = journal
            .begin(&admitted(), &write_request("k", Payload::None))
            .expect_err("cross-run admission must be refused");
        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::IdentityUnproven(_))
        ));
    }
}
