//! `connect` against a live server: does the shipped provider bind a transport
//! and earn its capabilities, and does it refuse when it cannot?
//!
//! This suite exists because the provider *executable* used to bind nothing:
//! `connect` set `transport: None`, every `open_run` answered "no transport
//! facade is bound to this provider", and the crate named that `m1_integrate`'s
//! deferred seam. The tests below pin the two halves of closing it — a live
//! probe earns the capability names, and a configuration that names no reachable
//! export earns neither and fails rather than connecting quietly.
//!
//! Skipped unless `UMBRA_NFS_RAW_FIXTURE=<host>:<port>` names a live fixture, in
//! the established two-tier style: with `UMBRA_INTEGRATION_REQUIRED` set, a
//! missing fixture is a failure rather than a skip.
#![cfg(feature = "transport-raw")]

use umbra_core::{capabilities, BytePath, ErrorKind};
use umbra_storage::Storage;
use umbra_storage_nfs_userspace::storage::{NfsUserspaceConfig, NfsUserspaceStorage};
use umbra_storage_nfs_userspace::transport::Deadline;

/// Two-tier fixture input: skip by default, refuse to skip under
/// `UMBRA_INTEGRATION_REQUIRED`.
fn fixture() -> Option<(String, u16)> {
    let value = std::env::var("UMBRA_NFS_RAW_FIXTURE")
        .ok()
        .filter(|v| !v.is_empty());
    assert!(
        value.is_some() || std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
        "required integration needs UMBRA_NFS_RAW_FIXTURE"
    );
    let Some(target) = value else {
        eprintln!("SKIP: set UMBRA_NFS_RAW_FIXTURE=<host>:<port>");
        return None;
    };
    let (host, port) = target.rsplit_once(':').expect("host:port");
    Some((host.to_owned(), port.parse().expect("numeric port")))
}

/// The run layout this fixture carries: `<umbra>/<runs>/<run-id>/{root,control}`.
fn config(host: String, port: u16, export: &[u8], run_parent: &[u8]) -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: host.into_bytes(),
        port,
        export: BytePath::new(export.to_vec()).expect("export"),
        run_parent: BytePath::new(run_parent.to_vec()).expect("run parent"),
        root_anchor: BytePath::new(b"root".to_vec()).expect("root anchor"),
        control_anchor: BytePath::new(b"control".to_vec()).expect("control anchor"),
        deadline: Deadline { millis: 15_000 },
    }
}

#[test]
fn connect_binds_a_live_transport_and_earns_both_probed_capability_names() {
    let Some((host, port)) = fixture() else {
        return;
    };
    let storage = NfsUserspaceStorage::connect(config(host, port, b"umbra", b"runs"))
        .expect("connect to the live fixture");

    let features = storage.capabilities().features;
    assert!(
        features.contains(capabilities::STORAGE_USERSPACE_NFSV4_V1),
        "a probed export must earn the NFSv4 name, saw {features:?}"
    );
    assert!(
        features.contains(capabilities::STORAGE_USERSPACE_ROUTING_V1),
        "a probed export must earn the routing name, saw {features:?}"
    );
    // The routing name's whole claim: no kernel path exists for this backend, so
    // the names travel together rather than being independently settable.
    assert!(
        !storage.capabilities().kernel_shadow,
        "a userspace client never has a kernel shadow"
    );
}

#[test]
fn a_configuration_naming_no_reachable_run_parent_is_refused_before_any_run() {
    let Some((host, port)) = fixture() else {
        return;
    };
    // The export exists; the run parent does not. The probe must say so at
    // connect time rather than let a run get as far as taking a writer lease.
    let error = NfsUserspaceStorage::connect(config(
        host,
        port,
        b"umbra",
        b"umbra-no-such-run-parent-dir",
    ))
    .expect_err("an unreachable run parent must refuse the connection");
    assert!(
        matches!(
            error.kind,
            ErrorKind::StorageUnavailable | ErrorKind::NotFound
        ),
        "unexpected kind {:?}: {error}",
        error.kind
    );
}

#[test]
fn a_provider_built_over_facades_advertises_no_probed_capability() {
    // `with_facades` is the fake-transport seam the state-machine suites use. It
    // reaches no server, so it qualifies nothing — the property that keeps a
    // declared name a claim and a probe the only thing that settles it. This case
    // needs no fixture: that is the point.
    let storage = NfsUserspaceStorage::with_facades(
        config("127.0.0.1".to_owned(), 12345, b"umbra", b"runs"),
        Box::new(umbra_storage_nfs_userspace::fake::FakeTransport::new()),
        Box::new(umbra_storage_nfs_userspace::replay::InProcessReplayLog::default()),
    )
    .expect("the fake facades build a provider");
    let features = storage.capabilities().features;
    assert!(
        !features.contains(capabilities::STORAGE_USERSPACE_NFSV4_V1)
            && !features.contains(capabilities::STORAGE_USERSPACE_ROUTING_V1),
        "a fake-backed provider advertised a probed name: {features:?}"
    );
}

#[test]
fn an_open_run_s_binding_advertises_exactly_what_the_provider_advertises() {
    let Some((host, port)) = fixture() else {
        return;
    };
    let mut storage = NfsUserspaceStorage::connect(config(host, port, b"umbra", b"runs"))
        .expect("connect to the live fixture");
    let run_id = umbra_core::RunId(uuid::Uuid::new_v4());
    let binding = storage
        .open_run(&umbra_core::OpenRunRequest {
            run_id,
            intent: umbra_core::OpenRunIntent::CreateNew,
            immutable_base: umbra_core::ImmutableBaseContract {
                identity: "umbra-binding-capabilities".into(),
                fingerprint: vec![1, 2, 3, 4],
            },
            policy: umbra_core::StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        })
        .expect("open a fresh run");

    // Every consumer that adopts the binding -- `umbra-storage::provider::Proxy`
    // does, on every `open_run` -- must end up advertising what this provider
    // advertises. They used to differ: the binding came from
    // `Operations::capabilities`, which does not extend with `probed_features`,
    // so the proxy under-claimed against its own backend for the life of the run.
    assert_eq!(
        binding.capabilities,
        storage.capabilities(),
        "the binding and the provider disagree about this run's capabilities"
    );
    assert!(
        binding
            .capabilities
            .features
            .contains(capabilities::STORAGE_USERSPACE_NFSV4_V1),
        "a probed name is missing from the binding: {:?}",
        binding.capabilities.features
    );
    assert!(
        binding.capabilities.max_io_bytes > 0,
        "an open run must advertise a transfer bound, or every routed byte is refused"
    );

    storage.close_run().expect("close the run");
}
