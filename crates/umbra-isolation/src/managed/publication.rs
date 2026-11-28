//! Parallel Start/ACK publication independent of the main provider client.
use super::{boot_nanos, invalid, Channel};
use std::io;
use umbra_core::managed::{ControlMessage, Participant, Start, TicketState, ACK_NANOS};

/// An independently owned direct notification route.
pub struct Route {
    peer: Participant,
    channel: Channel,
}

impl Route {
    /// Bind a channel to the participant recorded in the pre-GO inventory.
    pub fn new(peer: Participant, channel: Channel) -> io::Result<Self> {
        peer.validate().map_err(invalid)?;
        Ok(Self { peer, channel })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Writing,
    AwaitingAck,
    Acknowledged,
    AcknowledgedClosed,
    Failed,
}

/// One combined publication attempt. No route receives a fresh timeout.
pub struct Publication {
    local: Participant,
    inventory: Vec<Participant>,
    start: Start,
    deadline: u64,
    routes: Vec<(Route, State)>,
    failures: Vec<String>,
    received: Vec<(Participant, Start)>,
}

impl Publication {
    /// Commit locally before publishing; attempt every route even on partial failure.
    pub fn begin(
        start: Start,
        expected_origin: Participant,
        local: Participant,
        tickets: &mut TicketState,
        routes: Vec<Route>,
    ) -> io::Result<Self> {
        let deadline = start
            .ticket
            .started
            .checked_add(ACK_NANOS)
            .ok_or_else(|| invalid("publication deadline overflow"))?
            .min(start.ticket.deadline);
        tickets
            .accept(start.clone(), expected_origin, local)
            .map_err(invalid)?;
        let mut publication = Self {
            local,
            inventory: routes
                .iter()
                .map(|route| route.peer)
                .chain([local, start.origin])
                .collect(),
            start,
            deadline,
            routes: Vec::new(),
            failures: Vec::new(),
            received: Vec::new(),
        };
        for mut route in routes {
            let state = match route
                .channel
                .queue(&ControlMessage::Start(publication.start.clone()))
            {
                Ok(()) => State::Writing,
                Err(error) => {
                    tickets.fail();
                    publication
                        .failures
                        .push(format!("{:?}: {error}", route.peer.role));
                    State::Failed
                }
            };
            publication.routes.push((route, state));
        }
        Ok(publication)
    }

    /// Supply the full pre-GO inventory for notifications forwarded by peers.
    pub fn inventory(&mut self, inventory: &[Participant]) {
        self.inventory = inventory.to_vec();
    }

    /// Advance every route once. True means all attempts terminated, not success.
    pub fn progress(&mut self, tickets: &mut TicketState) -> io::Result<bool> {
        let expired = boot_nanos()? >= self.deadline;
        for (route, state) in &mut self.routes {
            if matches!(state, State::AcknowledgedClosed | State::Failed)
                || (expired
                    && *state == State::Acknowledged
                    && route.channel.written == route.channel.output.len())
            {
                continue;
            }
            let result: io::Result<()> = if expired {
                Err(io::ErrorKind::TimedOut.into())
            } else {
                (|| {
                    if !route.channel.flush(self.deadline)? {
                        return Ok(());
                    }
                    if *state == State::Writing {
                        *state = State::AwaitingAck;
                    }
                    let message = match route.channel.receive::<ControlMessage>(self.deadline) {
                        // The receive clock can cross the cutoff after progress
                        // sampled it. Apply the same completed-route rule as the
                        // expiry branch above, without extending the deadline.
                        Err(error)
                            if *state == State::Acknowledged
                                && error.kind() == io::ErrorKind::TimedOut
                                && route.channel.written == route.channel.output.len() =>
                        {
                            return Ok(());
                        }
                        Err(error)
                            if *state == State::Acknowledged
                                && error.kind() == io::ErrorKind::UnexpectedEof
                                && route.channel.input.is_empty() =>
                        {
                            *state = State::AcknowledgedClosed;
                            return Ok(());
                        }
                        result => result?,
                    };
                    if let Some(message) = message {
                        match message {
                            ControlMessage::Ack(ack) => {
                                ack.validate(&self.start, route.peer).map_err(invalid)?;
                                if !self.inventory.contains(&ack.committed.origin) {
                                    return Err(invalid("ACK committed an unknown origin"));
                                }
                                tickets
                                    .accept(ack.committed.clone(), ack.committed.origin, self.local)
                                    .map_err(invalid)?;
                                *state = State::Acknowledged;
                            }
                            ControlMessage::Start(start) => {
                                if !self.inventory.contains(&start.origin) {
                                    return Err(invalid("concurrent Start has unknown origin"));
                                }
                                let ack_deadline = start
                                    .ticket
                                    .started
                                    .checked_add(ACK_NANOS)
                                    .ok_or_else(|| invalid("ACK clock overflow"))?;
                                self.deadline = self.deadline.min(ack_deadline);
                                let ack = tickets
                                    .accept(start.clone(), start.origin, self.local)
                                    .map_err(invalid)?;
                                if !self.received.iter().any(|(_, prior)| prior == &start) {
                                    if self.received.len() >= 16 {
                                        return Err(invalid(
                                            "publication notification bound exceeded",
                                        ));
                                    }
                                    self.received.push((route.peer, start));
                                }
                                route.channel.queue(&ControlMessage::Ack(ack))?;
                            }
                            _ => return Err(invalid("expected Start or acknowledgement")),
                        }
                    }
                    Ok(())
                })()
            };
            if let Err(error) = result {
                tickets.fail();
                self.failures
                    .push(format!("{:?}: {error}", route.peer.role));
                *state = State::Failed;
            }
        }
        Ok(self.routes.iter().all(|(route, state)| {
            matches!(state, State::AcknowledgedClosed | State::Failed)
                || (*state == State::Acknowledged
                    && route.channel.written == route.channel.output.len())
        }))
    }

    /// Wait for any unfinished route under the already committed cutoff.
    /// Expiry returns to progress so every remaining route records its failure.
    pub(super) fn wait(&self) -> io::Result<()> {
        let (mut descriptors, deadline) = self.wait_descriptors();
        super::readiness::wait_descriptors(&mut descriptors, deadline)
    }

    pub(super) fn wait_descriptors(&self) -> (Vec<libc::pollfd>, u64) {
        use std::os::fd::AsRawFd;
        let descriptors: Vec<_> = self
            .routes
            .iter()
            .filter_map(|(route, state)| {
                if matches!(state, State::AcknowledgedClosed | State::Failed) {
                    return None;
                }
                Some(libc::pollfd {
                    fd: route.channel.stream.as_raw_fd(),
                    events: if route.channel.written != route.channel.output.len() {
                        libc::POLLOUT
                    } else {
                        libc::POLLIN
                    },
                    revents: 0,
                })
            })
            .collect();
        (descriptors, self.deadline)
    }

    /// Sticky attributable errors; later child collection never erases these.
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// Inspect live publication state without advancing or acknowledging a route.
    #[cfg(feature = "test-clock")]
    pub fn test_pending_peers(&self) -> Vec<Participant> {
        self.routes
            .iter()
            .filter_map(|(route, state)| {
                (matches!(state, State::Writing | State::AwaitingAck)
                    || (*state == State::Acknowledged
                        && route.channel.written != route.channel.output.len()))
                .then_some(route.peer)
            })
            .collect()
    }

    /// Original notifications committed during this exchange, paired with the
    /// delivering peer. The role loop must retain them for forwarding.
    pub fn received(&self) -> &[(Participant, Start)] {
        &self.received
    }

    /// Recover the independent routes after publication for role-loop ownership.
    pub fn into_routes(self) -> Vec<Route> {
        self.routes.into_iter().map(|(route, _)| route).collect()
    }
}

impl Route {
    /// Bound peer identity used by owner RPC selection.
    pub fn peer(&self) -> Participant {
        self.peer
    }

    /// Borrow the exclusively owned channel after Start publication has completed.
    pub fn channel(&mut self) -> &mut Channel {
        &mut self.channel
    }

    /// Receive an original Start and commit it before queueing its ACK.
    /// `expected_origin` comes from the complete pre-GO participant inventory,
    /// so a forwarding peer cannot replace the origin with its own identity.
    pub fn receive_start(
        &mut self,
        tickets: &mut TicketState,
        local: Participant,
        expected_origin: Participant,
        deadline: u64,
    ) -> io::Result<Option<Start>> {
        self.receive_start_from(tickets, local, &[expected_origin], deadline)
    }

    /// Accept a forwarded original only when its origin is in the pre-GO inventory.
    pub fn receive_start_from(
        &mut self,
        tickets: &mut TicketState,
        local: Participant,
        inventory: &[Participant],
        deadline: u64,
    ) -> io::Result<Option<Start>> {
        let Some(message) = self.channel.receive::<ControlMessage>(deadline)? else {
            return Ok(None);
        };
        let ControlMessage::Start(start) = message else {
            return Err(invalid("expected cleanup Start"));
        };
        if !inventory.contains(&start.origin) {
            tickets.fail();
            return Err(invalid("Start origin absent from pre-GO inventory"));
        }
        let expected_origin = start.origin;
        let ack = tickets
            .accept(start.clone(), expected_origin, local)
            .map_err(invalid)?;
        if let Err(error) = self.channel.queue(&ControlMessage::Ack(ack)) {
            tickets.fail();
            return Err(error);
        }
        Ok(Some(start))
    }

    /// Advance a queued ACK under the original publication deadline.
    pub fn flush_ack(&mut self, start: &Start, tickets: &mut TicketState) -> io::Result<bool> {
        let deadline = start
            .ticket
            .started
            .checked_add(ACK_NANOS)
            .ok_or_else(|| invalid("ACK deadline overflow"))?
            .min(start.ticket.deadline);
        self.channel.flush(deadline).inspect_err(|_| tickets.fail())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use umbra_core::managed::{CleanupPath, Role, RunIdentity, Ticket};

    #[cfg(feature = "test-clock")]
    #[test]
    fn acknowledged_route_keeps_success_when_clock_crosses_receive_cutoff() {
        use std::{cell::Cell, rc::Rc};
        let _clock = super::super::test_clock::install(|| Ok(100));
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let local = Participant {
            role: Role::Worker,
            pid: 42,
            birth: (1, 0),
        };
        let peer = Participant {
            role: Role::CleanupOwner,
            pid: 43,
            birth: (1, 0),
        };
        let start = Start {
            run,
            origin: local,
            ticket: Ticket::new(100, CleanupPath::Trusted, 1).unwrap(),
        };
        let cutoff = start.ticket.started + ACK_NANOS;
        let (left, right) = UnixStream::pair().unwrap();
        let mut tickets = TicketState::new(run, 1).unwrap();
        let mut remote = TicketState::new(run, 1).unwrap();
        let mut publication = Publication::begin(
            start.clone(),
            local,
            local,
            &mut tickets,
            vec![Route::new(peer, Channel::new(left).unwrap()).unwrap()],
        )
        .unwrap();
        let mut channel = Channel::new(right).unwrap();
        assert!(!publication.progress(&mut tickets).unwrap());
        assert!(matches!(
            channel.receive::<ControlMessage>(cutoff).unwrap(),
            Some(ControlMessage::Start(found)) if found == start
        ));
        let ack = remote.accept(start, local, peer).unwrap();
        channel.queue(&ControlMessage::Ack(ack)).unwrap();
        assert!(channel.flush(cutoff).unwrap());
        assert!(publication.progress(&mut tickets).unwrap());
        assert!(!tickets.failed());

        // The controller may advance after observing a completed publication
        // while progress is between its expiry check and the channel read.
        let samples = Rc::new(Cell::new(0));
        let observed = samples.clone();
        let _crossing = super::super::test_clock::install(move || {
            let prior = observed.get();
            observed.set(prior + 1);
            Ok(if prior == 0 { cutoff - 1 } else { cutoff })
        });
        assert!(publication.progress(&mut tickets).unwrap());
        assert!(samples.get() >= 2, "exercise both clock samples");
        assert!(
            !tickets.failed(),
            "completed publication failed at cutoff: {:?}",
            publication.failures()
        );
    }

    #[test]
    fn acknowledged_route_still_acknowledges_concurrent_start() {
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let local = Participant {
            role: Role::Worker,
            pid: 42,
            birth: (1, 0),
        };
        let peer = Participant {
            role: Role::CleanupOwner,
            pid: 43,
            birth: (1, 0),
        };
        let silent = Participant {
            role: Role::Guardian,
            pid: 44,
            birth: (1, 0),
        };
        let start = Start {
            run,
            origin: local,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
        };
        let deadline = start.ticket.started + ACK_NANOS;
        let (left, right) = UnixStream::pair().unwrap();
        let (other, _silent_peer) = UnixStream::pair().unwrap();
        let mut tickets = TicketState::new(run, 1).unwrap();
        let mut remote = TicketState::new(run, 1).unwrap();
        let mut publication = Publication::begin(
            start.clone(),
            local,
            local,
            &mut tickets,
            vec![
                Route::new(peer, Channel::new(left).unwrap()).unwrap(),
                Route::new(silent, Channel::new(other).unwrap()).unwrap(),
            ],
        )
        .unwrap();
        let mut channel = Channel::new(right).unwrap();
        assert!(!publication.progress(&mut tickets).unwrap());
        assert!(matches!(
            channel.receive::<ControlMessage>(deadline).unwrap(),
            Some(ControlMessage::Start(_))
        ));
        let ack = remote.accept(start.clone(), local, peer).unwrap();
        channel.queue(&ControlMessage::Ack(ack)).unwrap();
        assert!(channel.flush(deadline).unwrap());
        assert!(!publication.progress(&mut tickets).unwrap());
        let concurrent = Start {
            origin: peer,
            ..start.clone()
        };
        channel.queue(&ControlMessage::Start(concurrent)).unwrap();
        assert!(channel.flush(deadline).unwrap());
        assert!(!publication.progress(&mut tickets).unwrap());
        #[cfg(feature = "test-clock")]
        assert!(
            publication.test_pending_peers().contains(&peer),
            "queued concurrent ACK must keep the observation pending"
        );
        assert!(!publication.progress(&mut tickets).unwrap());
        assert!(
            matches!(channel.receive::<ControlMessage>(deadline).unwrap(),
            Some(ControlMessage::Ack(ack)) if ack.committed.ticket == start.ticket),
            "an acknowledged route must answer Start while another peer is pending"
        );
    }

    #[test]
    fn simultaneous_start_commits_earliest_before_acknowledging_both() {
        simultaneous(false);
    }

    #[test]
    fn forwarded_origin_from_full_inventory_keeps_original_ticket() {
        simultaneous(true);
    }

    fn simultaneous(forwarded: bool) {
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let a = Participant {
            role: Role::Worker,
            pid: 42,
            birth: (1, 0),
        };
        let b = Participant {
            role: Role::CleanupOwner,
            pid: 43,
            birth: (1, 0),
        };
        let origin = if forwarded {
            Participant {
                role: Role::Guardian,
                pid: 44,
                birth: (1, 0),
            }
        } else {
            a
        };
        let (left, right) = UnixStream::pair().unwrap();
        let mut sa = TicketState::new(run, 1).unwrap();
        let mut sb = TicketState::new(run, 1).unwrap();
        let first = Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap();
        let later = Ticket::new(first.started + 1, CleanupPath::Trusted, 1).unwrap();
        let mut pa = Publication::begin(
            Start {
                run,
                origin,
                ticket: first,
            },
            origin,
            a,
            &mut sa,
            vec![Route::new(b, Channel::new(left).unwrap()).unwrap()],
        )
        .unwrap();
        let mut pb = Publication::begin(
            Start {
                run,
                origin: b,
                ticket: later,
            },
            b,
            b,
            &mut sb,
            vec![Route::new(a, Channel::new(right).unwrap()).unwrap()],
        )
        .unwrap();
        pa.inventory(&[a, b, origin]);
        pb.inventory(&[a, b, origin]);
        let mut done = false;
        for _ in 0..20 {
            let da = pa.progress(&mut sa).unwrap();
            let db = pb.progress(&mut sb).unwrap();
            if da && db {
                done = true;
                break;
            }
        }
        assert!(done);
        assert!(
            pb.received().iter().any(|(peer, start)| {
                *peer == a && start.origin == origin && start.ticket == first
            }),
            "forwarded original must remain available to the role loop"
        );
        assert!(
            pa.received()
                .iter()
                .any(|(peer, start)| { *peer == b && start.origin == b && start.ticket == later }),
            "simultaneous notification must remain available to the role loop"
        );
        assert!(pa.failures().is_empty() && pb.failures().is_empty());
        assert_eq!(sa.ticket(), Some(first));
        assert_eq!(sb.ticket(), Some(first));
    }

    #[test]
    fn failed_direct_route_does_not_prevent_alternate_ack() {
        let run: RunIdentity = serde_json::from_str(
            r#"{"nonce":"00000000-0000-0000-0000-000000000001","generation":1}"#,
        )
        .unwrap();
        let origin = Participant {
            role: Role::Worker,
            pid: 42,
            birth: (1, 0),
        };
        let relay = Participant {
            role: Role::Relay,
            pid: 43,
            birth: (1, 0),
        };
        let caller = Participant {
            role: Role::Caller,
            pid: 44,
            birth: (1, 0),
        };
        let (dead, gone) = UnixStream::pair().unwrap();
        drop(gone);
        let (out, incoming) = UnixStream::pair().unwrap();
        let mut receiver = Route::new(origin, Channel::new(incoming).unwrap()).unwrap();
        let mut sender_tickets = TicketState::new(run, 64).unwrap();
        let mut receiver_tickets = TicketState::new(run, 64).unwrap();
        let start = Start {
            run,
            origin,
            ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 64).unwrap(),
        };
        let mut publication = Publication::begin(
            start.clone(),
            origin,
            origin,
            &mut sender_tickets,
            vec![
                Route::new(caller, Channel::new(dead).unwrap()).unwrap(),
                Route::new(relay, Channel::new(out).unwrap()).unwrap(),
            ],
        )
        .unwrap();
        while !sender_tickets.failed() {
            assert!(
                boot_nanos().unwrap() < publication.deadline,
                "closed direct route must be observed before the publication timeout"
            );
            assert!(!publication.progress(&mut sender_tickets).unwrap());
            std::thread::yield_now();
        }
        assert!(
            boot_nanos().unwrap() < publication.deadline,
            "timeout is not evidence of the closed direct route"
        );
        assert!(sender_tickets.failed());
        let original_deadline = publication.deadline;
        publication.deadline = boot_nanos().unwrap() + 20_000_000;
        publication.wait().unwrap();
        assert!(boot_nanos().unwrap() >= publication.deadline);
        // Only the test selects a second cutoff to continue checking alternate
        // delivery; production retains its original deadline throughout.
        publication.deadline = original_deadline;
        let received = receiver
            .receive_start(&mut receiver_tickets, relay, origin, publication.deadline)
            .unwrap()
            .unwrap();
        assert_eq!(receiver_tickets.ticket(), Some(start.ticket));
        assert!(receiver
            .flush_ack(&received, &mut receiver_tickets)
            .unwrap());
        publication.wait().unwrap();
        assert!(publication.progress(&mut sender_tickets).unwrap());
        assert_eq!(publication.failures().len(), 1);
        assert!(
            sender_tickets.failed(),
            "alternate success cannot erase direct-route failure"
        );
    }
}
