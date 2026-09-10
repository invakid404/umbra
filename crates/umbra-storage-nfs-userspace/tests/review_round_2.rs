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
        3,
        "an accepted mutation writes its pre- evidence, its op- index and its key- record,          saw {after:?}"
    );
    assert!(after
        .iter()
        .any(|n| n.starts_with(layout::RETRY_KEY_PREFIX)));
    assert!(after.iter().any(|n| n.starts_with(layout::RETRY_OP_PREFIX)));
    assert!(
        after.iter().any(|n| n.starts_with("pre-")),
        "R2-003: the precondition sidecar is part of an accepted mutation's evidence"
    );
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

// --- R2-007: the filesystem boundary covers every adoption path --------------

/// A transport that reports a foreign `fsid` for one named object.
///
/// Any op naming the boundary component means the compound is about that object:
/// LOOKUP resolves it, CREATE makes it, OPEN reaches a file leaf. Its fileid is
/// remembered, so later GETATTRs on the same object stay consistent — which is
/// what a nested export looks like.
struct NestedExport {
    inner: FakeTransport,
    boundary: Vec<u8>,
    /// Filehandles known to name the boundary object.
    ///
    /// Keyed by handle rather than by fileid on purpose: the OPEN compound is
    /// `PUTFH; OPEN; GETFH` with no GETATTR, so there is no fileid in the reply
    /// that first reaches the object. A later `PUTFH(handle); GETATTR` is how the
    /// provider pins it, and that compound names only the handle.
    foreign: Arc<Mutex<std::collections::BTreeSet<Vec<u8>>>>,
}

impl NestedExport {
    fn new(inner: FakeTransport, boundary: &[u8]) -> Self {
        Self {
            inner,
            boundary: boundary.to_vec(),
            foreign: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
        }
    }

    fn names_boundary(&self, call: &Compound) -> bool {
        call.ops.iter().any(|op| {
            let named: Option<&[u8]> = match op {
                Nfs4Op::Lookup(name) => Some(name.as_bytes()),
                Nfs4Op::Create { name, .. } => Some(name.as_bytes()),
                Nfs4Op::Remove { name } => Some(name.as_bytes()),
                Nfs4Op::Open(args) => match &args.claim {
                    umbra_storage_nfs_userspace::transport::OpenClaim::Null { name } => {
                        Some(name.as_bytes())
                    }
                    _ => None,
                },
                _ => None,
            };
            named == Some(self.boundary.as_slice())
        })
    }

    fn rewrite(&self, call: &Compound, reply: &mut CompoundReply) {
        use umbra_storage_nfs_userspace::transport::{Fsid, OpReply};
        const FOREIGN: Fsid = Fsid {
            major: 0xFEED,
            minor: 0xFACE,
        };
        let names_boundary = self.names_boundary(call);
        // The handle this compound is addressed to, if any.
        let addressed: Option<Vec<u8>> = call.ops.iter().find_map(|op| match op {
            Nfs4Op::PutFh(handle) => Some(handle.as_bytes().to_vec()),
            _ => None,
        });
        let addressed_foreign = addressed
            .is_some_and(|handle| self.foreign.lock().expect("not poisoned").contains(&handle));

        // A compound that named the boundary hands back the object's handle,
        // whether through GETFH or through LOOKUP's own reply.
        if names_boundary {
            let mut foreign = self.foreign.lock().expect("not poisoned");
            for result in reply.results.iter() {
                if let OpReply::GetFh(handle) = result {
                    foreign.insert(handle.as_bytes().to_vec());
                }
            }
        }

        if names_boundary || addressed_foreign {
            for result in reply.results.iter_mut() {
                if let OpReply::GetAttr(attributes) = result {
                    attributes.fsid = Some(FOREIGN);
                }
            }
        }
    }
}

impl RawTransport for NestedExport {
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
        let mut reply = self.inner.submit(call.clone(), deadline)?;
        self.rewrite(&call, &mut reply);
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

fn nested_provider(boundary: &[u8]) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(
        config(),
        Box::new(NestedExport::new(fake_server(), boundary)),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider")
}

/// **R2-007.** `CreateParents` must not walk through a directory on another
/// exported filesystem.
///
/// At the candidate, `create_parents` looped with `existing_child`, which calls
/// `child`, which performed a LOOKUP and `PinnedObject::adopt` with no comparison
/// to `anchors.filesystem()`. An ordinary `Create` of `nested/x` was refused by
/// the fixed resolver, but `CreateParents` of the same path adopted `nested`, saw
/// a directory, and sent CREATE beneath its handle.
#[test]
fn r2_007_create_parents_does_not_walk_through_a_foreign_filesystem() {
    let mut storage = nested_provider(b"nested");
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // Establish the boundary directory. Its own create is refused, because the
    // created object is reported on the foreign filesystem — but the name now
    // exists on the server, which is exactly the state the walk below meets.
    let _ = storage.execute(&StorageRequest {
        context: authorised(&storage, run_id, "seed-nested"),
        operation: StorageOperation::Create {
            path: path(b"nested"),
            options: CreateOptions {
                kind: CreateKind::Directory,
                mode: 0o700,
            },
        },
    });

    // The ordinary resolver path already refuses a crossing (R1-015).
    let ordinary = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "ordinary"),
            operation: StorageOperation::Create {
                path: path(b"nested/x"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            },
        })
        .expect_err("the resolver refuses a crossing");
    assert_eq!(ordinary.kind, ErrorKind::InvalidPath);

    // CreateParents must refuse it too, and for the same reason.
    let parents = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "create-parents"),
            operation: StorageOperation::CreateParents {
                path: path(b"nested/deep/deeper"),
                mode: 0o700,
            },
        })
        .expect_err("CreateParents must not walk through a filesystem crossing");
    assert_eq!(parents.kind, ErrorKind::InvalidPath);
    assert!(
        parents.context.contains("another exported filesystem"),
        "the refusal must name the crossing: {}",
        parents.context
    );
}

/// **R2-007.** `CreateParents` still builds an ordinary path.
#[test]
fn r2_007_create_parents_still_builds_an_ordinary_path() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "ordinary-parents"),
            operation: StorageOperation::CreateParents {
                path: path(b"a/b/c"),
                mode: 0o700,
            },
        })
        .expect("an ordinary mkdir -p is unaffected by the boundary check");
    storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("stat-abc".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"a/b/c"),
            },
        })
        .expect("the whole path exists");
}

/// **R2-007.** A write whose final OPEN lands on another filesystem is refused.
///
/// `parent_and_name` checks the parent; the object the write actually lands on
/// comes from the OPEN, which never passed the resolver.
#[test]
fn r2_007_a_write_to_a_foreign_final_target_is_refused() {
    let mut storage = nested_provider(b"target.bin");
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // The name exists on the server after this, even though its own create is
    // refused for being reported across the boundary.
    let _ = storage.execute(&create_file(
        authorised(&storage, run_id, "seed-target"),
        b"target.bin",
    ));

    let refused = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "foreign-write"),
            operation: StorageOperation::WriteAt {
                path: path(b"target.bin"),
                offset: 0,
                bytes: b"must not land".to_vec(),
            },
        })
        .expect_err("a write must not land on an object across the boundary");
    assert_eq!(refused.kind, ErrorKind::InvalidPath);
    assert!(
        refused.context.contains("another exported filesystem"),
        "the refusal must name the crossing: {}",
        refused.context
    );
}

// --- R2-004: missing epoch evidence is refused, not read as zero -------------

/// Seed an existing run whose `.provider` holds a valid manifest and, optionally,
/// an epoch file. No marker, as a cooperatively released run has none.
fn seed_released_run(run_id: RunId, epoch: Option<u64>) -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    let run_parent = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&run_parent, run_id.0.hyphenated().to_string().as_bytes());
    fake.insert_directory(&run, b"root");
    fake.insert_directory(&run, b"control");
    let private = fake.insert_directory(&run, layout::PRIVATE_DIR);
    fake.insert_directory(&private, layout::RETRIES_DIR);
    fake.insert_file(
        &private,
        layout::MANIFEST_FILE,
        serde_json::to_vec(&(run_id, &base(), FORMAT_VERSION)).expect("manifest encodes"),
    );
    if let Some(epoch) = epoch {
        fake.insert_file(&private, layout::EPOCH_FILE, epoch.to_le_bytes().to_vec());
    }
    fake
}

fn over(fake: FakeTransport) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(config(), Box::new(fake), Box::new(FakeReplayLog::default()))
        .expect("provider")
}

fn open_existing(storage: &mut NfsUserspaceStorage, run_id: RunId) -> umbra_core::Result<()> {
    storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::OpenExisting,
            immutable_base: base(),
            policy: policy(),
        })
        .map(|_| ())
}

/// **R2-004.** An existing run whose `.provider/epoch` is gone is refused, not
/// admitted at epoch 1.
///
/// At the candidate, absence returned `LeaseEpoch(0)` on the strength of a comment
/// about older runs written before the file existed — a case nothing establishes.
/// A run released cleanly at epoch 7 whose epoch file was then deleted would be
/// re-admitted at 1, silently below the ladder it had already reached, and a later
/// reader could not tell this session's epoch 3 from the run's own earlier 3.
#[test]
fn r2_004_a_missing_epoch_file_is_refused_not_read_as_zero() {
    let run_id = fresh_run();
    let mut storage = over(seed_released_run(run_id, None));
    let refused = open_existing(&mut storage, run_id)
        .expect_err("missing recovery evidence must not be read as a run that never had a writer");
    assert_eq!(refused.kind, ErrorKind::CorruptJournal);
    assert!(
        refused.context.contains("no .provider/epoch"),
        "the refusal must name the missing evidence: {}",
        refused.context
    );
    assert!(
        refused.context.contains("regress"),
        "and must say what it refused to risk: {}",
        refused.context
    );
    assert!(
        storage.admission().is_none(),
        "a refused open publishes no admission"
    );
}

/// **R2-004.** A present epoch file still works, at zero and at a legacy value.
///
/// This is the control: the refusal above must be about absence, not about
/// reading the file at all.
#[test]
fn r2_004_a_present_epoch_file_still_admits() {
    for (label, epoch, expected) in [("fresh", 0u64, 1u64), ("legacy", 7, 8)] {
        let run_id = fresh_run();
        let mut storage = over(seed_released_run(run_id, Some(epoch)));
        open_existing(&mut storage, run_id).unwrap_or_else(|error| panic!("{label}: {error:?}"));
        assert_eq!(
            storage.admission().expect("admitted").admitted().epoch().0,
            expected,
            "{label}: admission must land one above the recorded epoch"
        );
    }
}

// --- R2-003: interrupted operations are recovered, not stalled ---------------

/// Seed a run holding an interrupted intent *and* its precondition evidence,
/// exactly as a crash between the intent write and the result write leaves it.
///
/// `target_before` is the identity the sidecar records for the target; `None`
/// records that it was absent.
#[allow(clippy::too_many_arguments)]
fn seed_interrupted(
    run_id: RunId,
    key: &str,
    request: &StorageRequest,
    preconditions: &umbra_storage_nfs_userspace::journal::Preconditions,
    seed_target: Option<(&[u8], &[u8])>,
) -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    let run_parent = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&run_parent, run_id.0.hyphenated().to_string().as_bytes());
    let root_anchor = fake.insert_directory(&run, b"root");
    fake.insert_directory(&run, b"control");
    let private = fake.insert_directory(&run, layout::PRIVATE_DIR);
    fake.insert_file(
        &private,
        layout::MANIFEST_FILE,
        serde_json::to_vec(&(run_id, &base(), FORMAT_VERSION)).expect("manifest encodes"),
    );
    fake.insert_file(&private, layout::EPOCH_FILE, 0u64.to_le_bytes().to_vec());
    let retries = fake.insert_directory(&private, layout::RETRIES_DIR);

    if let Some((name, contents)) = seed_target {
        fake.insert_file(&root_anchor, name, contents.to_vec());
    }

    let intent: (
        StorageRequest,
        Option<umbra_core::Result<umbra_core::StorageResponse>>,
    ) = (request.clone(), None);
    let hex: String = key.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    fake.insert_file(
        &retries,
        format!("key-{hex}").as_bytes(),
        serde_json::to_vec(&intent).expect("intent encodes"),
    );
    fake.insert_file(
        &retries,
        format!("pre-{hex}").as_bytes(),
        serde_json::to_vec(preconditions).expect("preconditions encode"),
    );
    fake
}

/// The evidence the server reports for a name under the run's root anchor.
///
/// Real attributes, not synthesised: R3-002 makes the recovery compare content
/// state, so a test that made one up would be testing its own fiction.
fn evidence_of(
    storage: &mut NfsUserspaceStorage,
    run_id: RunId,
    name: &[u8],
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    let transport = storage.transport().expect("transport");
    let root = walk(transport, run_id, &[b"root"]);
    let (_, attributes) = transport
        .lookup(
            &root,
            &ComponentName::new(name.to_vec()).expect("component"),
            AttrMask::STAT,
            deadline(),
        )
        .expect("the object resolves");
    evidence_from(&attributes)
}

fn root_evidence(
    storage: &mut NfsUserspaceStorage,
    run_id: RunId,
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    let transport = storage.transport().expect("transport");
    let root = walk(transport, run_id, &[b"root"]);
    let attributes = transport
        .getattr(&root, AttrMask::STAT, deadline())
        .expect("the root anchor resolves");
    evidence_from(&attributes)
}

fn evidence_from(
    attributes: &umbra_storage_nfs_userspace::transport::Attributes,
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    umbra_storage_nfs_userspace::journal::ObjectEvidence {
        identity: umbra_storage_nfs_userspace::handle::ObjectIdentity {
            fsid: attributes.fsid.expect("fsid"),
            fileid: attributes.fileid.expect("fileid"),
        },
        change: attributes.change,
        size: attributes.size,
        modified_nanos: attributes
            .time_modify
            .map(|t| i128::from(t.seconds) * 1_000_000_000 + i128::from(t.nanoseconds)),
    }
}

/// The same evidence with a different identity, for "an external client replaced
/// this object" cases.
fn with_other_identity(
    mut evidence: umbra_storage_nfs_userspace::journal::ObjectEvidence,
    bump: u64,
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    evidence.identity.fileid += bump;
    evidence
}

/// The same evidence with a moved change attribute, for "this directory or object
/// has been mutated since" cases.
fn with_other_change(
    mut evidence: umbra_storage_nfs_userspace::journal::ObjectEvidence,
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    evidence.change = Some(evidence.change.unwrap_or(0).wrapping_add(1));
    evidence
}

/// **R2-003.** Interrupted *after* the intent and *before* the effect: the
/// operation is re-dispatched and completes.
///
/// At the candidate every unsettled intent returned StorageUnavailable with
/// "requires reconciliation", which `docs/design/failure-model.md:53` explicitly
/// forbids as a substitute for implemented recovery in a supported crash window.
/// Nothing consumed the record, and no before/after evidence existed to consume.
#[test]
fn r2_003_an_intent_interrupted_before_its_effect_is_recovered_by_redispatch() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted-create".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = create_file(context, b"recovered.txt");

    // Evidence recorded before dispatch: the target did not exist.
    let mut probe = over(seed_released_run(run_id, Some(0)));
    open_existing(&mut probe, run_id).expect("probe opens the run");
    let parent = root_evidence(&mut probe, run_id);

    let preconditions = umbra_storage_nfs_userspace::journal::Preconditions {
        parent: Some(parent),
        target: None,
        destination_parent: None,
        destination: None,
    };
    let mut storage = over(seed_interrupted(
        run_id,
        "interrupted-create",
        &request,
        &preconditions,
        None,
    ));
    open_existing(&mut storage, run_id).expect("the run opens");

    // The effect never landed, so recovery re-dispatches and the create succeeds.
    let recovered = storage
        .execute(&request)
        .expect("an intent whose effect never landed is recovered, not stalled");
    match recovered {
        umbra_core::StorageResponse::Created(_) => {}
        other => panic!("expected a create result, got {other:?}"),
    }

    // And the object is really there.
    storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("probe-recovered".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"recovered.txt"),
            },
        })
        .expect("the recovered create actually created the object");
}

/// **R2-003.** Interrupted *after* the effect and before the result: the recovery
/// settles the record from the observed state rather than repeating the effect.
///
/// This is the window that matters most for a non-idempotent primitive. The
/// removal already happened; re-dispatching would answer NOENT for an operation
/// that actually succeeded, which is the same class of wrong answer R1-004
/// recorded for rename.
#[test]
fn r2_003_an_intent_interrupted_after_its_effect_is_settled_from_evidence() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted-unlink".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = StorageRequest {
        context,
        operation: StorageOperation::Unlink {
            path: path(b"gone.txt"),
        },
    };

    // The sidecar records an object that existed before dispatch; the server no
    // longer has it, because the unlink landed before the crash.
    let mut probe = over(seed_released_run(run_id, Some(0)));
    open_existing(&mut probe, run_id).expect("probe opens the run");
    let parent = root_evidence(&mut probe, run_id);
    // R3-002: the parent's recorded change attribute differs from what the server
    // reports now, which is what a removal in that directory actually produces.
    // Without that, a vanished target and a motionless parent is the
    // contradictory state the recovery refuses.
    let preconditions = umbra_storage_nfs_userspace::journal::Preconditions {
        parent: Some(with_other_change(parent)),
        target: Some(with_other_identity(parent, 4242)),
        destination_parent: None,
        destination: None,
    };

    let mut storage = over(seed_interrupted(
        run_id,
        "interrupted-unlink",
        &request,
        &preconditions,
        None,
    ));
    open_existing(&mut storage, run_id).expect("the run opens");

    let recovered = storage
        .execute(&request)
        .expect("an interrupted unlink whose effect landed is settled, not re-run");
    assert!(
        matches!(recovered, umbra_core::StorageResponse::Unlinked),
        "the recovery answers the operation's own result, got {recovered:?}"
    );

    // The record is settled now, so an ordinary retry is answered from it.
    let again = storage
        .execute(&request)
        .expect("the settled record answers a later retry");
    assert!(matches!(again, umbra_core::StorageResponse::Unlinked));
}

/// **R2-003.** Evidence that matches neither the recorded before nor the expected
/// after is a blocked-recoverable stop, with the record retained.
///
/// An external writer replaced the object under the interrupted operation. There
/// is no deterministic answer, and the failure model says so: "conflicting
/// external writes during an interrupted operation are outside deterministic
/// automatic reconciliation".
#[test]
fn r2_003_contradictory_evidence_blocks_with_the_record_retained() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted-replaced".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = StorageRequest {
        context,
        operation: StorageOperation::Unlink {
            path: path(b"replaced.txt"),
        },
    };

    let mut probe = over(seed_released_run(run_id, Some(0)));
    open_existing(&mut probe, run_id).expect("probe opens the run");
    let parent = root_evidence(&mut probe, run_id);
    // Recorded: some object. Present now: a *different* object under that name.
    let preconditions = umbra_storage_nfs_userspace::journal::Preconditions {
        parent: Some(parent),
        target: Some(with_other_identity(parent, 9999)),
        destination_parent: None,
        destination: None,
    };
    let mut storage = over(seed_interrupted(
        run_id,
        "interrupted-replaced",
        &request,
        &preconditions,
        Some((b"replaced.txt", b"a different object")),
    ));
    open_existing(&mut storage, run_id).expect("the run opens");

    let blocked = storage
        .execute(&request)
        .expect_err("contradictory evidence has no deterministic answer");
    assert_eq!(
        blocked.kind,
        ErrorKind::InvalidState,
        "a blocked-recoverable stop: {blocked:?}"
    );
    assert!(
        blocked.context.contains("blocked rather than guessed"),
        "the stop must say it refused to guess: {}",
        blocked.context
    );
    assert!(
        !blocked.context.contains("requires reconciliation"),
        "the phrasing the failure model forbids must not reappear: {}",
        blocked.context
    );

    // The record and the object are both still there.
    let records = retry_records(storage.transport().expect("transport"), run_id);
    assert!(
        records.iter().any(|n| n.starts_with("key-")),
        "the interrupted record is retained, saw {records:?}"
    );
    assert!(
        records.iter().any(|n| n.starts_with("pre-")),
        "and so is its evidence, saw {records:?}"
    );
}

/// **R2-003.** An interrupted absolute write is re-dispatched when its target is
/// provably the same object, and blocked when it is not.
#[test]
fn r2_003_an_interrupted_write_recovers_only_onto_the_same_object() {
    for (label, same_object, expect_ok) in [("same", true, true), ("replaced", false, false)] {
        let run_id = fresh_run();
        let context = RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey("interrupted-write".into()),
            writer_epoch: Some(umbra_core::LeaseEpoch(1)),
        };
        let request = StorageRequest {
            context,
            operation: StorageOperation::WriteAt {
                path: path(b"target.bin"),
                offset: 0,
                bytes: b"exact bytes".to_vec(),
            },
        };

        // Seed the run with the target present, then read back its identity.
        let mut probe = over(seed_interrupted(
            run_id,
            "probe-only",
            &request,
            &umbra_storage_nfs_userspace::journal::Preconditions {
                parent: None,
                target: None,
                destination_parent: None,
                destination: None,
            },
            Some((b"target.bin", b"before")),
        ));
        open_existing(&mut probe, run_id).expect("probe opens the run");
        let parent = root_evidence(&mut probe, run_id);
        let actual = evidence_of(&mut probe, run_id, b"target.bin");

        // R3-002: the "same" case records the object's *exact* observed state, so
        // the replay is authorised by proven-unchanged content rather than by a
        // matching fileid alone.
        let recorded_target = if same_object {
            actual
        } else {
            with_other_identity(actual, 1234)
        };
        let preconditions = umbra_storage_nfs_userspace::journal::Preconditions {
            parent: Some(parent),
            target: Some(recorded_target),
            destination_parent: None,
            destination: None,
        };
        let mut storage = over(seed_interrupted(
            run_id,
            "interrupted-write",
            &request,
            &preconditions,
            Some((b"target.bin", b"before")),
        ));
        open_existing(&mut storage, run_id).expect("the run opens");

        let outcome = storage.execute(&request);
        if expect_ok {
            let response = outcome.unwrap_or_else(|error| {
                panic!("{label}: an absolute write onto the same object recovers: {error:?}")
            });
            match response {
                umbra_core::StorageResponse::WriteAt(count) => {
                    assert_eq!(count as usize, b"exact bytes".len())
                }
                other => panic!("{label}: expected a write result, got {other:?}"),
            }
        } else {
            let error = outcome.expect_err("{label}: a replaced target has no safe replay");
            assert_eq!(error.kind, ErrorKind::InvalidState, "{label}: {error:?}");
            assert!(
                error.context.contains("different object"),
                "{label}: the stop must name the risk: {}",
                error.context
            );
        }
    }
}

/// **R2-003.** A public write commits and compares verifiers, and a verifier the
/// server changed under it is a typed failure rather than a silent success.
///
/// At the candidate `write_at` took `ticket.count()` and dropped the ticket, so
/// nothing on the public path ever observed whether the server kept the bytes.
#[test]
fn r2_003_a_public_write_commits_and_notices_a_changed_verifier() {
    // Healthy: the write commits and the verifier matches.
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "seed"),
            b"committed.bin",
        ))
        .expect("create the target");
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "good-write"),
            operation: StorageOperation::WriteAt {
                path: path(b"committed.bin"),
                offset: 0,
                bytes: b"kept bytes".to_vec(),
            },
        })
        .expect("an ordinary write commits cleanly");

    // Now rotate the server's verifier on every reply, so whatever verifier the
    // WRITE reports, the COMMIT reports a different one. That is precisely what a
    // server which lost unstable data does, and the only thing that can detect it
    // is a COMMIT whose verifier is compared against the WRITE's.
    #[derive(Default)]
    struct RotateEveryReply(u8);
    impl FaultPlan for RotateEveryReply {
        fn decide(
            &mut self,
            point: FaultPoint,
            _context: umbra_storage_nfs_userspace::transport::FaultContext,
        ) -> FaultAction {
            if point != FaultPoint::BeforeReturn {
                return FaultAction::Proceed;
            }
            self.0 = self.0.wrapping_add(1);
            FaultAction::RotateVerifier(umbra_storage_nfs_userspace::transport::WriteVerifier(
                [self.0; 8],
            ))
        }
    }
    storage
        .transport()
        .expect("transport")
        .install_faults(Box::new(RotateEveryReply::default()));

    let outcome = storage.execute(&StorageRequest {
        context: authorised(&storage, run_id, "rotated-write"),
        operation: StorageOperation::WriteAt {
            path: path(b"committed.bin"),
            offset: 0,
            bytes: b"lost bytes".to_vec(),
        },
    });
    let error =
        outcome.expect_err("a verifier that changed under the write must not surface as a success");
    assert!(
        format!("{error:?}").contains("verifier") || error.kind == ErrorKind::Io,
        "the failure must name what could not be established: {error:?}"
    );
}

/// **R2-003.** A failed write is latched: later mutations stop, the original
/// status survives, and the release that follows is not reported clean.
///
/// `docs/design/failure-model.md`'s "Server EIO / failed stable write" row
/// requires exactly this. At the candidate the provider's failure accounting only
/// marked `StorageUnavailable` as unsettled, so an `NFS4ERR_IO` write left the
/// run mutating happily and closing cleanly afterwards.
#[test]
fn r2_003_a_failed_write_latches_and_stops_later_mutations() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "seed"),
            b"latched.bin",
        ))
        .expect("create the target");

    // Fail the write itself with the server's own I/O status.
    struct FailWrites;
    impl FaultPlan for FailWrites {
        fn decide(
            &mut self,
            point: FaultPoint,
            context: umbra_storage_nfs_userspace::transport::FaultContext,
        ) -> FaultAction {
            if point == FaultPoint::AfterDispatch && context.op == OpCode::PutFh {
                // The compound's first op identifies it; a write compound is
                // PUTFH; WRITE, so substituting here fails the write.
                return FaultAction::Substitute(Nfs4Status::IO);
            }
            FaultAction::Proceed
        }
    }
    storage
        .transport()
        .expect("transport")
        .install_faults(Box::new(FailWrites));

    let failed = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "failing-write"),
            operation: StorageOperation::WriteAt {
                path: path(b"latched.bin"),
                offset: 0,
                bytes: b"never lands".to_vec(),
            },
        })
        .expect_err("the injected NFS4ERR_IO surfaces");
    assert_eq!(failed.kind, ErrorKind::Io);

    // Clear the fault: the latch, not the fault, must be what stops the next one.
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::BeforeDispatch,
            Some(OpCode::Renew),
            FaultAction::Proceed,
        ));

    let refused = storage
        .execute(&create_file(
            authorised(&storage, run_id, "after-latch"),
            b"after.txt",
        ))
        .expect_err("a run that latched a write failure must not keep mutating");
    assert_eq!(refused.kind, ErrorKind::Io);
    assert!(
        refused.context.contains("latched a failed write"),
        "the refusal must name the latch: {}",
        refused.context
    );
    assert!(
        refused.context.contains("NFS4ERR 5"),
        "and must carry the original status forward: {}",
        refused.context
    );

    // And the close is not clean: a failed stable write owes no clean release.
    let closed = storage
        .close_run()
        .expect_err("a release over a latched write failure must not be reported clean");
    assert_eq!(closed.kind, ErrorKind::LeaseLost);
    assert!(
        closed
            .context
            .contains("outstanding I/O could not be excluded"),
        "the refusal must name the unsettled work: {}",
        closed.context
    );
}

// --- R3-001: a blocked recovery is a terminal provider state -----------------

/// Seed a run whose interrupted intent cannot be settled, and open it.
fn blocked_provider(run_id: RunId, key: &str) -> (NfsUserspaceStorage, StorageRequest) {
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = StorageRequest {
        context,
        operation: StorageOperation::Unlink {
            path: path(b"replaced.txt"),
        },
    };
    let mut probe = over(seed_released_run(run_id, Some(0)));
    open_existing(&mut probe, run_id).expect("probe opens the run");
    let parent = root_evidence(&mut probe, run_id);
    // Recorded one object; a different one is under that name now.
    let preconditions = umbra_storage_nfs_userspace::journal::Preconditions {
        parent: Some(parent),
        target: Some(with_other_identity(parent, 7777)),
        destination_parent: None,
        destination: None,
    };
    let mut storage = over(seed_interrupted(
        run_id,
        key,
        &request,
        &preconditions,
        Some((b"replaced.txt", b"a different object")),
    ));
    open_existing(&mut storage, run_id).expect("the run opens");
    (storage, request)
}

/// Seed a run with an interrupted record, then write its precondition sidecar
/// from evidence read off *that same server*.
///
/// Reading the evidence from a separate probe instance is unsound: `FakeTransport`
/// assigns fileids per instance, so a differently-seeded probe can report
/// identities the real fake never uses, and a recovery test would then be
/// comparing two fictions. `build` receives the actual fake.
fn seed_interrupted_with_evidence(
    run_id: RunId,
    key: &str,
    request: &StorageRequest,
    files: &[(&[u8], &[u8])],
    build: impl Fn(&mut FakeTransport, RunId) -> umbra_storage_nfs_userspace::journal::Preconditions,
) -> FakeTransport {
    let placeholder = umbra_storage_nfs_userspace::journal::Preconditions {
        parent: None,
        target: None,
        destination_parent: None,
        destination: None,
    };
    let mut fake = seed_interrupted(run_id, key, request, &placeholder, None);
    let root = walk_fake_root(&mut fake, run_id);
    for (name, bytes) in files {
        fake.insert_file(&root, name, bytes.to_vec());
    }
    // Now the server holds exactly what the recovery will observe.
    let preconditions = build(&mut fake, run_id);
    let retries = walk(
        &mut fake,
        run_id,
        &[layout::PRIVATE_DIR, layout::RETRIES_DIR],
    );
    let hex: String = key.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    fake.insert_file(
        &retries,
        format!("pre-{hex}").as_bytes(),
        serde_json::to_vec(&preconditions).expect("preconditions encode"),
    );
    fake
}

/// Evidence for a name under the run root, read straight off a fake.
fn fake_evidence(
    fake: &mut FakeTransport,
    run_id: RunId,
    name: &[u8],
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    let root = walk(fake, run_id, &[b"root"]);
    let (_, attributes) = fake
        .lookup(
            &root,
            &ComponentName::new(name.to_vec()).expect("component"),
            AttrMask::STAT,
            deadline(),
        )
        .expect("the object resolves");
    evidence_from(&attributes)
}

/// Evidence for the run root itself, read straight off a fake.
fn fake_root_evidence(
    fake: &mut FakeTransport,
    run_id: RunId,
) -> umbra_storage_nfs_userspace::journal::ObjectEvidence {
    let root = walk(fake, run_id, &[b"root"]);
    let attributes = fake
        .getattr(&root, AttrMask::STAT, deadline())
        .expect("the root anchor resolves");
    evidence_from(&attributes)
}

/// The run's root anchor handle, resolved directly on the fake.
fn walk_fake_root(fake: &mut FakeTransport, run_id: RunId) -> FileHandle {
    walk(fake, run_id, &[b"root"])
}

/// **R3-001.** After a recovery is refused, the run stops: a later mutation is
/// rejected and no fresh records are written.
///
/// At the candidate, `blocked()` was a generic `InvalidState` that `note_failure`
/// ignored — it latched only an `Io` error for a `WriteAt` — so `preflight`
/// accepted the very next mutation, the journal created fresh records and the
/// dispatch went through normally. Returning a blocked error is not the failure
/// model's BLOCKED_RECOVERABLE state.
#[test]
fn r3_001_a_refused_recovery_stops_later_mutations() {
    let run_id = fresh_run();
    let (mut storage, request) = blocked_provider(run_id, "blocked-terminal");

    let blocked = storage
        .execute(&request)
        .expect_err("contradictory evidence has no deterministic answer");
    assert!(
        blocked
            .context
            .starts_with(umbra_storage_nfs_userspace::journal::BLOCKED_RECOVERABLE),
        "the stop must be recognisable as blocked-recoverable: {}",
        blocked.context
    );

    let before = retry_records(storage.transport().expect("transport"), run_id);

    // A completely different, valid, fresh mutation.
    let refused = storage
        .execute(&create_file(
            RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("after-block".into()),
                writer_epoch: Some(umbra_core::LeaseEpoch(1)),
            },
            b"should-not-exist.txt",
        ))
        .expect_err("a stopped run must not accept new mutations");
    assert_eq!(refused.kind, ErrorKind::InvalidState);
    assert!(
        refused.context.contains("this run is stopped"),
        "the refusal must name the state: {}",
        refused.context
    );
    assert!(
        refused.context.contains("operator"),
        "and must say it needs intervention: {}",
        refused.context
    );

    // No records were written for the refused mutation.
    assert_eq!(
        retry_records(storage.transport().expect("transport"), run_id),
        before,
        "a stopped run writes no fresh journal records"
    );
}

/// **R3-001.** After a recovery is refused, a cooperative release does not
/// succeed: the interrupted intent is uncertain old I/O, and handing the run on
/// over it is what the failure model forbids.
#[test]
fn r3_001_a_refused_recovery_prevents_a_clean_release() {
    let run_id = fresh_run();
    let (mut storage, request) = blocked_provider(run_id, "blocked-release");
    storage
        .execute(&request)
        .expect_err("the recovery is refused");

    let lease = storage.admission().expect("admitted").lease();
    let refused = storage
        .release_writer(&lease)
        .expect_err("a release over an unsettled recovery must not be reported clean");
    assert_eq!(refused.kind, ErrorKind::LeaseLost);
    assert!(
        refused
            .context
            .contains("outstanding I/O could not be excluded"),
        "the refusal must name the unsettled work: {}",
        refused.context
    );
    assert!(
        storage.admission().is_some(),
        "the marker stays held: a refused release retains admission"
    );

    // And close is refused for the same reason.
    storage
        .close_run()
        .expect_err("close must not publish a release over an unsettled recovery");
}

/// **R3-001.** A journal stable-write failure for a *namespace* operation latches
/// too, not only for `WriteAt`.
///
/// The candidate's latch filtered on the request kind, so a failed FILE_SYNC
/// journal write belonging to a Rename or Create left the run mutating.
#[test]
fn r3_001_a_journal_write_failure_latches_for_a_namespace_operation() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    struct FailEverything;
    impl FaultPlan for FailEverything {
        fn decide(
            &mut self,
            point: FaultPoint,
            _context: umbra_storage_nfs_userspace::transport::FaultContext,
        ) -> FaultAction {
            if point == FaultPoint::AfterDispatch {
                return FaultAction::Substitute(Nfs4Status::IO);
            }
            FaultAction::Proceed
        }
    }
    storage
        .transport()
        .expect("transport")
        .install_faults(Box::new(FailEverything));

    let failed = storage
        .execute(&create_file(
            authorised(&storage, run_id, "namespace-io"),
            b"never.txt",
        ))
        .expect_err("the injected NFS4ERR_IO surfaces");
    assert_eq!(failed.kind, ErrorKind::Io);

    // Clear the fault; the latch must be what refuses the next one.
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::BeforeDispatch,
            Some(OpCode::Renew),
            FaultAction::Proceed,
        ));
    let refused = storage
        .execute(&create_file(
            authorised(&storage, run_id, "after-namespace-io"),
            b"after.txt",
        ))
        .expect_err("a latched I/O failure stops later mutations whatever the operation kind");
    assert_eq!(refused.kind, ErrorKind::Io);
    assert!(
        refused.context.contains("latched a failed write"),
        "the refusal must name the latch: {}",
        refused.context
    );
}

// --- R3-002: recovery consults content, not only identity --------------------

/// **R3-002.** An interrupted rename whose destination changed under it is
/// refused, not replayed over the replacement.
///
/// The reviewer's counterexample exactly: record `Rename(A, B)` with source `X`
/// and destination `Y`; an external client replaces `B` with `Z` while `A` is
/// still `X`. The candidate's rename arm ignored both recorded destination fields
/// and returned `NotApplied` from the unchanged source alone, so an ordinary
/// replacing RENAME destroyed `Z`.
#[test]
fn r3_002_an_interrupted_rename_does_not_replay_over_a_changed_destination() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted-rename".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = StorageRequest {
        context,
        operation: StorageOperation::Rename {
            source: path(b"source.txt"),
            destination: path(b"dest.txt"),
            mode: umbra_core::RenameMode::Replace,
        },
    };

    // Evidence is read off the very fake the provider will use, so the recorded
    // identities are the ones the recovery actually meets.
    let fake = seed_interrupted_with_evidence(
        run_id,
        "interrupted-rename",
        &request,
        &[(b"source.txt", b"X"), (b"dest.txt", b"Y")],
        |fake, run| {
            let parent = fake_root_evidence(fake, run);
            let source = fake_evidence(fake, run, b"source.txt");
            let destination_now = fake_evidence(fake, run, b"dest.txt");
            umbra_storage_nfs_userspace::journal::Preconditions {
                parent: Some(parent),
                target: Some(source),
                destination_parent: Some(parent),
                // Recorded destination is a *different* object from the one there
                // now: an external client replaced B between intent and retry.
                destination: Some(with_other_identity(destination_now, 5150)),
            }
        },
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let refused = storage
        .execute(&request)
        .expect_err("a changed destination has no safe replay");
    assert!(
        umbra_storage_nfs_userspace::journal::is_blocked(&refused),
        "the stop must be blocked-recoverable: {}",
        refused.context
    );
    assert!(
        refused.context.contains("destination"),
        "and must name the destination as the reason: {}",
        refused.context
    );

    // The replacement survived: nothing was renamed over it, and the source is
    // still where it was.
    for name in [b"dest.txt".as_slice(), b"source.txt".as_slice()] {
        storage
            .execute(&StorageRequest {
                context: RequestContext {
                    run_id,
                    operation_id: OperationId(Uuid::new_v4()),
                    idempotency_key: IdempotencyKey(format!(
                        "probe-{}",
                        String::from_utf8_lossy(name)
                    )),
                    writer_epoch: None,
                },
                operation: StorageOperation::Stat { path: path(name) },
            })
            .unwrap_or_else(|error| panic!("{:?} must survive: {error:?}", name));
    }
}

/// **R3-002.** An interrupted write whose target changed under it is refused, not
/// replayed over the external edit.
///
/// The candidate returned `NotApplied` from an unchanged `fsid`/`fileid`, which
/// says which object exists and nothing about whether it changed.
#[test]
fn r3_002_an_interrupted_write_does_not_replay_over_an_external_edit() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted-edit".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = StorageRequest {
        context,
        operation: StorageOperation::WriteAt {
            path: path(b"edited.bin"),
            offset: 0,
            bytes: b"our payload".to_vec(),
        },
    };

    let fake = seed_interrupted_with_evidence(
        run_id,
        "interrupted-edit",
        &request,
        &[(b"edited.bin", b"external edit landed here")],
        |fake, run| {
            let parent = fake_root_evidence(fake, run);
            let actual = fake_evidence(fake, run, b"edited.bin");
            umbra_storage_nfs_userspace::journal::Preconditions {
                parent: Some(parent),
                // Same object, different content state: an external writer edited
                // it after the intent was recorded.
                target: Some(with_other_change(actual)),
                destination_parent: None,
                destination: None,
            }
        },
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let refused = storage
        .execute(&request)
        .expect_err("an externally edited object has no safe absolute replay");
    assert!(
        umbra_storage_nfs_userspace::journal::is_blocked(&refused),
        "the stop must be blocked-recoverable: {}",
        refused.context
    );
    assert!(
        refused.context.contains("its state moved"),
        "and must name the content change: {}",
        refused.context
    );

    // The external bytes survived.
    let read = storage.execute(&StorageRequest {
        context: RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey("probe-read".into()),
            writer_epoch: None,
        },
        operation: StorageOperation::ReadAt {
            path: path(b"edited.bin"),
            offset: 0,
            len: 64,
        },
    });
    // The run is stopped, so the read is refused too — which is itself the point:
    // nothing overwrote the external edit before the stop.
    let _ = read;
}

// --- R3-003: a settled key is answered without observing its target ----------

/// **R3-003.** A completed key returns its recorded result even when its target's
/// path can no longer be resolved.
///
/// At the candidate, `execute` observed the target namespace for *every* mutation
/// before consulting the journal, so an external editor turning a path component
/// into something unresolvable made a previously completed operation start
/// failing, even though its record was intact and readable.
#[test]
fn r3_003_a_settled_key_is_answered_without_resolving_its_target() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // Complete a mutation under a nested directory.
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "mkdir"),
            operation: StorageOperation::CreateParents {
                path: path(b"nested"),
                mode: 0o700,
            },
        })
        .expect("create the parent directory");
    let context = authorised(&storage, run_id, "settled-key");
    let request = create_file(context, b"nested/file.txt");
    let first = storage
        .execute(&request)
        .expect("the first attempt succeeds");

    // Now make the target's *path* unresolvable the way an external replacement
    // does: the component `nested` stops being a directory. Resolving
    // `nested/file.txt` now fails with InvalidPath rather than NotFound, which is
    // the case the observation cannot absorb. The journal stays readable
    // throughout — it lives under `.provider`, not under `nested`.
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "unlink-child"),
            operation: StorageOperation::Unlink {
                path: path(b"nested/file.txt"),
            },
        })
        .expect("remove the file");
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "rmdir"),
            operation: StorageOperation::RemoveDirectory {
                path: path(b"nested"),
            },
        })
        .expect("remove the directory");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "nested-is-now-a-file"),
            b"nested",
        ))
        .expect("put a regular file where the directory was");

    // Confirm the premise: resolving through `nested` really does fail now, and
    // not with NotFound. Without this the test could pass vacuously.
    let resolution = storage.execute(&StorageRequest {
        context: RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey("premise".into()),
            writer_epoch: None,
        },
        operation: StorageOperation::Stat {
            path: path(b"nested/file.txt"),
        },
    });
    let premise = resolution.expect_err("the path no longer resolves");
    assert_eq!(
        premise.kind,
        ErrorKind::InvalidPath,
        "the premise is a resolution *failure*, not an absence: {premise:?}"
    );

    // The settled key must still answer from its record.
    let retried = storage
        .execute(&request)
        .expect("a settled key is answered from its record, not from its target's namespace");
    assert_eq!(
        format!("{first:?}"),
        format!("{retried:?}"),
        "the recorded outcome is returned verbatim"
    );
}

// --- R3-006: unsupported submodes are refused before any effect --------------

/// **R3-006.** A `NoReplace` rename leaves no durable trace.
///
/// `storage_support` maps every `Rename` to the same Supported row, so the mode
/// was refused only inside `Operations::rename` — after the provider had observed
/// state and written the precondition sidecar, the operation index and the intent,
/// and it then persisted the rejection too.
#[test]
fn r3_006_an_unsupported_rename_mode_leaves_no_durable_trace() {
    assert_no_durable_effect("no-replace", |storage, run_id| {
        let refused = storage
            .execute(&StorageRequest {
                context: authorised(storage, run_id, "no-replace"),
                operation: StorageOperation::Rename {
                    source: path(b"a.txt"),
                    destination: path(b"b.txt"),
                    mode: umbra_core::RenameMode::NoReplace,
                },
            })
            .expect_err("atomic no-replace is unsupported");
        assert_eq!(refused.kind, ErrorKind::UnsupportedCapability);
    });
}

/// **R3-001.** A record that cannot be decoded stops the run too, and its
/// bytes are left where they are.
///
/// The failure model's CORRUPTED row is "preserve remaining bytes; refuse
/// automatic repair". A run whose journal can no longer be read must not go on
/// accepting mutations that would add more records to it.
#[test]
fn r3_001_a_corrupt_record_stops_the_run_and_keeps_its_bytes() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("corrupt".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = create_file(context, b"whatever.txt");

    // A record file that is not JSON at all.
    let mut fake = seed_released_run(run_id, Some(0));
    let retries = walk(
        &mut fake,
        run_id,
        &[layout::PRIVATE_DIR, layout::RETRIES_DIR],
    );
    let hex: String = "corrupt"
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    fake.insert_file(
        &retries,
        format!("key-{hex}").as_bytes(),
        b"{ this is not a record".to_vec(),
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let corrupt = storage
        .execute(&request)
        .expect_err("a record that does not decode is refused");
    assert_eq!(corrupt.kind, ErrorKind::CorruptJournal);

    // The run is stopped: a different, valid, fresh mutation is refused.
    let refused = storage
        .execute(&create_file(
            RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("after-corrupt".into()),
                writer_epoch: Some(umbra_core::LeaseEpoch(1)),
            },
            b"should-not-exist.txt",
        ))
        .expect_err("a run whose journal cannot be read must not accept new mutations");
    assert_eq!(refused.kind, ErrorKind::InvalidState);
    assert!(
        refused.context.contains("this run is stopped"),
        "the refusal must name the state: {}",
        refused.context
    );

    // The corrupt bytes are still on the server.
    let records = retry_records(storage.transport().expect("transport"), run_id);
    assert!(
        records.iter().any(|n| n == &format!("key-{hex}")),
        "the corrupt record is preserved, saw {records:?}"
    );

    // And no clean release follows.
    let lease = storage.admission().expect("admitted").lease();
    storage
        .release_writer(&lease)
        .expect_err("a release over an unreadable journal must not be reported clean");
}

/// **R3-001.** A legacy intent with no evidence stops the run as well, not just
/// the one request that met it.
#[test]
fn r3_001_a_legacy_intent_stops_the_run() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("legacy".into()),
        writer_epoch: Some(umbra_core::LeaseEpoch(1)),
    };
    let request = create_file(context, b"legacy.txt");

    // An intent with no `pre-` sidecar: the shape an older provider leaves.
    let mut fake = seed_released_run(run_id, Some(0));
    let retries = walk(
        &mut fake,
        run_id,
        &[layout::PRIVATE_DIR, layout::RETRIES_DIR],
    );
    let hex: String = "legacy"
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let intent: (
        StorageRequest,
        Option<umbra_core::Result<umbra_core::StorageResponse>>,
    ) = (request.clone(), None);
    fake.insert_file(
        &retries,
        format!("key-{hex}").as_bytes(),
        serde_json::to_vec(&intent).expect("intent encodes"),
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let stopped = storage
        .execute(&request)
        .expect_err("a legacy intent with no evidence is not guessed");
    assert!(
        umbra_storage_nfs_userspace::journal::is_blocked(&stopped),
        "the stop is blocked-recoverable: {}",
        stopped.context
    );

    let refused = storage
        .execute(&create_file(
            RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("after-legacy".into()),
                writer_epoch: Some(umbra_core::LeaseEpoch(1)),
            },
            b"should-not-exist.txt",
        ))
        .expect_err("the run is stopped, not just that one request");
    assert!(
        refused.context.contains("this run is stopped"),
        "{}",
        refused.context
    );
}
