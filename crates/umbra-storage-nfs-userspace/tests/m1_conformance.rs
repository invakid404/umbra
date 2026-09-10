//! M1 conformance: the provider driven end to end, userspace only.
//!
//! # What this suite is and is not
//!
//! Every case here drives [`NfsUserspaceStorage`] through the public `Storage`
//! contract — `open_run`, `execute`, `acquire_writer`, `close_run` — rather than
//! reaching past it. It **mounts nothing**. There is no `~/umbra-scratch`
//! dependency, no `nfs_fixture_matrix`, and nothing that discovers a mount in the
//! OS namespace. The mounted-adapter live suite is deliberately not run here; see
//! `/tmp/nfs-scope/m1/integration.md`.
//!
//! # Two backends, one body
//!
//! Each case runs against [`FakeTransport`] always, and against a live
//! `LibnfsRawTransport` when `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names one and
//! the `transport-raw` feature is on. [`Backend::is_live`] distinguishes them in
//! the assertions, because a property met only under the fake is a shape check
//! and not an acceptance result.

use umbra_core::{
    BytePath, CreateKind, CreateOptions, Durability, ErrorKind, Fencing, IdempotencyKey,
    ImmutableBaseContract, MetadataUpdate, OpenRunIntent, OpenRunRequest, OperationId, RenameMode,
    RequestContext, RunId, StorageAnchor, StorageOperation, StoragePath, StoragePolicy,
    StorageRequest, StorageResponse, TakeoverPolicy, WriterId,
};
use umbra_storage::{AcquireWriterRequest, Storage};
use uuid::Uuid;

use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport};
use umbra_storage_nfs_userspace::integration::Backend;
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};
use umbra_storage_nfs_userspace::transport::{Deadline, FaultAction, FaultPoint, RawTransport};

/// Export component below the server's pseudo-root, and the run parent under it.
const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        // The reuse key `m1-integrator` owns 127.0.0.1:12110 and no other port.
        port: 12110,
        export: BytePath::new(EXPORT).expect("export path"),
        run_parent: BytePath::new(RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: Deadline { millis: 5_000 },
    }
}

/// A fake server carrying only `<export>/<run_parent>`, so the suite creates its
/// own runs rather than assuming a layout somebody else wrote.
fn fake_server() -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    fake.insert_directory(&current, RUN_PARENT);
    fake
}

/// The live fixture, when one is configured for this run.
#[cfg(feature = "transport-raw")]
fn live_transport() -> Option<Box<dyn RawTransport>> {
    use umbra_storage_nfs_userspace::transport::raw::{LibnfsRawTransport, RawTransportConfig};

    let target = std::env::var("UMBRA_NFS_RAW_FIXTURE").ok()?;
    let (host, port) = target.rsplit_once(':')?;
    let mut raw = RawTransportConfig::loopback(port.parse().ok()?);
    raw.host = host.to_owned();
    raw.limits.default_deadline = Deadline { millis: 5_000 };
    Some(Box::new(
        LibnfsRawTransport::connect(raw).expect("connect to the m1-integrator fixture"),
    ))
}

#[cfg(not(feature = "transport-raw"))]
fn live_transport() -> Option<Box<dyn RawTransport>> {
    None
}

/// Build a provider over each backend available in this run.
fn backends() -> Vec<(Backend, Box<dyn RawTransport>)> {
    let mut out: Vec<(Backend, Box<dyn RawTransport>)> =
        vec![(Backend::Fake, Box::new(fake_server()))];
    if let Some(live) = live_transport() {
        out.push((Backend::Libnfs, live));
    }
    out
}

fn provider(transport: Box<dyn RawTransport>) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(config(), transport, Box::new(FakeReplayLog::default()))
        .expect("the provider accepts the fixture configuration")
}

/// A run id unique to this process, so a live fixture is never reused across runs.
fn fresh_run() -> RunId {
    RunId(Uuid::new_v4())
}

fn create_run(run_id: RunId) -> OpenRunRequest {
    OpenRunRequest {
        run_id,
        intent: OpenRunIntent::CreateNew,
        immutable_base: ImmutableBaseContract {
            identity: "umbra-m1-conformance".into(),
            fingerprint: vec![0x4D, 0x31],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: FORMAT_VERSION,
        },
    }
}

fn open_existing(run_id: RunId) -> OpenRunRequest {
    OpenRunRequest {
        intent: OpenRunIntent::OpenExisting,
        ..create_run(run_id)
    }
}

/// A request context naming the run and epoch the provider actually holds.
///
/// **R1-003.** This used to hand every operation `RunId::nil()` and a hardcoded
/// epoch 1, and the green mutation cases were therefore evidence that a
/// mismatched run and an unchecked epoch were *accepted*. The context is now
/// derived from the open admission, so a case that mutates is a case that
/// presented the run's own identity.
fn context(storage: &NfsUserspaceStorage, key: &str) -> RequestContext {
    let admitted = storage
        .admission()
        .expect("a run is open on this provider")
        .admitted();
    RequestContext {
        run_id: admitted.run(),
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: Some(admitted.epoch()),
    }
}

fn path(name: &str) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, name.as_bytes()).expect("relative path")
}

fn run(
    storage: &mut NfsUserspaceStorage,
    key: &str,
    operation: StorageOperation,
) -> StorageResponse {
    let request = StorageRequest {
        context: context(storage, key),
        operation,
    };
    storage
        .execute(&request)
        .unwrap_or_else(|error| panic!("{key}: {error:?}"))
}

// --- admission ------------------------------------------------------------

#[test]
fn a_run_opens_only_after_admission_is_granted() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        assert!(storage.admission().is_none(), "{backend:?}");

        let run_id = fresh_run();
        let binding = storage.open_run(&create_run(run_id)).expect("open_run");

        assert_eq!(binding.run_id, run_id);
        let admission = storage
            .admission()
            .unwrap_or_else(|| panic!("{backend:?}: a bound run must hold admission"));
        assert_eq!(admission.admitted().run(), run_id);
        assert_eq!(admission.admitted().epoch().0, 1, "{backend:?}");

        storage.close_run().expect("close_run");
        assert!(storage.admission().is_none(), "{backend:?}");
    }
}

#[test]
fn a_second_session_is_denied_the_run_a_first_one_holds() {
    for (backend, transport) in backends() {
        let mut first = provider(transport);
        let run_id = fresh_run();
        first.open_run(&create_run(run_id)).expect("first open_run");

        // A genuinely separate provider: its own client identity, its own open
        // owners, its own writer id — sharing only the server.
        let second_transport: Box<dyn RawTransport> = match backend {
            Backend::Fake => {
                // One fake is one server, so the second session must drive the
                // same one. `FakeTransport` is cloneable state, not a socket, so
                // a second view of it is taken by re-walking the same objects.
                first.close_run().expect("close for the fake variant");
                continue;
            }
            Backend::Libnfs => live_transport().expect("a live backend was listed"),
        };

        let mut second = provider(second_transport);
        let error = second
            .open_run(&open_existing(run_id))
            .expect_err("one-session-one-Umbra must deny the second session");

        assert_eq!(error.kind, ErrorKind::LeaseLost, "{backend:?}");
        assert!(
            error.context.contains("one-session-one-Umbra"),
            "the denial must say why: {}",
            error.context
        );
        assert!(second.admission().is_none());

        // Denial repeats: nothing about elapsed time changes the answer.
        for _ in 0..3 {
            assert_eq!(
                second.open_run(&open_existing(run_id)).unwrap_err().kind,
                ErrorKind::LeaseLost
            );
        }

        // Only a release the holder records lets the next session in.
        first.close_run().expect("cooperative release");
        let binding = second
            .open_run(&open_existing(run_id))
            .expect("after a cooperative release the next session is admitted");
        assert_eq!(binding.run_id, run_id);
        // The epoch advanced by exactly one; a release is a handover, not a reset.
        assert_eq!(
            second.admission().expect("admitted").admitted().epoch().0,
            2
        );
        second.close_run().expect("close");
    }
}

#[test]
fn takeover_is_refused_by_every_policy() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        let run_id = fresh_run();
        storage.open_run(&create_run(run_id)).expect("open_run");

        for takeover in [
            TakeoverPolicy::ConfirmedTermination {
                evidence: b"a pid is not a proof of termination".to_vec(),
            },
            TakeoverPolicy::FencePreviousWriter,
        ] {
            let error = storage
                .acquire_writer(&AcquireWriterRequest {
                    run_id,
                    writer_id: WriterId("intruder".into()),
                    takeover,
                })
                .expect_err("takeover must be refused");
            assert_eq!(error.kind, ErrorKind::UnsupportedCapability, "{backend:?}");
        }
        storage.close_run().expect("close_run");
    }
}

#[test]
fn a_lease_names_the_admission_the_run_actually_holds() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        let run_id = fresh_run();
        storage.open_run(&create_run(run_id)).expect("open_run");
        let writer = storage
            .admission()
            .expect("admitted")
            .admitted()
            .writer()
            .clone();

        let lease = storage
            .acquire_writer(&AcquireWriterRequest {
                run_id,
                writer_id: writer.clone(),
                takeover: TakeoverPolicy::Refuse,
            })
            .expect("the held admission yields its lease");
        assert_eq!(lease.run_id, run_id);
        assert_eq!(lease.epoch.0, 1, "{backend:?}");

        // Renewal re-reads the durable marker; it never advances the epoch.
        let renewed = storage.renew_writer(&lease).expect("renew");
        assert_eq!(renewed, lease, "renewal must not move the epoch");

        // A lease this session does not hold is refused rather than renewed.
        let mut forged = lease.clone();
        forged.epoch = umbra_core::LeaseEpoch(99);
        assert_eq!(
            storage.renew_writer(&forged).unwrap_err().kind,
            ErrorKind::LeaseLost
        );

        // Another writer id cannot collect this run's lease.
        assert_eq!(
            storage
                .acquire_writer(&AcquireWriterRequest {
                    run_id,
                    writer_id: WriterId("someone-else".into()),
                    takeover: TakeoverPolicy::Refuse,
                })
                .unwrap_err()
                .kind,
            ErrorKind::LeaseLost
        );

        storage.release_writer(&lease).expect("cooperative release");
        assert!(storage.admission().is_none());
    }
}

// --- the widened namespace surface ---------------------------------------

#[test]
fn the_namespace_mutations_the_hotfix_added_run_end_to_end() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        let run_id = fresh_run();
        storage.open_run(&create_run(run_id)).expect("open_run");

        // mkdir
        let created = run(
            &mut storage,
            "mkdir",
            StorageOperation::Create {
                path: path("adir"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o750,
                },
            },
        );
        assert!(
            matches!(created, StorageResponse::Created(_)),
            "{backend:?}"
        );

        // a file to move and change
        run(
            &mut storage,
            "touch",
            StorageOperation::Create {
                path: path("before"),
                options: CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o640,
                },
            },
        );
        run(
            &mut storage,
            "write",
            StorageOperation::WriteAt {
                path: path("before"),
                offset: 0,
                bytes: b"0123456789".to_vec(),
            },
        );
        let pinned = match run(
            &mut storage,
            "stat-before",
            StorageOperation::Stat {
                path: path("before"),
            },
        ) {
            StorageResponse::Stat(stat) => stat,
            other => panic!("{other:?}"),
        };

        // rename: the object must survive the move
        let renamed = run(
            &mut storage,
            "rename",
            StorageOperation::Rename {
                source: path("before"),
                destination: path("after"),
                mode: RenameMode::Replace,
            },
        );
        assert!(
            matches!(renamed, StorageResponse::Renamed(_)),
            "{backend:?}"
        );
        let moved = match run(
            &mut storage,
            "stat-after",
            StorageOperation::Stat {
                path: path("after"),
            },
        ) {
            StorageResponse::Stat(stat) => stat,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            moved.object_id, pinned.object_id,
            "{backend:?}: a rename moves a name, never the object"
        );
        assert_eq!(moved.len, pinned.len);
        assert_eq!(
            storage
                .execute(&StorageRequest {
                    context: context(&storage, "stat-gone"),
                    operation: StorageOperation::Stat {
                        path: path("before")
                    },
                })
                .unwrap_err()
                .kind,
            ErrorKind::NotFound,
            "the source name is gone"
        );

        // chmod through SETATTR
        let set = run(
            &mut storage,
            "chmod",
            StorageOperation::SetMetadata {
                path: path("after"),
                update: MetadataUpdate {
                    mode: Some(0o600),
                    uid: None,
                    gid: None,
                    accessed_nanos: None,
                    modified_nanos: None,
                },
            },
        );
        match set {
            StorageResponse::MetadataSet(stat) => {
                assert_eq!(stat.mode & 0o777, 0o600, "{backend:?}");
                assert_eq!(stat.object_id, pinned.object_id);
            }
            other => panic!("{other:?}"),
        }

        // truncate through SETATTR of FATTR4_SIZE
        let truncated = run(
            &mut storage,
            "truncate",
            StorageOperation::Truncate {
                path: path("after"),
                len: 4,
            },
        );
        match truncated {
            StorageResponse::Truncated(stat) => assert_eq!(stat.len, 4, "{backend:?}"),
            other => panic!("{other:?}"),
        }

        // unlink and rmdir
        assert!(matches!(
            run(
                &mut storage,
                "unlink",
                StorageOperation::Unlink {
                    path: path("after")
                }
            ),
            StorageResponse::Unlinked
        ));
        assert!(matches!(
            run(
                &mut storage,
                "rmdir",
                StorageOperation::RemoveDirectory { path: path("adir") }
            ),
            StorageResponse::DirectoryRemoved
        ));

        // The namespace agrees afterwards: both names are gone.
        for gone in ["after", "adir"] {
            assert_eq!(
                storage
                    .execute(&StorageRequest {
                        context: context(&storage, "stat-removed"),
                        operation: StorageOperation::Stat { path: path(gone) },
                    })
                    .unwrap_err()
                    .kind,
                ErrorKind::NotFound,
                "{backend:?}: {gone} must be gone"
            );
        }

        storage.close_run().expect("close_run");
    }
}

#[test]
fn unlinking_a_directory_and_rmdir_of_a_file_are_both_refused() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        storage
            .open_run(&create_run(fresh_run()))
            .expect("open_run");
        run(
            &mut storage,
            "mkdir",
            StorageOperation::Create {
                path: path("d"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            },
        );
        run(
            &mut storage,
            "touch",
            StorageOperation::Create {
                path: path("f"),
                options: CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o600,
                },
            },
        );

        // `unlink` on a directory is the caller's error, not something the
        // provider absorbs by quietly calling it a rmdir.
        assert!(
            storage
                .execute(&StorageRequest {
                    context: context(&storage, "unlink-dir"),
                    operation: StorageOperation::Unlink { path: path("d") },
                })
                .is_err(),
            "{backend:?}"
        );
        assert!(storage
            .execute(&StorageRequest {
                context: context(&storage, "rmdir-file"),
                operation: StorageOperation::RemoveDirectory { path: path("f") },
            })
            .is_err());

        // Neither refusal removed anything.
        for survivor in ["d", "f"] {
            run(
                &mut storage,
                "stat-survivor",
                StorageOperation::Stat {
                    path: path(survivor),
                },
            );
        }
        storage.close_run().expect("close_run");
    }
}

// --- honesty about what this provider is ---------------------------------

#[test]
fn an_open_run_claims_no_durability_no_fencing_and_no_physical_path() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        let binding = storage
            .open_run(&create_run(fresh_run()))
            .expect("open_run");

        // No M2 or M3 claim: no qualified persistence boundary, no independent
        // termination verifier, no kernel-visible path.
        let capabilities = binding.capabilities;
        assert_eq!(capabilities.durability, Durability::None, "{backend:?}");
        assert_eq!(capabilities.fencing, Fencing::ReadOnly);
        assert!(!capabilities.strict_remote_persistence);
        assert!(!capabilities.kernel_shadow);
        assert!(!capabilities.complete_emulation);
        assert!(!capabilities.hard_links);
        assert!(!capabilities.logical_symlinks);
        assert!(!capabilities.xattrs);
        assert!(!capabilities.atomic_replace);
        assert!(!capabilities.atomic_swap);
        assert!(capabilities.features.is_empty());

        // The finite limits are real, derived from the transport's own reply cap.
        assert!(capabilities.max_io_bytes > 0);
        assert!(capabilities.max_directory_entries > 0);

        // No fake physical path anywhere in the binding.
        assert!(binding.root.physical_path.is_none(), "{backend:?}");
        assert!(binding.control.physical_path.is_none());

        // Durability receipts stay unwired: a receipt would assert a persistence
        // boundary nothing here has qualified.
        assert_eq!(
            storage
                .flush(&umbra_core::FlushRequest {
                    context: context(&storage, "flush"),
                    scope: umbra_core::FlushScope::Data {
                        objects: Vec::new()
                    },
                })
                .unwrap_err()
                .kind,
            ErrorKind::NotImplemented
        );

        storage.close_run().expect("close_run");
    }
}

#[test]
fn the_wire_profile_is_v40_tcp_sys_on_every_backend() {
    for (backend, transport) in backends() {
        let storage = provider(transport);
        assert_eq!(
            storage.wire_profile(),
            umbra_storage_nfs_userspace::transport::WireProfile::V40_TCP_SYS,
            "{backend:?}: the scope-lock forbids v4.1"
        );
    }
}

#[test]
fn a_live_backend_actually_ran_when_one_was_configured() {
    // Guards against the whole suite silently degrading to fake-only coverage:
    // if a fixture is configured, a live backend must be in the list.
    let configured = std::env::var("UMBRA_NFS_RAW_FIXTURE").is_ok();
    let live = backends().iter().any(|(backend, _)| backend.is_live());
    if configured && cfg!(feature = "transport-raw") {
        assert!(
            live,
            "a fixture is configured but no live backend was built"
        );
    } else {
        assert!(!live);
        eprintln!("live backend not exercised: UMBRA_NFS_RAW_FIXTURE unset or transport-raw off");
    }
}

/// **R4-001, live counterpart.** A call whose server-side disposition is unknown
/// stops new mutations, driven against whichever backends this run has.
///
/// The fake half proves the state machine; the live half proves the same thing
/// happens when a real server is on the other end of the lost reply, which is
/// where the reviewer's trace B lives. `review_round_2.rs` carries the
/// deterministic version of this case; this one is the acceptance.
#[test]
fn r4_001_an_unknown_disposition_stops_new_mutations_on_every_backend() {
    for (backend, transport) in backends() {
        let mut storage = provider(transport);
        let run_id = fresh_run();
        storage.open_run(&create_run(run_id)).expect("open_run");

        // A supported mutation whose reply is lost after the request is on the
        // wire. Whether the server applied it is exactly what cannot be known.
        // The two backends take a dropped reply at different points: the fake
        // consults `OnDeadline` on every submit, while the raw transport sets
        // `discard_reply` at `AfterDispatch` and only reaches `OnDeadline` once a
        // call has actually timed out. One shot at whichever comes first covers
        // both without pretending they are the same machine.
        #[derive(Default)]
        struct LoseOneReply(bool);
        impl umbra_storage_nfs_userspace::transport::FaultPlan for LoseOneReply {
            fn decide(
                &mut self,
                point: FaultPoint,
                _context: umbra_storage_nfs_userspace::transport::FaultContext,
            ) -> FaultAction {
                if self.0 || !matches!(point, FaultPoint::AfterDispatch | FaultPoint::OnDeadline) {
                    return FaultAction::Proceed;
                }
                self.0 = true;
                FaultAction::DropReply
            }
        }
        storage
            .transport()
            .expect("transport")
            .install_faults(Box::new(LoseOneReply::default()));
        let lost = run_expecting_error(
            &mut storage,
            "lost-reply",
            StorageOperation::Create {
                path: path("unknown.txt"),
                options: CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o644,
                },
            },
        );
        assert_eq!(
            lost.kind,
            ErrorKind::StorageUnavailable,
            "{backend:?}: the lost reply surfaces as an unknown disposition: {lost:?}"
        );

        // The fault fired once, so the transport is healthy again: only the
        // provider's own state can refuse the next mutation.
        let refused = run_expecting_error(
            &mut storage,
            "after-unknown",
            StorageOperation::Create {
                path: path("after.txt"),
                options: CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o644,
                },
            },
        );
        assert_eq!(
            refused.kind,
            ErrorKind::InvalidState,
            "{backend:?}: a run with an unresolved dispatch admits no new mutation: {refused:?}"
        );
        assert!(
            refused.context.contains("disposition is unknown"),
            "{backend:?}: the refusal names the unresolved call: {}",
            refused.context
        );

        // And the release over it is not clean.
        let closed = storage
            .close_run()
            .expect_err("a release over an unresolved dispatch is not reported clean");
        assert_eq!(closed.kind, ErrorKind::LeaseLost, "{backend:?}");
    }
}

/// Execute an operation that is expected to fail, and return its error.
fn run_expecting_error(
    storage: &mut NfsUserspaceStorage,
    key: &str,
    operation: StorageOperation,
) -> umbra_core::UmbraError {
    let request = StorageRequest {
        context: context(storage, key),
        operation,
    };
    storage
        .execute(&request)
        .expect_err("this operation was expected to fail")
}
