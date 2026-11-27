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

enum Pending {
    Local,
    Entry,
    Child(usize),
    Provider,
}

/// Retains installed watches across readiness. Child handles remain with the
/// caller on every error so the actual parent can perform startup rollback.
pub struct Bootstrap {
    phase: Phase,
    pending: Pending,
    index: usize,
    members: Vec<Member>,
    receipts: Vec<Ready>,
    watched: Vec<umbra_core::managed::Participant>,
    watch: Option<ProcessWatch>,
    provider: Option<super::provider_startup::ProviderStartup>,
}

impl Bootstrap {
    pub(super) fn watch_descriptor(&self) -> Option<i32> {
        self.watch.as_ref().map(ProcessWatch::descriptor)
    }

    /// Extra members and receipts describe locally owned resources, such as the
    /// cleanup owner's provider. Descendant roles are gathered from bootstrap.
    pub fn new(members: Vec<Member>, receipts: Vec<Ready>) -> io::Result<Self> {
        Ok(Self {
            phase: Phase::Members,
            pending: Pending::Local,
            index: 0,
            members,
            receipts,
            watched: Vec::new(),
            watch: None,
            provider: None,
        })
    }

    /// Include the cleanup owner's provider in the same startup barrier.
    pub fn with_provider(provider: super::provider_startup::ProviderStartup) -> io::Result<Self> {
        let mut bootstrap = Self::new(Vec::new(), Vec::new())?;
        bootstrap.provider = Some(provider);
        Ok(bootstrap)
    }

    /// Forward only the GO already accepted by the owning entry.
    pub fn forward_provider_go(&mut self, entry: &Entry) -> io::Result<bool> {
        entry.forward_provider_go(
            self.provider
                .as_mut()
                .ok_or_else(|| invalid("bootstrap has no provider"))?,
        )
    }

    pub(super) fn wait_for_provider(&self) -> io::Result<()> {
        self.provider
            .as_ref()
            .ok_or_else(|| invalid("bootstrap has no provider"))?
            .wait_for_activity()
    }

    /// One bounded startup step. True means readiness has been queued, never
    /// permission to run; the caller must still accept durable GO.
    pub fn poll(
        &mut self,
        entry: &mut Entry,
        children: &mut [PendingRole],
        external_caller: bool,
    ) -> io::Result<bool> {
        self.pending = Pending::Local;
        let result = self.progress(entry, children, external_caller);
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    /// Wait only for the exchange that stalled; local phase changes run immediately.
    pub fn wait_for_activity(&self, entry: &Entry, children: &[PendingRole]) -> io::Result<()> {
        match self.pending {
            Pending::Local => Ok(()),
            Pending::Entry => entry.wait_for_startup(),
            Pending::Child(index) => children[index].wait_for_activity(),
            Pending::Provider => self.provider.as_ref().unwrap().wait_for_activity(),
        }
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
                if let Some(provider) = &mut self.provider {
                    if !self
                        .members
                        .iter()
                        .any(|m| m.participant.role == umbra_core::managed::Role::Provider)
                    {
                        let Some(member) = provider.member()? else {
                            self.pending = Pending::Provider;
                            return Ok(false);
                        };
                        self.members.push(member);
                    }
                }
                while let Some(child) = children.get_mut(self.index) {
                    let Some(members) = child.receive_members()? else {
                        self.pending = Pending::Child(self.index);
                        return Ok(false);
                    };
                    self.members.extend(members);
                    self.index += 1;
                }
                if entry.announce_members(&self.members)? {
                    self.phase = Phase::Inventory;
                    self.index = 0;
                } else {
                    self.pending = Pending::Entry;
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
                } else {
                    self.pending = Pending::Entry;
                }
            }
            Phase::Distribute => {
                let inventory = entry
                    .inventory()
                    .ok_or_else(|| invalid("bootstrap inventory missing"))?;
                while let Some(child) = children.get_mut(self.index) {
                    if !child.send_inventory(inventory, external_caller)? {
                        self.pending = Pending::Child(self.index);
                        return Ok(false);
                    }
                    self.index += 1;
                }
                if let Some(provider) = &mut self.provider {
                    if !provider.distribute(inventory)? {
                        self.pending = Pending::Provider;
                        return Ok(false);
                    }
                }
                self.index = 0;
                self.phase = Phase::Readiness;
            }
            Phase::Readiness => {
                if let Some(provider) = &mut self.provider {
                    if !self
                        .receipts
                        .iter()
                        .any(|r| r.participant.role == umbra_core::managed::Role::Provider)
                    {
                        let Some(receipt) = provider.readiness()? else {
                            self.pending = Pending::Provider;
                            return Ok(false);
                        };
                        self.receipts.push(receipt);
                    }
                }
                while let Some(child) = children.get_mut(self.index) {
                    let Some(receipts) = child.receive_readiness(entry.readiness_deadline)? else {
                        self.pending = Pending::Child(self.index);
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
