//! Per-run descriptor ownership for the managed role tree.
use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use umbra_core::managed::Role;

/// Purpose prevents treating admission, cancellation and notification as aliases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Purpose {
    /// Worker to owner requests and provider handoff.
    OwnerControl,
    /// Guardian observation and startup barrier.
    GuardianControl,
    /// Two-sided resource admission.
    Admission,
    /// Independent provider cancellation.
    Cancellation,
    /// Direct Start/ACK route.
    Notification,
}

/// Exactly one role owns this endpoint after spawn.
pub struct Endpoint {
    /// Opposite participant's role.
    pub peer: Role,
    /// Protocol assigned to this pair.
    pub purpose: Purpose,
    /// Owned close-on-exec socket.
    pub stream: UnixStream,
}

/// Startup holds all endpoints until each role's spawn file actions adopt them.
pub struct Topology {
    endpoints: BTreeMap<Role, Vec<Endpoint>>,
}

impl Topology {
    /// Seven CLI pairs; executable-library callers add four direct pairs.
    pub fn new(external_caller: bool) -> io::Result<Self> {
        use Purpose::*;
        use Role::*;
        let mut topology = Self {
            endpoints: BTreeMap::new(),
        };
        for (a, b, purpose) in [
            (Worker, CleanupOwner, OwnerControl),
            (Worker, Guardian, GuardianControl),
            (Guardian, CleanupOwner, GuardianControl),
            (CleanupOwner, Provider, Admission),
            (CleanupOwner, Provider, Cancellation),
            (Worker, Relay, Notification),
            (CleanupOwner, Relay, Notification),
        ] {
            topology.pair(a, b, purpose)?;
        }
        if external_caller {
            for role in [Relay, Guardian, Worker, CleanupOwner] {
                topology.pair(Caller, role, Notification)?;
            }
        }
        Ok(topology)
    }

    fn pair(&mut self, a: Role, b: Role, purpose: Purpose) -> io::Result<()> {
        let (left, right) = UnixStream::pair()?;
        self.endpoints.entry(a).or_default().push(Endpoint {
            peer: b,
            purpose,
            stream: left,
        });
        self.endpoints.entry(b).or_default().push(Endpoint {
            peer: a,
            purpose,
            stream: right,
        });
        Ok(())
    }

    /// Move a role's entire assignment exactly once; no implicit socket copies.
    pub fn take(&mut self, role: Role) -> io::Result<Vec<Endpoint>> {
        self.endpoints
            .remove(&role)
            .ok_or_else(|| super::invalid("role endpoints already assigned"))
    }

    /// Outstanding startup descriptors, for rollback and measured peak inventory.
    pub fn outstanding(&self) -> usize {
        self.endpoints.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn each_endpoint_has_one_role_and_the_ruled_cost() {
        let mut cli = Topology::new(false).unwrap();
        assert_eq!(cli.outstanding(), 14);
        for (role, count) in [
            (Role::Worker, 3),
            (Role::Guardian, 2),
            (Role::CleanupOwner, 5),
            (Role::Provider, 2),
            (Role::Relay, 2),
        ] {
            assert_eq!(cli.take(role).unwrap().len(), count);
            assert!(cli.take(role).is_err());
        }
        assert_eq!(cli.outstanding(), 0);
        let library = Topology::new(true).unwrap();
        assert_eq!(library.outstanding(), 22);
    }
}
