//! Provider-lifetime resource inventory and terminal closure proof.
//! Transport agreement is necessary but cannot establish acquisition coverage:
//! backends must reserve slots before creating resources.

use crate::{ErrorKind, Result, UmbraError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Negotiated platform closure protocol.
pub const CAPABILITY: &str = "platform.shutdown-ledger.v1";

/// Fresh provider or shutdown-request identity.
pub type Nonce = Uuid;

/// A never-reused slot in one provider instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotId {
    /// Fresh provider identity, independent of process ID reuse.
    pub instance: Uuid,
    /// Checked, monotonically increasing allocation number, starting at one.
    pub sequence: u64,
}

/// Independently accountable ownership class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    /// A tracee whose exit accounting must be established.
    Tracee,
    /// A debugserver child that must be collected by its owner.
    Tracer,
    /// A retained Mach send right, including an exec replacement.
    TaskRight,
}

/// Immutable resource reservation. Identity remains optional until acquisition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    /// Unique reservation identity.
    pub id: SlotId,
    /// Owning tracee reservation, when applicable.
    pub parent: Option<SlotId>,
    /// Prior right retained until this exec replacement is acquired.
    pub replaces: Option<SlotId>,
    /// Generation fixed at reservation. Native child-creation reservations
    /// retain their creator's generation; task rights and tracers use their
    /// tracee's attachment generation.
    pub generation: u64,
    /// Exec generation at reservation.
    pub exec_generation: u64,
    /// Resource obligation.
    pub kind: ResourceKind,
}

/// Identity acquired into a reservation, separate from its immutable descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Native process ID, when the resource has one.
    pub pid: Option<i32>,
    /// Attach-time process birth identity, if available.
    pub birth: Option<(u64, u64)>,
}

/// Explicit host observation; an attempted signal is never confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Proof {
    /// Exact owned child returned a terminal wait status.
    Collected {
        /// Host terminal status, retained without interpreting command success.
        status: String,
    },
    /// Identity-bound exit-accounting observation for a tracee.
    Accounted {
        /// Existing backend barrier evidence.
        evidence: String,
        /// Whether the provider also owned and collected the root.
        root_collected: bool,
        /// Whether collection was a provider obligation.
        root_owned: bool,
    },
    /// The owned send right was released.
    Released,
}

/// Slot settlement; no absence or pending state is silently successful.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// Creation has not returned an accountable outcome.
    Reserved,
    /// Resource exists and is not yet confirmed closed.
    Acquired,
    /// Existing cleanup is running.
    Settling,
    /// Creation definitively produced no resource.
    NeverCreated,
    /// Every obligation for this slot was established.
    Confirmed(Proof),
    /// Terminal failure with retained diagnostic.
    Unconfirmed(String),
}

/// Mutable settlement paired with its immutable descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Immutable reservation.
    pub slot: Slot,
    /// Actual acquisition identity; never inferred from a later reused PID.
    pub identity: Option<Identity>,
    /// Current or final settlement.
    pub disposition: Disposition,
}

impl Entry {
    /// Whether the recorded identity and terminal proof establish settlement.
    /// This does not establish that a caller supplied the complete inventory.
    pub fn confirms(&self) -> bool {
        match (&self.slot.kind, &self.disposition) {
            (_, Disposition::NeverCreated) => self.identity.is_none(),
            (ResourceKind::Tracer, Disposition::Confirmed(Proof::Collected { status })) => {
                !status.is_empty()
                    && self
                        .identity
                        .as_ref()
                        .is_some_and(|id| id.pid.is_some_and(|pid| pid > 0))
            }
            (ResourceKind::TaskRight, Disposition::Confirmed(Proof::Released)) => {
                self.identity.is_some()
            }
            (
                ResourceKind::Tracee,
                Disposition::Confirmed(Proof::Accounted {
                    root_owned,
                    root_collected,
                    evidence,
                }),
            ) => {
                !evidence.is_empty()
                    && *root_owned == self.slot.parent.is_none()
                    && (*root_owned == *root_collected)
                    && self
                        .identity
                        .as_ref()
                        .is_some_and(|id| id.pid.is_some_and(|pid| pid > 0))
            }
            _ => false,
        }
    }
}

/// Frozen acceptance, which is not a closure confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    /// Fresh client request nonce.
    pub nonce: Uuid,
    /// Provider instance identity.
    pub instance: Uuid,
    /// Frozen inventory generation.
    pub generation: u64,
    /// Last reserved sequence.
    pub high_water: u64,
    /// Every reservation ever issued, including retired entries.
    pub slots: Vec<Slot>,
    /// Remaining existing cleanup windows, in milliseconds.
    pub remaining_millis: u64,
}

/// Final inventory, published only after resource-owning destruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Final {
    /// Acceptance nonce.
    pub nonce: Uuid,
    /// Acceptance generation.
    pub generation: u64,
    /// Exactly one terminal settlement per accepted slot.
    pub entries: Vec<Entry>,
    /// Secondary cleanup diagnostics, in observation order.
    pub diagnostics: Vec<String>,
}

/// One bounded poll response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Poll {
    /// Owner is still within its existing cleanup windows.
    Pending {
        /// Most recently published progress generation.
        progress: u64,
    },
    /// Terminal resource inventory; provider exit remains a separate obligation.
    Final(Final),
}

fn invalid(context: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, "platform.shutdown", context)
}

impl Accepted {
    /// Validate the complete contiguous reservation inventory.
    pub fn validate(&self) -> Result<()> {
        if self.high_water != self.slots.len() as u64 {
            return Err(invalid("inventory count does not match high water"));
        }
        let mut ids = BTreeMap::new();
        for slot in &self.slots {
            if slot.id.instance != self.instance
                || slot.id.sequence == 0
                || slot.id.sequence > self.high_water
                || ids.insert(slot.id, slot).is_some()
            {
                return Err(invalid("invalid or duplicate reservation identity"));
            }
        }
        for slot in &self.slots {
            if let Some(parent) = slot.parent {
                if !ids.contains_key(&parent) || parent.sequence >= slot.id.sequence {
                    return Err(invalid("invalid reservation parent"));
                }
                if ids
                    .get(&parent)
                    .is_none_or(|parent| parent.kind != ResourceKind::Tracee)
                {
                    return Err(invalid("reservation parent is not a tracee"));
                }
            } else if slot.kind != ResourceKind::Tracee {
                return Err(invalid("resource reservation has no tracee owner"));
            }
            if let Some(previous) = slot.replaces {
                let old = ids
                    .get(&previous)
                    .ok_or_else(|| invalid("unknown replacement source"))?;
                if old.id.sequence >= slot.id.sequence
                    || old.kind != ResourceKind::TaskRight
                    || slot.kind != ResourceKind::TaskRight
                    || old.parent != slot.parent
                    || old.generation != slot.generation
                    || old.exec_generation >= slot.exec_generation
                {
                    return Err(invalid("invalid right replacement"));
                }
            }
        }
        Ok(())
    }

    /// Validate final framing and return independently established tree proof.
    /// An unconfirmed ledger is well-formed but cannot establish termination.
    pub fn confirms(&self, final_state: &Final) -> Result<bool> {
        self.validate()?;
        if final_state.nonce != self.nonce
            || final_state.generation != self.generation
            || final_state.entries.len() != self.slots.len()
        {
            return Err(invalid("final does not match accepted inventory"));
        }
        let slots: BTreeMap<_, _> = self.slots.iter().map(|slot| (slot.id, slot)).collect();
        let mut seen = BTreeSet::new();
        let mut confirmed = true;
        for entry in &final_state.entries {
            if !seen.insert(entry.slot.id) {
                return Err(invalid("duplicate final slot"));
            }
            let expected = slots
                .get(&entry.slot.id)
                .copied()
                .ok_or_else(|| invalid("unknown final slot"))?;
            if expected != &entry.slot {
                return Err(invalid("final changed immutable reservation"));
            }
            confirmed &= entry.confirms();
        }
        Ok(confirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inventory() -> (Accepted, Final) {
        let instance = Uuid::new_v4();
        let slot = Slot {
            id: SlotId {
                instance,
                sequence: 1,
            },
            parent: None,
            replaces: None,
            generation: 1,
            exec_generation: 0,
            kind: ResourceKind::Tracee,
        };
        let accepted = Accepted {
            nonce: Uuid::new_v4(),
            instance,
            generation: 1,
            high_water: 1,
            slots: vec![slot.clone()],
            remaining_millis: 100,
        };
        let final_state = Final {
            nonce: accepted.nonce,
            generation: 1,
            entries: vec![Entry {
                slot,
                identity: Some(Identity {
                    pid: Some(123),
                    birth: Some((1, 2)),
                }),
                disposition: Disposition::Confirmed(Proof::Accounted {
                    evidence: "exact owned terminal wait".into(),
                    root_owned: true,
                    root_collected: true,
                }),
            }],
            diagnostics: vec![],
        };
        (accepted, final_state)
    }
    #[test]
    fn shutdown_rejects_inconsistent_inventory() {
        let (accepted, final_state) = inventory();
        assert!(accepted.confirms(&final_state).unwrap());
        for mutation in 0..9 {
            let mut accepted = accepted.clone();
            let mut final_state = final_state.clone();
            match mutation {
                0 => final_state.entries.clear(),
                1 => final_state.entries.push(final_state.entries[0].clone()),
                2 => final_state.entries[0].slot.id.sequence += 1,
                3 => final_state.entries[0].slot.id.instance = Uuid::new_v4(),
                4 => final_state.generation += 1,
                5 => final_state.nonce = Uuid::new_v4(),
                6 => accepted.high_water += 1,
                7 => final_state.entries[0].slot.exec_generation += 1,
                8 => accepted.slots[0].parent = Some(accepted.slots[0].id),
                _ => unreachable!(),
            }
            assert!(
                accepted.confirms(&final_state).is_err(),
                "mutation {mutation}"
            );
        }
    }
    #[test]
    fn shutdown_requires_tracee_ownership_relationships() {
        let (mut accepted, _) = inventory();
        let root = accepted.slots[0].clone();
        let right = Slot {
            id: SlotId {
                instance: accepted.instance,
                sequence: 2,
            },
            parent: Some(root.id),
            replaces: None,
            generation: root.generation,
            exec_generation: 0,
            kind: ResourceKind::TaskRight,
        };
        accepted.slots.push(right.clone());
        accepted.high_water = 2;
        assert!(accepted.validate().is_ok());
        for mutation in 0..2 {
            let mut malformed = accepted.clone();
            match mutation {
                0 => malformed.slots[1].parent = None,
                1 => {
                    malformed.slots.push(Slot {
                        id: SlotId {
                            instance: accepted.instance,
                            sequence: 3,
                        },
                        parent: Some(right.id),
                        kind: ResourceKind::Tracee,
                        ..root.clone()
                    });
                    malformed.high_water = 3;
                }
                _ => unreachable!(),
            }
            assert!(
                malformed.validate().is_err(),
                "invalid ownership relationship {mutation} accepted"
            );
        }
    }

    #[test]
    fn shutdown_requires_every_obligation() {
        let (accepted, mut final_state) = inventory();
        for disposition in [
            Disposition::Reserved,
            Disposition::Acquired,
            Disposition::Settling,
            Disposition::Unconfirmed("wait unavailable".into()),
            Disposition::NeverCreated,
            Disposition::Confirmed(Proof::Released),
            Disposition::Confirmed(Proof::Accounted {
                evidence: "zombie".into(),
                root_owned: true,
                root_collected: false,
            }),
            Disposition::Confirmed(Proof::Accounted {
                evidence: "zombie".into(),
                root_owned: false,
                root_collected: false,
            }),
        ] {
            final_state.entries[0].disposition = disposition;
            assert!(!accepted.confirms(&final_state).unwrap());
        }
        final_state.entries[0].identity = None;
        final_state.entries[0].disposition = Disposition::NeverCreated;
        assert!(accepted.confirms(&final_state).unwrap());
    }
}
