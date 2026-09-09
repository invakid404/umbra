//! Golden layout and record fixtures for sequential existing-run compatibility.
//!
//! The mounted `nfs` adapter and this userspace provider must agree byte for byte
//! on what an existing run looks like on the server, so a run created by one can
//! later be opened by the other. Sequential, not concurrent: one-session-one-Umbra
//! admission is product-wide.
//!
//! Each golden is rebuilt here from the same `umbra-core` DTOs the mounted adapter
//! serialises, then compared against the committed bytes. A drift in core's
//! encoding therefore fails this test rather than surfacing on a live run. The
//! layout names come from `crates/umbra-storage-nfs/src/lib.rs`, which creates
//! `.provider/{retries,epoch,manifest}` on `CreateNew` and writes retry records as
//! `(StorageRequest, Option<Result<StorageResponse>>)`.
//!
//! Nothing here contacts a server. These are byte fixtures; the operations node
//! asserts a live provider reproduces them.
//!
//! Regenerate after an intentional encoding change with
//! `UMBRA_GOLDEN_UPDATE=1 cargo test -p umbra-storage-nfs-userspace --test goldens`,
//! then review the diff: an unreviewed regeneration defeats the fixture.

use std::path::{Path, PathBuf};

use umbra_core::{
    ErrorKind, IdempotencyKey, ImmutableBaseContract, LeaseEpoch, OperationId, RequestContext,
    Result, RunId, StorageAnchor, StorageOperation, StoragePath, StorageRequest, StorageResponse,
    UmbraError,
};
use umbra_storage_nfs_userspace::layout;
use uuid::Uuid;

/// Pinned run identity. Chosen once; never regenerated.
const RUN_UUID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
/// Pinned operation identity for the write record fixtures.
const OPERATION_UUID: &str = "11112222-3333-4444-8555-666677778888";
/// Pinned writer token, the 16 raw bytes `writer.lock` holds.
const WRITER_TOKEN_UUID: &str = "99887766-5544-4332-8110-aabbccddeeff";
/// Pinned idempotency key for the write record fixtures.
const IDEMPOTENCY_KEY: &str = "umbra-golden-write-1";
/// Format version both adapters read and write.
const FORMAT_VERSION: u32 = 1;

fn goldens() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

/// Compare against the committed fixture, or rewrite it when explicitly asked.
fn assert_golden(name: &str, actual: &[u8]) {
    let path = goldens().join(name);
    if std::env::var_os("UMBRA_GOLDEN_UPDATE").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|error| {
        panic!("missing golden {}: {error}", path.display());
    });
    assert_eq!(
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(&expected),
        "golden {name} drifted"
    );
    assert_eq!(actual, expected, "golden {name} drifted in raw bytes");
}

fn run_id() -> RunId {
    RunId(Uuid::parse_str(RUN_UUID).unwrap())
}

fn immutable_base() -> ImmutableBaseContract {
    ImmutableBaseContract {
        identity: "umbra-golden-base".into(),
        fingerprint: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04],
    }
}

fn request() -> StorageRequest {
    StorageRequest {
        context: RequestContext {
            run_id: run_id(),
            operation_id: OperationId(Uuid::parse_str(OPERATION_UUID).unwrap()),
            idempotency_key: IdempotencyKey(IDEMPOTENCY_KEY.into()),
            writer_epoch: Some(LeaseEpoch(1)),
        },
        // A non-UTF-8 component, because byte names must survive both adapters.
        operation: StorageOperation::WriteAt {
            path: StoragePath::new(StorageAnchor::Root, b"work/\xffoutput.bin".to_vec()).unwrap(),
            offset: 0,
            bytes: b"golden".to_vec(),
        },
    }
}

type Record = (StorageRequest, Option<Result<StorageResponse>>);

#[test]
fn run_layout_matches_the_mounted_adapter() {
    // Every name and mode an existing run exposes, in the order a reader walks
    // them. `<run>` is `<export>/<run_parent>/<run-id>`; the run id is the
    // hyphenated UUID string, never a physical host path.
    let mut listing = String::new();
    listing.push_str("# umbra run layout, format version 1\n");
    listing.push_str("# <path>\t<kind>\t<mode>\n");
    for (path, kind, mode) in [
        ("<run>", "dir", 0o700),
        ("<run>/control", "dir", 0o700),
        ("<run>/root", "dir", 0o700),
        ("<run>/.provider", "dir", 0o700),
        ("<run>/.provider/epoch", "file", 0o600),
        ("<run>/.provider/manifest", "file", 0o600),
        ("<run>/.provider/retries", "dir", 0o700),
        ("<run>/.provider/writer.lock", "file", 0o600),
    ] {
        listing.push_str(&format!("{path}\t{kind}\t0o{mode:o}\n"));
    }
    assert_golden("run-layout.txt", listing.as_bytes());

    // The constants the crate exports must be the same names this fixture pins.
    assert!(listing.contains(std::str::from_utf8(layout::PRIVATE_DIR).unwrap()));
    assert!(listing.contains(std::str::from_utf8(layout::RETRIES_DIR).unwrap()));
    assert!(listing.contains(std::str::from_utf8(layout::EPOCH_FILE).unwrap()));
    assert!(listing.contains(std::str::from_utf8(layout::MANIFEST_FILE).unwrap()));
    assert!(listing.contains(std::str::from_utf8(layout::WRITER_LOCK_FILE).unwrap()));
    assert_eq!(layout::DIRECTORY_MODE, 0o700);
}

#[test]
fn manifest_bytes_match_the_mounted_adapter_encoding() {
    // Mirrors `serde_json::to_vec(&(run_id, &immutable_base, format_version))`.
    let manifest = serde_json::to_vec(&(run_id(), &immutable_base(), FORMAT_VERSION)).unwrap();
    assert_golden("manifest.json", &manifest);

    // A different base, run or format must not decode as this manifest: the
    // comparison is byte equality, not a structural subset.
    let other = serde_json::to_vec(&(run_id(), &immutable_base(), FORMAT_VERSION + 1)).unwrap();
    assert_ne!(manifest, other);
}

#[test]
fn epoch_and_writer_lock_are_fixed_width_binaries() {
    // `.provider/epoch` is a little-endian u64, created as zero.
    assert_golden("epoch.bin", &0u64.to_le_bytes());
    // `.provider/writer.lock` holds the raw 16 UUID bytes of the writer token,
    // created exclusively. Its presence is authority; its age never is.
    let token = Uuid::parse_str(WRITER_TOKEN_UUID).unwrap();
    assert_golden("writer-lock.bin", token.as_bytes());
    assert_eq!(token.as_bytes().len(), 16);
}

#[test]
fn retry_records_pin_the_intent_and_both_settled_outcomes() {
    let intent: Record = (request(), None);
    assert_golden("retry-intent.json", &serde_json::to_vec(&intent).unwrap());

    let completed: Record = (request(), Some(Ok(StorageResponse::WriteAt(6))));
    assert_golden(
        "retry-result-ok.json",
        &serde_json::to_vec(&completed).unwrap(),
    );

    // A recorded failure is the settled answer for its key forever, so its bytes
    // are pinned too. Losing them would let a retry be reinterpreted.
    let failed: Record = (
        request(),
        Some(Err(UmbraError::new(
            ErrorKind::Io,
            "write_at",
            "protocol: NFS4ERR 28 at Write (compound index 1)",
        ))),
    );
    assert_golden(
        "retry-result-err.json",
        &serde_json::to_vec(&failed).unwrap(),
    );
}

#[test]
fn retry_file_names_are_derived_the_way_the_mounted_adapter_derives_them() {
    let key = IdempotencyKey(IDEMPOTENCY_KEY.into());
    let hex: String = key
        .0
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let key_file = format!("{}{hex}", layout::RETRY_KEY_PREFIX);
    let operation = OperationId(Uuid::parse_str(OPERATION_UUID).unwrap());
    let op_file = format!("{}{}", layout::RETRY_OP_PREFIX, operation.0);

    // The op-id index file holds the key file's name, which is how a reused
    // operation id under a different key is detected.
    let mut names = String::new();
    names.push_str("# <file>\t<contents>\n");
    names.push_str(&format!("{key_file}\t<retry record JSON>\n"));
    names.push_str(&format!("{op_file}\t{key_file}\n"));
    assert_golden("retry-file-names.txt", names.as_bytes());

    assert!(key_file.starts_with("key-"));
    assert!(op_file.starts_with("op-"));
    assert_eq!(hex.len(), IDEMPOTENCY_KEY.len() * 2);
}

#[test]
fn goldens_are_committed_and_no_update_flag_leaked_into_the_run() {
    // A run with the update flag set rewrites fixtures instead of checking them,
    // so it must never be the state a normal test run observes.
    assert!(
        std::env::var_os("UMBRA_GOLDEN_UPDATE").is_none(),
        "UMBRA_GOLDEN_UPDATE must not be set for a verifying run"
    );
    for name in [
        "run-layout.txt",
        "manifest.json",
        "epoch.bin",
        "writer-lock.bin",
        "retry-intent.json",
        "retry-result-ok.json",
        "retry-result-err.json",
        "retry-file-names.txt",
    ] {
        let path = goldens().join(name);
        assert!(path.is_file(), "missing golden {}", path.display());
    }
}
