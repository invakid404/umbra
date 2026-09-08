use umbra_core::*;
use umbra_storage::Storage;
use umbra_storage_tar::{TarStorage, TarStorageConfig};
use uuid::Uuid;

pub fn request() -> OpenRunRequest {
    OpenRunRequest {
        run_id: RunId(Uuid::new_v4()),
        intent: OpenRunIntent::CreateNew,
        immutable_base: ImmutableBaseContract {
            identity: "test-base".into(),
            fingerprint: vec![1, 2, 3],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        },
    }
}
pub fn writer(run_id: RunId) -> AcquireWriterRequest {
    AcquireWriterRequest {
        run_id,
        writer_id: WriterId(Uuid::new_v4().to_string()),
        takeover: TakeoverPolicy::Refuse,
    }
}
pub fn ctx(lease: &WriterLease) -> RequestContext {
    RequestContext {
        run_id: lease.run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: Some(lease.epoch),
    }
}
pub fn path(bytes: &[u8]) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec()).unwrap()
}
pub fn file() -> CreateOptions {
    CreateOptions {
        kind: CreateKind::File,
        mode: 0o600,
    }
}
pub fn directory() -> CreateOptions {
    CreateOptions {
        kind: CreateKind::Directory,
        mode: 0o750,
    }
}
pub fn setup() -> (tempfile::TempDir, TarStorage, OpenRunRequest, WriterLease) {
    let temp = tempfile::tempdir().unwrap();
    let mut s = TarStorage::new(TarStorageConfig::new(temp.path().join("run.tar")));
    let req = request();
    s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    (temp, s, req, lease)
}
pub fn flush(s: &mut dyn Storage, lease: &WriterLease) -> DurabilityReceipt {
    s.flush(&FlushRequest {
        context: ctx(lease),
        scope: FlushScope::EntireRun,
    })
    .unwrap()
}
