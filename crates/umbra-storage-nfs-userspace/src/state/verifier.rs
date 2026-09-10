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

/// MurmurHash3's 64-bit finaliser, `fmix64`.
///
/// Fixed, published constants and no state, so the output is stable for the life
/// of the format rather than for the life of a build. This is deliberately *not*
/// `DefaultHasher`, whose algorithm is explicitly allowed to change between Rust
/// releases: a create verifier that changed under a compiler upgrade would turn
/// every outstanding interrupted create into a false collision.
const fn fmix64(mut z: u64) -> u64 {
    z ^= z >> 33;
    z = z.wrapping_mul(0xff51_afd7_ed55_8ccd);
    z ^= z >> 33;
    z = z.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    z ^= z >> 33;
    z
}

/// Arbitrary odd constant separating the two halves' contributions, so that
/// swapping them is not a no-op.
const HALF_SEPARATOR: u64 = 0x9e37_79b9_7f4a_7c15;

/// Derive the `EXCLUSIVE4` create verifier for one operation identity.
///
/// The derivation is deterministic and total: the same [`OperationId`] always
/// yields the same eight bytes. That is what lets a replayed create be recognised
/// without consulting any storage.
///
/// **F21.** It is *not* injective, and the previous doc comment's "two distinct
/// ids yield distinct bytes" could not be true of any function from 128 bits to
/// 64. The old fold made that far worse than the pigeonhole bound requires: it
/// XORed byte `i` of each half together, so every pair of ids differing only by a
/// swap of paired bytes collided outright. Operation ids `1` and `1 << 64` — one
/// bit set in each half, at paired positions — both produced
/// `[0, 0, 0, 0, 0, 0, 0, 1]`, and `fake.rs` treats equal exclusive verifiers as
/// the same create, so one operation could adopt another's object.
///
/// The halves are now mixed rather than folded: the high half goes through a full
/// avalanche before the low half is introduced, so a swap is not a no-op and a
/// single flipped input bit changes about half the output bits. Collisions still
/// exist — 2^64 outputs for 2^128 inputs — and the probability of two random ids
/// colliding is about 2^-64, which is a bound this module documents rather than a
/// uniqueness it claims. What matters for correctness is that a collision now
/// needs coincidence rather than structure.
///
/// **Migration.** A verifier derived by an older build is not reproduced by this
/// one. [`CreateVerifierLedger`] retains a key's verifier once it has been used
/// and `authority_recovery` persists that ledger, so a create whose verifier
/// reached durable storage still replays exactly. A create interrupted *before*
/// its verifier was persisted will present the new derivation on retry, the
/// server will not recognise it, and the reply is `NFS4ERR_EXIST` — which
/// `storage::execute` latches as a safe give-up with the status retained. That is
/// a stop, not a false success, and it is the conservative direction.
pub fn create_verifier_for(operation: OperationId) -> Verifier {
    let bytes = *operation.0.as_bytes();
    let high = u64::from_be_bytes(bytes[..8].try_into().expect("eight bytes"));
    let low = u64::from_be_bytes(bytes[8..].try_into().expect("eight bytes"));
    let mixed = fmix64(high ^ HALF_SEPARATOR);
    Verifier(fmix64(mixed ^ low).to_be_bytes())
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

    /// **F21.** The structured collision the old fold produced is gone.
    ///
    /// `create_verifier_for` XORed byte `i` of each half of the operation id
    /// together, so any two ids differing only by a swap of paired bytes produced
    /// identical verifiers. Ids `1` and `1 << 64` both derived
    /// `[0, 0, 0, 0, 0, 0, 0, 1]`, and `fake.rs` — like a real server — treats
    /// equal `EXCLUSIVE4` verifiers as the same create, so one operation could
    /// adopt another's object.
    ///
    /// (CodeRabbit's own example, `1` versus `1 << 56`, does *not* collide under
    /// the old fold: those bits land in different output bytes. It is recorded
    /// here so the effective control is not confused with the ineffective one.)
    #[test]
    fn f21_paired_byte_swaps_no_longer_collide() {
        let one = OperationId(Uuid::from_u128(1));
        let paired = OperationId(Uuid::from_u128(1 << 64));
        assert_ne!(
            create_verifier_for(one),
            create_verifier_for(paired),
            "1 and 1<<64 are the reproduced collision and must now differ"
        );

        // The whole family the fold collapsed: one bit in each half, at paired
        // byte positions.
        for bit in 0..64u32 {
            let low = OperationId(Uuid::from_u128(1u128 << bit));
            let high = OperationId(Uuid::from_u128(1u128 << (bit + 64)));
            assert_ne!(
                create_verifier_for(low),
                create_verifier_for(high),
                "bit {bit} in each half must not fold to the same verifier"
            );
        }

        // And the derivation still separates ids that differ anywhere at all.
        let mut seen = std::collections::BTreeSet::new();
        for n in 1..512u128 {
            for shift in [0u32, 32, 64, 96] {
                let id = OperationId(Uuid::from_u128(n << shift));
                assert!(
                    seen.insert(create_verifier_for(id).0),
                    "{n} << {shift} collided within a small sample"
                );
            }
        }
    }

    /// **F21.** The derivation is pinned to exact bytes, so a later change cannot
    /// silently invalidate every outstanding interrupted create.
    ///
    /// Uniqueness is *not* claimed: 2^128 identities cannot map injectively onto
    /// 2^64 verifiers, and the module documents the collision probability rather
    /// than promising there is none. What is claimed is that these inputs produce
    /// these bytes, for the life of the format.
    /// Transcribed from the derivation, not recomputed by it: changing the mixer
    /// must fail this test rather than move with it.
    const PINNED_ZERO: [u8; 8] = [99, 147, 213, 28, 6, 198, 24, 220];
    const PINNED_ONE: [u8; 8] = [141, 130, 117, 19, 153, 245, 93, 84];
    const PINNED_PAIRED: [u8; 8] = [58, 60, 127, 138, 209, 103, 98, 90];

    #[test]
    fn f21_the_derivation_is_pinned_to_fixed_vectors() {
        assert_eq!(
            create_verifier_for(OperationId(Uuid::from_u128(0))).0,
            PINNED_ZERO,
            "the all-zero identity's verifier is part of the on-the-wire format"
        );
        assert_eq!(
            create_verifier_for(OperationId(Uuid::from_u128(1))).0,
            PINNED_ONE
        );
        assert_eq!(
            create_verifier_for(OperationId(Uuid::from_u128(1 << 64))).0,
            PINNED_PAIRED
        );
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
