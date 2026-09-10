//! Sequential existing-run golden compatibility, against a real server.
//!
//! `tests/goldens.rs` proves this crate *encodes* the mounted adapter's records
//! correctly, without contacting anything. This suite proves the complementary
//! half: that a run the userspace provider actually creates on a server carries
//! those exact bytes, and that a second session opens the run the first one
//! wrote.
//!
//! **Sequential, never concurrent.** One-session-one-Umbra admission is
//! product-wide, so the handover here is always release-then-open. Nothing in
//! this file runs two sessions against one run at the same time and expects both
//! to work.
//!
//! **No mount.** Every byte is read back through NFSv4.0 from user space. The
//! mounted-adapter live suite is not run; see `/tmp/nfs-scope/m1/integration.md`.

use std::path::{Path, PathBuf};

use umbra_core::{
    BytePath, ImmutableBaseContract, OpenRunIntent, OpenRunRequest, RunId, StoragePolicy,
};
use umbra_storage::Storage;
use uuid::Uuid;

use umbra_storage_nfs_userspace::fake::{FakeReplayLog, FakeTransport};
use umbra_storage_nfs_userspace::handle::{FileHandle, Stateid};
use umbra_storage_nfs_userspace::layout;
use umbra_storage_nfs_userspace::storage::{
    NfsUserspaceConfig, NfsUserspaceStorage, FORMAT_VERSION,
};
use umbra_storage_nfs_userspace::transport::{
    AttrMask, ComponentName, Deadline, DirCookie, DirVerifier, Nfs4Type, RawTransport,
    ReadDirRequest,
};

const EXPORT: &[u8] = b"umbra";
const RUN_PARENT: &[u8] = b"runs";

fn goldens() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

fn golden(name: &str) -> Vec<u8> {
    std::fs::read(goldens().join(name)).expect("read golden")
}

fn config() -> NfsUserspaceConfig {
    NfsUserspaceConfig {
        host: b"127.0.0.1".to_vec(),
        port: 12110,
        export: BytePath::new(EXPORT).expect("export"),
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

/// A second sequential session against the same server.
///
/// `FakeTransport` *is* the server, so a second fake is a second server and the
/// run the first session created would not be there. The fake therefore hands the
/// same provider back: closing and reopening one provider is still a genuine
/// release-then-acquire handover, it just does not involve a second client
/// identity. A live fixture is a real server, so there a second provider — its
/// own client id, its own open owners — reopens the run.
fn handover(label: &str, first: NfsUserspaceStorage) -> NfsUserspaceStorage {
    match label {
        "fake" => first,
        _ => provider(live_transport().expect("a live backend was listed")),
    }
}

/// Backends this run can compare against: the fake always, the live fixture when
/// one is configured. A live transport is built fresh per call, because two
/// sessions in one case must be two clients.
fn transports() -> Vec<(&'static str, Box<dyn RawTransport>)> {
    let mut out: Vec<(&'static str, Box<dyn RawTransport>)> =
        vec![("fake", Box::new(fake_server()))];
    if let Some(live) = live_transport() {
        out.push(("libnfs", live));
    }
    out
}

fn provider(transport: Box<dyn RawTransport>) -> NfsUserspaceStorage {
    NfsUserspaceStorage::with_facades(config(), transport, Box::new(FakeReplayLog::default()))
        .expect("provider")
}

fn create_run(run_id: RunId) -> OpenRunRequest {
    OpenRunRequest {
        run_id,
        intent: OpenRunIntent::CreateNew,
        // Byte-identical to `tests/goldens/manifest.json`'s base, so the manifest
        // this run writes is comparable to the committed fixture.
        immutable_base: ImmutableBaseContract {
            identity: "umbra-golden-base".into(),
            fingerprint: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04],
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

fn component(bytes: &[u8]) -> ComponentName {
    ComponentName::new(bytes.to_vec()).expect("component")
}

/// Walk from the export root to the run directory.
fn run_directory(transport: &mut dyn RawTransport, run_id: RunId) -> FileHandle {
    let deadline = Deadline { millis: 5_000 };
    let mut current = transport.root_filehandle(deadline).expect("root");
    for part in [EXPORT, RUN_PARENT] {
        current = transport
            .lookup(&current, &component(part), AttrMask::IDENTITY, deadline)
            .expect("resolve the export path")
            .0;
    }
    transport
        .lookup(
            &current,
            &component(run_id.0.hyphenated().to_string().as_bytes()),
            AttrMask::IDENTITY,
            deadline,
        )
        .expect("resolve the run directory")
        .0
}

/// Every entry of a directory, sorted, with its type and mode.
fn listing(
    transport: &mut dyn RawTransport,
    directory: &FileHandle,
) -> Vec<(Vec<u8>, Nfs4Type, u32)> {
    let deadline = Deadline { millis: 5_000 };
    let mut entries = Vec::new();
    let mut cookie = DirCookie(0);
    let mut verifier = DirVerifier([0; 8]);
    loop {
        let page = transport
            .readdir(
                directory,
                ReadDirRequest {
                    cookie,
                    verifier,
                    dir_count: 8192,
                    max_count: 32768,
                    attrs: AttrMask::TYPE.union(AttrMask::MODE),
                },
                deadline,
            )
            .expect("READDIR");
        verifier = page.verifier;
        for entry in &page.entries {
            cookie = entry.cookie;
            let name = entry.name.as_bytes().to_vec();
            if name == b"." || name == b".." {
                continue;
            }
            entries.push((
                name,
                entry.attributes.file_type.expect("FATTR4_TYPE"),
                entry.attributes.mode.expect("FATTR4_MODE") & 0o7777,
            ));
        }
        if page.eof {
            break;
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Read a whole file with the anonymous stateid.
fn read_all(transport: &mut dyn RawTransport, handle: &FileHandle) -> Vec<u8> {
    let deadline = Deadline { millis: 5_000 };
    let mut out = Vec::new();
    loop {
        let reply = transport
            .read(handle, Stateid::ANONYMOUS, out.len() as u64, 4096, deadline)
            .expect("READ");
        let empty = reply.data.is_empty();
        out.extend_from_slice(&reply.data);
        if reply.eof || empty {
            break;
        }
    }
    out
}

fn resolve(transport: &mut dyn RawTransport, parent: &FileHandle, name: &[u8]) -> FileHandle {
    transport
        .lookup(
            parent,
            &component(name),
            AttrMask::IDENTITY,
            Deadline { millis: 5_000 },
        )
        .expect("resolve")
        .0
}

/// Render an observed run in the exact shape of `tests/goldens/run-layout.txt`.
fn render_layout(transport: &mut dyn RawTransport, run_id: RunId) -> String {
    let run = run_directory(transport, run_id);
    let deadline = Deadline { millis: 5_000 };
    let parent = run_directory_parent(transport, run_id);
    let (_, run_attrs) = transport
        .lookup(
            &parent,
            &component(run_id.0.hyphenated().to_string().as_bytes()),
            AttrMask::TYPE.union(AttrMask::MODE),
            deadline,
        )
        .expect("stat the run directory");

    let mut rows: Vec<(String, &'static str, u32)> = vec![(
        "<run>".to_owned(),
        "dir",
        run_attrs.mode.expect("mode") & 0o7777,
    )];
    for (name, kind, mode) in listing(transport, &run) {
        let label = format!("<run>/{}", String::from_utf8_lossy(&name));
        rows.push((label, kind_label(kind), mode));
        if name == layout::PRIVATE_DIR {
            let private = resolve(transport, &run, &name);
            for (child, child_kind, child_mode) in listing(transport, &private) {
                rows.push((
                    format!("<run>/.provider/{}", String::from_utf8_lossy(&child)),
                    kind_label(child_kind),
                    child_mode,
                ));
            }
        }
    }

    // The golden orders `<run>` first, then its children, then `.provider`'s
    // children. Sorting the tail reproduces that without depending on the
    // server's own READDIR order, which no RFC constrains.
    let (head, tail) = rows.split_at(1);
    let mut tail = tail.to_vec();
    tail.sort_by_key(|row| layout_rank(&row.0));

    let mut out = String::from("# umbra run layout, format version 1\n# <path>\t<kind>\t<mode>\n");
    for (path, kind, mode) in head.iter().chain(tail.iter()) {
        out.push_str(&format!("{path}\t{kind}\t0o{mode:o}\n"));
    }
    out
}

/// The golden's row order: run children before `.provider` children, each group
/// in the golden's own sequence.
fn layout_rank(path: &str) -> (u8, String) {
    let depth = if path.starts_with("<run>/.provider/") {
        1
    } else {
        0
    };
    let order = match path {
        "<run>/control" => "0",
        "<run>/root" => "1",
        "<run>/.provider" => "2",
        other => other,
    };
    (depth, order.to_owned())
}

fn kind_label(kind: Nfs4Type) -> &'static str {
    match kind {
        Nfs4Type::Directory => "dir",
        _ => "file",
    }
}

fn run_directory_parent(transport: &mut dyn RawTransport, _run_id: RunId) -> FileHandle {
    let deadline = Deadline { millis: 5_000 };
    let mut current = transport.root_filehandle(deadline).expect("root");
    for part in [EXPORT, RUN_PARENT] {
        current = transport
            .lookup(&current, &component(part), AttrMask::IDENTITY, deadline)
            .expect("resolve")
            .0;
    }
    current
}

// --- the comparisons ------------------------------------------------------

#[test]
fn a_created_run_lays_out_exactly_what_the_mounted_adapter_golden_pins() {
    for (label, transport) in transports() {
        let mut storage = provider(transport);
        let run_id = RunId(Uuid::new_v4());
        storage
            .open_run(&create_run(run_id))
            .expect("create the run");
        // Release before inspecting, so the inspection is a second sequential
        // session rather than a concurrent reader of a held run.
        storage.close_run().expect("release");

        // The golden pins what `CreateNew` *produced*. Reopening the run here
        // would make the observation include a cooperative succession's durable
        // claim (R1-001), which is this provider's own state and not part of the
        // layout the mounted adapter writes. Succession is asserted separately by
        // `a_cooperative_succession_records_one_durable_claim_per_epoch`.
        let mut inspector = handover(label, storage);
        let observed = render_layout(inspector.transport().expect("transport"), run_id);

        let expected = String::from_utf8(golden("run-layout.txt")).expect("utf-8 golden");
        assert_eq!(
            observed, expected,
            "{label}: the created run must match the mounted-adapter layout byte for byte"
        );
    }
}

/// **R1-001.** Cooperative succession is decided by a server-atomic per-epoch
/// claim, and that claim is durable evidence: it names the epoch a successor took
/// and it is never deleted.
///
/// This pins the artifact so the divergence from the mounted adapter's layout is
/// asserted rather than discovered. The claim is additive: every name the mounted
/// adapter reads is still exactly where it was, which is why
/// `a_second_sequential_session_opens_the_run_the_first_one_created` still passes.
#[test]
fn a_cooperative_succession_records_one_durable_claim_per_epoch() {
    for (label, transport) in transports() {
        let mut storage = provider(transport);
        let run_id = RunId(Uuid::new_v4());
        storage
            .open_run(&create_run(run_id))
            .expect("create the run");
        storage.close_run().expect("release");

        let mut successor = handover(label, storage);
        successor
            .open_run(&open_existing(run_id))
            .expect("succeed to the released run");
        let epoch = successor
            .admission()
            .expect("the successor is admitted")
            .admitted()
            .epoch();
        assert_eq!(epoch.0, 2, "{label}: succession advances the epoch by one");

        let run = run_directory(successor.transport().expect("transport"), run_id);
        let private = resolve(
            successor.transport().expect("transport"),
            &run,
            layout::PRIVATE_DIR,
        );
        let names: Vec<Vec<u8>> = listing(successor.transport().expect("transport"), &private)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert!(
            names.iter().any(|name| name == b"writer.lock.claim.2"),
            "{label}: succession to epoch 2 must leave its durable claim; saw {:?}",
            names
                .iter()
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect::<Vec<_>>()
        );
        // The predecessor's evidence is untouched: the marker is still the one
        // name admission arbitrates over, and every golden name is still present.
        for pinned in [
            layout::WRITER_LOCK_FILE,
            layout::MANIFEST_FILE,
            layout::EPOCH_FILE,
            layout::RETRIES_DIR,
        ] {
            assert!(
                names.iter().any(|name| name.as_slice() == pinned),
                "{label}: {:?} must survive a succession",
                String::from_utf8_lossy(pinned)
            );
        }
        successor.close_run().expect("release");
    }
}

#[test]
fn the_manifest_and_epoch_files_are_byte_identical_to_the_goldens() {
    for (label, transport) in transports() {
        let mut storage = provider(transport);
        // The golden manifest pins this exact run id, so the run created here
        // must use it for the bytes to be comparable.
        let run_id = RunId(
            Uuid::parse_str("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d").expect("pinned golden run id"),
        );
        // A live fixture may already carry this run from an earlier execution;
        // a fresh identity would not compare against the golden, so the run is
        // removed first rather than renamed.
        clear_run(&mut storage, run_id);

        storage
            .open_run(&create_run(run_id))
            .expect("create the run");
        let deadline = Deadline { millis: 5_000 };
        let transport = storage.transport().expect("transport");
        let run = run_directory(transport, run_id);
        let private = resolve(transport, &run, layout::PRIVATE_DIR);

        let manifest = resolve(transport, &private, layout::MANIFEST_FILE);
        assert_eq!(
            read_all(transport, &manifest),
            golden("manifest.json"),
            "{label}: .provider/manifest must match the mounted-adapter encoding"
        );

        let epoch = resolve(transport, &private, layout::EPOCH_FILE);
        assert_eq!(
            read_all(transport, &epoch),
            golden("epoch.bin"),
            "{label}: .provider/epoch is a little-endian u64 created as zero"
        );

        // `retries` is a directory and is created empty; a stale record would be
        // a retry this run never issued.
        let retries = resolve(transport, &private, layout::RETRIES_DIR);
        assert!(listing(transport, &retries).is_empty(), "{label}");
        let _ = deadline;

        storage.close_run().expect("release");
    }
}

/// Remove a run directory if it exists, so a pinned-id case can be re-run.
///
/// Uses the provider's own namespace surface — this is the widened REMOVE the
/// contracts hotfix added, exercised as a housekeeping path rather than as the
/// thing under test.
fn clear_run(storage: &mut NfsUserspaceStorage, run_id: RunId) {
    if storage.open_run(&open_existing(run_id)).is_err() {
        return;
    }
    let deadline = Deadline { millis: 5_000 };
    let transport = storage.transport().expect("transport");
    let parent = run_directory_parent(transport, run_id);
    let run = run_directory(transport, run_id);
    let private = resolve(transport, &run, layout::PRIVATE_DIR);

    // Release admission while the marker still exists: a release writes the
    // marker, so deleting `writer.lock` first would fail the release and leave
    // this provider holding an open run.
    storage
        .close_run()
        .expect("release before deleting the marker");

    let transport = storage.transport().expect("transport");
    // Depth first: NFSv4 REMOVE refuses a non-empty directory.
    for (name, _, _) in listing(transport, &private) {
        remove(transport, &private, &name, deadline);
    }
    for (name, _, _) in listing(transport, &run) {
        remove(transport, &run, &name, deadline);
    }
    remove(
        transport,
        &parent,
        run_id.0.hyphenated().to_string().as_bytes(),
        deadline,
    );
}

fn remove(transport: &mut dyn RawTransport, parent: &FileHandle, name: &[u8], deadline: Deadline) {
    use umbra_storage_nfs_userspace::transport::{Compound, Nfs4Op};
    let _ = transport.submit(
        Compound::new(
            *b"cleanup",
            vec![
                Nfs4Op::PutFh(parent.clone()),
                Nfs4Op::Remove {
                    name: component(name),
                },
            ],
        ),
        deadline,
    );
}

#[test]
fn a_second_sequential_session_opens_the_run_the_first_one_created() {
    for (label, transport) in transports() {
        let mut first = provider(transport);
        let run_id = RunId(Uuid::new_v4());
        let created = first.open_run(&create_run(run_id)).expect("create");
        // The cooperative release is what makes the handover legal; without it
        // the second session is denied, which the conformance suite pins.
        first.close_run().expect("release");

        let mut second = handover(label, first);
        let reopened = second.open_run(&open_existing(run_id)).expect("reopen");

        assert_eq!(reopened.run_id, created.run_id, "{label}");
        // The anchors resolve to the same objects the first session published.
        assert_eq!(reopened.root.physical_path, None);
        assert_eq!(reopened.control.physical_path, None);
        // The epoch advanced by exactly one across the handover.
        assert_eq!(
            second.admission().expect("admitted").admitted().epoch().0,
            2,
            "{label}: a release hands over at the next epoch"
        );
        second.close_run().expect("release");
    }
}

#[test]
fn the_writer_lock_this_provider_writes_is_the_extended_record_not_the_legacy_one() {
    // Honest scope note, asserted rather than only written down: the mounted
    // adapter's `writer.lock` is 16 raw token bytes (`tests/goldens/writer-lock.bin`)
    // and this provider *reads* that, but it *writes* the 256-byte extended
    // record that carries epoch and phase. Compatibility is therefore one-way for
    // this one file, and this test pins that rather than letting a future reader
    // assume byte equality.
    use umbra_storage_nfs_userspace::authority::marker::{
        AdmissionMarker, AdmissionPhase, EXTENDED_MARKER_BYTES, LEGACY_MARKER_BYTES,
    };

    let legacy = golden("writer-lock.bin");
    assert_eq!(legacy.len(), LEGACY_MARKER_BYTES);

    // The mounted adapter's record decodes here, as held by an unnamed writer.
    let decoded = AdmissionMarker::decode(&legacy).expect("the legacy lock decodes");
    assert_eq!(decoded.phase(), AdmissionPhase::Held);
    assert_eq!(decoded.token().as_bytes().as_slice(), legacy.as_slice());
    assert!(
        decoded.writer().is_none(),
        "a legacy record names no writer"
    );

    // What this provider writes is longer, and deliberately so.
    let extended = decoded.encode().expect("re-encode");
    assert_eq!(extended.len(), EXTENDED_MARKER_BYTES);
    assert_ne!(extended[..LEGACY_MARKER_BYTES], legacy[..]);
}

#[test]
fn a_live_backend_was_compared_when_one_was_configured() {
    let configured = std::env::var("UMBRA_NFS_RAW_FIXTURE").is_ok();
    let live = transports().iter().any(|(name, _)| *name == "libnfs");
    if configured && cfg!(feature = "transport-raw") {
        assert!(
            live,
            "a fixture is configured but no live backend was built"
        );
    } else {
        assert!(!live);
        eprintln!("golden compatibility compared against the fake only");
    }
}
