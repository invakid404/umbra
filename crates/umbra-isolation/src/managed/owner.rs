//! Cleanup-owner custody, independent of worker transport failures.
use super::{
    boot_nanos, invalid,
    publication::{Publication, Route},
};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::{Child, ExitStatus};
use umbra_core::managed::{Participant, RunIdentity, Start, TicketState};

/// Before-GO provider launch custody. A failed handshake leaves its actual
/// child here for the owner's startup rollback; dropping never sends a signal.
#[derive(Default)]
pub struct Launch {
    attempted: bool,
    pub(super) child: Option<Child>,
}

/// Run-bound metadata preceding the single provider descriptor.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderOffer {
    /// Exact startup run.
    pub run: RunIdentity,
    /// Actual provider parent.
    pub owner: Participant,
    /// Kernel provider identity.
    pub provider: Participant,
    /// Negotiated provider handshake.
    pub welcome: umbra_core::provider::Welcome,
}

impl ProviderOffer {
    /// Validate the direct-route offer before consuming its descriptor.
    pub fn validate(&self, run: RunIdentity) -> io::Result<()> {
        use umbra_core::managed::Role;
        if self.run != run
            || self.owner.role != Role::CleanupOwner
            || self.provider.role != Role::Provider
        {
            return Err(invalid("provider offer run or role mismatch"));
        }
        run.validate().map_err(invalid)?;
        let (provider, parent) =
            super::process::identity_with_parent(self.provider.pid, Role::Provider)?;
        if provider != self.provider
            || parent != self.owner.pid
            || super::process::identity(self.owner.pid, Role::CleanupOwner)? != self.owner
        {
            return Err(invalid("provider offer parent or birth mismatch"));
        }
        Ok(())
    }
}

/// RPC connection prepared for transfer, with no provider child authority.
pub struct Handoff {
    /// Provider identity validated against the cleanup owner's actual parent edge.
    pub provider: Participant,
    /// Negotiated handshake transferred with the connection.
    pub welcome: umbra_core::provider::Welcome,
    stream: UnixStream,
    sent: bool,
    offered_run: Option<RunIdentity>,
}

impl Launch {
    /// Launch once under the original readiness deadline. Any spawned child
    /// stays in this slot even if identity validation or stream preparation fails.
    pub fn connect(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        readiness_deadline: u64,
    ) -> io::Result<Handoff> {
        self.connect_configured(descriptor, readiness_deadline, |_| Ok(()))
    }

    /// Attach the provider's private startup endpoints before spawning it.
    /// The original readiness cutoff and this owner's child custody apply
    /// equally to configuration failures and handshake failures.
    pub fn connect_configured(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        readiness_deadline: u64,
        configure: impl FnOnce(&mut std::process::Command) -> io::Result<()>,
    ) -> io::Result<Handoff> {
        self.connect_configured_with_timeout(descriptor, readiness_deadline, 5_000, configure)
    }

    /// Keep configured RPC timeout distinct from the absolute startup cutoff.
    pub fn connect_configured_with_timeout(
        &mut self,
        descriptor: &umbra_core::provider::ProviderDescriptor,
        readiness_deadline: u64,
        timeout_ms: u64,
        configure: impl FnOnce(&mut std::process::Command) -> io::Result<()>,
    ) -> io::Result<Handoff> {
        if self.attempted {
            return Err(invalid("provider launch already attempted"));
        }
        self.attempted = true;
        let readiness_origin = std::time::Instant::now();
        let remaining = readiness_deadline.saturating_sub(boot_nanos()?);
        if remaining == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        super::executable::startup_observation(
            "provider-launch-begin",
            Some(umbra_core::managed::Role::Provider),
        );
        let client = umbra_core::provider::Client::connect_managed_configured_until(
            descriptor,
            timeout_ms,
            readiness_origin + std::time::Duration::from_nanos(remaining),
            &mut self.child,
            configure,
        )
        .map_err(io::Error::other)?;
        super::executable::startup_observation(
            "provider-launch-end",
            Some(umbra_core::managed::Role::Provider),
        );
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| invalid("missing provider child"))?;
        let (provider, parent) =
            super::process::identity_with_parent(child.id(), umbra_core::managed::Role::Provider)?;
        if parent != std::process::id() || boot_nanos()? >= readiness_deadline {
            return Err(invalid("provider parent mismatch or late handshake"));
        }
        let (stream, welcome) = client.into_managed_handoff().map_err(io::Error::other)?;
        umbra_core::cost_snapshot!("provider-handoff-ready", None, &[]);
        Ok(Handoff {
            provider,
            welcome,
            stream,
            sent: false,
            offered_run: None,
        })
    }

    /// Transfer actual wait authority into Owner or startup rollback.
    pub fn into_child(self) -> Option<Child> {
        self.child
    }
}

impl Handoff {
    /// Publish one run-bound offer followed by its descriptor on the same route.
    pub fn publish(
        &mut self,
        channel: &mut super::Channel,
        run: RunIdentity,
        deadline: u64,
    ) -> io::Result<bool> {
        if self.offered_run.is_some_and(|prior| prior != run) {
            return Err(invalid("provider handoff run changed"));
        }
        if self.offered_run.is_none() {
            if self.sent {
                return Err(invalid("provider descriptor already sent"));
            }
            let offer = ProviderOffer {
                run,
                owner: super::process::identity(
                    std::process::id(),
                    umbra_core::managed::Role::CleanupOwner,
                )?,
                provider: self.provider,
                welcome: self.welcome.clone(),
            };
            offer.validate(run)?;
            #[cfg(feature = "probe-managed-offer-generation")]
            let offer = ProviderOffer {
                run: RunIdentity {
                    generation: run
                        .generation
                        .checked_add(1)
                        .ok_or_else(|| invalid("probe generation overflow"))?,
                    ..run
                },
                ..offer
            };
            channel.queue(&offer)?;
            self.offered_run = Some(run);
        }
        if !channel.flush(deadline)? {
            return Ok(false);
        }
        self.send(channel.handoff_stream()?, deadline)
    }

    /// Transfer only the RPC descriptor on the dedicated startup handoff phase.
    /// Retrying after WouldBlock preserves the original deadline and never sends
    /// a second descriptor after success.
    pub fn send(&mut self, route: &UnixStream, readiness_deadline: u64) -> io::Result<bool> {
        if self.sent {
            return Ok(true);
        }
        if boot_nanos()? >= readiness_deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.sent = umbra_core::managed_handoff::send(route, &self.stream)?;
        Ok(self.sent)
    }

    /// Retain the original RPC endpoint for cancellation until the independent
    /// route is installed. This is valid on both success and rollback.
    pub fn into_cancellation(self) -> UnixStream {
        self.stream
    }
}

/// Provider custody remains here after the worker receives its connection.
/// Drop never signals the provider. The role loop must collect or report the
/// original-ticket residual before terminating its own process.
pub struct Owner {
    run: RunIdentity,
    capacity: u64,
    participant: Participant,
    provider: Child,
    cancellation: Option<UnixStream>,
    tickets: TicketState,
    publication: Option<Publication>,
    routes: Vec<Route>,
    pending_ack: Vec<Option<u64>>,
    failed_routes: Vec<bool>,
    forwarding: Vec<std::collections::VecDeque<Start>>,
    awaiting: Vec<Option<Start>>,
    forwarded: Vec<Start>,
    inventory: Option<Vec<Participant>>,
    transport_closed: bool,
    cancellation_requested: bool,
    collected: Option<ExitStatus>,
    failures: Vec<String>,
    watch: Option<super::process::ProcessWatch>,
    admission: Option<super::admission::AdmissionService>,
    admission_closed: bool,
    independent_cancellation: Option<super::Channel>,
    cancellation_queued: bool,
}

impl Owner {
    /// Install the dedicated provider route before any Start.
    pub fn install_cancellation(&mut self, channel: super::Channel) -> io::Result<()> {
        if self.independent_cancellation.is_some() || self.tickets.ticket().is_some() {
            return Err(invalid("late or duplicate cancellation installation"));
        }
        self.independent_cancellation = Some(channel);
        self.cancellation.take();
        Ok(())
    }
    /// Adopt the independently assigned admission route before cleanup starts.
    pub fn install_admission(&mut self, channel: super::Channel) -> io::Result<()> {
        if self.admission.is_some() || self.tickets.ticket().is_some() {
            return Err(invalid("late or duplicate admission installation"));
        }
        self.admission = Some(super::admission::AdmissionService::new(
            self.run,
            self.capacity,
            channel,
        )?);
        Ok(())
    }

    /// Conservative count includes entries whose responses or proofs were lost.
    pub fn outstanding_admissions(&self) -> Option<u64> {
        self.admission.as_ref().map(|service| service.outstanding())
    }

    fn freeze_admission(&mut self) {
        if let Some(service) = &mut self.admission {
            service.freeze();
        }
    }

    /// Adopt the already installed startup queue, preserving the descriptor
    /// budget and observations even when a peer's socket remains duplicated.
    pub fn install_watch(
        &mut self,
        watch: super::process::ProcessWatch,
        inventory: &super::inventory::Inventory,
    ) -> io::Result<()> {
        let result = (|| {
            if self.watch.is_some() || self.tickets.ticket().is_some() {
                return Err(invalid("late or duplicate owner watch installation"));
            }
            self.install_inventory(inventory, inventory.external_caller)?;
            if !watch.covers(&inventory.watches(self.participant)?) {
                return Err(invalid("owner queue lacks required startup watches"));
            }
            self.watch = Some(watch);
            Ok(())
        })();
        if result.is_err() {
            self.tickets.fail();
        }
        result
    }

    fn observe(&mut self) -> io::Result<()> {
        if self.collected.is_some() {
            return Ok(());
        }
        let Some(watch) = &self.watch else {
            return Ok(());
        };
        let reason = match watch.exited() {
            Ok(None) => return Ok(()),
            Ok(Some(peer)) if peer.pid == self.provider.id() && self.tickets.ticket().is_some() => {
                // Only try_wait below may convert provider exit to collection.
                return Ok(());
            }
            Ok(Some(peer)) => format!("owner observed {:?} exit before collection", peer.role),
            Err(error) => {
                self.watch = None;
                format!("owner process watch failed: {error}")
            }
        };
        if self.tickets.ticket().is_none() {
            let start = Start {
                run: self.run,
                origin: self.participant,
                ticket: umbra_core::managed::Ticket::new(
                    boot_nanos()?,
                    umbra_core::managed::CleanupPath::Failed,
                    self.capacity,
                )
                .map_err(invalid)?,
            };
            self.start(start, self.participant)?;
        }
        self.cancel(reason)
    }

    fn forward(&mut self, start: Start, source: Option<usize>) -> io::Result<()> {
        if self.forwarded.contains(&start) {
            return Ok(());
        }
        if self.forwarded.len() >= 16 {
            self.tickets.fail();
            return Err(invalid("owner forwarding notification bound exceeded"));
        }
        self.forwarded.push(start.clone());
        for (index, queue) in self.forwarding.iter_mut().enumerate() {
            if Some(index) != source && !self.failed_routes[index] {
                queue.push_back(start.clone());
            }
        }
        Ok(())
    }

    /// Receive notifications outside an active publication. ACKs use the
    /// original notification cutoff; a route failure cannot clear collection.
    fn poll_notifications(&mut self) -> io::Result<()> {
        use umbra_core::managed::{CleanupPath, ControlMessage, Ticket, ACK_NANOS};
        let inventory = self.inventory.clone().unwrap_or_else(|| {
            self.routes
                .iter()
                .map(Route::peer)
                .chain([self.participant])
                .collect()
        });
        for index in 0..self.routes.len() {
            if self.failed_routes[index] {
                continue;
            }
            let result = (|| {
                let mut deadline = self.tickets.ticket().map_or(u64::MAX, |t| t.deadline);
                if let Some(start) = self.awaiting[index].as_ref() {
                    deadline = deadline.min(
                        start
                            .ticket
                            .started
                            .checked_add(ACK_NANOS)
                            .ok_or_else(|| invalid("forwarding ACK clock overflow"))?,
                    );
                }
                let peer = self.routes[index].peer();
                let channel = self.routes[index].channel();
                if let Some(cutoff) = self.pending_ack[index] {
                    if !channel.flush(cutoff.min(deadline))? {
                        return Ok(());
                    }
                    self.pending_ack[index] = None;
                }
                if self.awaiting[index].is_none() {
                    if let Some(start) = self.forwarding[index].pop_front() {
                        deadline = deadline.min(
                            start
                                .ticket
                                .started
                                .checked_add(ACK_NANOS)
                                .ok_or_else(|| invalid("forwarding ACK clock overflow"))?,
                        );
                        channel.queue(&ControlMessage::Start(start.clone()))?;
                        self.awaiting[index] = Some(start);
                    }
                }
                if !channel.flush(deadline)? {
                    return Ok(());
                }
                let Some(message) = channel.receive::<ControlMessage>(deadline)? else {
                    return Ok(());
                };
                if let ControlMessage::Ack(ack) = message {
                    let start = self.awaiting[index]
                        .as_ref()
                        .ok_or_else(|| invalid("unsolicited forwarding ACK"))?;
                    ack.validate(start, peer).map_err(invalid)?;
                    if !inventory.contains(&ack.committed.origin) {
                        return Err(invalid("forwarding ACK has unknown origin"));
                    }
                    self.tickets
                        .accept(
                            ack.committed.clone(),
                            ack.committed.origin,
                            self.participant,
                        )
                        .map_err(invalid)?;
                    self.awaiting[index] = None;
                    return Ok(());
                }
                let ControlMessage::Start(start) = message else {
                    return Err(invalid("unexpected owner notification"));
                };
                if !inventory.contains(&start.origin) {
                    return Err(invalid("unknown owner notification origin"));
                }
                let ack = self
                    .tickets
                    .accept(start.clone(), start.origin, self.participant)
                    .map_err(invalid)?;
                if let Some(service) = &mut self.admission {
                    service.freeze();
                }
                let cutoff = start
                    .ticket
                    .started
                    .checked_add(ACK_NANOS)
                    .ok_or_else(|| invalid("owner ACK clock overflow"))?
                    .min(self.tickets.ticket().unwrap().deadline);
                channel.queue(&ControlMessage::Ack(ack))?;
                self.pending_ack[index] = Some(cutoff);
                if start.ticket.path == CleanupPath::Failed {
                    self.cancellation_requested = true;
                }
                self.forward(start, Some(index))?;
                Ok(())
            })();
            if let Err(error) = result {
                self.failed_routes[index] = true;
                self.pending_ack[index] = None;
                self.awaiting[index] = None;
                self.forwarding[index].clear();
                self.tickets.fail();
                self.failures.push(format!("owner notification: {error}"));
                if self.tickets.ticket().is_none() {
                    let start = Start {
                        run: self.run,
                        origin: self.participant,
                        ticket: Ticket::new(boot_nanos()?, CleanupPath::Failed, self.capacity)
                            .map_err(invalid)?,
                    };
                    self.tickets
                        .accept(start.clone(), self.participant, self.participant)
                        .map_err(invalid)?;
                    self.forward(start, Some(index))?;
                }
                self.cancellation_requested = true;
            }
        }
        Ok(())
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
            if !participants.contains(&self.participant)
                || !participants.iter().any(|p| {
                    p.pid == self.provider.id() && p.role == umbra_core::managed::Role::Provider
                })
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

    /// Adopt resources already enrolled during startup. On failure all owned
    /// handles return to the caller for the bounded before-GO rollback.
    pub fn new(
        participant: Participant,
        run: RunIdentity,
        capacity: u64,
        provider: Child,
        cancellation: UnixStream,
        routes: Vec<Route>,
    ) -> Result<Self, (io::Error, Child, UnixStream, Vec<Route>)> {
        let tickets = participant
            .validate()
            .and_then(|_| TicketState::new(run, capacity));
        match tickets {
            Ok(tickets) => Ok(Self {
                run,
                capacity,
                participant,
                provider,
                cancellation: Some(cancellation),
                tickets,
                publication: None,
                pending_ack: vec![None; routes.len()],
                failed_routes: vec![false; routes.len()],
                forwarding: vec![std::collections::VecDeque::new(); routes.len()],
                awaiting: vec![None; routes.len()],
                forwarded: Vec::new(),
                routes,
                inventory: None,
                transport_closed: false,
                cancellation_requested: false,
                collected: None,
                failures: Vec::new(),
                watch: None,
                admission: None,
                admission_closed: false,
                independent_cancellation: None,
                cancellation_queued: false,
            }),
            Err(error) => Err((invalid(error), provider, cancellation, routes)),
        }
    }

    /// Commit and publish the original Start before initiating transport closure.
    pub fn start(&mut self, start: Start, expected_origin: Participant) -> io::Result<()> {
        if self.publication.is_some() || self.tickets.ticket().is_some() {
            // Repeated notifications can only shorten the committed ticket.
            self.tickets
                .accept(start.clone(), expected_origin, self.participant)
                .map_err(invalid)?;
            self.freeze_admission();
            return self.forward(start, None);
        }
        self.forwarded.push(start.clone());
        let mut publication = Publication::begin(
            start,
            expected_origin,
            self.participant,
            &mut self.tickets,
            std::mem::take(&mut self.routes),
        )?;
        if let Some(inventory) = &self.inventory {
            publication.inventory(inventory);
        }
        self.publication = Some(publication);
        self.freeze_admission();
        Ok(())
    }

    /// Request failure cleanup after the original Start has been committed.
    /// A trusted Start alone leaves the cooperative provider protocol usable.
    pub fn cancel(&mut self, reason: String) -> io::Result<()> {
        if reason.is_empty() || self.tickets.ticket().is_none() {
            return Err(invalid(
                "cancellation requires Start and an attributable reason",
            ));
        }
        self.tickets.fail();
        self.freeze_admission();
        self.failures.push(reason);
        self.cancellation_requested = true;
        Ok(())
    }

    /// Progress publication and actual collection without a blocking wait.
    /// Neither a worker error nor a publication failure kills the provider.
    pub fn progress(&mut self) -> io::Result<Option<ExitStatus>> {
        self.observe()?;
        if let Some(publication) = self.publication.as_mut() {
            if !publication.progress(&mut self.tickets)? {
                return Ok(None);
            }
            self.failures.extend_from_slice(publication.failures());
            let received = publication.received().to_vec();
            self.routes = self.publication.take().unwrap().into_routes();
            for (peer, start) in received {
                if start.ticket.path == umbra_core::managed::CleanupPath::Failed {
                    self.cancellation_requested = true;
                }
                let source = self.routes.iter().position(|route| route.peer() == peer);
                self.forward(start, source)?;
            }
        }
        self.poll_notifications()?;
        if self.tickets.ticket().is_some() {
            self.freeze_admission();
        }
        let deadline = self.tickets.ticket().map_or(u64::MAX, |t| t.deadline);
        let admission_error = self
            .admission
            .as_mut()
            .filter(|_| !self.admission_closed)
            .and_then(|service| service.poll(deadline).err());
        if let Some(error) = admission_error {
            self.admission_closed = true;
            let orderly_eof = error.kind() == io::ErrorKind::UnexpectedEof
                && self.tickets.ticket().is_some()
                && self.outstanding_admissions() == Some(0);
            // The failed endpoint retains its charges. Initiate cleanup once;
            // later polls still have to collect the actual provider child.
            if !orderly_eof && !self.cancellation_requested {
                if self.tickets.ticket().is_none() {
                    let start = Start {
                        run: self.run,
                        origin: self.participant,
                        ticket: umbra_core::managed::Ticket::new(
                            boot_nanos()?,
                            umbra_core::managed::CleanupPath::Failed,
                            self.capacity,
                        )
                        .map_err(invalid)?,
                    };
                    self.start(start, self.participant)?;
                }
                self.cancel(format!("owner admission channel: {error}"))?;
                if self.publication.is_some() {
                    return Ok(None);
                }
            }
        }
        if self.cancellation_requested && !self.transport_closed {
            if let Some(channel) = &mut self.independent_cancellation {
                let ticket = self
                    .tickets
                    .ticket()
                    .ok_or_else(|| invalid("cancellation without ticket"))?;
                let result = (|| {
                    if !self.cancellation_queued {
                        channel.queue(&super::cancellation::Notification {
                            run: self.run,
                            ticket,
                        })?;
                        self.cancellation_queued = true;
                    }
                    let cutoff = ticket.cutoffs(self.capacity).map_err(invalid)?.fallback;
                    channel.flush(cutoff)
                })();
                match result {
                    Ok(false) => return Ok(None),
                    Ok(true) => {}
                    Err(error) => {
                        self.tickets.fail();
                        self.failures
                            .push(format!("independent cancellation: {error}"));
                    }
                }
                self.transport_closed = true;
            } else {
                // shutdown reaches the worker's duplicate too, but leaves the provider
                // process alive to complete EOF-triggered resource cleanup.
                if let Err(error) = self
                    .cancellation
                    .as_ref()
                    .ok_or_else(|| invalid("missing cancellation route"))?
                    .shutdown(Shutdown::Both)
                {
                    self.tickets.fail();
                    self.failures
                        .push(format!("provider transport closure: {error}"));
                }
                self.transport_closed = true;
            }
        }
        if self.collected.is_none() {
            match self.provider.try_wait() {
                Ok(Some(status)) => {
                    if self
                        .outstanding_admissions()
                        .is_some_and(|count| count != 0)
                    {
                        self.tickets.fail();
                        self.failures
                            .push("provider exited with admission obligations".into());
                    }
                    if !status.success() {
                        self.tickets.fail();
                        self.failures.push(format!("provider exited: {status}"));
                    }
                    self.collected = Some(status);
                }
                Ok(None) => {}
                Err(error) => {
                    self.tickets.fail();
                    self.failures.push(format!("provider parent wait: {error}"));
                    return Err(error);
                }
            }
        }
        // Complete committed notifications before publishing parent collection.
        // A later poll flushes them under the retained original cutoff.
        if self.pending_ack.iter().any(Option::is_some)
            || self.awaiting.iter().any(Option::is_some)
            || self.forwarding.iter().any(|queue| !queue.is_empty())
        {
            return Ok(None);
        }
        Ok(self.collected)
    }

    /// Check the committed ticket without starting an inactivity timer.
    pub fn expired(&self) -> io::Result<bool> {
        let now = boot_nanos()?;
        Ok(self
            .tickets
            .ticket()
            .is_some_and(|ticket| ticket.remaining(now) == 0))
    }

    /// Outcome remains failed even when provider collection later succeeds.
    pub fn failed(&self) -> bool {
        self.tickets.failed()
    }

    /// Inspect the owner's committed minimum during shared-clock qualification.
    #[cfg(feature = "test-clock")]
    pub fn test_clock_ticket(&self) -> Option<umbra_core::managed::Ticket> {
        self.tickets.ticket()
    }

    /// Routes still awaiting publication completion at the shared-clock barrier.
    #[cfg(feature = "test-clock")]
    pub fn test_pending_publication(&self) -> Vec<Participant> {
        self.publication
            .as_ref()
            .map_or_else(Vec::new, Publication::test_pending_peers)
    }

    /// Preserve attribution for custody and the outer run result.
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// Return the still-owned child at a bounded residual boundary. The caller
    /// may continue the remaining original allocation, never start another one.
    pub fn into_child(self) -> Child {
        self.provider
    }
}

/// Nonblocking worker-facing owner service. Other notification routes remain in
/// Owner's publication set; this channel is the worker's independent control.
pub struct Service {
    owner: Owner,
    worker: Participant,
    provider: Participant,
    channel: super::Channel,
    response_deadline: Option<u64>,
    collect_requested: bool,
    receipt_sent: bool,
    disconnected: bool,
    forced: bool,
}

impl Service {
    /// Enter notified failure cleanup without releasing provider child custody.
    pub fn fail(&mut self, reason: String) -> io::Result<()> {
        self.disconnect(reason)
    }

    /// Collection after a failed exchange never changes the failed outcome.
    pub fn failed(&self) -> bool {
        self.owner.failed()
    }

    /// Bind the identities established before GO to an exclusively owned channel.
    pub fn new(
        owner: Owner,
        worker: Participant,
        provider: Participant,
        channel: super::Channel,
    ) -> Result<Self, Box<(io::Error, Owner, super::Channel)>> {
        if worker.validate().is_err()
            || provider.validate().is_err()
            || owner.provider.id() != provider.pid
        {
            return Err(Box::new((
                invalid("owner service identity mismatch"),
                owner,
                channel,
            )));
        }
        Ok(Self {
            owner,
            worker,
            provider,
            channel,
            response_deadline: None,
            collect_requested: false,
            receipt_sent: false,
            disconnected: false,
            forced: false,
        })
    }

    fn disconnect(&mut self, reason: String) -> io::Result<()> {
        self.disconnected = true;
        self.response_deadline = None;
        if self.owner.tickets.ticket().is_none() {
            let start = Start {
                run: self.owner.run,
                origin: self.owner.participant,
                ticket: umbra_core::managed::Ticket::new(
                    boot_nanos()?,
                    umbra_core::managed::CleanupPath::Failed,
                    self.owner.capacity,
                )
                .map_err(invalid)?,
            };
            self.owner.start(start, self.owner.participant)?;
        }
        self.owner.cancel(reason)
    }

    /// Wait for control, admission, notification or process readiness under
    /// the earliest retained cutoff; no Start means no run-age timer.
    pub fn wait_for_activity(&mut self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut descriptors = Vec::new();
        fn add(descriptors: &mut Vec<libc::pollfd>, interest: (std::os::fd::BorrowedFd<'_>, bool)) {
            descriptors.push(libc::pollfd {
                fd: interest.0.as_raw_fd(),
                events: if interest.1 {
                    libc::POLLOUT
                } else {
                    libc::POLLIN
                },
                revents: 0,
            });
        }
        let mut deadline = self.owner.tickets.ticket().map_or(u64::MAX, |t| t.deadline);
        if let Some(ticket) = self.owner.tickets.ticket() {
            let cutoffs = ticket.cutoffs(self.owner.capacity).map_err(invalid)?;
            if !self.owner.cancellation_requested {
                deadline = deadline.min(cutoffs.cooperative);
            }
            if !self.forced && self.owner.collected.is_none() {
                deadline = deadline.min(cutoffs.fallback);
            }
        }
        if let Some(cutoff) = self.response_deadline {
            deadline = deadline.min(cutoff);
        }
        if !self.disconnected && (!self.receipt_sent || self.response_deadline.is_some()) {
            add(&mut descriptors, self.channel.wait_interest());
        }
        if let Some(watch) = &self.owner.watch {
            descriptors.push(libc::pollfd {
                fd: watch.descriptor(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if !self.owner.admission_closed {
            if let Some(admission) = &self.owner.admission {
                add(&mut descriptors, admission.wait_interest());
            }
        }
        if self.owner.cancellation_requested && !self.owner.transport_closed {
            if let Some(channel) = &self.owner.independent_cancellation {
                if !self.owner.cancellation_queued {
                    return Ok(());
                }
                add(&mut descriptors, channel.wait_interest());
            }
        }
        if let Some(publication) = &self.owner.publication {
            let (routes, cutoff) = publication.wait_descriptors();
            descriptors.extend(routes);
            deadline = deadline.min(cutoff);
        }
        for (index, route) in self.owner.routes.iter_mut().enumerate() {
            if self.owner.failed_routes[index] {
                continue;
            }
            if !self.owner.forwarding[index].is_empty()
                && self.owner.awaiting[index].is_none()
                && self.owner.pending_ack[index].is_none()
            {
                return Ok(());
            }
            if let Some(cutoff) = self.owner.pending_ack[index] {
                deadline = deadline.min(cutoff);
            }
            if let Some(start) = &self.owner.awaiting[index] {
                deadline = deadline.min(
                    start
                        .ticket
                        .started
                        .checked_add(umbra_core::managed::ACK_NANOS)
                        .ok_or_else(|| invalid("owner ACK cutoff overflow"))?,
                );
            }
            add(&mut descriptors, route.channel().wait_interest());
        }
        super::readiness::wait_descriptors(&mut descriptors, deadline)
    }

    /// One event-loop step; true requires parent collection and response delivery
    /// (or a disconnected worker). Expiry never turns into a collection receipt.
    pub fn poll(&mut self) -> io::Result<bool> {
        use std::os::unix::process::ExitStatusExt;
        use umbra_core::managed::{ControlMessage, ProviderReceipt, ACK_NANOS};
        if let Some(deadline) = self.response_deadline {
            match self.channel.flush(deadline) {
                Ok(true) => self.response_deadline = None,
                Ok(false) => {}
                Err(error) => self.disconnect(format!("owner response: {error}"))?,
            }
        }
        if !self.disconnected && self.response_deadline.is_none() && !self.receipt_sent {
            let deadline = self
                .owner
                .tickets
                .ticket()
                .map_or(u64::MAX, |ticket| ticket.deadline);
            match self.channel.receive::<ControlMessage>(deadline) {
                Ok(Some(ControlMessage::Start(start))) => {
                    let origin = start.origin;
                    if origin != self.worker
                        && !self
                            .owner
                            .inventory
                            .as_ref()
                            .is_some_and(|members| members.contains(&origin))
                    {
                        self.disconnect("worker forwarded an unenrolled origin".into())?;
                        return Ok(false);
                    }
                    let original = start.clone();
                    let result = self.owner.start(start, origin).and_then(|_| {
                        let ack = self
                            .owner
                            .tickets
                            .accept(original.clone(), origin, self.owner.participant)
                            .map_err(invalid)?;
                        self.channel.queue(&ControlMessage::Ack(ack))?;
                        self.response_deadline = Some(
                            original
                                .ticket
                                .started
                                .checked_add(ACK_NANOS)
                                .ok_or_else(|| invalid("ACK clock overflow"))?
                                .min(original.ticket.deadline),
                        );
                        Ok(())
                    });
                    if let Err(error) = result {
                        self.disconnect(format!("invalid worker Start: {error}"))?;
                    }
                }
                Ok(Some(ControlMessage::Cancel { run, reason })) if run == self.owner.run => {
                    if let Err(error) = self.owner.cancel(reason) {
                        self.disconnect(error.to_string())?;
                    }
                }
                Ok(Some(ControlMessage::Collect { run }))
                    if run == self.owner.run && self.owner.tickets.ticket().is_some() =>
                {
                    self.collect_requested = true
                }
                Ok(Some(_)) => self.disconnect("unexpected owner control message".into())?,
                Ok(None) => {}
                Err(error) => self.disconnect(format!("worker control: {error}"))?,
            }
        }
        let collected = self.owner.progress()?;
        if self.owner.collected.is_none() {
            if let Some(ticket) = self.owner.tickets.ticket() {
                let cutoffs = ticket.cutoffs(self.owner.capacity).map_err(invalid)?;
                let now = boot_nanos()?;
                if now >= cutoffs.cooperative && !self.owner.cancellation_requested {
                    self.owner
                        .cancel("cooperative provider opportunity expired".into())?;
                }
                if now >= cutoffs.fallback && !self.forced {
                    self.forced = true;
                    self.owner.tickets.fail();
                    self.owner
                        .failures
                        .push("provider fallback opportunity expired".into());
                    // progress above tried the real parent wait first. ECHILD
                    // returns an error before reaching this signal boundary.
                    if let Err(error) = super::process::signal_current(self.provider, libc::SIGKILL)
                    {
                        self.owner
                            .failures
                            .push(format!("provider fallback signal: {error}"));
                    }
                }
            }
        }
        if let Some(status) = collected {
            #[cfg(feature = "probe-cancellation-ticket")]
            if let Some(directory) = std::env::var_os("UMBRA_NATIVE_SETUP_PROBE") {
                let directory = std::path::PathBuf::from(directory);
                let receipt = ProviderReceipt {
                    run: self.owner.run,
                    owner: self.owner.participant,
                    provider: self.provider,
                    status: status.into_raw(),
                    failed: self.owner.failed(),
                };
                std::fs::write(
                    directory.join("collected.tmp"),
                    serde_json::to_vec(&receipt).map_err(io::Error::other)?,
                )?;
                std::fs::rename(
                    directory.join("collected.tmp"),
                    directory.join("collected.json"),
                )?;
            }
            if self.disconnected {
                return Ok(true);
            }
            if self.collect_requested && !self.receipt_sent && self.response_deadline.is_none() {
                self.channel
                    .queue(&ControlMessage::Collected(ProviderReceipt {
                        run: self.owner.run,
                        owner: self.owner.participant,
                        provider: self.provider,
                        status: status.into_raw(),
                        failed: self.owner.failed(),
                    }))?;
                self.response_deadline = Some(self.owner.tickets.ticket().unwrap().deadline);
                self.receipt_sent = true;
            }
            return Ok(self.receipt_sent && self.response_deadline.is_none());
        }
        if self.owner.expired()? {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(false)
    }

    /// Retain custody on an event-loop error for the original-ticket fallback.
    pub fn into_owner(self) -> Owner {
        self.owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::process::{Command, Stdio};
    use umbra_core::managed::{CleanupPath, Role, Ticket};

    #[test]
    fn dedicated_cancellation_preserves_rpc_and_collects_actual_child() {
        use std::io::Read;
        let run = RunIdentity {
            nonce: umbra_core::shutdown::Nonce::from_u128(1),
            generation: 1,
        };
        let participant =
            super::super::process::identity(std::process::id(), Role::CleanupOwner).unwrap();
        let (control, mut rpc) = UnixStream::pair().unwrap();
        // Model the worker retaining its transferred endpoint.
        let _worker_rpc = control.try_clone().unwrap();
        let owner_rpc_fd = std::os::fd::AsRawFd::as_raw_fd(&control);
        rpc.set_nonblocking(true).unwrap();
        let child = Command::new("/usr/bin/true").spawn().unwrap();
        let mut owner = match Owner::new(participant, run, 1, child, control, Vec::new()) {
            Ok(owner) => owner,
            Err((error, mut child, _, _)) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{error}");
            }
        };
        let result = (|| -> io::Result<()> {
            let (sender, receiver) = UnixStream::pair()?;
            owner.install_cancellation(super::super::Channel::new(sender)?)?;
            // Record before later allocations can reuse the descriptor number.
            let owner_rpc_closed = unsafe { libc::fcntl(owner_rpc_fd, libc::F_GETFD) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::EBADF);
            let ticket = Ticket::new(boot_nanos()?, CleanupPath::Failed, 1).map_err(invalid)?;
            owner.start(
                Start {
                    run,
                    origin: participant,
                    ticket,
                },
                participant,
            )?;
            owner.cancel("fixture requested cleanup".into())?;
            let deadline = boot_nanos()? + 1_000_000_000;
            loop {
                if let Some(status) = owner.progress()? {
                    assert!(status.success());
                    break;
                }
                if boot_nanos()? >= deadline {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                std::thread::yield_now();
            }
            let mut receiver = super::super::Channel::new(receiver)?;
            let message = loop {
                if let Some(message) =
                    receiver.receive::<super::super::cancellation::Notification>(deadline)?
                {
                    break message;
                }
                std::thread::yield_now();
            };
            assert!(
                owner_rpc_closed,
                "dedicated cancellation must release the owner RPC copy"
            );
            assert_eq!(message.run, run);
            assert_eq!(message.ticket, ticket);
            assert_eq!(
                rpc.read(&mut [0u8; 1]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock,
                "dedicated cancellation must leave main RPC open"
            );
            assert!(
                owner.failed(),
                "actual child success cannot erase cancellation failure"
            );
            Ok(())
        })();
        let mut child = owner.into_child();
        if result.is_err() {
            let _ = child.kill();
        }
        assert!(child.wait().unwrap().success());
        result.unwrap();
    }

    #[test]
    fn owner_closes_transport_then_collects_provider_after_cleanup() {
        owner_collection(false);
    }

    #[test]
    fn idle_owner_acknowledges_start_then_collects_provider_after_cleanup() {
        owner_collection(true);
    }

    #[test]
    fn admission_eof_after_zero_obligations_still_requires_provider_wait() {
        let run = RunIdentity {
            nonce: umbra_core::shutdown::Nonce::from_u128(1),
            generation: 1,
        };
        let participant =
            super::super::process::identity(std::process::id(), Role::CleanupOwner).unwrap();
        let (control, input) = UnixStream::pair().unwrap();
        let child = Command::new("/usr/bin/true")
            .stdin(Stdio::from(OwnedFd::from(input)))
            .spawn()
            .unwrap();
        let mut owner = match Owner::new(participant, run, 1, child, control, Vec::new()) {
            Ok(owner) => owner,
            Err((error, mut child, _, _)) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{error}");
            }
        };
        let result = (|| -> io::Result<ExitStatus> {
            let (provider, owner_channel) = UnixStream::pair()?;
            owner.install_admission(super::super::Channel::new(owner_channel)?)?;
            owner.start(
                Start {
                    run,
                    origin: participant,
                    ticket: Ticket::new(boot_nanos()?, CleanupPath::Trusted, 1).map_err(invalid)?,
                },
                participant,
            )?;
            drop(provider);
            let deadline = boot_nanos()? + 1_000_000_000;
            loop {
                if let Some(status) = owner.progress()? {
                    return Ok(status);
                }
                if boot_nanos()? >= deadline {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                std::thread::yield_now();
            }
        })();
        let failed = owner.failed();
        let mut child = owner.into_child();
        if result.is_err() {
            let _ = child.kill();
        }
        let collected = child.wait().unwrap();
        assert!(result.unwrap().success());
        assert!(collected.success());
        assert!(!failed, "orderly empty admission EOF must preserve success");
    }

    fn owner_collection(independent_start: bool) {
        use umbra_core::managed::ControlMessage;
        let (control, input) = UnixStream::pair().unwrap();
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "while read line; do :; done; /usr/bin/true & resource=$!; wait \"$resource\"",
            ])
            .stdin(Stdio::from(OwnedFd::from(input)))
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let participant =
            super::super::process::identity(std::process::id(), Role::CleanupOwner).unwrap();
        let guardian = Participant {
            role: Role::Guardian,
            ..participant
        };
        let (incoming, outgoing) = UnixStream::pair().unwrap();
        let mut peer = super::super::Channel::new(outgoing).unwrap();
        let relay = Participant {
            role: Role::Relay,
            ..participant
        };
        let (relay_incoming, relay_outgoing) = UnixStream::pair().unwrap();
        let mut relay_peer = super::super::Channel::new(relay_outgoing).unwrap();
        let routes = if independent_start {
            vec![
                Route::new(guardian, super::super::Channel::new(incoming).unwrap()).unwrap(),
                Route::new(relay, super::super::Channel::new(relay_incoming).unwrap()).unwrap(),
            ]
        } else {
            Vec::new()
        };
        let mut owner = match Owner::new(participant, run, 1, child, control, routes) {
            Ok(owner) => owner,
            Err((error, mut child, control, _)) => {
                drop(control);
                let _ = child.kill();
                let _ = child.wait();
                panic!("{error}");
            }
        };
        let result = (|| -> io::Result<ExitStatus> {
            use super::super::{admission::AdmissionClient, Channel};
            use umbra_core::{AdmissionError, AdmissionOperation};
            let (provider_admission, owner_admission) = UnixStream::pair()?;
            owner.install_admission(Channel::new(owner_admission)?)?;
            let mut admission = AdmissionClient::new(run, Channel::new(provider_admission)?)?;
            let obligation = umbra_core::shutdown::Nonce::from_u128(2);
            admission.begin(AdmissionOperation::Reserve { obligation })?;
            let admission_deadline = boot_nanos()? + 1_000_000_000;
            loop {
                owner.progress()?;
                if let Some(result) = admission.poll(admission_deadline)? {
                    if result != Ok(1) {
                        return Err(invalid("initial admission was not granted"));
                    }
                    break;
                }
            }
            if owner.progress()?.is_some() {
                return Err(invalid("provider exited before EOF"));
            }
            if owner.tickets.ticket().is_some() {
                return Err(invalid("idle owner invented a cleanup ticket"));
            }
            let start = Start {
                run,
                origin: if independent_start {
                    guardian
                } else {
                    participant
                },
                ticket: Ticket::new(boot_nanos()?, CleanupPath::Trusted, 1).map_err(invalid)?,
            };
            if independent_start {
                let deadline = start.ticket.started + umbra_core::managed::ACK_NANOS;
                peer.queue(&ControlMessage::Start(start.clone()))?;
                while !peer.flush(deadline)? {
                    std::thread::yield_now();
                }
                loop {
                    if owner.progress()?.is_some() || owner.transport_closed {
                        return Err(invalid("forwarded trusted Start closed provider transport"));
                    }
                    if let Some(message) = relay_peer.receive::<ControlMessage>(deadline)? {
                        let ControlMessage::Start(forwarded) = message else {
                            return Err(invalid("expected forwarded Start"));
                        };
                        if forwarded != start {
                            return Err(invalid("forwarding changed original Start"));
                        }
                        let mut tickets = TicketState::new(run, 1).map_err(invalid)?;
                        let ack = tickets
                            .accept(forwarded, guardian, relay)
                            .map_err(invalid)?;
                        relay_peer.queue(&ControlMessage::Ack(ack))?;
                        while !relay_peer.flush(deadline)? {
                            std::thread::yield_now();
                        }
                        break;
                    }
                    std::thread::yield_now();
                }
                while owner.awaiting.iter().any(Option::is_some) {
                    owner.progress()?;
                    if boot_nanos()? >= deadline {
                        return Err(invalid("forwarding ACK was not consumed"));
                    }
                }
                if owner.failed() {
                    return Err(invalid("valid independent forwarding failed"));
                }
                loop {
                    if owner.progress()?.is_some() || owner.transport_closed {
                        return Err(invalid("idle trusted Start closed provider transport"));
                    }
                    if let Some(message) = peer.receive::<ControlMessage>(deadline)? {
                        let ControlMessage::Ack(ack) = message else {
                            return Err(invalid("owner did not send ACK"));
                        };
                        ack.validate(&start, participant).map_err(invalid)?;
                        if owner.tickets.ticket() != Some(start.ticket) {
                            return Err(invalid("owner ACK preceded commitment"));
                        }
                        break;
                    }
                    std::thread::yield_now();
                }
            } else {
                owner.start(start, participant)?;
            }
            if owner.progress()?.is_some() || owner.transport_closed {
                return Err(invalid(
                    "trusted Start prematurely closed provider transport",
                ));
            }
            admission.begin(AdmissionOperation::Entering {
                obligation,
                resource: umbra_core::shutdown::Nonce::from_u128(3),
            })?;
            let admission_deadline = boot_nanos()? + 1_000_000_000;
            loop {
                owner.progress()?;
                if let Some(result) = admission.poll(admission_deadline)? {
                    if result != Err(AdmissionError::Frozen)
                        || owner.outstanding_admissions() != Some(1)
                        || owner.transport_closed
                    {
                        return Err(invalid(
                            "Start did not freeze admission before cancellation",
                        ));
                    }
                    break;
                }
            }
            owner.cancel("test worker transport failure".into())?;
            let deadline = boot_nanos()? + 2_000_000_000;
            loop {
                if let Some(status) = owner.progress()? {
                    return Ok(status);
                }
                if boot_nanos()? >= deadline {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })();
        let failed = owner.failed();
        let mut child = owner.into_child();
        if result.is_err() {
            let _ = child.kill();
        }
        let final_wait = child.wait().unwrap();
        assert!(result.unwrap().success());
        assert!(final_wait.success());
        assert!(
            failed,
            "provider collection cannot clear the failure trigger"
        );
    }
}
