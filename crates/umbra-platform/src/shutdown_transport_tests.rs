use super::*;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId as HostThread;
use umbra_core::shutdown::{Identity, Proof, ResourceKind};

struct Affine {
    _not_send: Rc<()>,
    ledger: Ledger,
    slot: Option<crate::shutdown::Reservation>,
    log: Arc<Mutex<Vec<(&'static str, HostThread)>>>,
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    envelope: Duration,
}
impl Affine {
    fn record(&self, event: &'static str) {
        self.log
            .lock()
            .unwrap()
            .push((event, std::thread::current().id()));
    }
}
impl Drop for Affine {
    fn drop(&mut self) {
        self.record("drop-entered");
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        self.slot
            .take()
            .unwrap()
            .confirm(Proof::Collected {
                status: "exit 0".into(),
            })
            .unwrap();
        self.record("drop-finished");
        if FAILURE.with(|value| value.get()) == Failure::OwnerPanic {
            panic!("controlled owner destruction panic");
        }
    }
}
impl TraceBackend for Affine {
    fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
        unreachable!()
    }
    fn next_event(&mut self) -> Result<TraceEvent> {
        unreachable!()
    }
    fn poll_event(&mut self, _: Duration) -> Result<crate::EventPoll> {
        unreachable!()
    }
    fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
        unreachable!()
    }
    fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
        unreachable!()
    }
    fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
        unreachable!()
    }
    fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
        unreachable!()
    }
    fn resume(&mut self, _: ResumeCommand) -> Result<()> {
        unreachable!()
    }
}
impl TraceControl for Affine {
    fn capabilities(&self) -> PlatformCapabilities {
        self.record("normal");
        PlatformCapabilities::default()
    }
    fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
        unreachable!()
    }
    fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
        self.record("terminate");
        Ok(())
    }
    fn begin_shutdown(&mut self, nonce: Nonce) -> Result<Start> {
        self.record("begin");
        Ok(Start::Local {
            accepted: self.ledger.freeze(nonce, self.envelope)?,
            ledger: self.ledger.clone(),
        })
    }
}
struct Abi(Arc<Mutex<Vec<(&'static str, HostThread)>>>);
impl Drop for Abi {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap()
            .push(("abi-drop", std::thread::current().id()));
    }
}
impl SyscallAbi for Abi {
    fn decode_entry(&self, _: &RegisterSet, _: &mut dyn TraceMemory) -> Result<Option<FsOp>> {
        unreachable!()
    }
    fn apply_rewrite(&self, _: &mut RegisterSet, _: &PreparedRewrite) -> Result<()> {
        unreachable!()
    }
    fn emulate_result(&self, _: &mut RegisterSet, _: &EmulatedResult) -> Result<()> {
        unreachable!()
    }
}

fn call(connection: &mut Connection, id: u64, request: Request) -> Result<Response> {
    connection.begin();
    connection.send(&Frame::Request {
        id,
        payload: wire::encode(&request)?,
    })?;
    match connection.receive()? {
        Frame::Response { id: actual, result } if actual == id => wire::decode(&result?),
        _ => panic!("unexpected frame"),
    }
}

fn exercise(envelope: Duration, invalid_poll: bool) {
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let owner_log = log.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let owner = std::thread::spawn(move || {
        let ledger = Ledger::new();
        ledger.record_threads(owner_log.clone());
        let root = crate::shutdown::collected_model_root(&ledger);
        let slot = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
        slot.acquired(Identity {
            pid: Some(123),
            birth: None,
        })
        .unwrap();
        let backend = Affine {
            _not_send: Rc::new(()),
            ledger,
            slot: Some(slot),
            log: owner_log.clone(),
            entered: entered_tx,
            release: release_rx,
            envelope,
        };
        backend.record("constructed");
        serve_session(
            Connection::new(right, Duration::from_secs(2)),
            PlatformSession {
                control: Box::new(backend),
                abi: Box::new(Abi(owner_log)),
            },
        )
    });
    let owner_id = owner.thread().id();
    let mut connection = Connection::new(left, Duration::from_secs(2));
    assert!(matches!(
        call(&mut connection, 1, Request::Capabilities).unwrap(),
        Response::Capabilities(_)
    ));
    let nonce = Nonce::new_v4();
    let Response::ShutdownAccepted(accepted) =
        call(&mut connection, 2, Request::BeginShutdown { nonce }).unwrap()
    else {
        panic!("acceptance required")
    };
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let poll = |nonce| Request::PollShutdown {
        nonce,
        generation: accepted.generation,
        max_wait_ms: 1,
    };
    let result = call(
        &mut connection,
        3,
        poll(if invalid_poll { Nonce::new_v4() } else { nonce }),
    );
    if envelope.is_zero() || invalid_poll {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("failed closure returned success"),
        };
        if !invalid_poll {
            assert!(
                error.context.contains("owner-progress failure"),
                "{error:?}"
            );
        }
        release_tx.send(()).unwrap();
        assert!(owner.join().unwrap().is_err());
    } else {
        assert!(matches!(
            result.unwrap(),
            Response::ShutdownPoll(Poll::Pending { .. })
        ));
        assert!(!log
            .lock()
            .unwrap()
            .iter()
            .any(|(event, _)| *event == "drop-finished"));
        release_tx.send(()).unwrap();
        let mut id = 4;
        loop {
            match call(&mut connection, id, poll(nonce)).unwrap() {
                Response::ShutdownPoll(Poll::Pending { .. }) => id += 1,
                Response::ShutdownPoll(Poll::Final(final_state)) => {
                    assert!(accepted.confirms(&final_state).unwrap());
                    assert!(log
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(event, _)| *event == "abi-drop"));
                    break;
                }
                _ => panic!("wrong shutdown phase"),
            }
        }
        owner.join().unwrap().unwrap();
    }
    let events = log.lock().unwrap();
    for (event, thread) in events.iter() {
        if *event == "transport" {
            assert_ne!(*thread, owner_id);
        } else {
            assert_eq!(*thread, owner_id);
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| *event == "transport")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| *event == "drop-finished")
            .count(),
        1
    );
}

#[test]
fn shutdown_thread_affinity() {
    exercise(Duration::from_secs(1), false);
}
#[test]
fn shutdown_drop_latch_envelope_fails_closed() {
    exercise(Duration::ZERO, false);
}
#[test]
fn shutdown_wrong_nonce_still_destroys_on_owner() {
    exercise(Duration::from_secs(1), true);
}

#[derive(Clone, Copy, Debug)]
enum Script {
    Confirmed,
    NonzeroExit,
    MissingFinal,
    StaleId,
    DuplicateAccepted,
    PendingBeforeAccepted,
    WrongNonce,
    PendingSlot,
    /// #205: a pending tracer, an unacquired task right and one poisoned
    /// diagnostic, for the named unconfirmed-obligation message.
    NamedObligations,
}

fn scripted(script: Script) -> Closure {
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut child = std::process::Command::new(if matches!(script, Script::NonzeroExit) {
        "/usr/bin/false"
    } else {
        "/usr/bin/true"
    })
    .spawn()
    .unwrap();
    // The child supplies only the independent owned exit status. It is not the
    // socket peer, so synchronize its completion before the scripted Final;
    // cold executable startup must not masquerade as provider post-Final work.
    let status = child.wait().unwrap();
    assert_eq!(status.success(), !matches!(script, Script::NonzeroExit));
    let mut client = Client::from_owned_connection(
        Connection::new(left, Duration::from_secs(2)),
        wire::Welcome {
            id: "script".into(),
            role: "platform".into(),
            version: wire::PROTOCOL_VERSION,
            capabilities: Default::default(),
        },
        child,
    );
    let peer = std::thread::spawn(move || {
        let mut connection = Connection::new(right, Duration::from_secs(2));
        let Frame::Request { id, payload } = connection.receive().unwrap() else {
            panic!("begin request")
        };
        let Request::BeginShutdown { nonce } = wire::decode(&payload).unwrap() else {
            panic!("begin phase")
        };
        let ledger = Ledger::new();
        let root = crate::shutdown::collected_model_root(&ledger);
        let slot = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
        slot.acquired(Identity {
            pid: Some(123),
            birth: None,
        })
        .unwrap();
        if matches!(script, Script::NamedObligations) {
            ledger
                .reserve(ResourceKind::TaskRight, Some(root), 1, 0)
                .unwrap();
        }
        if !matches!(script, Script::PendingSlot | Script::NamedObligations) {
            slot.confirm(Proof::Collected {
                status: "exit 0".into(),
            })
            .unwrap();
        }
        let accepted = ledger.freeze(nonce, Duration::from_secs(1)).unwrap();
        let first = if matches!(script, Script::PendingBeforeAccepted) {
            Response::ShutdownPoll(Poll::Pending { progress: 0 })
        } else {
            Response::ShutdownAccepted(accepted.clone())
        };
        connection
            .send(&Frame::Response {
                id,
                result: wire::encode(&first),
            })
            .unwrap();
        if matches!(script, Script::PendingBeforeAccepted) {
            return;
        }
        let Frame::Request {
            id: poll_id,
            payload,
        } = connection.receive().unwrap()
        else {
            panic!("poll required after acceptance")
        };
        assert!(poll_id > id);
        assert!(
            matches!(wire::decode::<Request>(&payload).unwrap(), Request::PollShutdown { nonce: actual, generation, .. }
            if actual == nonce && generation == accepted.generation)
        );
        if matches!(script, Script::MissingFinal) {
            return;
        }
        let mut final_state = ledger.final_snapshot().unwrap();
        if matches!(script, Script::WrongNonce) {
            final_state.nonce = Nonce::new_v4();
        }
        if matches!(script, Script::NamedObligations) {
            final_state
                .diagnostics
                .push("poisoned: scripted ownership failure".into());
        }
        let response = if matches!(script, Script::DuplicateAccepted) {
            Response::ShutdownAccepted(accepted)
        } else {
            Response::ShutdownPoll(Poll::Final(final_state))
        };
        connection
            .send(&Frame::Response {
                id: if matches!(script, Script::StaleId) {
                    id
                } else {
                    poll_id
                },
                result: wire::encode(&response),
            })
            .unwrap();
    });
    let closure = close_client(&mut client, Nonce::new_v4());
    peer.join().unwrap();
    match script {
        Script::Confirmed => {
            assert!(closure.error.is_none());
            assert!(closure.tree_terminated);
            assert!(
                !client.owns_provider(),
                "confirmed closure must collect the owned provider"
            );
            assert!(closure.final_to_exit.is_some());
        }
        Script::NonzeroExit => {
            assert!(closure.error.is_some());
            assert!(
                closure.tree_terminated,
                "exit failure cannot erase validated resource proof"
            );
            assert!(!client.owns_provider());
            assert!(closure.final_to_exit.is_some());
        }
        _ => {
            assert!(closure.error.is_some(), "{script:?}");
            assert!(!closure.tree_terminated, "{script:?}");
        }
    }
    closure
}
#[test]
fn shutdown_success_reports_decoded_final_interval() {
    // Advance only the shutdown clock. The independent exit wait uses its own
    // clock, so returning that shorter measurement cannot satisfy this oracle.
    CLOCK.with(|clock| *clock.borrow_mut() = Some(Arc::new(Mutex::new(std::time::Instant::now()))));
    CLOCK_TICK.with(|tick| tick.set(Duration::from_secs(1)));
    let closure = scripted(Script::Confirmed);
    CLOCK.with(|clock| *clock.borrow_mut() = None);
    CLOCK_TICK.with(|tick| tick.set(Duration::ZERO));
    assert_eq!(closure.final_to_exit, Some(Duration::from_secs(1)));
}
#[test]
fn shutdown_owned_provider_exit_is_required() {
    scripted(Script::Confirmed);
}
#[test]
fn shutdown_exit_wait_failure_preserves_proof() {
    scripted(Script::NonzeroExit);
}
#[test]
fn shutdown_stale_phase_and_id() {
    for script in [
        Script::MissingFinal,
        Script::StaleId,
        Script::DuplicateAccepted,
        Script::PendingBeforeAccepted,
        Script::WrongNonce,
        Script::PendingSlot,
    ] {
        scripted(script);
    }
}

#[test]
fn shutdown_affinity_failure_paths() {
    for failure in [
        Failure::Spawn,
        Failure::Handoff,
        Failure::TransportPanic,
        Failure::Acceptance,
        Failure::OwnerPanic,
        Failure::Eof,
    ] {
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let owner_log = log.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            FAILURE.with(|value| value.set(failure));
            let ledger = Ledger::new();
            ledger.record_threads(owner_log.clone());
            let root = crate::shutdown::collected_model_root(&ledger);
            let slot = ledger
                .reserve(ResourceKind::Tracer, Some(root), 1, 0)
                .unwrap();
            slot.acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
            let backend = Affine {
                _not_send: Rc::new(()),
                ledger,
                slot: Some(slot),
                log: owner_log.clone(),
                entered: entered_tx,
                release: release_rx,
                envelope: Duration::from_secs(1),
            };
            backend.record("constructed");
            serve_session(
                Connection::new(right, Duration::from_secs(2)),
                PlatformSession {
                    control: Box::new(backend),
                    abi: Box::new(Abi(owner_log)),
                },
            )
        });
        let owner_id = owner.thread().id();
        let mut connection = Connection::new(left, Duration::from_secs(2));
        let nonce = Nonce::new_v4();
        let acceptance = call(&mut connection, 1, Request::BeginShutdown { nonce });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match failure {
            Failure::OwnerPanic => {
                let Response::ShutdownAccepted(accepted) = acceptance.unwrap() else {
                    panic!("acceptance")
                };
                release_tx.send(()).unwrap();
                let mut id = 2;
                loop {
                    let result = call(
                        &mut connection,
                        id,
                        Request::PollShutdown {
                            nonce,
                            generation: accepted.generation,
                            max_wait_ms: 100,
                        },
                    );
                    match result {
                        Err(_) => break,
                        Ok(Response::ShutdownPoll(Poll::Pending { .. })) => id += 1,
                        _ => panic!("owner panic manufactured final proof"),
                    }
                }
            }
            Failure::Eof => {
                assert!(matches!(acceptance.unwrap(), Response::ShutdownAccepted(_)));
                connection.invalidate();
                release_tx.send(()).unwrap();
            }
            _ => {
                assert!(acceptance.is_err(), "{failure:?}");
                release_tx.send(()).unwrap();
            }
        }
        assert!(owner.join().unwrap().is_err(), "{failure:?}");
        let events = log.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|(event, _)| *event == "drop-entered")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|(event, _)| *event == "abi-drop")
                .count(),
            1
        );
        for (event, thread) in events.iter() {
            if *event != "transport" {
                assert_eq!(*thread, owner_id, "{failure:?}");
            }
        }
    }
}

#[test]
fn shutdown_poll_preserves_deadlines() {
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let start = std::time::Instant::now();
    let clock = Arc::new(Mutex::new(start));
    let owner_clock = clock.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let owner = std::thread::spawn(move || {
        CLOCK.with(|value| *value.borrow_mut() = Some(owner_clock));
        let ledger = Ledger::new();
        let root = crate::shutdown::collected_model_root(&ledger);
        let slot = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
        slot.acquired(Identity {
            pid: Some(123),
            birth: None,
        })
        .unwrap();
        let backend = Affine {
            _not_send: Rc::new(()),
            ledger,
            slot: Some(slot),
            log: Arc::new(Mutex::new(Vec::new())),
            entered: entered_tx,
            release: release_rx,
            envelope: Duration::from_secs(5),
        };
        // Each frame is much shorter than the owner's inherited work schedule.
        serve_session(
            Connection::new(right, Duration::from_secs(1)),
            PlatformSession {
                control: Box::new(backend),
                abi: Box::new(Abi(Arc::new(Mutex::new(Vec::new())))),
            },
        )
    });
    let mut connection = Connection::new(left, Duration::from_secs(1));
    let nonce = Nonce::new_v4();
    let Response::ShutdownAccepted(accepted) =
        call(&mut connection, 1, Request::BeginShutdown { nonce }).unwrap()
    else {
        panic!("acceptance")
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    for id in 2..=4 {
        // Advance beyond one frame, but remain inside the existing owner envelope.
        *clock.lock().unwrap() = start + Duration::from_secs(id - 1);
        assert!(matches!(
            call(
                &mut connection,
                id,
                Request::PollShutdown {
                    nonce,
                    generation: accepted.generation,
                    max_wait_ms: 1,
                }
            )
            .unwrap(),
            Response::ShutdownPoll(Poll::Pending { .. })
        ));
    }
    *clock.lock().unwrap() = start + Duration::from_secs(6);
    let result = call(
        &mut connection,
        5,
        Request::PollShutdown {
            nonce,
            generation: accepted.generation,
            max_wait_ms: 1,
        },
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("poll restarted the exhausted owner envelope"),
    };
    assert!(error.context.contains("owner-progress failure"));
    release_tx.send(()).unwrap();
    assert!(owner.join().unwrap().is_err());
}

/// #205: `close_client`'s unconfirmed-Final error names each unconfirmed
/// entry (kind, pid or `?`, disposition), keeps the diagnostics and the exit
/// result, and does not list a confirmed entry.
#[test]
fn the_close_client_message_names_its_obligations() {
    let (accepted, final_state) = crate::shutdown::scripted_unconfirmed_final();
    assert!(!accepted.confirms(&final_state).unwrap());
    let exit: Result<Duration> = Ok(Duration::from_millis(3));
    let message = super::unconfirmed_final(&final_state, &exit);
    for named in [
        "Tracee pid=4242 Unconfirmed(\"owner destroyed without terminal proof\")",
        "TaskRight pid=? Unconfirmed(\"owner destroyed without terminal proof\")",
        "poisoned: scripted ownership failure",
        "exit: Ok(3ms)",
    ] {
        assert!(message.contains(named), "{named} missing from: {message}");
    }
    assert!(!message.contains("Tracer pid=5151"), "{message}");
}

/// #205: `close_client` itself, driven through a scripted provider, names each
/// unconfirmed obligation at its call site (kind, pid or `?`, disposition),
/// keeps the diagnostics and the exit result, and does not list the confirmed
/// root.
#[test]
fn close_client_names_its_unconfirmed_obligations() {
    let closure = scripted(Script::NamedObligations);
    let message = closure.error.expect("unconfirmed closure").to_string();
    for named in [
        "Tracer pid=123 Unconfirmed(\"owner destroyed without terminal proof\")",
        "TaskRight pid=? Unconfirmed(\"owner destroyed without terminal proof\")",
        "poisoned: scripted ownership failure",
        "exit: Ok(",
    ] {
        assert!(message.contains(named), "{named} missing from: {message}");
    }
    assert!(!message.contains("Tracee pid=123"), "{message}");
}

#[path = "cleanup_owner_tests.rs"]
mod cleanup_owner_tests;
