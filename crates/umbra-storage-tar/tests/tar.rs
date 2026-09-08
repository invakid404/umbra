#![cfg(unix)]
mod support;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use support::*;
use umbra_core::*;
use umbra_storage::Storage;
use umbra_storage_tar::{TarStorage, TarStorageConfig};
use uuid::Uuid;

#[test]
fn open_lifecycle_and_failed_open_is_unbound() {
    let temp = tempfile::tempdir().unwrap();
    let config = TarStorageConfig::new(temp.path().join("run.tar"));
    let mut s = TarStorage::new(config.clone());
    let mut req = request();
    req.policy.require_kernel_shadow = true;
    assert_eq!(
        s.open_run(&req).unwrap_err().kind,
        ErrorKind::UnsupportedCapability
    );
    assert!(!config.archive_path.exists());
    req.policy.require_kernel_shadow = false;
    let b = s.open_run(&req).unwrap();
    assert_eq!(b.run_id, req.run_id);
    assert!(b.root.physical_path.is_none() && b.control.physical_path.is_none());
    assert_ne!(b.root.handle, b.control.handle);
    assert_eq!(s.open_run(&req).unwrap_err().kind, ErrorKind::InvalidState);
    s.close_run().unwrap();
    assert_eq!(s.open_run(&req).unwrap_err().kind, ErrorKind::AlreadyExists);
    req.intent = OpenRunIntent::OpenExisting;
    s.open_run(&req).unwrap();
    s.close_run().unwrap();
    req.immutable_base.fingerprint.push(4);
    assert_eq!(
        s.open_run(&req).unwrap_err().kind,
        ErrorKind::ProtocolMismatch
    );
    req.immutable_base.fingerprint.pop();
    s.open_run(&req).unwrap();
    s.close_run().unwrap();
}

#[test]
fn writer_exclusivity_stale_tokens_epochs_and_abandoned_lock() {
    let (temp, mut first, mut req, lease) = setup();
    assert_eq!(first.renew_writer(&lease).unwrap(), lease);
    assert_eq!(
        first.acquire_writer(&writer(req.run_id)).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    assert_eq!(first.close_run().unwrap_err().kind, ErrorKind::InvalidState);
    let mut stale = lease.clone();
    stale.renewal_token.push(0);
    assert_eq!(
        first.renew_writer(&stale).unwrap_err().kind,
        ErrorKind::LeaseLost
    );
    assert_eq!(
        first.release_writer(&stale).unwrap_err().kind,
        ErrorKind::LeaseLost
    );
    req.intent = OpenRunIntent::OpenExisting;
    let config = TarStorageConfig::new(temp.path().join("run.tar"));
    let mut second = TarStorage::new(config.clone());
    second.open_run(&req).unwrap();
    assert_eq!(
        second.acquire_writer(&writer(req.run_id)).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    first
        .create(&ctx(&lease), &path(b"staged"), &file())
        .unwrap();
    first.release_writer(&lease).unwrap();
    first.close_run().unwrap();
    let newer = second.acquire_writer(&writer(req.run_id)).unwrap();
    assert!(newer.epoch.0 > lease.epoch.0);
    assert!(second.stat(&ctx(&newer), &path(b"staged")).is_ok());
    assert_eq!(
        second
            .create(&ctx(&lease), &path(b"stale"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::LeaseLost
    );
    drop(second);
    let mut third = TarStorage::new(config);
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
fn missing_writer_lock_is_reported_as_lease_lost() {
    let (temp, mut s, _, lease) = setup();
    fs::remove_file(temp.path().join("run.tar.provider/writer.lock")).unwrap();
    assert_eq!(
        s.renew_writer(&lease).unwrap_err().kind,
        ErrorKind::LeaseLost
    );
    assert_eq!(
        s.create(&ctx(&lease), &path(b"denied"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::LeaseLost
    );
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"denied")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        s.release_writer(&lease).unwrap_err().kind,
        ErrorKind::LeaseLost
    );
}

#[test]
fn exclusive_create_and_all_mutations_require_current_authority() {
    let (_temp, mut s, _, lease) = setup();
    let p = path(b"new");
    let made = s.create(&ctx(&lease), &p, &file()).unwrap();
    assert_eq!(made.stat.kind, ObjectKind::File);
    assert_eq!(
        s.create(&ctx(&lease), &p, &file()).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    let mut reader = ctx(&lease);
    reader.writer_epoch = None;
    for operation in [
        StorageOperation::Create {
            path: path(b"missing"),
            options: file(),
        },
        StorageOperation::Unlink { path: p.clone() },
        StorageOperation::WriteAt {
            path: p.clone(),
            offset: 0,
            bytes: vec![1],
        },
        StorageOperation::Rename {
            source: p.clone(),
            destination: path(b"x"),
            mode: RenameMode::Replace,
        },
        StorageOperation::RemoveDirectory { path: path(b"dir") },
    ] {
        assert_eq!(
            s.execute(&StorageRequest {
                context: reader.clone(),
                operation
            })
            .unwrap_err()
            .kind,
            ErrorKind::LeaseLost
        );
    }
    assert_eq!(s.stat(&reader, &p).unwrap(), made.stat);
    let mut wrong = reader;
    wrong.run_id = RunId(Uuid::new_v4());
    assert_eq!(
        s.stat(&wrong, &p).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn create_requires_explicit_parents_and_valid_modes() {
    let (_temp, mut s, _, lease) = setup();
    let p = path(b"missing/nested/file");
    assert_eq!(
        s.create(&ctx(&lease), &p, &file()).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"missing")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    for kind in [
        CreateKind::File,
        CreateKind::Directory,
        CreateKind::LogicalSymlink {
            target: BytePath::new(b"target".to_vec()).unwrap(),
        },
    ] {
        assert_eq!(
            s.create(
                &ctx(&lease),
                &path(b"invalid"),
                &CreateOptions {
                    kind,
                    mode: 0o10000
                },
            )
            .unwrap_err()
            .kind,
            ErrorKind::InvalidInput
        );
    }
    let parents = |mode| StorageRequest {
        context: ctx(&lease),
        operation: StorageOperation::CreateParents {
            path: path(b"missing/nested"),
            mode,
        },
    };
    assert_eq!(
        s.execute(&parents(0o10000)).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
    assert!(s
        .list(&ctx(&lease), &path(b""), None, 10)
        .unwrap()
        .entries
        .is_empty());
    assert_eq!(
        s.execute(&parents(0o755)).unwrap(),
        StorageResponse::ParentsCreated
    );
    for dir in [b"missing".as_slice(), b"missing/nested"] {
        assert_eq!(s.stat(&ctx(&lease), &path(dir)).unwrap().mode, 0o755);
    }
    let made = s.create(&ctx(&lease), &p, &file()).unwrap();
    assert_eq!(made.stat.mode, 0o600);
    assert_eq!(
        s.create(&ctx(&lease), &p, &file()).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn retry_budget_counts_cumulative_writes_across_flush_and_reopen() {
    let (_temp, mut s, mut req, lease) = setup();
    let p = path(b"file");
    s.create(&ctx(&lease), &p, &file()).unwrap();
    let bytes = vec![255; MAX_IO_BYTES];
    for _ in 0..3 {
        assert_eq!(
            s.write_at(&ctx(&lease), &p, 0, &bytes).unwrap(),
            bytes.len()
        );
    }
    let rejected = s.write_at(&ctx(&lease), &p, 0, &bytes).unwrap_err();
    assert_eq!(rejected.kind, ErrorKind::StorageUnavailable);
    assert_eq!(s.stat(&ctx(&lease), &p).unwrap().len, MAX_IO_BYTES as u64);
    flush(&mut s, &lease);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    assert_eq!(
        s.write_at(&ctx(&lease), &p, 0, &bytes).unwrap_err().kind,
        ErrorKind::StorageUnavailable
    );
    let mut out = vec![0; MAX_IO_BYTES];
    assert_eq!(
        s.read_at(&ctx(&lease), &p, 0, &mut out).unwrap(),
        bytes.len()
    );
    assert_eq!(out, bytes);
    assert_eq!(s.write_at(&ctx(&lease), &p, 0, b"small").unwrap(), 5);
    s.unlink(&ctx(&lease), &p).unwrap();
    s.create(&ctx(&lease), &p, &file()).unwrap();
    assert_eq!(
        s.write_at(&ctx(&lease), &p, 0, &bytes).unwrap_err().kind,
        ErrorKind::StorageUnavailable
    );
    flush(&mut s, &lease);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn bounded_positional_io_sparse_holes_overwrites_and_eof() {
    let (_temp, mut s, _, lease) = setup();
    let p = path(b"nested/raw-\xff");
    s.create(&ctx(&lease), &path(b"nested"), &directory())
        .unwrap();
    s.create(&ctx(&lease), &p, &file()).unwrap();
    assert_eq!(s.write_at(&ctx(&lease), &p, 3, b"hello").unwrap(), 5);
    assert_eq!(s.write_at(&ctx(&lease), &p, 4, b"AB").unwrap(), 2);
    let mut out = [99; 12];
    assert_eq!(s.read_at(&ctx(&lease), &p, 0, &mut out).unwrap(), 8);
    assert_eq!(&out[..8], b"\0\0\0hABlo");
    assert_eq!(&out[8..], &[99; 4]);
    assert_eq!(s.read_at(&ctx(&lease), &p, 100, &mut out).unwrap(), 0);
    assert_eq!(s.write_at(&ctx(&lease), &p, 100, b"").unwrap(), 0);
    assert_eq!(s.stat(&ctx(&lease), &p).unwrap().len, 8);
    for offset in [u64::MAX, i64::MAX as u64] {
        assert_eq!(
            s.write_at(&ctx(&lease), &p, offset, &[1]).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            s.execute(&StorageRequest {
                context: ctx(&lease),
                operation: StorageOperation::ReadAt {
                    path: p.clone(),
                    offset,
                    len: 1
                }
            })
            .unwrap_err()
            .kind,
            ErrorKind::InvalidInput
        );
    }
    let large = vec![1; MAX_IO_BYTES + 1];
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::WriteAt {
                path: p.clone(),
                offset: 0,
                bytes: large
            }
        })
        .unwrap_err()
        .kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::ReadAt {
                path: p.clone(),
                offset: 0,
                len: MAX_IO_BYTES as u32 + 1
            }
        })
        .unwrap_err()
        .kind,
        ErrorKind::InvalidInput
    );
    let max = vec![7; MAX_IO_BYTES];
    assert_eq!(s.write_at(&ctx(&lease), &p, 0, &max).unwrap(), max.len());
    let mut out = vec![0; MAX_IO_BYTES];
    assert_eq!(s.read_at(&ctx(&lease), &p, 0, &mut out).unwrap(), max.len());
    assert_eq!(out, max);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn list_pages_are_scoped_bounded_and_invalidated() {
    let (_temp, mut s, mut req, lease) = setup();
    s.create(&ctx(&lease), &path(b"dir"), &directory()).unwrap();
    for p in [b"dir/a".as_slice(), b"dir/b", b"dir/raw-\xff"] {
        s.create(&ctx(&lease), &path(p), &file()).unwrap();
    }
    let mut cursor = None;
    let mut names = Vec::new();
    loop {
        let page = s
            .list(&ctx(&lease), &path(b"dir"), cursor.as_ref(), 1)
            .unwrap();
        assert!(page.entries.len() <= 1);
        names.extend(page.entries.into_iter().map(|e| e.name.into_bytes()));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        names,
        vec![b"a".to_vec(), b"b".to_vec(), b"raw-\xff".to_vec()]
    );
    for limit in [0, MAX_DIRECTORY_ENTRIES + 1] {
        assert_eq!(
            s.list(&ctx(&lease), &path(b"dir"), None, limit)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
    }
    let cursor = s
        .list(&ctx(&lease), &path(b"dir"), None, 1)
        .unwrap()
        .next
        .unwrap();
    assert_eq!(
        s.list(&ctx(&lease), &path(b""), Some(&cursor), 1)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
    s.create(&ctx(&lease), &path(b"unrelated"), &file())
        .unwrap();
    assert_eq!(
        s.list(&ctx(&lease), &path(b"dir"), Some(&cursor), 1)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
    let cursor = s
        .list(&ctx(&lease), &path(b"dir"), None, 1)
        .unwrap()
        .next
        .unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    s.open_run(&req).unwrap();
    assert_eq!(
        s.list(&ctx(&lease), &path(b"dir"), Some(&cursor), 1)
            .unwrap_err()
            .kind,
        ErrorKind::StaleHandle
    );
    s.close_run().unwrap();
}

#[test]
fn rename_noreplace_replacement_subtrees_and_cross_anchor() {
    let (_temp, mut s, _, lease) = setup();
    let a = s.create(&ctx(&lease), &path(b"a"), &file()).unwrap();
    let b = s.create(&ctx(&lease), &path(b"b"), &file()).unwrap();
    let rename = |source, destination, mode| StorageRequest {
        context: ctx(&lease),
        operation: StorageOperation::Rename {
            source,
            destination,
            mode,
        },
    };
    assert_eq!(
        s.execute(&rename(path(b"a"), path(b"b"), RenameMode::NoReplace))
            .unwrap_err()
            .kind,
        ErrorKind::AlreadyExists
    );
    assert_eq!(s.stat(&ctx(&lease), &path(b"b")).unwrap(), b.stat);
    let control = StoragePath::new(StorageAnchor::Control, b"b").unwrap();
    assert_eq!(
        s.execute(&rename(path(b"a"), control, RenameMode::Replace))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(s.stat(&ctx(&lease), &path(b"a")).unwrap(), a.stat);
    assert!(matches!(
        s.execute(&rename(path(b"a"), path(b"c"), RenameMode::NoReplace))
            .unwrap(),
        StorageResponse::Renamed(_)
    ));
    s.execute(&rename(path(b"c"), path(b"b"), RenameMode::Replace))
        .unwrap();
    assert_eq!(s.stat(&ctx(&lease), &path(b"b")).unwrap(), a.stat);
    s.execute(&StorageRequest {
        context: ctx(&lease),
        operation: StorageOperation::CreateParents {
            path: path(b"dir/sub"),
            mode: 0o755,
        },
    })
    .unwrap();
    s.create(&ctx(&lease), &path(b"dir/sub/file"), &file())
        .unwrap();
    assert_eq!(
        s.execute(&rename(
            path(b"dir"),
            path(b"dir/sub/new"),
            RenameMode::Replace
        ))
        .unwrap_err()
        .kind,
        ErrorKind::InvalidPath
    );
    s.execute(&rename(path(b"dir"), path(b"moved"), RenameMode::NoReplace))
        .unwrap();
    assert!(s.stat(&ctx(&lease), &path(b"moved/sub/file")).is_ok());
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn unlink_remove_directory_and_hard_links_are_honest() {
    let (_temp, mut s, _, lease) = setup();
    s.create(&ctx(&lease), &path(b"dir"), &directory()).unwrap();
    s.create(&ctx(&lease), &path(b"dir/file"), &file()).unwrap();
    let remove = |p| StorageRequest {
        context: ctx(&lease),
        operation: StorageOperation::RemoveDirectory { path: path(p) },
    };
    assert_eq!(
        s.execute(&remove(b"dir")).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    assert_eq!(
        s.unlink(&ctx(&lease), &path(b"dir")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    assert!(!s.capabilities().hard_links);
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::Link {
                source: path(b"dir/file"),
                destination: path(b"hard")
            }
        })
        .unwrap_err()
        .kind,
        ErrorKind::UnsupportedCapability
    );
    s.unlink(&ctx(&lease), &path(b"dir/file")).unwrap();
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"dir/file")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        s.execute(&remove(b"dir")).unwrap(),
        StorageResponse::DirectoryRemoved
    );
    assert_eq!(
        s.execute(&remove(b"")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn logical_symlinks_stat_and_unlink_never_follow_targets() {
    let (temp, mut s, _, lease) = setup();
    let outside = temp.path().join("secret");
    fs::write(&outside, b"safe").unwrap();
    let target = BytePath::new(outside.as_os_str().as_bytes().to_vec()).unwrap();
    let options = CreateOptions {
        kind: CreateKind::LogicalSymlink {
            target: target.clone(),
        },
        mode: 0o777,
    };
    let made = s.create(&ctx(&lease), &path(b"link"), &options).unwrap();
    assert_eq!(s.stat(&ctx(&lease), &path(b"link")).unwrap(), made.stat);
    assert_eq!(made.stat.kind, ObjectKind::LogicalSymlink);
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::ReadLink {
                path: path(b"link")
            }
        })
        .unwrap(),
        StorageResponse::ReadLink(target)
    );
    for p in [path(b"link"), path(b"link/child")] {
        assert_eq!(
            s.write_at(&ctx(&lease), &p, 0, b"bad").unwrap_err().kind,
            ErrorKind::InvalidPath
        );
        assert_eq!(
            s.read_at(&ctx(&lease), &p, 0, &mut [0; 4])
                .unwrap_err()
                .kind,
            ErrorKind::InvalidPath
        );
    }
    assert_eq!(
        s.create(&ctx(&lease), &path(b"link/new"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    for p in [b"../escape".as_slice(), b"a/../b", b"/abs", b"a\0b"] {
        assert!(StoragePath::new(StorageAnchor::Root, p).is_err());
    }
    flush(&mut s, &lease);
    s.unlink(&ctx(&lease), &path(b"link")).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"safe");
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

#[test]
fn atomic_swap_negotiation_and_cross_anchor_preflight() {
    let (_temp, mut s, _, lease) = setup();
    let p = path(b"a");
    let made = s.create(&ctx(&lease), &p, &file()).unwrap();
    assert!(!s.capabilities().atomic_swap);
    assert_eq!(
        s.atomic_swap(&ctx(&lease), &p, &path(b"b"))
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    let control = StoragePath::new(StorageAnchor::Control, b"b").unwrap();
    assert_eq!(
        s.atomic_swap(&ctx(&lease), &p, &control).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::AtomicSwap {
                left: p.clone(),
                right: control
            }
        })
        .unwrap_err()
        .kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(s.stat(&ctx(&lease), &p).unwrap(), made.stat);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
}

fn tar_bytes(path: &std::path::Path, suffix: &[u8]) -> Option<Vec<u8>> {
    let mut a = tar::Archive::new(fs::File::open(path).unwrap());
    for e in a.entries().unwrap() {
        let mut e = e.unwrap();
        if e.path_bytes().ends_with(suffix) {
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes).unwrap();
            return Some(bytes);
        }
    }
    None
}
#[test]
fn flush_all_scopes_publish_local_archive_and_stable_identity() {
    let (temp, mut s, mut req, lease) = setup();
    let archive = temp.path().join("run.tar");
    let p = path(b"payload");
    let made = s.create(&ctx(&lease), &p, &file()).unwrap();
    s.write_at(&ctx(&lease), &p, 0, b"persisted").unwrap();
    assert!(tar_bytes(&archive, b"/root/payload").is_none());
    for scope in [
        FlushScope::Data {
            objects: vec![made.stat.object_id],
        },
        FlushScope::DataAndMetadata {
            objects: vec![made.stat.object_id],
        },
        FlushScope::EntireRun,
    ] {
        let receipt = s
            .flush(&FlushRequest {
                context: ctx(&lease),
                scope: scope.clone(),
            })
            .unwrap();
        assert_eq!(receipt.scope, scope);
        assert_eq!(receipt.durability, Durability::Local);
        assert_eq!(receipt.durability, s.capabilities().durability);
        assert!(receipt
            .evidence
            .windows(archive.as_os_str().as_bytes().len())
            .any(|w| w == archive.as_os_str().as_bytes()));
        assert!(String::from_utf8_lossy(&receipt.evidence).contains("parent directory fsync"));
        assert_eq!(tar_bytes(&archive, b"/root/payload").unwrap(), b"persisted");
    }
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    // A copied, flushed archive is independently readable without its staging sidecar.
    let copy = temp.path().join("copy.tar");
    fs::copy(&archive, &copy).unwrap();
    let mut reopened = TarStorage::new(TarStorageConfig::new(copy));
    req.intent = OpenRunIntent::OpenExisting;
    reopened.open_run(&req).unwrap();
    assert_eq!(
        reopened.stat(&ctx(&lease), &p).unwrap().object_id,
        made.stat.object_id
    );
    let mut bytes = [0; 9];
    reopened.read_at(&ctx(&lease), &p, 0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"persisted");
    reopened.close_run().unwrap();
}

#[test]
fn retries_survive_reopen_conflicts_and_name_reuse() {
    let (_temp, mut s, mut req, lease) = setup();
    let create = ctx(&lease);
    let p = path(b"entry");
    let made = s.create(&create, &p, &file()).unwrap();
    assert_eq!(s.create(&create, &p, &file()).unwrap(), made);
    let mut key = create.clone();
    key.idempotency_key.0.push('x');
    assert_eq!(
        s.create(&key, &p, &file()).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(
        s.create(&create, &path(b"conflict"), &file())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    let mut op = create.clone();
    op.operation_id = OperationId(Uuid::new_v4());
    assert_eq!(
        s.create(&op, &p, &file()).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
    let remove = ctx(&lease);
    s.unlink(&remove, &p).unwrap();
    let replacement = s.create(&ctx(&lease), &p, &file()).unwrap();
    s.unlink(&remove, &p).unwrap();
    assert_eq!(s.stat(&ctx(&lease), &p).unwrap(), replacement.stat);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    s.open_run(&req).unwrap();
    let newer = s.acquire_writer(&writer(req.run_id)).unwrap();
    let mut retry = remove;
    retry.writer_epoch = Some(newer.epoch);
    s.unlink(&retry, &p).unwrap();
    assert_eq!(s.stat(&ctx(&newer), &p).unwrap(), replacement.stat);
    s.release_writer(&newer).unwrap();
    s.close_run().unwrap();
}

#[test]
fn provider_protocol_handshake_options_and_crud() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("ipc.tar");
    let options = umbra_core::provider::encode(
        &BytePath::new(archive.as_os_str().as_bytes().to_vec()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        TarStorageConfig::from_options(&options).unwrap(),
        TarStorageConfig::new(&archive)
    );
    let mut descriptor: umbra_core::provider::ProviderDescriptor =
        serde_json::from_str(include_str!("../provider.json")).unwrap();
    descriptor.executable =
        BytePath::new(env!("CARGO_BIN_EXE_umbra-storage-tar").as_bytes().to_vec()).unwrap();
    descriptor.options = options;
    let mut proxy = umbra_storage::provider::Proxy::connect(&descriptor, 10_000).unwrap();
    assert_eq!(proxy.capabilities().durability, Durability::Local);
    let req = request();
    proxy.open_run(&req).unwrap();
    let lease = proxy.acquire_writer(&writer(req.run_id)).unwrap();
    proxy.create(&ctx(&lease), &path(b"ipc"), &file()).unwrap();
    proxy
        .write_at(&ctx(&lease), &path(b"ipc"), 0, b"wire")
        .unwrap();
    let mut out = [0; 4];
    proxy
        .read_at(&ctx(&lease), &path(b"ipc"), 0, &mut out)
        .unwrap();
    assert_eq!(&out, b"wire");
    flush(&mut proxy, &lease);
    proxy.release_writer(&lease).unwrap();
    proxy.close_run().unwrap();
    assert_eq!(tar_bytes(&archive, b"/root/ipc").unwrap(), b"wire");
}

#[test]
fn corrupt_archive_and_physical_symlink_are_rejected_without_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.tar");
    fs::write(&archive, b"not tar").unwrap();
    let mut s = TarStorage::new(TarStorageConfig::new(&archive));
    let mut req = request();
    req.intent = OpenRunIntent::OpenExisting;
    assert_eq!(
        s.open_run(&req).unwrap_err().kind,
        ErrorKind::CorruptJournal
    );
    assert!(!temp.path().join("bad.tar.provider").exists());
    assert_eq!(s.close_run().unwrap_err().kind, ErrorKind::InvalidState);
    let link = temp.path().join("link.tar");
    std::os::unix::fs::symlink(&archive, &link).unwrap();
    let mut s = TarStorage::new(TarStorageConfig::new(link));
    assert_eq!(s.open_run(&req).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(fs::read(archive).unwrap(), b"not tar");
}

#[test]
fn custom_layout_control_anchor_and_read_only_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = TarStorageConfig::new(temp.path().join("custom.tar"));
    config.run_parent = path(b"runs/nested");
    config.root_anchor = BytePath::new(b"shadow".to_vec()).unwrap();
    config.control_anchor = BytePath::new(b"metadata".to_vec()).unwrap();
    let mut s = TarStorage::new(config.clone());
    let mut req = request();
    s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    let p = StoragePath::new(StorageAnchor::Control, b"state").unwrap();
    s.create(&ctx(&lease), &p, &file()).unwrap();
    s.write_at(&ctx(&lease), &p, 0, b"control").unwrap();
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"state")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        s.execute(&StorageRequest {
            context: ctx(&lease),
            operation: StorageOperation::CreateParents {
                path: path(b"parents/nested"),
                mode: 0o750
            }
        })
        .unwrap(),
        StorageResponse::ParentsCreated
    );
    assert_eq!(
        s.stat(&ctx(&lease), &path(b"parents/nested")).unwrap().kind,
        ObjectKind::Directory
    );
    let receipt = flush(&mut s, &lease);
    assert_eq!(receipt.durability, Durability::Local);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    assert_eq!(
        tar_bytes(&config.archive_path, b"/metadata/state").unwrap(),
        b"control"
    );
    let mut a = tar::Archive::new(fs::File::open(&config.archive_path).unwrap());
    let expected = format!("runs/nested/{}/metadata/state", req.run_id.0).into_bytes();
    assert!(a
        .entries()
        .unwrap()
        .any(|e| e.unwrap().path_bytes().as_ref() == expected));
    req.intent = OpenRunIntent::OpenExisting;
    req.policy.read_only = true;
    s.open_run(&req).unwrap();
    assert_eq!(
        s.acquire_writer(&writer(req.run_id)).unwrap_err().kind,
        ErrorKind::Denied
    );
    assert_eq!(s.stat(&ctx(&lease), &p).unwrap().len, 7);
    s.close_run().unwrap();
}

#[test]
fn release_and_close_preserve_staging_without_flushing_archive() {
    let (temp, mut s, mut req, lease) = setup();
    let archive = temp.path().join("run.tar");
    s.create(&ctx(&lease), &path(b"pending"), &file()).unwrap();
    s.write_at(&ctx(&lease), &path(b"pending"), 0, b"staged")
        .unwrap();
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    assert!(tar_bytes(&archive, b"/root/pending").is_none());
    req.intent = OpenRunIntent::OpenExisting;
    s.open_run(&req).unwrap();
    let newer = s.acquire_writer(&writer(req.run_id)).unwrap();
    let mut bytes = [0; 6];
    s.read_at(&ctx(&newer), &path(b"pending"), 0, &mut bytes)
        .unwrap();
    assert_eq!(&bytes, b"staged");
    flush(&mut s, &newer);
    s.release_writer(&newer).unwrap();
    s.close_run().unwrap();
    assert_eq!(tar_bytes(&archive, b"/root/pending").unwrap(), b"staged");
}

#[test]
fn malformed_tar_entries_and_truncation_cannot_escape_or_bind() {
    use std::io::Write;
    let (temp, mut s, mut req, lease) = setup();
    flush(&mut s, &lease);
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    let original = fs::read(temp.path().join("run.tar")).unwrap();
    for (i, name) in [b"../outside".as_slice(), b"/absolute", b"unexpected"]
        .into_iter()
        .enumerate()
    {
        let p = temp.path().join(format!("malicious-{i}.tar"));
        let mut output = fs::File::create(&p).unwrap();
        output
            .write_all(&original[..original.len() - 1024])
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o600);
        h.as_mut_bytes()[..name.len()].copy_from_slice(name);
        h.set_cksum();
        let mut builder = tar::Builder::new(output);
        builder.append(&h, b"evil".as_slice()).unwrap();
        builder.finish().unwrap();
        drop(builder);
        let mut bad = TarStorage::new(TarStorageConfig::new(&p));
        assert_eq!(
            bad.open_run(&req).unwrap_err().kind,
            ErrorKind::CorruptJournal
        );
        assert_eq!(bad.close_run().unwrap_err().kind, ErrorKind::InvalidState);
        assert!(!temp
            .path()
            .join(format!("malicious-{i}.tar.provider"))
            .exists());
    }
    let p = temp.path().join("truncated.tar");
    fs::write(&p, &original[..original.len() - 512]).unwrap();
    let mut bad = TarStorage::new(TarStorageConfig::new(p));
    assert_eq!(
        bad.open_run(&req).unwrap_err().kind,
        ErrorKind::CorruptJournal
    );
    assert!(!temp.path().join("outside").exists());
}

#[test]
fn competing_threads_only_one_writer_and_readers_keep_snapshot() {
    let (temp, mut s, mut req, lease) = setup();
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    req.intent = OpenRunIntent::OpenExisting;
    let config = TarStorageConfig::new(temp.path().join("run.tar"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let config = config.clone();
        let req = req.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut s = TarStorage::new(config);
            s.open_run(&req).unwrap();
            barrier.wait();
            let lease = s.acquire_writer(&writer(req.run_id));
            barrier.wait();
            if let Ok(lease) = &lease {
                s.release_writer(lease).unwrap();
            }
            s.close_run().unwrap();
            lease
        }));
    }
    barrier.wait();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.iter().find_map(|r| r.as_ref().err()).unwrap().kind,
        ErrorKind::AlreadyExists
    );
    let mut reader = TarStorage::new(config);
    reader.open_run(&req).unwrap();
    s.open_run(&req).unwrap();
    let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
    s.create(&ctx(&lease), &path(b"later"), &file()).unwrap();
    assert_eq!(
        reader.stat(&ctx(&lease), &path(b"later")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    s.release_writer(&lease).unwrap();
    s.close_run().unwrap();
    reader.close_run().unwrap();
    reader.open_run(&req).unwrap();
    assert!(reader.stat(&ctx(&lease), &path(b"later")).is_ok());
    reader.close_run().unwrap();
}
