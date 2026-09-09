//! Replay facade: durable intent identity, verifier accounting and backpressure.
//!
//! The failure model requires that enough payload and identity be persisted
//! *before* a mutation is dispatched to rebuild its outcome without the RPC reply
//! cache, that the replay buffer be bounded, and that exhaustion apply
//! backpressure **before dispatch** rather than after. This facade is the seam
//! `authority_recovery` builds that on.
//!
//! Two rules shape the types:
//!
//! * A recorded outcome is final for its key. [`Admission::Recorded`] returns it
//!   verbatim, including a recorded failure, so a retry cannot be reinterpreted.
//! * A digest is not a payload. [`Payload::Digest`] exists so a caller can say
//!   "the bytes live somewhere else that is guaranteed to survive"; a recovery
//!   that needs bytes and finds only a digest fails with
//!   [`ReplayError::PayloadMissing`] instead of inventing them.

use serde::{Deserialize, Serialize};
use umbra_core::{IdempotencyKey, LeaseEpoch, OperationId, RunId};

use crate::error::{FacadeError, ReplayError, RetainedError};
use crate::handle::ObjectIdentity;
use crate::transport::{Stability, WriteVerifier};

/// What a durable intent describes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentKind {
    /// A positional write. Recovery needs the exact bytes and range.
    Write {
        /// Object the write targets.
        object: ObjectIdentity,
        /// Absolute offset.
        offset: u64,
        /// Requested stability.
        stability: Stability,
    },
    /// A commit of a byte range.
    Commit {
        /// Object the commit targets.
        object: ObjectIdentity,
        /// Absolute offset.
        offset: u64,
        /// Byte count; zero means to end of file.
        count: u32,
    },
    /// A namespace mutation. Never resubmitted under a new identity merely
    /// because its reply was lost.
    Namespace {
        /// Operation name for diagnostics, byte-exact.
        operation: String,
    },
    /// An exclusive create, whose verifier is its whole idempotency story.
    ExclusiveCreate {
        /// Verifier sent with `EXCLUSIVE4`.
        verifier: [u8; 8],
    },
}

/// Payload retention for one intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    /// The bytes themselves, retained by the log.
    Inline(Vec<u8>),
    /// A digest only, valid solely when a surviving payload source is guaranteed.
    /// Recovery that needs bytes and finds this fails rather than guessing.
    Digest {
        /// Digest of the payload.
        digest: [u8; 32],
        /// Byte length the digest covers.
        len: u64,
        /// Who guarantees the bytes still exist. Free text, recorded verbatim.
        source: String,
    },
    /// No payload is needed to rebuild this intent.
    None,
}

impl Payload {
    /// Bytes retained by the log, if any.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Inline(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Bytes this payload costs against the buffer budget.
    pub fn retained_len(&self) -> u64 {
        match self {
            Self::Inline(bytes) => bytes.len() as u64,
            Self::Digest { .. } | Self::None => 0,
        }
    }
}

/// A durable intent recorded before dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayIntent {
    /// Run this intent belongs to.
    pub run_id: RunId,
    /// Operation identity. Reusing it for a different key is an error.
    pub operation: OperationId,
    /// Idempotency key. The identity a retry is matched on.
    pub key: IdempotencyKey,
    /// Writer epoch that authorised the mutation.
    pub epoch: LeaseEpoch,
    /// What is being attempted.
    pub kind: IntentKind,
    /// Retained payload.
    pub payload: Payload,
}

/// The settled outcome of an intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayOutcome {
    /// Completed. The recorded effect is the answer for this key forever.
    Completed(CompletedWrite),
    /// Failed, with the error retained verbatim for this key.
    Failed(RetainedError),
}

/// What a completed mutation actually achieved.
///
/// A short count and a weaker-than-requested stability are recorded as they
/// happened; neither is rounded up to the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedWrite {
    /// Bytes the server accepted.
    pub count: u32,
    /// Stability the server actually reached.
    pub committed: Stability,
    /// Verifier the server returned, for later COMMIT comparison.
    pub verifier: Option<WriteVerifier>,
}

/// A recorded intent with, once known, its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayRecord {
    /// The intent as it was recorded before dispatch.
    pub intent: ReplayIntent,
    /// The outcome, absent while the operation is still in flight or unknown.
    pub outcome: Option<ReplayOutcome>,
}

/// What `admit` decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// A fresh intent was recorded; dispatch may proceed.
    Fresh,
    /// This key already has a settled outcome. Return it; do not dispatch.
    Recorded(ReplayOutcome),
    /// This key was recorded but its outcome is unknown. Dispatching again would
    /// risk repeating a non-idempotent effect, so recovery must settle it first.
    Indeterminate,
}

/// Bounds on the replay buffer. Exhaustion is backpressure, not data loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayBudget {
    /// Maximum records retained.
    pub max_records: u32,
    /// Maximum inline payload bytes retained.
    pub max_payload_bytes: u64,
}

/// Current occupancy of the replay buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backpressure {
    /// Records retained.
    pub records: u32,
    /// Inline payload bytes retained.
    pub payload_bytes: u64,
    /// Configured bounds.
    pub budget: ReplayBudget,
}

impl Backpressure {
    /// Whether a further intent of `payload_len` bytes would exceed the budget.
    pub fn would_exceed(&self, payload_len: u64) -> bool {
        self.records >= self.budget.max_records
            || self.payload_bytes.saturating_add(payload_len) > self.budget.max_payload_bytes
    }
}

/// Verifier accounting across WRITE and COMMIT.
///
/// An unstable WRITE is only durable once a COMMIT returns the same verifier the
/// WRITE did. A changed verifier means the server lost the unstable data and the
/// bytes must be rewritten from a retained payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierMatch {
    /// Same verifier: unstable writes covered by this commit are stable.
    Match,
    /// Verifier changed: the covered writes must be replayed from payload.
    Changed {
        /// Verifier recorded at WRITE time.
        recorded: WriteVerifier,
        /// Verifier observed at COMMIT time.
        observed: WriteVerifier,
    },
    /// No verifier was recorded for the range, so nothing can be concluded.
    Unknown,
}

impl VerifierMatch {
    /// Compare a recorded verifier against one observed at COMMIT.
    pub fn compare(recorded: Option<WriteVerifier>, observed: WriteVerifier) -> Self {
        match recorded {
            None => Self::Unknown,
            Some(recorded) if recorded == observed => Self::Match,
            Some(recorded) => Self::Changed { recorded, observed },
        }
    }

    /// Convert a mismatch into the replay error consumers act on.
    pub fn into_result(self) -> Result<(), FacadeError> {
        match self {
            Self::Match => Ok(()),
            Self::Changed { recorded, observed } => {
                Err(FacadeError::Replay(ReplayError::VerifierChanged {
                    recorded: recorded.0,
                    observed: observed.0,
                }))
            }
            Self::Unknown => Err(FacadeError::Replay(ReplayError::Indeterminate)),
        }
    }
}

/// The durable replay ledger consumed by `authority_recovery`.
///
/// # Invariants an implementation must uphold
///
/// 1. `admit` records the intent durably **before** returning [`Admission::Fresh`],
///    and refuses with [`ReplayError::CapacityExhausted`] before dispatch when the
///    budget is spent.
/// 2. A key with a recorded outcome always returns that outcome, success or
///    failure, and never dispatches again.
/// 3. Reusing a key with a different payload is [`ReplayError::KeyConflict`];
///    reusing an operation id for a different key is
///    [`ReplayError::OperationReused`].
/// 4. `record` is the only way an outcome becomes settled, and a settled outcome
///    is never rewritten.
/// 5. `retire` releases budget only for keys whose outcome is settled and whose
///    effects are durable. An indeterminate record is never dropped to make room.
pub trait ReplayLog: Send {
    /// Admit an intent for dispatch, or return the settled answer for its key.
    fn admit(&mut self, intent: &ReplayIntent) -> Result<Admission, ReplayError>;

    /// Settle the outcome for a key. Recording twice is an error, not an update.
    fn record(&mut self, key: &IdempotencyKey, outcome: ReplayOutcome) -> Result<(), ReplayError>;

    /// Read a record back.
    fn get(&self, key: &IdempotencyKey) -> Option<&ReplayRecord>;

    /// Release a settled, durable record and its budget.
    fn retire(&mut self, key: &IdempotencyKey) -> Result<(), ReplayError>;

    /// Current occupancy, for a caller that wants to slow down before being told.
    fn pressure(&self) -> Backpressure;

    /// Record the verifier a WRITE returned, for later COMMIT comparison.
    fn note_write_verifier(&mut self, key: &IdempotencyKey, verifier: WriteVerifier);

    /// Compare a COMMIT verifier against what was recorded for a key.
    fn check_commit_verifier(&self, key: &IdempotencyKey, observed: WriteVerifier)
        -> VerifierMatch;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_only_payload_retains_no_bytes_and_no_budget() {
        let payload = Payload::Digest {
            digest: [0; 32],
            len: 4096,
            source: "journal record 12".into(),
        };
        assert_eq!(payload.bytes(), None);
        assert_eq!(payload.retained_len(), 0);
        assert_eq!(Payload::Inline(vec![1, 2, 3]).retained_len(), 3);
    }

    #[test]
    fn verifier_comparison_never_guesses() {
        let a = WriteVerifier([1; 8]);
        let b = WriteVerifier([2; 8]);
        assert_eq!(VerifierMatch::compare(Some(a), a), VerifierMatch::Match);
        assert_eq!(
            VerifierMatch::compare(Some(a), b),
            VerifierMatch::Changed {
                recorded: a,
                observed: b
            }
        );
        assert_eq!(VerifierMatch::compare(None, a), VerifierMatch::Unknown);
        assert!(VerifierMatch::compare(None, a).into_result().is_err());
    }

    #[test]
    fn backpressure_trips_on_either_bound() {
        let pressure = Backpressure {
            records: 3,
            payload_bytes: 900,
            budget: ReplayBudget {
                max_records: 4,
                max_payload_bytes: 1000,
            },
        };
        assert!(!pressure.would_exceed(100));
        assert!(pressure.would_exceed(101));
        let full = Backpressure {
            records: 4,
            ..pressure
        };
        assert!(full.would_exceed(0));
    }
}
