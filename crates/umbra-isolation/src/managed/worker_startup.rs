//! Worker startup before opening namespace resources or arming custody.
use super::{
    bootstrap::Bootstrap, executable::Entry, invalid, owner::ProviderOffer, topology::Purpose,
    worker::WorkerCleanup, Channel,
};
use std::io;
use umbra_core::managed::Role;

/// Retains the provider connection and independent owner control separately.
pub struct WorkerStartup {
    /// Worker entry, later used for arming and GO acceptance.
    pub entry: Entry,
    /// Independent control route, later moved into WorkerCleanup.
    pub control: Option<Channel>,
    /// Validated provider offer from the actual parent.
    pub offer: Option<ProviderOffer>,
    /// RPC connection without any provider process handle.
    pub client: Option<umbra_core::provider::Client>,
    bootstrap: Bootstrap,
    ready: bool,
    failed: bool,
    arming: Option<super::arming::WorkerArming>,
    armed: bool,
    authorized: bool,
}

impl WorkerStartup {
    /// Prepare the worker's independent owner control before any run resources.
    pub fn new(mut entry: Entry) -> io::Result<Self> {
        if entry.role != Role::Worker {
            return Err(invalid("worker startup role mismatch"));
        }
        let matches: Vec<_> = entry
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, endpoint)| endpoint.purpose == Purpose::OwnerControl)
            .map(|(index, endpoint)| (index, endpoint.peer))
            .collect();
        let [(index, Role::CleanupOwner)] = matches.as_slice() else {
            return Err(invalid("worker owner control inventory invalid"));
        };
        let endpoint = entry.endpoints.remove(*index);
        Ok(Self {
            entry,
            control: Some(Channel::new(endpoint.stream)?),
            offer: None,
            client: None,
            bootstrap: Bootstrap::new(Vec::new(), Vec::new())?,
            ready: false,
            failed: false,
            arming: None,
            armed: false,
            authorized: false,
        })
    }

    /// Persist Armed through the supplied namespace, then accept the parent's GO.
    /// The callback is invoked at most once by WorkerArming.
    pub fn arm_and_accept_go(
        &mut self,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<bool> {
        if self.failed || self.client.is_none() {
            return Err(invalid("worker arming before handoff or after failure"));
        }
        let result = self.advance_arming(persist);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn advance_arming(
        &mut self,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<bool> {
        if self.authorized {
            return Ok(true);
        }
        if self.arming.is_none() {
            let matches: Vec<_> = self
                .entry
                .endpoints
                .iter()
                .enumerate()
                .filter(|(_, e)| e.peer == Role::Relay && e.purpose == Purpose::Notification)
                .map(|(index, _)| index)
                .collect();
            let [index] = matches.as_slice() else {
                return Err(invalid("worker relay route missing"));
            };
            let endpoint = self.entry.endpoints.remove(*index);
            let external = self
                .entry
                .inventory()
                .ok_or_else(|| invalid("worker inventory missing"))?
                .external_caller;
            self.arming = Some(super::arming::WorkerArming::new(
                Channel::new(endpoint.stream)?,
                self.entry.launch.run,
                self.entry.launch.capacity,
                external,
                self.entry.readiness_deadline,
            )?);
        }
        if !self.armed {
            self.armed = self.arming.as_mut().unwrap().poll(persist)?;
            if !self.armed {
                return Ok(false);
            }
        }
        self.authorized = self.entry.await_go(self.entry.readiness_deadline)?;
        Ok(self.authorized)
    }

    /// Transfer GO-authorized runtime state without any provider child handle.
    pub fn take_runtime(
        &mut self,
    ) -> io::Result<(
        umbra_core::provider::Client,
        super::worker::SharedWorkerCleanup,
        super::fail_stop::WorkerBoundary,
    )> {
        if self.failed || !self.authorized {
            return Err(invalid("worker runtime before durable GO"));
        }
        let boundary = self.entry.worker_boundary()?;
        let inventory = self
            .entry
            .inventory()
            .ok_or_else(|| invalid("worker inventory missing"))?
            .clone();
        let offer = self
            .offer
            .as_ref()
            .ok_or_else(|| invalid("worker provider offer missing"))?;
        let local = super::process::identity(std::process::id(), Role::Worker)?;
        let mut routes = Vec::new();
        let relay = inventory
            .members
            .iter()
            .find(|m| m.participant.role == Role::Relay)
            .ok_or_else(|| invalid("relay missing from inventory"))?
            .participant;
        routes.push(super::publication::Route::new(
            relay,
            self.arming
                .take()
                .ok_or_else(|| invalid("worker arming route transferred"))?
                .into_channel(),
        )?);
        routes.push(super::publication::Route::new(
            offer.owner,
            self.control
                .take()
                .ok_or_else(|| invalid("worker owner route transferred"))?,
        )?);
        for endpoint in self.entry.endpoints.drain(..) {
            match (endpoint.purpose, endpoint.peer) {
                (Purpose::GuardianControl, Role::Guardian)
                | (Purpose::Notification, Role::Caller) => {}
                _ => return Err(invalid("unexpected worker runtime endpoint")),
            }
            let peer = inventory
                .members
                .iter()
                .find(|m| m.participant.role == endpoint.peer)
                .ok_or_else(|| invalid("worker endpoint peer absent"))?
                .participant;
            routes.push(super::publication::Route::new(
                peer,
                Channel::new(endpoint.stream)?,
            )?);
        }
        let mut cleanup = WorkerCleanup::new(
            self.entry.launch.run,
            local,
            offer.owner,
            offer.provider,
            self.entry.launch.capacity,
            routes,
        )?;
        cleanup.install_inventory(&inventory, inventory.external_caller)?;
        Ok((
            self.client
                .take()
                .ok_or_else(|| invalid("worker RPC already transferred"))?,
            super::worker::SharedWorkerCleanup::new(cleanup),
            boundary,
        ))
    }

    /// Complete readiness and RPC receipt. This does not accept GO or arm storage.
    pub fn poll(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        timeout_ms: u64,
        external_caller: bool,
    ) -> io::Result<bool> {
        if self.failed {
            return Err(invalid("worker startup already failed"));
        }
        let result = self.progress(descriptor, timeout_ms, external_caller);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn progress(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        timeout_ms: u64,
        external_caller: bool,
    ) -> io::Result<bool> {
        if self.client.is_some() {
            return Ok(true);
        }
        if !self.ready {
            self.ready = self
                .bootstrap
                .poll(&mut self.entry, &mut [], external_caller)?;
            if !self.ready {
                return Ok(false);
            }
        }
        if !self.entry.flush_readiness()? {
            return Ok(false);
        }
        let control = self
            .control
            .as_mut()
            .ok_or_else(|| invalid("worker control transferred early"))?;
        if self.offer.is_none() {
            self.offer = control.receive::<ProviderOffer>(self.entry.readiness_deadline)?;
        }
        let Some(offer) = &self.offer else {
            return Ok(false);
        };
        let inventory = self
            .entry
            .inventory()
            .ok_or_else(|| invalid("worker inventory missing"))?;
        if ![offer.owner, offer.provider].iter().all(|peer| {
            inventory
                .members
                .iter()
                .any(|member| member.participant == *peer)
        }) {
            return Err(invalid("provider offer differs from startup inventory"));
        }
        self.client = WorkerCleanup::receive_offer(
            offer,
            self.entry.launch.run,
            control.handoff_stream()?,
            descriptor,
            timeout_ms,
            self.entry.readiness_deadline,
        )?;
        Ok(self.client.is_some())
    }
}
