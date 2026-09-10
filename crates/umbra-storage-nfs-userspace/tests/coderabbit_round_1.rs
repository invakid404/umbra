//! Fixes for the CodeRabbit review of PR #33, triaged in
//! `/tmp/nfs-scope/m1-finish/cr-2.md` as `F04`–`F31`.
//!
//! Each case names the finding it covers and states in its own doc comment what
//! the reviewed candidate did, so a later round can trace fix to finding without
//! reading the diff. Only the VALID and the accurate half of each PARTIAL finding
//! is pinned here; the triage's INVALID and DEFERRED entries are deliberately
//! absent.
//!
//! **This suite mounts nothing.** No `~/umbra-scratch`, no `nfs_fixture_matrix`,
//! no OS-visible NFS mount, no live Ganesha.

use umbra_core::{
    BytePath, ErrorKind, ImmutableBaseContract, OpenRunIntent, OpenRunRequest, RunId,
    StorageAnchor, StoragePath, StoragePolicy,
};
use umbra_storage::Storage;
use uuid::Uuid;

use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport};
use umbra_storage_nfs_userspace::layout;
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};
use umbra_storage_nfs_userspace::transport::{ComponentName, Deadline, RawTransport};

/// Records the largest single allocation this test binary makes.
///
/// **F17** needs evidence about an *eager reservation*, and a reservation is one
/// allocation: a `Vec::with_capacity(n)` asks the allocator for `n * size_of::<T>()`
/// bytes in a single call, whether or not a single entry is ever written. Peak
/// resident memory would be noisy across parallel tests; the largest single
/// request is not, because nothing else in this binary asks for anything close.
struct LargestAllocation;

static LARGEST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe impl std::alloc::GlobalAlloc for LargestAllocation {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, std::sync::atomic::Ordering::Relaxed);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: LargestAllocation = LargestAllocation;

const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

fn deadline() -> Deadline {
    Deadline { millis: 5_000 }
}

fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        port: 12112,
        export: BytePath::new(EXPORT).expect("export path"),
        run_parent: BytePath::new(RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: deadline(),
    }
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

fn fresh_run() -> RunId {
    RunId(Uuid::new_v4())
}

#[allow(dead_code)]
fn path(bytes: &[u8]) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec()).expect("a valid contract path")
}

fn name(bytes: &[u8]) -> ComponentName {
    ComponentName::new(bytes.to_vec()).expect("a valid component")
}

/// An existing run whose `.provider` holds a valid manifest and an epoch file.
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

// --- F04: the private-file reader follows short READ replies -----------------

/// **F04.** `.provider/epoch` and `.provider/manifest` are read whole, across as
/// many short replies as the server chooses to send.
///
/// At the reviewed candidate `read_private_file` issued exactly one `READ` and
/// returned `reply.data` verbatim. A short reply is legal —
/// [RFC 7530 §16.25.4](https://www.rfc-editor.org/rfc/rfc7530.html#section-16.25.4)
/// lets a server answer with fewer bytes than requested and leave `eof` clear —
/// so a server that capped its replies at three bytes handed the epoch decoder a
/// three-byte slice, and a perfectly healthy run was refused as corrupt.
///
/// Three bytes is a deliberate choice: it divides neither the eight-byte epoch
/// nor the manifest, so both files need a final partial chunk.
#[test]
fn f04_a_short_read_reply_does_not_truncate_the_private_files() {
    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, Some(7));
    fake.set_read_cap(Some(3));
    let mut storage = over(fake);

    open_existing(&mut storage, run_id)
        .expect("a run whose server answers short reads still opens");

    // The epoch really was decoded from all eight bytes: a truncated read would
    // not have produced 7, and the next admission sits one above it.
    assert_eq!(
        storage
            .admission()
            .expect("a run is open")
            .admitted()
            .epoch(),
        umbra_core::LeaseEpoch(8),
        "the epoch ladder advances from the recorded 7, so all eight bytes were read"
    );
}

/// **F04.** A single-byte cap is still enough, and an exact-fit cap does not
/// mistake the boundary for a truncation.
#[test]
fn f04_b_byte_at_a_time_and_exact_fit_reads_both_complete() {
    for cap in [1_u32, 8] {
        let run_id = fresh_run();
        let mut fake = seed_released_run(run_id, Some(3));
        fake.set_read_cap(Some(cap));
        let mut storage = over(fake);
        open_existing(&mut storage, run_id)
            .unwrap_or_else(|error| panic!("cap {cap} must still open the run: {error:?}"));
        assert_eq!(
            storage
                .admission()
                .expect("a run is open")
                .admitted()
                .epoch(),
            umbra_core::LeaseEpoch(4),
            "cap {cap} must read the whole epoch"
        );
    }
}

/// **F04.** A server that returns no bytes and no end of file is refused, not
/// looped on.
///
/// The reader advances by what actually arrived, so a server that never advances
/// would spin forever. That is not an answer: the run is refused with a transport
/// diagnosis, distinct from the corrupt-frame refusal a bad file gets.
#[test]
fn f04_c_a_stalled_read_is_refused_rather_than_looped_on() {
    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, Some(7));
    fake.set_read_cap(Some(0));
    let mut storage = over(fake);

    let refused = open_existing(&mut storage, run_id)
        .expect_err("a server that never advances cannot produce a whole file");
    assert_eq!(
        refused.kind,
        ErrorKind::ProtocolMismatch,
        "a stalled read is a reply-shape failure, not a corrupt file: {refused:?}"
    );
    assert!(
        refused.context.contains("no end of file"),
        "the diagnosis must name what the server did: {}",
        refused.context
    );
}

/// **F04.** A private file larger than the bound is refused rather than read as a
/// truncated one.
#[test]
fn f04_d_a_private_file_past_the_bound_is_refused() {
    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, None);
    // Rebuild the epoch file as something far larger than the 64 KiB bound.
    let private = walk(&mut fake, run_id, &[layout::PRIVATE_DIR]);
    fake.insert_file(&private, layout::EPOCH_FILE, vec![0u8; 96 * 1024]);
    let mut storage = over(fake);

    let refused = open_existing(&mut storage, run_id)
        .expect_err("a file past the bound is not one this provider wrote");
    assert_eq!(refused.kind, ErrorKind::CorruptJournal);
    assert!(
        refused.context.contains("bound"),
        "the diagnosis must name the bound it exceeded: {}",
        refused.context
    );
}

/// **F04.** A transport failure during the read is propagated, not read as
/// absence.
#[test]
fn f04_e_a_failed_read_is_propagated_not_absorbed_as_absence() {
    use umbra_storage_nfs_userspace::error::Nfs4Status;
    use umbra_storage_nfs_userspace::fake::ScriptedFault;
    use umbra_storage_nfs_userspace::transport::{FaultAction, FaultPoint};

    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, Some(7));
    fake.set_read_cap(Some(3));
    // Fail after the first chunk has already been accumulated, which is the
    // window a single-shot reader never had.
    fake.install_faults(ScriptedFault::once(
        FaultPoint::AfterDispatch,
        Some(umbra_storage_nfs_userspace::transport::OpCode::Read),
        FaultAction::Substitute(Nfs4Status::IO),
    ));
    let mut storage = over(fake);

    let failed = open_existing(&mut storage, run_id)
        .expect_err("an IO status during the read is a failure, not an absent file");
    assert_ne!(
        failed.kind,
        ErrorKind::NotFound,
        "a failed read must never be reported as absence: {failed:?}"
    );
}

// --- R2-02: the whole-file reader reserves budget for the COMPOUND tag -------

/// A transport that enforces the raw decoder's shared reply budget.
///
/// `transport::raw::decode` charges the echoed COMPOUND tag *and* the READ
/// payload — the zero-copy path included — against one `max_reply_bytes`
/// figure. `FakeTransport` models protocol shape and does not account for that,
/// so this wrapper supplies the accounting the raw decoder does. It also counts
/// submissions, which is how "refused before dispatch" is asserted rather than
/// assumed.
struct Budgeted {
    inner: FakeTransport,
    budget: usize,
    submits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Budgeted {
    fn new(inner: FakeTransport, budget: usize) -> Self {
        Self {
            inner,
            budget,
            submits: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn submits(&self) -> usize {
        self.submits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl RawTransport for Budgeted {
    fn wire_profile(&self) -> umbra_storage_nfs_userspace::transport::WireProfile {
        self.inner.wire_profile()
    }
    fn limits(&self) -> umbra_storage_nfs_userspace::transport::TransportLimits {
        umbra_storage_nfs_userspace::transport::TransportLimits {
            max_reply_bytes: self.budget,
            ..self.inner.limits()
        }
    }
    fn connection(&self) -> umbra_storage_nfs_userspace::transport::ConnectionState {
        self.inner.connection()
    }
    fn submit(
        &mut self,
        call: umbra_storage_nfs_userspace::transport::Compound,
        call_deadline: Deadline,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::CompoundReply,
    > {
        use umbra_storage_nfs_userspace::transport::OpReply;
        self.submits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reply = self.inner.submit(call, call_deadline)?;
        let charged = reply.tag.len()
            + reply
                .results
                .iter()
                .map(|op| match op {
                    OpReply::Read(read) => read.data.len(),
                    _ => 0,
                })
                .sum::<usize>();
        if charged > self.budget {
            return Err(
                umbra_storage_nfs_userspace::error::TransportError::Malformed(format!(
                    "tag plus READ payload {charged} exceeds budget {}",
                    self.budget
                )),
            );
        }
        Ok(reply)
    }
    fn cancel(
        &mut self,
        token: umbra_storage_nfs_userspace::transport::CallToken,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::Retirement,
    > {
        self.inner.cancel(token)
    }
    fn reconnect(
        &mut self,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::ConnectionEpoch,
    > {
        self.inner.reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn umbra_storage_nfs_userspace::transport::FaultPlan>) {
        self.inner.install_faults(plan)
    }
}

/// A `Budgeted` transport over a fake holding one file of `bytes` under the root.
fn budgeted(
    bytes: Vec<u8>,
    budget: usize,
) -> (Budgeted, umbra_storage_nfs_userspace::handle::FileHandle) {
    let mut inner = FakeTransport::new();
    let root = inner.root();
    let handle = inner.insert_file(&root, b"budgeted", bytes);
    (Budgeted::new(inner, budget), handle)
}

/// **R2-02.** A constrained reply budget still reads a whole file, because the
/// chunk leaves room for the tag the reply carries with it.
///
/// The round-1 helper sized its chunk at the *whole* `max_reply_bytes`. The
/// budget is not all payload: `RawTransport::read` sends a four-byte tag, the
/// server echoes it, and `transport::raw::decode` charges the echoed tag and the
/// READ data to the same figure. So a full reply to a full-budget request always
/// overruns by the tag length, and a perfectly healthy file became
/// `Malformed: tag plus READ payload 132 exceeds budget 128`.
#[test]
fn f04_f_a_constrained_budget_still_reads_the_whole_file() {
    use umbra_storage_nfs_userspace::crud::read_whole;
    use umbra_storage_nfs_userspace::handle::Stateid;

    let (mut transport, handle) = budgeted(vec![42u8; 256], 128);
    let whole = read_whole(
        &mut transport,
        &handle,
        Stateid::ANONYMOUS,
        1024,
        deadline(),
    )
    .expect("a 128-byte budget carries a 124-byte payload plus its 4-byte tag");

    assert!(whole.complete, "end of file was reached inside the limit");
    assert_eq!(whole.data, vec![42u8; 256]);
    assert!(
        transport.submits() >= 3,
        "256 bytes cannot arrive in fewer than three 124-byte payloads: {}",
        transport.submits()
    );
}

/// **R2-02.** A final read that exactly fills the available payload and carries
/// `eof` is still a complete read, not a boundary the loop mistakes for more.
#[test]
fn f04_g_an_exact_fit_final_read_reports_end_of_file() {
    use umbra_storage_nfs_userspace::crud::read_whole;
    use umbra_storage_nfs_userspace::handle::Stateid;

    // Two payloads of exactly 124 bytes: 128 budget minus the four-byte tag.
    let (mut transport, handle) = budgeted(vec![7u8; 248], 128);
    let whole = read_whole(
        &mut transport,
        &handle,
        Stateid::ANONYMOUS,
        1024,
        deadline(),
    )
    .expect("an exact fit is not an overrun");

    assert!(whole.complete);
    assert_eq!(whole.data.len(), 248);
    assert_eq!(
        transport.submits(),
        2,
        "exactly two full payloads, the second carrying eof"
    );
}

/// **R2-02.** Intermediate short replies still advance, and every one of them
/// stays inside the budget. The round-1 short-read behaviour is unchanged; only
/// the size the reader asks for is.
#[test]
fn f04_g_short_replies_under_a_constrained_budget_still_advance() {
    use umbra_storage_nfs_userspace::crud::read_whole;
    use umbra_storage_nfs_userspace::handle::Stateid;

    let (mut transport, handle) = budgeted(vec![9u8; 300], 128);
    // The server answers with far less than the 124 asked for, and never sets
    // eof early. Advancing by what arrived is what finishes the file.
    transport.inner.set_read_cap(Some(30));
    let whole = read_whole(
        &mut transport,
        &handle,
        Stateid::ANONYMOUS,
        1024,
        deadline(),
    )
    .expect("short replies are legal and are followed");

    assert!(whole.complete);
    assert_eq!(whole.data, vec![9u8; 300]);
    assert!(
        transport.submits() >= 10,
        "300 bytes in 30-byte replies takes at least ten: {}",
        transport.submits()
    );
}

/// **R2-02.** The caller's total bound still wins over the budget-derived chunk,
/// and reaching it without end of file is still reported rather than refused.
#[test]
fn f04_g_the_total_bound_still_bounds_a_constrained_read() {
    use umbra_storage_nfs_userspace::crud::read_whole;
    use umbra_storage_nfs_userspace::handle::Stateid;

    let (mut transport, handle) = budgeted(vec![3u8; 256], 128);
    let bounded = read_whole(&mut transport, &handle, Stateid::ANONYMOUS, 100, deadline())
        .expect("a limit below one chunk is a legal request");

    assert!(
        !bounded.complete,
        "the bound was reached before end of file, which is an answer not a failure"
    );
    assert_eq!(bounded.data.len(), 100);
    assert_eq!(bounded.data, vec![3u8; 100]);
}

/// **R2-02.** A budget that cannot hold the tag plus a byte of progress is
/// refused *before* anything is dispatched.
///
/// The lower-bound edge matters as much as the reservation itself: clamping zero
/// available payload back up to one byte would issue exactly the over-budget
/// request the reservation exists to prevent, and looping on a zero-byte request
/// would hang.
#[test]
fn f04_h_a_budget_too_small_for_the_tag_is_refused_before_dispatch() {
    use umbra_storage_nfs_userspace::crud::read_whole;
    use umbra_storage_nfs_userspace::handle::Stateid;

    for budget in [0usize, 1, 4] {
        let (mut transport, handle) = budgeted(vec![1u8; 64], budget);
        let refused = read_whole(
            &mut transport,
            &handle,
            Stateid::ANONYMOUS,
            1024,
            deadline(),
        )
        .expect_err("a budget with no room for payload cannot read anything");
        assert!(
            matches!(
                refused,
                umbra_storage_nfs_userspace::error::FacadeError::Transport(
                    umbra_storage_nfs_userspace::error::TransportError::Malformed(_)
                )
            ),
            "budget {budget}: {refused:?}"
        );
        assert!(
            refused.to_string().contains("budget"),
            "budget {budget}: the diagnosis must name the bound it cannot meet: {refused}"
        );
        assert_eq!(
            transport.submits(),
            0,
            "budget {budget}: nothing may reach the wire when no request could fit"
        );
    }

    // One byte over the tag is enough to make progress.
    let (mut transport, handle) = budgeted(vec![1u8; 8], 5);
    let whole = read_whole(
        &mut transport,
        &handle,
        Stateid::ANONYMOUS,
        1024,
        deadline(),
    )
    .expect("a single byte of payload per reply is slow, not impossible");
    assert!(whole.complete);
    assert_eq!(whole.data, vec![1u8; 8]);
}

/// **R2-02.** The tag the reader reserves for is the tag that is actually sent,
/// and the server echoes it back into the same budget.
///
/// The whole fix rests on those two facts. If `RawTransport::read` stopped using
/// `READ_TAG`, or a server answered with a tag of its own choosing, the reserved
/// figure would be wrong in a way no arithmetic test would catch.
#[test]
fn f04_h_the_reserved_tag_is_the_one_on_the_wire() {
    use umbra_storage_nfs_userspace::handle::Stateid;
    use umbra_storage_nfs_userspace::transport::READ_TAG;

    let (mut transport, handle) = budgeted(vec![5u8; 16], 1024);
    let reply = transport
        .inner
        .submit(
            umbra_storage_nfs_userspace::transport::Compound::new(
                READ_TAG,
                vec![
                    umbra_storage_nfs_userspace::transport::Nfs4Op::PutFh(handle.clone()),
                    umbra_storage_nfs_userspace::transport::Nfs4Op::Read {
                        stateid: Stateid::ANONYMOUS,
                        offset: 0,
                        count: 16,
                    },
                ],
            ),
            deadline(),
        )
        .expect("the read is answered");
    assert_eq!(
        reply.tag.len(),
        READ_TAG.len(),
        "the server echoes the tag, and it is charged to the same budget"
    );

    // And a whole-file read of the same object costs exactly tag + payload per
    // reply, which the budgeted wrapper would have refused otherwise.
    let whole = umbra_storage_nfs_userspace::crud::read_whole(
        &mut transport,
        &handle,
        Stateid::ANONYMOUS,
        1024,
        deadline(),
    )
    .expect("a generous budget reads it in one reply");
    assert!(whole.complete);
    assert_eq!(whole.data.len(), 16);
}

/// **R2-02, adjacent.** The `max_io_bytes` the provider advertises is a size a
/// `ReadAt` of exactly that many bytes can actually be answered with.
///
/// `OperationLimits::from_transport` says an advertised limit is "one the
/// provider can meet rather than one it hopes to". Under a constrained budget it
/// was not: it advertised the whole reply budget, and a `ReadAt` of exactly that
/// produced a reply costing budget-plus-tag, which the decoder refuses.
#[test]
fn f04_h_the_advertised_io_bound_leaves_room_for_the_tag() {
    use umbra_storage_nfs_userspace::ops::OperationLimits;
    use umbra_storage_nfs_userspace::transport::READ_TAG;

    let (transport, _) = budgeted(vec![0u8; 1], 4096);
    let limits = OperationLimits::from_transport(&transport);
    assert_eq!(
        limits.max_io_bytes as usize,
        4096 - READ_TAG.len(),
        "the advertised read size plus its tag must fit the reply budget"
    );
    assert!(
        limits.max_io_bytes as usize + READ_TAG.len() <= 4096,
        "which is the property that makes it meetable"
    );
    assert!(limits.max_directory_entries > 0);
}

/// **R2-02, adjacent.** The retry journal's own record reader works under a
/// constrained budget too.
///
/// `journal`'s bounded reader is the one `crud::read_whole` was modelled on, uses
/// the same `b"read"` tag through `read_anonymous`, and had the identical
/// arithmetic: a chunk sized at the whole reply budget. It was simply not the
/// reader the review reproduced.
///
/// Driven through the provider, so every budgeted read on the path is exercised
/// at once — the manifest, the epoch file, the 256-byte admission marker, and
/// then the retry record read back on the second attempt at the same key.
#[test]
fn f04_h_a_constrained_budget_runs_the_whole_provider_read_path() {
    let run_id = fresh_run();
    let (budgeted_transport, _) = {
        let seeded = seed_released_run(run_id, Some(0));
        (Budgeted::new(seeded, 128), ())
    };
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(budgeted_transport),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");

    // Opening reads the manifest and the epoch file; admission reads the marker,
    // which is 256 bytes and so needs three 124-byte payloads on its own.
    open_existing(&mut storage, run_id).expect("a constrained budget still opens the run");
    let epoch = storage
        .admission()
        .expect("a run is open")
        .admitted()
        .epoch();

    let request = create_request(run_id, epoch, "budgeted-key", b"budgeted.txt");
    let created = storage
        .execute(&request)
        .expect("a create under a constrained budget still writes its record and dispatches");
    assert!(matches!(created, umbra_core::StorageResponse::Created(_)));

    // The retry reads that record back through the journal's own bounded reader.
    let replayed = storage
        .execute(&request)
        .expect("the settled record is read back whole under the same budget");
    assert!(matches!(replayed, umbra_core::StorageResponse::Created(_)));
}

/// Walk to a directory inside a seeded run.
fn walk(
    transport: &mut dyn RawTransport,
    run_id: RunId,
    parts: &[&[u8]],
) -> umbra_storage_nfs_userspace::handle::FileHandle {
    let mut cursor = transport.root_filehandle(deadline()).expect("root");
    let run = run_id.0.hyphenated().to_string();
    let mut components: Vec<&[u8]> = EXPORT.split(|byte| *byte == b'/').collect();
    components.push(RUN_PARENT);
    components.push(run.as_bytes());
    components.extend_from_slice(parts);
    for part in components {
        cursor = transport
            .lookup(
                &cursor,
                &name(part),
                umbra_storage_nfs_userspace::transport::AttrMask::STAT,
                deadline(),
            )
            .unwrap_or_else(|error| panic!("walk {:?}: {error:?}", String::from_utf8_lossy(part)))
            .0;
    }
    cursor
}

// --- F11: a deadline needs recovery, it is not directly retriable ------------

/// **F11.** `DeadlineExpired` classifies as `NeedsRecovery`, not `Retriable`.
///
/// At the reviewed candidate it was grouped with `QueueFull`, whose refusal
/// happens *before* dispatch. A deadline is not that: `Retirement` proves the
/// call was withdrawn from this pump and proves nothing about the server, which
/// may have received and applied the request. `ErrorClass::Retriable` promises
/// "no state was lost", so a caller that trusted it would re-offer a mutation
/// whose first attempt could still land.
///
/// The error value itself is untouched by classification — the retirement, the
/// token it names and the rendered detail are all still what the transport
/// produced.
#[test]
fn f11_a_deadline_needs_recovery_and_a_full_queue_is_still_retriable() {
    use umbra_storage_nfs_userspace::error::{ErrorClass, FacadeError, TransportError};
    use umbra_storage_nfs_userspace::fake::ScriptedFault;
    use umbra_storage_nfs_userspace::transport::{FaultAction, FaultPoint};

    let mut fake = FakeTransport::new();
    fake.install_faults(ScriptedFault::once(
        FaultPoint::OnDeadline,
        None,
        FaultAction::DropReply,
    ));
    let deadline_error = fake
        .root_filehandle(deadline())
        .expect_err("the dropped reply must reach its deadline");
    let FacadeError::Transport(TransportError::DeadlineExpired { retirement }) = &deadline_error
    else {
        panic!("expected a deadline, got {deadline_error:?}");
    };
    let token = retirement.token();
    let rendered = deadline_error.to_string();

    assert_eq!(
        deadline_error.class(),
        ErrorClass::NeedsRecovery,
        "a withdrawn registration is not proof the server did nothing"
    );
    assert_ne!(deadline_error.class(), ErrorClass::Retriable);

    // Classification discards nothing.
    let FacadeError::Transport(TransportError::DeadlineExpired { retirement }) = &deadline_error
    else {
        unreachable!("still a deadline");
    };
    assert_eq!(retirement.token(), token, "the retirement is unchanged");
    assert_eq!(
        deadline_error.to_string(),
        rendered,
        "the detail is unchanged"
    );
    assert_eq!(
        deadline_error.status(),
        None,
        "a deadline carries no server status, before or after classification"
    );

    // The pre-dispatch refusal keeps its own answer: nothing reached the wire,
    // so the same identity may simply be offered again.
    let queue_full = FacadeError::Transport(TransportError::QueueFull {
        depth: 64,
        capacity: 64,
    });
    assert_eq!(queue_full.class(), ErrorClass::Retriable);
}

// --- F15: a REMOVE that cannot prove what it unlinked stops the run ----------

/// A transport that replaces the doomed name between the probe and the REMOVE.
///
/// The replacement travels over the wire — a real `REMOVE` then a creating
/// `OPEN` — so the parent's `FATTR4_CHANGE` moves exactly as it would on a
/// server, which is the evidence the fix reads. Seeding the fake's maps directly
/// would not move it and would prove nothing.
struct ReplaceBeforeRemove {
    inner: FakeTransport,
    fired: bool,
    parent: umbra_storage_nfs_userspace::handle::FileHandle,
    victim: Vec<u8>,
}

impl RawTransport for ReplaceBeforeRemove {
    fn wire_profile(&self) -> umbra_storage_nfs_userspace::transport::WireProfile {
        self.inner.wire_profile()
    }
    fn limits(&self) -> umbra_storage_nfs_userspace::transport::TransportLimits {
        self.inner.limits()
    }
    fn connection(&self) -> umbra_storage_nfs_userspace::transport::ConnectionState {
        self.inner.connection()
    }
    fn submit(
        &mut self,
        call: umbra_storage_nfs_userspace::transport::Compound,
        call_deadline: Deadline,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::CompoundReply,
    > {
        use umbra_storage_nfs_userspace::transport::Nfs4Op;
        let removes = call
            .ops
            .iter()
            .any(|op| matches!(op, Nfs4Op::Remove { name } if name.as_bytes() == self.victim));
        if removes && !self.fired {
            self.fired = true;
            // Another client unlinks the name and creates a different object
            // under it, in the window between our probe and our REMOVE.
            let parent = self.parent.clone();
            let victim = name(&self.victim);
            self.inner
                .submit(
                    umbra_storage_nfs_userspace::transport::Compound::new(
                        *b"race",
                        vec![
                            Nfs4Op::PutFh(parent.clone()),
                            Nfs4Op::Remove {
                                name: victim.clone(),
                            },
                        ],
                    ),
                    call_deadline,
                )
                .expect("the racing unlink lands");
            let owner = umbra_storage_nfs_userspace::handle::Session::establish(
                umbra_storage_nfs_userspace::handle::SessionId(9),
                umbra_storage_nfs_userspace::handle::ClientId(9),
                umbra_storage_nfs_userspace::transport::ConnectionEpoch(1),
            )
            .open_owner(b"racer".to_vec())
            .expect("a fresh session mints owners");
            self.inner
                .open(
                    &parent,
                    umbra_storage_nfs_userspace::transport::OpenArgs {
                        seqid: 0,
                        share_access: umbra_storage_nfs_userspace::transport::ShareAccess::BOTH,
                        share_deny: umbra_storage_nfs_userspace::transport::ShareDeny::NONE,
                        owner,
                        how: umbra_storage_nfs_userspace::transport::OpenHow::Guarded {
                            mode: 0o600,
                        },
                        claim: umbra_storage_nfs_userspace::transport::OpenClaim::Null {
                            name: victim,
                        },
                    },
                    call_deadline,
                )
                .expect("the racing create lands");
        }
        self.inner.submit(call, call_deadline)
    }
    fn cancel(
        &mut self,
        token: umbra_storage_nfs_userspace::transport::CallToken,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::Retirement,
    > {
        self.inner.cancel(token)
    }
    fn reconnect(
        &mut self,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::ConnectionEpoch,
    > {
        self.inner.reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn umbra_storage_nfs_userspace::transport::FaultPlan>) {
        self.inner.install_faults(plan)
    }
}

/// **F15.** A racing replacement between the probe and the REMOVE is not reported
/// as the caller's own removal, and the run does not release cleanly afterwards.
///
/// At the reviewed candidate the dispatcher proved the pinned identity, discarded
/// the REMOVE's `ChangeInfo`, and answered `Removed` regardless — so the unlink of
/// somebody else's object was reported as the caller's. REMOVE is a pathname
/// operation and a name is not an object; no COMPOUND can make it one, because
/// [RFC 7530 §14.2](https://www.rfc-editor.org/rfc/rfc7530.html#section-14.2)
/// guarantees order but not atomicity. What the server *can* answer is whether
/// its directory moved, and that is what the fix reads.
#[test]
fn f15_a_replacement_between_the_probe_and_the_remove_is_not_a_pinned_removal() {
    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, Some(0));
    let root = walk(&mut fake, run_id, &[b"root"]);
    fake.insert_file(&root, b"doomed.txt", b"mine".to_vec());

    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(ReplaceBeforeRemove {
            inner: fake,
            fired: false,
            parent: root,
            victim: b"doomed.txt".to_vec(),
        }),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    open_existing(&mut storage, run_id).expect("the run opens");

    let epoch = storage
        .admission()
        .expect("a run is open")
        .admitted()
        .epoch();
    let refused = storage
        .execute(&umbra_core::StorageRequest {
            context: umbra_core::RequestContext {
                run_id,
                operation_id: umbra_core::OperationId(Uuid::new_v4()),
                idempotency_key: umbra_core::IdempotencyKey("raced-unlink".into()),
                writer_epoch: Some(epoch),
            },
            operation: umbra_core::StorageOperation::Unlink {
                path: path(b"doomed.txt"),
            },
        })
        .expect_err("a removal whose target cannot be proven is not a success");
    assert!(
        refused
            .context
            .contains("cannot be shown to be the object the caller pinned"),
        "the diagnosis must say what could not be proven: {}",
        refused.context
    );

    // The run stopped: a fresh, entirely unrelated mutation is refused with the
    // original diagnosis carried forward.
    let stopped = storage
        .execute(&umbra_core::StorageRequest {
            context: umbra_core::RequestContext {
                run_id,
                operation_id: umbra_core::OperationId(Uuid::new_v4()),
                idempotency_key: umbra_core::IdempotencyKey("after-raced-unlink".into()),
                writer_epoch: Some(epoch),
            },
            operation: umbra_core::StorageOperation::Create {
                path: path(b"unrelated.txt"),
                options: umbra_core::CreateOptions {
                    kind: umbra_core::CreateKind::File,
                    mode: 0o644,
                },
            },
        })
        .expect_err("a run holding an unprovable removal admits no new mutation");
    assert_eq!(stopped.kind, ErrorKind::InvalidState);
    assert!(
        stopped.context.contains("this run is stopped"),
        "the refusal must name the state: {}",
        stopped.context
    );

    // And no clean release follows: the marker stays held.
    let lease = storage.admission().expect("admitted").lease();
    let release = storage
        .release_writer(&lease)
        .expect_err("a release over an unprovable removal is not clean");
    assert_eq!(release.kind, ErrorKind::LeaseLost);
    assert!(
        storage.admission().is_some(),
        "a refused release retains admission"
    );
}

/// **F15.** An undisturbed unlink still succeeds. The evidence check is a proof
/// obligation, not a new refusal of ordinary pathname removal.
#[test]
fn f15_an_undisturbed_unlink_is_still_a_clean_removal() {
    let run_id = fresh_run();
    let mut fake = seed_released_run(run_id, Some(0));
    let root = walk(&mut fake, run_id, &[b"root"]);
    fake.insert_file(&root, b"quiet.txt", b"mine".to_vec());
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let epoch = storage
        .admission()
        .expect("a run is open")
        .admitted()
        .epoch();
    let removed = storage
        .execute(&umbra_core::StorageRequest {
            context: umbra_core::RequestContext {
                run_id,
                operation_id: umbra_core::OperationId(Uuid::new_v4()),
                idempotency_key: umbra_core::IdempotencyKey("quiet-unlink".into()),
                writer_epoch: Some(epoch),
            },
            operation: umbra_core::StorageOperation::Unlink {
                path: path(b"quiet.txt"),
            },
        })
        .expect("an undisturbed unlink removes the name it proved");
    assert!(matches!(removed, umbra_core::StorageResponse::Unlinked));

    // And the run carries on.
    storage
        .execute(&umbra_core::StorageRequest {
            context: umbra_core::RequestContext {
                run_id,
                operation_id: umbra_core::OperationId(Uuid::new_v4()),
                idempotency_key: umbra_core::IdempotencyKey("after-quiet".into()),
                writer_epoch: Some(epoch),
            },
            operation: umbra_core::StorageOperation::Create {
                path: path(b"next.txt"),
                options: umbra_core::CreateOptions {
                    kind: umbra_core::CreateKind::File,
                    mode: 0o644,
                },
            },
        })
        .expect("nothing about a proven removal stops the run");
}

// --- F17: a caller's page limit does not size an eager allocation ------------

/// **F17.** A page limit is a request, not a measurement of the directory.
///
/// `page` reserved `limit` `DirectoryEntry` slots before issuing a single
/// READDIR, so a caller asking for a large page against a three-entry directory
/// paid for the request rather than for the answer — and `u32::MAX` asks for
/// billions of slots, tens of gigabytes, on an empty directory.
///
/// The wire request was already bounded by `max_reply_bytes`, so no page can
/// return more than `MAX_SERVER_PAGES` replies' worth of entries however large
/// the limit is. The reservation is now bounded by what can actually arrive, and
/// the vector still grows on demand if it does.
#[test]
fn f17_a_huge_page_limit_does_not_reserve_a_huge_allocation() {
    use std::sync::atomic::Ordering;
    use umbra_storage_nfs_userspace::identity::PinnedObject;
    use umbra_storage_nfs_userspace::pages::NoiseFilter;

    let mut fake = FakeTransport::new();
    let root = fake.root();
    let listed = fake.insert_directory(&root, b"small");
    for entry in [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()] {
        fake.insert_file(&listed, entry, b"x".to_vec());
    }
    let pinned = PinnedObject::pin(&mut fake, listed, deadline()).expect("pin the directory");

    // Two million entries is well past anything this directory holds, and past
    // anything the transport's 1 MiB reply bound could carry across four server
    // pages. It is chosen to be large enough to dominate every other allocation
    // in this binary and small enough that the pre-fix reservation is *recorded*
    // rather than aborting the process, which a `u32::MAX` reservation would.
    let limit = 2_000_000_u32;
    let entry_bytes = std::mem::size_of::<umbra_core::DirectoryEntry>();

    LARGEST.store(0, Ordering::Relaxed);
    let page = umbra_storage_nfs_userspace::pages::page(
        &mut fake,
        &pinned,
        None,
        limit,
        &NoiseFilter::default(),
        deadline(),
    )
    .expect("a huge limit is a legal request");
    let largest = LARGEST.load(Ordering::Relaxed);

    // The answer is still correct and complete.
    let names: Vec<Vec<u8>> = page
        .entries
        .iter()
        .map(|entry| entry.name.as_bytes().to_vec())
        .collect();
    assert_eq!(
        names,
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
        "every entry the directory holds is returned, in order"
    );
    assert!(page.next.is_none(), "a three-entry directory is exhausted");

    // And it did not cost the request. The pre-fix reservation alone would be
    // `limit * size_of::<DirectoryEntry>()`, hundreds of megabytes.
    let refused_bound = 16 * 1024 * 1024;
    assert!(
        largest < refused_bound,
        "the largest single allocation was {largest} bytes; a reservation sized by the \
         caller's limit would be about {} bytes",
        limit as usize * entry_bytes
    );
}

/// **F17.** An ordinary limit still reserves for an ordinary page, and paging
/// across server pages still works.
#[test]
fn f17_ordinary_paging_is_unchanged() {
    use umbra_storage_nfs_userspace::identity::PinnedObject;
    use umbra_storage_nfs_userspace::pages::NoiseFilter;

    let mut fake = FakeTransport::new();
    let root = fake.root();
    let listed = fake.insert_directory(&root, b"many");
    for index in 0..10u32 {
        fake.insert_file(&listed, format!("f{index:02}").as_bytes(), b"x".to_vec());
    }
    let pinned = PinnedObject::pin(&mut fake, listed, deadline()).expect("pin the directory");

    let first = umbra_storage_nfs_userspace::pages::page(
        &mut fake,
        &pinned,
        None,
        4,
        &NoiseFilter::default(),
        deadline(),
    )
    .expect("a small page");
    assert_eq!(first.entries.len(), 4, "the caller's limit still bounds it");
    let cursor = first.next.expect("more entries remain");

    let second = umbra_storage_nfs_userspace::pages::page(
        &mut fake,
        &pinned,
        Some(&cursor),
        4,
        &NoiseFilter::default(),
        deadline(),
    )
    .expect("the continuation");
    assert_eq!(second.entries.len(), 4);
    assert_ne!(
        first.entries[0].name, second.entries[0].name,
        "the cursor advanced"
    );
}

// --- F18 / F31: a committed OPEN is released, not leaked --------------------

/// Records the sequenced operations that reached the wire.
#[derive(Clone, Debug, Default)]
struct WireOps(std::sync::Arc<std::sync::Mutex<Vec<(&'static str, u32)>>>);

impl WireOps {
    fn seen(&self) -> Vec<(&'static str, u32)> {
        self.0.lock().expect("not poisoned").clone()
    }
}

/// A transport that records sequenced operations and can strip identity from the
/// `getattr` that follows an OPEN.
struct Sequenced {
    inner: FakeTransport,
    ops: WireOps,
    /// Drop FSID/FILEID from `getattr` replies, which is what makes
    /// `object_identity` fail after a committed OPEN.
    blind_identity: bool,
    /// Fail the CLOSE with this status instead of letting it through.
    refuse_close: Option<umbra_storage_nfs_userspace::error::Nfs4Status>,
}

impl RawTransport for Sequenced {
    fn wire_profile(&self) -> umbra_storage_nfs_userspace::transport::WireProfile {
        self.inner.wire_profile()
    }
    fn limits(&self) -> umbra_storage_nfs_userspace::transport::TransportLimits {
        self.inner.limits()
    }
    fn connection(&self) -> umbra_storage_nfs_userspace::transport::ConnectionState {
        self.inner.connection()
    }
    fn submit(
        &mut self,
        call: umbra_storage_nfs_userspace::transport::Compound,
        call_deadline: Deadline,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::CompoundReply,
    > {
        use umbra_storage_nfs_userspace::transport::{Nfs4Op, OpReply};
        {
            let mut seen = self.ops.0.lock().expect("not poisoned");
            for op in &call.ops {
                match op {
                    Nfs4Op::Open(args) => seen.push(("OPEN", args.seqid)),
                    Nfs4Op::OpenConfirm { seqid, .. } => seen.push(("OPEN_CONFIRM", *seqid)),
                    Nfs4Op::Close { seqid, .. } => seen.push(("CLOSE", *seqid)),
                    _ => {}
                }
            }
        }
        if let Some(status) = self.refuse_close {
            if call.ops.iter().any(|op| matches!(op, Nfs4Op::Close { .. })) {
                return Ok(umbra_storage_nfs_userspace::transport::CompoundReply {
                    tag: call.tag.clone(),
                    results: Vec::new(),
                    failure: Some(umbra_storage_nfs_userspace::error::ProtocolError {
                        status,
                        op: umbra_storage_nfs_userspace::transport::OpCode::Close,
                        index: 1,
                    }),
                });
            }
        }
        let mut reply = self.inner.submit(call, call_deadline)?;
        if self.blind_identity {
            for result in &mut reply.results {
                if let OpReply::GetAttr(attributes) = result {
                    attributes.fsid = None;
                    attributes.fileid = None;
                }
            }
        }
        Ok(reply)
    }
    fn cancel(
        &mut self,
        token: umbra_storage_nfs_userspace::transport::CallToken,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::Retirement,
    > {
        self.inner.cancel(token)
    }
    fn reconnect(
        &mut self,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::ConnectionEpoch,
    > {
        self.inner.reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn umbra_storage_nfs_userspace::transport::FaultPlan>) {
        self.inner.install_faults(plan)
    }
}

fn registry(label: u64) -> umbra_storage_nfs_userspace::state::open_owner::OpenOwnerRegistry {
    umbra_storage_nfs_userspace::state::open_owner::OpenOwnerRegistry::new(
        umbra_storage_nfs_userspace::handle::Session::establish(
            umbra_storage_nfs_userspace::handle::SessionId(label),
            umbra_storage_nfs_userspace::handle::ClientId(label),
            umbra_storage_nfs_userspace::transport::ConnectionEpoch(1),
        ),
        format!("cr-{label}").into_bytes(),
    )
}

/// **F18.** An OPEN that committed and then could not be identified is released,
/// not left on the server.
///
/// The candidate burned the owner and dropped the reply, on the reasoning that
/// "the server releases it when the lease expires". It does not: this client goes
/// on renewing the very lease that keeps the state alive, so the open survives as
/// long as the session does. Both the handle and the stateid the OPEN returned
/// are in hand, so the state is released explicitly — OPEN_CONFIRM first, because
/// the server asked for it and an unconfirmed stateid is not one a CLOSE may
/// carry.
///
/// The answer is unchanged: the identity is still unproven, so this is still
/// `Abandoned` and the owner is still burned.
#[test]
fn f18_a_committed_open_whose_identity_fails_is_closed_on_the_server() {
    use umbra_storage_nfs_userspace::state::open_owner::{OpenOutcome, OpenRequest};
    use umbra_storage_nfs_userspace::transport::{OpenHow, ShareAccess, ShareDeny};

    let mut fake = FakeTransport::new();
    let root = fake.root();
    fake.insert_file(&root, b"opened", b"bytes".to_vec());
    let ops = WireOps::default();
    let mut transport = Sequenced {
        inner: fake,
        ops: ops.clone(),
        blind_identity: true,
        refuse_close: None,
    };

    let mut owners = registry(1);
    let lease = owners.allocate().expect("a fresh registry mints leases");
    let outcome = owners.open(
        lease,
        &mut transport,
        &OpenRequest {
            parent: root.clone(),
            name: name(b"opened"),
            how: OpenHow::NoCreate,
            share_access: ShareAccess::READ,
            share_deny: ShareDeny::NONE,
        },
        deadline(),
    );

    assert!(
        matches!(outcome, OpenOutcome::Abandoned { .. }),
        "an unproven identity is still abandoned: {outcome:?}"
    );
    assert_eq!(
        owners.burned(),
        1,
        "and the owner is still burned; cleanup does not resurrect authority"
    );

    // The wire says the state was released, with the seqids the owner owed.
    // A fresh owner opens at 0, so the confirm carries 1 and the close 2.
    assert_eq!(
        ops.seen(),
        vec![("OPEN", 0), ("OPEN_CONFIRM", 1), ("CLOSE", 2)],
        "a committed OPEN must be confirmed and closed, in that order, correctly sequenced"
    );
}

/// **F18.** A cleanup the server refuses changes nothing about the answer.
#[test]
fn f18_a_refused_cleanup_still_reports_the_original_identity_failure() {
    use umbra_storage_nfs_userspace::error::Nfs4Status;
    use umbra_storage_nfs_userspace::state::open_owner::{OpenOutcome, OpenRequest};
    use umbra_storage_nfs_userspace::transport::{OpenHow, ShareAccess, ShareDeny};

    let mut fake = FakeTransport::new();
    let root = fake.root();
    fake.insert_file(&root, b"opened", b"bytes".to_vec());
    let ops = WireOps::default();
    let mut transport = Sequenced {
        inner: fake,
        ops: ops.clone(),
        blind_identity: true,
        refuse_close: Some(Nfs4Status::SERVERFAULT),
    };

    let mut owners = registry(2);
    let lease = owners.allocate().expect("lease");
    let outcome = owners.open(
        lease,
        &mut transport,
        &OpenRequest {
            parent: root.clone(),
            name: name(b"opened"),
            how: OpenHow::NoCreate,
            share_access: ShareAccess::READ,
            share_deny: ShareDeny::NONE,
        },
        deadline(),
    );

    let OpenOutcome::Abandoned { error } = outcome else {
        panic!("an unproven identity is still abandoned");
    };
    assert!(
        error.to_string().contains("identity cannot be proven"),
        "the original identity failure is what the caller reads, not the CLOSE's: {error}"
    );
    assert_eq!(owners.burned(), 1);
    assert!(
        ops.seen().contains(&("CLOSE", 2)),
        "the cleanup was still attempted: {:?}",
        ops.seen()
    );
}

/// **F31.** A reclaim that returns a different object is closed before it is
/// surrendered.
///
/// `CLAIM_PREVIOUS` succeeded — the server minted open state for this owner and
/// handed back a stateid — but it named the wrong object, so it cannot be carried
/// forward. The candidate dropped the `OpenFile`, and `OpenFile::drop` sends
/// nothing: CLOSE is a wire operation this module dispatches explicitly. The
/// state stayed alive under a client that keeps renewing its lease.
///
/// `IdentityChanged` stays the reported cause; the CLOSE is cleanup, not the
/// reason.
#[test]
fn f31_an_identity_mismatched_reclaim_is_closed_before_it_is_surrendered() {
    use umbra_storage_nfs_userspace::handle::ObjectIdentity;
    use umbra_storage_nfs_userspace::state::reclaim::{ReclaimPlan, ReclaimTarget, SurrenderCause};
    use umbra_storage_nfs_userspace::transport::Fsid;
    use umbra_storage_nfs_userspace::transport::{AttrMask, ShareAccess, ShareDeny};

    let mut fake = FakeTransport::new();
    let root = fake.root();
    fake.insert_file(&root, b"reclaimed", b"bytes".to_vec());
    // CLAIM_PREVIOUS is only answered inside the server's grace window.
    fake.set_grace(true);
    let (handle, _) = fake
        .lookup(&root, &name(b"reclaimed"), AttrMask::STAT, deadline())
        .expect("resolve the target");

    let ops = WireOps::default();
    let mut transport = Sequenced {
        inner: fake,
        ops: ops.clone(),
        blind_identity: false,
        refuse_close: None,
    };

    // A target pinned to an identity the server will not answer with: the
    // reclaim succeeds and comes back as somebody else's object.
    let report = ReclaimPlan::from_targets(vec![ReclaimTarget {
        handle,
        identity: ObjectIdentity {
            fsid: Fsid {
                major: 0xDEAD,
                minor: 0xBEEF,
            },
            fileid: 0xFFFF_FFFF,
        },
        share_access: ShareAccess::READ,
        share_deny: ShareDeny::NONE,
    }])
    .run(&mut registry(3), &mut transport, deadline());

    assert!(
        report.recovered.is_empty(),
        "the wrong object is not recovered"
    );
    assert_eq!(report.surrendered.len(), 1);
    assert_eq!(
        report.surrendered[0].cause,
        SurrenderCause::IdentityChanged,
        "the identity is why it was surrendered, whatever the CLOSE said"
    );

    let closes: Vec<_> = ops
        .seen()
        .into_iter()
        .filter(|(op, _)| *op == "CLOSE")
        .collect();
    assert_eq!(
        closes.len(),
        1,
        "exactly one CLOSE, for the open the reclaim actually minted: {:?}",
        ops.seen()
    );
    assert_eq!(
        ops.seen(),
        vec![("OPEN", 0), ("OPEN_CONFIRM", 1), ("CLOSE", 2)],
        "and it carries the seqid the owner owed"
    );
}

/// **F31.** A cleanup the server refuses is retained as evidence beside the
/// identity mismatch, never reported as a clean release.
#[test]
fn f31_a_failed_cleanup_is_retained_beside_the_identity_change() {
    use umbra_storage_nfs_userspace::error::Nfs4Status;
    use umbra_storage_nfs_userspace::handle::ObjectIdentity;
    use umbra_storage_nfs_userspace::state::reclaim::{ReclaimPlan, ReclaimTarget, SurrenderCause};
    use umbra_storage_nfs_userspace::transport::Fsid;
    use umbra_storage_nfs_userspace::transport::{AttrMask, ShareAccess, ShareDeny};

    let mut fake = FakeTransport::new();
    let root = fake.root();
    fake.insert_file(&root, b"reclaimed", b"bytes".to_vec());
    fake.set_grace(true);
    let (handle, _) = fake
        .lookup(&root, &name(b"reclaimed"), AttrMask::STAT, deadline())
        .expect("resolve the target");

    let mut transport = Sequenced {
        inner: fake,
        ops: WireOps::default(),
        blind_identity: false,
        refuse_close: Some(Nfs4Status::SERVERFAULT),
    };

    let report = ReclaimPlan::from_targets(vec![ReclaimTarget {
        handle,
        identity: ObjectIdentity {
            fsid: Fsid {
                major: 0xDEAD,
                minor: 0xBEEF,
            },
            fileid: 0xFFFF_FFFF,
        },
        share_access: ShareAccess::READ,
        share_deny: ShareDeny::NONE,
    }])
    .run(&mut registry(4), &mut transport, deadline());

    assert_eq!(report.surrendered[0].cause, SurrenderCause::IdentityChanged);
    let retained = report.surrendered[0]
        .error
        .as_ref()
        .expect("the cleanup failure is retained as evidence");
    assert_eq!(retained.status(), Some(Nfs4Status::SERVERFAULT));
}

// --- F12: partial journal preparation does not conflict with its own retry ---

/// Fails the first `GUARDED4` OPEN that creates a `.provider/retries` record
/// whose name starts with `prefix`.
///
/// `definite` chooses between the two failure shapes that matter: a server status
/// (the operation provably did not happen) and a lost connection (its disposition
/// is unknown). The whole point of F12's accepted scope is that only the first
/// may be resumed.
struct FailRecordCreate {
    inner: FakeTransport,
    prefix: &'static str,
    definite: bool,
    fired: bool,
}

impl RawTransport for FailRecordCreate {
    fn wire_profile(&self) -> umbra_storage_nfs_userspace::transport::WireProfile {
        self.inner.wire_profile()
    }
    fn limits(&self) -> umbra_storage_nfs_userspace::transport::TransportLimits {
        self.inner.limits()
    }
    fn connection(&self) -> umbra_storage_nfs_userspace::transport::ConnectionState {
        self.inner.connection()
    }
    fn submit(
        &mut self,
        call: umbra_storage_nfs_userspace::transport::Compound,
        call_deadline: Deadline,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::CompoundReply,
    > {
        use umbra_storage_nfs_userspace::transport::{Nfs4Op, OpenClaim, OpenHow};
        let creates = call.ops.iter().any(|op| match op {
            Nfs4Op::Open(args) => {
                let named = match &args.claim {
                    OpenClaim::Null { name } => {
                        String::from_utf8_lossy(name.as_bytes()).starts_with(self.prefix)
                    }
                    OpenClaim::Previous { .. } => false,
                };
                named && matches!(args.how, OpenHow::Guarded { .. })
            }
            _ => false,
        });
        if creates && !self.fired {
            self.fired = true;
            if self.definite {
                return Ok(umbra_storage_nfs_userspace::transport::CompoundReply {
                    tag: call.tag.clone(),
                    results: Vec::new(),
                    failure: Some(umbra_storage_nfs_userspace::error::ProtocolError {
                        status: umbra_storage_nfs_userspace::error::Nfs4Status::ACCESS,
                        op: umbra_storage_nfs_userspace::transport::OpCode::Open,
                        index: 1,
                    }),
                });
            }
            return Err(
                umbra_storage_nfs_userspace::error::TransportError::Disconnected {
                    epoch: umbra_storage_nfs_userspace::transport::ConnectionEpoch(1),
                    detail: "the record create's reply was lost".into(),
                },
            );
        }
        self.inner.submit(call, call_deadline)
    }
    fn cancel(
        &mut self,
        token: umbra_storage_nfs_userspace::transport::CallToken,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::Retirement,
    > {
        self.inner.cancel(token)
    }
    fn reconnect(
        &mut self,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<
        umbra_storage_nfs_userspace::transport::ConnectionEpoch,
    > {
        self.inner.reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn umbra_storage_nfs_userspace::transport::FaultPlan>) {
        self.inner.install_faults(plan)
    }
}

fn create_request(
    run_id: RunId,
    epoch: umbra_core::LeaseEpoch,
    key: &str,
    file: &[u8],
) -> umbra_core::StorageRequest {
    umbra_core::StorageRequest {
        context: umbra_core::RequestContext {
            run_id,
            operation_id: umbra_core::OperationId(Uuid::new_v4()),
            idempotency_key: umbra_core::IdempotencyKey(key.into()),
            writer_epoch: Some(epoch),
        },
        operation: umbra_core::StorageOperation::Create {
            path: path(file),
            options: umbra_core::CreateOptions {
                kind: umbra_core::CreateKind::File,
                mode: 0o644,
            },
        },
    }
}

fn retry_names(storage: &mut NfsUserspaceStorage, run_id: RunId) -> Vec<String> {
    let transport = storage.transport().expect("transport");
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
                    attrs: umbra_storage_nfs_userspace::transport::AttrMask::STAT,
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

/// **F12.** A definite failure part way through preparation does not make the
/// identical request permanently unadmittable.
///
/// Preparation is three `GUARDED4` creates — sidecar, index, intent — and a
/// server status part way through — here `NFS4ERR_ACCESS`, chosen because it is
/// definite without also latching the run the way an I/O failure does — leaves
/// the earlier ones standing with nothing dispatched. The candidate then refused
/// the identical retry with its own work:
/// the index it wrote was read back as another key's claim, and the sidecar it
/// wrote was answered `NFS4ERR_EXIST`. Neither is a conflict.
///
/// Only *definite* failures are resumed. The lost-reply control below is the half
/// that must stay blocked.
#[test]
fn f12_a_definite_failure_after_the_sidecar_does_not_block_the_identical_retry() {
    for prefix in [layout::RETRY_OP_PREFIX, layout::RETRY_KEY_PREFIX] {
        let run_id = fresh_run();
        let mut storage = NfsUserspaceStorage::with_facades(
            config(),
            Box::new(FailRecordCreate {
                inner: seed_released_run(run_id, Some(0)),
                prefix,
                definite: true,
                fired: false,
            }),
            Box::new(FakeReplayLog::default()),
        )
        .expect("provider");
        open_existing(&mut storage, run_id).expect("the run opens");
        let epoch = storage
            .admission()
            .expect("a run is open")
            .admitted()
            .epoch();

        let request = create_request(run_id, epoch, "half-prepared", b"half-prepared.txt");
        let refused = storage
            .execute(&request)
            .expect_err("the injected NFS4ERR_ACCESS surfaces");
        assert_eq!(
            refused.kind,
            ErrorKind::Denied,
            "{prefix}: a definite server status, not an unknown disposition: {refused:?}"
        );

        // Nothing was dispatched against the target: only preparation ran.
        let held = retry_names(&mut storage, run_id);
        assert!(
            held.iter().any(|name| name.starts_with("pre-")),
            "{prefix}: the sidecar landed before the failure: {held:?}"
        );
        assert!(
            !held.iter().any(|name| name.starts_with(prefix)),
            "{prefix}: the record the failure hit did not land: {held:?}"
        );
        let probe = storage.execute(&umbra_core::StorageRequest {
            context: umbra_core::RequestContext {
                run_id,
                operation_id: umbra_core::OperationId(Uuid::new_v4()),
                idempotency_key: umbra_core::IdempotencyKey(format!("probe-{prefix}")),
                writer_epoch: None,
            },
            operation: umbra_core::StorageOperation::Stat {
                path: path(b"half-prepared.txt"),
            },
        });
        assert_eq!(
            probe
                .expect_err("nothing was dispatched against the target")
                .kind,
            ErrorKind::NotFound,
            "{prefix}: preparation must not have touched the namespace"
        );

        // The fault was one-shot, so the identical request may now be admitted.
        let created = storage.execute(&request).unwrap_or_else(|error| {
            panic!("{prefix}: the identical retry must be admitted: {error:?}")
        });
        assert!(matches!(created, umbra_core::StorageResponse::Created(_)));
        let after = retry_names(&mut storage, run_id);
        assert!(
            after
                .iter()
                .any(|name| name.starts_with(layout::RETRY_KEY_PREFIX)),
            "{prefix}: preparation completed on the retry: {after:?}"
        );
    }
}

/// **F12, negative control.** A *lost* reply during preparation stays blocked.
///
/// This is the half the bot's broader remedy would have broken. When the record
/// create's disposition is unknown, a `GUARDED4` create may still land after the
/// absence was observed, so matching preparation is not evidence that nothing is
/// outstanding. The provider's unresolved-call ledger holds an `IntentWrite`
/// obligation for the key, and only that key's own settled record discharges it —
/// never absence alone.
#[test]
fn f12_a_lost_reply_during_preparation_is_still_blocked() {
    let run_id = fresh_run();
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(FailRecordCreate {
            inner: seed_released_run(run_id, Some(0)),
            prefix: layout::RETRY_KEY_PREFIX,
            definite: false,
            fired: false,
        }),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    open_existing(&mut storage, run_id).expect("the run opens");
    let epoch = storage
        .admission()
        .expect("a run is open")
        .admitted()
        .epoch();

    let request = create_request(run_id, epoch, "lost-intent", b"lost-intent.txt");
    let lost = storage
        .execute(&request)
        .expect_err("the lost reply surfaces");
    assert_eq!(
        lost.kind,
        ErrorKind::StorageUnavailable,
        "the disposition must be unknown, or this is not the control it claims to be"
    );

    // The identical request is refused: absence of the intent record does not
    // exclude a create that may still land.
    let blocked = storage
        .execute(&request)
        .expect_err("an unknown intent write is not resumable by matching preparation");
    assert_eq!(blocked.kind, ErrorKind::InvalidState);
    assert!(
        blocked.context.contains("disposition is unknown"),
        "the refusal must be the unresolved-call gate: {}",
        blocked.context
    );

    // And so is unrelated work, and the release.
    let unrelated = storage
        .execute(&create_request(
            run_id,
            epoch,
            "unrelated",
            b"unrelated.txt",
        ))
        .expect_err("the run admits no new mutation either");
    assert_eq!(unrelated.kind, ErrorKind::InvalidState);
    let lease = storage.admission().expect("admitted").lease();
    assert_eq!(
        storage
            .release_writer(&lease)
            .expect_err("no clean release over an unknown intent write")
            .kind,
        ErrorKind::LeaseLost
    );
}
