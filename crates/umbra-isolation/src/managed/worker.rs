//! Worker adapter for independent publication and provider-parent collection.
use super::{
    boot_nanos, invalid,
    publication::{Publication, Route},
};
use std::{
    io,
    time::{Duration, Instant},
};
use umbra_core::managed::{
    CleanupPath, ControlMessage, Participant, ProviderReceipt, RunIdentity, Start, Ticket,
    TicketState,
};
use umbra_core::{ErrorKind, UmbraError};
/// Platform cleanup contract implemented by the worker adapters.
pub use umbra_platform::provider::ManagedCleanup;

/// Worker owns control endpoints, never the platform provider child.
pub struct WorkerCleanup {
    run: RunIdentity,
    local: Participant,
    owner: Participant,
    provider: Participant,
    capacity: u64,
    tickets: TicketState,
    routes: Vec<Route>,
    pending_ack: Vec<Option<u64>>,
    retired: Vec<bool>,
    forwarding: Vec<std::collections::VecDeque<Start>>,
    awaiting: Vec<Option<Start>>,
    forwarded: Vec<Start>,
    inventory: Option<Vec<Participant>>,
    receipt: Option<ProviderReceipt>,
}

/// Thread-affine shared cleanup state for the worker run owner and platform proxy.
/// Every user of this handle retains the same ticket and parent-wait receipt.
#[derive(Clone)]
pub struct SharedWorkerCleanup {
    inner: std::rc::Rc<std::cell::RefCell<WorkerCleanup>>,
    run: RunIdentity,
    local: Participant,
}

impl SharedWorkerCleanup {
    /// Progress independent notifications while an observer response is pending.
    pub fn observer_progress(&self) -> umbra_core::Result<Option<u64>> {
        let mut cleanup = self.inner.try_borrow_mut().map_err(error)?;
        cleanup.poll_notifications().map_err(error)?;
        Ok(cleanup.tickets.ticket().map(|ticket| ticket.deadline))
    }

    /// Wait for observer I/O or independent cleanup notifications together.
    pub fn observer_wait(&self, channel: &super::Channel, deadline: u64) -> umbra_core::Result<()> {
        self.inner
            .try_borrow_mut()
            .map_err(error)?
            .wait_for_activity(Some((channel, deadline)))
            .map_err(error)
    }

    /// Share an enrolled worker's independent notification routes.
    pub fn new(cleanup: WorkerCleanup) -> Self {
        Self {
            run: cleanup.run,
            local: cleanup.local,
            inner: std::rc::Rc::new(std::cell::RefCell::new(cleanup)),
        }
    }

    /// Publish failure and end the registered worker without unwinding owners.
    pub fn fail_stop(
        &self,
        boundary: super::fail_stop::WorkerBoundary,
        failure: &UmbraError,
    ) -> std::io::Result<std::convert::Infallible> {
        boundary.fail_stop_shared(self, failure)
    }

    pub(super) fn belongs_to(&self, run: RunIdentity, local: Participant) -> bool {
        self.run == run && self.local == local
    }
}

impl ManagedCleanup for SharedWorkerCleanup {
    fn provider_receipt(&self) -> Option<umbra_core::managed::ProviderReceipt> {
        self.inner.try_borrow().ok()?.provider_receipt()
    }
    fn start(&mut self) -> umbra_core::Result<Instant> {
        super::executable::startup_observation(
            "cleanup-start-requested",
            Some(umbra_core::managed::Role::Worker),
        );
        self.inner.try_borrow_mut().map_err(error)?.start()
    }
    fn failed(&mut self, failure: &UmbraError) {
        self.inner.borrow_mut().failed(failure);
    }
    fn collect_provider(&mut self) -> umbra_core::Result<Duration> {
        super::executable::startup_observation(
            "provider-collection-begin",
            Some(umbra_core::managed::Role::Worker),
        );
        self.inner
            .try_borrow_mut()
            .map_err(error)?
            .collect_provider()
    }
    fn abandon(&mut self) {
        self.inner.borrow_mut().abandon();
    }
}

fn error(error: impl std::fmt::Display) -> UmbraError {
    UmbraError::new(
        ErrorKind::ProtocolMismatch,
        "managed.cleanup",
        error.to_string(),
    )
}

fn forwarding_cutoff(start: Option<&Start>, deadline: u64) -> io::Result<u64> {
    match start {
        Some(start) => Ok(deadline.min(
            start
                .ticket
                .started
                .checked_add(umbra_core::managed::ACK_NANOS)
                .ok_or_else(|| invalid("forwarding ACK clock overflow"))?,
        )),
        None => Ok(deadline),
    }
}

impl WorkerCleanup {
    fn collection_complete(&self) -> bool {
        self.receipt.is_some()
            && self.pending_ack.iter().all(Option::is_none)
            && self.awaiting.iter().all(Option::is_none)
            && self.forwarding.iter().all(|queue| queue.is_empty())
    }

    pub(super) fn belongs_to(&self, run: RunIdentity, local: Participant) -> bool {
        self.run == run && self.local == local
    }

    fn forward(&mut self, start: Start, source: Participant) -> io::Result<()> {
        if self.forwarded.contains(&start) {
            return Ok(());
        }
        if self.forwarded.len() >= 16 {
            self.tickets.fail();
            return Err(invalid("worker forwarding bound exceeded"));
        }
        self.forwarded.push(start.clone());
        for (index, (route, queue)) in self.routes.iter().zip(&mut self.forwarding).enumerate() {
            if !self.retired[index] && route.peer() != source {
                queue.push_back(start.clone());
            }
        }
        Ok(())
    }

    fn progress_forwarding(&mut self) -> io::Result<()> {
        for index in 0..self.routes.len() {
            if self.retired[index] || self.pending_ack[index].is_some() {
                continue;
            }
            let result = (|| -> io::Result<()> {
                if self.awaiting[index].is_none() {
                    if let Some(start) = self.forwarding[index].pop_front() {
                        self.routes[index]
                            .channel()
                            .queue(&ControlMessage::Start(start.clone()))?;
                        self.awaiting[index] = Some(start);
                    }
                }
                if let Some(start) = &self.awaiting[index] {
                    let deadline = start
                        .ticket
                        .started
                        .checked_add(umbra_core::managed::ACK_NANOS)
                        .ok_or_else(|| invalid("worker forwarding clock overflow"))?
                        .min(self.tickets.ticket().unwrap().deadline);
                    self.routes[index].channel().flush(deadline)?;
                }
                Ok(())
            })();
            if let Err(failure) = result {
                self.tickets.fail();
                if self.routes[index].peer() == self.owner {
                    return Err(failure);
                }
                self.retired[index] = true;
                self.pending_ack[index] = None;
                self.awaiting[index] = None;
                self.forwarding[index].clear();
            }
        }
        Ok(())
    }

    /// Poll independent routes while the worker is running. There is no idle
    /// timeout: only an accepted Start establishes a cleanup deadline. Returned
    /// notifications must be passed to the worker's finish arbitration.
    pub fn poll_notifications(&mut self) -> io::Result<Vec<Start>> {
        let result = self.poll_notifications_inner();
        if result.is_err() {
            self.tickets.fail();
        }
        result
    }

    fn poll_notifications_inner(&mut self) -> io::Result<Vec<Start>> {
        self.progress_forwarding()?;
        let inventory = self.inventory.clone().unwrap_or_else(|| {
            self.routes
                .iter()
                .map(Route::peer)
                .chain([self.local, self.owner, self.provider])
                .collect()
        });
        let mut notifications = Vec::new();
        let mut forwarding = Vec::new();
        for (index, (route, pending)) in self
            .routes
            .iter_mut()
            .zip(&mut self.pending_ack)
            .enumerate()
        {
            if self.retired[index] {
                continue;
            }
            let peer = route.peer();
            if peer == self.owner
                && self.receipt.is_some()
                && self.awaiting[index].is_none()
                && pending.is_none()
            {
                continue;
            }
            let channel = route.channel();
            let deadline = self
                .tickets
                .ticket()
                .map_or(u64::MAX, |ticket| ticket.deadline);
            let deadline = forwarding_cutoff(self.awaiting[index].as_ref(), deadline)?;
            if let Some(ack_deadline) = *pending {
                if !channel.flush(ack_deadline.min(deadline))? {
                    continue;
                }
                *pending = None;
            }
            if !channel.flush(deadline)? {
                continue;
            }
            let Some(message) = channel.receive::<ControlMessage>(deadline)? else {
                continue;
            };
            if let ControlMessage::Ack(ack) = message {
                let start = self.awaiting[index]
                    .as_ref()
                    .ok_or_else(|| invalid("unsolicited worker ACK"))?;
                ack.validate(start, peer).map_err(invalid)?;
                if !inventory.contains(&ack.committed.origin) {
                    return Err(invalid("worker ACK has unknown origin"));
                }
                self.tickets
                    .accept(ack.committed.clone(), ack.committed.origin, self.local)
                    .map_err(invalid)?;
                self.awaiting[index] = None;
                continue;
            }
            let ControlMessage::Start(start) = message else {
                return Err(invalid("unexpected idle worker control message"));
            };
            if !inventory.contains(&start.origin) {
                return Err(invalid("idle Start has unknown origin"));
            }
            let ack = self
                .tickets
                .accept(start.clone(), start.origin, self.local)
                .map_err(invalid)?;
            let cutoff = start
                .ticket
                .started
                .checked_add(umbra_core::managed::ACK_NANOS)
                .ok_or_else(|| invalid("idle ACK deadline overflow"))?
                .min(self.tickets.ticket().unwrap().deadline);
            channel.queue(&ControlMessage::Ack(ack))?;
            *pending = Some(cutoff);
            notifications.push(start.clone());
            forwarding.push((peer, start));
        }
        for (peer, start) in forwarding {
            self.forward(start, peer)?;
        }
        Ok(notifications)
    }

    /// Bind all startup identities before Start publication. Rebinding or late
    /// installation fails; full origins remain known even without a direct route.
    pub fn install_inventory(
        &mut self,
        inventory: &super::inventory::Inventory,
        external_caller: bool,
    ) -> io::Result<()> {
        let result = (|| {
            if self.tickets.ticket().is_some() {
                return Err(invalid("inventory after cleanup Start"));
            }
            inventory.validate(self.run, external_caller)?;
            let participants: Vec<_> = inventory.members.iter().map(|m| m.participant).collect();
            if [self.local, self.owner, self.provider]
                .iter()
                .any(|p| !participants.contains(p))
                || self
                    .routes
                    .iter()
                    .any(|route| !participants.contains(&route.peer()))
                || self
                    .inventory
                    .as_ref()
                    .is_some_and(|prior| prior != &participants)
            {
                return Err(invalid("cleanup inventory differs from owned identities"));
            }
            self.inventory = Some(participants);
            Ok(())
        })();
        if result.is_err() {
            self.tickets.fail();
        }
        result
    }

    /// Worker-owned namespace persistence barrier. The callback must persist
    /// through the authoritative namespace and use the supplied absolute cutoff.
    pub fn arm(
        &self,
        startup: &mut super::startup::Startup,
        record: umbra_core::CustodyRecord,
        persist: impl FnOnce(&umbra_core::CustodyRecord, u64) -> io::Result<umbra_core::Sequence>,
    ) -> io::Result<super::startup::Go> {
        if self.local.role != umbra_core::managed::Role::Worker
            || super::process::identity(std::process::id(), self.local.role)? != self.local
        {
            return Err(invalid("Armed persistence outside worker role"));
        }
        startup.persist_armed(self.run, record, persist)
    }

    /// Adopt the owner's one-shot descriptor under the shared startup cutoff.
    /// The authenticated Welcome arrives on the startup control exchange; this
    /// method neither spawns nor adopts provider wait authority.
    pub fn receive_provider(
        &self,
        route: &std::os::unix::net::UnixStream,
        welcome: &umbra_core::provider::Welcome,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        timeout_ms: u64,
        readiness_deadline: u64,
    ) -> io::Result<Option<umbra_core::provider::Client>> {
        Self::receive_offer(
            &super::owner::ProviderOffer {
                run: self.run,
                owner: self.owner,
                provider: self.provider,
                welcome: welcome.clone(),
            },
            self.run,
            route,
            descriptor,
            timeout_ms,
            readiness_deadline,
        )
    }

    /// Receive the owner handoff before building runtime cleanup routes.
    /// Validate the offered run and kernel parent edge before consuming its fd.
    pub fn receive_offer(
        offer: &super::owner::ProviderOffer,
        run: RunIdentity,
        route: &std::os::unix::net::UnixStream,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        timeout_ms: u64,
        readiness_deadline: u64,
    ) -> io::Result<Option<umbra_core::provider::Client>> {
        offer.validate(run)?;
        let welcome = &offer.welcome;
        if boot_nanos()? >= readiness_deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if timeout_ms == 0
            || timeout_ms > 60_000
            || welcome.role != "platform"
            || welcome.id != descriptor.id
            || welcome.role != descriptor.role
            || welcome.version != descriptor.protocol_version
            || !descriptor.capabilities.is_subset(&welcome.capabilities)
            || !welcome
                .capabilities
                .contains(umbra_core::shutdown::CAPABILITY)
        {
            return Err(invalid("invalid worker provider handoff"));
        }
        let Some(stream) = umbra_core::managed_handoff::receive(route)? else {
            return Ok(None);
        };
        let (provider, parent) =
            super::process::identity_with_parent(offer.provider.pid, offer.provider.role)?;
        if provider != offer.provider
            || parent != offer.owner.pid
            || super::process::identity(offer.owner.pid, offer.owner.role)? != offer.owner
        {
            return Err(invalid("provider handoff parent or birth mismatch"));
        }
        Ok(Some(umbra_core::provider::Client::from_connection(
            umbra_core::provider::Connection::new(stream, Duration::from_millis(timeout_ms)),
            welcome.clone(),
        )))
    }

    /// Bind the complete pre-GO identities and independently owned direct routes.
    pub fn new(
        run: RunIdentity,
        local: Participant,
        owner: Participant,
        provider: Participant,
        capacity: u64,
        routes: Vec<Route>,
    ) -> io::Result<Self> {
        for participant in [local, owner, provider] {
            participant.validate().map_err(invalid)?;
        }
        if !routes.iter().any(|route| route.peer() == owner) {
            return Err(invalid("cleanup owner route missing"));
        }
        Ok(Self {
            run,
            local,
            owner,
            provider,
            capacity,
            tickets: TicketState::new(run, capacity).map_err(invalid)?,
            pending_ack: vec![None; routes.len()],
            retired: vec![false; routes.len()],
            forwarding: vec![std::collections::VecDeque::new(); routes.len()],
            awaiting: vec![None; routes.len()],
            forwarded: Vec::new(),
            routes,
            inventory: None,
            receipt: None,
        })
    }

    fn publish(&mut self, path: CleanupPath) -> io::Result<()> {
        if self.tickets.ticket().is_some() {
            return Ok(());
        }
        let start = Start {
            run: self.run,
            origin: self.local,
            ticket: Ticket::new(boot_nanos()?, path, self.capacity).map_err(invalid)?,
        };
        let mut publication = Publication::begin(
            start,
            self.local,
            self.local,
            &mut self.tickets,
            std::mem::take(&mut self.routes),
        )?;
        if let Some(inventory) = &self.inventory {
            publication.inventory(inventory);
        }
        let result = loop {
            match publication.progress(&mut self.tickets) {
                Ok(true) => {
                    break if publication.failures().is_empty() {
                        Ok(())
                    } else {
                        Err(invalid("Start publication failed"))
                    };
                }
                Ok(false) => {
                    if let Err(error) = publication.wait() {
                        break Err(error);
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let received = publication.received().to_vec();
        self.routes = publication.into_routes();
        for (peer, start) in received {
            self.forward(start, peer)?;
        }
        result
    }

    fn wait_for_collection(&mut self) -> io::Result<()> {
        if self.tickets.ticket().is_none() {
            return Err(invalid("collection before Start"));
        }
        self.wait_for_activity(None)
    }

    fn wait_for_activity(&mut self, external: Option<(&super::Channel, u64)>) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut deadline = self
            .tickets
            .ticket()
            .map_or(u64::MAX, |ticket| ticket.deadline);
        let mut descriptors = Vec::new();
        if let Some((channel, cutoff)) = external {
            deadline = deadline.min(cutoff);
            let (fd, writing) = channel.wait_interest();
            descriptors.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            });
        }
        for (index, route) in self.routes.iter_mut().enumerate() {
            if self.retired[index] {
                continue;
            }
            deadline = forwarding_cutoff(self.awaiting[index].as_ref(), deadline)?;
            if let Some(ack) = self.pending_ack[index] {
                deadline = deadline.min(ack);
            }
            // Queued forwarding can advance locally without a socket event.
            if !self.forwarding[index].is_empty()
                && self.awaiting[index].is_none()
                && self.pending_ack[index].is_none()
            {
                return Ok(());
            }
            let channel = route.channel();
            let writing = channel.written != channel.output.len();
            if !writing && self.receipt.is_some() && self.awaiting[index].is_none() {
                continue;
            }
            descriptors.push(libc::pollfd {
                fd: channel.stream.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            });
        }
        super::readiness::wait_descriptors(&mut descriptors, deadline)
    }

    fn request(&mut self, message: ControlMessage) -> io::Result<()> {
        let deadline = self
            .tickets
            .ticket()
            .ok_or_else(|| invalid("owner request before Start"))?
            .deadline
            .min(
                boot_nanos()?
                    .checked_add(250_000_000)
                    .ok_or_else(|| invalid("owner publication clock overflow"))?,
            );
        let index = self
            .routes
            .iter()
            .position(|route| route.peer() == self.owner)
            .ok_or_else(|| invalid("owner route lost"))?;
        use std::os::fd::AsRawFd;
        let readiness = super::readiness::Readiness::new()?;
        let channel = self.routes[index].channel();
        let deadline = forwarding_cutoff(self.awaiting[index].as_ref(), deadline)?;
        if let Some(ack_deadline) = self.pending_ack[index] {
            while !channel.flush(ack_deadline.min(deadline))? {
                readiness.wait(channel.stream.as_raw_fd(), true, ack_deadline.min(deadline))?;
            }
            self.pending_ack[index] = None;
        }
        while !channel.flush(deadline)? {
            readiness.wait(channel.stream.as_raw_fd(), true, deadline)?;
        }
        channel.queue(&message)?;
        while !channel.flush(deadline)? {
            readiness.wait(channel.stream.as_raw_fd(), true, deadline)?;
        }
        Ok(())
    }
}

impl ManagedCleanup for WorkerCleanup {
    fn provider_receipt(&self) -> Option<ProviderReceipt> {
        self.receipt
            .as_ref()
            .filter(|receipt| {
                self.collection_complete()
                    && !receipt.failed
                    && receipt.status == 0
                    && !self.tickets.failed()
            })
            .cloned()
    }
    fn start(&mut self) -> umbra_core::Result<Instant> {
        self.publish(CleanupPath::Trusted).map_err(error)?;
        let cutoff = self
            .tickets
            .ticket()
            .unwrap()
            .cutoffs(self.capacity)
            .map_err(error)?
            .cooperative;
        let remaining = cutoff.saturating_sub(boot_nanos().map_err(error)?);
        Instant::now()
            .checked_add(Duration::from_nanos(remaining))
            .ok_or_else(|| error("local deadline overflow"))
    }

    fn failed(&mut self, failure: &UmbraError) {
        self.tickets.fail();
        if self.publish(CleanupPath::Failed).is_ok()
            && self
                .request(ControlMessage::Cancel {
                    run: self.run,
                    reason: failure.to_string(),
                })
                .is_err()
        {
            self.tickets.fail();
        }
    }

    fn collect_provider(&mut self) -> umbra_core::Result<Duration> {
        let began = Instant::now();
        if !self.collection_complete() {
            if self.receipt.is_none() {
                self.request(ControlMessage::Collect { run: self.run })
                    .map_err(error)?;
            }
            let inventory = self.inventory.clone().unwrap_or_else(|| {
                self.routes
                    .iter()
                    .map(Route::peer)
                    .chain([self.local, self.owner, self.provider])
                    .collect()
            });
            loop {
                self.progress_forwarding().map_err(error)?;
                let mut forwarding = Vec::new();
                for (index, (route, ack_deadline)) in self
                    .routes
                    .iter_mut()
                    .zip(&mut self.pending_ack)
                    .enumerate()
                {
                    if self.retired[index] {
                        continue;
                    }
                    let peer = route.peer();
                    let channel = route.channel();
                    if let Some(deadline) = *ack_deadline {
                        match channel.flush(deadline.min(self.tickets.ticket().unwrap().deadline)) {
                            Ok(true) => *ack_deadline = None,
                            Ok(false) => continue,
                            Err(failure) => {
                                self.tickets.fail();
                                if peer == self.owner {
                                    return Err(error(failure));
                                }
                                self.retired[index] = true;
                                *ack_deadline = None;
                                self.awaiting[index] = None;
                                self.forwarding[index].clear();
                                continue;
                            }
                        }
                    }
                    if self.receipt.is_some() && self.awaiting[index].is_none() {
                        continue;
                    }
                    let deadline = forwarding_cutoff(
                        self.awaiting[index].as_ref(),
                        self.tickets.ticket().unwrap().deadline,
                    )
                    .map_err(error)?;
                    match channel.flush(deadline) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(failure) => {
                            self.tickets.fail();
                            if peer == self.owner {
                                return Err(error(failure));
                            }
                            self.retired[index] = true;
                            *ack_deadline = None;
                            self.awaiting[index] = None;
                            self.forwarding[index].clear();
                            continue;
                        }
                    }
                    let message = match channel.receive::<ControlMessage>(deadline) {
                        Ok(message) => message,
                        Err(failure) => {
                            self.tickets.fail();
                            if peer == self.owner {
                                return Err(error(failure));
                            }
                            self.retired[index] = true;
                            *ack_deadline = None;
                            self.awaiting[index] = None;
                            self.forwarding[index].clear();
                            continue;
                        }
                    };
                    match message {
                        None => {}
                        Some(ControlMessage::Start(start)) => {
                            if !inventory.contains(&start.origin) {
                                self.tickets.fail();
                                return Err(error("collection Start has unknown origin"));
                            }
                            let deadline = start
                                .ticket
                                .started
                                .checked_add(umbra_core::managed::ACK_NANOS)
                                .ok_or_else(|| error("collection ACK deadline overflow"))?
                                .min(start.ticket.deadline);
                            let ack = self
                                .tickets
                                .accept(start.clone(), start.origin, self.local)
                                .map_err(error)?;
                            if let Err(failure) = channel.queue(&ControlMessage::Ack(ack)) {
                                self.tickets.fail();
                                if peer == self.owner {
                                    return Err(error(failure));
                                }
                                self.retired[index] = true;
                                *ack_deadline = None;
                                self.awaiting[index] = None;
                                self.forwarding[index].clear();
                                continue;
                            }
                            *ack_deadline = Some(deadline);
                            forwarding.push((peer, start));
                        }
                        Some(ControlMessage::Ack(ack)) => {
                            let start = self.awaiting[index]
                                .as_ref()
                                .ok_or_else(|| error("unsolicited collection ACK"))?;
                            ack.validate(start, peer).map_err(error)?;
                            if !inventory.contains(&ack.committed.origin) {
                                self.tickets.fail();
                                return Err(error("collection ACK has unknown origin"));
                            }
                            self.tickets
                                .accept(ack.committed.clone(), ack.committed.origin, self.local)
                                .map_err(error)?;
                            self.awaiting[index] = None;
                        }
                        Some(ControlMessage::Collected(receipt)) => {
                            if peer != self.owner
                                || receipt.run != self.run
                                || receipt.owner != self.owner
                                || receipt.provider != self.provider
                            {
                                self.tickets.fail();
                                return Err(error("provider collection identity mismatch"));
                            }
                            self.receipt = Some(receipt);
                        }
                        Some(_) => {
                            self.tickets.fail();
                            return Err(error("unexpected collection control message"));
                        }
                    }
                }
                for (peer, start) in forwarding {
                    self.forward(start, peer).map_err(error)?;
                }
                if self.collection_complete() {
                    break;
                }
                self.wait_for_collection().map_err(error)?;
            }
        }
        let receipt = self.receipt.as_ref().unwrap();
        super::executable::startup_observation(
            "provider-collection-received",
            Some(umbra_core::managed::Role::Worker),
        );
        #[cfg(feature = "shutdown-witness")]
        umbra_platform::shutdown::witness(serde_json::json!({
            "event": "provider-parent-wait",
            "receipt": receipt,
            "cleanup_failed": receipt.failed || self.tickets.failed(),
        }));
        if receipt.failed || receipt.status != 0 || self.tickets.failed() {
            return Err(error("managed cleanup retained a failure"));
        }
        Ok(began.elapsed())
    }

    fn abandon(&mut self) {
        let failure = error("worker platform dropped before completion");
        self.failed(&failure);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::{
        owner::{Owner, Service},
        process::identity,
        Channel,
    };
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use umbra_core::managed::Role;

    #[test]
    fn closed_caller_route_does_not_prevent_owner_collection() {
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let caller = Participant {
            role: Role::Caller,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (owner_socket, owner_peer) = UnixStream::pair().unwrap();
        let (caller_socket, caller_peer) = UnixStream::pair().unwrap();
        let mut worker = WorkerCleanup::new(
            run,
            local,
            owner,
            provider,
            1,
            vec![
                Route::new(caller, Channel::new(caller_socket).unwrap()).unwrap(),
                Route::new(owner, Channel::new(owner_socket).unwrap()).unwrap(),
            ],
        )
        .unwrap();
        let start = Start {
            run,
            origin: local,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
        };
        worker.tickets.accept(start.clone(), local, local).unwrap();
        drop(caller_peer);
        let deadline = start.ticket.deadline;
        std::thread::scope(|scope| {
            let peer = scope.spawn(move || {
                let mut channel = Channel::new(owner_peer).unwrap();
                loop {
                    if let Some(message) = channel.receive::<ControlMessage>(deadline).unwrap() {
                        assert!(matches!(message, ControlMessage::Collect { run: found } if found == run));
                        break;
                    }
                    assert!(boot_nanos().unwrap() < deadline);
                    std::thread::yield_now();
                }
                channel.queue(&ControlMessage::Collected(ProviderReceipt {
                    run, owner, provider, status: 0, failed: false,
                })).unwrap();
                while !channel.flush(deadline).unwrap() { std::thread::yield_now(); }
            });
            assert!(
                worker.collect_provider().is_err(),
                "closed caller remains failed"
            );
            peer.join().unwrap();
        });
        assert!(
            worker.receipt.is_some(),
            "live owner must still be collected"
        );
        assert!(
            worker.provider_receipt().is_none(),
            "failure must remain sticky"
        );
        assert_eq!(worker.tickets.ticket(), Some(start.ticket));
    }

    #[test]
    fn stored_receipt_waits_for_exchange_drain_on_retry() {
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (socket, peer) = UnixStream::pair().unwrap();
        let mut worker = WorkerCleanup::new(
            run,
            local,
            owner,
            provider,
            1,
            vec![Route::new(owner, Channel::new(socket).unwrap()).unwrap()],
        )
        .unwrap();
        let start = Start {
            run,
            origin: owner,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
        };
        let ack = worker.tickets.accept(start.clone(), owner, local).unwrap();
        worker.routes[0]
            .channel()
            .queue(&ControlMessage::Ack(ack.clone()))
            .unwrap();
        worker.pending_ack[0] = Some(start.ticket.started + umbra_core::managed::ACK_NANOS);
        worker.receipt = Some(ProviderReceipt {
            run,
            owner,
            provider,
            status: 0,
            failed: false,
        });
        assert!(
            worker.provider_receipt().is_none(),
            "receipt arrival is not exchange completion"
        );
        worker.collect_provider().unwrap();
        assert!(worker.provider_receipt().is_some());
        let mut peer = Channel::new(peer).unwrap();
        assert!(
            matches!(peer.receive::<ControlMessage>(start.ticket.deadline).unwrap(),
            Some(ControlMessage::Ack(found)) if found == ack)
        );
        assert!(
            peer.receive::<ControlMessage>(start.ticket.deadline)
                .unwrap()
                .is_none(),
            "receipt retry must not issue another Collect"
        );
    }

    #[test]
    fn observer_wait_wakes_for_independent_cleanup() {
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (socket, peer) = UnixStream::pair().unwrap();
        let cleanup = SharedWorkerCleanup::new(
            WorkerCleanup::new(
                run,
                local,
                owner,
                provider,
                1,
                vec![Route::new(owner, Channel::new(socket).unwrap()).unwrap()],
            )
            .unwrap(),
        );
        let (observer, _silent_observer) = UnixStream::pair().unwrap();
        let observer = Channel::new(observer).unwrap();
        let start = Start {
            run,
            origin: owner,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Failed, 1).unwrap(),
        };
        let mut peer = Channel::new(peer).unwrap();
        peer.queue(&ControlMessage::Start(start.clone())).unwrap();
        assert!(peer.flush(start.ticket.deadline).unwrap());
        let cutoff = boot_nanos().unwrap() + 1_000_000_000;
        cleanup.observer_wait(&observer, cutoff).unwrap();
        assert!(
            boot_nanos().unwrap() < cutoff,
            "cleanup route must wake a silent observer"
        );
        assert_eq!(
            cleanup.observer_progress().unwrap(),
            Some(start.ticket.deadline)
        );
    }

    #[test]
    fn forwarded_ack_uses_original_ack_cutoff_not_collection_deadline() {
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (socket, _silent_peer) = UnixStream::pair().unwrap();
        let mut worker = WorkerCleanup::new(
            run,
            local,
            owner,
            provider,
            1,
            vec![Route::new(owner, Channel::new(socket).unwrap()).unwrap()],
        )
        .unwrap();
        let start = Start {
            run,
            origin: local,
            ticket: Ticket::new(
                boot_nanos().unwrap() - umbra_core::managed::ACK_NANOS - 1_000_000,
                CleanupPath::Trusted,
                1,
            )
            .unwrap(),
        };
        assert!(start.ticket.deadline > boot_nanos().unwrap());
        worker.tickets.accept(start.clone(), local, local).unwrap();
        // Models a completely written Start whose peer never acknowledged it.
        worker.awaiting[0] = Some(start);
        assert_eq!(
            worker.poll_notifications().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(worker.tickets.failed());
    }

    #[test]
    fn idle_owner_start_ack_precedes_collect() {
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (worker_socket, owner_socket) = UnixStream::pair().unwrap();
        let mut worker = WorkerCleanup::new(
            run,
            local,
            owner,
            provider,
            1,
            vec![Route::new(owner, Channel::new(worker_socket).unwrap()).unwrap()],
        )
        .unwrap();
        assert!(worker.poll_notifications().unwrap().is_empty());
        assert!(
            worker.tickets.ticket().is_none(),
            "idle polling must not start a timer"
        );
        let start = Start {
            run,
            origin: owner,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Failed, 1).unwrap(),
        };
        let deadline = start.ticket.started + umbra_core::managed::ACK_NANOS;
        let mut channel = Channel::new(owner_socket).unwrap();
        channel
            .queue(&ControlMessage::Start(start.clone()))
            .unwrap();
        while !channel.flush(deadline).unwrap() {
            std::thread::yield_now();
        }
        loop {
            let notifications = worker.poll_notifications().unwrap();
            if !notifications.is_empty() {
                assert_eq!(notifications, vec![start.clone()]);
                break;
            }
            assert!(boot_nanos().unwrap() < deadline);
        }
        assert!(worker.pending_ack[0].is_some());
        std::thread::scope(|scope| {
            let peer = scope.spawn(|| {
                let receive = |channel: &mut Channel| loop {
                    if let Some(message) = channel.receive::<ControlMessage>(deadline).unwrap() {
                        break message;
                    }
                    std::thread::yield_now();
                };
                let ControlMessage::Ack(ack) = receive(&mut channel) else { panic!("ACK must precede Collect"); };
                assert_eq!(ack.committed, start);
                assert_eq!(ack.recipient, local);
                assert!(matches!(receive(&mut channel), ControlMessage::Collect { run: found } if found == run));
                channel.queue(&ControlMessage::Collected(ProviderReceipt {
                    run, owner, provider, status: 0, failed: false,
                })).unwrap();
                while !channel.flush(deadline).unwrap() { std::thread::yield_now(); }
            });
            assert!(
                worker.collect_provider().is_err(),
                "failure must survive a success receipt"
            );
            peer.join().unwrap();
        });
        assert!(worker.receipt.is_some());
        assert!(worker.pending_ack[0].is_none());
    }

    #[test]
    fn collection_commits_independent_start_before_ack_and_retains_failure() {
        // Live control sockets with a scripted owner receipt: the owner withholds
        // its receipt until the guardian has received the worker's ACK.
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let guardian = Participant {
            role: Role::Guardian,
            ..local
        };
        let provider = Participant {
            role: Role::Provider,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (worker_owner, owner_socket) = UnixStream::pair().unwrap();
        let (worker_guardian, guardian_socket) = UnixStream::pair().unwrap();
        // Keep the scripted peers enrolled until collection returns; this case
        // measures notification ordering rather than an additional peer loss.
        let _peer_lifetime = (
            owner_socket.try_clone().unwrap(),
            guardian_socket.try_clone().unwrap(),
        );
        let mut worker = WorkerCleanup::new(
            run,
            local,
            owner,
            provider,
            1,
            vec![
                Route::new(owner, Channel::new(worker_owner).unwrap()).unwrap(),
                Route::new(guardian, Channel::new(worker_guardian).unwrap()).unwrap(),
            ],
        )
        .unwrap();
        let original = Start {
            run,
            origin: local,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
        };
        worker
            .tickets
            .accept(original.clone(), local, local)
            .unwrap();
        std::thread::scope(|scope| {
            let peer = scope.spawn(|| {
                let deadline = boot_nanos().unwrap() + 2_000_000_000;
                let mut owner_channel = Channel::new(owner_socket).unwrap();
                let mut guardian_channel = Channel::new(guardian_socket).unwrap();
                loop {
                    if let Some(message) = owner_channel.receive::<ControlMessage>(deadline).unwrap() {
                        assert!(matches!(message, ControlMessage::Collect { run: found } if found == run));
                        break;
                    }
                    std::thread::yield_now();
                }
                let notification = Start {
                    run, origin: guardian,
                    ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Failed, 1).unwrap(),
                };
                guardian_channel.queue(&ControlMessage::Start(notification.clone())).unwrap();
                while !guardian_channel.flush(deadline).unwrap() { std::thread::yield_now(); }
                loop {
                    if let Some(message) = guardian_channel.receive::<ControlMessage>(deadline).unwrap() {
                        let ControlMessage::Ack(ack) = message else { panic!("expected ACK"); };
                        assert_eq!(ack.start, notification);
                        assert_eq!(ack.committed, original);
                        assert_eq!(ack.recipient, local);
                        break;
                    }
                    std::thread::yield_now();
                }
                loop {
                    if let Some(message) = owner_channel.receive::<ControlMessage>(deadline).unwrap() {
                        let ControlMessage::Start(forwarded) = message else { panic!("expected forwarded Start"); };
                        assert_eq!(forwarded, notification);
                        let mut tickets = TicketState::new(run, 1).unwrap();
                        tickets.accept(original.clone(), local, owner).unwrap();
                        let ack = tickets.accept(forwarded, guardian, owner).unwrap();
                        owner_channel.queue(&ControlMessage::Ack(ack)).unwrap();
                        while !owner_channel.flush(deadline).unwrap() { std::thread::yield_now(); }
                        break;
                    }
                    std::thread::yield_now();
                }
                owner_channel.queue(&ControlMessage::Collected(ProviderReceipt {
                    run, owner, provider, status: 0, failed: false,
                })).unwrap();
                while !owner_channel.flush(deadline).unwrap() { std::thread::yield_now(); }
            });
            assert!(worker.collect_provider().is_err());
            peer.join().unwrap();
        });
        assert!(
            worker.receipt.is_some(),
            "collection must consume the owner receipt"
        );
        assert!(
            worker.tickets.failed(),
            "later successful receipt cannot clear Start failure"
        );
        assert_eq!(worker.tickets.ticket(), Some(original.ticket));
    }

    #[test]
    fn worker_failure_gets_real_owner_wait_without_erasing_failure() {
        let (cancellation, input) = UnixStream::pair().unwrap();
        let child = Command::new("/bin/cat")
            .stdin(Stdio::from(OwnedFd::from(input)))
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let provider = identity(child.id(), Role::Provider).unwrap();
        let local = identity(std::process::id(), Role::Worker).unwrap();
        let owner_identity = Participant {
            role: Role::CleanupOwner,
            ..local
        };
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let (worker_channel, owner_channel) = UnixStream::pair().unwrap();
        let owner = match Owner::new(owner_identity, run, 1, child, cancellation, vec![]) {
            Ok(owner) => owner,
            Err((error, mut child, cancellation, _)) => {
                drop(cancellation);
                let _ = child.kill();
                let _ = child.wait();
                panic!("{error}");
            }
        };
        let mut service =
            match Service::new(owner, local, provider, Channel::new(owner_channel).unwrap()) {
                Ok(service) => service,
                Err(failure) => {
                    let (error, owner, _) = *failure;
                    let mut child = owner.into_child();
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{error}");
                }
            };
        std::thread::scope(|scope| {
            let server = scope.spawn(move || {
                let deadline = boot_nanos().unwrap() + 3_000_000_000;
                let outcome = loop {
                    match service.poll() {
                        Ok(true) => break Ok(()),
                        Ok(false) if boot_nanos().unwrap() < deadline => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Ok(false) => break Err(io::Error::from(io::ErrorKind::TimedOut)),
                        Err(error) => break Err(error),
                    }
                };
                let owner = service.into_owner();
                let failed = owner.failed();
                let mut child = owner.into_child();
                if outcome.is_err() {
                    let _ = child.kill();
                }
                let status = child.wait().unwrap();
                (outcome, failed, status)
            });
            let mut worker = WorkerCleanup::new(
                run,
                local,
                owner_identity,
                provider,
                1,
                vec![Route::new(owner_identity, Channel::new(worker_channel).unwrap()).unwrap()],
            )
            .unwrap();
            let started = worker.start();
            worker.failed(&error("injected worker transport loss"));
            let collected = worker.collect_provider();
            drop(worker);
            let (outcome, failed, status) = server.join().unwrap();
            started.unwrap();
            outcome.unwrap();
            assert!(failed);
            assert!(status.success());
            assert!(
                collected.is_err(),
                "successful wait must retain the triggering failure"
            );
        });
    }
}
