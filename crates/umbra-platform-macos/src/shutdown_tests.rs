use super::*;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

fn shared_reason(reason: TerminalCause) -> UmbraError {
    let tasks = Arc::new(Mutex::new(WatchedTasks::default()));
    tasks.lock().unwrap().latch(reason);
    let mut backend = MacosTraceBackend::default();
    backend.owner = Some(Watchdog {
        tasks: tasks.clone(),
        cancel: None,
        worker: None,
    });
    backend.watchdog = Some(Watchdog {
        tasks,
        cancel: None,
        worker: None,
    });
    backend.check_deadline().unwrap_err()
}

#[test]
fn timeout_attribution() {
    let error = shared_reason(TerminalCause::Deadline);
    assert_eq!(error.kind, ErrorKind::Io);
    assert_eq!(error.operation, "watchdog");
}

#[test]
fn owner_causes_keep_their_attribution() {
    for reason in [
        TerminalCause::OwnerGone,
        TerminalCause::MonitorFailed,
        TerminalCause::MonitorPanicked,
    ] {
        let error = shared_reason(reason);
        assert_eq!(error.kind, ErrorKind::Io);
        assert_eq!(error.operation, "owner-observation");
    }
}

// Exercises the real latch and destructor without issuing a Mach call. Channel
// rendezvous models a kernel call that completes only after its owner releases it.
// This proves ordering, not a numeric upper bound on kernel completion.
#[test]
fn drop_during_latch_joins_after_release() {
    struct HeldKill {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        returned: Arc<AtomicBool>,
    }
    impl KillOwned for HeldKill {
        fn kill_owned(&self) {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            self.returned.store(true, Ordering::SeqCst);
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let returned = Arc::new(AtomicBool::new(false));
    let task = HeldKill {
        entered: entered_tx,
        release: release_rx,
        returned: returned.clone(),
    };
    let (mut cancel_reader, cancel_writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let mut registry = WatchedTasks::default();
        registry.add(task);
        registry.latch(TerminalCause::Deadline);
    });
    entered_rx.recv().unwrap();
    let returned_control = returned.clone();
    let releaser = std::thread::spawn(move || {
        // The destructor's cancel byte establishes that Drop has begun while
        // the latch is still held. No scheduler sleeps are used.
        let mut byte = [0];
        cancel_reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
        assert!(!returned_control.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
    });
    let watchdog = Watchdog {
        tasks: Arc::new(Mutex::new(WatchedTasks::default())),
        cancel: Some(cancel_writer),
        worker: Some(worker),
    };
    drop(watchdog);
    assert!(returned.load(Ordering::SeqCst), "Drop must join the latch");
    releaser.join().unwrap();
}

#[test]
fn freeze_late_result() {
    use umbra_core::shutdown::{Identity, Nonce, Proof, ResourceKind};
    let ledger = umbra_platform::shutdown::Ledger::new();
    let slot = ledger.reserve(ResourceKind::Tracer, None, 1, 0).unwrap();
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        release_rx.recv().unwrap();
        slot.acquired(Identity {
            pid: Some(123),
            birth: None,
        })
        .unwrap();
        slot.confirm(Proof::Collected {
            status: "exit 0".into(),
        })
        .unwrap();
    });
    let accepted = ledger
        .freeze(Nonce::new_v4(), Duration::from_secs(1))
        .unwrap();
    assert!(!accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap());
    assert!(ledger.reserve(ResourceKind::Tracer, None, 1, 1).is_err());
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    assert!(accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap());
}

#[test]
fn replacement_keeps_old_tracer_collection_obligation() {
    use umbra_core::shutdown::{Identity, Nonce, ResourceKind};
    let ledger = umbra_platform::shutdown::Ledger::new();
    let (mut old, _old_peer) = Rsp::retirement_test_pair();
    let (mut replacement, _replacement_peer) = Rsp::retirement_test_pair();
    for (generation, tracer) in [(0, &mut old), (1, &mut replacement)] {
        let slot = ledger
            .reserve(ResourceKind::Tracer, None, 1, generation)
            .unwrap();
        slot.acquired(Identity {
            pid: Some(tracer.tracer_pid()),
            birth: None,
        })
        .unwrap();
        tracer.set_test_ownership(slot);
    }
    assert!(replacement.reap_tracer(Duration::ZERO));
    let accepted = ledger
        .freeze(Nonce::new_v4(), Duration::from_secs(1))
        .unwrap();
    assert_eq!(accepted.slots.len(), 2);
    assert!(
        !accepted
            .confirms(&ledger.final_snapshot().unwrap())
            .unwrap(),
        "replacement must not erase old tracer receipt"
    );
    assert!(old.reap_tracer(Duration::ZERO));
    assert!(accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap());
}
