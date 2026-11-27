//! Causal cleanup receipts. Timestamps alone cannot establish this order.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{io, path::Path};
use umbra_core::managed::{Participant, Role, RunIdentity};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stage {
    ProgramOperationsComplete,
    ProgramTerminal,
    ProgramParentCollected,
    Unconfirmed,
    DebuggerCleanupStarted,
    DebuggerParentCollected,
    RightsReleased,
    ResourceCertificate,
    ProviderParentCollected,
    WorkerParentCollected,
    OwnerParentCollected,
    GuardianParentCollected,
    RelayParentCollected,
    NamespaceComplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Id {
    pub producer: Participant,
    pub sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub run: RunIdentity,
    pub resource: u64,
    pub id: Id,
    pub stage: Stage,
    pub predecessors: Vec<Id>,
    /// Only actual parent waits set this field.
    pub collected: Option<Participant>,
    /// Emergency cleanup names the missing proof and the expired allocation.
    pub emergency: Option<(Stage, u64, u64)>,
}

pub struct Recorder<'a> {
    root: &'a Path,
    run: RunIdentity,
    producer: Participant,
    sequence: u64,
}

impl<'a> Recorder<'a> {
    pub fn new(root: &'a Path, run: RunIdentity, producer: Participant) -> Self {
        Self {
            root,
            run,
            producer,
            sequence: 0,
        }
    }

    /// Each producer writes its own immutable stage file after the operation.
    /// Consuming a predecessor requires its complete, published receipt.
    pub fn record(
        &mut self,
        stage: Stage,
        predecessors: &[Stage],
        collected: Option<Participant>,
        emergency: Option<(Stage, u64, u64)>,
    ) -> io::Result<()> {
        let predecessors = predecessors
            .iter()
            .map(|stage| {
                let prior = load(self.root, *stage)?
                    .ok_or_else(|| io::Error::other("predecessor not published"))?;
                if prior.run != self.run || prior.resource != 1 {
                    return Err(io::Error::other("predecessor identity mismatch"));
                }
                Ok(prior.id)
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.sequence += 1;
        let receipt = Receipt {
            run: self.run,
            resource: 1,
            id: Id {
                producer: self.producer,
                sequence: self.sequence,
            },
            stage,
            predecessors,
            collected,
            emergency,
        };
        let path = self.root.join(format!("order-{stage:?}.json"));
        if path.exists() {
            return Err(io::Error::other("duplicate order receipt"));
        }
        let pending = path.with_extension("pending");
        std::fs::write(&pending, serde_json::to_vec(&receipt)?)?;
        std::fs::rename(pending, path)
    }
}

pub fn load(root: &Path, stage: Stage) -> io::Result<Option<Receipt>> {
    match std::fs::read(root.join(format!("order-{stage:?}.json"))) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Verifies causal references without treating process disappearance as a wait.
pub fn verify(
    receipts: &[Receipt],
    run: RunIdentity,
    emergency: bool,
    parents: &[(Participant, Participant)],
) -> Result<(), String> {
    use Stage::*;
    let mut accepted: Vec<&Receipt> = Vec::new();
    let mut stages = BTreeMap::new();
    for receipt in receipts {
        if receipt.run != run || receipt.resource != 1 || stages.contains_key(&receipt.stage) {
            return Err("wrong run/resource or duplicate stage".into());
        }
        let expected_sequence = accepted
            .iter()
            .filter(|prior| prior.id.producer == receipt.id.producer)
            .count() as u64
            + 1;
        if receipt.id.sequence != expected_sequence {
            return Err("producer sequence gap".into());
        }
        let producer = match receipt.stage {
            ProviderParentCollected => Role::CleanupOwner,
            WorkerParentCollected | OwnerParentCollected => Role::Guardian,
            GuardianParentCollected => Role::Relay,
            RelayParentCollected | NamespaceComplete => Role::Caller,
            _ => Role::Provider,
        };
        if receipt.id.producer.role != producer {
            return Err("receipt from wrong role".into());
        }
        let wait = matches!(
            receipt.stage,
            ProgramParentCollected
                | DebuggerParentCollected
                | ProviderParentCollected
                | WorkerParentCollected
                | OwnerParentCollected
                | GuardianParentCollected
                | RelayParentCollected
        );
        if wait {
            let child = receipt.collected.ok_or("missing actual wait identity")?;
            if !parents.contains(&(child, receipt.id.producer)) {
                return Err("wait receipt is not from the child's enrolled parent".into());
            }
            if accepted.iter().any(|prior| prior.collected == Some(child)) {
                return Err("child collection reused for a different resource".into());
            }
        } else if receipt.collected.is_some() {
            return Err("non-wait stage claimed child collection".into());
        }
        let predecessors = receipt
            .predecessors
            .iter()
            .map(|id| {
                accepted
                    .iter()
                    .find(|prior| prior.id == *id)
                    .map(|prior| prior.stage)
                    .ok_or_else(|| "missing or wrong-generation predecessor".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let required: &[Stage] = match receipt.stage {
            DebuggerCleanupStarted if emergency => &[Unconfirmed],
            DebuggerCleanupStarted => &[ProgramOperationsComplete, ProgramTerminal],
            DebuggerParentCollected => &[DebuggerCleanupStarted],
            ResourceCertificate => &[
                ProgramTerminal,
                ProgramOperationsComplete,
                ProgramParentCollected,
                DebuggerParentCollected,
                RightsReleased,
            ],
            ProviderParentCollected if emergency => &[DebuggerParentCollected, Unconfirmed],
            ProviderParentCollected => &[ResourceCertificate],
            OwnerParentCollected => &[ProviderParentCollected],
            GuardianParentCollected => &[WorkerParentCollected, OwnerParentCollected],
            RelayParentCollected => &[GuardianParentCollected],
            NamespaceComplete => &[RelayParentCollected, ResourceCertificate],
            _ => &[],
        };
        if required.iter().any(|stage| !predecessors.contains(stage)) {
            return Err(format!("missing causal dependency for {:?}", receipt.stage));
        }
        if receipt.stage == Unconfirmed {
            let Some((missing, deadline, observed)) = receipt.emergency else {
                return Err("emergency lacks expired allocation".into());
            };
            if !emergency || observed < deadline || stages.contains_key(&missing) {
                return Err("invalid emergency transition".into());
            }
        } else if receipt.emergency.is_some() {
            return Err("unexpected emergency metadata".into());
        }
        if emergency && matches!(receipt.stage, ResourceCertificate | NamespaceComplete) {
            return Err("emergency cleanup became confirmed success".into());
        }
        stages.insert(receipt.stage, receipt.id.clone());
        accepted.push(receipt);
    }
    let terminal = if emergency {
        RelayParentCollected
    } else {
        NamespaceComplete
    };
    if !stages.contains_key(&terminal) {
        return Err("incomplete cross-role cleanup".into());
    }
    Ok(())
}
