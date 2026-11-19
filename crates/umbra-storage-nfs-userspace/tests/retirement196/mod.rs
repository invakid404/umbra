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
