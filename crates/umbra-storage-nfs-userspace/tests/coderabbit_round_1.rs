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
