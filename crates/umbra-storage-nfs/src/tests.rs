use super::*;

#[test]
fn unsupported_flush_scope_is_rejected_without_mount_io() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/nonexistent-nfs-mount"));
    let result = storage.flush(&FlushRequest {
        context: RequestContext {
            run_id: RunId(Uuid::new_v4()),
            writer_epoch: Some(LeaseEpoch(1)),
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey("flush".into()),
        },
        scope: FlushScope::Data { objects: vec![] },
    });
    assert_eq!(result.unwrap_err().kind, ErrorKind::UnsupportedCapability);
    assert!(storage.health.check().is_ok());
}

#[test]
fn known_failure_blocks_close_and_reopen_without_erasing_evidence() {
    let mut storage = NfsStorage::new(NfsStorageConfig::new("/nonexistent-nfs-mount"));
    let failure = storage.health.fail(io(
        "sync_all",
        std::io::Error::from_raw_os_error(libc::ECONNRESET),
    ));
    assert_eq!(storage.close_run().unwrap_err(), failure);
    let request = OpenRunRequest {
        run_id: RunId(Uuid::new_v4()),
        intent: OpenRunIntent::OpenExisting,
        immutable_base: ImmutableBaseContract {
            identity: "base".into(),
            fingerprint: vec![],
        },
        policy: StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        },
    };
    assert_eq!(storage.open_run(&request).unwrap_err(), failure);
    assert_eq!(storage.capabilities().durability, Durability::Local);
}
