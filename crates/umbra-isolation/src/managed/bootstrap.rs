//! Nonblocking descendant startup coordination under the original cutoff.
use super::{
    boot_nanos,
    executable::{Entry, PendingRole},
    invalid,
    inventory::Member,
    process::{identity, ProcessWatch},
    startup::Ready,
};
use std::io;

#[derive(Clone, Copy)]
enum Phase {
    Members,
    Inventory,
    Distribute,
    Readiness,
    Ready,
    Failed,
}

/// Retains installed watches across readiness. Child handles remain with the
/// caller on every error so the actual parent can perform startup rollback.
pub struct Bootstrap {
    phase: Phase,
    index: usize,
    members: Vec<Member>,
    receipts: Vec<Ready>,
    watched: Vec<umbra_core::managed::Participant>,
    watch: Option<ProcessWatch>,
}

impl Bootstrap {
    /// Extra members and receipts describe locally owned resources, such as the
    /// cleanup owner's provider. Descendant roles are gathered from bootstrap.
    pub fn new(members: Vec<Member>, receipts: Vec<Ready>) -> io::Result<Self> {
        Ok(Self {
            phase: Phase::Members,
            index: 0,
            members,
            receipts,
            watched: Vec::new(),
            watch: None,
        })
    }

    /// One bounded startup step. True means readiness has been queued, never
    /// permission to run; the caller must still accept durable GO.
    pub fn poll(
        &mut self,
        entry: &mut Entry,
        children: &mut [PendingRole],
        external_caller: bool,
    ) -> io::Result<bool> {
        let result = self.progress(entry, children, external_caller);
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    fn progress(
        &mut self,
        entry: &mut Entry,
        children: &mut [PendingRole],
        external_caller: bool,
    ) -> io::Result<bool> {
        if matches!(self.phase, Phase::Failed) {
            return Err(invalid("bootstrap already failed"));
        }
        if matches!(self.phase, Phase::Ready) {
            return Ok(true);
        }
        if boot_nanos()? >= entry.readiness_deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match self.phase {
            Phase::Members => {
                while let Some(child) = children.get_mut(self.index) {
                    let Some(members) = child.receive_members()? else {
                        return Ok(false);
                    };
                    self.members.extend(members);
                    self.index += 1;
                }
                if entry.announce_members(&self.members)? {
                    self.phase = Phase::Inventory;
                    self.index = 0;
                }
            }
            Phase::Inventory => {
                if entry.receive_inventory(external_caller)? {
                    self.watched = entry
                        .inventory()
                        .unwrap()
                        .watches(identity(std::process::id(), entry.role)?)?;
                    if !self.watched.is_empty() {
                        let mut watch = ProcessWatch::new()?;
                        for peer in &self.watched {
                            watch.install(*peer)?;
                        }
                        self.watch = Some(watch);
                    }
                    self.phase = Phase::Distribute;
                }
            }
            Phase::Distribute => {
                let inventory = entry
                    .inventory()
                    .ok_or_else(|| invalid("bootstrap inventory missing"))?;
                while let Some(child) = children.get_mut(self.index) {
                    if !child.send_inventory(inventory, external_caller)? {
                        return Ok(false);
                    }
                    self.index += 1;
                }
                self.index = 0;
                self.phase = Phase::Readiness;
            }
            Phase::Readiness => {
                while let Some(child) = children.get_mut(self.index) {
                    let Some(receipts) = child.receive_readiness(entry.readiness_deadline)? else {
                        return Ok(false);
                    };
                    self.receipts.extend(receipts);
                    self.index += 1;
                }
                entry.ready_subtree(self.watched.clone(), self.receipts.clone())?;
                self.phase = Phase::Ready;
                return Ok(true);
            }
            Phase::Ready | Phase::Failed => unreachable!(),
        }
        Ok(false)
    }

    /// Transfer the already installed queue to the runtime after readiness.
    pub fn into_watch(self) -> io::Result<Option<ProcessWatch>> {
        if !matches!(self.phase, Phase::Ready) {
            return Err(invalid("bootstrap watch transfer before readiness"));
        }
        Ok(self.watch)
    }
}
