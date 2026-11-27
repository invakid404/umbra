//! Cleanup-owner startup retaining process custody on every failed step.
use super::{
    bootstrap::Bootstrap,
    executable::Entry,
    invalid,
    owner::{Handoff, Launch},
    topology::Purpose,
    Channel,
};
use std::io;
use umbra_core::managed::Role;

/// Startup resources remain available for parent-owned rollback after errors.
pub struct OwnerStartup {
    /// Registered cleanup-owner entry and its remaining endpoints.
    pub entry: Entry,
    /// Actual provider child, including unsuccessful handshake launches.
    pub launch: Launch,
    /// Prepared RPC transfer; retained until runtime adoption.
    pub handoff: Option<Handoff>,
    /// Startup watches and the independent provider bootstrap.
    pub bootstrap: Option<Bootstrap>,
    /// Worker control route, later used by the owner service.
    pub control: Option<Channel>,
    /// Provider custody retained if runtime construction fails after adoption.
    pub owner: Option<super::owner::Owner>,
    phase: u8,
    stalled: bool,
    failed: bool,
}

impl OwnerStartup {
    /// Collect a provider that never received GO; preserve the actual Child
    /// in Launch on errors or expiry. No acquisition is authorized in this phase.
    pub fn rollback(&mut self) -> io::Result<bool> {
        self.failed = true;
        if self.owner.is_some() {
            return Err(invalid("owner rollback after runtime adoption"));
        }
        self.entry.close_before_go()?;
        self.bootstrap.take();
        self.handoff.take();
        self.control.take();
        let Some(child) = self.launch.child.as_mut() else {
            return Ok(true);
        };
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        let deadline = self
            .entry
            .readiness_deadline
            .checked_add(9_000_000_000)
            .ok_or_else(|| invalid("rollback clock overflow"))?;
        if super::boot_nanos()? >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        child.kill()?;
        Ok(child.try_wait()?.is_some())
    }

    /// Retain the entry before any provider acquisition.
    pub fn new(entry: Entry) -> Self {
        Self {
            entry,
            launch: Launch::default(),
            handoff: None,
            bootstrap: None,
            control: None,
            owner: None,
            phase: 0,
            stalled: false,
            failed: false,
        }
    }

    /// Adopt all runtime routes after GO. On error this object still owns the
    /// provider through either `launch` or `owner`, for bounded rollback.
    pub fn take_service(&mut self) -> io::Result<super::owner::Service> {
        if self.failed || self.phase != 5 {
            return Err(invalid("runtime adoption before GO or after failure"));
        }
        let result = self.adopt_service();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn adopt_service(&mut self) -> io::Result<super::owner::Service> {
        use super::publication::Route;
        let inventory = self
            .entry
            .inventory()
            .ok_or_else(|| invalid("owner runtime inventory missing"))?
            .clone();
        let participant = super::process::identity(std::process::id(), Role::CleanupOwner)?;
        let worker = inventory
            .members
            .iter()
            .find(|m| m.participant.role == Role::Worker)
            .ok_or_else(|| invalid("worker missing from inventory"))?
            .participant;
        let provider = self
            .handoff
            .as_ref()
            .ok_or_else(|| invalid("provider handoff missing"))?
            .provider;
        let mut admission = None;
        let mut cancellation = None;
        let mut routes = Vec::new();
        for endpoint in self.entry.endpoints.drain(..) {
            let peer = inventory
                .members
                .iter()
                .find(|m| m.participant.role == endpoint.peer)
                .ok_or_else(|| invalid("endpoint peer missing from inventory"))?
                .participant;
            let channel = Channel::new(endpoint.stream)?;
            match (endpoint.purpose, endpoint.peer) {
                (Purpose::Admission, Role::Provider) if admission.is_none() => {
                    admission = Some(channel)
                }
                (Purpose::Cancellation, Role::Provider) if cancellation.is_none() => {
                    cancellation = Some(channel)
                }
                (Purpose::Notification, Role::Caller | Role::Relay)
                | (Purpose::GuardianControl, Role::Guardian) => {
                    routes.push(Route::new(peer, channel)?)
                }
                _ => return Err(invalid("unexpected owner runtime endpoint")),
            }
        }
        let admission = admission.ok_or_else(|| invalid("owner admission missing"))?;
        let cancellation = cancellation.ok_or_else(|| invalid("owner cancellation missing"))?;
        let watch = self
            .bootstrap
            .take()
            .ok_or_else(|| invalid("owner bootstrap missing"))?
            .into_watch()?
            .ok_or_else(|| invalid("owner startup watch missing"))?;
        let child = self
            .launch
            .child
            .take()
            .ok_or_else(|| invalid("provider custody missing"))?;
        let rpc_cancellation = self.handoff.take().unwrap().into_cancellation();
        match super::owner::Owner::new(
            participant,
            self.entry.launch.run,
            self.entry.launch.capacity,
            child,
            rpc_cancellation,
            routes,
        ) {
            Ok(owner) => self.owner = Some(owner),
            Err((error, child, _, _)) => {
                self.launch.child = Some(child);
                return Err(error);
            }
        }
        let owner = self.owner.as_mut().unwrap();
        owner.install_admission(admission)?;
        owner.install_cancellation(cancellation)?;
        owner.install_watch(watch, &inventory)?;
        let control = self
            .control
            .take()
            .ok_or_else(|| invalid("worker control missing"))?;
        match super::owner::Service::new(self.owner.take().unwrap(), worker, provider, control) {
            Ok(service) => {
                self.phase = 6;
                Ok(service)
            }
            Err(failure) => {
                let (error, owner, control) = *failure;
                self.owner = Some(owner);
                self.control = Some(control);
                Err(error)
            }
        }
    }

    /// Advance startup. True means the owner accepted and forwarded durable GO.
    /// Any error permanently stops advancement but preserves child custody.
    pub fn poll(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        external_caller: bool,
        timeout_ms: u64,
    ) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("owner startup already failed"));
        }
        let phase = self.phase;
        let result = self.progress(descriptor, external_caller, timeout_ms);
        self.stalled = self.phase == phase;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Wait on the stalled phase without delaying local transitions.
    pub fn wait_for_activity(&self) -> io::Result<()> {
        if !self.stalled {
            return Ok(());
        }
        match self.phase {
            1 => self
                .bootstrap
                .as_ref()
                .unwrap()
                .wait_for_activity(&self.entry, &[]),
            2 => self
                .control
                .as_ref()
                .unwrap()
                .wait_until(self.entry.readiness_deadline),
            3 => self.entry.wait_for_startup(),
            4 => self.bootstrap.as_ref().unwrap().wait_for_provider(),
            _ => Err(invalid("owner wait outside startup")),
        }
    }

    fn progress(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        external_caller: bool,
        timeout_ms: u64,
    ) -> io::Result<bool> {
        match self.phase {
            0 => {
                if self.entry.role != Role::CleanupOwner {
                    return Err(invalid("owner startup in another role"));
                }
                let matches: Vec<_> = self
                    .entry
                    .endpoints
                    .iter()
                    .enumerate()
                    .filter(|(_, endpoint)| endpoint.purpose == Purpose::OwnerControl)
                    .map(|(index, endpoint)| (index, endpoint.peer))
                    .collect();
                let [(index, Role::Worker)] = matches.as_slice() else {
                    return Err(invalid("owner worker control inventory invalid"));
                };
                let endpoint = self.entry.endpoints.remove(*index);
                self.control = Some(Channel::new(endpoint.stream)?);
                let (handoff, provider) =
                    self.entry
                        .connect_provider(&mut self.launch, descriptor, timeout_ms)?;
                self.handoff = Some(handoff);
                self.bootstrap = Some(Bootstrap::with_provider(provider)?);
                self.phase = 1;
            }
            1 => {
                if self.bootstrap.as_mut().unwrap().poll(
                    &mut self.entry,
                    &mut [],
                    external_caller,
                )? {
                    self.phase = 2;
                }
            }
            2 => {
                if self.handoff.as_mut().unwrap().publish(
                    self.control.as_mut().unwrap(),
                    self.entry.launch.run,
                    self.entry.readiness_deadline,
                )? {
                    self.phase = 3;
                }
            }
            3 => {
                if self.entry.await_go(self.entry.readiness_deadline)? {
                    self.phase = 4;
                }
            }
            4 => {
                if self
                    .bootstrap
                    .as_mut()
                    .unwrap()
                    .forward_provider_go(&self.entry)?
                {
                    self.phase = 5;
                }
            }
            5 => return Ok(true),
            6 => return Err(invalid("owner runtime already transferred")),
            _ => unreachable!(),
        }
        Ok(self.phase == 5)
    }
}
