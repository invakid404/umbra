//! In-crate test fixture: a fake server carrying one umbra run layout.
//!
//! Every module's unit tests build the same server shape here so a change to the
//! layout is made in one place. Nothing in this module reaches a socket, a host
//! path or a live Ganesha; `FakeTransport` is an in-memory shape fake.
//!
//! Unit tests mint a synthetic [`Session`] rather than running SETCLIENTID,
//! because they are testing this node's operations, not the client-id lifecycle
//! `raw_state` already owns and tests. The integration suite drives the real
//! establish path through [`StateSession`](crate::integration::StateSession).

use umbra_core::{
    BytePath, IdempotencyKey, ImmutableBaseContract, LeaseEpoch, OpenRunIntent, OpenRunRequest,
    OperationId, RequestContext, RunId, StoragePolicy,
};
use uuid::Uuid;

use crate::fake::{FakeReplayLog, FakeTransport};
use crate::handle::{ClientId, FileHandle, Session, SessionId};
use crate::ops::{MutationContext, Operations, OpsContext};
use crate::state::open_owner::OpenOwnerRegistry;
use crate::storage::{NfsUserspaceConfig, FORMAT_VERSION};
use crate::transport::{ConnectionEpoch, Deadline};

/// Export path below the server's pseudo-root.
pub(crate) const EXPORT: &[u8] = b"exports/umbra";
/// Directory holding run-id directories.
pub(crate) const RUN_PARENT: &[u8] = b"runs";
/// Pinned run identity, so a fixture never depends on a random value.
pub(crate) const RUN_UUID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

/// The run this fixture lays out.
pub(crate) fn run_id() -> RunId {
    RunId(Uuid::parse_str(RUN_UUID).expect("pinned run uuid"))
}

/// A configuration matching the fixture layout.
pub(crate) fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        port: 12108,
        export: BytePath::new(EXPORT).expect("export path"),
        run_parent: BytePath::new(RUN_PARENT).expect("run parent"),
        root_anchor: BytePath::new(b"root").expect("root anchor"),
        control_anchor: BytePath::new(b"control").expect("control anchor"),
        deadline: Deadline { millis: 5_000 },
    }
}

/// Handles of the fixture layout, for a test that wants to seed content.
pub(crate) struct Layout {
    /// The run directory.
    pub(crate) run: FileHandle,
    /// The tracee-visible root anchor.
    pub(crate) root: FileHandle,
    /// The control anchor.
    pub(crate) control: FileHandle,
}

/// A fake server holding `<export>/<run_parent>/<run_id>/{root,control,.provider}`.
pub(crate) fn server() -> (FakeTransport, Layout) {
    let mut fake = FakeTransport::new();
    let mut current = fake.root();
    for component in EXPORT.split(|byte| *byte == b'/') {
        current = fake.insert_directory(&current, component);
    }
    current = fake.insert_directory(&current, RUN_PARENT);
    let run = fake.insert_directory(&current, RUN_UUID.as_bytes());
    let root = fake.insert_directory(&run, b"root");
    let control = fake.insert_directory(&run, b"control");
    fake.insert_directory(&run, crate::layout::PRIVATE_DIR);
    (fake, Layout { run, root, control })
}

/// An `OPEN_EXISTING` request for the fixture run.
pub(crate) fn open_run_request() -> OpenRunRequest {
    OpenRunRequest {
        run_id: run_id(),
        intent: OpenRunIntent::OpenExisting,
        immutable_base: ImmutableBaseContract {
            identity: "umbra-fixture-base".into(),
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

/// Open the fixture run's operations surface.
pub(crate) fn operations(fake: &mut FakeTransport) -> Operations {
    Operations::open(
        fake,
        &config(),
        &open_run_request(),
        0,
        Deadline { millis: 5_000 },
    )
    .expect("the fixture layout opens")
}

/// A request context, with or without proven writer authority.
pub(crate) fn context(key: &str, epoch: Option<u64>) -> RequestContext {
    RequestContext {
        run_id: run_id(),
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(key.into()),
        writer_epoch: epoch.map(LeaseEpoch),
    }
}

/// A synthetic open-owner registry for a unit test.
pub(crate) fn owners() -> OpenOwnerRegistry {
    OpenOwnerRegistry::new(
        Session::establish(SessionId(1), ClientId(7), ConnectionEpoch(1)),
        b"umbra/fixture".to_vec(),
    )
}

/// A replay log with the default budget.
pub(crate) fn replay() -> FakeReplayLog {
    FakeReplayLog::default()
}

/// Build a request context bundle over the supplied facades.
///
/// `owners` present means the caller holds proven writer authority; absent means
/// it does not, which is how the read and mutation paths are separated.
pub(crate) fn ops_context<'a>(
    transport: &'a mut FakeTransport,
    replay: &'a mut FakeReplayLog,
    owners: Option<&'a mut OpenOwnerRegistry>,
) -> OpsContext<'a> {
    OpsContext {
        transport,
        replay,
        mutations: owners.map(|owners| MutationContext {
            owners,
            create_verifier: None,
            namespace: None,
        }),
        deadline: Deadline { millis: 5_000 },
    }
}
