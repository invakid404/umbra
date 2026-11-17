use super::*;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

fn shared_reason(reason: &'static str) -> UmbraError {
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
    let error = shared_reason("session timed out");
    assert_eq!(error.kind, ErrorKind::Io);
    assert_eq!(error.operation, "watchdog");
}

#[test]
fn owner_causes_keep_their_attribution() {
    for reason in [
        "supervisor exited",
        "owner monitor failed",
        "owner monitor panicked",
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
        registry.latch("session timed out");
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
