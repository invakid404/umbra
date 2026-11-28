//! Assemble runtime notification routes from the pre-GO endpoint assignment.
use super::{
    executable::Entry, invalid, inventory::Inventory, process::identity, publication::Route,
    topology::Purpose, Channel,
};
use std::{collections::BTreeMap, io};
use umbra_core::managed::{Participant, Role};

/// Owns the notification endpoints while startup exchanges temporarily borrow
/// their assigned channels. Admission and cancellation stay with their owner.
pub struct RuntimeChannels {
    local: Participant,
    inventory: Inventory,
    expected: Vec<Role>,
    channels: BTreeMap<Role, Channel>,
}

impl RuntimeChannels {
    /// Bind the validated startup identities before any child may be released.
    /// Remaining admission/cancellation endpoints stay in Entry.
    pub fn from_entry(entry: &mut Entry) -> io::Result<Self> {
        let inventory = entry
            .inventory()
            .ok_or_else(|| invalid("runtime channels before inventory"))?
            .clone();
        let local = identity(std::process::id(), entry.role)?;
        let mut routes = Self::new(local, inventory)?;
        let mut retained = Vec::new();
        for endpoint in std::mem::take(&mut entry.endpoints) {
            if matches!(endpoint.purpose, Purpose::Admission | Purpose::Cancellation) {
                retained.push(endpoint);
            } else {
                routes.insert(endpoint.peer, Channel::new(endpoint.stream)?)?;
            }
        }
        entry.endpoints = retained;
        Ok(routes)
    }

    /// Build for the outer relay or external caller from its validated inventory.
    pub fn new(local: Participant, inventory: Inventory) -> io::Result<Self> {
        inventory.validate(inventory.run, inventory.external_caller)?;
        if !inventory
            .members
            .iter()
            .any(|member| member.participant == local)
        {
            return Err(invalid("runtime channel role absent from inventory"));
        }
        let mut expected = match local.role {
            Role::Worker => vec![Role::CleanupOwner, Role::Guardian, Role::Relay],
            Role::CleanupOwner => vec![Role::Worker, Role::Guardian, Role::Relay],
            Role::Guardian | Role::Relay => vec![Role::Worker, Role::CleanupOwner],
            Role::Caller if inventory.external_caller => vec![
                Role::Worker,
                Role::CleanupOwner,
                Role::Guardian,
                Role::Relay,
            ],
            _ => return Err(invalid("unsupported runtime notification role")),
        };
        if inventory.external_caller && local.role != Role::Caller {
            expected.push(Role::Caller);
        }
        Ok(Self {
            local,
            inventory,
            expected,
            channels: BTreeMap::new(),
        })
    }

    /// Return an existing endpoint after its startup exchange, without allocating
    /// or duplicating descriptors. Unassigned or duplicate peers are refused.
    pub fn insert(&mut self, peer: Role, channel: Channel) -> io::Result<()> {
        if !self.expected.contains(&peer) || self.channels.contains_key(&peer) {
            return Err(invalid(
                "unexpected or duplicate runtime notification endpoint",
            ));
        }
        self.channels.insert(peer, channel);
        Ok(())
    }

    /// Require every assigned route before handing ownership to a runtime role.
    pub fn finish(self) -> io::Result<Vec<Route>> {
        if self.channels.len() != self.expected.len()
            || self
                .expected
                .iter()
                .any(|peer| !self.channels.contains_key(peer))
        {
            return Err(invalid("missing runtime notification endpoint"));
        }
        self.channels
            .into_iter()
            .map(|(role, channel)| {
                let peer = self
                    .inventory
                    .members
                    .iter()
                    .find(|member| member.participant.role == role)
                    .ok_or_else(|| invalid("runtime peer absent from inventory"))?
                    .participant;
                if peer == self.local {
                    return Err(invalid("runtime notification loopback"));
                }
                Route::new(peer, channel)
            })
            .collect()
    }
}
