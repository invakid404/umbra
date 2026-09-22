use super::*;

#[test]
fn unsupported_flush_scope_is_rejected_without_mount_io() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/nonexistent-nfs-mount"));
    let result = storage.flush(&FlushRequest {
        context: RequestContext {
            run_id: RunId(Uuid::new_v4()),
            writer_epoch: Some(LeaseEpoch(1)),
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey("flush".into()),
        },
        scope: FlushScope::Data { objects: vec![] },
    });
    assert_eq!(result.unwrap_err().kind, ErrorKind::UnsupportedCapability);
    assert!(storage.health.check().is_ok());
}

#[test]
fn known_failure_blocks_close_and_reopen_without_erasing_evidence() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/nonexistent-nfs-mount"));
    let failure = storage.health.fail(io(
        "sync_all",
        std::io::Error::from_raw_os_error(libc::ECONNRESET),
    ));
    assert_eq!(storage.close_run().unwrap_err(), failure);
    let request = OpenRunRequest {
        run_id: RunId(Uuid::new_v4()),
        intent: OpenRunIntent::OpenExisting,
        immutable_base: ImmutableBaseContract {
            identity: "base".into(),
            fingerprint: vec![],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        },
    };
    assert_eq!(storage.open_run(&request).unwrap_err(), failure);
    assert_eq!(storage.capabilities().durability, Durability::Local);
}

// A private live-probe-thread counter for the `bounded` unit tests. Reserving slots
// here to exercise the budget never touches the process-global counter the
// `open_run` tests drive, so the two cannot refuse each other under `cargo test`'s
// parallelism.
static TEST_LIVE: AtomicUsize = AtomicUsize::new(0);

// A deliberately small budget for the concurrency test, so it can oversubscribe it
// with only a handful of threads.
const TEST_BUDGET: usize = 3;

/// Shadows the crate's [`bounded`] within this module so every `bounded` unit test
/// runs against `TEST_LIVE`/`TEST_BUDGET` rather than the real probe counter.
fn bounded(timeout: Duration, probe: impl FnOnce() -> bool + Send + 'static) -> bool {
    bounded_with(&TEST_LIVE, TEST_BUDGET, timeout, probe)
}

/// Serializes the tests that exercise `bounded`. `TEST_LIVE` is a single `static`,
/// so these tests must neither run concurrently nor start while a previous test's
/// just-released probe thread is still draining. Holding this lock excludes the
/// others; waiting for the counter to reach zero gives each test an empty budget to
/// start from.
fn serialize_bounded_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let start = std::time::Instant::now();
    while TEST_LIVE.load(Ordering::Acquire) != 0 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the live-probe counter did not return to zero between bounded tests"
        );
        thread::sleep(Duration::from_millis(5));
    }
    guard
}

#[test]
fn bounded_returns_the_probe_verdict_when_it_finishes_in_time() {
    let _serial = serialize_bounded_tests();
    assert!(
        bounded(Duration::from_secs(5), || true),
        "a true verdict passes through"
    );
    assert!(
        !bounded(Duration::from_secs(5), || false),
        "a false verdict passes through"
    );
}

#[test]
fn bounded_answers_false_when_the_probe_outlives_the_timeout() {
    let _serial = serialize_bounded_tests();
    // The probe parks on a channel whose sender the test keeps alive, so it is
    // still running when the timeout fires. Milliseconds, not seconds, keep the
    // test fast; the elapsed bound asserts `bounded` did not wait for the probe.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let start = std::time::Instant::now();
    let answer = bounded(Duration::from_millis(50), move || {
        let _ = release_rx.recv();
        true
    });
    assert!(
        !answer,
        "a probe still running at the timeout answers false"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "bounded returned on the timeout, not after the probe finished"
    );
    // Release the orphaned thread so it exits cleanly instead of leaking.
    let _ = release_tx.send(());
}

#[test]
fn bounded_answers_false_when_the_probe_panics() {
    let _serial = serialize_bounded_tests();
    assert!(
        !bounded(Duration::from_secs(5), || panic!("probe blew up")),
        "a panicking probe drops the sender and answers false"
    );
}

#[test]
fn bounded_never_exceeds_the_budget_under_concurrent_callers() {
    let _serial = serialize_bounded_tests();

    // Oversubscribe the budget: launch budget + 2 callers that all reserve before
    // any can time out (a long timeout, and probes that park until the test releases
    // them). The atomic reservation must admit exactly `budget` and refuse the rest
    // without running their closures.
    const N: usize = TEST_BUDGET + 2;
    let ran = std::sync::Arc::new(AtomicUsize::new(0));
    let refused = std::sync::Arc::new(AtomicUsize::new(0));
    // The admitted probes and this test thread meet here to release together.
    let gate = std::sync::Arc::new(std::sync::Barrier::new(TEST_BUDGET + 1));
    // All callers line up here so their reservations race at once.
    let lineup = std::sync::Arc::new(std::sync::Barrier::new(N));

    let handles: Vec<_> = (0..N)
        .map(|_| {
            let ran = std::sync::Arc::clone(&ran);
            let refused = std::sync::Arc::clone(&refused);
            let gate = std::sync::Arc::clone(&gate);
            let lineup = std::sync::Arc::clone(&lineup);
            thread::spawn(move || {
                lineup.wait();
                let answer = bounded(Duration::from_secs(5), move || {
                    ran.fetch_add(1, Ordering::AcqRel);
                    gate.wait(); // hold the slot until the test releases us
                    true
                });
                if !answer {
                    refused.fetch_add(1, Ordering::AcqRel);
                }
                answer
            })
        })
        .collect();

    // Wait until every caller has resolved: exactly `budget` are parked in their
    // probe and the other two were refused without running.
    let start = std::time::Instant::now();
    while ran.load(Ordering::Acquire) < TEST_BUDGET
        || refused.load(Ordering::Acquire) < N - TEST_BUDGET
    {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "exactly the budget should reserve a slot; the rest are refused"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        ran.load(Ordering::Acquire),
        TEST_BUDGET,
        "exactly the budget ran their closure"
    );
    assert_eq!(
        refused.load(Ordering::Acquire),
        N - TEST_BUDGET,
        "the over-budget callers answered false without running"
    );

    // Release the parked probes; their threads exit and free their slots.
    gate.wait();
    let answers: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        answers.iter().filter(|&&a| a).count(),
        TEST_BUDGET,
        "the admitted callers saw their probe's verdict"
    );

    // The counter returns to zero once every probe thread has exited.
    let drain = std::time::Instant::now();
    while TEST_LIVE.load(Ordering::Acquire) != 0 {
        assert!(
            drain.elapsed() < Duration::from_secs(5),
            "every reserved slot is released after the probes exit"
        );
        thread::sleep(Duration::from_millis(5));
    }

    // After release, a fresh probe runs again now that the budget has freed up.
    assert!(
        bounded(Duration::from_secs(5), || true),
        "a new probe runs once the budget frees up"
    );
}
