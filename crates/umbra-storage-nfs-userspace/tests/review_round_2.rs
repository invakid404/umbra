//! Fixes for the round-2 independent review, driven through the public contract.
//!
//! Every case names the finding it covers (`R2-0NN`) and states in its own doc
//! comment what the round-2 candidate did, so a later round can trace fix to
//! finding without reading the diff.
//!
//! Round 2's recurring complaint is that a returned error is not evidence of an
//! absent effect. These tests therefore assert on the *server*: which operations
//! reached the wire, and what `.provider/retries` holds afterwards.
//!
//! **This suite mounts nothing.** No `~/umbra-scratch`, no `nfs_fixture_matrix`,
//! no OS-visible NFS mount, no live Ganesha.

use std::sync::{Arc, Mutex};

use umbra_core::{
    BytePath, CreateKind, CreateOptions, ErrorKind, IdempotencyKey, ImmutableBaseContract,
    OpenRunIntent, OpenRunRequest, OperationId, RequestContext, RunId, StorageAnchor,
    StorageOperation, StoragePath, StoragePolicy, StorageRequest,
};
use umbra_storage::Storage;
use uuid::Uuid;

use umbra_storage_nfs_userspace::error::Nfs4Status;
use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport, ScriptedFault};
use umbra_storage_nfs_userspace::handle::{FileHandle, Stateid};
use umbra_storage_nfs_userspace::layout;
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, CallToken, ComponentName, Compound, CompoundReply, ConnectionEpoch, ConnectionState,
    Deadline, FaultAction, FaultPlan, FaultPoint, Nfs4Op, OpCode, RawTransport, Retirement,
    TransportLimits, TransportResult, WireProfile,
};

const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

fn deadline() -> Deadline {
    Deadline { millis: 5_000 }
}

fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        // The reuse key `m1-fixer` owns 127.0.0.1:12112 and no other port. The
        // fake never opens a socket; this records the identity, it does not use it.
        port: 12112,
        export: BytePath::new(EXPORT).expect("export path"),
        run_parent: BytePath::new(RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: deadline(),
    }
}

fn fake_server() -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    fake.insert_directory(&current, RUN_PARENT);
    fake
}

fn base() -> ImmutableBaseContract {
    ImmutableBaseContract {
        identity: "umbra-review-base".into(),
        fingerprint: vec![0xDE, 0xAD, 0xBE, 0xEF],
    }
}

fn policy() -> StoragePolicy {
    StoragePolicy {
        read_only: false,
        require_strict_remote_persistence: false,
        require_kernel_shadow: false,
        format_version: FORMAT_VERSION,
    }
}

fn create_run(run_id: RunId) -> OpenRunRequest {
    OpenRunRequest {
        run_id,
        intent: OpenRunIntent::CreateNew,
        immutable_base: base(),
        policy: policy(),
    }
}

fn fresh_run() -> RunId {
    RunId(Uuid::new_v4())
}

fn path(bytes: &[u8]) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec()).expect("a valid contract path")
}

fn name(bytes: &[u8]) -> ComponentName {
    ComponentName::new(bytes.to_vec()).expect("a valid component")
}

/// A context naming the run and epoch the provider actually holds.
fn authorised(storage: &NfsUserspaceStorage, run_id: RunId, key: &str) -> RequestContext {
    let epoch = storage
        .admission()
        .expect("a run is open")
        .admitted()
        .epoch();
    RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: Some(epoch),
    }
}

fn create_file(context: RequestContext, file: &[u8]) -> StorageRequest {
    StorageRequest {
        context,
        operation: StorageOperation::Create {
            path: path(file),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o644,
            },
        },
    }
}

// --- server-side observation -------------------------------------------------

/// Counts of the operations that actually reached the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Wire {
    /// Operations that create or change server state.
    modifying: usize,
    /// Every opcode seen, in order, for diagnostics.
    ops: Vec<OpCode>,
}

impl Wire {
    fn note(&mut self, call: &Compound) {
        for op in &call.ops {
            let opcode = op.opcode();
            self.ops.push(opcode);
            let modifies = match op {
                // An OPEN only modifies when it is allowed to create.
                Nfs4Op::Open(args) => !matches!(
                    args.how,
                    umbra_storage_nfs_userspace::transport::OpenHow::NoCreate
                ),
                Nfs4Op::Write { .. }
                | Nfs4Op::Create { .. }
                | Nfs4Op::Remove { .. }
                | Nfs4Op::Rename { .. }
                | Nfs4Op::SetAttr { .. } => true,
                _ => false,
            };
            if modifies {
                self.modifying += 1;
            }
        }
    }
}

/// A transport that records what was dispatched through it.
///
/// Round 2's finding is that a refusal returned to the caller says nothing about
/// what already reached the server. This is how the tests below say something
/// about it.
struct Recording {
    inner: FakeTransport,
    wire: Arc<Mutex<Wire>>,
}

impl Recording {
    fn new(inner: FakeTransport) -> (Self, Arc<Mutex<Wire>>) {
        let wire = Arc::new(Mutex::new(Wire::default()));
        (
            Self {
                inner,
                wire: Arc::clone(&wire),
            },
            wire,
        )
    }
}

impl RawTransport for Recording {
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
        self.wire.lock().expect("not poisoned").note(&call);
        self.inner.submit(call, deadline)
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

fn provider() -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(
        config(),
        Box::new(fake_server()),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider")
}

fn recording_provider() -> (NfsUserspaceStorage, Arc<Mutex<Wire>>) {
    let (transport, wire) = Recording::new(fake_server());
    let storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(transport),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    (storage, wire)
}

/// Walk to a directory inside the open run.
fn walk(transport: &mut dyn RawTransport, run_id: RunId, parts: &[&[u8]]) -> FileHandle {
    let mut cursor = transport.root_filehandle(deadline()).expect("root");
    let run = run_id.0.hyphenated().to_string();
    let mut components: Vec<&[u8]> = EXPORT.split(|byte| *byte == b'/').collect();
    components.push(RUN_PARENT);
    components.push(run.as_bytes());
    components.extend_from_slice(parts);
    for part in components {
        cursor = transport
            .lookup(&cursor, &name(part), AttrMask::STAT, deadline())
            .unwrap_or_else(|error| panic!("walk {:?}: {error:?}", String::from_utf8_lossy(part)))
            .0;
    }
    cursor
}

/// Names currently held by the run's `.provider/retries` directory.
fn retry_records(transport: &mut dyn RawTransport, run_id: RunId) -> Vec<String> {
    let retries = walk(
        transport,
        run_id,
        &[layout::PRIVATE_DIR, layout::RETRIES_DIR],
    );
    let mut out = Vec::new();
    let mut cursor = umbra_storage_nfs_userspace::transport::DirCookie(0);
    let mut verifier = umbra_storage_nfs_userspace::transport::DirVerifier([0; 8]);
    loop {
        let page = transport
            .readdir(
                &retries,
                umbra_storage_nfs_userspace::transport::ReadDirRequest {
                    cookie: cursor,
                    verifier,
                    dir_count: 4096,
                    max_count: 4096,
                    attrs: AttrMask::STAT,
                },
                deadline(),
            )
            .expect("readdir the retries directory");
        verifier = page.verifier;
        for entry in &page.entries {
            out.push(String::from_utf8_lossy(entry.name.as_bytes()).into_owned());
            cursor = entry.cookie;
        }
        if page.eof || page.entries.is_empty() {
            break;
        }
    }
    out.sort();
    out
}

/// The raw bytes of the run's admission marker.
fn marker_bytes(transport: &mut dyn RawTransport, run_id: RunId) -> Vec<u8> {
    let marker = walk(
        transport,
        run_id,
        &[layout::PRIVATE_DIR, layout::WRITER_LOCK_FILE],
    );
    transport
        .read(&marker, Stateid::ANONYMOUS, 0, 4096, deadline())
        .expect("read the marker")
        .data
}

/// Fail the next marker round trip, which is what latches authority loss.
fn break_renewal(storage: &mut NfsUserspaceStorage) {
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            None,
            FaultAction::Substitute(Nfs4Status::IO),
        ));
}

// --- R2-001: the authority latch covers the journal and the release ----------

/// **R2-001.** After authority is lost, an exact-key retry must not be answered
/// from the journal's cached record.
///
/// At the round-2 candidate, `execute` consulted the journal *before* the latch:
/// `check_presented_epoch` looked only at the still-present session's epoch, the
/// journal returned key K's recorded success, and the caller was handed a success
/// by a provider that could no longer prove it held the run. The latch was reached
/// only later, inside `request`.
#[test]
fn r2_001_a_cached_record_is_not_returned_after_authority_is_lost() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let lease = storage.admission().expect("admitted").lease();

    let context = authorised(&storage, run_id, "cached-key");
    let request = create_file(context, b"cached.txt");
    storage
        .execute(&request)
        .expect("the first attempt succeeds");

    break_renewal(&mut storage);
    storage
        .renew_writer(&lease)
        .expect_err("the renewal cannot re-prove the marker");

    // The identical key again. A cached success here would be a success reported
    // by a provider with no authority.
    let refused = storage
        .execute(&request)
        .expect_err("a lost-authority provider must not answer from its journal");
    assert_eq!(refused.kind, ErrorKind::LeaseLost);
    assert!(
        refused.context.contains("lost writer authority"),
        "the refusal must name the latched loss, not the record: {}",
        refused.context
    );
}

/// **R2-001.** After authority is lost, a fresh key must leave no durable records.
///
/// At the candidate the provider performed `GUARDED4` creates and `FILE_SYNC4`
/// writes of the `op-` index and `key-` intent before `request` finally returned
/// `LeaseLost`, so a run kept accumulating records written by a session that held
/// nothing.
#[test]
fn r2_001_a_lost_provider_writes_no_journal_records_for_a_fresh_key() {
    let (mut storage, wire) = recording_provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let lease = storage.admission().expect("admitted").lease();

    break_renewal(&mut storage);
    storage.renew_writer(&lease).expect_err("renewal fails");

    let before = retry_records(storage.transport().expect("transport"), run_id);
    let modifying_before = wire.lock().expect("not poisoned").modifying;

    let refused = storage
        .execute(&create_file(
            RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("fresh-after-loss".into()),
                writer_epoch: Some(umbra_core::LeaseEpoch(1)),
            },
            b"fresh.txt",
        ))
        .expect_err("a lost-authority provider must not mutate");
    assert_eq!(refused.kind, ErrorKind::LeaseLost);

    let after = retry_records(storage.transport().expect("transport"), run_id);
    assert_eq!(
        before, after,
        "no index or intent record may be written after the latch"
    );
    assert_eq!(
        wire.lock().expect("not poisoned").modifying,
        modifying_before,
        "no modifying operation may reach the wire after the latch; saw {:?}",
        wire.lock().expect("not poisoned").ops
    );
}

/// **R2-001.** A close after a latched loss publishes no release.
///
/// At the candidate `close_run` called `release` whenever a session existed, and
/// neither that helper nor `AdmissionControl::release` consulted the latch, so a
/// provider that had just failed to re-prove ownership would overwrite the marker
/// with `Released` at its own stale epoch — destroying whatever the record had
/// become.
#[test]
fn r2_001_close_after_a_latched_loss_leaves_the_marker_untouched() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let lease = storage.admission().expect("admitted").lease();

    let before = marker_bytes(storage.transport().expect("transport"), run_id);

    break_renewal(&mut storage);
    storage.renew_writer(&lease).expect_err("renewal fails");

    let closed = storage
        .close_run()
        .expect_err("a close that published no release is not a clean handover");
    assert_eq!(closed.kind, ErrorKind::LeaseLost);

    let after = marker_bytes(storage.transport().expect("transport"), run_id);
    assert_eq!(
        before, after,
        "the marker must be byte-identical: a surrendered session publishes nothing"
    );
    assert!(
        storage.admission().is_none(),
        "the proof is consumed, not retained"
    );
}

// --- R2-002: validation happens before any durable side effect ---------------

/// Run `body` on a fresh run and assert it left the journal and the wire alone.
fn assert_no_durable_effect(label: &str, body: impl FnOnce(&mut NfsUserspaceStorage, RunId)) {
    let (mut storage, wire) = recording_provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let before = retry_records(storage.transport().expect("transport"), run_id);
    let modifying_before = wire.lock().expect("not poisoned").modifying;

    body(&mut storage, run_id);

    let after = retry_records(storage.transport().expect("transport"), run_id);
    assert_eq!(
        before, after,
        "{label}: a refused request must leave no retry record behind"
    );
    assert_eq!(
        wire.lock().expect("not poisoned").modifying,
        modifying_before,
        "{label}: a refused request must send no modifying operation; saw {:?}",
        wire.lock().expect("not poisoned").ops
    );
}

/// **R2-002.** A request naming another run is refused before the journal runs.
///
/// At the candidate, the journal wrote run B's request into run A's
/// `.provider/retries` and consumed its operation and key names; only afterwards
/// did `Operations::execute` reject it, and the journal then persisted that
/// rejection as a settled record.
#[test]
fn r2_002_a_wrong_run_request_leaves_no_durable_trace() {
    assert_no_durable_effect("wrong run", |storage, run_id| {
        let mut context = authorised(storage, run_id, "wrong-run");
        context.run_id = RunId(Uuid::nil());
        let refused = storage
            .execute(&create_file(context, b"wrong-run.txt"))
            .expect_err("a request naming another run is refused");
        assert_eq!(refused.kind, ErrorKind::InvalidInput);
    });
}

/// **R2-002.** A mutation on a read-only run is refused before the journal runs.
#[test]
fn r2_002_a_read_only_mutation_leaves_no_durable_trace() {
    let (mut storage, wire) = recording_provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("create");
    storage.close_run().expect("release");
    storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::OpenExisting,
            immutable_base: base(),
            policy: StoragePolicy {
                read_only: true,
                ..policy()
            },
        })
        .expect("reopen read-only");

    let before = retry_records(storage.transport().expect("transport"), run_id);
    let modifying_before = wire.lock().expect("not poisoned").modifying;

    let refused = storage
        .execute(&create_file(
            authorised(&storage, run_id, "read-only"),
            b"nope.txt",
        ))
        .expect_err("a read-only run refuses mutations");
    assert_eq!(refused.kind, ErrorKind::Denied);

    assert_eq!(
        retry_records(storage.transport().expect("transport"), run_id),
        before,
        "a read-only refusal must leave no retry record behind"
    );
    assert_eq!(
        wire.lock().expect("not poisoned").modifying,
        modifying_before,
        "a read-only refusal must send no modifying operation; saw {:?}",
        wire.lock().expect("not poisoned").ops
    );
}

/// **R2-002.** An unsupported operation is refused before the journal runs.
#[test]
fn r2_002_an_unsupported_operation_leaves_no_durable_trace() {
    assert_no_durable_effect("unsupported", |storage, run_id| {
        let refused = storage
            .execute(&StorageRequest {
                context: authorised(storage, run_id, "unsupported"),
                operation: StorageOperation::Create {
                    path: path(b"link"),
                    options: CreateOptions {
                        kind: CreateKind::LogicalSymlink {
                            target: BytePath::new(b"elsewhere").expect("target"),
                        },
                        mode: 0o777,
                    },
                },
            })
            .expect_err("logical symlinks are unsupported");
        assert_eq!(refused.kind, ErrorKind::UnsupportedCapability);
    });
}

/// **R2-002.** An oversized write is refused before the journal runs.
///
/// This one matters most: the journal encodes the whole payload into its record,
/// so writing the intent first meant a request the provider was about to reject
/// for exceeding `max_io_bytes` was durably persisted at full size beforehand.
#[test]
fn r2_002_an_oversized_write_leaves_no_durable_trace() {
    assert_no_durable_effect("oversized", |storage, run_id| {
        let capabilities = storage
            .operations()
            .expect("a run is open")
            .capabilities()
            .max_io_bytes as usize;
        let refused = storage
            .execute(&StorageRequest {
                context: authorised(storage, run_id, "oversized"),
                operation: StorageOperation::WriteAt {
                    path: path(b"big.bin"),
                    offset: 0,
                    bytes: vec![0u8; capabilities + 1],
                },
            })
            .expect_err("a write past max_io_bytes is refused");
        assert_eq!(refused.kind, ErrorKind::InvalidInput);
    });
}

/// Control for the four assertions above: the observer does detect records and
/// modifying operations when a request is *accepted*.
///
/// Without this, `r2_002_*` could pass because `retry_records` always returned an
/// empty list or `Wire` never counted anything.
#[test]
fn r2_002_the_effect_observer_is_not_vacuous() {
    let (mut storage, wire) = recording_provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let before = retry_records(storage.transport().expect("transport"), run_id);
    assert!(before.is_empty(), "a fresh run has no retry records");
    let modifying_before = wire.lock().expect("not poisoned").modifying;

    storage
        .execute(&create_file(
            authorised(&storage, run_id, "accepted"),
            b"accepted.txt",
        ))
        .expect("an accepted mutation");

    let after = retry_records(storage.transport().expect("transport"), run_id);
    assert_eq!(
        after.len(),
        2,
        "an accepted mutation writes exactly its op- index and key- record, saw {after:?}"
    );
    assert!(after
        .iter()
        .any(|n| n.starts_with(layout::RETRY_KEY_PREFIX)));
    assert!(after.iter().any(|n| n.starts_with(layout::RETRY_OP_PREFIX)));
    assert!(
        wire.lock().expect("not poisoned").modifying > modifying_before,
        "an accepted mutation sends modifying operations"
    );
}

// --- R2-010: a record is read whole, not assumed to fit one reply ------------

/// A transport that truncates every READ reply and reports "not end of file".
///
/// This is a legal server: NFSv4 permits a short `READ` that is not at EOF. The
/// journal used to treat the first reply as the whole record.
struct ShortReads {
    inner: FakeTransport,
    /// Bytes any one READ may return.
    cap: usize,
}

impl RawTransport for ShortReads {
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
        let mut reply = self.inner.submit(call, deadline)?;
        for result in reply.results.iter_mut() {
            if let umbra_storage_nfs_userspace::transport::OpReply::Read(read) = result {
                if read.data.len() > self.cap {
                    read.data.truncate(self.cap);
                    // Truncated, so this reply is no longer the end of the file.
                    read.eof = false;
                }
            }
        }
        Ok(reply)
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

/// **R2-010.** A record delivered in short, non-EOF replies is still read whole.
///
/// At the candidate, `read_record` issued one `READ` for `MAX_RECORD_BYTES` and
/// deserialized whatever came back. A server returning a legal short read made
/// the provider report `CorruptJournal` for a record it had written itself.
#[test]
fn r2_010_a_record_delivered_in_short_reads_is_still_read_whole() {
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(ShortReads {
            inner: fake_server(),
            cap: 16,
        }),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let request = create_file(authorised(&storage, run_id, "short-reads"), b"short.txt");
    let first = storage
        .execute(&request)
        .expect("the first attempt succeeds");

    // The retry has to read the settled record back, 16 bytes at a time.
    let retried = storage
        .execute(&request)
        .expect("an exact-key retry must be answered, not called corrupt");
    assert_eq!(
        format!("{first:?}"),
        format!("{retried:?}"),
        "the record read back in pieces must be the record that was written"
    );
}

/// **R2-010.** A valid record larger than one transport reply round-trips.
///
/// The provider advertises writes up to `max_io_bytes`, and a `WriteAt` record
/// embeds that payload as a JSON byte array, which is several times larger than
/// the bytes themselves. With a single `READ` capped at `max_reply_bytes`, a
/// record the provider wrote itself could not be read back.
#[test]
fn r2_010_a_record_larger_than_one_reply_round_trips() {
    // The raw transport caps every READ reply at `limits().max_reply_bytes`
    // (`raw/args.rs`); `FakeTransport` does not, so a fake-only run would return
    // even a multi-megabyte record in one reply and prove nothing. Capping here
    // models the real transport, which is where the defect lives.
    let reply_cap = fake_server().limits().max_reply_bytes;
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(ShortReads {
            inner: fake_server(),
            cap: reply_cap,
        }),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "big-target"),
            b"big.bin",
        ))
        .expect("create the target");

    // Every byte encodes as at least two JSON characters, so this payload's
    // record is comfortably past one reply while staying inside max_io_bytes.
    let advertised = storage
        .operations()
        .expect("a run is open")
        .capabilities()
        .max_io_bytes as usize;
    let payload_len = advertised.min(reply_cap);
    let payload = vec![0xFFu8; payload_len];

    let request = StorageRequest {
        context: authorised(&storage, run_id, "big-write"),
        operation: StorageOperation::WriteAt {
            path: path(b"big.bin"),
            offset: 0,
            bytes: payload.clone(),
        },
    };
    let first = storage.execute(&request).expect("the large write succeeds");

    // The record's JSON is larger than one reply; the retry must still find it.
    let retried = storage
        .execute(&request)
        .expect("an exact-key retry of a large record must be answered");
    assert_eq!(
        format!("{first:?}"),
        format!("{retried:?}"),
        "a record spanning several replies must round-trip exactly"
    );
    match retried {
        umbra_core::StorageResponse::WriteAt(count) => {
            assert_eq!(count as usize, payload.len())
        }
        other => panic!("expected a write response, got {other:?}"),
    }
}
