#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::ffi::{CString, OsStr};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use umbra_core::*;
use umbra_storage::Storage;
use umbra_storage_nfs::{NfsStorage, NfsStorageConfig};
use uuid::Uuid;

fn fixture() -> Option<(tempfile::TempDir, NfsStorageConfig)> {
    let Some(mount) = std::env::var_os("UMBRA_TEST_NFS_MOUNT") else {
        eprintln!("skipping live NFS test: UMBRA_TEST_NFS_MOUNT is unset");
        return None;
    };
    let dir = tempfile::Builder::new()
        .prefix("umbra-nfs-test-")
        .tempdir_in(&mount)
        .unwrap_or_else(|e| panic!("explicit live NFS mount {mount:?} is not writable: {e}"));
    let mut config = NfsStorageConfig::new(mount);
    config.run_parent = path(dir.path().file_name().unwrap().as_bytes());
    Some((dir, config))
}
fn request() -> OpenRunRequest {
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
fn writer(run: RunId) -> AcquireWriterRequest {
    AcquireWriterRequest {
        run_id: run,
        writer_id: WriterId(Uuid::new_v4().to_string()),
        takeover: TakeoverPolicy::Refuse,
    }
}
fn ctx(lease: &WriterLease) -> RequestContext {
    RequestContext {
        run_id: lease.run_id,
        operation_id: OperationId(Uuid::new_v4()),
        idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        writer_epoch: Some(lease.epoch),
    }
}
fn path(bytes: &[u8]) -> StoragePath {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec()).unwrap()
}
fn file() -> CreateOptions {
    CreateOptions {
        kind: CreateKind::File,
        mode: 0o600,
    }
}
fn physical(binding: &RuntimeDirectoryBinding) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(
        binding.physical_path.as_ref().unwrap().as_bytes(),
    ))
}

fn assert_local_barrier(receipt: &DurabilityReceipt, lease: &WriterLease) {
    assert_eq!(receipt.durability, Durability::Local);
    assert_eq!(receipt.run_id, lease.run_id);
    assert_eq!(receipt.writer_epoch, lease.epoch);
    assert_eq!(receipt.scope, FlushScope::EntireRun);
    let evidence = std::str::from_utf8(&receipt.evidence).unwrap();
    for expected in [
        "OS sync_all completed",
        "run files, directories and run parent",
        "NFS COMMIT/write-verifier recovery is managed by the kernel",
        "cannot be independently checked through File",
        "remote stable storage unqualified",
        "no durable client replica claimed",
    ] {
        assert!(
            evidence.contains(expected),
            "missing {expected:?}: {evidence}"
        );
    }
}

fn payload() -> Vec<u8> {
    // Exceeds the scratch mount's 32 KiB write size, with binary bytes and a tail.
    (0..256 * 1024 + 37).map(|i| (i % 251) as u8).collect()
}

#[test]
fn flush_external_writes_reports_kernel_boundary_and_rechecks_repeated_key() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let mut storage = NfsStorage::connect(config).unwrap();
    assert_eq!(storage.capabilities().durability, Durability::Local);
    let req = request();
    let binding = storage.open_run(&req).unwrap();
    let lease = storage.acquire_writer(&writer(req.run_id)).unwrap();
    let flush = FlushRequest {
        context: ctx(&lease),
        scope: FlushScope::EntireRun,
    };
    let mut expected = payload();
    let root = physical(&binding.root);
    fs::create_dir_all(root.join("nested")).unwrap();
    let filename = root.join("nested/payload");
    let mut original = fs::File::create(&filename).unwrap();
    original.write_all(&expected).unwrap();
    // Collect the original writer's sync error before the barrier, allowing
    // pending NFS attributes to settle. Keep the descriptor open: an open FD
    // alone does not violate quiescence. Readback cannot cover prior errors.
    original.sync_all().unwrap();
    assert_local_barrier(&storage.flush(&flush).unwrap(), &lease);
    assert_eq!(fs::read(&filename).unwrap(), expected);

    original.write_all(b"second generation").unwrap();
    expected.extend_from_slice(b"second generation");
    original.sync_all().unwrap();
    // A repeated key must perform a new barrier, never return cached evidence.
    assert_local_barrier(&storage.flush(&flush).unwrap(), &lease);
    drop(original);
    assert_eq!(fs::read(&filename).unwrap(), expected);

    // Readback can hit the client cache, so it cannot prove a repeated barrier
    // ran. An unsupported entry forces a fresh traversal to fail with this same
    // key; returning a cached successful receipt would incorrectly pass it.
    let fifo = root.join("unsupported-fifo");
    let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    let failure = storage.flush(&flush).unwrap_err();
    assert_eq!(failure.kind, ErrorKind::UnsupportedCapability);
    fs::remove_file(fifo).unwrap();
    // Removing the obstacle must not clear the retained failed-barrier state.
    assert_eq!(storage.flush(&flush).unwrap_err(), failure);
    drop(storage); // Failed runs cannot release their writer; fixture cleans up.
}

// Disruptive tests are ignored by default and serialize with one another.
// Run only these tests, with --ignored --test-threads=1, on the idle scratch
// export. The explicit opt-in and identity checks prevent targeting other NFS.
static FAULT_TEST_LOCK: Mutex<()> = Mutex::new(());
const SCRATCH_CONTAINER: &str = "umbra-nfs-nfs-1";

fn docker(args: &[&str]) -> Vec<u8> {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn check_fault_target(config: &NfsStorageConfig) {
    assert_eq!(std::env::var("UMBRA_TEST_NFS_FAULTS").as_deref(), Ok("1"));
    let scratch =
        PathBuf::from(std::env::var_os("HOME").unwrap()).join("umbra-scratch/nfs/mnt/umbra-nfs");
    assert_eq!(
        fs::canonicalize(&config.mount_root).unwrap(),
        fs::canonicalize(scratch).unwrap()
    );
    let inspect: serde_json::Value =
        serde_json::from_slice(&docker(&["inspect", SCRATCH_CONTAINER])).unwrap();
    let server = &inspect[0];
    assert_eq!(
        server["Config"]["Labels"]["com.docker.compose.project"],
        "umbra-nfs"
    );
    assert_eq!(
        server["Config"]["Labels"]["com.docker.compose.service"],
        "nfs"
    );
    assert_eq!(server["State"]["Running"], true);
    assert_eq!(server["State"]["Paused"], false);
    assert!(server["Mounts"].as_array().unwrap().iter().any(|mount| {
        mount["Name"] == "umbra-nfs_export-data" && mount["Destination"] == "/export/umbra"
    }));
}

struct RestoreServer(Option<&'static str>);
impl RestoreServer {
    fn restore(&mut self) {
        if let Some(action) = self.0 {
            docker(&[action, SCRATCH_CONTAINER]);
            self.0 = None;
        }
    }
}
impl Drop for RestoreServer {
    fn drop(&mut self) {
        if let Some(action) = self.0 {
            // Also restore on an assertion failure, without panicking in Drop.
            let result = Command::new("docker")
                .args([action, SCRATCH_CONTAINER])
                .status();
            eprintln!("scratch server recovery ({action}): {result:?}");
        }
    }
}

#[test]
#[ignore = "pauses the scratch NFS server; requires UMBRA_TEST_NFS_FAULTS=1 and an idle export"]
fn scratch_transport_outage_recovers_to_local_without_remote_claim() {
    let _serial = FAULT_TEST_LOCK.lock().unwrap();
    let (_temp, config) = fixture().expect("UMBRA_TEST_NFS_MOUNT is required");
    check_fault_target(&config);
    let mut storage = NfsStorage::connect(config).unwrap();
    let req = request();
    let binding = storage.open_run(&req).unwrap();
    let lease = storage.acquire_writer(&writer(req.run_id)).unwrap();
    let filename = physical(&binding.root).join("payload");
    let expected = payload();
    let mut original = fs::File::create(&filename).unwrap();
    original.write_all(&expected).unwrap();
    original.sync_all().unwrap();
    drop(original);
    let flush = FlushRequest {
        context: ctx(&lease),
        scope: FlushScope::EntireRun,
    };
    let mut restore = RestoreServer(Some("unpause"));
    docker(&["pause", SCRATCH_CONTAINER]);
    let (started_tx, started_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = storage.flush(&flush);
        tx.send((storage, result)).unwrap();
    });
    started_rx.recv().unwrap();
    let while_paused = rx.recv_timeout(Duration::from_secs(2));
    restore.restore(); // Resume before assertions or waiting for blocked I/O.
    assert!(
        matches!(while_paused, Err(mpsc::RecvTimeoutError::Timeout)),
        "hard-mounted barrier must not return a receipt during this outage"
    );
    let (mut storage, result) = rx
        .recv_timeout(Duration::from_secs(45))
        .expect("barrier did not finish after server resumed; completion remains unknown");
    worker.join().unwrap();
    assert_local_barrier(&result.unwrap(), &lease);
    assert_eq!(fs::read(filename).unwrap(), expected);
    storage.release_writer(&lease).unwrap();
    storage.close_run().unwrap();
    // This is transparent kernel recovery, not an exposed transport errno.
    // sync_all includes any required COMMIT; no userspace COMMIT gap exists.
}

#[test]
#[ignore = "SIGKILLs the scratch NFS server; requires UMBRA_TEST_NFS_FAULTS=1 and an idle export"]
fn scratch_server_killed_after_flush_preserves_export_bytes() {
    let _serial = FAULT_TEST_LOCK.lock().unwrap();
    let (_temp, config) = fixture().expect("UMBRA_TEST_NFS_MOUNT is required");
    check_fault_target(&config);
    let mount = config.mount_root.clone();
    let mut storage = NfsStorage::connect(config).unwrap();
    let req = request();
    let binding = storage.open_run(&req).unwrap();
    let lease = storage.acquire_writer(&writer(req.run_id)).unwrap();
    let filename = physical(&binding.root).join("payload");
    let expected = payload();
    let mut original = fs::File::create(&filename).unwrap();
    original.write_all(&expected).unwrap();
    original.sync_all().unwrap();
    drop(original);
    let receipt = storage
        .flush(&FlushRequest {
            context: ctx(&lease),
            scope: FlushScope::EntireRun,
        })
        .unwrap();
    assert_local_barrier(&receipt, &lease);
    // No read or further provider sync before SIGKILL. Dropping retains the
    // writer lock; this test never claims takeover/recovery of an abandoned run.
    drop(storage);
    let mut restore = RestoreServer(Some("start"));
    docker(&["kill", "--signal=KILL", SCRATCH_CONTAINER]);
    restore.restore();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let health = docker(&[
            "inspect",
            "--format",
            "{{.State.Health.Status}}",
            SCRATCH_CONTAINER,
        ]);
        if health == b"healthy\n" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scratch server did not become healthy after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let export_file = PathBuf::from("/export/umbra").join(filename.strip_prefix(mount).unwrap());
    // Direct server read bypasses the original NFS client's data cache. The
    // VM/volume survives: this tests a server-process crash, not power loss.
    let actual = docker(&[
        "exec",
        SCRATCH_CONTAINER,
        "cat",
        export_file.to_str().unwrap(),
    ]);
    assert_eq!(actual, expected);
    assert_eq!(fs::read(filename).unwrap(), expected);
}

#[test]
fn crud_bytes_pagination_and_durability() {
    let Some((_temp, mut config)) = fixture() else {
        return;
    };
    config.root_anchor = BytePath::new(b"shadow".to_vec()).unwrap();
    config.control_anchor = BytePath::new(b"metadata".to_vec()).unwrap();
    let mut storage = NfsStorage::connect(config).unwrap();
    let req = request();
    let binding = storage.open_run(&req).unwrap();
    assert_eq!(physical(&binding.root).file_name().unwrap(), "shadow");
    let caps = storage.capabilities();
    assert!(caps.atomic_replace);
    assert!(!caps.strict_remote_persistence && !caps.kernel_shadow && !caps.atomic_swap);
    assert_eq!(caps.fencing, Fencing::ConfirmedTermination);
    assert!(caps.max_io_bytes > 0 && caps.max_directory_entries > 0);
    let lease = storage.acquire_writer(&writer(req.run_id)).unwrap();
    assert_eq!(storage.renew_writer(&lease).unwrap(), lease);
    let p = path(b"parents/nested/raw-\xff");
    let created = storage.create(&ctx(&lease), &p, &file()).unwrap();
    assert_eq!(
        storage.create(&ctx(&lease), &p, &file()).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    assert_eq!(storage.write_at(&ctx(&lease), &p, 3, b"hello").unwrap(), 5);
    let mut bytes = [99; 12];
    assert_eq!(storage.read_at(&ctx(&lease), &p, 0, &mut bytes).unwrap(), 8);
    assert_eq!(&bytes[..8], b"\0\0\0hello");
    assert_eq!(
        storage.read_at(&ctx(&lease), &p, 99, &mut bytes).unwrap(),
        0
    );
    assert_eq!(
        storage.stat(&ctx(&lease), &p).unwrap().object_id,
        created.stat.object_id
    );
    for name in [b"parents/nested/a".as_slice(), b"parents/nested/b"] {
        storage.create(&ctx(&lease), &path(name), &file()).unwrap();
    }
    let mut cursor = None;
    let mut names = Vec::new();
    loop {
        let page = storage
            .list(&ctx(&lease), &path(b"parents/nested"), cursor.as_ref(), 1)
            .unwrap();
        assert!(page.entries.len() <= 1);
        names.extend(page.entries.into_iter().map(|e| e.name.into_bytes()));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    names.sort();
    assert_eq!(
        names,
        vec![b"a".to_vec(), b"b".to_vec(), b"raw-\xff".to_vec()]
    );
    let dest = path(b"parents/nested/a");
    storage
        .execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::Rename {
                source: p.clone(),
                destination: dest.clone(),
                mode: RenameMode::Replace,
            },
        })
        .unwrap();
    assert_eq!(
        storage.stat(&ctx(&lease), &dest).unwrap().object_id,
        created.stat.object_id
    );
    assert_eq!(
        storage.stat(&ctx(&lease), &p).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        storage
            .atomic_swap(&ctx(&lease), &dest, &path(b"parents/nested/b"))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    assert_eq!(storage.stat(&ctx(&lease), &dest).unwrap().len, 8);
    storage.unlink(&ctx(&lease), &dest).unwrap();
    assert_eq!(
        storage.stat(&ctx(&lease), &dest).unwrap_err().kind,
        ErrorKind::NotFound
    );
    let control = StoragePath::new(StorageAnchor::Control, b"state".to_vec()).unwrap();
    storage.create(&ctx(&lease), &control, &file()).unwrap();
    assert!(physical(&binding.control).join("state").exists());
    assert!(!physical(&binding.root).join("state").exists());
    let receipt = storage
        .flush(&FlushRequest {
            context: ctx(&lease),
            scope: FlushScope::EntireRun,
        })
        .unwrap();
    assert_eq!(receipt.durability, Durability::Local);
    assert_eq!(receipt.run_id, req.run_id);
    assert_eq!(receipt.writer_epoch, lease.epoch);
    assert_eq!(receipt.scope, FlushScope::EntireRun);
    let evidence = String::from_utf8(receipt.evidence).unwrap();
    assert!(evidence.contains("OS sync_all completed"));
    assert!(evidence.contains("remote stable storage unqualified"));
    assert!(evidence.contains("cannot be independently checked"));
    assert_eq!(
        storage.close_run().unwrap_err().kind,
        ErrorKind::InvalidState
    );
    storage.release_writer(&lease).unwrap();
    assert_eq!(
        storage
            .write_at(&ctx(&lease), &p, 0, b"bad")
            .unwrap_err()
            .kind,
        ErrorKind::LeaseLost
    );
    storage.close_run().unwrap();
}

#[test]
fn competing_writers_persistent_epochs_and_no_takeover() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let mut first = NfsStorage::new(config.clone());
    let mut req = request();
    first.open_run(&req).unwrap();
    let lease = first.acquire_writer(&writer(req.run_id)).unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    let mut second = NfsStorage::new(config.clone());
    second.open_run(&req).unwrap();
    assert_eq!(
        second.acquire_writer(&writer(req.run_id)).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    let mut forged = lease.clone();
    forged.renewal_token.push(0);
    assert_eq!(
        first.renew_writer(&forged).unwrap_err().kind,
        ErrorKind::LeaseLost
    );
    first.release_writer(&lease).unwrap();
    first.close_run().unwrap();
    let newer = second.acquire_writer(&writer(req.run_id)).unwrap();
    assert!(newer.epoch.0 > lease.epoch.0);
    assert_eq!(
        second
            .create(&ctx(&lease), &path(b"stale"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::LeaseLost
    );
    drop(second); // crashed/abandoned writer: lock must survive
    let mut third = NfsStorage::new(config);
    third.open_run(&req).unwrap();
    for takeover in [
        TakeoverPolicy::Refuse,
        TakeoverPolicy::FencePreviousWriter,
        TakeoverPolicy::ConfirmedTermination {
            evidence: b"unverified".to_vec(),
        },
    ] {
        let mut wr = writer(req.run_id);
        wr.takeover = takeover;
        assert!(third.acquire_writer(&wr).is_err());
    }
    third.close_run().unwrap();
}

#[test]
fn retries_conflicts_and_cursor_invalidation() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let mut s = NfsStorage::new(config);
    let req = request();
    s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    let context = ctx(&lease);
    let p = path(b"entry");
    let made = s.create(&context, &p, &file()).unwrap();
    assert_eq!(s.create(&context, &p, &file()).unwrap(), made);
    assert_eq!(
        s.create(&context, &path(b"other"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    let mut changed_key = context.clone();
    changed_key.idempotency_key.0.push('x');
    assert_eq!(
        s.create(&changed_key, &path(b"other"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    let page = s.list(&ctx(&lease), &path(b""), None, 1).unwrap();
    let remove = ctx(&lease);
    s.unlink(&remove, &p).unwrap();
    s.create(&ctx(&lease), &p, &file()).unwrap();
    s.unlink(&remove, &p).unwrap();
    assert!(s.stat(&ctx(&lease), &p).is_ok());
    assert_eq!(
        s.list(&ctx(&lease), &path(b""), page.next.as_ref(), 1)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn containment_nofollow_and_request_bounds() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"safe").unwrap();
    let mut s = NfsStorage::new(config);
    let req = request();
    let binding = s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    let root = physical(&binding.root);
    symlink(outside.path(), root.join("escape")).unwrap();
    symlink(outside.path().join("secret"), root.join("link")).unwrap();
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"link")).unwrap().kind,
        ObjectKind::LogicalSymlink
    );
    for p in [path(b"escape/secret"), path(b"link")] {
        assert_eq!(
            s.write_at(&ctx(&lease), &p, 0, b"bad").unwrap_err().kind,
            ErrorKind::InvalidPath
        );
    }
    assert!(s
        .create(&ctx(&lease), &path(b"escape/new"), &file())
        .is_err());
    assert!(!outside.path().join("new").exists());
    for bytes in [b"/abs".as_slice(), b"..", b"a/../b", b"a\0b"] {
        assert!(StoragePath::new(StorageAnchor::Root, bytes.to_vec()).is_err());
    }
    let mut wrong = ctx(&lease);
    wrong.run_id = RunId(Uuid::new_v4());
    assert_eq!(
        s.stat(&wrong, &path(b"")).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::ReadAt {
                path: path(b"link"),
                offset: i64::MAX as u64,
                len: 1
            }
        })
        .unwrap_err()
        .kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(
        s.list(&ctx(&lease), &path(b""), None, MAX_DIRECTORY_ENTRIES + 1)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    s.unlink(&ctx(&lease), &path(b"link")).unwrap();
    assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"safe");
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn provider_protocol_handshake_and_crud() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let root = BytePath::new(config.mount_root.as_os_str().as_bytes().to_vec()).unwrap();
    let mut descriptor: umbra_core::provider::ProviderDescriptor =
        serde_json::from_str(include_str!("../provider.json")).unwrap();
    descriptor.executable =
        BytePath::new(env!("CARGO_BIN_EXE_umbra-storage-nfs").as_bytes().to_vec()).unwrap();
    descriptor.options = umbra_core::provider::encode(&root).unwrap();
    let mut proxy = umbra_storage::provider::Proxy::connect(&descriptor, 10_000).unwrap();
    assert!(proxy.capabilities().atomic_replace);
    let req = request();
    let run_dir = config.mount_root.join(req.run_id.0.to_string());
    // IPC options are the exact mount root, so this isolated run sits directly
    // under it; cleanup only the random run created by this test.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(run_dir);
    proxy.open_run(&req).unwrap();
    let lease = proxy.acquire_writer(&writer(req.run_id)).unwrap();
    proxy.create(&ctx(&lease), &path(b"ipc"), &file()).unwrap();
    proxy
        .write_at(&ctx(&lease), &path(b"ipc"), 0, b"wire")
        .unwrap();
    let mut bytes = [0; 4];
    proxy
        .read_at(&ctx(&lease), &path(b"ipc"), 0, &mut bytes)
        .unwrap();
    assert_eq!(&bytes, b"wire");
    proxy.release_writer(&lease).unwrap();
    proxy.close_run().unwrap();
}

#[test]
fn absent_mount_rejected_without_creating_it() {
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("absent");
    assert!(NfsStorage::connect(NfsStorageConfig::new(&absent)).is_err());
    assert!(!absent.exists());
    assert!(NfsStorage::connect(NfsStorageConfig::new(temp.path())).is_err());
}

#[test]
fn open_run_qualifies_a_new_backend_after_mount_validation() {
    let Some((_temp, config)) = fixture() else {
        return;
    };
    let mut storage = NfsStorage::new(config);
    assert!(!storage
        .capabilities()
        .features
        .contains(capabilities::STORAGE_MOUNTED_NFSV4_V1));
    storage.open_run(&request()).unwrap();
    assert!(storage
        .capabilities()
        .features
        .contains(capabilities::STORAGE_MOUNTED_NFSV4_V1));
    storage.close_run().unwrap();
}
