//! Per-run admission ledger used by the managed cleanup owner. Callers must commit the returned grant
//! before performing an acquisition; this module performs no resource I/O.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Default simultaneous cleanup capacity; never a session lifetime limit.
pub const DEFAULT_CLEANUP_CAPACITY: u64 = 64;

/// Terminal evidence supplied by the collector of an obligation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCertificate {
    /// An acquisition was never entered and cannot subsequently start.
    CancelledBeforeEntry,
    /// An entered creator returned a definitive failure without producing a
    /// resource, and no creation-capable operation remains in flight.
    NoResource {
        /// Identity committed by Entering, even though creation failed.
        resource: Uuid,
        /// The creator reached its terminal checkpoint.
        creator_terminal: bool,
    },
    /// The creator stopped and the identified resource was actually collected.
    Collected {
        /// Stable resource identity assigned when entering.
        resource: Uuid,
        /// Creator terminal checkpoint.
        creator_terminal: bool,
        /// Actual collection, not EOF, disappearance or exit notification.
        parent_collected: bool,
    },
}

/// Serialized ledger operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionOperation {
    /// Charge one obligation and return its grant.
    Reserve {
        /// Unique obligation identity.
        obligation: Uuid,
    },
    /// Commit acquisition entry before any side effect.
    Entering {
        /// Charged obligation.
        obligation: Uuid,
        /// Identity used by later collection evidence.
        resource: Uuid,
    },
    /// Begin retirement without releasing capacity.
    Retiring {
        /// Charged obligation.
        obligation: Uuid,
    },
    /// Release only after terminal evidence has been validated.
    Release {
        /// Charged obligation.
        obligation: Uuid,
        /// Required terminal proof.
        certificate: TerminalCertificate,
    },
    /// Atomically stop further grants and entries.
    Freeze,
}

/// Request identity makes the latest exact retransmission idempotent.
/// Older requests fail as stale, so replay storage is bounded for unlimited runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequest {
    /// Negotiated run nonce.
    pub nonce: Uuid,
    /// Negotiated ledger generation.
    pub generation: u64,
    /// Consecutive transaction number starting at one.
    pub sequence: u64,
    /// Requested transition.
    pub operation: AdmissionOperation,
}

/// Owner acknowledgement of one exact admission transaction. The provider must
/// match the full request before treating the transition as committed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionResponse {
    /// Exact operation and identity applied by the owner.
    pub request: AdmissionRequest,
    /// Committed outstanding count or an attributable refusal.
    pub result: Result<u64, AdmissionError>,
    /// Owner's retained absolute cleanup cutoff, or MAX before cleanup starts.
    pub cutoff: u64,
}

/// Provider-side transport to the authoritative per-run owner ledger.
/// The absolute boot-clock cutoff belongs to the operation, never to inactivity.
pub trait AdmissionTransport: Send {
    /// Return only an acknowledgement of the exact operation. Transport errors
    /// leave the operation ambiguous and must never authorize acquisition.
    fn exchange(
        &mut self,
        operation: AdmissionOperation,
        deadline: u64,
    ) -> std::io::Result<Result<u64, AdmissionError>>;
}

/// Attributable refusal or invalid protocol state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionError {
    /// No acquisition is permitted when capacity is exhausted.
    Capacity {
        /// Negotiated maximum.
        capacity: u64,
        /// Outstanding obligations, including retirement.
        current: u64,
    },
    /// Admission was frozen.
    Frozen,
    /// Invalid identity, sequence, transition, proof or arithmetic.
    Invalid,
}

#[derive(Clone, Debug)]
struct Obligation {
    resource: Option<Uuid>,
    retiring: bool,
}

/// Pure state machine, one instance per run and no shared state.
/// Mutating access serializes freeze with grants. Errors never release a charge.
#[derive(Debug)]
pub struct AdmissionLedger {
    nonce: Uuid,
    generation: u64,
    capacity: u64,
    frozen: bool,
    failed: bool,
    obligations: BTreeMap<Uuid, Obligation>,
    last: Option<(AdmissionRequest, Result<u64, AdmissionError>)>,
}

impl AdmissionLedger {
    /// Freeze directly in the owner's serialized event loop before cancellation.
    /// Already charged and entered resources remain outstanding.
    pub fn freeze(&mut self) -> u64 {
        self.frozen = true;
        self.current()
    }

    /// Negotiate a nonzero capacity and fresh run identity without side effects.
    pub fn new(nonce: Uuid, generation: u64, capacity: u64) -> Result<Self, AdmissionError> {
        if nonce.is_nil() || generation == 0 || capacity == 0 {
            return Err(AdmissionError::Invalid);
        }
        Ok(Self {
            nonce,
            generation,
            capacity,
            frozen: false,
            failed: false,
            obligations: BTreeMap::new(),
            last: None,
        })
    }

    /// Exact outstanding count, including grants whose replies were lost.
    pub fn current(&self) -> u64 {
        self.obligations.len() as u64
    }

    /// A protocol violation is sticky even if later cleanup succeeds.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Commit one operation before returning the resulting charged count.
    /// Lost replies retain charges. Exact latest duplicates return the committed
    /// result without applying another transition. Stale input freezes admission.
    pub fn apply(&mut self, request: AdmissionRequest) -> Result<u64, AdmissionError> {
        if let Some((prior, result)) = &self.last {
            if prior == &request {
                return result.clone();
            }
        }
        let expected = self
            .last
            .as_ref()
            .map_or(Some(1), |(prior, _)| prior.sequence.checked_add(1));
        if request.nonce != self.nonce
            || request.generation != self.generation
            || Some(request.sequence) != expected
        {
            self.failed = true;
            self.frozen = true;
            return Err(AdmissionError::Invalid);
        }
        let result = self.transition(&request.operation);
        if result == Err(AdmissionError::Invalid) {
            self.failed = true;
            self.frozen = true;
        }
        self.last = Some((request, result.clone()));
        result
    }

    fn transition(&mut self, operation: &AdmissionOperation) -> Result<u64, AdmissionError> {
        match operation {
            AdmissionOperation::Reserve { obligation } => {
                if self.frozen {
                    return Err(AdmissionError::Frozen);
                }
                if obligation.is_nil() || self.obligations.contains_key(obligation) {
                    return Err(AdmissionError::Invalid);
                }
                let current = self.current();
                if current >= self.capacity {
                    return Err(AdmissionError::Capacity {
                        capacity: self.capacity,
                        current,
                    });
                }
                current.checked_add(1).ok_or(AdmissionError::Invalid)?;
                self.obligations.insert(
                    *obligation,
                    Obligation {
                        resource: None,
                        retiring: false,
                    },
                );
            }
            AdmissionOperation::Entering {
                obligation,
                resource,
            } => {
                if self.frozen {
                    return Err(AdmissionError::Frozen);
                }
                let slot = self
                    .obligations
                    .get_mut(obligation)
                    .ok_or(AdmissionError::Invalid)?;
                if resource.is_nil() || slot.resource.is_some() || slot.retiring {
                    return Err(AdmissionError::Invalid);
                }
                slot.resource = Some(*resource);
            }
            AdmissionOperation::Retiring { obligation } => {
                let slot = self
                    .obligations
                    .get_mut(obligation)
                    .ok_or(AdmissionError::Invalid)?;
                slot.retiring = true;
            }
            AdmissionOperation::Release {
                obligation,
                certificate,
            } => {
                let slot = self
                    .obligations
                    .get(obligation)
                    .ok_or(AdmissionError::Invalid)?;
                let proven = match certificate {
                    TerminalCertificate::CancelledBeforeEntry => slot.resource.is_none(),
                    TerminalCertificate::NoResource {
                        resource,
                        creator_terminal,
                    } => slot.resource == Some(*resource) && *creator_terminal,
                    TerminalCertificate::Collected {
                        resource,
                        creator_terminal,
                        parent_collected,
                    } => slot.resource == Some(*resource) && *creator_terminal && *parent_collected,
                };
                if !slot.retiring || !proven {
                    return Err(AdmissionError::Invalid);
                }
                self.obligations.remove(obligation);
            }
            AdmissionOperation::Freeze => self.frozen = true,
        }
        Ok(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entered_creation_failure_requires_terminal_creator_and_exact_resource() {
        let nonce = Uuid::from_u128(1);
        let obligation = Uuid::from_u128(2);
        let resource = Uuid::from_u128(3);
        for (proof_resource, creator_terminal, expected) in [
            (resource, true, Ok(0)),
            (resource, false, Err(AdmissionError::Invalid)),
            (Uuid::from_u128(4), true, Err(AdmissionError::Invalid)),
        ] {
            let mut ledger = AdmissionLedger::new(nonce, 1, 1).unwrap();
            let operations = [
                AdmissionOperation::Reserve { obligation },
                AdmissionOperation::Entering {
                    obligation,
                    resource,
                },
                AdmissionOperation::Retiring { obligation },
            ];
            for (index, operation) in operations.into_iter().enumerate() {
                assert_eq!(
                    ledger.apply(AdmissionRequest {
                        nonce,
                        generation: 1,
                        sequence: index as u64 + 1,
                        operation,
                    }),
                    Ok(1)
                );
            }
            let result = ledger.apply(AdmissionRequest {
                nonce,
                generation: 1,
                sequence: 4,
                operation: AdmissionOperation::Release {
                    obligation,
                    certificate: TerminalCertificate::NoResource {
                        resource: proof_resource,
                        creator_terminal,
                    },
                },
            });
            assert_eq!(result, expected);
            assert_eq!(ledger.current(), u64::from(expected.is_err()));
        }
    }

    #[test]
    fn r2_budget_overflow_and_writer_death() {
        let nonce = Uuid::from_u128(1);
        let mut ledger = AdmissionLedger::new(nonce, 1, u64::MAX).unwrap();
        let reserve = AdmissionRequest {
            nonce,
            generation: 1,
            sequence: 1,
            operation: AdmissionOperation::Reserve {
                obligation: Uuid::from_u128(2),
            },
        };
        ledger.apply(reserve.clone()).unwrap();
        // Place the protocol at its final representable transaction without
        // performing a lifetime-sized workload. No wrapping restart is allowed.
        ledger.last = Some((
            AdmissionRequest {
                sequence: u64::MAX,
                ..reserve.clone()
            },
            Ok(1),
        ));
        assert_eq!(
            ledger.apply(AdmissionRequest {
                sequence: 0,
                operation: AdmissionOperation::Freeze,
                ..reserve
            }),
            Err(AdmissionError::Invalid)
        );
        assert!(ledger.failed());
        assert_eq!(
            ledger.current(),
            1,
            "loss of protocol authority cannot clear obligations"
        );
    }
}
