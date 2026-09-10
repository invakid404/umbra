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
