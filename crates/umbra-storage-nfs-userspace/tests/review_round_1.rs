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
use umbra_storage_nfs_userspace::transport::{Deadline, FaultAction, FaultPoint};

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
