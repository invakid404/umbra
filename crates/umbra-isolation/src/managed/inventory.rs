//! Complete pre-GO identities, independent of the later installed-watch receipt.
use super::{invalid, process};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};
use umbra_core::managed::{Participant, Role, RunIdentity};

/// Initial identity announcement before the role installs its peer watches.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Member {
    /// Kernel birth identity and assigned role.
    pub participant: Participant,
    /// Actual managed parent; absent only for the outermost role.
    pub parent: Option<Participant>,
}

/// Complete identity subtree sent upward before inventory distribution.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Announcement {
    /// Exact startup invocation.
    pub run: RunIdentity,
    /// Announcing role followed by all its managed descendants.
    pub members: Vec<Member>,
}

impl Announcement {
    /// Validate every declared descendant against the expected process tree.
    pub fn validate(
        &self,
        run: RunIdentity,
        root: Participant,
        parent: Participant,
    ) -> io::Result<()> {
        use Role::*;
        let expected: &[Role] = match root.role {
            Relay => &[Relay, Guardian, Worker, CleanupOwner, Provider],
            Guardian => &[Guardian, Worker, CleanupOwner, Provider],
            CleanupOwner => &[CleanupOwner, Provider],
            Worker => &[Worker],
            _ => return Err(invalid("invalid announcing role")),
        };
        if self.run != run || self.members.len() != expected.len() {
            return Err(invalid("announcement run or subtree size mismatch"));
        }
        let roles: BTreeMap<_, _> = self
            .members
            .iter()
            .map(|m| (m.participant.role, m))
            .collect();
        let pids: BTreeSet<_> = self.members.iter().map(|m| m.participant.pid).collect();
        if roles.len() != expected.len()
            || pids.len() != expected.len()
            || expected.iter().any(|r| !roles.contains_key(r))
        {
            return Err(invalid("announcement duplicate or missing role"));
        }
        if roles[&root.role].participant != root {
            return Err(invalid("announcement root mismatch"));
        }
        for member in &self.members {
            let expected_parent = if member.participant.role == root.role {
                parent
            } else {
                let parent_role = match member.participant.role {
                    Guardian => Relay,
                    Worker | CleanupOwner => Guardian,
                    Provider => CleanupOwner,
                    _ => return Err(invalid("invalid subtree parent")),
                };
                roles
                    .get(&parent_role)
                    .ok_or_else(|| invalid("subtree parent missing"))?
                    .participant
            };
            let (found, parent_pid) =
                process::identity_with_parent(member.participant.pid, member.participant.role)?;
            if found != member.participant
                || member.parent != Some(expected_parent)
                || parent_pid != expected_parent.pid
                || process::identity(expected_parent.pid, expected_parent.role)? != expected_parent
            {
                return Err(invalid("announcement kernel parent or birth mismatch"));
            }
        }
        Ok(())
    }
}

/// Fixed complete topology distributed before readiness is acknowledged.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Inventory {
    /// Exact invocation.
    pub run: RunIdentity,
    /// Whether the outermost role is an executable-library caller.
    pub external_caller: bool,
    /// One member per required role.
    pub members: Vec<Member>,
}

impl Inventory {
    /// Check complete descendant readiness against the already validated topology.
    pub fn validate_readiness(
        &self,
        root: Participant,
        receipts: &[super::startup::Ready],
    ) -> io::Result<()> {
        let parent = self
            .members
            .iter()
            .find(|m| m.participant == root)
            .and_then(|m| m.parent)
            .ok_or_else(|| invalid("readiness subtree root missing parent"))?;
        let announcement = Announcement {
            run: self.run,
            members: receipts
                .iter()
                .map(|r| Member {
                    participant: r.participant,
                    parent: r.parent,
                })
                .collect(),
        };
        announcement.validate(self.run, root, parent)?;
        for receipt in receipts {
            let mut seen = BTreeSet::new();
            if receipt.run != self.run
                || !self.members.contains(&Member {
                    participant: receipt.participant,
                    parent: receipt.parent,
                })
                || receipt.watched.iter().any(|peer| {
                    !seen.insert(peer.role) || !self.members.iter().any(|m| m.participant == *peer)
                })
                || self
                    .watches(receipt.participant)?
                    .iter()
                    .any(|peer| !receipt.watched.contains(peer))
            {
                return Err(invalid("readiness receipt differs from startup inventory"));
            }
        }
        Ok(())
    }

    /// Verify exact role coverage, distinct PIDs and every kernel parent edge.
    pub fn validate(&self, run: RunIdentity, external_caller: bool) -> io::Result<()> {
        run.validate().map_err(invalid)?;
        if self.run != run
            || self.external_caller != external_caller
            || self.members.len() != if external_caller { 6 } else { 5 }
        {
            return Err(invalid("startup inventory run or size mismatch"));
        }
        let mut roles = BTreeMap::new();
        let mut pids = BTreeSet::new();
        for member in &self.members {
            member.participant.validate().map_err(invalid)?;
            if roles.insert(member.participant.role, member).is_some()
                || !pids.insert(member.participant.pid)
            {
                return Err(invalid("duplicate startup participant"));
            }
        }
        for (role, parent_role) in [
            (Role::Caller, None),
            (Role::Relay, external_caller.then_some(Role::Caller)),
            (Role::Guardian, Some(Role::Relay)),
            (Role::Worker, Some(Role::Guardian)),
            (Role::CleanupOwner, Some(Role::Guardian)),
            (Role::Provider, Some(Role::CleanupOwner)),
        ] {
            if role == Role::Caller && !external_caller {
                continue;
            }
            let member = roles
                .get(&role)
                .ok_or_else(|| invalid("missing startup role"))?;
            let expected_parent = match parent_role {
                Some(role) => Some(
                    roles
                        .get(&role)
                        .ok_or_else(|| invalid("missing startup parent"))?
                        .participant,
                ),
                None => None,
            };
            if member.parent != expected_parent {
                return Err(invalid("startup inventory parent edge mismatch"));
            }
            let (found, parent_pid) = process::identity_with_parent(member.participant.pid, role)?;
            if found != member.participant || expected_parent.is_some_and(|p| p.pid != parent_pid) {
                return Err(invalid("startup inventory kernel identity mismatch"));
            }
        }
        Ok(())
    }

    /// Required independent observations for the participant's private queue.
    /// The complete inventory must have been validated before installing these.
    pub fn watches(&self, local: Participant) -> io::Result<Vec<Participant>> {
        if !self.members.iter().any(|m| m.participant == local) {
            return Err(invalid("local role absent from startup inventory"));
        }
        Ok(self
            .members
            .iter()
            .map(|m| m.participant)
            .filter(|peer| peer.role != local.role && requires_watch(local.role, peer.role))
            .collect())
    }
}

pub(super) fn requires_watch(observer: Role, subject: Role) -> bool {
    match observer {
        Role::Caller => matches!(
            subject,
            Role::Relay | Role::Guardian | Role::Worker | Role::CleanupOwner
        ),
        Role::Relay => matches!(subject, Role::Worker | Role::Guardian | Role::CleanupOwner),
        Role::Guardian => matches!(subject, Role::Worker | Role::CleanupOwner | Role::Relay),
        Role::CleanupOwner => matches!(
            subject,
            Role::Provider | Role::Worker | Role::Guardian | Role::Relay
        ),
        Role::Worker | Role::Provider => false,
    }
}
