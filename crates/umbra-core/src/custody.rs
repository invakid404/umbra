//! Dormant managed-run custody records. No runtime path arms these yet.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ErrorKind, LeaseEpoch, Result, RunId, UmbraError};

/// Terminal evidence checked before preparing a managed namespace finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustodyFinishProof {
    /// Exact generation being finalized.
    pub generation: Uuid,
    /// Digest of the ordered terminal receipt and resource inventory.
    pub certificate: [u8; 32],
    /// Validated ledger Final, including all prior control failures.
    pub ledger_final: bool,
    /// Actual provider collection by its parent.
    pub provider_collected: bool,
    /// All creators reached a terminal checkpoint.
    pub creators_terminal: bool,
    /// No failed or unsettled cleanup remains.
    pub cleanup_confirmed: bool,
    /// Root outcome committed with the namespace.
    pub root_result: i32,
}

/// Identity retained for later collection proof; a PID alone is insufficient.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyParticipant {
    /// Process identifier.
    pub pid: u32,
    /// Platform birth identity.
    pub birth: u64,
}

/// Durable custody phase, distinct from the irreversible recovery failure latch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustodyPhase {
    /// Persisted before any managed acquisition is permitted.
    Armed {
        /// Negotiated simultaneous obligation capacity.
        capacity: u64,
        /// Startup exchange identity.
        startup_nonce: Uuid,
        /// Participants whose identities were validated before startup.
        participants: Vec<CustodyParticipant>,
    },
    /// Namespace preparation retains writer authority.
    FinishPrepared {
        /// Digest identifying the exact validated terminal certificate.
        certificate: [u8; 32],
        /// Root outcome included in the prepared decision.
        root_result: i32,
    },
    /// Durable commit of the exact prepared certificate.
    Complete {
        /// Must equal the prepared digest.
        certificate: [u8; 32],
    },
    /// Sticky failure; later disappearance cannot turn this into success.
    Failed {
        /// Attributable diagnostic.
        reason: String,
    },
}

/// A custody transition within the authoritative journal's writer epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyRecord {
    /// Stable run identity.
    pub run_id: RunId,
    /// Random generation identity.
    pub generation: Uuid,
    /// Consecutive custody sequence, starting at one.
    pub sequence: u64,
    /// Committed phase.
    pub state: CustodyPhase,
}

/// Replayed custody authority. Epoch changes do not erase unresolved custody.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyState {
    /// Epoch which armed this generation.
    pub writer_epoch: LeaseEpoch,
    /// Latest accepted transition.
    pub record: CustodyRecord,
}

impl CustodyState {
    /// Nonterminal and failed custody permits inspection only.
    pub fn recovery_only(&self) -> bool {
        !matches!(self.record.state, CustodyPhase::Complete { .. })
    }

    /// Validate a transition without modifying the prior state on error.
    /// Exact duplicate receipts are idempotent; conflicting or stale ones fail.
    pub fn transition(
        previous: Option<&Self>,
        run_id: RunId,
        epoch: LeaseEpoch,
        record: &CustodyRecord,
    ) -> Result<Self> {
        let invalid = || {
            UmbraError::new(
                ErrorKind::InvalidState,
                "custody.transition",
                "invalid custody run, epoch, generation, sequence or phase",
            )
        };
        if record.run_id != run_id || record.generation.is_nil() {
            return Err(invalid());
        }
        match previous {
            None => {
                if record.sequence != 1 {
                    return Err(invalid());
                }
                match &record.state {
                    CustodyPhase::Armed {
                        capacity,
                        startup_nonce,
                        participants,
                    } if *capacity > 0
                        && !startup_nonce.is_nil()
                        && !participants.is_empty()
                        && participants.iter().all(|p| p.pid > 0 && p.birth > 0) => {}
                    _ => return Err(invalid()),
                }
            }
            Some(prior) => {
                if prior.writer_epoch != epoch || prior.record.generation != record.generation {
                    return Err(invalid());
                }
                if prior.record == *record {
                    return Ok(prior.clone());
                }
                if prior.record.sequence.checked_add(1) != Some(record.sequence) {
                    return Err(invalid());
                }
                match (&prior.record.state, &record.state) {
                    (CustodyPhase::Armed { .. }, CustodyPhase::FinishPrepared { .. })
                    | (
                        CustodyPhase::Armed { .. } | CustodyPhase::FinishPrepared { .. },
                        CustodyPhase::Failed { .. },
                    ) => {}
                    (
                        CustodyPhase::FinishPrepared {
                            certificate: prepared,
                            ..
                        },
                        CustodyPhase::Complete { certificate },
                    ) if prepared == certificate => {}
                    _ => return Err(invalid()),
                }
            }
        }
        Ok(Self {
            writer_epoch: epoch,
            record: record.clone(),
        })
    }
}
