//! Verifier accounting: exclusive-create verifiers and WRITE/COMMIT matching.
//!
//! # Two unrelated verifiers, kept apart
//!
//! NFSv4.0 calls both of these `verifier4`, which is exactly why they are
//! separate types here. The **create verifier** ([`Verifier`]) is the whole
//! idempotency story for `EXCLUSIVE4`: reusing it identifies a replay of *my*
//! create, and a different one on an existing name is a genuine collision. The
//! **write verifier** ([`WriteVerifier`]) is the server's storage incarnation:
//! when it changes, unstable data the server acknowledged is gone.
//!
//! # Create verifiers are caller-owned
//!
//! A create verifier is only useful if it survives the crash that made the retry
//! necessary, so this module does not invent one per attempt. It derives the
//! verifier from the caller's [`OperationId`] — deterministic, so the same
//! operation always presents the same verifier, and distinct, so two different
//! creates of the same name really do collide — and the ledger is
//! [`CreateVerifierLedger::export`]/[`CreateVerifierLedger::import`]-able so
//! `authority_recovery` can persist it beside the replay log rather than having
//! protocol state open a file of its own.
//!
//! # WRITE and COMMIT feed the replay facade
//!
//! The comparison itself is [`VerifierMatch`], already frozen in the replay
//! facade. What lives here is the rule about *when* the comparison is owed: a
//! write the server committed at `FILE_SYNC` is stable on arrival and needs no
//! COMMIT, while an `UNSTABLE` write is not durable until a COMMIT returns the
//! same verifier the WRITE did. Recording a verifier for a write that never
//! needed one would invite a later `Unknown` to be read as a failure.

use std::collections::BTreeMap;

use umbra_core::{IdempotencyKey, OperationId};

use crate::replay::{ReplayLog, VerifierMatch};
use crate::transport::{CommitReply, Stability, Verifier, WriteReply, WriteVerifier};

/// Derive the `EXCLUSIVE4` create verifier for one operation identity.
///
/// The derivation is deterministic and total: the same [`OperationId`] always
/// yields the same eight bytes, and two distinct ids yield distinct bytes for as
/// long as the fold below stays injective on the halves it mixes. That is what
/// lets a replayed create be recognised without consulting any storage.
pub fn create_verifier_for(operation: OperationId) -> Verifier {
    let bytes = *operation.0.as_bytes();
    let mut verifier = [0u8; 8];
    for (index, slot) in verifier.iter_mut().enumerate() {
        // Fold the 16-byte identity into 8 bytes by pairing each half's bytes.
        *slot = bytes[index] ^ bytes[index + 8];
    }
    Verifier(verifier)
}

/// Create verifiers retained per idempotency key.
///
/// A retry of the same logical create must present the verifier the first attempt
/// used, or the server will read it as a different create and answer
/// `NFS4ERR_EXIST` for what is actually a replay.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateVerifierLedger {
    entries: BTreeMap<IdempotencyKey, Verifier>,
}

impl CreateVerifierLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// The verifier to present for `key`, deriving and retaining one on first use.
    ///
    /// Once a key has a verifier, that verifier is returned for every later call
    /// regardless of the operation id offered, because changing it mid-retry is
    /// precisely how a replay turns into a false collision.
    pub fn verifier_for(&mut self, key: &IdempotencyKey, operation: OperationId) -> Verifier {
        *self
            .entries
            .entry(key.clone())
            .or_insert_with(|| create_verifier_for(operation))
    }

    /// The retained verifier for `key`, if any.
    pub fn get(&self, key: &IdempotencyKey) -> Option<Verifier> {
        self.entries.get(key).copied()
    }

    /// Forget a key whose create is settled and whose record has been retired.
    pub fn forget(&mut self, key: &IdempotencyKey) -> Option<Verifier> {
        self.entries.remove(key)
    }

    /// Number of retained verifiers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Export the ledger so its owner can persist it.
    ///
    /// Protocol state does not choose a persistence mechanism; that belongs to
    /// `authority_recovery`, which already owns the durable replay log.
    pub fn export(&self) -> Vec<(IdempotencyKey, Verifier)> {
        self.entries
            .iter()
            .map(|(key, verifier)| (key.clone(), *verifier))
            .collect()
    }

    /// Restore an exported ledger after a restart.
    pub fn import(entries: impl IntoIterator<Item = (IdempotencyKey, Verifier)>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
        }
    }
}

/// What a WRITE achieved, and whether it still owes a COMMIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteRecord {
    /// Bytes the server accepted. A short count is recorded as it happened.
    pub count: u32,
    /// Stability the server actually reached, which may be weaker than requested.
    pub committed: Stability,
    /// The verifier to compare at COMMIT time, when one is owed.
    pub verifier: WriteVerifier,
}

impl WriteRecord {
    /// Read a WRITE reply.
    pub fn from_reply(reply: &WriteReply) -> Self {
        Self {
            count: reply.count,
            committed: reply.committed,
            verifier: reply.verifier,
        }
    }

    /// Whether this write is only durable once a COMMIT confirms the verifier.
    pub fn needs_commit(&self) -> bool {
        matches!(self.committed, Stability::Unstable)
    }

    /// Whether the server accepted fewer bytes than were offered.
    pub fn is_short(&self, requested: usize) -> bool {
        (self.count as usize) < requested
    }
}

/// Record a WRITE's verifier against its key, when the write owes a COMMIT.
///
/// Returns whether a verifier was recorded. A `DATA_SYNC` or `FILE_SYNC` write is
/// already stable, so recording one for it would create an expectation of a
/// COMMIT that never comes.
pub fn note_write(log: &mut dyn ReplayLog, key: &IdempotencyKey, record: &WriteRecord) -> bool {
    if !record.needs_commit() {
        return false;
    }
    log.note_write_verifier(key, record.verifier);
    true
}

/// Compare a COMMIT's verifier against what the WRITE recorded for `key`.
///
/// [`VerifierMatch::Unknown`] means nothing was recorded, which is a real answer
/// and not a match: it is the state a caller reaches by committing a range it
/// never wrote unstably, and it must not be read as durability.
pub fn check_commit(
    log: &dyn ReplayLog,
    key: &IdempotencyKey,
    reply: &CommitReply,
) -> VerifierMatch {
    log.check_commit_verifier(key, reply.verifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeReplayLog;
    use uuid::Uuid;

    fn key(name: &str) -> IdempotencyKey {
        IdempotencyKey(name.into())
    }

    #[test]
    fn one_operation_always_derives_the_same_create_verifier() {
        let operation = OperationId(Uuid::from_u128(0x1234_5678_9abc_def0));
        assert_eq!(
            create_verifier_for(operation),
            create_verifier_for(operation)
        );
        let other = OperationId(Uuid::from_u128(0x1234_5678_9abc_def1));
        assert_ne!(create_verifier_for(operation), create_verifier_for(other));
    }

    #[test]
    fn a_retry_of_one_key_presents_the_verifier_the_first_attempt_used() {
        let mut ledger = CreateVerifierLedger::new();
        let first = ledger.verifier_for(&key("create-1"), OperationId(Uuid::from_u128(1)));
        // A retry that happens to carry a fresh operation id must not change the
        // verifier, or the server reads the replay as a colliding create.
        let retry = ledger.verifier_for(&key("create-1"), OperationId(Uuid::from_u128(999)));
        assert_eq!(first, retry);
        let different = ledger.verifier_for(&key("create-2"), OperationId(Uuid::from_u128(2)));
        assert_ne!(first, different);
    }

    #[test]
    fn the_ledger_round_trips_so_its_owner_can_persist_it() {
        let mut ledger = CreateVerifierLedger::new();
        ledger.verifier_for(&key("a"), OperationId(Uuid::from_u128(1)));
        ledger.verifier_for(&key("b"), OperationId(Uuid::from_u128(2)));
        let restored = CreateVerifierLedger::import(ledger.export());
        assert_eq!(restored, ledger);
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn only_an_unstable_write_owes_a_commit_verifier() {
        let mut log = FakeReplayLog::default();
        let unstable = WriteRecord {
            count: 4,
            committed: Stability::Unstable,
            verifier: WriteVerifier([1; 8]),
        };
        assert!(unstable.needs_commit());
        assert!(note_write(&mut log, &key("w1"), &unstable));
        assert_eq!(
            check_commit(
                &log,
                &key("w1"),
                &CommitReply {
                    verifier: WriteVerifier([1; 8])
                }
            ),
            VerifierMatch::Match
        );

        let stable = WriteRecord {
            committed: Stability::FileSync,
            ..unstable
        };
        assert!(!stable.needs_commit());
        assert!(!note_write(&mut log, &key("w2"), &stable));
        assert_eq!(
            check_commit(
                &log,
                &key("w2"),
                &CommitReply {
                    verifier: WriteVerifier([1; 8])
                }
            ),
            VerifierMatch::Unknown,
            "nothing was recorded, so nothing may be concluded"
        );
    }

    #[test]
    fn a_rotated_verifier_reports_the_change_rather_than_durability() {
        let mut log = FakeReplayLog::default();
        let record = WriteRecord {
            count: 8,
            committed: Stability::Unstable,
            verifier: WriteVerifier([0xA1; 8]),
        };
        note_write(&mut log, &key("w"), &record);
        let observed = check_commit(
            &log,
            &key("w"),
            &CommitReply {
                verifier: WriteVerifier([0xFF; 8]),
            },
        );
        assert_eq!(
            observed,
            VerifierMatch::Changed {
                recorded: WriteVerifier([0xA1; 8]),
                observed: WriteVerifier([0xFF; 8])
            }
        );
        assert!(observed.into_result().is_err());
    }

    #[test]
    fn a_short_count_is_recorded_as_it_happened() {
        let record = WriteRecord::from_reply(&WriteReply {
            count: 2,
            committed: Stability::Unstable,
            verifier: WriteVerifier([0; 8]),
        });
        assert!(record.is_short(4));
        assert_eq!(
            record.count, 2,
            "the count is never rounded up to the request"
        );
    }
}
