//! Per-run readiness inventory and Armed-before-GO barrier.
use super::{boot_nanos, invalid};
use std::collections::BTreeMap;
use std::io;
use umbra_core::managed::{Participant, Role, RunIdentity};

/// A ready participant names its real parent and installed watch inventory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ready {
    /// Exact startup identity.
    pub run: RunIdentity,
    /// Kernel-validated participant.
    pub participant: Participant,
    /// Parent process that retains collection authority, absent for the caller.
    pub parent: Option<Participant>,
    /// Participants watched independently of control-channel EOF.
    pub watched: Vec<Participant>,
}

/// Startup authorization carrying the exact accepted durable Armed record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Go {
    /// Run whose readiness was checked.
    pub run: RunIdentity,
    /// Exact persisted record.
    pub record: umbra_core::CustodyRecord,
    /// Journal sequence returned by persistence.
    pub durable_sequence: umbra_core::Sequence,
    /// Original persistence cutoff, never renewed while forwarding.
    pub deadline: u64,
}

impl Go {
    /// Verify the receiver is enrolled in the matching Armed authorization.
    pub fn validate(&self, run: RunIdentity, local: Participant, capacity: u64) -> io::Result<()> {
        run.validate().map_err(invalid)?;
        local.validate().map_err(invalid)?;
        if boot_nanos()? >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let umbra_core::CustodyPhase::Armed {
            capacity: armed_capacity,
            startup_nonce,
            participants,
        } = &self.record.state
        else {
            return Err(invalid("GO without Armed custody"));
        };
        let birth = local
            .birth
            .0
            .checked_mul(1_000_000)
            .and_then(|n| n.checked_add(local.birth.1))
            .ok_or_else(|| invalid("GO birth overflow"))?;
        if self.run != run
            || *startup_nonce != run.nonce
            || self.durable_sequence.0 == 0
            || self.record.sequence != 1
            || self.record.generation.is_nil()
            || *armed_capacity != capacity
            || participants
                .iter()
                .filter(|p| p.pid == local.pid && p.birth == birth)
                .count()
                != 1
        {
            return Err(invalid("GO custody does not match receiver"));
        }
        Ok(())
    }
}

/// Complete readiness is required before the worker may persist Armed.
pub struct Startup {
    run: RunIdentity,
    external_caller: bool,
    deadline: u64,
    ready: BTreeMap<Role, Ready>,
    armed: bool,
    expected_armed: Option<umbra_core::CustodyRecord>,
    durable_sequence: Option<umbra_core::Sequence>,
    persistence_deadline: Option<u64>,
    failed: bool,
}

impl Startup {
    /// Reconstruct readiness in the worker using the inherited startup cutoff.
    /// Receiving a complete inventory never grants a fresh readiness window.
    pub fn from_readiness(
        run: RunIdentity,
        external_caller: bool,
        deadline: u64,
        receipts: Vec<Ready>,
    ) -> io::Result<Self> {
        let mut startup = Self::new(run, external_caller)?;
        if deadline > startup.deadline || boot_nanos()? >= deadline {
            return Err(invalid("invalid inherited readiness cutoff"));
        }
        startup.deadline = deadline;
        for receipt in receipts {
            startup.accept(receipt)?;
        }
        if !startup.complete() {
            return Err(invalid("incomplete worker readiness inventory"));
        }
        Ok(startup)
    }

    /// Start the shared two-second readiness allocation.
    pub fn new(run: RunIdentity, external_caller: bool) -> io::Result<Self> {
        run.validate().map_err(invalid)?;
        Ok(Self {
            run,
            external_caller,
            deadline: boot_nanos()?
                .checked_add(2_000_000_000)
                .ok_or_else(|| invalid("readiness clock overflow"))?,
            ready: BTreeMap::new(),
            armed: false,
            expected_armed: None,
            durable_sequence: None,
            persistence_deadline: None,
            failed: false,
        })
    }

    /// Shared absolute readiness cutoff passed to every spawned descendant.
    pub fn readiness_deadline(&self) -> u64 {
        self.deadline
    }

    /// Validate a ready message against the kernel before retaining it.
    pub fn accept(&mut self, ready: Ready) -> io::Result<()> {
        let result = (|| {
            if self.failed
                || self.armed
                || self.persistence_deadline.is_some()
                || boot_nanos()? >= self.deadline
                || ready.run != self.run
            {
                return Err(invalid("invalid or late readiness"));
            }
            if self.ready.contains_key(&ready.participant.role)
                || self
                    .ready
                    .values()
                    .any(|r| r.participant.pid == ready.participant.pid)
            {
                return Err(invalid("duplicate readiness identity"));
            }
            let (found, parent_pid) = super::process::identity_with_parent(
                ready.participant.pid,
                ready.participant.role,
            )?;
            if ready.parent.is_some_and(|parent| parent.pid != parent_pid) {
                return Err(invalid("readiness parent mismatch"));
            }
            if found != ready.participant {
                return Err(invalid("readiness birth mismatch"));
            }
            self.ready.insert(ready.participant.role, ready);
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Verify every parent edge before authorizing the Armed persistence step.
    pub fn complete(&self) -> bool {
        let parents = [
            (Role::Relay, self.external_caller.then_some(Role::Caller)),
            (Role::Guardian, Some(Role::Relay)),
            (Role::Worker, Some(Role::Guardian)),
            (Role::CleanupOwner, Some(Role::Guardian)),
            (Role::Provider, Some(Role::CleanupOwner)),
        ];
        !self.failed
            && self.ready.len() == (if self.external_caller { 6 } else { 5 })
            && self.ready.values().all(|ready| {
                let mut seen = std::collections::BTreeSet::new();
                ready.watched.iter().all(|watched| {
                    seen.insert(watched.role)
                        && self
                            .ready
                            .get(&watched.role)
                            .is_some_and(|peer| peer.participant == *watched)
                })
            })
            && self.ready.values().all(|observer| {
                self.ready.values().all(|subject| {
                    !super::inventory::requires_watch(
                        observer.participant.role,
                        subject.participant.role,
                    ) || observer.watched.contains(&subject.participant)
                })
            })
            && parents.iter().all(|(role, parent)| {
                let Some(ready) = self.ready.get(role) else {
                    return false;
                };
                match parent {
                    Some(parent) => self
                        .ready
                        .get(parent)
                        .is_some_and(|p| ready.parent == Some(p.participant)),
                    None => ready.parent.is_none(),
                }
            })
    }

    /// Start the single five-second persistence allocation after readiness.
    pub fn begin_arming(&mut self, record: umbra_core::CustodyRecord) -> io::Result<u64> {
        let now = boot_nanos()?;
        if !self.complete() || self.persistence_deadline.is_some() {
            self.failed = true;
            return Err(invalid(
                "Armed persistence requires timely complete readiness",
            ));
        }
        self.validate_arming_record(&record)?;
        self.expected_armed = Some(record);
        let deadline = now
            .checked_add(5_000_000_000)
            .ok_or_else(|| invalid("persistence clock overflow"))?;
        self.persistence_deadline = Some(deadline);
        Ok(deadline)
    }

    /// Validate the proposed record without starting persistence time.
    pub fn validate_arming_record(&mut self, record: &umbra_core::CustodyRecord) -> io::Result<()> {
        if !self.complete() {
            return Err(invalid("arming without complete readiness"));
        }
        let valid = (|| {
            let umbra_core::CustodyPhase::Armed {
                capacity,
                startup_nonce,
                participants,
            } = &record.state
            else {
                return None;
            };
            if *capacity == 0
                || *startup_nonce != self.run.nonce
                || record.sequence != 1
                || record.generation.is_nil()
                || participants.len() != self.ready.len()
            {
                return None;
            }
            for (ready, persisted) in self.ready.values().zip(participants) {
                let identity = ready.participant;
                let birth = identity
                    .birth
                    .0
                    .checked_mul(1_000_000)?
                    .checked_add(identity.birth.1)?;
                if persisted.pid != identity.pid || persisted.birth != birth {
                    return None;
                }
            }
            Some(())
        })();
        if valid.is_none() {
            self.failed = true;
            return Err(invalid(
                "Armed record differs from validated readiness inventory",
            ));
        }
        Ok(())
    }

    /// Accept the exact Armed record and sequence returned by durable persistence.
    pub fn persist_armed(
        &mut self,
        run: RunIdentity,
        record: umbra_core::CustodyRecord,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<Go> {
        if run != self.run {
            self.failed = true;
            return Err(invalid("Armed persistence run mismatch"));
        }
        let deadline = self.begin_arming(record.clone())?;
        let sequence = match persist(&record, deadline) {
            Ok(sequence) => sequence,
            Err(error) => {
                // Ambiguous persistence still prohibits GO. The authoritative
                // journal may retain Armed and must be inspected on recovery.
                self.failed = true;
                return Err(error);
            }
        };
        self.commit_armed(&record, sequence)?;
        self.authorization(run)
    }

    /// Accept the exact Armed record and sequence returned by durable persistence.
    pub fn commit_armed(
        &mut self,
        record: &umbra_core::CustodyRecord,
        durable_sequence: umbra_core::Sequence,
    ) -> io::Result<()> {
        let now = boot_nanos()?;
        if self.expected_armed.as_ref() != Some(record)
            || durable_sequence.0 == 0
            || !self.complete()
            || self.armed
            || !self
                .persistence_deadline
                .is_some_and(|deadline| now < deadline)
        {
            self.failed = true;
            return Err(invalid("Armed requires complete matching startup"));
        }
        self.armed = true;
        self.durable_sequence = Some(durable_sequence);
        Ok(())
    }

    /// Return authorization only after the exact persistence receipt was accepted.
    pub fn authorization(&self, run: RunIdentity) -> io::Result<Go> {
        if !self.go() || run != self.run {
            return Err(invalid("GO before Armed or for another run"));
        }
        Ok(Go {
            run,
            deadline: self
                .persistence_deadline
                .ok_or_else(|| invalid("missing persistence deadline"))?,
            record: self
                .expected_armed
                .clone()
                .ok_or_else(|| invalid("missing Armed record"))?,
            durable_sequence: self
                .durable_sequence
                .ok_or_else(|| invalid("missing Armed sequence"))?,
        })
    }

    /// No session acquisition may happen unless this barrier is open.
    pub fn go(&self) -> bool {
        self.armed && !self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_does_not_renew_readiness_or_persistence() {
        let run = RunIdentity {
            nonce: uuid::Uuid::new_v4(),
            generation: 1,
        };
        let mut startup = Startup::new(run, false).unwrap();
        let parents = [
            (Role::Relay, None),
            (Role::Guardian, Some(Role::Relay)),
            (Role::Worker, Some(Role::Guardian)),
            (Role::CleanupOwner, Some(Role::Guardian)),
            (Role::Provider, Some(Role::CleanupOwner)),
        ];
        let identities: BTreeMap<_, _> = parents
            .iter()
            .enumerate()
            .map(|(i, (role, _))| {
                (
                    *role,
                    Participant {
                        role: *role,
                        pid: i as u32 + 100,
                        birth: (1, i as u64),
                    },
                )
            })
            .collect();
        // Model an inventory already checked against the kernel before preparation.
        for (role, parent) in parents {
            startup.ready.insert(
                role,
                Ready {
                    run,
                    participant: identities[&role],
                    parent: parent.map(|p| identities[&p]),
                    watched: identities
                        .values()
                        .filter(|p| super::super::inventory::requires_watch(role, p.role))
                        .copied()
                        .collect(),
                },
            );
        }
        assert!(startup.complete());
        startup.deadline = 1;
        let record = umbra_core::CustodyRecord {
            run_id: serde_json::from_value(serde_json::json!(run.nonce)).unwrap(),
            generation: uuid::Uuid::new_v4(),
            sequence: 1,
            state: umbra_core::CustodyPhase::Armed {
                capacity: 64,
                startup_nonce: run.nonce,
                participants: identities
                    .values()
                    .map(|p| umbra_core::CustodyParticipant {
                        pid: p.pid,
                        birth: p.birth.0 * 1_000_000 + p.birth.1,
                    })
                    .collect(),
            },
        };
        let before = boot_nanos().unwrap();
        let cutoff = startup.begin_arming(record.clone()).unwrap();
        assert!(cutoff >= before + 5_000_000_000);
        assert!(cutoff <= boot_nanos().unwrap() + 5_000_000_000);
        assert_eq!(startup.deadline, 1);
        assert!(startup.begin_arming(record).is_err());
        assert_eq!(startup.persistence_deadline, Some(cutoff));
        assert!(!startup.go());
        assert!(Startup::from_readiness(run, false, 1, Vec::new()).is_err());
    }

    #[test]
    fn go_requires_current_receipt_and_exact_receiver() {
        let nonce = "00000000-0000-0000-0000-000000000001";
        let go: Go = serde_json::from_value(serde_json::json!({
            "run": {"nonce": nonce, "generation": 1},
            "record": {"run_id": nonce, "generation": nonce, "sequence": 1,
                "state": {"Armed": {"capacity": 64, "startup_nonce": nonce,
                    "participants": [{"pid": 42, "birth": 1000002}]}}},
            "durable_sequence": 9, "deadline": boot_nanos().unwrap() + 5_000_000_000u64
        }))
        .unwrap();
        let local = Participant {
            role: Role::Worker,
            pid: 42,
            birth: (1, 2),
        };
        let mut startup = Startup::new(go.run, false).unwrap();
        let mut wrote = false;
        assert!(startup
            .persist_armed(go.run, go.record.clone(), |_, _| {
                wrote = true;
                Ok(umbra_core::Sequence(9))
            })
            .is_err());
        assert!(!wrote, "incomplete readiness must prevent journal writes");
        assert!(!startup.go());
        assert!(go.validate(go.run, local, 64).is_ok());
        assert!(go.validate(go.run, local, 63).is_err());
        assert!(go
            .validate(
                go.run,
                Participant {
                    birth: (1, 3),
                    ..local
                },
                64
            )
            .is_err());
        let mut changed = go.clone();
        changed.run.generation += 1;
        assert!(changed.validate(go.run, local, 64).is_err());
        let mut changed = go.clone();
        changed.durable_sequence.0 = 0;
        assert!(changed.validate(go.run, local, 64).is_err());
        let mut changed = go.clone();
        changed.deadline = 0;
        assert_eq!(
            changed.validate(go.run, local, 64).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
