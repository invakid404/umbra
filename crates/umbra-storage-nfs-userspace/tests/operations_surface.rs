//! M1 acceptance for the storage operations surface, driven over the
//! `raw_rpc` / `raw_state` integration seam.
//!
//! Every scenario here runs the same [`Operations`] code a live provider would,
//! over a [`StateSession`] carrying a **real** client incarnation: SETCLIENTID,
//! SETCLIENTID_CONFIRM, open-owner minting, OPEN_CONFIRM sequencing and CLOSE all
//! happen as they would on the wire. Only the transport is the in-memory shape
//! fake, and [`Backend::is_live`] is asserted false so no cell of this file can be
//! mistaken for a live-server result.
//!
//! # What is proven here
//!
//! * object identity is stable across a rename and distinct after a replacement;
//! * an in-place edit by another writer is visible through an open handle;
//! * a pathname replacement leaves open handles on the original object and does
//!   not retarget them, while a fresh open finds the replacement;
//! * an object whose last name is gone stays readable through an open handle
//!   until CLOSE;
//! * READDIR is bounded and its cursor is invalidated explicitly, never silently
//!   restarted or answered from a cached snapshot;
//! * WRITE stability is reported as it happened, and an `UNSTABLE` write is
//!   durable only when COMMIT returns the verifier the WRITE did;
//! * capabilities M1 does not offer are refused with a typed error, and
//!   capabilities the syscall matrix authorises but the frozen transport cannot
//!   encode name their owner instead of claiming to work.
//!
//! # What is explicitly out of scope
//!
//! `kqueue`/`kevent` with `EVFILT_VNODE` and FSEvents are **out of scope** by the
//! pinned `docs/design/syscall-matrix.md` notifications decision: NFS reads do not
//! yield a complete change-event stream, and no local mirror is manufactured.
//! Nothing in this crate registers, delivers or emulates a file-change
//! notification, and [`notifications_are_out_of_scope_by_design`] asserts the
//! capability table records that as a decision rather than an omission.
//!
//! No live Ganesha is started, no NFS mount is touched, and no host path outside
//! this repository is read or written.

use std::sync::{Arc, Mutex};

use umbra_core::{
    BytePath, CreateKind, CreateOptions, ErrorKind, IdempotencyKey, ImmutableBaseContract,
    LeaseEpoch, ListCursor, ObjectKind, OpenRunIntent, OpenRunRequest, OperationId, RenameMode,
    RequestContext, RunId, StorageAnchor, StorageOperation, StoragePath, StoragePolicy,
    StorageRequest, StorageResponse,
};
use umbra_storage_nfs_userspace::anchor::component;
use umbra_storage_nfs_userspace::capability::{Support, CONTRACT_SURFACE, OUT_OF_SURFACE};
use umbra_storage_nfs_userspace::crud::{CreateDisposition, MutationIdentity, OpenObject, WriteAt};
use umbra_storage_nfs_userspace::error::{FacadeResult, ReplayError};
use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport, ScriptedFault};
use umbra_storage_nfs_userspace::handle::{FileHandle, ObjectIdentity};
use umbra_storage_nfs_userspace::identity::PinnedObject;
use umbra_storage_nfs_userspace::integration::{identity_for, Backend, StateSession};
use umbra_storage_nfs_userspace::namespace::dispatch::TransportDispatcher;
use umbra_storage_nfs_userspace::namespace::{
    NamespaceDispatcher, NamespaceEffect, NamespaceMutation, NamespaceOutcome,
};
use umbra_storage_nfs_userspace::ops::{MutationContext, Operations, OpsContext};
use umbra_storage_nfs_userspace::replay::VerifierMatch;
use umbra_storage_nfs_userspace::state::open_owner::CloseOutcome;
use umbra_storage_nfs_userspace::state::verifier::create_verifier_for;
use umbra_storage_nfs_userspace::storage::{NfsUserspaceConfig, FORMAT_VERSION};
use umbra_storage_nfs_userspace::transport::{
    Compound, CompoundReply, ConnectionEpoch, ConnectionState, Deadline, DirVerifier, FaultAction,
    FaultPlan, FaultPoint, RawTransport, ShareAccess, Stability, TransportLimits, TransportResult,
    Verifier, WireProfile, WriteVerifier,
};
use uuid::Uuid;

// --- Harness -----------------------------------------------------------------

const EXPORT: &[u8] = b"exports/umbra";
const RUN_PARENT: &[u8] = b"runs";
const RUN_UUID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

fn deadline() -> Deadline {
    Deadline { millis: 5_000 }
}

fn run_id() -> RunId {
    RunId(Uuid::parse_str(RUN_UUID).expect("pinned run uuid"))
}

fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        // The reuse key's reserved loopback port. Nothing here opens a socket;
        // the number is recorded so a fixture identity is never borrowed from
        // another role by accident.
        port: 12108,
        export: BytePath::new(EXPORT).expect("export"),
        run_parent: BytePath::new(RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: deadline(),
    }
}

/// Handles of the seeded layout, for a test that plays the external writer.
struct Layout {
    root: FileHandle,
}

/// A fake server holding one umbra run layout.
fn seeded() -> (FakeTransport, Layout) {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    current = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&current, RUN_UUID.as_bytes());
    let root = fake.insert_directory(&run, b"root");
    fake.insert_directory(&run, b"control");
    fake.insert_directory(&run, b".provider");
    (fake, Layout { root })
}

fn open_run_request() -> OpenRunRequest {
    OpenRunRequest {
        run_id: run_id(),
        intent: OpenRunIntent::OpenExisting,
        immutable_base: ImmutableBaseContract {
            identity: "umbra-m1-storage-ops".into(),
            fingerprint: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: FORMAT_VERSION,
        },
    }
}

fn request_context(key: &str, epoch: Option<u64>) -> RequestContext {
    RequestContext {
        run_id: run_id(),
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: epoch.map(LeaseEpoch),
    }
}

fn path(bytes: &[u8]) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec()).expect("path")
}

/// Establish a real client incarnation over the supplied transport.
///
/// This is the whole SETCLIENTID / SETCLIENTID_CONFIRM lifecycle plus the lease
/// clock, running through `integration.rs` exactly as a live session would.
fn established(session: &mut StateSession) {
    let root = session.root(deadline()).expect("the export root resolves");
    session
        .establish_and_adopt(&root, 0, deadline())
        .expect("a client incarnation is established");
}

/// A `FakeTransport` two owners can drive: the session, and an external writer.
///
/// `StateSession::over_fake` takes the fake by value, which is right for a
/// scenario that needs no outside actor. The coherence scenarios need one — an
/// edit or a replacement that Umbra did not perform — so those go through
/// `StateSession::new`, the injection point `integration.rs` documents, with this
/// shared delegate. It adds no behaviour: every method forwards.
#[derive(Clone)]
struct SharedFake(Arc<Mutex<FakeTransport>>);

impl SharedFake {
    fn new(fake: FakeTransport) -> Self {
        Self(Arc::new(Mutex::new(fake)))
    }

    /// Act as an external writer against the same server.
    fn external<R>(&self, act: impl FnOnce(&mut FakeTransport) -> R) -> R {
        act(&mut self.0.lock().expect("the fake is not poisoned"))
    }
}

impl RawTransport for SharedFake {
    fn wire_profile(&self) -> WireProfile {
        self.0.lock().expect("lock").wire_profile()
    }
    fn limits(&self) -> TransportLimits {
        self.0.lock().expect("lock").limits()
    }
    fn connection(&self) -> ConnectionState {
        self.0.lock().expect("lock").connection()
    }
    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply> {
        self.0.lock().expect("lock").submit(call, deadline)
    }
    fn cancel(
        &mut self,
        token: umbra_storage_nfs_userspace::transport::CallToken,
    ) -> TransportResult<umbra_storage_nfs_userspace::transport::Retirement> {
        self.0.lock().expect("lock").cancel(token)
    }
    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.0.lock().expect("lock").reconnect()
    }
    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.0.lock().expect("lock").install_faults(plan)
    }
}

/// A namespace dispatcher that reports a scripted effect.
///
/// It stands in for the NFSv4.0 `RENAME`, `REMOVE`, `CREATE` and `SETATTR` the
/// frozen `Nfs4Op` cannot encode. It proves this node's rules — the identity a
/// rename must preserve, the outcome kind that must match — and it is **not**
/// evidence that any server performed the mutation.
struct StandIn {
    effect: Option<NamespaceEffect>,
    calls: usize,
}

impl NamespaceDispatcher for StandIn {
    fn dispatch(
        &mut self,
        _: &mut dyn RawTransport,
        mutation: &NamespaceMutation,
    ) -> FacadeResult<NamespaceOutcome> {
        self.calls += 1;
        let effect = || {
            self.effect
                .clone()
                .expect("this scenario scripted an effect")
        };
        Ok(match mutation {
            NamespaceMutation::Remove { .. } => NamespaceOutcome::Removed,
            NamespaceMutation::Rename { .. } => NamespaceOutcome::Renamed(effect()),
            NamespaceMutation::CreateDirectory { .. } => NamespaceOutcome::Created(effect()),
            NamespaceMutation::SetAttributes { .. } | NamespaceMutation::Truncate { .. } => {
                NamespaceOutcome::AttributesSet(effect())
            }
        })
    }
}

// --- Scenarios ---------------------------------------------------------------

#[test]
fn the_surface_runs_over_a_real_client_incarnation_and_is_never_mistaken_for_live() {
    let (fake, _) = seeded();
    let mut session =
        StateSession::over_fake(fake, identity_for("storage-ops", Verifier([0x11; 8])));
    established(&mut session);
    assert_eq!(session.backend(), Backend::Fake);
    assert!(
        !session.backend().is_live(),
        "a criterion met only under the fake is a shape check, not an acceptance result"
    );

    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");
    let binding = operations.binding();
    assert_eq!(binding.run_id, run_id());
    // A userspace client has no kernel-visible path, so it publishes none.
    assert_eq!(binding.root.physical_path, None);
    assert_eq!(binding.control.physical_path, None);

    let mut replay = FakeReplayLog::default();
    let key = IdempotencyKey("m1-create".into());
    let operation = OperationId(Uuid::new_v4());
    // Resolve the create verifier before splitting: the ledger and the
    // incarnation both live in `ProtocolState` and cannot be borrowed together.
    let verifier = session
        .state()
        .create_verifiers()
        .verifier_for(&key, operation);
    assert_eq!(verifier, create_verifier_for(operation));

    let (state, transport) = session.split();
    let owners = state
        .incarnation()
        .expect("an incarnation was adopted")
        .open_owners();
    // R1-007: an `EXCLUSIVE4` create applies the requested mode with a follow-up
    // SETATTR, which goes through the namespace seam, so an exclusive create needs
    // a dispatcher bound exactly as a directory create does.
    let mut dispatcher = TransportDispatcher::new();
    let mut ops = OpsContext {
        transport,
        replay: &mut replay,
        mutations: Some(MutationContext {
            owners,
            create_verifier: Some(verifier),
            namespace: Some(&mut dispatcher),
        }),
        deadline: deadline(),
    };

    let created = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: RequestContext {
                    idempotency_key: key.clone(),
                    operation_id: operation,
                    ..context_with_epoch()
                },
                operation: StorageOperation::Create {
                    path: path(b"journal"),
                    options: CreateOptions {
                        kind: CreateKind::File,
                        mode: 0o640,
                    },
                },
            },
        )
        .expect("an exclusive create");
    let StorageResponse::Created(created) = created else {
        panic!("a create answers with an object result");
    };
    assert_eq!(created.stat.kind, ObjectKind::File);

    let StorageResponse::WriteAt(count) = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-write", Some(4)),
                operation: StorageOperation::WriteAt {
                    path: path(b"journal"),
                    offset: 0,
                    bytes: b"committed bytes".to_vec(),
                },
            },
        )
        .expect("a write")
    else {
        panic!("a write answers with a count");
    };
    assert_eq!(count, 15);

    let StorageResponse::ReadAt(bytes) = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-read", None),
                operation: StorageOperation::ReadAt {
                    path: path(b"journal"),
                    offset: 0,
                    len: 64,
                },
            },
        )
        .expect("a read")
    else {
        panic!("a read answers with bytes");
    };
    assert_eq!(bytes, b"committed bytes");

    let StorageResponse::Stat(stat) = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-stat", None),
                operation: StorageOperation::Stat {
                    path: path(b"journal"),
                },
            },
        )
        .expect("a stat")
    else {
        panic!("a stat answers with a stat");
    };
    assert_eq!(stat.len, 15);
    assert_eq!(
        stat.object_id, created.stat.object_id,
        "one object keeps one contract identity"
    );
}

fn context_with_epoch() -> RequestContext {
    request_context("unused", Some(4))
}

#[test]
fn an_unstable_write_is_durable_only_when_the_commit_verifier_matches() {
    let (fake, _) = seeded();
    let mut session =
        StateSession::over_fake(fake, identity_for("storage-ops", Verifier([0x12; 8])));
    established(&mut session);
    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");
    let root = operations.anchors().root().pin().clone();

    let mut replay = FakeReplayLog::default();
    let (state, transport) = session.split();
    let owners = state.incarnation().expect("incarnation").open_owners();

    let file = OpenObject::open(
        owners,
        transport,
        &root,
        &component(b"unstable".to_vec()).expect("component"),
        CreateDisposition::CreateNew { mode: 0o600 },
        ShareAccess::BOTH,
        deadline(),
    )
    .expect("create");

    let identity = MutationIdentity::from_context(&request_context("m1-unstable", Some(4)))
        .expect("a writer epoch");
    let ticket = file
        .write(
            transport,
            &mut replay,
            &identity,
            WriteAt {
                offset: 0,
                stability: Stability::Unstable,
                data: b"payload".to_vec(),
            },
            deadline(),
        )
        .expect("write");
    assert!(ticket.needs_commit(), "UNSTABLE owes a COMMIT");
    assert_eq!(
        file.commit(transport, &replay, &ticket, deadline())
            .expect("commit"),
        VerifierMatch::Match
    );

    // A server restart rotates the write verifier, meaning it lost the unstable
    // data. The bytes must be rewritten from the retained payload, and saying so
    // is a typed replay error rather than a generic failure.
    let second = MutationIdentity::from_context(&request_context("m1-unstable-2", Some(4)))
        .expect("a writer epoch");
    // A server restart rotates the write verifier: the unstable data is gone. The
    // fake is owned by the session here, so the rotation is driven through the
    // transport's own fault hook rather than by reaching around it. The hook fires
    // as the WRITE returns, so the WRITE still reports the verifier that gets
    // recorded and the later COMMIT is the first call to see the new one — which
    // is exactly the shape of a restart between the two.
    transport.install_faults(ScriptedFault::once(
        FaultPoint::BeforeReturn,
        None,
        FaultAction::RotateVerifier(WriteVerifier([0x99; 8])),
    ));
    let ticket = file
        .write(
            transport,
            &mut replay,
            &second,
            WriteAt {
                offset: 16,
                stability: Stability::Unstable,
                data: b"more".to_vec(),
            },
            deadline(),
        )
        .expect("write");
    let error = file
        .commit(transport, &replay, &ticket, deadline())
        .unwrap_err();
    assert!(
        matches!(
            error,
            umbra_storage_nfs_userspace::error::FacadeError::Replay(
                ReplayError::VerifierChanged { .. }
            )
        ),
        "a changed verifier is reported as lost unstable data, not as success"
    );
}

#[test]
fn object_identity_is_stable_across_a_rename_and_a_copy_is_refused() {
    let (fake, _) = seeded();
    let mut session =
        StateSession::over_fake(fake, identity_for("storage-ops", Verifier([0x13; 8])));
    established(&mut session);
    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");
    let root = operations.anchors().root().pin().clone();

    let mut replay = FakeReplayLog::default();
    let (state, transport) = session.split();
    let owners = state.incarnation().expect("incarnation").open_owners();

    let held = OpenObject::open(
        owners,
        transport,
        &root,
        &component(b"before".to_vec()).expect("component"),
        CreateDisposition::CreateNew { mode: 0o600 },
        ShareAccess::BOTH,
        deadline(),
    )
    .expect("create");
    let pinned = held.identity();
    let pinned_handle = held.handle().clone();

    // A dispatcher reporting the same object is the only answer a rename may give.
    let mut honest = StandIn {
        effect: Some(NamespaceEffect {
            identity: pinned,
            handle: pinned_handle.clone(),
        }),
        calls: 0,
    };
    let renamed = {
        let mut ops = OpsContext {
            transport,
            replay: &mut replay,
            mutations: Some(MutationContext {
                owners,
                create_verifier: None,
                namespace: Some(&mut honest),
            }),
            deadline: deadline(),
        };
        operations
            .execute(
                &mut ops,
                &StorageRequest {
                    context: request_context("m1-rename", Some(4)),
                    operation: StorageOperation::Rename {
                        source: path(b"before"),
                        destination: path(b"after"),
                        mode: RenameMode::Replace,
                    },
                },
            )
            .expect("a rename that moved the name, not the object")
    };
    let StorageResponse::Renamed(renamed) = renamed else {
        panic!("a rename answers with an object result");
    };
    assert_eq!(honest.calls, 1);
    // The renamed object is the same object, by contract identity.
    assert_eq!(
        renamed.stat.object_id,
        umbra_storage_nfs_userspace::identity::object_id(pinned)
    );
    // The handle held across the rename was never re-resolved and is untouched.
    assert_eq!(held.identity(), pinned);
    assert_eq!(held.handle().as_bytes(), pinned_handle.as_bytes());
    assert_eq!(
        held.read(transport, 0, 16, deadline()).expect("read").data,
        Vec::<u8>::new()
    );

    // A dispatcher that "renamed" by producing a different object broke every
    // open handle on the source, so it is refused rather than believed.
    let mut dishonest = StandIn {
        effect: Some(NamespaceEffect {
            identity: ObjectIdentity {
                fileid: pinned.fileid + 1_000,
                ..pinned
            },
            handle: pinned_handle,
        }),
        calls: 0,
    };
    let mut ops = OpsContext {
        transport,
        replay: &mut replay,
        mutations: Some(MutationContext {
            owners,
            create_verifier: None,
            namespace: Some(&mut dishonest),
        }),
        deadline: deadline(),
    };
    let error = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-rename-copy", Some(4)),
                operation: StorageOperation::Rename {
                    source: path(b"before"),
                    destination: path(b"elsewhere"),
                    mode: RenameMode::Replace,
                },
            },
        )
        .unwrap_err();
    assert!(error
        .context
        .contains("a rename moves a name, never the object"));
}

#[test]
fn external_edits_are_coherent_through_open_handles() {
    let (fake, layout) = seeded();
    let shared = SharedFake::new(fake);
    shared.external(|fake| {
        fake.insert_file(&layout.root, b"target", b"original".to_vec());
    });
    let mut session = StateSession::new(
        Backend::Fake,
        Box::new(shared.clone()),
        identity_for("storage-ops", Verifier([0x14; 8])),
    );
    established(&mut session);
    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");
    let root = operations.anchors().root().pin().clone();

    let (state, transport) = session.split();
    let owners = state.incarnation().expect("incarnation").open_owners();
    let held = OpenObject::open(
        owners,
        transport,
        &root,
        &component(b"target".to_vec()).expect("component"),
        CreateDisposition::OpenExisting,
        ShareAccess::READ,
        deadline(),
    )
    .expect("open");
    let original = held.identity();
    assert_eq!(
        held.read(transport, 0, 64, deadline()).expect("read").data,
        b"original"
    );

    // (1) In-place edit: one object, edited through a second open, seen through
    //     the first handle without it ever being re-resolved.
    let identity =
        MutationIdentity::from_context(&request_context("m1-inplace", Some(4))).expect("epoch");
    let writer = OpenObject::open(
        owners,
        transport,
        &root,
        &component(b"target".to_vec()).expect("component"),
        CreateDisposition::OpenExisting,
        ShareAccess::WRITE,
        deadline(),
    )
    .expect("a second open of the same object stands in for another writer");
    assert_eq!(writer.identity(), original, "one name, one object");
    let mut replay = FakeReplayLog::default();
    writer
        .write(
            transport,
            &mut replay,
            &identity,
            WriteAt {
                offset: 0,
                stability: Stability::FileSync,
                data: b"edited!!".to_vec(),
            },
            deadline(),
        )
        .expect("write");
    assert_eq!(
        held.read(transport, 0, 64, deadline()).expect("read").data,
        b"edited!!",
        "an in-place edit is visible through a handle that stayed bound"
    );
    assert_eq!(held.identity(), original);

    // (2) Pathname replacement: the name is rebound to a different object.
    shared.external(|fake| {
        fake.insert_file(&layout.root, b"target", b"replacement".to_vec());
    });
    assert_eq!(
        held.read(transport, 0, 64, deadline()).expect("read").data,
        b"edited!!",
        "an open handle is never retargeted by a replacement"
    );
    assert_eq!(held.identity(), original);

    let fresh = OpenObject::open(
        owners,
        transport,
        &root,
        &component(b"target".to_vec()).expect("component"),
        CreateDisposition::OpenExisting,
        ShareAccess::READ,
        deadline(),
    )
    .expect("open");
    assert_ne!(
        fresh.identity(),
        original,
        "a new open sees the replacement, with its own identity"
    );
    assert_eq!(
        fresh.read(transport, 0, 64, deadline()).expect("read").data,
        b"replacement"
    );

    // (3) Retention: the original object's last name is gone, and it stays
    //     readable through the open state until CLOSE consumes it.
    assert_eq!(
        held.read(transport, 0, 64, deadline()).expect("read").data,
        b"edited!!"
    );
    assert!(matches!(
        held.close(transport, deadline()),
        CloseOutcome::Closed(_)
    ));
    assert!(matches!(
        writer.close(transport, deadline()),
        CloseOutcome::Closed(_)
    ));
    assert!(matches!(
        fresh.close(transport, deadline()),
        CloseOutcome::Closed(_)
    ));
}

#[test]
fn directory_pages_are_bounded_and_cursors_are_invalidated_explicitly() {
    let (fake, layout) = seeded();
    let shared = SharedFake::new(fake);
    shared.external(|fake| {
        for index in 0..40u32 {
            fake.insert_file(&layout.root, format!("e{index:03}").as_bytes(), Vec::new());
        }
    });
    let mut session = StateSession::new(
        Backend::Fake,
        Box::new(shared.clone()),
        identity_for("storage-ops", Verifier([0x15; 8])),
    );
    established(&mut session);
    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");

    let mut replay = FakeReplayLog::default();
    let (_, transport) = session.split();
    let mut ops = OpsContext {
        transport,
        replay: &mut replay,
        mutations: None,
        deadline: deadline(),
    };

    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut cursor: Option<ListCursor> = None;
    let mut rounds = 0;
    loop {
        let StorageResponse::List(page) = operations
            .execute(
                &mut ops,
                &StorageRequest {
                    context: request_context("m1-list", None),
                    operation: StorageOperation::List {
                        path: path(b""),
                        cursor: cursor.clone(),
                        limit: 6,
                    },
                },
            )
            .expect("a bounded page")
        else {
            panic!("a list answers with a page");
        };
        assert!(page.entries.len() <= 6, "a page never exceeds its limit");
        for entry in &page.entries {
            let name = entry.name.as_bytes();
            assert_ne!(name, b".");
            assert_ne!(name, b"..");
            seen.push(name.to_vec());
        }
        cursor = page.next;
        rounds += 1;
        assert!(rounds < 60, "paging terminates");
        if cursor.is_none() {
            break;
        }
        if rounds == 2 {
            break;
        }
    }
    let held = cursor
        .clone()
        .expect("a partial enumeration leaves a cursor");
    assert!(
        seen.len() >= 6 && seen.len() < 40,
        "the page budget bounds one call"
    );

    // A server restart rotates the cookie verifier, voiding every outstanding
    // cookie. The contract permits explicit invalidation; it forbids answering
    // from an indefinitely cached snapshot.
    shared.external(|fake| fake.rotate_dir_verifier(DirVerifier([0x5A; 8])));
    let error = operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-list-stale", None),
                operation: StorageOperation::List {
                    path: path(b""),
                    cursor: Some(held),
                    limit: 6,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::StaleHandle);

    // A fresh enumeration still works: invalidation voids cursors, not directories.
    assert!(operations
        .execute(
            &mut ops,
            &StorageRequest {
                context: request_context("m1-list-fresh", None),
                operation: StorageOperation::List {
                    path: path(b""),
                    cursor: None,
                    limit: 6,
                },
            },
        )
        .is_ok());
}

#[test]
fn unsupported_capabilities_are_refused_and_deferred_ones_name_their_owner() {
    let (fake, layout) = seeded();
    let shared = SharedFake::new(fake);
    shared.external(|fake| {
        fake.insert_file(&layout.root, b"note", b"x".to_vec());
        fake.insert_directory(&layout.root, b"dir");
    });
    let mut session = StateSession::new(
        Backend::Fake,
        Box::new(shared.clone()),
        identity_for("storage-ops", Verifier([0x16; 8])),
    );
    established(&mut session);
    let operations = Operations::open(
        session.transport(),
        &config(),
        &open_run_request(),
        0,
        deadline(),
    )
    .expect("the run opens");

    let mut replay = FakeReplayLog::default();
    let (state, transport) = session.split();
    let owners = state.incarnation().expect("incarnation").open_owners();
    let mut ops = OpsContext {
        transport,
        replay: &mut replay,
        mutations: Some(MutationContext {
            owners,
            create_verifier: None,
            namespace: None,
        }),
        deadline: deadline(),
    };

    // Never offered: a typed refusal, no silent fallback, no empty success.
    for operation in [
        StorageOperation::Link {
            source: path(b"note"),
            destination: path(b"link"),
        },
        StorageOperation::ReadLink {
            path: path(b"note"),
        },
        StorageOperation::GetXattr {
            path: path(b"note"),
            name: b"user.x".to_vec(),
            max_bytes: 8,
        },
        StorageOperation::SetXattr {
            path: path(b"note"),
            name: b"user.x".to_vec(),
            value: vec![1],
        },
        StorageOperation::RemoveXattr {
            path: path(b"note"),
            name: b"user.x".to_vec(),
        },
        StorageOperation::AtomicSwap {
            left: path(b"note"),
            right: path(b"other"),
        },
        StorageOperation::Create {
            path: path(b"symlink"),
            options: CreateOptions {
                kind: CreateKind::LogicalSymlink {
                    target: BytePath::new(b"elsewhere").expect("target"),
                },
                mode: 0o777,
            },
        },
    ] {
        let error = operations
            .execute(
                &mut ops,
                &StorageRequest {
                    context: request_context("m1-unsupported", Some(4)),
                    operation: operation.clone(),
                },
            )
            .unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::UnsupportedCapability,
            "{operation:?} must be refused, never emulated"
        );
    }

    // Authorised by the syscall matrix, unreachable through the frozen transport:
    // the refusal names the owner instead of claiming a capability either way.
    for operation in [
        StorageOperation::Unlink {
            path: path(b"note"),
        },
        StorageOperation::RemoveDirectory { path: path(b"dir") },
        StorageOperation::Rename {
            source: path(b"note"),
            destination: path(b"moved"),
            mode: RenameMode::Replace,
        },
        StorageOperation::Create {
            path: path(b"newdir"),
            options: CreateOptions {
                kind: CreateKind::Directory,
                mode: 0o700,
            },
        },
        StorageOperation::Truncate {
            path: path(b"note"),
            len: 0,
        },
    ] {
        let error = operations
            .execute(
                &mut ops,
                &StorageRequest {
                    context: request_context("m1-deferred", Some(4)),
                    operation: operation.clone(),
                },
            )
            .unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::NotImplemented,
            "{operation:?} needs a dispatcher this context does not carry"
        );
        assert!(error.context.contains("no namespace dispatcher is bound"));
    }
}

#[test]
fn notifications_are_out_of_scope_by_design() {
    // The pinned syscall matrix decides this, not this node: remote file-change
    // notification is out of scope, `EVFILT_VNODE` on a virtual remote descriptor
    // is explicitly rejected, and FSEvents is a service no tracee is granted for a
    // fabricated remote path. Nothing in this crate registers, delivers or
    // emulates a notification. The capability table records that as a decision so
    // a reader does not have to read it as an omission.
    let recorded = OUT_OF_SURFACE
        .iter()
        .find(|note| {
            note.syscalls
                .iter()
                .any(|entry| entry.contains("EVFILT_VNODE"))
        })
        .expect("the notifications decision is recorded");
    assert!(matches!(recorded.support, Support::Unsupported { .. }));
    assert!(recorded.syscalls.iter().any(|e| e.contains("FSEvents")));

    // And nothing in the contract surface claims a notification capability.
    assert!(CONTRACT_SURFACE
        .iter()
        .all(|row| !row.syscalls.iter().any(|entry| entry.contains("FSEvents"))));
}

#[test]
fn a_pinned_object_refuses_to_answer_for_a_handle_that_names_something_else() {
    let (fake, layout) = seeded();
    let shared = SharedFake::new(fake);
    let (first, second) = shared.external(|fake| {
        (
            fake.insert_file(&layout.root, b"a", b"a".to_vec()),
            fake.insert_file(&layout.root, b"b", b"bb".to_vec()),
        )
    });
    let mut session = StateSession::new(
        Backend::Fake,
        Box::new(shared.clone()),
        identity_for("storage-ops", Verifier([0x17; 8])),
    );
    established(&mut session);
    let transport = session.transport();
    let pin = PinnedObject::pin(transport, first, deadline()).expect("pin");
    let attributes = transport
        .getattr(
            pin.handle(),
            umbra_storage_nfs_userspace::transport::AttrMask::STAT,
            deadline(),
        )
        .expect("attributes");
    // The situation a volatile filehandle creates: the pinned identity, a handle
    // that now resolves elsewhere. Safe-stop rather than answer for the wrong
    // object.
    let forged = PinnedObject::adopt(second, &attributes).expect("adopt");
    assert!(forged.revalidate(transport, deadline()).is_err());
}
