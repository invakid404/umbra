//! External caller startup with actual relay custody and independent watches.
use super::{
    channels::RuntimeChannels,
    executable::{self, PendingRole},
    guard::Guard,
    invalid,
    inventory::{Inventory, Member},
    process::{identity, ProcessWatch},
    startup::{Ready, Startup},
    topology::{Endpoint, Topology},
    Channel,
};
use std::{collections::BTreeMap, io};
use umbra_core::managed::{LaunchConfiguration, Role};

/// Retains the relay on every failed startup step for direct-parent rollback.
pub struct CallerStartup {
    /// Actual relay child, transferred only after readiness publication.
    pub children: Vec<PendingRole>,
    launch: LaunchConfiguration,
    deadline: u64,
    endpoints: Vec<Endpoint>,
    inventory: Option<Inventory>,
    watch: Option<ProcessWatch>,
    receipt: Option<Ready>,
    guard: Option<Guard>,
    distributed: bool,
    received: bool,
    queued: bool,
    complete: bool,
    failed: bool,
    rollback: Option<super::rollback::Rollback>,
}

impl CallerStartup {
    /// Wait for retained children during pre-GO rollback.
    pub fn wait_for_rollback(&mut self) -> io::Result<()> {
        self.rollback
            .as_mut()
            .ok_or_else(|| invalid("rollback not started"))?
            .wait_for_activity()
    }

    /// Wait after a stalled startup step, retaining the relay and original cutoff.
    pub fn wait_for_activity(&self) -> io::Result<()> {
        if self.failed || self.complete {
            return Err(invalid("caller wait outside startup"));
        }
        self.children
            .first()
            .ok_or_else(|| invalid("caller relay missing"))?
            .wait_for_activity()
    }

    /// Continue failed cleanup after readiness when GO may have reached a child.
    /// Pending child handles stay here if runtime adoption cannot be validated.
    pub fn fail_runtime(&mut self) -> io::Result<super::guard::Outcome> {
        self.failed = true;
        let guard = self
            .guard
            .as_mut()
            .ok_or_else(|| invalid("runtime guard not prepared"))?;
        guard.adopt_ready_children(&mut self.children)?;
        guard.notify_failure()?;
        Ok(self.guard.take().unwrap().run_parent())
    }

    /// Roll back only before publishing caller readiness can authorize arming.
    pub fn rollback(&mut self) -> io::Result<bool> {
        self.failed = true;
        if self.queued || self.complete {
            return Err(invalid("caller rollback after arming authorization"));
        }
        let deadline = self
            .deadline
            .checked_add(9_000_000_000)
            .ok_or_else(|| invalid("caller rollback clock overflow"))?;
        super::rollback::Rollback::retain(&mut self.children, &mut self.rollback, deadline)?;
        self.endpoints.clear();
        self.guard.take();
        self.rollback.as_mut().unwrap().poll()
    }

    /// Spawn only the registered executable, retaining the original cutoff.
    pub fn spawn(launch: LaunchConfiguration, deadline: u64) -> io::Result<Self> {
        let mut topology = Topology::new(true)?;
        let endpoints = topology.take(Role::Caller)?;
        let mut assignments = BTreeMap::new();
        for role in [
            Role::Relay,
            Role::Guardian,
            Role::Worker,
            Role::CleanupOwner,
            Role::Provider,
        ] {
            assignments.insert(role, topology.take(role)?);
        }
        let relay = executable::spawn_tree(
            Role::Relay,
            Role::Caller,
            &launch,
            assignments,
            false,
            deadline,
        )?;
        Ok(Self {
            children: vec![relay],
            launch,
            deadline,
            endpoints,
            inventory: None,
            watch: None,
            receipt: None,
            guard: None,
            distributed: false,
            received: false,
            queued: false,
            complete: false,
            failed: false,
            rollback: None,
        })
    }

    /// Build the complete inventory, then publish only installed observations.
    pub fn poll(&mut self) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("caller startup already failed"));
        }
        let result = self.progress();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn progress(&mut self) -> io::Result<bool> {
        if self.complete {
            return Ok(true);
        }
        if super::boot_nanos()? >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let local = identity(std::process::id(), Role::Caller)?;
        if self.inventory.is_none() {
            let Some(mut members) = self.children[0].receive_members()? else {
                return Ok(false);
            };
            members.push(Member {
                participant: local,
                parent: None,
            });
            let inventory = Inventory {
                run: self.launch.run,
                external_caller: true,
                members,
            };
            inventory.validate(self.launch.run, true)?;
            let watched = inventory.watches(local)?;
            let mut watch = ProcessWatch::new()?;
            for peer in &watched {
                watch.install(*peer)?;
            }
            self.receipt = Some(Ready {
                run: self.launch.run,
                participant: local,
                parent: None,
                watched,
            });
            self.watch = Some(watch);
            self.inventory = Some(inventory);
        }
        let inventory = self.inventory.as_ref().unwrap();
        if !self.distributed {
            self.distributed = self.children[0].send_inventory(inventory, true)?;
            if !self.distributed {
                return Ok(false);
            }
        }
        if !self.received {
            let Some(mut receipts) = self.children[0].receive_readiness(self.deadline)? else {
                return Ok(false);
            };
            receipts.push(self.receipt.as_ref().unwrap().clone());
            Startup::from_readiness(self.launch.run, true, self.deadline, receipts)?;
            self.received = true;
        }
        // Prepare the caller guard before authorizing the relay to arm storage.
        if self.guard.is_none() {
            let mut routes = RuntimeChannels::new(local, inventory.clone())?;
            for endpoint in self.endpoints.drain(..) {
                routes.insert(endpoint.peer, Channel::new(endpoint.stream)?)?;
            }
            let mut guard = Guard::from_startup(
                local,
                inventory,
                self.launch.capacity,
                self.watch.take().unwrap(),
            )?;
            for route in routes.finish()? {
                guard.route(route.peer(), route);
            }
            self.guard = Some(guard);
        }
        let flushed = self.children[0].publish_caller(if self.queued {
            None
        } else {
            self.receipt.as_ref()
        })?;
        self.queued = true;
        if !flushed {
            return Ok(false);
        }
        self.guard
            .as_mut()
            .unwrap()
            .adopt_ready_children(&mut self.children)?;
        self.complete = true;
        Ok(true)
    }

    /// Transfer the prepared caller runtime after the startup exchange completes.
    pub fn take_guard(&mut self) -> io::Result<Guard> {
        if self.failed || !self.complete {
            return Err(invalid("caller runtime before readiness"));
        }
        self.guard
            .take()
            .ok_or_else(|| invalid("caller runtime already taken"))
    }
}
