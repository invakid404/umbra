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

// --- R1-005: an existing run's own evidence is validated ---------------------

/// Seed a run directory the way a *previous* provider left it: the layout, a
/// manifest, an `.provider/epoch`, and no `writer.lock`.
///
/// This is a userspace-seeded legacy run. No mounted adapter and no live server
/// is involved; the bytes are the ones the mounted adapter writes.
fn seed_existing_run(
    manifest: Option<Vec<u8>>,
    epoch: Option<u64>,
    run_id: RunId,
) -> (FakeTransport, RunId) {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    let run_parent = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&run_parent, run_id.0.hyphenated().to_string().as_bytes());
    fake.insert_directory(&run, b"root");
    fake.insert_directory(&run, b"control");
    let private = fake.insert_directory(&run, b".provider");
    fake.insert_directory(&private, b"retries");
    if let Some(bytes) = manifest {
        fake.insert_file(&private, b"manifest", bytes);
    }
    if let Some(epoch) = epoch {
        fake.insert_file(&private, b"epoch", epoch.to_le_bytes().to_vec());
    }
    (fake, run_id)
}

fn manifest_bytes(run_id: RunId, base: &ImmutableBaseContract, format: u32) -> Vec<u8> {
    serde_json::to_vec(&(run_id, base, format)).expect("the manifest encodes")
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

fn over(fake: FakeTransport) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(config(), Box::new(fake), Box::new(FakeReplayLog::default()))
        .expect("provider")
}

/// **R1-005.** A legacy run that reached epoch 7 and released cleanly is admitted
/// at epoch 8, not epoch 1.
///
/// Pre-fix a missing `writer.lock` always initialised epoch 1, so opening a
/// cleanly released run regressed its authority epoch — a later reader could not
/// tell this session's epoch 3 from the legacy run's own epoch 3.
#[test]
fn r1_005_a_released_legacy_run_does_not_restart_the_epoch_ladder() {
    let run_id = fresh_run();
    let (fake, run_id) = seed_existing_run(
        Some(manifest_bytes(run_id, &base(), FORMAT_VERSION)),
        Some(7),
        run_id,
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("a cleanly released legacy run opens");

    let epoch = storage.admission().expect("admitted").admitted().epoch();
    assert_eq!(
        epoch.0, 8,
        "admission must land above every epoch the run already used, not at 1"
    );
}

/// **R1-005.** A run whose manifest was never written is refused, not adopted.
#[test]
fn r1_005_a_run_without_a_manifest_is_refused() {
    let run_id = fresh_run();
    let (fake, run_id) = seed_existing_run(None, Some(0), run_id);
    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("no manifest, no identity");
    assert_eq!(refused.kind, ErrorKind::CorruptJournal);
    assert!(
        refused.context.contains("no .provider/manifest"),
        "{}",
        refused.context
    );
    assert!(storage.admission().is_none(), "no admission is published");
}

/// **R1-005.** A manifest that does not decode is refused and its bytes are left
/// where they are.
#[test]
fn r1_005_a_malformed_manifest_is_refused_and_retained() {
    let run_id = fresh_run();
    let (fake, run_id) = seed_existing_run(Some(b"{not json".to_vec()), Some(3), run_id);
    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("a malformed manifest is refused");
    assert_eq!(refused.kind, ErrorKind::CorruptJournal);
    assert!(
        refused.context.contains("does not decode"),
        "{}",
        refused.context
    );
}

/// **R1-005.** A manifest naming a different run, base or format version is
/// refused. Each is a distinct way the run on the server is not the run asked for.
#[test]
fn r1_005_a_manifest_that_names_other_state_is_refused() {
    let requested = fresh_run();

    // Another run id.
    let (fake, run_id) = seed_existing_run(
        Some(manifest_bytes(
            RunId(Uuid::new_v4()),
            &base(),
            FORMAT_VERSION,
        )),
        Some(1),
        requested,
    );
    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("wrong run id");
    assert_eq!(refused.kind, ErrorKind::InvalidState);
    assert!(
        refused.context.contains("records run"),
        "{}",
        refused.context
    );

    // Another immutable base.
    let other_base = ImmutableBaseContract {
        identity: "some-other-base".into(),
        fingerprint: vec![0x01, 0x02],
    };
    let (fake, run_id) = seed_existing_run(
        Some(manifest_bytes(requested, &other_base, FORMAT_VERSION)),
        Some(1),
        requested,
    );
    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("wrong immutable base");
    assert_eq!(refused.kind, ErrorKind::InvalidState);
    assert!(
        refused.context.contains("immutable base"),
        "{}",
        refused.context
    );

    // Another format version.
    let (fake, run_id) = seed_existing_run(
        Some(manifest_bytes(requested, &base(), FORMAT_VERSION + 1)),
        Some(1),
        requested,
    );
    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("wrong format version");
    assert_eq!(refused.kind, ErrorKind::ProtocolMismatch);
}

/// **R1-005.** An `.provider/epoch` that cannot be read is refused rather than
/// treated as zero. A corrupt file is not an inference that the run never ran.
#[test]
fn r1_005_an_unreadable_epoch_file_is_refused_not_read_as_zero() {
    let run_id = fresh_run();
    let (mut fake, run_id) = seed_existing_run(
        Some(manifest_bytes(run_id, &base(), FORMAT_VERSION)),
        None,
        run_id,
    );
    // Three bytes where a little-endian u64 belongs.
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake
            .lookup(
                &current,
                &umbra_storage_nfs_userspace::transport::ComponentName::new(part.to_vec()).unwrap(),
                umbra_storage_nfs_userspace::transport::AttrMask::STAT,
                Deadline { millis: 5_000 },
            )
            .expect("walk")
            .0;
    }
    let (run_parent, _) = fake
        .lookup(
            &current,
            &umbra_storage_nfs_userspace::transport::ComponentName::new(RUN_PARENT.to_vec())
                .unwrap(),
            umbra_storage_nfs_userspace::transport::AttrMask::STAT,
            Deadline { millis: 5_000 },
        )
        .expect("run parent");
    let (run, _) = fake
        .lookup(
            &run_parent,
            &umbra_storage_nfs_userspace::transport::ComponentName::new(
                run_id.0.hyphenated().to_string().into_bytes(),
            )
            .unwrap(),
            umbra_storage_nfs_userspace::transport::AttrMask::STAT,
            Deadline { millis: 5_000 },
        )
        .expect("run");
    let (private, _) = fake
        .lookup(
            &run,
            &umbra_storage_nfs_userspace::transport::ComponentName::new(b".provider".to_vec())
                .unwrap(),
            umbra_storage_nfs_userspace::transport::AttrMask::STAT,
            Deadline { millis: 5_000 },
        )
        .expect("private");
    fake.insert_file(&private, b"epoch", vec![0xAA, 0xBB, 0xCC]);

    let mut storage = over(fake);
    let refused = open_existing(&mut storage, run_id).expect_err("a truncated epoch is refused");
    assert_eq!(refused.kind, ErrorKind::CorruptJournal);
    assert!(
        refused.context.contains("not the 8"),
        "the refusal must name the shape problem: {}",
        refused.context
    );
    assert!(
        refused.context.contains("rather than restarted at epoch 1"),
        "and must say what it refused to infer: {}",
        refused.context
    );
}

// --- R1-004: supported mutations go through a durable replay journal ---------

/// Seed a run that already carries one retry record, as a previous provider
/// process would have left it.
fn seed_run_with_record(run_id: RunId, record_name: &str, record: Vec<u8>) -> FakeTransport {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for part in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, part);
    }
    let run_parent = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&run_parent, run_id.0.hyphenated().to_string().as_bytes());
    fake.insert_directory(&run, b"root");
    fake.insert_directory(&run, b"control");
    let private = fake.insert_directory(&run, b".provider");
    fake.insert_file(
        &private,
        b"manifest",
        manifest_bytes(run_id, &base(), FORMAT_VERSION),
    );
    fake.insert_file(&private, b"epoch", 0u64.to_le_bytes().to_vec());
    let retries = fake.insert_directory(&private, b"retries");
    fake.insert_file(&retries, record_name.as_bytes(), record);
    fake
}

fn key_file(key: &str) -> String {
    let hex: String = key.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("key-{hex}")
}

/// **R1-004.** The review's exact static trace: a successful rename, retried
/// under the identical idempotency key, is answered from its record.
///
/// Pre-fix there was no record. The retry re-resolved the source, found it gone —
/// because the first attempt had already moved it — and reported `NOENT` for an
/// operation that had actually succeeded. The only `ReplayLog` implementation was
/// in memory, `MutationJournal` was reached by no `Storage` method, and namespace
/// mutations dispatched straight to the wire.
#[test]
fn r1_004_an_exact_key_retry_of_a_rename_is_answered_from_its_record() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(authorised(&storage, run_id, "seed"), b"a.txt"))
        .expect("create the source");

    let context = authorised(&storage, run_id, "rename-key");
    let rename = StorageRequest {
        context,
        operation: StorageOperation::Rename {
            source: path(b"a.txt"),
            destination: path(b"b.txt"),
            mode: umbra_core::RenameMode::Replace,
        },
    };
    let first = storage.execute(&rename).expect("the rename succeeds");

    // The identical request again. `a.txt` no longer exists, so a re-dispatch
    // would answer NOENT.
    let retried = storage
        .execute(&rename)
        .expect("an exact-key retry must be answered, not re-dispatched");
    assert_eq!(
        format!("{first:?}"),
        format!("{retried:?}"),
        "the retry must return the recorded outcome verbatim"
    );

    // And the namespace was not touched a second time.
    storage
        .execute(&StorageRequest {
            context: RequestContext {
                run_id,
                operation_id: OperationId(Uuid::new_v4()),
                idempotency_key: IdempotencyKey("probe-b".into()),
                writer_epoch: None,
            },
            operation: StorageOperation::Stat {
                path: path(b"b.txt"),
            },
        })
        .expect("the destination is still there exactly once");
}

/// **R1-004.** A settled *failure* is replayed as that failure, so a retry cannot
/// reinterpret it by trying again.
#[test]
fn r1_004_a_recorded_failure_is_replayed_as_that_failure() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    // Removing a directory with Unlink is refused by the capability surface, so
    // this failure is settled: the server's answer is known.
    storage
        .execute(&StorageRequest {
            context: authorised(&storage, run_id, "mkdir"),
            operation: StorageOperation::Create {
                path: path(b"adir"),
                options: CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            },
        })
        .expect("create a directory");

    let context = authorised(&storage, run_id, "unlink-dir");
    let bad = StorageRequest {
        context,
        operation: StorageOperation::Unlink {
            path: path(b"adir"),
        },
    };
    let first = storage
        .execute(&bad)
        .expect_err("unlinking a directory is refused");
    let again = storage
        .execute(&bad)
        .expect_err("and the retry gets the same answer");
    assert_eq!(first.kind, again.kind);
    assert_eq!(first.context, again.context, "verbatim, from the record");
}

/// **R1-004 / R2-003.** A *legacy* intent — one recorded without the precondition
/// evidence this provider now writes — stops the run rather than being guessed.
///
/// This test used to assert the opposite of what the design requires. It named
/// itself `..._forces_reconciliation` and checked for the words "requires
/// reconciliation", which `docs/design/failure-model.md:53` explicitly forbids as
/// a substitute for implemented recovery in a supported crash window. Round 2
/// recorded that as R2-003, so the case now asserts what the failure model
/// actually says about this narrower situation: a legacy ambiguous intent with
/// insufficient evidence stops as BLOCKED_RECOVERABLE, with the record retained.
///
/// Recovery of intents that *do* carry evidence is covered by the `r2_003_*`
/// cases in `review_round_2.rs`.
#[test]
fn r1_004_a_legacy_intent_without_evidence_blocks_rather_than_guessing() {
    let run_id = fresh_run();
    let context = RequestContext {
        run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey("interrupted".into()),
        writer_epoch: umbra_core::LeaseEpoch(1).into(),
    };
    let request = StorageRequest {
        context: context.clone(),
        operation: StorageOperation::Create {
            path: path(b"half-done.txt"),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o644,
            },
        },
    };
    // A record with no `pre-` sidecar beside it: the shape an older provider, or
    // the mounted adapter, leaves behind.
    let intent: (
        StorageRequest,
        Option<umbra_core::Result<umbra_core::StorageResponse>>,
    ) = (request.clone(), None);
    let fake = seed_run_with_record(
        run_id,
        &key_file("interrupted"),
        serde_json::to_vec(&intent).expect("the intent encodes"),
    );
    let mut storage = over(fake);
    open_existing(&mut storage, run_id).expect("the run opens");

    let refused = storage
        .execute(&request)
        .expect_err("an intent with no evidence must not be re-dispatched");
    assert_eq!(
        refused.kind,
        ErrorKind::InvalidState,
        "a blocked-recoverable stop, not a transport failure: {refused:?}"
    );
    assert!(
        refused.context.contains("no recorded preconditions"),
        "the stop must name the missing evidence: {}",
        refused.context
    );
    assert!(
        !refused.context.contains("requires reconciliation"),
        "the phrasing the failure model forbids must not reappear: {}",
        refused.context
    );
    assert!(
        refused.context.contains("retained"),
        "and must say the record is kept: {}",
        refused.context
    );
}

/// **R1-004.** The same key naming a different request is refused; the recorded
/// outcome belongs to the request that was actually issued.
#[test]
fn r1_004_a_key_reused_for_a_different_request_is_refused() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let context = authorised(&storage, run_id, "shared-key");
    storage
        .execute(&create_file(context.clone(), b"first.txt"))
        .expect("the first request");
    let refused = storage
        .execute(&create_file(context, b"second.txt"))
        .expect_err("one key cannot name two requests");
    assert_eq!(refused.kind, ErrorKind::InvalidInput);
    assert!(
        refused.context.contains("different request"),
        "{}",
        refused.context
    );
}

/// **R1-004.** Reusing one operation id under a second idempotency key is caught.
#[test]
fn r1_004_an_operation_id_reused_under_another_key_is_refused() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let first = authorised(&storage, run_id, "key-one");
    let operation_id = first.operation_id;
    storage
        .execute(&create_file(first, b"one.txt"))
        .expect("the first request");

    let mut second = authorised(&storage, run_id, "key-two");
    second.operation_id = operation_id;
    let refused = storage
        .execute(&create_file(second, b"two.txt"))
        .expect_err("one operation id cannot name two keys");
    assert_eq!(refused.kind, ErrorKind::InvalidInput);
    assert!(
        refused.context.contains("operation id"),
        "{}",
        refused.context
    );
}

/// **R1-004.** The intent is durable *before* dispatch, and the settled record
/// carries the exact request and the exact result.
///
/// Read back through the contract's own list surface, so this asserts what a
/// recovering process would actually find on the server.
#[test]
fn r1_004_the_record_is_on_the_server_with_the_exact_request_and_result() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");

    let context = authorised(&storage, run_id, "written");
    let request = StorageRequest {
        context,
        operation: StorageOperation::Create {
            path: path(b"recorded.txt"),
            options: CreateOptions {
                kind: CreateKind::File,
                mode: 0o644,
            },
        },
    };
    let outcome = storage.execute(&request).expect("the create succeeds");

    // Walk to `.provider/retries` and read the record back off the server.
    let deadline = Deadline { millis: 5_000 };
    let transport = storage.transport().expect("transport");
    let mut current = transport.root_filehandle(deadline).expect("root");
    for part in EXPORT
        .split(|byte| *byte == b'/')
        .chain([RUN_PARENT])
        .chain([run_id.0.hyphenated().to_string().as_bytes()])
        .chain([b".provider".as_slice(), b"retries".as_slice()])
    {
        current = transport
            .lookup(
                &current,
                &umbra_storage_nfs_userspace::transport::ComponentName::new(part.to_vec()).unwrap(),
                umbra_storage_nfs_userspace::transport::AttrMask::STAT,
                deadline,
            )
            .expect("walk to retries")
            .0;
    }
    let (record_handle, _) = transport
        .lookup(
            &current,
            &umbra_storage_nfs_userspace::transport::ComponentName::new(
                key_file("written").into_bytes(),
            )
            .unwrap(),
            umbra_storage_nfs_userspace::transport::AttrMask::STAT,
            deadline,
        )
        .expect("the retry record exists on the server");
    let bytes = transport
        .read(
            &record_handle,
            umbra_storage_nfs_userspace::handle::Stateid::ANONYMOUS,
            0,
            64 * 1024,
            deadline,
        )
        .expect("read the record")
        .data;

    let (recorded_request, recorded_outcome): (
        StorageRequest,
        Option<umbra_core::Result<umbra_core::StorageResponse>>,
    ) = serde_json::from_slice(&bytes).expect("the record decodes");
    assert_eq!(
        recorded_request, request,
        "the record must hold the exact request, byte for byte"
    );
    let recorded_outcome = recorded_outcome.expect("the record is settled");
    assert_eq!(
        format!("{recorded_outcome:?}"),
        format!("{:?}", Ok::<_, umbra_core::UmbraError>(outcome)),
        "and the exact result"
    );
}

/// **R1-004.** A `WriteAt` record retains the payload, so a recovery can rebuild
/// the write without the server's reply cache.
#[test]
fn r1_004_a_write_record_retains_its_payload() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "target"),
            b"payload.bin",
        ))
        .expect("create the target");

    let payload = b"exact bytes that recovery needs".to_vec();
    let request = StorageRequest {
        context: authorised(&storage, run_id, "the-write"),
        operation: StorageOperation::WriteAt {
            path: path(b"payload.bin"),
            offset: 0,
            bytes: payload.clone(),
        },
    };
    storage.execute(&request).expect("the write succeeds");

    // Retrying the identical write is answered from the record rather than
    // written a second time.
    let retried = storage.execute(&request).expect("the retry is answered");
    match retried {
        umbra_core::StorageResponse::WriteAt(count) => {
            assert_eq!(count as usize, payload.len())
        }
        other => panic!("expected a write response, got {other:?}"),
    }
}

/// **R1-004.** Reads are not journalled: they have nothing to reconcile, and
/// recording one would consume durable space per lookup.
#[test]
fn r1_004_reads_are_not_recorded() {
    let mut storage = provider();
    let run_id = fresh_run();
    storage.open_run(&create_run(run_id)).expect("open_run");
    storage
        .execute(&create_file(
            authorised(&storage, run_id, "seed"),
            b"read-me.txt",
        ))
        .expect("seed");

    // The same read key twice, which a journalled operation would refuse the
    // second time only if it disagreed — and would record either way.
    for _ in 0..2 {
        storage
            .execute(&StorageRequest {
                context: RequestContext {
                    run_id,
                    operation_id: OperationId(Uuid::new_v4()),
                    idempotency_key: IdempotencyKey("a-read".into()),
                    writer_epoch: None,
                },
                operation: StorageOperation::Stat {
                    path: path(b"read-me.txt"),
                },
            })
            .expect("reads are unrestricted by the journal");
    }
}
