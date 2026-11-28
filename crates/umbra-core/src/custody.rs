//! Managed-run custody records persisted before runtime acquisition is authorized.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ErrorKind, LeaseEpoch, Result, RunId, UmbraError};

/// Terminal evidence checked before preparing a managed namespace finish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Root outcome committed with the namespace. `i32::MIN` denotes a
    /// supervisor failure without an observed root outcome; it does not assert
    /// a tracee exit status.
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
        if matches!(&record.state, CustodyPhase::Failed { reason } if reason.trim().is_empty()) {
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
                if !prior.recovery_only()
                    && epoch.0 > prior.writer_epoch.0
                    && record.generation != prior.record.generation
                {
                    // A completed invocation remains history; a later writer may
                    // start a distinct generation with the initial Armed contract.
                    return Self::transition(None, run_id, epoch, record);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_empty_failure_is_rejected_and_valid_replay_is_idempotent() {
        let run = RunId(Uuid::new_v4());
        let epoch = LeaseEpoch(1);
        let armed = CustodyRecord {
            run_id: run,
            generation: Uuid::new_v4(),
            sequence: 1,
            state: CustodyPhase::Armed {
                capacity: 64,
                startup_nonce: Uuid::new_v4(),
                participants: vec![CustodyParticipant { pid: 1, birth: 1 }],
            },
        };
        let prior = CustodyState::transition(None, run, epoch, &armed).unwrap();
        for reason in ["", " \n\t"] {
            let failed = CustodyRecord {
                sequence: 2,
                state: CustodyPhase::Failed {
                    reason: reason.into(),
                },
                ..armed.clone()
            };
            let bytes = serde_json::to_vec(&failed).unwrap();
            let decoded = serde_json::from_slice(&bytes).unwrap();
            assert!(CustodyState::transition(Some(&prior), run, epoch, &decoded).is_err());
        }
        let failed = CustodyRecord {
            sequence: 2,
            state: CustodyPhase::Failed {
                reason: "provider collection failed".into(),
            },
            ..armed
        };
        let terminal = CustodyState::transition(Some(&prior), run, epoch, &failed).unwrap();
        assert_eq!(
            CustodyState::transition(Some(&terminal), run, epoch, &failed).unwrap(),
            terminal
        );
    }
}
