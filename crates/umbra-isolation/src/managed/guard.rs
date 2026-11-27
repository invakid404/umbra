//! Parent-role event polling, independent of the worker's main RPC connection.
use super::{
    boot_nanos, invalid,
    process::{ProcessWatch, SpawnedChild},
    publication::{Publication, Route},
};
use std::io;
use umbra_core::managed::{Participant, RunIdentity, Start, TicketState};

/// Notification and collection are different facts.
pub enum Event {
    /// Complete Start accepted and committed before its ACK was queued.
    Start(Start),
    /// Kernel exit notification; does not prove parent collection.
    Exited(Participant),
    /// Actual parent's waitpid receipt, including the raw kernel status.
    Collected(Participant, i32),
    /// Attributable channel, watch, wait or ticket error.
    Failed(String),
    /// An original-ticket stage has expired; the callback chooses its role action.
    OpportunityExpired(u64),
}

/// Explicit parent-role actions, executed against this guard's retained state.
#[derive(Clone)]
pub enum Action {
    /// Forward the original Start without changing origin or deadline.
    Publish(Start),
    /// Signal an actual child only after its original-ticket cutoff.
    Force {
        /// Pre-GO identity.
        participant: Participant,
        /// Absolute opportunity cutoff.
        not_before: u64,
    },
}
/// Whole parent-role outcome; unresolved handles remain explicit at expiry.
pub struct Outcome {
    /// Original worker failure, retained separately from role collection diagnostics.
    pub primary: Option<umbra_core::UmbraError>,
    /// Every accepted notification, wait receipt and attributed failure.
    pub events: Vec<Event>,
    /// Sticky failure independent of eventual collection.
    pub failed: bool,
    /// Still-owned children retained for the remaining original-ticket fallback.
    pub unresolved: Vec<SpawnedChild>,
}

struct Peer {
    route: Route,
    identity: Participant,
    ack: Option<Start>,
    failed: bool,
    received: bool,
}

struct Owned {
    child: SpawnedChild,
    identity: Participant,
    collected: bool,
    lost: bool,
}

/// One guardian, relay or external caller's independent monitoring state.
pub struct Guard {
    local: Participant,
    run: RunIdentity,
    tickets: TicketState,
    watch: ProcessWatch,
    peers: Vec<Peer>,
    children: Vec<Owned>,
    expired: bool,
    inventory: Vec<Participant>,
    watch_failed: bool,
    clock_failed: bool,
    capacity: u64,
    publication: Option<Publication>,
    published: Vec<Start>,
    stage_events: std::collections::BTreeSet<u64>,
    exchanges: std::collections::BTreeMap<umbra_core::managed::Role, super::Channel>,
    pending_failure: Option<Start>,
}

impl Guard {
    pub(super) fn watch_descriptor(&self) -> i32 {
        self.watch.descriptor()
    }

    /// Commit a failed cleanup trigger before returning to publication polling.
    /// An existing cleanup allocation is preserved without renewal.
    pub fn notify_failure(&mut self) -> io::Result<()> {
        let ticket = match self.tickets.ticket() {
            Some(ticket) => ticket,
            None => umbra_core::managed::Ticket::new(
                boot_nanos()?,
                umbra_core::managed::CleanupPath::Failed,
                self.capacity,
            )
            .map_err(invalid)?,
        };
        let start = Start {
            run: self.run,
            origin: self.local,
            ticket,
        };
        self.tickets
            .accept(start.clone(), self.local, self.local)
            .map_err(invalid)?;
        self.tickets.fail();
        self.pending_failure = Some(start);
        Ok(())
    }

    /// Compose a guardian/relay runtime from its executable assignment and
    /// already installed startup queue. Prepare this before releasing GO.
    pub fn from_entry(
        entry: &mut super::executable::Entry,
        watch: ProcessWatch,
    ) -> io::Result<Self> {
        Self::from_entry_restoring(entry, watch, Vec::new())
    }

    /// Restore assigned startup channels before validating runtime coverage.
    pub fn from_entry_restoring(
        entry: &mut super::executable::Entry,
        watch: ProcessWatch,
        restored: Vec<(umbra_core::managed::Role, super::Channel)>,
    ) -> io::Result<Self> {
        use umbra_core::managed::Role;
        if !matches!(entry.role, Role::Guardian | Role::Relay) {
            return Err(invalid("entry is not a parent runtime role"));
        }
        let inventory = entry
            .inventory()
            .ok_or_else(|| invalid("parent runtime before inventory"))?
            .clone();
        let local = super::process::identity(std::process::id(), entry.role)?;
        let mut channels = super::channels::RuntimeChannels::from_entry(entry)?;
        for (peer, channel) in restored {
            channels.insert(peer, channel)?;
        }
        let channels = channels.finish()?;
        let mut guard = Self::from_startup(local, &inventory, entry.launch.capacity, watch)?;
        for route in channels {
            guard.route(route.peer(), route);
        }
        Ok(guard)
    }

    /// Transfer bootstrap child handles after GO forwarding. Validation happens
    /// before consuming any handle, preserving all custody on an error.
    pub fn adopt_ready_children(
        &mut self,
        children: &mut Vec<super::executable::PendingRole>,
    ) -> io::Result<()> {
        let mut identities = Vec::new();
        for child in children.iter() {
            let (participant, parent) = child.guard_identity()?;
            if parent != self.local
                || participant.pid != child.child.pid()
                || !self.inventory.contains(&participant)
                || identities.contains(&participant)
                || self
                    .children
                    .iter()
                    .any(|owned| owned.identity.pid == participant.pid)
            {
                return Err(invalid("runtime child custody differs from startup"));
            }
            identities.push(participant);
        }
        for (child, identity) in children.drain(..).zip(identities) {
            let (child, exchange) = child.into_runtime_parts();
            if matches!(
                identity.role,
                umbra_core::managed::Role::Worker
                    | umbra_core::managed::Role::Guardian
                    | umbra_core::managed::Role::Relay
            ) {
                self.exchanges.insert(identity.role, exchange);
            }
            self.children.push(Owned {
                child,
                identity,
                collected: false,
                lost: false,
            });
        }
        Ok(())
    }

    /// Move the child's observer/result channel without releasing child custody.
    pub fn take_exchange(&mut self, role: umbra_core::managed::Role) -> io::Result<super::Channel> {
        self.exchanges
            .remove(&role)
            .ok_or_else(|| invalid("child exchange unavailable"))
    }

    /// Run the parent fallback schedule inside the original helper allocation.
    /// No original ticket means no timer and no invented caller-return bound.
    /// Only actual children are signalled; unresolved custody stays in Outcome.
    pub fn run_parent(self) -> Outcome {
        self.run_parent_with_progress(|_| Ok(()))
    }

    /// Poll application forwarding alongside independent cleanup and collection.
    pub fn run_parent_with_progress(
        self,
        mut progress: impl FnMut(Option<umbra_core::managed::Ticket>) -> io::Result<()>,
    ) -> Outcome {
        self.run_parent_observed(|guard| progress(guard.tickets.ticket()))
    }

    /// Observe the actual production state without replacing its timers or loop.
    #[cfg(feature = "test-clock")]
    pub fn run_parent_with_test_observer(
        self,
        mut observe: impl FnMut(super::test_clock::GuardState) -> io::Result<()>,
    ) -> Outcome {
        self.run_parent_observed(|guard| observe(guard.test_clock_state()))
    }

    /// Add a fixture activity source while retaining the production parent loop.
    #[cfg(feature = "test-clock")]
    pub fn run_parent_with_test_activity(
        self,
        mut observe: impl FnMut(super::test_clock::GuardState) -> io::Result<()>,
        wait: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> Outcome {
        self.run_parent_waiting(|guard| observe(guard.test_clock_state()), wait)
    }

    fn run_parent_observed(self, progress: impl FnMut(&Self) -> io::Result<()>) -> Outcome {
        self.run_parent_waiting(progress, |guard| guard.wait_for_activity(None))
    }

    /// Run application progress and its borrowed readiness together with cleanup.
    pub fn run_parent_with_wait(
        self,
        mut progress: impl FnMut(Option<umbra_core::managed::Ticket>) -> io::Result<()>,
        wait: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> Outcome {
        self.run_parent_waiting(|guard| progress(guard.tickets.ticket()), wait)
    }

    fn run_parent_waiting(
        self,
        progress: impl FnMut(&Self) -> io::Result<()>,
        wait: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> Outcome {
        let local = self.local.role;
        let capacity = self.capacity;
        let children: Vec<_> = self.children.iter().map(|child| child.identity).collect();
        let mut forced = std::collections::BTreeSet::new();
        self.run_with_progress(
            move |event, ticket| {
                let Event::OpportunityExpired(now) = event else {
                    return Ok(Vec::new());
                };
                let Some(ticket) = ticket else {
                    return Ok(Vec::new());
                };
                let mut actions = Vec::new();
                for participant in &children {
                    let cutoff = parent_cutoff(local, participant.role, ticket, capacity)?;
                    if *now >= cutoff && forced.insert(participant.pid) {
                        actions.push(Action::Force {
                            participant: *participant,
                            not_before: cutoff,
                        });
                    }
                }
                Ok(actions)
            },
            progress,
            wait,
        )
    }

    /// Drive this parent's independent observation and collection loop.
    /// Original Starts are forwarded automatically. The hook schedules fallback;
    /// it receives the original ticket and cannot replace this guard's deadline.
    pub fn run(
        self,
        on_event: impl FnMut(&Event, Option<umbra_core::managed::Ticket>) -> io::Result<Vec<Action>>,
    ) -> Outcome {
        self.run_with_progress(on_event, |_| Ok(()), |guard| guard.wait_for_activity(None))
    }

    fn run_with_progress(
        mut self,
        mut on_event: impl FnMut(&Event, Option<umbra_core::managed::Ticket>) -> io::Result<Vec<Action>>,
        mut progress: impl FnMut(&Self) -> io::Result<()>,
        mut wait: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> Outcome {
        let mut progress_failed = false;
        let mut events = Vec::new();
        let mut pending = std::collections::VecDeque::new();
        loop {
            if let Some(start) = self.pending_failure.take() {
                pending.push_front(Action::Publish(start));
            }
            if !progress_failed {
                if let Err(error) = progress(&self) {
                    progress_failed = true;
                    self.tickets.fail();
                    events.push(Event::Failed(format!("parent forwarding: {error}")));
                    if let Err(error) = self.notify_failure() {
                        events.push(Event::Failed(format!(
                            "parent failure notification: {error}"
                        )));
                    }
                }
            }
            for event in self.poll() {
                if let Event::Start(start) = &event {
                    if pending.len() < 16 {
                        pending.push_back(Action::Publish(start.clone()));
                    } else {
                        self.tickets.fail();
                        events.push(Event::Failed(
                            "Start forwarding queue exceeded bound".into(),
                        ));
                    }
                }
                let result = on_event(&event, self.tickets.ticket()).and_then(|actions| {
                    if pending.len() + actions.len() > 16 {
                        return Err(invalid("parent action queue exceeded bound"));
                    }
                    pending.extend(actions);
                    Ok(())
                });
                if let Err(error) = result {
                    self.tickets.fail();
                    events.push(Event::Failed(format!("parent role action: {error}")));
                }
                events.push(event);
            }
            while let Some(action) = pending.pop_front() {
                match self.act(action.clone()) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        pending.push_front(action);
                        break;
                    }
                    Err(error) => {
                        self.tickets.fail();
                        events.push(Event::Failed(format!("parent action: {error}")));
                    }
                    Ok(()) => {}
                }
            }
            if (self.collected()
                && pending.is_empty()
                && self.pending_failure.is_none()
                && self.publication.is_none()
                && self
                    .peers
                    .iter()
                    .all(|peer| peer.failed || peer.ack.is_none()))
                || self.expired
            {
                break;
            }
            let waiting = if progress_failed {
                self.wait_for_activity(None)
            } else {
                wait(&mut self)
            };
            if let Err(error) = waiting {
                self.tickets.fail();
                events.push(Event::Failed(format!("parent readiness: {error}")));
                if let Err(error) = self.notify_failure() {
                    events.push(Event::Failed(format!(
                        "parent failure notification: {error}"
                    )));
                }
            }
        }
        #[cfg(feature = "shutdown-witness")]
        {
            let details = serde_json::json!({
                "participant": self.local,
                "failed": self.failed() || self.tickets.ticket().is_none(),
                "expired": self.expired,
                "unresolved": self.children.iter().filter(|child| !child.collected).count(),
                "errors": events.iter().filter(|event| matches!(event, Event::Failed(_))).count(),
                "opportunities_expired": events.iter().filter(|event| matches!(event, Event::OpportunityExpired(_))).count(),
                "observed_exits": events.iter().filter_map(|event| match event {
                    Event::Exited(participant) => Some(participant),
                    _ => None,
                }).collect::<Vec<_>>(),
                "collected": events.iter().filter_map(|event| match event {
                    Event::Collected(participant, status) => Some(serde_json::json!({
                        "participant": participant, "status": status,
                    })),
                    _ => None,
                }).collect::<Vec<_>>(),
            });
            crate::performance_observation::record_details(
                "managed_guard",
                "managed_guard_completed",
                Some(u64::from(self.local.pid)),
                details.clone(),
            );
            umbra_platform::shutdown::witness(serde_json::json!({
                "event": "managed-guard-completed", "details": details,
            }));
        }
        Outcome {
            primary: None,
            failed: self.failed() || self.tickets.ticket().is_none(),
            events,
            unresolved: self
                .children
                .into_iter()
                .filter(|owned| !owned.collected)
                .map(|owned| owned.child)
                .collect(),
        }
    }

    /// Create the private queue after exec, without starting an inactivity timer.
    pub fn new(local: Participant, run: RunIdentity, capacity: u64) -> io::Result<Self> {
        Self::with_watch(local, run, capacity, ProcessWatch::new()?)
    }

    /// Preserve the startup queue and validated full inventory in the parent
    /// runtime without allocating a second event queue.
    pub fn from_startup(
        local: Participant,
        inventory: &super::inventory::Inventory,
        capacity: u64,
        watch: ProcessWatch,
    ) -> io::Result<Self> {
        inventory.validate(inventory.run, inventory.external_caller)?;
        if !watch.covers(&inventory.watches(local)?) {
            return Err(invalid("parent queue lacks required startup watches"));
        }
        let mut guard = Self::with_watch(local, inventory.run, capacity, watch)?;
        guard.inventory = inventory
            .members
            .iter()
            .map(|member| member.participant)
            .collect();
        Ok(guard)
    }

    fn with_watch(
        local: Participant,
        run: RunIdentity,
        capacity: u64,
        watch: ProcessWatch,
    ) -> io::Result<Self> {
        local.validate().map_err(invalid)?;
        Ok(Self {
            local,
            run,
            tickets: TicketState::new(run, capacity).map_err(invalid)?,
            watch,
            peers: Vec::new(),
            children: Vec::new(),
            expired: false,
            inventory: vec![local],
            watch_failed: false,
            clock_failed: false,
            capacity,
            publication: None,
            published: Vec::new(),
            stage_events: std::collections::BTreeSet::new(),
            exchanges: std::collections::BTreeMap::new(),
            pending_failure: None,
        })
    }

    fn act(&mut self, action: Action) -> io::Result<()> {
        match action {
            Action::Publish(start) => {
                if !self.inventory.contains(&start.origin) {
                    return Err(invalid("forwarded Start origin absent from inventory"));
                }
                if self.published.iter().any(|prior| {
                    prior.origin == start.origin && prior.ticket.deadline <= start.ticket.deadline
                }) {
                    return Ok(());
                }
                if self.publication.is_some() || self.peers.iter().any(|peer| peer.ack.is_some()) {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let (retained, forwarding): (Vec<_>, Vec<_>) = std::mem::take(&mut self.peers)
                    .into_iter()
                    .partition(|peer| peer.received);
                self.peers = retained;
                let routes = forwarding.into_iter().map(|peer| peer.route).collect();
                let mut publication = Publication::begin(
                    start.clone(),
                    start.origin,
                    self.local,
                    &mut self.tickets,
                    routes,
                )?;
                publication.inventory(&self.inventory);
                self.publication = Some(publication);
                self.published.retain(|prior| prior.origin != start.origin);
                self.published.push(start);
                Ok(())
            }
            Action::Force {
                participant,
                not_before,
            } => {
                let ticket = self
                    .tickets
                    .ticket()
                    .ok_or_else(|| invalid("force without original Start"))?;
                let child = self
                    .children
                    .iter_mut()
                    .find(|owned| owned.identity == participant)
                    .ok_or_else(|| invalid("force targets a non-child"))?;
                if child.child.force_after(participant, ticket, not_before)? {
                    self.tickets.fail();
                }
                Ok(())
            }
        }
    }

    /// Install a birth-validated watch even if another role holds a channel copy.
    pub fn watch(&mut self, participant: Participant) -> io::Result<()> {
        self.watch.install(participant)?;
        self.inventory.push(participant);
        Ok(())
    }

    /// Validate the full topology and install required observations before Ready.
    /// Failure retains any already-installed watches and permanently fails the run.
    pub fn install_inventory(
        &mut self,
        inventory: &super::inventory::Inventory,
        external_caller: bool,
    ) -> io::Result<Vec<Participant>> {
        let result = (|| {
            inventory.validate(self.run, external_caller)?;
            let watched = inventory.watches(self.local)?;
            for participant in &watched {
                self.watch.install(*participant)?;
            }
            self.inventory = inventory.members.iter().map(|m| m.participant).collect();
            Ok(watched)
        })();
        if result.is_err() {
            self.tickets.fail();
        }
        result
    }

    /// Attach a direct Start route from the pre-GO participant inventory.
    pub fn route(&mut self, identity: Participant, route: Route) {
        self.peers.push(Peer {
            identity,
            route,
            ack: None,
            failed: false,
            received: false,
        });
    }

    /// Adopt only an actual child; return the handle on identity mismatch.
    pub fn child(
        &mut self,
        identity: Participant,
        child: SpawnedChild,
    ) -> Result<(), SpawnedChild> {
        if child.pid() != identity.pid
            || self.children.iter().any(|c| c.identity.pid == identity.pid)
        {
            return Err(child);
        }
        if !self.watch.covers(&[identity]) && self.watch.install(identity).is_err() {
            self.tickets.fail();
            return Err(child);
        }
        self.children.push(Owned {
            child,
            identity,
            collected: false,
            lost: false,
        });
        Ok(())
    }

    /// Poll every independent source once without blocking on a silent peer.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(publication) = self.publication.as_mut() {
            match publication.progress(&mut self.tickets) {
                Ok(true) => {
                    events.extend(publication.failures().iter().cloned().map(Event::Failed));
                    events.extend(
                        publication
                            .received()
                            .iter()
                            .map(|(_, start)| Event::Start(start.clone())),
                    );
                    for route in self.publication.take().unwrap().into_routes() {
                        self.peers.push(Peer {
                            identity: route.peer(),
                            route,
                            ack: None,
                            failed: false,
                            received: false,
                        });
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    self.tickets.fail();
                    events.push(Event::Failed(format!("Start forwarding: {error}")));
                    events.extend(
                        publication
                            .received()
                            .iter()
                            .map(|(_, start)| Event::Start(start.clone())),
                    );
                    for route in self.publication.take().unwrap().into_routes() {
                        self.peers.push(Peer {
                            identity: route.peer(),
                            route,
                            ack: None,
                            failed: true,
                            received: false,
                        });
                    }
                }
            }
        }
        // No ticket means no session-age timeout. Even a lost route cannot
        // manufacture a caller-return bound before an original Start arrives.
        let deadline = self.receive_deadline();
        for peer in &mut self.peers {
            if peer.failed {
                continue;
            }
            let result = (|| {
                if let Some(start) = peer.ack.as_ref() {
                    if !peer.route.flush_ack(start, &mut self.tickets)? {
                        return Ok(());
                    }
                    peer.ack = None;
                }
                if let Some(start) = peer.route.receive_start_from(
                    &mut self.tickets,
                    self.local,
                    &self.inventory,
                    deadline,
                )? {
                    peer.ack = Some(start.clone());
                    peer.received = true;
                    events.push(Event::Start(start));
                }
                Ok(())
            })();
            if let Err(error) = result {
                let error: io::Error = error;
                peer.failed = true;
                if error.kind() == io::ErrorKind::UnexpectedEof
                    && self.tickets.ticket().is_some()
                    && peer.ack.is_none()
                    && peer.route.channel().at_frame_boundary()
                {
                    // A closed notification route is not collection evidence.
                    // Actual child waits and the outer worker-result exchange
                    // remain required. Partial frames and pending ACKs fail.
                    continue;
                }
                // A failed route cannot finish this ACK. Retaining it would
                // block independent Start forwarding until the outer deadline.
                peer.ack = None;
                self.tickets.fail();
                events.push(Event::Failed(format!(
                    "{:?} control: {error}",
                    peer.identity.role
                )));
            }
        }
        if !self.watch_failed {
            match self.watch.exited() {
                Ok(Some(participant)) => events.push(Event::Exited(participant)),
                Ok(None) => {}
                Err(error) => {
                    self.watch_failed = true;
                    self.tickets.fail();
                    events.push(Event::Failed(format!("process watch: {error}")));
                }
            }
        }
        for owned in &mut self.children {
            if owned.collected || owned.lost {
                continue;
            }
            match owned.child.collect() {
                Ok(Some(status)) => {
                    owned.collected = true;
                    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                        self.tickets.fail();
                        events.push(Event::Failed(exit_diagnostic(owned.identity.role, status)));
                    }
                    events.push(Event::Collected(owned.identity, status));
                    #[cfg(feature = "shutdown-witness")]
                    crate::performance_observation::record_details(
                        "collection_stage",
                        "child-collected",
                        Some(u64::from(owned.identity.pid)),
                        serde_json::json!({
                            "parent": self.local, "child": owned.identity, "status": status,
                        }),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    owned.lost = true;
                    self.tickets.fail();
                    events.push(Event::Failed(format!(
                        "{:?} parent wait: {error}",
                        owned.identity.role
                    )));
                }
            }
        }
        if !self.expired && !self.clock_failed {
            match boot_nanos() {
                Ok(now) if self.tickets.ticket().is_some_and(|t| now >= t.deadline) => {
                    self.expired = true;
                    self.tickets.fail();
                    events.push(Event::Failed("original cleanup ticket expired".into()));
                }
                Err(error) => {
                    self.clock_failed = true;
                    self.tickets.fail();
                    events.push(Event::Failed(format!("boot clock: {error}")));
                }
                Ok(now) => {
                    if let Some(ticket) = self.tickets.ticket() {
                        match ticket.cutoffs(self.capacity) {
                            Ok(cutoffs) => {
                                let mut stages =
                                    vec![cutoffs.cooperative, cutoffs.fallback, cutoffs.helpers];
                                for child in &self.children {
                                    if let Ok(cutoff) = parent_cutoff(
                                        self.local.role,
                                        child.identity.role,
                                        ticket,
                                        self.capacity,
                                    ) {
                                        stages.push(cutoff);
                                    }
                                }
                                for cutoff in stages {
                                    if now >= cutoff && self.stage_events.insert(cutoff) {
                                        events.push(Event::OpportunityExpired(cutoff));
                                    }
                                }
                            }
                            Err(error) => {
                                self.tickets.fail();
                                events.push(Event::Failed(error.into()));
                            }
                        }
                    }
                }
            }
        }
        events
    }

    /// Wait on cleanup, process-exit and application traffic without renewing a cutoff.
    /// The borrowed application descriptor remains owned by the progress callback.
    pub fn wait_for_activity(
        &mut self,
        application: Option<(std::os::fd::BorrowedFd<'_>, bool, u64)>,
    ) -> io::Result<()> {
        self.wait_for_activity_sources(application.as_slice())
    }

    /// Include a test hook's file notification without replacing observer traffic.
    #[cfg(feature = "probe-cancellation-ticket")]
    pub fn wait_for_test_activity(
        &mut self,
        application: Option<(std::os::fd::BorrowedFd<'_>, bool, u64)>,
        activity: std::os::fd::BorrowedFd<'_>,
    ) -> io::Result<()> {
        let mut sources = application.into_iter().collect::<Vec<_>>();
        sources.push((activity, false, u64::MAX));
        self.wait_for_activity_sources(&sources)
    }

    fn wait_for_activity_sources(
        &mut self,
        applications: &[(std::os::fd::BorrowedFd<'_>, bool, u64)],
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        if self.pending_failure.is_some() {
            return Ok(());
        }
        let mut deadline = self.receive_deadline();
        if let Some(ticket) = self.tickets.ticket() {
            let cutoffs = ticket.cutoffs(self.capacity).map_err(invalid)?;
            let mut stages = vec![cutoffs.cooperative, cutoffs.fallback, cutoffs.helpers];
            for child in &self.children {
                stages.push(parent_cutoff(
                    self.local.role,
                    child.identity.role,
                    ticket,
                    self.capacity,
                )?);
            }
            for cutoff in stages {
                if !self.stage_events.contains(&cutoff) {
                    deadline = deadline.min(cutoff);
                }
            }
        }
        let mut descriptors = Vec::new();
        if !self.watch_failed {
            descriptors.push(libc::pollfd {
                fd: self.watch.descriptor(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for peer in &mut self.peers {
            if peer.failed {
                continue;
            }
            if let Some(start) = &peer.ack {
                deadline = deadline.min(
                    start
                        .ticket
                        .started
                        .checked_add(umbra_core::managed::ACK_NANOS)
                        .ok_or_else(|| invalid("ACK cutoff overflow"))?,
                );
            }
            let (fd, writing) = peer.route.channel().wait_interest();
            descriptors.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            });
        }
        if let Some(publication) = &self.publication {
            let (routes, cutoff) = publication.wait_descriptors();
            descriptors.extend(routes);
            deadline = deadline.min(cutoff);
        }
        for &(fd, writing, cutoff) in applications {
            descriptors.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            });
            deadline = deadline.min(cutoff);
        }
        super::readiness::wait_descriptors(&mut descriptors, deadline)
    }

    /// Parent collection never clears notification or protocol failure.
    pub fn failed(&self) -> bool {
        self.tickets.failed()
    }

    fn receive_deadline(&self) -> u64 {
        self.tickets
            .ticket()
            .map_or(u64::MAX, |ticket| ticket.deadline)
    }

    /// Inspect the same state used by notification reads and stage polling.
    #[cfg(feature = "test-clock")]
    pub fn test_clock_state(&self) -> super::test_clock::GuardState {
        super::test_clock::GuardState {
            ticket: self.tickets.ticket(),
            receive_deadline: self.receive_deadline(),
            failed: self.tickets.failed(),
            expired: self.expired,
            stage_events: self.stage_events.iter().copied().collect(),
            pending_publication: self
                .publication
                .as_ref()
                .map_or_else(Vec::new, Publication::test_pending_peers),
        }
    }

    /// True only after every owned child produced an actual parent wait receipt.
    pub fn collected(&self) -> bool {
        self.children.iter().all(|child| child.collected)
    }
}

fn parent_cutoff(
    parent: umbra_core::managed::Role,
    child: umbra_core::managed::Role,
    ticket: umbra_core::managed::Ticket,
    capacity: u64,
) -> io::Result<u64> {
    use umbra_core::managed::Role;
    let slot = match (parent, child) {
        (Role::Guardian, Role::Worker) => 0,
        (Role::Guardian, Role::CleanupOwner) => 1,
        (Role::Relay, Role::Guardian) => 2,
        (Role::Caller, Role::Relay) => 3,
        _ => return Err(invalid("unsupported parent fallback edge")),
    };
    let cutoffs = ticket.cutoffs(capacity).map_err(invalid)?;
    let cutoff = cutoffs
        .fallback
        .checked_add(slot * 1_000_000_000)
        .ok_or_else(|| invalid("parent fallback clock overflow"))?;
    if cutoff >= cutoffs.helpers {
        return Err(invalid("parent fallback exceeds helper allocation"));
    }
    Ok(cutoff)
}

fn exit_diagnostic(role: umbra_core::managed::Role, status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("{role:?} exited with code {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("{role:?} ended by signal {}", libc::WTERMSIG(status))
    } else {
        format!("{role:?} returned unexpected wait status {status}")
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use umbra_core::managed::{CleanupPath, Role, Ticket};

    #[test]
    fn application_readiness_wakes_guard_without_start() {
        use std::io::Write;
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        let local = super::super::process::identity(std::process::id(), Role::Caller).unwrap();
        let run = RunIdentity {
            nonce: uuid::Uuid::new_v4(),
            generation: 1,
        };
        let mut guard = Guard::new(local, run, 1).unwrap();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[1]).unwrap();
        let cutoff = boot_nanos().unwrap() + 1_000_000_000;
        guard
            .wait_for_activity(Some((reader.as_fd(), false, cutoff)))
            .unwrap();
        assert!(boot_nanos().unwrap() < cutoff);
        assert!(guard.tickets.ticket().is_none());
        assert!(!guard.failed());
    }

    #[test]
    fn child_failure_reports_exit_code() {
        assert_eq!(
            exit_diagnostic(Role::Guardian, 1 << 8),
            "Guardian exited with code 1"
        );
        assert_eq!(
            exit_diagnostic(Role::Worker, 7 << 8),
            "Worker exited with code 7"
        );
    }

    #[cfg(feature = "test-clock")]
    #[test]
    fn test_observer_publishes_polled_state_before_wait() {
        use super::super::process::identity;
        use std::cell::Cell;
        use std::os::unix::net::UnixStream;
        let local = identity(std::process::id(), Role::Guardian).unwrap();
        let run = RunIdentity {
            nonce: uuid::Uuid::new_v4(),
            generation: 1,
        };
        let mut guard = Guard::new(local, run, 1).unwrap();
        let child = SpawnedChild::spawn(
            std::path::Path::new("/bin/sleep"),
            &["sleep".into(), "30".into()],
            &[],
            false,
            &[],
        )
        .unwrap();
        let participant = identity(child.pid(), Role::Worker).unwrap();
        assert!(guard.child(participant, child).is_ok());
        let (writer, reader) = UnixStream::pair().unwrap();
        guard.route(
            participant,
            Route::new(participant, super::super::Channel::new(reader).unwrap()).unwrap(),
        );
        drop(writer);
        let observed = Cell::new(false);
        let stale = Cell::new(false);
        let first = Cell::new(true);
        let outcome = guard.run_parent_with_test_activity(
            |state| {
                observed.set(state.failed);
                Ok(())
            },
            |guard| {
                if first.replace(false) {
                    stale.set(guard.failed() && !observed.get());
                    // Only this test's still-owned child is stopped; the guard
                    // must subsequently produce the actual parent receipt.
                    unsafe {
                        libc::kill(participant.pid, libc::SIGKILL);
                    }
                }
                guard.wait_for_activity(None)
            },
        );
        assert!(outcome.unresolved.is_empty());
        assert!(outcome
            .events
            .iter()
            .any(|e| matches!(e, Event::Collected(p, _) if *p == participant)));
        assert!(!stale.get(), "observer slept with stale pre-poll state");
    }

    #[test]
    fn notification_eof_requires_start_and_complete_frames() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        for (started, partial, expected_failed) in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
        ] {
            let local =
                super::super::process::identity(std::process::id(), Role::Guardian).unwrap();
            let peer = super::super::process::identity(std::process::id(), Role::Worker).unwrap();
            let run = RunIdentity {
                nonce: uuid::Uuid::new_v4(),
                generation: 1,
            };
            let mut guard = Guard::new(local, run, 1).unwrap();
            if started {
                let start = Start {
                    run,
                    origin: local,
                    ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
                };
                guard.tickets.accept(start, local, local).unwrap();
            }
            let (mut writer, reader) = UnixStream::pair().unwrap();
            guard.route(
                peer,
                Route::new(peer, super::super::Channel::new(reader).unwrap()).unwrap(),
            );
            if partial {
                writer.write_all(&[0]).unwrap();
            }
            drop(writer);
            let events = guard.poll();
            assert_eq!(
                guard.failed(),
                expected_failed,
                "started={started} partial={partial}"
            );
            assert_eq!(
                events.iter().any(|event| matches!(event, Event::Failed(_))),
                expected_failed
            );
        }
    }

    #[test]
    fn parent_waits_for_original_cutoff_then_collects_stalled_child() {
        use super::super::process::identity;
        let local = identity(std::process::id(), Role::Guardian).unwrap();
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let mut guard = Guard::new(local, run, 1).unwrap();
        let child = SpawnedChild::spawn(
            std::path::Path::new("/bin/sleep"),
            &["sleep".into(), "30".into()],
            &[],
            false,
            &[],
        )
        .unwrap();
        let participant = identity(child.pid(), Role::Worker).unwrap();
        assert!(guard.child(participant, child).is_ok());
        let allocation = Ticket::new(0, CleanupPath::Failed, 1)
            .unwrap()
            .cutoffs(1)
            .unwrap();
        let cutoff = boot_nanos().unwrap() + 100_000_000;
        let ticket = Ticket::new(cutoff - allocation.fallback, CleanupPath::Failed, 1).unwrap();
        guard
            .tickets
            .accept(
                Start {
                    run,
                    origin: local,
                    ticket,
                },
                local,
                local,
            )
            .unwrap();
        let events = guard.poll();
        assert!(!events
            .iter()
            .any(|event| matches!(event, Event::Collected(..))));
        assert!(
            !guard.collected(),
            "live child must remain owned before cutoff"
        );
        let outcome = guard.run_parent();
        let finished = boot_nanos().unwrap();
        let collected = outcome.events.iter().any(|event| {
            matches!(
                event, Event::Collected(found, status)
                    if *found == participant && libc::WIFSIGNALED(*status)
                        && libc::WTERMSIG(*status) == libc::SIGKILL
            )
        });
        let unresolved = outcome.unresolved.len();
        // Retain actual-parent cleanup even when the policy assertion fails.
        for mut child in outcome.unresolved {
            let _ = child.force_after(participant, ticket, ticket.started);
            while child.collect().unwrap().is_none() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert_eq!(unresolved, 0);
        assert!(collected && outcome.failed);
        assert!(finished >= cutoff && finished < ticket.cutoffs(1).unwrap().helpers);
    }

    #[test]
    fn custody_ticket_deadlines() {
        for capacity in [1, 64] {
            for path in [CleanupPath::Trusted, CleanupPath::Failed] {
                let ticket = Ticket::new(1_000, path, capacity).unwrap();
                let stages = ticket.cutoffs(capacity).unwrap();
                let edges = [
                    (Role::Guardian, Role::Worker),
                    (Role::Guardian, Role::CleanupOwner),
                    (Role::Relay, Role::Guardian),
                    (Role::Caller, Role::Relay),
                ];
                let mut prior = None;
                for (parent, child) in edges {
                    let cutoff = parent_cutoff(parent, child, ticket, capacity).unwrap();
                    assert!(cutoff >= stages.fallback && cutoff < stages.helpers);
                    assert!(prior.is_none_or(|before| cutoff > before));
                    prior = Some(cutoff);
                }
                assert!(parent_cutoff(Role::Relay, Role::Provider, ticket, capacity).is_err());
            }
        }
    }
}
