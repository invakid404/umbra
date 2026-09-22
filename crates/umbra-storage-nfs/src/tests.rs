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

// ---- #100: the bounded layout phase -------------------------------------------

const OWNERSHIP_FIDELITY: &str = umbra_core::capabilities::STORAGE_OWNERSHIP_FIDELITY_V1;

// A deliberately small budget for the `bounded_layout` concurrency test. Each test
// owns a fresh `Arc<AtomicUsize>` counter (matching how a real provider owns
// `layout_live`), so these tests need no cross-test serialization.
const TEST_LAYOUT_BUDGET: usize = 3;

fn layout_request(intent: OpenRunIntent) -> OpenRunRequest {
    OpenRunRequest {
        run_id: RunId(Uuid::new_v4()),
        intent,
        immutable_base: ImmutableBaseContract {
            identity: "base".into(),
            fingerprint: vec![1, 2, 3],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        },
    }
}

/// The manifest bytes `nfs_layout_at` writes for a request, recomputed so tests can
/// assert a retry's run holds its own bytes untouched.
fn expected_manifest(request: &OpenRunRequest) -> Vec<u8> {
    serde_json::to_vec(&(
        request.run_id,
        &request.immutable_base,
        request.policy.format_version,
    ))
    .unwrap()
}

/// Run `nfs_layout_at` against a local tempdir `parent`, the way `flush_tests`'
/// `fixture()` exercises `native` without a real NFS mount. Returns the layout result
/// and a fresh thread-owned health (unused by these callers, but the real signature).
fn layout_at(
    parent_dir: &Path,
    config: &NfsStorageConfig,
    request: &OpenRunRequest,
) -> Result<Layout> {
    let parent = File::open(parent_dir).unwrap();
    let mount = parent.try_clone().unwrap();
    let health = native::FlushHealth::default();
    nfs_layout_at(mount, parent, config, request, &health)
}

#[test]
fn bounded_layout_passes_an_in_time_result_through_unchanged() {
    let live = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        bounded_layout(&live, Duration::from_secs(5), || Ok(42u32)).unwrap(),
        42,
        "an in-time Ok passes through"
    );
    let err = bounded_layout(&live, Duration::from_secs(5), || {
        Err::<(), _>(error(ErrorKind::ProtocolMismatch, "open_run", "mismatch"))
    })
    .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::ProtocolMismatch,
        "an in-time Err passes through with its own kind, not remapped"
    );
}

#[test]
fn bounded_layout_times_out_to_storage_unavailable() {
    let live = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let start = std::time::Instant::now();
    let err = bounded_layout(&live, Duration::from_millis(50), move || {
        let _ = release_rx.recv();
        Ok(())
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StorageUnavailable);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "bounded_layout returned on the timeout, not after the layout finished"
    );
    let _ = release_tx.send(()); // release the orphan so it exits cleanly
}

#[test]
fn bounded_layout_maps_a_panicking_layout_to_storage_unavailable() {
    let live = Arc::new(AtomicUsize::new(0));
    let err = bounded_layout(&live, Duration::from_secs(5), || -> Result<()> {
        panic!("layout blew up")
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StorageUnavailable);
}

#[test]
fn bounded_layout_never_exceeds_the_budget_and_spawns_nothing_over_it() {
    let live = Arc::new(AtomicUsize::new(0));
    const N: usize = TEST_LAYOUT_BUDGET + 2;
    let ran = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(std::sync::Barrier::new(TEST_LAYOUT_BUDGET + 1));
    let lineup = Arc::new(std::sync::Barrier::new(N));

    let handles: Vec<_> = (0..N)
        .map(|_| {
            let live = Arc::clone(&live);
            let ran = Arc::clone(&ran);
            let refused = Arc::clone(&refused);
            let gate = Arc::clone(&gate);
            let lineup = Arc::clone(&lineup);
            thread::spawn(move || {
                lineup.wait();
                let result = bounded_layout_with(
                    &live,
                    TEST_LAYOUT_BUDGET,
                    Duration::from_secs(5),
                    move || {
                        ran.fetch_add(1, Ordering::AcqRel);
                        gate.wait();
                        Ok(())
                    },
                );
                if let Err(ref e) = result {
                    assert_eq!(e.kind, ErrorKind::StorageUnavailable);
                    refused.fetch_add(1, Ordering::AcqRel);
                }
                result.is_ok()
            })
        })
        .collect();

    let start = std::time::Instant::now();
    while ran.load(Ordering::Acquire) < TEST_LAYOUT_BUDGET
        || refused.load(Ordering::Acquire) < N - TEST_LAYOUT_BUDGET
    {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "exactly the budget should reserve a slot; the rest are refused"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ran.load(Ordering::Acquire), TEST_LAYOUT_BUDGET);
    assert_eq!(refused.load(Ordering::Acquire), N - TEST_LAYOUT_BUDGET);

    gate.wait();
    let admitted = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(admitted, TEST_LAYOUT_BUDGET);

    let drain = std::time::Instant::now();
    while live.load(Ordering::Acquire) != 0 {
        assert!(
            drain.elapsed() < Duration::from_secs(5),
            "every reserved slot is released after the layouts exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// #100 test 1: a failing layout -- what `bounded_layout` hands back on a timeout,
/// budget exhaustion, spawn failure or panic -- surfaces before the qualification
/// line: `StorageUnavailable`, no run installed, no ownership advertised, and health
/// left clean (h1: a layout failure that never routed through `observe` cannot
/// poison). Drives the seam with a fast-failing layout rather than parking a thread,
/// mirroring how the probe suite keeps parking off the real budget.
#[test]
fn open_run_layout_failure_installs_no_run_and_advertises_nothing() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/tmp/umbra-nfs-test-mount"));
    let request = layout_request(OpenRunIntent::CreateNew);
    let err = storage
        .open_run_with(&request, LAYOUT_TIMEOUT, || -> Result<LayoutOutcome> {
            Err(error(
                ErrorKind::StorageUnavailable,
                "open_run",
                "layout I/O timed out; mount unresponsive",
            ))
        })
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StorageUnavailable);
    assert!(storage.run.is_none(), "a failed layout installs no run");
    assert!(
        !storage.capabilities().features.contains(OWNERSHIP_FIDELITY),
        "a failed layout advertises no ownership fidelity"
    );
    assert!(
        storage.health.check().is_ok(),
        "a layout failure outside observe never poisons health"
    );
}

/// #100 test 2: an orphaned layout that loses the claim race touches nothing. An
/// orphan is what a timed-out `open_run` leaves behind -- a detached thread still
/// running the layout. Model it directly against a tempdir parent, parked *before*
/// its claim `mkdir`. A retry of the same id wins the claim and builds the run
/// intact; releasing the orphan, its first op gets `AlreadyExists` and it stops.
#[test]
fn open_run_orphan_that_loses_the_claim_touches_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let config = NfsStorageConfig::new("/unused");
    let request = layout_request(OpenRunIntent::CreateNew);

    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let orphan = {
        let parent_dir = temp.path().to_path_buf();
        let config = config.clone();
        let request = request.clone();
        thread::spawn(move || {
            let parent = File::open(&parent_dir).unwrap();
            let mount = parent.try_clone().unwrap();
            let health = native::FlushHealth::default();
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            nfs_layout_at(mount, parent, &config, &request, &health)
        })
    };
    started_rx.recv().unwrap(); // parked before its claim mkdir

    // The retry wins the claim and builds the run.
    layout_at(temp.path(), &config, &request).unwrap();

    // Release the orphan: its first op now loses the claim and it stops.
    release_tx.send(()).unwrap();
    assert_eq!(
        orphan.join().unwrap().unwrap_err().kind,
        ErrorKind::AlreadyExists,
        "the orphan that lost the claim stops at its first op"
    );

    // The retry's run is intact: the anchors exist and the manifest holds its bytes.
    let run = temp.path().join(request.run_id.0.to_string());
    assert!(run.join("root").is_dir());
    assert!(run.join("control").is_dir());
    assert_eq!(
        std::fs::read(run.join(".provider/manifest")).unwrap(),
        expected_manifest(&request)
    );
}

/// #100 test 3: a "complete but unclaimed" run. Let an orphaned layout finish
/// completely with no holder. The result is a valid run: a same-id `CreateNew` gets
/// `AlreadyExists`, and `OpenExisting` reopens it. Garbage like crash residue, safe.
#[test]
fn open_run_orphan_may_complete_an_unclaimed_run() {
    let temp = tempfile::tempdir().unwrap();
    let config = NfsStorageConfig::new("/unused");
    let request = layout_request(OpenRunIntent::CreateNew);

    layout_at(temp.path(), &config, &request).unwrap();

    assert_eq!(
        layout_at(temp.path(), &config, &request).unwrap_err().kind,
        ErrorKind::AlreadyExists,
        "a same-id CreateNew over the complete run gets AlreadyExists"
    );
    let mut reopen = request.clone();
    reopen.intent = OpenRunIntent::OpenExisting;
    layout_at(temp.path(), &config, &reopen)
        .expect("OpenExisting reopens the complete-but-unclaimed run");
}

/// #100 test 4: every partial-layout residue an orphan can leave is refused. Each
/// case is built directly on disk, then both an `OpenExisting` (the poison detector)
/// and a same-id `CreateNew` must refuse it.
#[test]
fn open_run_refuses_every_partial_layout_residue() {
    use std::fs;
    let config = NfsStorageConfig::new("/unused");
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Box<dyn Fn(&Path)>)> = vec![
        (
            "dir only",
            Box::new(|run: &Path| {
                fs::create_dir(run).unwrap();
            }),
        ),
        (
            "+root",
            Box::new(|run: &Path| {
                fs::create_dir(run).unwrap();
                fs::create_dir(run.join("root")).unwrap();
            }),
        ),
        (
            "+control",
            Box::new(|run: &Path| {
                fs::create_dir(run).unwrap();
                fs::create_dir(run.join("root")).unwrap();
                fs::create_dir(run.join("control")).unwrap();
            }),
        ),
        (
            ".provider, no manifest",
            Box::new(|run: &Path| {
                fs::create_dir(run).unwrap();
                fs::create_dir(run.join("root")).unwrap();
                fs::create_dir(run.join("control")).unwrap();
                fs::create_dir(run.join(".provider")).unwrap();
            }),
        ),
        (
            "+short manifest",
            Box::new(|run: &Path| {
                fs::create_dir(run).unwrap();
                fs::create_dir(run.join("root")).unwrap();
                fs::create_dir(run.join("control")).unwrap();
                fs::create_dir(run.join(".provider")).unwrap();
                fs::write(run.join(".provider/manifest"), b"short").unwrap();
            }),
        ),
    ];

    for (name, build) in cases {
        let temp = tempfile::tempdir().unwrap();
        let request = layout_request(OpenRunIntent::CreateNew);
        let run = temp.path().join(request.run_id.0.to_string());
        build(&run);

        let mut reopen = request.clone();
        reopen.intent = OpenRunIntent::OpenExisting;
        assert!(
            layout_at(temp.path(), &config, &reopen).is_err(),
            "OpenExisting must refuse partial residue `{name}`"
        );
        assert_eq!(
            layout_at(temp.path(), &config, &request).unwrap_err().kind,
            ErrorKind::AlreadyExists,
            "a same-id CreateNew over residue `{name}` gets AlreadyExists"
        );
    }
}

/// #100 test 5 (h1 witness): a layout timeout does not poison `self.health`. The
/// layout parks past the timeout, then finishes by failing with an Io-kind error into
/// its OWN thread-owned health -- which is discarded, never merged. Afterwards
/// `self.health.check()` is Ok, so a subsequent `open_run` is not refused by it.
#[test]
fn a_layout_timeout_does_not_poison_health() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/tmp/umbra-nfs-test-mount"));
    let request = layout_request(OpenRunIntent::CreateNew);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let start = std::time::Instant::now();
    let err = storage
        .open_run_with(
            &request,
            Duration::from_millis(50),
            move || -> Result<LayoutOutcome> {
                let _ = release_rx.recv(); // park past the timeout
                let health = native::FlushHealth::default();
                let io_err = io("sync_all", std::io::Error::from_raw_os_error(libc::EIO));
                // Poison this thread's OWN health; the main thread already gave up on us.
                let outcome = health.observe::<()>(Err(io_err)).map(|()| unreachable!());
                Ok((true, health, outcome))
            },
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StorageUnavailable);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(
        storage.health.check().is_ok(),
        "a layout timeout must not poison self.health (h1)"
    );
    let _ = release_tx.send(()); // release the orphan so it exits cleanly
}

/// #100 test 6 (in-time equivalence): an in-time layout Io error poisons `self.health`
/// exactly as today's inline `self.health.observe(..)?` would. The returned error
/// keeps the `uncertain` prefix, and the poisoning is sticky -- a following `open_run`
/// is refused by the `health.check()` that stays first on the main thread.
#[test]
fn an_in_time_layout_io_error_poisons_health_exactly_as_before() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/tmp/umbra-nfs-test-mount"));
    let request = layout_request(OpenRunIntent::CreateNew);
    let err = storage
        .open_run_with(&request, LAYOUT_TIMEOUT, || -> Result<LayoutOutcome> {
            let health = native::FlushHealth::default();
            let io_err = io("sync_all", std::io::Error::from_raw_os_error(libc::EIO));
            // Observe the failure into the thread health and propagate its result --
            // exactly what `health.observe(create_file(..))?` does inline today.
            let outcome = health.observe::<()>(Err(io_err)).map(|()| unreachable!());
            Ok((true, health, outcome))
        })
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Io);
    assert!(
        err.context.contains("persistence outcome unknown"),
        "the merged error carries the uncertain prefix"
    );
    // Sticky: a following open_run is refused by health.check() before any layout.
    let second = storage
        .open_run_with(&request, LAYOUT_TIMEOUT, || -> Result<LayoutOutcome> {
            panic!("health.check() must refuse before the layout runs")
        })
        .unwrap_err();
    assert_eq!(second, err);
}
