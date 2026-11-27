//! Hosted macOS checks of production cleanup protocols with ordinary children.
#![cfg(target_os = "macos")]

use std::{
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::{Duration, Instant},
};
use umbra_core::managed::{
    CleanupPath, ControlMessage, Participant, Role, RunIdentity, Start, Ticket, TicketState,
    ACK_NANOS,
};
use umbra_isolation::managed::{
    boot_nanos,
    guard::{Event, Guard},
    process::{identity, ProcessWatch, SpawnedChild},
    publication::Route,
    Channel,
};
#[path = "support/runtime_clock.rs"]
mod runtime_clock;
use runtime_clock::SharedClock;
#[path = "support/cleanup_order.rs"]
mod cleanup_order;
#[path = "support/late_acquisition.rs"]
mod late_acquisition;
#[path = "support/runtime_tree.rs"]
mod runtime_tree;

fn cleanup_order_waits_for_held_barriers() {
    runtime_tree::run_ordered(false).unwrap();
}

fn cleanup_order_emergency_stays_unconfirmed() {
    runtime_tree::run_ordered(true).unwrap();
}

#[test]
fn r2_da_order_across_roles() {
    cleanup_order_waits_for_held_barriers();
    cleanup_order_emergency_stays_unconfirmed();
}

#[cfg(feature = "managed-endpoint-probe")]
#[test]
fn r2_exec_and_fork_endpoint_inventory() {
    let mut failed = Vec::new();
    for mode in ["cli", "library"] {
        let evidence = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_umbra-managed-endpoint-probe"))
            .arg(evidence.path())
            .arg(mode)
            .status()
            .unwrap();
        eprintln!("hosted endpoint inventory {mode}: {status}");
        if !status.success() {
            failed.push(mode);
        }
    }
    assert!(failed.is_empty(), "{failed:?} endpoint inventory failed");
}

#[test]
fn r2_creator_past_tail() {
    late_acquisition::run(true);
}

#[test]
fn r2_custody_owner_dies_no_false_collection() {
    runtime_tree::run(runtime_tree::Case::OwnerLost).unwrap();
}

#[test]
fn r2_acquire_after_expired_outer_census() {
    late_acquisition::run(false);
}

#[test]
fn runtime_tree_child() {
    runtime_tree::child().unwrap();
}

#[test]
fn custody_normal_reaps_all() {
    runtime_tree::run(runtime_tree::Case::Normal).unwrap();
}

#[test]
fn custody_stuck_custodian_reaps_all() {
    runtime_tree::run(runtime_tree::Case::StuckCustodian).unwrap();
}

#[test]
fn r2_uncancellable_work_outer_cleanup() {
    runtime_tree::run(runtime_tree::Case::UncancellableWorker).unwrap();
}

#[test]
fn r2_total_publication_loss_preserves_custody() {
    runtime_tree::total_publication_loss().unwrap();
}

#[test]
fn r2_library_relay_stalls_before_start() {
    runtime_tree::run(runtime_tree::Case::RelayStalled).unwrap();
}

#[test]
fn r2_guardian_stalls_before_first_trigger() {
    relay_commits_before_ack();
    runtime_tree::run(runtime_tree::Case::GuardianStalled).unwrap();
}

fn relay_commits_before_ack() {
    let clock = SharedClock::new(boot_nanos().unwrap());
    let _scope = clock.install();
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let worker = identity(std::process::id(), Role::Worker).unwrap();
    let relay = identity(std::process::id(), Role::Relay).unwrap();
    let start = Start {
        run,
        origin: worker,
        ticket: Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap(),
    };
    let (sender, receiver) = UnixStream::pair().unwrap();
    let mut sender = Channel::new(sender).unwrap();
    let mut receiver = Route::new(worker, Channel::new(receiver).unwrap()).unwrap();
    let mut tickets = TicketState::new(run, 1).unwrap();
    sender.queue(&ControlMessage::Start(start.clone())).unwrap();
    assert!(sender.flush(start.ticket.deadline).unwrap());
    clock.advance_to(start.ticket.started + 10_000_000);
    let received = receiver
        .receive_start_from(&mut tickets, relay, &[worker], start.ticket.deadline)
        .unwrap()
        .unwrap();
    assert!(receiver.flush_ack(&received, &mut tickets).unwrap());
    let Some(ControlMessage::Ack(ack)) = sender.receive(start.ticket.deadline).unwrap() else {
        panic!("relay omitted the publication acknowledgement");
    };
    ack.validate(&start, relay).unwrap();
    assert_eq!(
        tickets.ticket(),
        Some(start.ticket),
        "relay acknowledged before committing the original ticket"
    );
    assert_eq!(received, start, "relay renewed the received cleanup ticket");
}

#[test]
fn r2_library_owner_start_relay_stalled_worker_alive() {
    runtime_tree::run(runtime_tree::Case::OwnerDirect).unwrap();
}

#[test]
fn r2_library_received_start_forwarded_before_origin_exit() {
    runtime_tree::run(runtime_tree::Case::ForwardBeforeExit).unwrap();
}

#[test]
fn r2_grace_exhaustion_is_loud() {
    let clock = SharedClock::new(boot_nanos().unwrap());
    let _scope = clock.install();
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::Guardian).unwrap();
    let mut peers: Vec<_> = [Role::Worker, Role::CleanupOwner]
        .into_iter()
        .map(|role| Peer::spawn_with_clock(role, Some(&clock)))
        .collect();
    let mut guard = Guard::new(local, run, 1).unwrap();
    for peer in &mut peers {
        guard.watch(peer.participant).unwrap();
        guard.route(
            peer.participant,
            Route::new(peer.participant, peer.channel.take().unwrap()).unwrap(),
        );
        assert!(guard
            .child(peer.participant, peer.child.take().unwrap())
            .is_ok());
    }
    guard.notify_failure().unwrap();
    let original = guard.test_clock_state().ticket.unwrap();
    let mut phase = 0;
    let mut changed = false;
    let mut outcome = guard.run_parent_with_test_observer(|state| {
        changed |= state.ticket != Some(original);
        if phase == 0 && state.pending_publication.len() == peers.len() {
            clock.advance_to(original.started + ACK_NANOS + 10_000_000);
            phase = 1;
        } else if phase == 1 && state.pending_publication.is_empty() {
            for peer in &peers {
                if identity(peer.participant.pid, peer.participant.role)? != peer.participant {
                    return Err(std::io::Error::other(
                        "custody lost before original window exhausted",
                    ));
                }
            }
            clock.advance_to(original.deadline);
            phase = 2;
        }
        Ok(())
    });
    let unconfirmed = outcome.unresolved.len();
    let invented_collection = outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Collected(_, _)));
    let held = peers.iter().all(|peer| {
        identity(peer.participant.pid, peer.participant.role).ok() == Some(peer.participant)
    });
    // These ordinary stand-ins belong directly to the test parent. Production
    // has returned Unconfirmed; only the harness releases and collects them.
    for peer in &peers {
        peer.release().unwrap();
    }
    let mut rescue_statuses = Vec::new();
    for child in &mut outcome.unresolved {
        loop {
            if let Some(status) = child.collect().unwrap() {
                rescue_statuses.push(status);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(phase, 2, "parent abandoned custody at ACK expiry");
    assert!(!changed, "expired window was renewed");
    assert!(held && outcome.failed);
    assert_eq!(unconfirmed, 2);
    assert!(
        !invented_collection,
        "harness wait became a production receipt"
    );
    assert_eq!(rescue_statuses, [0, 0]);
    assert!(outcome.events.iter().any(|event| matches!(event, Event::Failed(message) if message == "original cleanup ticket expired")));
}

/// An idle peer withholds ACKs. When supplied an original Start, it scripts the
/// concurrent Start/ACK exchange. Its release latch is not a collection receipt.
#[test]
fn runtime_child() {
    let Some(directory) = std::env::var_os("UMBRA_R2_PEER_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    let clock_scope =
        std::env::var_os("UMBRA_R2_CLOCK").map(|path| runtime_clock::install(Path::new(&path)));
    let mut stream = if let Some(fd) = std::env::var_os("UMBRA_R2_INHERITED_FD") {
        let fd = fd.to_str().unwrap().parse().unwrap();
        // The test parent's explicit spawn file actions transferred this socket.
        unsafe { UnixStream::from_raw_fd(fd) }
    } else {
        UnixStream::connect(directory.join("s")).unwrap()
    };
    stream.write_all(b"R").unwrap();
    let mut channel = Channel::new(stream).unwrap();
    let bound = Instant::now() + Duration::from_secs(3);
    let mut last_clock = None;
    let mut scripted = None;
    while !directory.join("release").exists() {
        if scripted.is_none() && directory.join("start").exists() {
            let start: Start =
                serde_json::from_slice(&std::fs::read(directory.join("start")).unwrap()).unwrap();
            let mut tickets = TicketState::new(start.run, 1).unwrap();
            tickets
                .accept(start.clone(), start.origin, start.origin)
                .unwrap();
            channel
                .queue(&ControlMessage::Start(start.clone()))
                .unwrap();
            assert!(channel.flush(u64::MAX).unwrap());
            scripted = Some((tickets, start));
        }
        if let Some((tickets, original)) = &mut scripted {
            if let Some(message) = channel.receive::<ControlMessage>(u64::MAX).unwrap() {
                let caller =
                    identity(unsafe { libc::getppid() }.try_into().unwrap(), Role::Caller).unwrap();
                match message {
                    ControlMessage::Start(start) => {
                        let ack = tickets.accept(start, caller, original.origin).unwrap();
                        channel.queue(&ControlMessage::Ack(ack)).unwrap();
                        assert!(channel.flush(u64::MAX).unwrap());
                    }
                    ControlMessage::Ack(ack) => ack.validate(original, caller).unwrap(),
                    other => panic!("unexpected scripted message: {other:?}"),
                }
            }
        }
        if clock_scope.is_some() {
            let now = boot_nanos().unwrap();
            if last_clock != Some(now) {
                std::fs::write(directory.join("clock-next"), now.to_le_bytes()).unwrap();
                std::fs::rename(
                    directory.join("clock-next"),
                    directory.join("clock-observed"),
                )
                .unwrap();
                last_clock = Some(now);
            }
        }
        assert!(Instant::now() < bound, "test release latch not opened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Peer {
    directory: tempfile::TempDir,
    child: Option<SpawnedChild>,
    participant: Participant,
    channel: Option<Channel>,
}

impl Peer {
    fn inheriting(role: Role, stream: &UnixStream) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("u-r2-")
            .tempdir_in("/tmp")
            .unwrap();
        let executable = std::env::current_exe().unwrap();
        let environment = umbra_isolation::spawn::environment(
            &[
                umbra_isolation::managed::executable::ENV_ROLE,
                "UMBRA_R2_CLOCK",
                "UMBRA_R2_INHERITED_FD",
            ],
            &[
                (
                    "UMBRA_R2_PEER_DIR",
                    directory.path().to_str().unwrap().to_owned(),
                ),
                ("UMBRA_R2_INHERITED_FD", stream.as_raw_fd().to_string()),
            ],
        );
        let child = SpawnedChild::spawn(
            &executable,
            &[
                executable.as_os_str().to_owned(),
                "--exact".into(),
                "runtime_child".into(),
                "--nocapture".into(),
            ],
            &environment,
            false,
            &[stream.as_raw_fd()],
        )
        .unwrap();
        let participant = identity(child.pid(), role).unwrap();
        Self {
            directory,
            child: Some(child),
            participant,
            channel: None,
        }
    }

    fn spawn(role: Role) -> Self {
        Self::spawn_with_clock(role, None)
    }
    fn spawn_with_clock(role: Role, clock: Option<&SharedClock>) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("u-r2-")
            .tempdir_in("/tmp")
            .unwrap();
        let listener = UnixListener::bind(directory.path().join("s")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut added = vec![(
            "UMBRA_R2_PEER_DIR",
            directory.path().to_str().unwrap().to_owned(),
        )];
        if let Some(clock) = clock {
            added.push(("UMBRA_R2_CLOCK", clock.path().to_str().unwrap().to_owned()));
        }
        let environment = umbra_isolation::spawn::environment(
            &[
                umbra_isolation::managed::executable::ENV_ROLE,
                "UMBRA_R2_CLOCK",
                "UMBRA_R2_INHERITED_FD",
            ],
            &added,
        );
        let child = SpawnedChild::spawn(
            &executable,
            &[
                executable.as_os_str().to_owned(),
                "--exact".into(),
                "runtime_child".into(),
                "--nocapture".into(),
            ],
            &environment,
            false,
            &[],
        )
        .unwrap();
        let participant = identity(child.pid(), role).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "child rendezvous expired");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut ready = [0];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        Self {
            directory,
            child: Some(child),
            participant,
            channel: Some(Channel::new(stream).unwrap()),
        }
    }
    fn release(&self) -> std::io::Result<()> {
        std::fs::write(self.directory.path().join("release"), b"release")
    }
    fn send_start(&self, start: &Start) -> std::io::Result<()> {
        let next = self.directory.path().join("start-next");
        std::fs::write(&next, serde_json::to_vec(start)?)?;
        std::fs::rename(next, self.directory.path().join("start"))
    }
    fn collect_scripted_peer(&mut self) -> std::io::Result<i32> {
        loop {
            if let Some(status) = self.child.as_mut().unwrap().collect()? {
                self.child.take();
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.release();
        if let Some(child) = self.child.as_mut() {
            // Setup-failure cleanup by the same actual parent.
            while matches!(child.collect(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn parent_ack_loss(role: Role, child_roles: &[Role]) {
    use std::{cell::Cell, os::fd::AsFd};
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), role).unwrap();
    let mut guard = Guard::new(local, run, 1).unwrap();
    let mut peers: Vec<_> = child_roles.iter().copied().map(Peer::spawn).collect();
    for peer in &mut peers {
        guard.watch(peer.participant).unwrap();
        guard.route(
            peer.participant,
            Route::new(peer.participant, peer.channel.take().unwrap()).unwrap(),
        );
        assert!(guard
            .child(peer.participant, peer.child.take().unwrap())
            .is_ok());
    }
    guard.notify_failure().unwrap();
    let original = Cell::new(None);
    let held_past_ack = Cell::new(false);
    // The fixture releases its child ten milliseconds after ACK expiry.
    // Include that existing observation time in the production wait.
    let (observation, _held_endpoint) = UnixStream::pair().unwrap();
    let mut outcome = guard.run_parent_with_wait(
        |ticket| {
            let ticket = ticket.ok_or_else(|| std::io::Error::other("missing original ticket"))?;
            if let Some(prior) = original.get() {
                if ticket != prior {
                    return Err(std::io::Error::other("ticket renewed"));
                }
            } else {
                original.set(Some(ticket));
            }
            let now = boot_nanos()?;
            if !held_past_ack.get() && now >= ticket.started + ACK_NANOS + 10_000_000 {
                if now
                    >= ticket
                        .cutoffs(1)
                        .map_err(std::io::Error::other)?
                        .cooperative
                {
                    return Err(std::io::Error::other(
                        "observation missed local cleanup window",
                    ));
                }
                for peer in &peers {
                    if identity(peer.participant.pid, peer.participant.role)? != peer.participant {
                        return Err(std::io::Error::other("child custody lost at ACK expiry"));
                    }
                }
                held_past_ack.set(true);
                for peer in &peers {
                    peer.release()?;
                }
            }
            Ok(())
        },
        |guard| {
            let observation = original
                .get()
                .filter(|_| !held_past_ack.get())
                .map(|ticket| {
                    (
                        observation.as_fd(),
                        false,
                        ticket.started + ACK_NANOS + 10_000_000,
                    )
                });
            guard.wait_for_activity(observation)
        },
    );
    // Recover test children before asserting, without crediting these rescue
    // waits as successful production collection.
    for peer in &peers {
        peer.release().unwrap();
    }
    let unresolved = outcome.unresolved.len();
    for child in &mut outcome.unresolved {
        while child.collect().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(held_past_ack.get(), "parent stopped at combined ACK expiry");
    assert!(outcome.failed);
    assert_eq!(
        unresolved, 0,
        "production parent did not collect its children"
    );
    assert!(
        outcome
            .events
            .iter()
            .any(|event| matches!(event, Event::Failed(message) if message.contains("timed out"))),
        "missing ACK-expiry failure"
    );
    for peer in &peers {
        let receipts: Vec<_> = outcome
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Collected(participant, status) if *participant == peer.participant => {
                    Some(*status)
                }
                _ => None,
            })
            .collect();
        assert_eq!(receipts, [0], "missing exact successful parent wait");
    }
}

#[test]
fn r2_guardian_ack_loss_preserves_child_collection() {
    parent_ack_loss(Role::Guardian, &[Role::Worker, Role::CleanupOwner]);
}

#[test]
fn r2_relay_ack_loss_preserves_child_collection() {
    parent_ack_loss(Role::Relay, &[Role::Guardian]);
}

#[test]
fn r2_unknown_start_exit_never_renews() {
    let base = boot_nanos().unwrap();
    let clock = SharedClock::new(base);
    let _scope = clock.install();
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::Caller).unwrap();
    let mut peer = Peer::spawn_with_clock(Role::Relay, Some(&clock));
    let mut observed_exit = ProcessWatch::new().unwrap();
    observed_exit.install(peer.participant).unwrap();
    let mut guard = Guard::new(local, run, 1).unwrap();
    guard.watch(peer.participant).unwrap();
    assert!(guard
        .child(peer.participant, peer.child.take().unwrap())
        .is_ok());
    assert!(guard.poll().is_empty());
    // An arbitrarily old, unnotified caller still has no cleanup deadline.
    clock.set(base + 1_000_000_000_000);
    assert!(
        guard.poll().is_empty(),
        "session age invented a cleanup trigger"
    );
    assert_eq!(
        identity(peer.participant.pid, Role::Relay).unwrap(),
        peer.participant
    );
    let until = Instant::now() + Duration::from_secs(2);
    while std::fs::read(peer.directory.path().join("clock-observed"))
        .ok()
        .as_deref()
        != Some(&(base + 1_000_000_000_000).to_le_bytes()[..])
    {
        assert!(
            Instant::now() < until,
            "child did not observe the shared clock"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    peer.release().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while observed_exit.exited().unwrap().is_none() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut invented = false;
    let outcome = guard.run_parent_with_progress(|ticket| {
        invented |= ticket.is_some();
        Ok(())
    });
    assert!(
        !invented,
        "unknown-start late exit invented a fresh cleanup ticket"
    );
    assert!(outcome.failed, "unknown cleanup cannot certify success");
    assert!(outcome.unresolved.is_empty());
    assert!(!outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Start(_) | Event::OpportunityExpired(_))));
    assert!(outcome.events.iter().any(|event| matches!(event,
        Event::Collected(participant, 0) if *participant == peer.participant)));
}

#[test]
fn r2_owner_exit_with_inherited_fd() {
    let (mut parent_stream, inherited) = UnixStream::pair().unwrap();
    parent_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut owner = Peer::inheriting(Role::CleanupOwner, &inherited);
    let mut ready = [0];
    parent_stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    let mut holder = Peer::inheriting(Role::Worker, &inherited);
    parent_stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    drop(inherited);
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::Guardian).unwrap();
    let mut guard = Guard::new(local, run, 1).unwrap();
    let mut independent_watch = ProcessWatch::new().unwrap();
    independent_watch.install(owner.participant).unwrap();
    for peer in [&mut owner, &mut holder] {
        guard.watch(peer.participant).unwrap();
    }
    owner.release().unwrap();
    let bound = Instant::now() + Duration::from_secs(2);
    while independent_watch.exited().unwrap().is_none() {
        assert!(Instant::now() < bound, "fixture owner did not exit");
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut channel = Channel::new(parent_stream).unwrap();
    assert!(
        channel
            .receive::<ControlMessage>(boot_nanos().unwrap() + 1_000_000_000)
            .unwrap()
            .is_none(),
        "holder did not keep the owner endpoint open"
    );
    guard.route(
        owner.participant,
        Route::new(owner.participant, channel).unwrap(),
    );
    for peer in [&mut owner, &mut holder] {
        assert!(guard
            .child(peer.participant, peer.child.take().unwrap())
            .is_ok());
    }
    let mut observed = false;
    let mut rescued = false;
    let mut events = Vec::new();
    while !guard.collected() {
        for event in guard.poll() {
            if matches!(&event, Event::Exited(peer) if *peer == owner.participant) {
                observed = true;
                holder.release().unwrap();
            }
            events.push(event);
        }
        if Instant::now() >= bound {
            // The external fixture releases its child on regression. This is
            // not a production silence timer or successful watch observation.
            rescued = true;
            holder.release().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    holder.release().unwrap();
    // Any wait still available to this parent was omitted by production.
    let mut missing_wait = false;
    for peer in [&owner, &holder] {
        loop {
            let mut status = 0;
            let result =
                unsafe { libc::waitpid(peer.participant.pid as i32, &mut status, libc::WNOHANG) };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
                break;
            }
            missing_wait = true;
            if result > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(
        observed && !rescued,
        "independent owner exit observation was lost"
    );
    assert!(
        !missing_wait,
        "production guard omitted actual parent collection"
    );
    for peer in [&owner, &holder] {
        assert!(events.iter().any(
            |event| matches!(event, Event::Collected(found, 0) if *found == peer.participant)
        ));
    }
}

fn earlier_start(after_observation: bool) {
    let base = boot_nanos().unwrap();
    let clock = SharedClock::new(base);
    let _scope = clock.install();
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::Caller).unwrap();
    let mut relay = Peer::spawn_with_clock(Role::Relay, Some(&clock));
    let mut owner = Peer::spawn_with_clock(Role::CleanupOwner, Some(&clock));
    let mut observed =
        after_observation.then(|| Peer::spawn_with_clock(Role::Worker, Some(&clock)));
    let mut guard = Guard::new(local, run, 1).unwrap();
    guard.watch(relay.participant).unwrap();
    guard.watch(owner.participant).unwrap();
    guard.route(
        owner.participant,
        Route::new(owner.participant, owner.channel.take().unwrap()).unwrap(),
    );
    assert!(guard
        .child(relay.participant, relay.child.take().unwrap())
        .is_ok());
    let now = if let Some(peer) = &observed {
        guard.watch(peer.participant).unwrap();
        let mut observation = ProcessWatch::new().unwrap();
        observation.install(peer.participant).unwrap();
        clock.set(base + 1_000_000_000_000);
        peer.release().unwrap();
        let bound = Instant::now() + Duration::from_secs(2);
        while observation.exited().unwrap().is_none() {
            assert!(Instant::now() < bound);
            std::thread::sleep(Duration::from_millis(1));
        }
        boot_nanos().unwrap()
    } else {
        guard.notify_failure().unwrap();
        base
    };
    let earlier = Start {
        run,
        origin: owner.participant,
        ticket: Ticket::new(now - 100_000_000, CleanupPath::Failed, 1).unwrap(),
    };
    if !after_observation {
        owner.send_start(&earlier).unwrap();
    }
    let observation_bound = clock.observation_bound(earlier.ticket.deadline);
    let mut observation_without_ticket = false;
    let mut committed = None;
    let mut outcome = guard.run(|event, ticket| {
        if let Event::Exited(participant) = event {
            if observed
                .as_ref()
                .is_some_and(|peer| peer.participant == *participant)
            {
                observation_without_ticket = ticket.is_none();
                owner.send_start(&earlier)?;
            }
        }
        if let Event::Start(start) = event {
            if *start == earlier {
                committed = ticket;
                // Release inside the original local window; the missing ACKs
                // may fail publication but cannot renew this earlier ticket.
                clock.advance_to(now + ACK_NANOS + 10_000_000);
                relay.release()?;
                owner.release()?;
            }
        }
        Ok(Vec::new())
    });
    let completed_in_bound = observation_bound.finish();
    relay.release().unwrap();
    owner.release().unwrap();
    let unresolved = outcome.unresolved.len();
    for child in &mut outcome.unresolved {
        while child.collect().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let owner_status = owner.collect_scripted_peer().unwrap();
    let observed_status = observed
        .as_mut()
        .map(|peer| peer.collect_scripted_peer().unwrap());
    assert_eq!(
        committed,
        Some(earlier.ticket),
        "caller did not commit the earlier original deadline"
    );
    assert!(
        completed_in_bound,
        "caller exhausted the external observation bound"
    );
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event, Event::OpportunityExpired(_))),
        "caller consumed a local cleanup opportunity after children were released"
    );
    if after_observation {
        assert!(
            observation_without_ticket,
            "exit observation invented a ticket"
        );
    }
    assert!(outcome.failed);
    assert_eq!(owner_status, 0, "scripted owner peer failed");
    assert!(
        observed_status.is_none_or(|status| status == 0),
        "scripted observation peer failed"
    );
    assert_eq!(unresolved, 0, "production caller omitted relay collection");
    assert!(outcome.events.iter().any(|event| matches!(event,
        Event::Collected(participant, 0) if *participant == relay.participant)));
}

#[test]
fn r2_library_earlier_start_after_local_ticket() {
    earlier_start(false);
}

#[test]
fn r2_library_earlier_start_after_observation() {
    earlier_start(true);
}

#[test]
fn r2_ack_loss_preserves_owner_provider_custody() {
    use umbra_isolation::managed::owner::Owner;
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let local = identity(std::process::id(), Role::CleanupOwner).unwrap();
    let mut silent = Peer::spawn(Role::Guardian);
    let directory = tempfile::Builder::new()
        .prefix("u-owner-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(directory.path().join("s")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_child", "--nocapture"])
        .env("UMBRA_R2_PEER_DIR", directory.path())
        .spawn()
        .unwrap();
    let participant = identity(provider.id(), Role::Provider).unwrap();
    let bound = Instant::now() + Duration::from_secs(2);
    let mut cancellation = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < bound);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("{error}"),
        }
    };
    cancellation.set_nonblocking(false).unwrap();
    cancellation
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut ready = [0];
    cancellation.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    let route = Route::new(silent.participant, silent.channel.take().unwrap()).unwrap();
    let mut owner = Owner::new(local, run, 1, provider, cancellation, vec![route])
        .unwrap_or_else(|(error, ..)| panic!("{error}"));
    let ticket = Ticket::new(boot_nanos().unwrap(), CleanupPath::Trusted, 1).unwrap();
    owner
        .start(
            Start {
                run,
                origin: local,
                ticket,
            },
            local,
        )
        .unwrap();
    let mut held_past_ack = false;
    let result = (|| -> std::io::Result<_> {
        loop {
            if let Some(status) = owner.progress()? {
                return Ok(status);
            }
            let now = boot_nanos()?;
            if !held_past_ack && now >= ticket.started + ACK_NANOS + 10_000_000 {
                if now
                    >= ticket
                        .cutoffs(1)
                        .map_err(std::io::Error::other)?
                        .cooperative
                {
                    return Err(std::io::Error::other("local cleanup window expired"));
                }
                if identity(participant.pid, Role::Provider)? != participant {
                    return Err(std::io::Error::other("provider lost at ACK expiry"));
                }
                held_past_ack = true;
                std::fs::write(directory.path().join("release"), b"release")?;
            }
            if Instant::now() >= bound {
                return Err(std::io::Error::other("test observation bound expired"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    let failed = owner.failed();
    let timed_out = owner
        .failures()
        .iter()
        .any(|failure| failure.contains("timed out"));
    let mut provider = owner.into_child();
    std::fs::write(directory.path().join("release"), b"release").unwrap();
    silent.release().unwrap();
    // ECHILD here distinguishes the owner's real wait from a fabricated exit
    // status. Any wait performed below for test rescue cannot satisfy it.
    let mut raw_status = 0;
    // SAFETY: this is our retained child PID and writable status storage.
    let kernel_wait =
        unsafe { libc::waitpid(participant.pid as i32, &mut raw_status, libc::WNOHANG) };
    let already_collected =
        kernel_wait == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    let _ = provider.wait();
    assert!(held_past_ack, "owner abandoned provider at ACK expiry");
    assert!(
        result.unwrap().success(),
        "provider did not exit successfully"
    );
    assert!(failed && timed_out, "publication failure was erased");
    assert!(
        already_collected,
        "production owner omitted the actual parent wait"
    );
}
