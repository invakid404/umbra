//! Dormant per-run admission ledger. Callers must commit the returned grant
//! before performing an acquisition; this module performs no resource I/O.
use std::collections::BTreeMap;
use uuid::Uuid;

/// Default simultaneous cleanup capacity; never a session lifetime limit.
pub const DEFAULT_CLEANUP_CAPACITY: u64 = 64;

/// Terminal evidence supplied by the collector of an obligation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalCertificate {
    /// An acquisition was never entered and cannot subsequently start.
    CancelledBeforeEntry,
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Attributable refusal or invalid protocol state.
#[derive(Clone, Debug, PartialEq, Eq)]
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
