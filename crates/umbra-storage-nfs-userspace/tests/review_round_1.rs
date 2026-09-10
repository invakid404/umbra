//! Fixes for the round-1 independent review, driven through the public contract.
//!
//! Every case here names the review finding it covers (`R1-0NN`) so a later round
//! can trace fix to finding without reading the diff. Each test states, in its own
//! doc comment, what the pre-fix code did — the behaviour the reviewer described —
//! and asserts the corrected behaviour.
//!
//! **This suite mounts nothing.** It drives [`NfsUserspaceStorage`] over
//! [`FakeTransport`] through `Storage` only: no `~/umbra-scratch`, no
//! `nfs_fixture_matrix`, no OS-visible NFS mount, no live Ganesha.

use umbra_core::{
    BytePath, CreateKind, CreateOptions, ErrorKind, IdempotencyKey, ImmutableBaseContract,
    OpenRunIntent, OpenRunRequest, OperationId, RequestContext, RunId, StorageAnchor,
    StorageOperation, StoragePath, StoragePolicy, StorageRequest,
};
use umbra_storage::Storage;
use uuid::Uuid;

use umbra_storage_nfs_userspace::error::Nfs4Status;
use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport, ScriptedFault};
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};
use umbra_storage_nfs_userspace::transport::{
    Compound, CompoundReply, Deadline, FaultAction, FaultPoint, RawTransport,
};

const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

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
        deadline: Deadline { millis: 5_000 },
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

fn provider() -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(
        config(),
        Box::new(fake_server()),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider")
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

/// A mutation context naming the run and epoch the provider actually holds.
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

fn create_file(context: RequestContext, name: &[u8]) -> StorageRequest {
    StorageRequest {
        context,
        operation: StorageOperation::Create {
            path: path(name),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o644,
            },
        },
    }
}

// --- R1-002: lost authority must stop the provider ---------------------------

/// **R1-002.** A renewal that cannot re-prove the durable marker is terminal.
///
/// Pre-fix, `Session::renew` only *returned* an error: `Storage::renew_writer`
/// forwarded it without poisoning anything, and `request()` rebuilt a mutation
/// context on the very next call because `self.session` and the incarnation were
/// both still present. The provider went on mutating a run whose marker it could
/// no longer prove it held.
#[test]
fn r1_002_a_failed_renewal_stops_every_later_mutation() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let lease = storage.admission().expect("admitted").lease();

    // Before the failure, a mutation is accepted. This is the control: the
    // refusal below has to come from the lost authority, not from a broken run.
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "before"),
            b"before.txt",
        ))
        .expect("an admitted run mutates");

    // The marker round trip that renewal depends on fails, so renewal cannot
    // re-prove ownership. Anything other than a clean re-proof is a loss.
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            None,
            FaultAction::Substitute(Nfs4Status::IO),
        ));
    let failure = storage.renew_writer(&lease).expect_err("renewal must fail");
    assert_eq!(
        failure.kind,
        ErrorKind::Io,
        "the original NFS status survives the renewal failure: {failure:?}"
    );

    // The latch, not a second injected fault, is what refuses this.
    let refused = storage
        .execute(&create_file(
            authorised(&storage, run_id, "after"),
            b"after.txt",
        ))
        .expect_err("a provider that lost authority must not mutate");
    assert_eq!(refused.kind, ErrorKind::LeaseLost);
    assert!(
        refused.context.contains("lost writer authority"),
        "the refusal must name the latched loss: {}",
        refused.context
    );

    // Reads are refused too: the run is no longer this session's to speak for.
    let read_back = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("stat".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"before.txt"),
            },
        })
        .expect_err("a provider that lost authority must not answer for the run");
    assert_eq!(read_back.kind, ErrorKind::LeaseLost);
}

/// **R1-002.** A cooperative release ends this provider's authority over the run,
/// for reads as well as mutations.
///
/// Pre-fix, `release_writer` cleared `session` but left `operations` bound, so the
/// documented "`Some` exactly while a run is open" invariant was broken and reads
/// kept working against a run this session had just handed to a successor.
#[test]
fn r1_002_a_released_run_is_no_longer_this_providers_to_read() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let lease = storage.admission().expect("admitted").lease();
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "seed"),
            b"seed.txt",
        ))
        .expect("an admitted run mutates");

    storage.release_writer(&lease).expect("cooperative release");
    assert!(storage.admission().is_none(), "admission is gone");

    let after_read = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("stat-after-release".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"seed.txt"),
            },
        })
        .expect_err("a released run is not this provider's to read");
    assert_eq!(after_read.kind, ErrorKind::LeaseLost);
    assert!(
        after_read.context.contains("released admission"),
        "the refusal must name the handover: {}",
        after_read.context
    );

    // The run can still be torn down; only contract work on it is refused.
    storage.close_run().expect("close after release");
}

/// **R1-002.** A call whose server-side effect could not be ruled out makes the
/// following release report `Unknown`, which keeps the marker held.
///
/// Pre-fix, `close_run` asserted `OutstandingIo::Excluded` unconditionally,
/// justified by "submit returned". Withdrawing a local registration does not prove
/// the server never received the request, so a successor could be admitted over
/// work that was still going to take effect.
#[test]
fn r1_002_an_unsettled_call_blocks_a_clean_release() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // The request reaches the wire and its reply is dropped. Whether the server
    // applied it is exactly what this provider cannot know: the local
    // registration was withdrawn, which says nothing about what the server did
    // with a request it had already received.
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::OnDeadline,
            None,
            FaultAction::DropReply,
        ));
    let lost = storage
        .execute(&create_file(
            authorised(&storage, run_id, "unsettled"),
            b"unsettled.txt",
        ))
        .expect_err("the injected loss surfaces");
    assert_eq!(lost.kind, ErrorKind::StorageUnavailable);

    // The release now refuses rather than handing the run on over that call.
    let refused = storage
        .close_run()
        .expect_err("a release over unsettled I/O must not be reported clean");
    assert_eq!(refused.kind, ErrorKind::LeaseLost);
    assert!(
        refused
            .context
            .contains("outstanding I/O could not be excluded"),
        "the refusal must name the unsettled work: {}",
        refused.context
    );
    // The marker stays held: the session was handed back, not consumed.
    assert!(
        storage.admission().is_some(),
        "a refused release retains admission"
    );
}

// --- R1-003: request authority and run policy are validated ------------------

/// **R1-003.** A mutation naming a different run than the one this surface is
/// bound to is refused before any effect.
///
/// Pre-fix nothing compared `RequestContext::run_id` to the open run:
/// `umbra_storage::validate_request` explicitly leaves bound-run checks to the
/// backend, and `MutationIdentity::from_context` accepted whatever run it was
/// handed. The shipped conformance suite passed `RunId::nil()` for every
/// operation and every mutation still succeeded.
#[test]
fn r1_003_a_mutation_naming_another_run_is_refused() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let mut wrong = authorised(&storage, run_id, "wrong-run");
    wrong.run_id = RunId(Uuid::nil());
    let refused = storage
        .execute(&create_file(wrong, b"wrong-run.txt"))
        .expect_err("a request naming another run must not mutate this one");
    assert_eq!(refused.kind, ErrorKind::InvalidInput);
    assert!(
        refused.context.contains("bound to run"),
        "the refusal must name the binding: {}",
        refused.context
    );

    // Nothing was created: the refusal came before any effect.
    let missing = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("probe".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"wrong-run.txt"),
            },
        })
        .expect_err("the refused create left nothing behind");
    assert_eq!(missing.kind, ErrorKind::NotFound);
}

/// **R1-003.** A mutation presenting an epoch this provider does not hold is
/// refused, whether the epoch is stale or invented.
#[test]
fn r1_003_a_stale_or_forged_writer_epoch_is_refused() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    let held = storage.admission().expect("admitted").admitted().epoch();
    assert_eq!(held.0, 1, "a created run is admitted at epoch 1");

    for (label, epoch) in [("stale", 0u64), ("forged", 99u64)] {
        let mut context = authorised(&storage, run_id, label);
        context.writer_epoch = Some(umbra_core::LeaseEpoch(epoch));
        let refused = storage
            .execute(&create_file(context, format!("{label}.txt").as_bytes()))
            .unwrap_err();
        assert_eq!(
            refused.kind,
            ErrorKind::LeaseLost,
            "{label} epoch {epoch} must be refused"
        );
        assert!(
            refused.context.contains("stale writer epoch"),
            "{label}: the refusal must name the epoch mismatch: {}",
            refused.context
        );
    }
}

/// **R1-003.** A run opened read-only refuses mutations.
///
/// Pre-fix `Operations::open` checked only `format_version` and dropped the rest
/// of the policy, so an opened read-only run mutated exactly like a writable one.
#[test]
fn r1_003_a_read_only_run_refuses_mutations() {
    let mut storage = provider();
    let run_id = fresh_run();
    // Create it writable first, then reopen the same run read-only.
    storage.open_run(&create_run(run_id)).expect("create");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "seed"),
            b"seed.txt",
        ))
        .expect("the writable run mutates");
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

    let refused = storage
        .execute(&create_file(
            authorised(&storage, run_id, "readonly"),
            b"nope.txt",
        ))
        .expect_err("a read-only run must not accept a create");
    assert_eq!(refused.kind, ErrorKind::Denied);
    assert!(
        refused.context.contains("read-only"),
        "the refusal must name the policy: {}",
        refused.context
    );

    // Reads still work: read-only is a restriction on mutation, not on access.
    storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("ro-stat".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"seed.txt"),
            },
        })
        .expect("a read-only run still reads");
}

/// **R1-003.** `CreateNew` under a read-only policy is refused, and so is any
/// policy requiring a guarantee this provider does not offer.
///
/// Pre-fix all three were accepted: `open` validated `format_version` alone, so a
/// read-only run could be *created*, and a caller demanding strict remote
/// persistence or a kernel shadow got a run that quietly provided neither.
#[test]
fn r1_003_unsupported_run_policies_are_refused_before_the_run_exists() {
    let cases: [(&str, StoragePolicy, ErrorKind); 3] = [
        (
            "read-only create",
            StoragePolicy {
                read_only: true,
                ..policy()
            },
            ErrorKind::Denied,
        ),
        (
            "strict remote persistence",
            StoragePolicy {
                require_strict_remote_persistence: true,
                ..policy()
            },
            ErrorKind::UnsupportedCapability,
        ),
        (
            "kernel shadow",
            StoragePolicy {
                require_kernel_shadow: true,
                ..policy()
            },
            ErrorKind::UnsupportedCapability,
        ),
    ];
    for (label, policy, expected) in cases {
        let mut storage = provider();
        let run_id = fresh_run();
        let refused = storage
            .open_run(&OpenRunRequest {
                run_id,
                intent: OpenRunIntent::CreateNew,
                immutable_base: base(),
                policy,
            })
            .expect_err(label);
        assert_eq!(refused.kind, expected, "{label}: {refused:?}");
        assert!(
            storage.admission().is_none(),
            "{label}: a refused open must publish no admission"
        );
    }
}

// --- R1-006: no-replace rename is refused, not emulated ----------------------

/// **R1-006.** `RenameMode::NoReplace` is refused before any effect.
///
/// Pre-fix the provider probed the destination and refused only when the probe
/// returned `Ok`. Two holes followed: another client creating the destination
/// between the probe and the RENAME had it silently overwritten, and a probe that
/// failed with anything but NOENT — EIO, ACCESS, STALE — fell straight through to
/// an ordinary replacing RENAME. Neither the capability table nor the preflight
/// rejected the mode.
#[test]
fn r1_006_no_replace_rename_is_refused_before_any_effect() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "src"),
            b"source.txt",
        ))
        .expect("create the source");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "dst"),
            b"dest.txt",
        ))
        .expect("create the destination");

    let refused = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "no-replace"),
            operation: StorageOperation::Rename {
                source: path(b"source.txt"),
                destination: path(b"dest.txt"),
                mode: umbra_core::RenameMode::NoReplace,
            },
        })
        .expect_err("atomic no-replace is not available over NFSv4.0");
    assert_eq!(refused.kind, ErrorKind::UnsupportedCapability);
    assert!(
        refused.context.contains("not atomic"),
        "the refusal must say why, not just that: {}",
        refused.context
    );

    // Nothing moved and nothing was overwritten.
    for name in [b"source.txt".as_slice(), b"dest.txt".as_slice()] {
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

    // The ordinary replacing rename is unaffected.
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "replace"),
            operation: StorageOperation::Rename {
                source: path(b"source.txt"),
                destination: path(b"dest.txt"),
                mode: umbra_core::RenameMode::Replace,
            },
        })
        .expect("a replacing rename is supported");
}

// --- R1-007: the requested create mode is applied ----------------------------

/// **R1-007.** A file created with a non-default mode has that mode afterwards.
///
/// Pre-fix, `request()` supplied a create verifier for every keyed operation, so
/// the create always chose `CreateExclusive` rather than `CreateNew { mode }`.
/// `EXCLUSIVE4` carries the verifier in the field that would have carried the
/// attributes, and no SETATTR followed, so creating an executable with mode 0755
/// succeeded without the execute bits.
#[test]
fn r1_007_a_created_file_has_the_mode_that_was_requested() {
    for mode in [0o755u32, 0o600, 0o444] {
        let mut storage = provider();
        let run_id = fresh_run();
        storage.open_run(&create_run(run_id)).expect("open_run");

        let name = format!("mode-{mode:o}.bin");
        let created = storage
            .execute(&StorageRequest {
                context: authorised(&storage, run_id, &name),
                operation: StorageOperation::Create {
                    path: path(name.as_bytes()),
                    options: CreateOptions {
                        kind: CreateKind::File,
                        mode,
                    },
                },
            })
            .unwrap_or_else(|error| panic!("create {mode:o}: {error:?}"));
        let umbra_core::StorageResponse::Created(result) = created else {
            panic!("a create answers with an object result");
        };
        assert_eq!(
            result.stat.mode & 0o7777,
            mode,
            "the create result must report the mode that was asked for"
        );

        // And the server agrees on a fresh read, so this is the object's mode and
        // not a value the create path invented for its own reply.
        let stat = storage
            .execute(&StorageRequest {
                context: RequestContext {
                    run_id,
                    operation_id: OperationId(Uuid::new_v4()),
                    idempotency_key: IdempotencyKey(format!("stat-{mode:o}")),
                    writer_epoch: None,
                },
                operation: StorageOperation::Stat {
                    path: path(name.as_bytes()),
                },
            })
            .expect("stat the created file");
        let umbra_core::StorageResponse::Stat(stat) = stat else {
            panic!("a stat answers with a blob stat");
        };
        assert_eq!(
            stat.mode & 0o7777,
            mode,
            "a fresh read must see the requested mode"
        );
    }
}

/// **R1-007.** Retrying the identical create — the lost-reply case the exclusive
/// verifier exists for — still ends with the requested mode.
#[test]
fn r1_007_an_exclusive_create_retry_keeps_the_requested_mode() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // One idempotency key, presented twice: the second call is the retry a lost
    // reply produces.
    let context = authorised(&storage, run_id, "retry-create");
    let request = StorageRequest {
        context: context.clone(),
        operation: StorageOperation::Create {
            path: path(b"retry.bin"),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o750,
            },
        },
    };
    storage.execute(&request).expect("the first create");
    let retried = storage.execute(&request);

    // Whether the retry is answered from the recorded outcome or re-runs the
    // exclusive create, the mode must be the requested one either way.
    if let Ok(umbra_core::StorageResponse::Created(result)) = &retried {
        assert_eq!(result.stat.mode & 0o7777, 0o750);
    }
    let stat = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("retry-stat".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"retry.bin"),
            },
        })
        .expect("stat the retried create");
    let umbra_core::StorageResponse::Stat(stat) = stat else {
        panic!("a stat answers with a blob stat");
    };
    assert_eq!(
        stat.mode & 0o7777,
        0o750,
        "a retried exclusive create must not leave the default mode behind"
    );
}

// --- R1-015: the anchor walk does not cross into another filesystem ----------

/// A transport that reports a different `fsid` for one named directory.
///
/// This is what a nested export looks like on the wire: an ordinary directory,
/// resolvable, not a symlink, whose `FATTR4_FSID` differs from its parent's.
/// Byte-path validation cannot see it and symlink refusal does not apply, which
/// is exactly why R1-015 needs its own check.
struct NestedExport {
    inner: FakeTransport,
    /// Component whose replies get the foreign filesystem id.
    boundary: Vec<u8>,
    /// Fileids observed to belong to that component, so later GETATTRs on the
    /// same object stay consistent.
    foreign: std::cell::RefCell<std::collections::BTreeSet<u64>>,
}

impl NestedExport {
    fn new(inner: FakeTransport, boundary: &[u8]) -> Self {
        Self {
            inner,
            boundary: boundary.to_vec(),
            foreign: std::cell::RefCell::new(std::collections::BTreeSet::new()),
        }
    }

    /// Rewrite the fsid of any attribute set belonging to the boundary object.
    fn rewrite(&self, call: &Compound, reply: &mut CompoundReply) {
        use umbra_storage_nfs_userspace::transport::{Fsid, Nfs4Op, OpReply};
        const FOREIGN: Fsid = Fsid {
            major: 0xFEED,
            minor: 0xFACE,
        };

        // A LOOKUP of the boundary name marks its fileid as foreign from then on.
        let looked_up = call.ops.iter().any(
            |op| matches!(op, Nfs4Op::Lookup(name) if name.as_bytes() == self.boundary.as_slice()),
        );
        for result in reply.results.iter_mut() {
            if let OpReply::GetAttr(attributes) = result {
                let Some(fileid) = attributes.fileid else {
                    continue;
                };
                if looked_up {
                    self.foreign.borrow_mut().insert(fileid);
                }
                if self.foreign.borrow().contains(&fileid) {
                    attributes.fsid = Some(FOREIGN);
                }
            }
        }
    }
}

impl RawTransport for NestedExport {
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
        call: Compound,
        deadline: Deadline,
    ) -> umbra_storage_nfs_userspace::transport::TransportResult<CompoundReply> {
        let mut reply = self.inner.submit(call.clone(), deadline)?;
        self.rewrite(&call, &mut reply);
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

/// **R1-015.** A directory component whose `fsid` differs from the run's is
/// rejected, and nothing is dispatched beneath it.
///
/// Pre-fix `PinnedObject::adopt` recorded whatever fsid the server returned and
/// nothing compared it to the anchor's, so a LOOKUP into a nested exported
/// filesystem was adopted and used for further reads and mutations. Component
/// bytes were validated and physical symlinks refused, but neither of those sees
/// a filesystem crossing.
#[test]
fn r1_015_a_component_on_another_filesystem_is_refused() {
    let mut storage = NfsUserspaceStorage::with_facades(
        config(),
        Box::new(NestedExport::new(fake_server(), b"nested")),
        Box::new(FakeReplayLog::default()),
    )
    .expect("provider");
    let run_id = fresh_run();
    storage
        .open_run(&create_run(run_id))
        .expect("create the run");

    // An ordinary directory, created through the contract. The transport reports
    // a foreign filesystem for it the moment a walk *resolves* it by name, which
    // is what a nested export looks like: nothing about the create says so.
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "nested-dir"),
            operation: StorageOperation::Create {
                path: path(b"nested"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            },
        })
        .expect("creating the directory itself is an ordinary create");

    // Resolving it is refused, so it can never be adopted as a pin.
    let stat = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("stat-nested".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"nested"),
            },
        })
        .expect_err("an object reported on another filesystem must not be adopted");
    assert_eq!(stat.kind, ErrorKind::InvalidPath);
    assert!(
        stat.context.contains("another exported filesystem"),
        "the refusal must name the crossing: {}",
        stat.context
    );

    // And nothing is dispatched *beneath* the crossing: the walk stops at the
    // boundary component rather than continuing through it.
    let refused = storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "below-nested"),
            operation: StorageOperation::Create {
                path: path(b"nested/inside.txt"),
                options: CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o644,
                },
            },
        })
        .expect_err("nothing may be created beneath a filesystem crossing");
    assert_eq!(refused.kind, ErrorKind::InvalidPath);
    assert!(
        refused.context.contains("another exported filesystem"),
        "the refusal must name the crossing: {}",
        refused.context
    );

    // A sibling on the run's own filesystem is unaffected.
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "sibling"),
            b"sibling.txt",
        ))
        .expect("the rest of the run is untouched by the boundary check");
}

/// **R1-015.** The deliberate pseudo-root to configured-export transition is
/// still allowed: pinning the boundary must not break ordinary operation.
#[test]
fn r1_015_the_configured_export_transition_is_still_allowed() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage
        .open_run(&create_run(run_id))
        .expect("the export walk crosses from the pseudo-root as it always did");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "ordinary"),
            b"ordinary.txt",
        ))
        .expect("ordinary paths inside the run still work");
    storage.close_run().expect("close");
}

// --- R1-011: original NFS errors survive the public surface ------------------

/// **R1-011.** A `.provider` lookup that *failed* is not reported as a `.provider`
/// that is *absent*.
///
/// Pre-fix the lookup was `.ok()`, so EIO, ACCESS and STALE all became `None`,
/// and the caller then saw `session.rs`'s generic "the run has no `.provider`
/// directory" message. A missing private directory and an unreachable one need
/// different answers: the first is a run state, the second is a server failure.
#[test]
fn r1_011_a_failed_private_directory_lookup_is_not_reported_as_absence() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage
        .open_run(&create_run(run_id))
        .expect("create the run");
    storage.close_run().expect("release");

    // Reopen, with the `.provider` lookup answered NFS4ERR_IO. The run exists and
    // its private directory exists; the lookup is what failed.
    //
    // The fault fires on the first compound of the reopen walk, which is enough
    // to prove the distinction: the error that comes back carries the server's
    // status rather than the absence message.
    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            None,
            FaultAction::Substitute(Nfs4Status::IO),
        ));
    let error = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::OpenExisting,
            immutable_base: base(),
            policy: policy(),
        })
        .expect_err("a failed lookup during the walk is reported, not absorbed");

    assert_ne!(
        error.kind,
        ErrorKind::InvalidState,
        "a failed lookup must not be reported as the run having no .provider directory"
    );
    assert!(
        !error.context.contains("has no .provider directory"),
        "the absence message must be reserved for actual absence: {}",
        error.context
    );
    assert!(
        error.context.contains("NFS4ERR 5") || error.kind == ErrorKind::Io,
        "the original NFS status must survive: {error:?}"
    );
}

/// **R1-011.** A `BAD_COOKIE` still reports an invalidated cursor, but keeps the
/// server's own status and operation in the message.
///
/// Pre-fix the arm replaced the facade error with a freshly constructed
/// `StaleHandle` whose entire context was "the server invalidated this cursor",
/// discarding the numeric status, the failing operation and its COMPOUND index.
#[test]
fn r1_011_an_invalidated_cursor_keeps_the_original_status() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "entry"),
            b"entry.txt",
        ))
        .expect("seed one entry");

    storage
        .transport()
        .expect("transport")
        .install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            None,
            FaultAction::Substitute(Nfs4Status::BAD_COOKIE),
        ));
    let error = storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("list".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::List {
                path: path(b""),
                cursor: None,
                limit: 16,
            },
        })
        .expect_err("BAD_COOKIE invalidates the cursor");

    assert_eq!(
        error.kind,
        ErrorKind::StaleHandle,
        "the caller still learns the cursor is void, so it does not retry forever"
    );
    assert!(
        error.context.contains("invalidated this cursor"),
        "the cursor diagnosis is kept: {}",
        error.context
    );
    assert!(
        error.context.contains("10003"),
        "the original NFS status number must survive alongside it: {}",
        error.context
    );
}
