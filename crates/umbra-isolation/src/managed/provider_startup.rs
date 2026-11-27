//! Provider bootstrap authority without ownership of its process handle.
use super::{
    invalid,
    inventory::{Announcement, Inventory, Member},
    startup::{Go, Ready},
    Channel,
};
use std::io;
use umbra_core::managed::{Participant, RunIdentity};

/// The owner retains this channel while `Launch` retains the actual child.
pub struct ProviderStartup {
    channel: Channel,
    run: RunIdentity,
    provider: Participant,
    owner: Participant,
    capacity: u64,
    deadline: u64,
    member: Option<Member>,
    inventory: Option<Inventory>,
    ready: Option<Ready>,
    go: Option<Go>,
}

impl ProviderStartup {
    pub(super) fn wait_for_activity(&self) -> io::Result<()> {
        self.channel.wait_until(self.deadline)
    }

    /// Exact provider control descriptor for the hosted inventory fixture.
    #[cfg(feature = "managed-endpoint-probe")]
    pub fn probe_bootstrap_fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.channel.stream.as_raw_fd()
    }
    pub(super) fn new(
        channel: Channel,
        run: RunIdentity,
        provider: Participant,
        owner: Participant,
        capacity: u64,
        deadline: u64,
    ) -> Self {
        Self {
            channel,
            run,
            provider,
            owner,
            capacity,
            deadline,
            member: None,
            inventory: None,
            ready: None,
            go: None,
        }
    }

    pub(super) fn member(&mut self) -> io::Result<Option<Member>> {
        if let Some(member) = &self.member {
            return Ok(Some(member.clone()));
        }
        let Some(announcement) = self.channel.receive::<Announcement>(self.deadline)? else {
            return Ok(None);
        };
        announcement.validate(self.run, self.provider, self.owner)?;
        self.member = announcement.members.into_iter().next();
        Ok(self.member.clone())
    }

    pub(super) fn distribute(&mut self, inventory: &Inventory) -> io::Result<bool> {
        inventory.validate(self.run, inventory.external_caller)?;
        let member = self
            .member
            .as_ref()
            .ok_or_else(|| invalid("provider inventory before announcement"))?;
        if !inventory.members.contains(member)
            || self.ready.is_some()
            || self
                .inventory
                .as_ref()
                .is_some_and(|prior| prior != inventory)
        {
            return Err(invalid("provider inventory changed or out of order"));
        }
        if self.inventory.is_none() {
            self.channel.queue(inventory)?;
            self.inventory = Some(inventory.clone());
        }
        self.channel.flush(self.deadline)
    }

    pub(super) fn readiness(&mut self) -> io::Result<Option<Ready>> {
        if let Some(ready) = &self.ready {
            return Ok(Some(ready.clone()));
        }
        let inventory = self
            .inventory
            .as_ref()
            .ok_or_else(|| invalid("provider readiness before inventory"))?;
        let Some(receipts) = self.channel.receive::<Vec<Ready>>(self.deadline)? else {
            return Ok(None);
        };
        inventory.validate_readiness(self.provider, &receipts)?;
        self.ready = receipts.into_iter().next();
        Ok(self.ready.clone())
    }

    pub(super) fn authorize(&mut self, go: &Go) -> io::Result<bool> {
        if self.ready.is_none() || self.go.as_ref().is_some_and(|prior| prior != go) {
            return Err(invalid("provider GO before readiness or changed"));
        }
        go.validate(self.run, self.provider, self.capacity)?;
        if self.go.is_none() {
            self.channel.queue(go)?;
            self.go = Some(go.clone());
        }
        self.channel.flush(self.deadline.min(go.deadline))
    }
}
