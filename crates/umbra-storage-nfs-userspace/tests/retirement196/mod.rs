//! #196 tests reuse the public-provider harness; no second server instrument.
use super::*;
use umbra_storage_nfs_userspace::error::ReplayError;
use umbra_storage_nfs_userspace::replay::*;
use umbra_storage_nfs_userspace::transport::WriteVerifier;

#[derive(Default)]
struct Observed {
    records: usize,
    bytes: usize,
    high_records: usize,
    high_bytes: usize,
    retire_calls: usize,
    fail_retire: bool,
    fail_after_remove: bool,
}
struct Watching {
    inner: FakeReplayLog,
    observed: Arc<Mutex<Observed>>,
}
impl Watching {
    fn sample(&self) {
        let p = self.inner.pressure();
        let mut o = self.observed.lock().unwrap();
        o.records = p.records as usize;
        o.bytes = p.payload_bytes as usize;
        o.high_records = o.high_records.max(o.records);
        o.high_bytes = o.high_bytes.max(o.bytes);
    }
}
impl ReplayLog for Watching {
    fn admit(&mut self, i: &ReplayIntent) -> Result<Admission, ReplayError> {
        let r = self.inner.admit(i);
        self.sample();
        r
    }
    fn record(&mut self, k: &IdempotencyKey, o: ReplayOutcome) -> Result<(), ReplayError> {
        self.inner.record(k, o)
    }
    fn get(&self, k: &IdempotencyKey) -> Option<&ReplayRecord> {
        self.inner.get(k)
    }
    fn retire(&mut self, k: &IdempotencyKey) -> Result<(), ReplayError> {
        let mut o = self.observed.lock().unwrap();
        o.retire_calls += 1;
        if o.fail_retire {
            return Err(ReplayError::Indeterminate);
        }
        let fail_after_remove = o.fail_after_remove;
        drop(o);
        let r = self.inner.retire(k);
        self.sample();
        if fail_after_remove {
            return Err(ReplayError::Indeterminate);
        }
        r
    }
    fn pressure(&self) -> Backpressure {
        self.inner.pressure()
    }
    fn note_write_verifier(&mut self, k: &IdempotencyKey, v: WriteVerifier) {
        self.inner.note_write_verifier(k, v)
    }
    fn check_commit_verifier(&self, k: &IdempotencyKey, v: WriteVerifier) -> VerifierMatch {
        self.inner.check_commit_verifier(k, v)
    }
}

#[test]
fn fix_196_retirement_error_after_removal_still_blocks_fresh_intent() {
    let (mut storage, run, observed) = provider();
    observed.lock().unwrap().fail_after_remove = true;
    let request = write(&storage, run, 0, 6);
    let answer = storage.execute(&request).unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
    assert_eq!(storage.execute(&request).unwrap(), answer);
    let before = retry_records(storage.transport().unwrap(), run);
    let error = storage.execute(&write(&storage, run, 1, 6)).unwrap_err();
    assert_eq!(error.operation, "replay-maintenance");
    assert_eq!(retry_records(storage.transport().unwrap(), run), before);
    assert_eq!(observed.lock().unwrap().retire_calls, 3);
    // Maintenance has no outstanding server I/O and cannot hold the writer.
    storage.close_run().unwrap();
}

#[test]
fn fix_196_clean_close_open_has_no_retained_successes() {
    let (mut storage, run, observed) = provider();
    storage.execute(&write(&storage, run, 0, 6)).unwrap();
    storage.close_run().unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
    let next = fresh_run();
    storage.open_run(&create_run(next)).unwrap();
    storage
        .execute(&create_file(authorised(&storage, next, "create"), b"out"))
        .unwrap();
    storage.execute(&write(&storage, next, 0, 6)).unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
    assert_eq!(observed.lock().unwrap().retire_calls, 2);
    storage.close_run().unwrap();
}

#[test]
fn fix_196_old_run_maintenance_precedes_new_run_coincident_key() {
    let (mut storage, run, observed) = provider();
    observed.lock().unwrap().fail_retire = true;
    storage.execute(&write(&storage, run, 0, 6)).unwrap();
    storage.close_run().unwrap();
    let next = fresh_run();
    storage.open_run(&create_run(next)).unwrap();
    let before = retry_records(storage.transport().unwrap(), next);
    assert!(storage.execute(&write(&storage, next, 0, 6)).is_err());
    assert_eq!(retry_records(storage.transport().unwrap(), next), before);
    assert_eq!(observed.lock().unwrap().records, 1);
    observed.lock().unwrap().fail_retire = false;
    storage
        .execute(&create_file(authorised(&storage, next, "create"), b"out"))
        .unwrap();
    storage.execute(&write(&storage, next, 0, 6)).unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
    storage.close_run().unwrap();
}
fn provider() -> (NfsUserspaceStorage, RunId, Arc<Mutex<Observed>>) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let replay = Watching {
        inner: FakeReplayLog::default(),
        observed: observed.clone(),
    };
    let mut storage =
        NfsUserspaceStorage::with_facades(config(), Box::new(fake_server()), Box::new(replay))
            .unwrap();
    let run = fresh_run();
    storage.open_run(&create_run(run)).unwrap();
    storage
        .execute(&create_file(authorised(&storage, run, "create"), b"out"))
        .unwrap();
    (storage, run, observed)
}
fn write(storage: &NfsUserspaceStorage, run: RunId, index: usize, size: usize) -> StorageRequest {
    StorageRequest {
        context: authorised(storage, run, &format!("write-{index}")),
        operation: StorageOperation::WriteAt {
            path: path(b"out"),
            offset: (index * size) as u64,
            bytes: vec![42; size],
        },
    }
}
#[test]
fn fix_196_successful_writes_hold_at_most_one_entry() {
    let (mut storage, run, observed) = provider();
    for i in 0..1000 {
        let request = write(&storage, run, i, 6);
        storage.execute(&request).unwrap();
        assert_eq!(
            observed.lock().unwrap().records,
            0,
            "settled write {i} retained"
        );
    }
    let o = observed.lock().unwrap();
    assert_eq!(o.high_records, 1);
    assert_eq!(o.high_bytes, 6);
    assert_eq!(o.retire_calls, 1000);
}
#[test]
fn fix_196_successful_payloads_release_their_budget() {
    let (mut storage, run, observed) = provider();
    let size = storage.capabilities().max_io_bytes as usize;
    assert!(size > 0);
    for i in 0..(4 * 1024 * 1024 / size + 2) {
        storage.execute(&write(&storage, run, i, size)).unwrap();
        assert_eq!(observed.lock().unwrap().bytes, 0);
    }
    let o = observed.lock().unwrap();
    assert_eq!(o.high_records, 1);
    assert_eq!(o.high_bytes, size);
}
#[test]
fn fix_196_failed_retirement_keeps_success_and_retries_the_same_key() {
    let (mut storage, run, observed) = provider();
    observed.lock().unwrap().fail_retire = true;
    let request = write(&storage, run, 0, 6);
    let answer = storage.execute(&request).unwrap();
    assert_eq!(observed.lock().unwrap().retire_calls, 1);
    assert_eq!(observed.lock().unwrap().records, 1);
    let receipt = storage
        .flush(&umbra_core::FlushRequest {
            context: authorised(&storage, run, "maintenance-flush"),
            scope: umbra_core::FlushScope::EntireRun,
        })
        .unwrap();
    let evidence = String::from_utf8(receipt.evidence).unwrap();
    assert!(evidence.contains("committed_writes=1"), "{evidence}");
    assert!(evidence.contains("committed_bytes=6"), "{evidence}");
    // Maintenance cannot turn the authoritative successful answer into an error.
    assert_eq!(storage.execute(&request).unwrap(), answer);
    assert_eq!(observed.lock().unwrap().retire_calls, 2);
    let fresh = write(&storage, run, 1, 6);
    assert!(
        storage.execute(&fresh).is_err(),
        "fresh admission must wait for maintenance"
    );
    assert_eq!(observed.lock().unwrap().records, 1);
    observed.lock().unwrap().fail_retire = false;
    assert_eq!(storage.execute(&request).unwrap(), answer);
    assert_eq!(observed.lock().unwrap().records, 0);
    storage.execute(&fresh).unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
}

/// Lose a successful retry-record WRITE reply after the server stored it.
/// Decode the exact journal tuple, so payload data cannot select the fault.
struct LoseSettlement {
    inner: FakeTransport,
    armed: Arc<Mutex<bool>>,
    short: bool,
    persist: bool,
}
impl RawTransport for LoseSettlement {
    fn wire_profile(&self) -> WireProfile {
        self.inner.wire_profile()
    }
    fn limits(&self) -> TransportLimits {
        self.inner.limits()
    }
    fn connection(&self) -> ConnectionState {
        self.inner.connection()
    }
    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply> {
        let selected = call.ops.iter().any(|op| {
            let Nfs4Op::Write { data, .. } = op else {
                return false;
            };
            let record = serde_json::from_slice::<(
                StorageRequest,
                Option<umbra_core::Result<umbra_core::StorageResponse>>,
            )>(data);
            matches!(
                record,
                Ok((
                    StorageRequest {
                        operation: StorageOperation::WriteAt { .. },
                        ..
                    },
                    Some(Ok(_))
                ))
            )
        });
        if selected && *self.armed.lock().unwrap() && !self.persist {
            *self.armed.lock().unwrap() = false;
            return Err(
                umbra_storage_nfs_userspace::error::TransportError::Disconnected {
                    epoch: ConnectionEpoch(1),
                    detail: "simulated settlement before persistence".into(),
                },
            );
        }
        let target = call
            .ops
            .iter()
            .any(|op| matches!(op, Nfs4Op::Write { data, .. } if data == &vec![42; 6]));
        self.inner
            .set_write_cap(if self.short && target { Some(3) } else { None });
        let result = self.inner.submit(call, deadline);
        let mut armed = self.armed.lock().unwrap();
        if selected && *armed && result.is_ok() {
            *armed = false;
            return Err(
                umbra_storage_nfs_userspace::error::TransportError::Disconnected {
                    epoch: ConnectionEpoch(1),
                    detail: "simulated settlement reply loss".into(),
                },
            );
        }
        result
    }
    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement> {
        self.inner.cancel(token)
    }
    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.inner.reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.inner.install_faults(plan)
    }
}

#[test]
fn fix_196_interrupted_settlement_retains_bytes_and_characterizes_294() {
    interrupted_settlement(false, true);
}
#[test]
fn fix_196_short_interrupted_settlement_keeps_actual_count() {
    interrupted_settlement(true, true);
}
#[test]
fn fix_196_unpersisted_settlement_requires_recovery_proof() {
    interrupted_settlement(false, false);
}
fn interrupted_settlement(short: bool, persist: bool) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let armed = Arc::new(Mutex::new(false));
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(LoseSettlement {
            inner: fake_server(),
            armed: armed.clone(),
            short,
            persist,
        }),
        Box::new(Watching {
            inner: FakeReplayLog::default(),
            observed: observed.clone(),
        }),
    )
    .unwrap();
    let run = fresh_run();
    storage.open_run(&create_run(run)).unwrap();
    storage
        .execute(&create_file(authorised(&storage, run, "create"), b"out"))
        .unwrap();
    let request = write(&storage, run, 0, 6);
    *armed.lock().unwrap() = true;
    assert_eq!(
        storage.execute(&request).unwrap_err().kind,
        ErrorKind::StorageUnavailable
    );
    assert!(!*armed.lock().unwrap(), "fault must fire");
    assert_eq!(observed.lock().unwrap().records, 1);
    assert_eq!(observed.lock().unwrap().bytes, 6);
    let flush = umbra_core::FlushRequest {
        context: authorised(&storage, run, "flush"),
        scope: umbra_core::FlushScope::EntireRun,
    };
    assert!(storage.flush(&flush).is_err());
    if !persist {
        let error = storage.execute(&request).unwrap_err();
        eprintln!("unpersisted settlement recovery: {error:?}");
        assert_eq!(error.kind, ErrorKind::InvalidState);
        assert!(format!("{error:?}").contains("BLOCKED_RECOVERABLE"));
        assert_eq!(observed.lock().unwrap().records, 1);
        assert_eq!(observed.lock().unwrap().retire_calls, 0);
        assert!(storage.execute(&write(&storage, run, 1, 6)).is_err());
        assert!(storage.close_run().is_err());
        return;
    }
    let count = if short { 3 } else { 6 };
    let answer = storage.execute(&request).unwrap();
    assert_eq!(answer, umbra_core::StorageResponse::WriteAt(count));
    assert_eq!(storage.execute(&request).unwrap(), answer);
    let read = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run, "readback"),
            operation: StorageOperation::ReadAt {
                path: path(b"out"),
                offset: 0,
                len: 6,
            },
        })
        .unwrap();
    assert_eq!(
        read,
        umbra_core::StorageResponse::ReadAt(vec![42; count as usize])
    );
    let stat = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run, "stat"),
            operation: StorageOperation::Stat { path: path(b"out") },
        })
        .unwrap();
    let umbra_core::StorageResponse::Stat(stat) = stat else {
        panic!("stat response");
    };
    for scope in [
        umbra_core::FlushScope::Data {
            objects: vec![stat.object_id],
        },
        umbra_core::FlushScope::DataAndMetadata {
            objects: vec![stat.object_id],
        },
    ] {
        let error = storage
            .flush(&umbra_core::FlushRequest {
                scope,
                ..flush.clone()
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::StorageUnavailable);
    }
    for scope in [
        umbra_core::FlushScope::Data { objects: vec![] },
        umbra_core::FlushScope::DataAndMetadata { objects: vec![] },
    ] {
        let receipt = storage
            .flush(&umbra_core::FlushRequest {
                scope: scope.clone(),
                ..flush.clone()
            })
            .unwrap();
        assert_eq!(receipt.scope, scope);
    }
    assert_eq!(observed.lock().unwrap().retire_calls, 0);
    let receipt = storage.flush(&flush).unwrap();
    let evidence = String::from_utf8(receipt.evidence).unwrap();
    assert!(evidence.contains("committed_writes=0"), "#294: {evidence}");
    assert!(evidence.contains("committed_bytes=0"), "#294: {evidence}");
    assert_eq!(observed.lock().unwrap().records, 1);
    // Repeated recovered successes accumulate; fresh accounted writes retire.
    for index in 1..3 {
        let repeated = write(&storage, run, index, 6);
        *armed.lock().unwrap() = true;
        assert!(storage.execute(&repeated).is_err());
        assert_eq!(storage.execute(&repeated).unwrap(), answer);
        assert_eq!(observed.lock().unwrap().records, index + 1);
    }
    storage
        .execute(&create_file(
            authorised(&storage, run, "create-fresh"),
            b"fresh",
        ))
        .unwrap();
    let mut fresh = write(&storage, run, 3, 6);
    fresh.operation = StorageOperation::WriteAt {
        path: path(b"fresh"),
        offset: 0,
        bytes: vec![42; 6],
    };
    assert_eq!(storage.execute(&fresh).unwrap(), answer);
    assert_eq!(observed.lock().unwrap().records, 3);
    let evidence = String::from_utf8(storage.flush(&flush).unwrap().evidence).unwrap();
    assert!(evidence.contains("committed_writes=1"), "{evidence}");
    assert!(
        evidence.contains(&format!("committed_bytes={count}")),
        "{evidence}"
    );
    assert!(storage
        .flush(&umbra_core::FlushRequest {
            scope: umbra_core::FlushScope::Data {
                objects: vec![stat.object_id]
            },
            ..flush
        })
        .is_err());
    storage.close_run().unwrap();
    assert_eq!(
        observed.lock().unwrap().records,
        3,
        "close is not cache replacement"
    );
}

#[test]
fn fix_196_verifier_change_retains_payload_and_writer() {
    #[derive(Default)]
    struct Rotate(u8);
    impl FaultPlan for Rotate {
        fn decide(
            &mut self,
            point: FaultPoint,
            _: umbra_storage_nfs_userspace::transport::FaultContext,
        ) -> FaultAction {
            if point != FaultPoint::BeforeReturn {
                return FaultAction::Proceed;
            }
            self.0 = self.0.wrapping_add(1);
            FaultAction::RotateVerifier(WriteVerifier([self.0; 8]))
        }
    }
    let shared = SharedFake::new(fake_server());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(shared.clone()),
        Box::new(Watching {
            inner: FakeReplayLog::default(),
            observed: observed.clone(),
        }),
    )
    .unwrap();
    let run = fresh_run();
    storage.open_run(&create_run(run)).unwrap();
    storage
        .execute(&create_file(authorised(&storage, run, "create"), b"out"))
        .unwrap();
    let held = shared.with(|t| marker_bytes(t, run));
    storage
        .transport()
        .unwrap()
        .install_faults(Box::new(Rotate::default()));
    assert!(storage.execute(&write(&storage, run, 0, 6)).is_err());
    let o = observed.lock().unwrap();
    assert_eq!(o.records, 1);
    assert_eq!(o.bytes, 6);
    assert_eq!(o.retire_calls, 0);
    drop(o);
    assert!(storage.close_run().is_err());
    assert_eq!(shared.with(|t| marker_bytes(t, run)), held);
    let mut contender = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(shared.clone()),
        Box::new(FakeReplayLog::default()),
    )
    .unwrap();
    assert!(open_existing(&mut contender, run).is_err());
    assert_eq!(shared.with(|t| marker_bytes(t, run)), held);
    assert_eq!(observed.lock().unwrap().records, 1);
}

#[test]
fn fix_196_retired_write_retry_ignores_changed_target_and_rejects_identity_reuse() {
    let (mut storage, run, observed) = provider();
    let request = write(&storage, run, 0, 6);
    let answer = storage.execute(&request).unwrap();
    assert_eq!(observed.lock().unwrap().records, 0);
    // Replace contents under a different operation identity.
    let mut change = write(&storage, run, 0, 3);
    change.context = authorised(&storage, run, "external-change");
    if let StorageOperation::WriteAt { bytes, .. } = &mut change.operation {
        *bytes = vec![17; 3];
    }
    storage.execute(&change).unwrap();
    let calls = observed.lock().unwrap().retire_calls;
    assert_eq!(storage.execute(&request).unwrap(), answer);
    assert_eq!(observed.lock().unwrap().retire_calls, calls);
    assert_eq!(
        storage
            .execute(&StorageRequest {
                context: authorised(&storage, run, "read-changed"),
                operation: StorageOperation::ReadAt {
                    path: path(b"out"),
                    offset: 0,
                    len: 6
                },
            })
            .unwrap(),
        umbra_core::StorageResponse::ReadAt(vec![17, 17, 17, 42, 42, 42])
    );
    let mut collision = request.clone();
    collision.context.idempotency_key = IdempotencyKey("other-key".into());
    assert!(storage.execute(&collision).is_err());
    let mut changed_request = request.clone();
    if let StorageOperation::WriteAt { offset, .. } = &mut changed_request.operation {
        *offset = 1;
    }
    assert!(storage.execute(&changed_request).is_err());
}

#[test]
fn fix_196_new_provider_resume_preserves_journal_and_epoch_refusals() {
    let shared = SharedFake::new(fake_server());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut first = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(shared.clone()),
        Box::new(Watching {
            inner: FakeReplayLog::default(),
            observed: observed.clone(),
        }),
    )
    .unwrap();
    let run = fresh_run();
    first.open_run(&create_run(run)).unwrap();
    first
        .execute(&create_file(authorised(&first, run, "create"), b"out"))
        .unwrap();
    let old = write(&first, run, 0, 6);
    assert_eq!(
        first.execute(&old).unwrap(),
        umbra_core::StorageResponse::WriteAt(6)
    );
    assert_eq!(observed.lock().unwrap().records, 0);
    first.close_run().unwrap();
    let released = shared.with(|t| marker_bytes(t, run));
    let records = shared.with(|t| retry_records(t, run));
    let next_observed = Arc::new(Mutex::new(Observed::default()));
    let mut next = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(shared.clone()),
        Box::new(Watching {
            inner: FakeReplayLog::default(),
            observed: next_observed.clone(),
        }),
    )
    .unwrap();
    open_existing(&mut next, run).unwrap();
    assert_ne!(shared.with(|t| marker_bytes(t, run)), released);
    assert_eq!(shared.with(|t| retry_records(t, run)), records);
    // A new writer epoch cannot reuse the old request's authority. Changing
    // that epoch also changes the exact durable request: neither is redispatch.
    let old_error = next.execute(&old).unwrap_err();
    eprintln!("resume old epoch: {old_error:?}");
    assert_eq!(old_error.kind, ErrorKind::LeaseLost);
    let mut reauthorized = old.clone();
    reauthorized.context.writer_epoch = authorised(&next, run, "unused").writer_epoch;
    let changed_error = next.execute(&reauthorized).unwrap_err();
    eprintln!("resume changed epoch: {changed_error:?}");
    assert_eq!(changed_error.kind, ErrorKind::InvalidInput);
    assert_eq!(next_observed.lock().unwrap().high_records, 0);
    assert_eq!(
        next.execute(&StorageRequest {
            context: authorised(&next, run, "read"),
            operation: StorageOperation::ReadAt {
                path: path(b"out"),
                offset: 0,
                len: 6
            }
        })
        .unwrap(),
        umbra_core::StorageResponse::ReadAt(vec![42; 6])
    );
    next.close_run().unwrap();
}

#[test]
fn fix_196_absent_retry_discharges_obligation_before_failed_maintenance() {
    let (mut storage, run, observed) = provider();
    observed.lock().unwrap().fail_retire = true;
    let a = write(&storage, run, 0, 6);
    let answer = storage.execute(&a).unwrap();
    let b = write(&storage, run, 1, 6);
    let before = retry_records(storage.transport().unwrap(), run);
    storage
        .transport()
        .unwrap()
        .install_faults(ScriptedFault::once(
            FaultPoint::OnDeadline,
            None,
            FaultAction::DropReply,
        ));
    assert_eq!(
        storage.execute(&b).unwrap_err().kind,
        ErrorKind::StorageUnavailable
    );
    assert_eq!(retry_records(storage.transport().unwrap(), run), before);
    let flush = umbra_core::FlushRequest {
        context: authorised(&storage, run, "cr3-flush"),
        scope: umbra_core::FlushScope::EntireRun,
    };
    assert!(
        storage.flush(&flush).is_err(),
        "B initially remains unresolved"
    );
    assert_eq!(
        storage.execute(&b).unwrap_err().operation,
        "replay-maintenance"
    );
    assert_eq!(
        retry_records(storage.transport().unwrap(), run),
        before,
        "B must remain before intent while A cleanup fails"
    );
    assert_eq!(observed.lock().unwrap().records, 1);
    let receipt = storage
        .flush(&flush)
        .expect("absence resolves B despite A maintenance");
    let evidence = String::from_utf8(receipt.evidence).unwrap();
    assert!(evidence.contains("committed_writes=1"), "{evidence}");
    assert!(evidence.contains("committed_bytes=6"), "{evidence}");
    assert_eq!(storage.execute(&a).unwrap(), answer);
    storage
        .close_run()
        .expect("proven absence must not hold writer release");
}

#[test]
fn fix_196_retirement_diagnostic_failure_preserves_success() {
    // A closed peer gives stderr EPIPE (Rust ignores EBADF on stdio).
    let (stderr, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    let stderr: std::os::fd::OwnedFd = stderr.into();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retirement196::fix_196_failed_retirement_keeps_success_and_retries_the_same_key",
            "--nocapture",
        ])
        .stderr(std::process::Stdio::from(stderr))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "child failed: {:?} {}",
        result.status,
        String::from_utf8_lossy(&result.stdout)
    );
}
