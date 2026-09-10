//! Retained-error tracking: a settled failure is the answer for its key forever.
//!
//! The frozen error facade already provides [`RetainedError`], which binds a
//! [`FacadeError`] to an operation identity and an idempotency key and records
//! whether it reached a persistence boundary. What that type does not do — and
//! deliberately so — is decide *which* failures deserve retaining, or what
//! happens when a second one arrives for the same key. Those are protocol-state
//! decisions, and they live here.
//!
//! # The three rules
//!
//! 1. **Durable beats everything.** Once a durable error is recorded for a key,
//!    it is that key's answer. A later attempt does not overwrite it and does not
//!    get to disagree with it; new work needs a new operation identity. Trying to
//!    replace one is [`ReplayError::KeyConflict`], not a silent update.
//! 2. **Volatile records are provisional.** The fake replay log marks its
//!    retained errors non-durable on purpose, so a consumer that gates on
//!    durability behaves the same against the fake as against a durable log. A
//!    volatile record may be upgraded by a durable one for the same key.
//! 3. **The original status survives.** Whatever is retained keeps its verbatim
//!    `NFS4ERR_*` word. Nothing is folded into a neighbour, promoted to success,
//!    or rewritten as a transport error on the way through — which is what makes
//!    a retained error usable as evidence rather than as a hint.
//!
//! # What is worth retaining
//!
//! [`is_settled`] answers that: a failure is retainable only when repeating the
//! identical request cannot produce a different answer. A `NFS4ERR_DELAY` or a
//! full queue is not settled and must not be frozen into a key, or a transient
//! condition becomes a permanent one.

use std::collections::BTreeMap;

use umbra_core::{IdempotencyKey, OperationId};

use crate::error::{ErrorClass, FacadeError, Nfs4Status, ReplayError, RetainedError};

/// Whether a failure is settled: retrying the identical request cannot change it.
///
/// Only [`ErrorClass::Permanent`] qualifies. `Retriable` plainly does not;
/// `NeedsRecovery` becomes answerable again once state is re-established; and
/// `SafeStop` means the answer is unknown, which is precisely the thing that must
/// not be frozen into a key as though it were known.
pub fn is_settled(error: &FacadeError) -> bool {
    matches!(error.class(), ErrorClass::Permanent)
}

/// Retained errors indexed by the key each one settles.
#[derive(Clone, Debug, Default)]
pub struct RetainedErrorLedger {
    entries: BTreeMap<IdempotencyKey, RetainedError>,
}

impl RetainedErrorLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain `error` as the answer for `key`.
    ///
    /// Refuses with [`ReplayError::KeyConflict`] when a durable record already
    /// stands for the key and this is not a byte-identical restatement of it. A
    /// volatile record may be replaced, including by a durable record for the
    /// same failure, which is how a write-through to a durable log is reflected.
    ///
    /// **F19.** Refuses with [`ReplayError::Indeterminate`] when a *durable*
    /// record is asked for a failure that is not settled. This module's own
    /// definition of settled is [`is_settled`] — only [`ErrorClass::Permanent`] —
    /// and nothing enforced it: `durable: true` wrote a permanent answer for an
    /// `NFS4ERR_DELAY`, a lost connection or a safe stop, and every later retry of
    /// that key read back a failure the server never made final. A transient
    /// condition may still be retained; it is retained *volatile*, which is what
    /// it is.
    ///
    /// The refusal happens before insertion, so a rejected durable retain leaves
    /// the ledger exactly as it was — it does not downgrade to volatile behind the
    /// caller's back, and it does not overwrite a record that already stands.
    pub fn retain(
        &mut self,
        operation: OperationId,
        key: &IdempotencyKey,
        error: FacadeError,
        durable: bool,
    ) -> Result<&RetainedError, ReplayError> {
        if durable && !is_settled(&error) {
            return Err(ReplayError::Indeterminate);
        }
        if let Some(existing) = self.entries.get(key) {
            if existing.is_durable() && (existing.error() != &error || !durable) {
                return Err(ReplayError::KeyConflict);
            }
        }
        let retained = RetainedError::record(operation, key.clone(), error, durable);
        self.entries.insert(key.clone(), retained);
        Ok(self
            .entries
            .get(key)
            .expect("the record just inserted is present"))
    }

    /// The settled answer for `key`, if there is one.
    pub fn get(&self, key: &IdempotencyKey) -> Option<&RetainedError> {
        self.entries.get(key)
    }

    /// The verbatim `NFS4ERR_*` retained for `key`, if the failure came from the
    /// server.
    ///
    /// This is the assertion the fault matrix makes: a status that went into the
    /// ledger comes back out as the same number, not as a classification of it.
    pub fn status(&self, key: &IdempotencyKey) -> Option<Nfs4Status> {
        self.entries.get(key).and_then(|e| e.error().status())
    }

    /// Whether a durable *settled* answer stands for `key`.
    ///
    /// **F19.** Both halves are asked. The durability bit says the record reached
    /// a persistence boundary; it says nothing about whether the failure it holds
    /// is final, and reading it alone reported "this key is settled durably" for
    /// any transient condition that had been written durably. `retain` now refuses
    /// to create such a record, and this asks the second question anyway, so a
    /// record predating the check — or reconstructed from a log written by an
    /// older build — cannot be read as settled either.
    pub fn is_durably_settled(&self, key: &IdempotencyKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.is_durable() && is_settled(entry.error()))
    }

    /// Release a key whose record has been retired from the replay log.
    ///
    /// Only a caller that has retired the durable record may do this; the ledger
    /// cannot verify that, so the operation is named for what it is rather than
    /// dressed up as a cache eviction.
    pub fn release(&mut self, key: &IdempotencyKey) -> Option<RetainedError> {
        self.entries.remove(key)
    }

    /// Number of retained answers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every retained answer, for a caller persisting or auditing the set.
    pub fn iter(&self) -> impl Iterator<Item = (&IdempotencyKey, &RetainedError)> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{AuthorityError, TransportError};
    use crate::transport::OpCode;
    use uuid::Uuid;

    fn key(name: &str) -> IdempotencyKey {
        IdempotencyKey(name.into())
    }

    fn operation(n: u128) -> OperationId {
        OperationId(Uuid::from_u128(n))
    }

    #[test]
    fn a_retained_status_comes_back_as_the_same_number() {
        let mut ledger = RetainedErrorLedger::new();
        let error = FacadeError::protocol(Nfs4Status::NOSPC, OpCode::Write, 1);
        ledger
            .retain(operation(1), &key("write-1"), error, true)
            .unwrap();
        assert_eq!(ledger.status(&key("write-1")), Some(Nfs4Status::NOSPC));
        assert_eq!(ledger.status(&key("write-1")).unwrap().0, 28);
    }

    #[test]
    fn an_unrecognised_status_is_retained_verbatim_too() {
        let mut ledger = RetainedErrorLedger::new();
        let unknown = Nfs4Status(65_535);
        ledger
            .retain(
                operation(1),
                &key("odd"),
                FacadeError::protocol(unknown, OpCode::Open, 0),
                true,
            )
            .unwrap();
        assert_eq!(ledger.status(&key("odd")), Some(unknown));
    }

    #[test]
    fn a_durable_answer_is_final_for_its_key() {
        let mut ledger = RetainedErrorLedger::new();
        let settled = FacadeError::protocol(Nfs4Status::EXIST, OpCode::Open, 1);
        ledger
            .retain(operation(1), &key("create"), settled.clone(), true)
            .unwrap();
        // A different answer for a settled key is refused rather than applied.
        assert_eq!(
            ledger.retain(
                operation(2),
                &key("create"),
                FacadeError::protocol(Nfs4Status::NOSPC, OpCode::Open, 1),
                true
            ),
            Err(ReplayError::KeyConflict)
        );
        // Restating the identical answer is allowed and changes nothing.
        assert!(ledger
            .retain(operation(1), &key("create"), settled, true)
            .is_ok());
        assert_eq!(ledger.status(&key("create")), Some(Nfs4Status::EXIST));
    }

    #[test]
    fn a_volatile_record_may_be_upgraded_to_a_durable_one() {
        let mut ledger = RetainedErrorLedger::new();
        let error = FacadeError::protocol(Nfs4Status::ACCESS, OpCode::Open, 1);
        ledger
            .retain(operation(1), &key("k"), error.clone(), false)
            .unwrap();
        assert!(!ledger.is_durably_settled(&key("k")));
        ledger.retain(operation(1), &key("k"), error, true).unwrap();
        assert!(ledger.is_durably_settled(&key("k")));
        // And once durable, it cannot be demoted back.
        assert!(ledger
            .retain(
                operation(1),
                &key("k"),
                FacadeError::protocol(Nfs4Status::ACCESS, OpCode::Open, 1),
                false
            )
            .is_err());
    }

    /// **F19.** A durable record is refused for a failure that is not settled.
    ///
    /// `is_settled` was this module's own definition of the word and nothing
    /// enforced it: `durable: true` wrote a permanent answer for an
    /// `NFS4ERR_DELAY` — a condition the server explicitly asks the client to
    /// retry — for a lost connection, or for a safe stop, and every later retry
    /// of that key read back a failure the server never made final.
    #[test]
    fn f19_a_transient_failure_is_never_retained_as_a_durable_answer() {
        for transient in [
            // Retriable: the server asked for a retry.
            FacadeError::protocol(Nfs4Status::DELAY, OpCode::Write, 1),
            FacadeError::protocol(Nfs4Status::GRACE, OpCode::Open, 0),
            // NeedsRecovery: answerable again once state is re-established.
            FacadeError::Transport(TransportError::Connect("lost".into())),
            FacadeError::Replay(ReplayError::VerifierChanged {
                recorded: [1; 8],
                observed: [2; 8],
            }),
            // SafeStop: the answer is unknown, which is exactly what must not be
            // frozen into a key as though it were known.
            FacadeError::Authority(AuthorityError::TakeoverRefused),
            FacadeError::Replay(ReplayError::Indeterminate),
        ] {
            let mut ledger = RetainedErrorLedger::new();
            assert_eq!(
                ledger.retain(operation(1), &key("t"), transient.clone(), true),
                Err(ReplayError::Indeterminate),
                "{transient:?} is not settled and must not become a durable answer"
            );
            assert!(
                ledger.get(&key("t")).is_none(),
                "a refused durable retain leaves the ledger untouched: {transient:?}"
            );

            // Volatile is what it is, and that is still allowed.
            ledger
                .retain(operation(1), &key("t"), transient.clone(), false)
                .expect("a transient failure may be retained volatile");
            assert!(
                !ledger.is_durably_settled(&key("t")),
                "and never reports itself durably settled: {transient:?}"
            );
            assert_eq!(ledger.status(&key("t")), transient.status());
        }
    }

    /// **F19.** A key whose volatile record is transient can still settle later,
    /// and the F10 invariant is untouched: a settled record is never overwritten.
    #[test]
    fn f19_a_transient_key_can_still_settle_afterwards() {
        let mut ledger = RetainedErrorLedger::new();
        let transient = FacadeError::protocol(Nfs4Status::DELAY, OpCode::Write, 1);
        ledger
            .retain(operation(1), &key("k"), transient, false)
            .expect("volatile");
        assert!(!ledger.is_durably_settled(&key("k")));

        let settled = FacadeError::protocol(Nfs4Status::NOSPC, OpCode::Write, 1);
        ledger
            .retain(operation(2), &key("k"), settled.clone(), true)
            .expect("a genuinely settled answer may replace a volatile one");
        assert!(ledger.is_durably_settled(&key("k")));
        assert_eq!(ledger.status(&key("k")), Some(Nfs4Status::NOSPC));

        // And it is final: neither a different answer nor a demotion is taken.
        assert_eq!(
            ledger.retain(
                operation(3),
                &key("k"),
                FacadeError::protocol(Nfs4Status::EXIST, OpCode::Write, 1),
                true
            ),
            Err(ReplayError::KeyConflict)
        );
        assert_eq!(
            ledger.retain(operation(3), &key("k"), settled, false),
            Err(ReplayError::KeyConflict)
        );
        assert_eq!(ledger.status(&key("k")), Some(Nfs4Status::NOSPC));
    }

    #[test]
    fn only_a_permanent_failure_is_settled() {
        assert!(is_settled(&FacadeError::protocol(
            Nfs4Status::EXIST,
            OpCode::Open,
            0
        )));
        assert!(
            !is_settled(&FacadeError::protocol(Nfs4Status::GRACE, OpCode::Open, 0)),
            "a retriable status must never be frozen into a key"
        );
        assert!(!is_settled(&FacadeError::protocol(
            Nfs4Status::NO_GRACE,
            OpCode::Open,
            0
        )));
        assert!(!is_settled(&FacadeError::Authority(
            AuthorityError::IdentityUnproven("unknown".into())
        )));
        assert!(!is_settled(&FacadeError::Transport(
            TransportError::QueueFull {
                depth: 1,
                capacity: 1
            }
        )));
    }
}
