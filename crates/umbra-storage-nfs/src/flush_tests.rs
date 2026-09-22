use super::*;
use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};

fn fixture() -> (tempfile::TempDir, File, File) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("run/sub")).unwrap();
    fs::write(temp.path().join("run/sub/file"), b"bytes").unwrap();
    let parent = File::open(temp.path()).unwrap();
    let dir = File::open(temp.path().join("run")).unwrap();
    (temp, dir, parent)
}

#[test]
fn barrier_covers_files_then_directories_and_parent_without_following_symlinks() {
    let (temp, dir, parent) = fixture();
    fs::write(temp.path().join("outside"), b"untouched").unwrap();
    symlink(temp.path().join("outside"), temp.path().join("run/link")).unwrap();
    let mut synchronized = Vec::new();
    flush_with(
        &dir,
        &parent,
        &FlushHealth::default(),
        &mut || Ok(()),
        &mut |fd| {
            synchronized.push(fd.metadata().unwrap().ino());
            Ok(())
        },
    )
    .unwrap();
    let expected: Vec<_> = ["run/sub/file", "run/sub", "run", ""]
        .iter()
        .map(|p| fs::metadata(temp.path().join(p)).unwrap().ino())
        .collect();
    assert_eq!(synchronized, expected);
    assert_eq!(fs::read(temp.path().join("outside")).unwrap(), b"untouched");
}

#[test]
fn exposed_transport_and_writeback_errors_prevent_later_success() {
    let (_temp, dir, parent) = fixture();
    for (errno, kind) in [
        (libc::ETIMEDOUT, ErrorKind::StorageUnavailable),
        (libc::ECONNRESET, ErrorKind::StorageUnavailable),
        (libc::EHOSTUNREACH, ErrorKind::StorageUnavailable),
        (libc::ESTALE, ErrorKind::StaleHandle),
        (libc::EIO, ErrorKind::Io),
        (libc::ENOSPC, ErrorKind::Io),
        (libc::EDQUOT, ErrorKind::Io),
    ] {
        let health = FlushHealth::default();
        let first = flush_with(&dir, &parent, &health, &mut || Ok(()), &mut |_| {
            Err(io("sync_all", std::io::Error::from_raw_os_error(errno)))
        })
        .unwrap_err();
        assert_eq!(first.kind, kind);
        assert_eq!(first.errno, Some(Errno(errno)));
        assert!(first.context.contains("persistence outcome unknown"));
        let repeated = flush_with(
            &dir,
            &parent,
            &health,
            &mut || panic!("failed barrier must not restart"),
            &mut |_| panic!("a subsequent successful sync cannot erase failure"),
        );
        assert_eq!(repeated.unwrap_err(), first);
    }
}

#[test]
fn authority_loss_after_file_sync_stops_before_parent_and_remains_sticky() {
    let (_temp, dir, parent) = fixture();
    let count = Cell::new(0);
    let health = FlushHealth::default();
    let result = flush_with(
        &dir,
        &parent,
        &health,
        &mut || {
            if count.get() > 0 {
                Err(error(ErrorKind::LeaseLost, "writer", "lease expired"))
            } else {
                Ok(())
            }
        },
        &mut |_| {
            count.set(count.get() + 1);
            Ok(())
        },
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::LeaseLost);
    assert_eq!(count.get(), 1);
    assert_eq!(health.check().unwrap_err().kind, ErrorKind::LeaseLost);
}

#[test]
fn namespace_change_during_flush_fails_the_whole_scope() {
    let (temp, dir, parent) = fixture();
    let mut changed = false;
    let result = flush_with(
        &dir,
        &parent,
        &FlushHealth::default(),
        &mut || Ok(()),
        &mut |_| {
            if !changed {
                fs::write(temp.path().join("run/sub/new"), b"racing writer").unwrap();
                changed = true;
            }
            Ok(())
        },
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::InvalidState);
}

#[test]
fn concurrent_external_write_during_barrier_is_rejected_and_retained() {
    // Exercise the real mounted filesystem when selected, with deterministic
    // scheduling at the private sync seam instead of a timing-dependent race.
    let temp = match std::env::var_os("UMBRA_TEST_NFS_MOUNT") {
        Some(mount) => tempfile::tempdir_in(mount).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    fs::create_dir(temp.path().join("run")).unwrap();
    let filename = temp.path().join("run/payload");
    let mut original = File::create(&filename).unwrap();
    original.write_all(b"before").unwrap();
    original.sync_all().unwrap();
    let dir = File::open(temp.path().join("run")).unwrap();
    let parent = File::open(temp.path()).unwrap();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        start_rx.recv().unwrap();
        original.write_all(b"concurrent generation").unwrap();
        original.sync_all().unwrap();
        done_tx.send(()).unwrap();
    });
    let health = FlushHealth::default();
    let mut changed = false;
    let result = flush_with(&dir, &parent, &health, &mut || Ok(()), &mut |fd| {
        sync(fd)?;
        if fd.metadata().unwrap().is_file() && !changed {
            // The writer changes this file after sync, before the barrier's
            // final stamp/entry check. No sleep or scheduler luck is needed.
            start_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            changed = true;
        }
        Ok(())
    });
    drop(start_tx); // Unblock the worker even if the barrier failed earlier.
    writer.join().unwrap();
    assert!(changed);
    let failure = result.unwrap_err();
    assert_eq!(failure.kind, ErrorKind::InvalidState);
    assert!(failure.context.contains("changed during barrier"));
    assert!(failure.context.contains("persistence outcome unknown"));
    // Quiescence restored after the race cannot erase the failed barrier.
    assert_eq!(
        flush(&dir, &parent, &health, &mut || Ok(())).unwrap_err(),
        failure
    );
}

#[test]
fn replaced_directory_identity_and_depth_overflow_are_rejected() {
    let (temp, dir, parent) = fixture();
    fs::rename(temp.path().join("run"), temp.path().join("old")).unwrap();
    fs::create_dir(temp.path().join("run")).unwrap();
    assert_eq!(
        verify_entry(&parent, b"run", &dir).unwrap_err().kind,
        ErrorKind::StaleHandle
    );
    let result = flush_tree(
        &dir,
        dir.metadata().unwrap().dev(),
        MAX_FLUSH_DEPTH,
        &mut || Ok(()),
        &mut |_| panic!("must not partially sync an excluded scope"),
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::UnsupportedCapability);
}

#[test]
fn failure_in_parent_sync_is_not_a_successful_run_barrier() {
    let (_temp, dir, parent) = fixture();
    let parent_ino = parent.metadata().unwrap().ino();
    let health = FlushHealth::default();
    let result = flush_with(&dir, &parent, &health, &mut || Ok(()), &mut |fd| {
        if fd.metadata().unwrap().ino() == parent_ino {
            Err(io("sync_all", std::io::Error::from_raw_os_error(libc::EIO)))
        } else {
            Ok(())
        }
    });
    assert_eq!(result.unwrap_err().kind, ErrorKind::Io);
    assert!(health.check().is_err());
}

#[test]
fn mutation_sync_failure_is_sticky_but_input_rejection_is_not() {
    let health = FlushHealth::default();
    let rejected: Result<()> = Err(error(ErrorKind::InvalidPath, "path", "symlink rejected"));
    assert!(health.observe(rejected).is_err());
    health.check().unwrap();
    let failed: Result<()> = Err(io(
        "sync_all",
        std::io::Error::from_raw_os_error(libc::EACCES),
    ));
    let first = health.observe(failed).unwrap_err();
    assert_eq!(first.kind, ErrorKind::Denied);
    assert_eq!(health.check().unwrap_err(), first);
}

#[test]
fn os_sync_barrier_completes_on_a_local_fixture() {
    let (_temp, dir, parent) = fixture();
    let health = FlushHealth::default();
    flush(&dir, &parent, &health, &mut || Ok(())).unwrap();
    health.check().unwrap();
}

/// The syscall boundary `StorageOperation::SetMetadata` is built on, exercised
/// the way the rest of this module exercises `native`: against a real directory
/// FD in a tempdir, with no NFS mount involved. The kernel's NFS client is what
/// turns these into SETATTR on the wire; this crate never encodes one.
///
/// Self-chown rather than cross-uid, because CI has no second identity to give
/// an object to. That is the shape `STORAGE_OWNERSHIP_FIDELITY_V1` actually
/// claims -- "what the kernel permits is applied and a refusal is reported" --
/// and the refusal half arrives through the errno mapper already tested above.
#[test]
fn chown_honours_the_unchanged_sentinel_and_chmod_sets_the_mode() {
    let (temp, dir, _parent) = fixture();
    let before = native_stat(&dir, b"sub/file");

    // `None` in both positions is `(uid_t)-1` twice: POSIX's "change nothing",
    // which is what `MetadataUpdate`'s absent fields have to mean.
    chown(&dir, b"sub/file", None, None).unwrap();
    let after = native_stat(&dir, b"sub/file");
    assert_eq!((after.uid, after.gid), (before.uid, before.gid));

    // Naming the identity it already has is the chown an unprivileged process is
    // always permitted, and it is exactly the carry the overlay emits when the
    // base object is one umbra already owns.
    chown(&dir, b"sub/file", Some(before.uid), Some(before.gid)).unwrap();
    assert_eq!(native_stat(&dir, b"sub/file").uid, before.uid);

    chmod(&dir, b"sub/file", 0o640).unwrap();
    assert_eq!(native_stat(&dir, b"sub/file").mode, 0o640);
    assert_eq!(
        fs::metadata(temp.path().join("run/sub/file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );

    // An absent name is `NotFound`, through the same mapper every other arm uses.
    assert_eq!(
        chown(&dir, b"sub/absent", Some(before.uid), None)
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        chmod(&dir, b"sub/absent", 0o600).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

fn native_stat(dir: &File, name: &[u8]) -> BlobStat {
    stat(dir, name).unwrap()
}

/// The #101 hazard, pinned as a positive test so the reason the probe moved off
/// `.provider` can never be lost: a SETATTR on the live `.provider` mid-flush bumps
/// its ctime, the barrier catches that as `.provider` changing, and because
/// `FlushHealth` is sticky the whole provider is poisoned -- not just one flush.
/// This is exactly what the old `ownership_supported(&self.private)` probe did on a
/// late finish, and exactly what `ownership_probe`'s isolated target now avoids.
///
/// Deterministic via the `flush_with` synchronize seam: the SETATTR fires when the
/// barrier synchronizes `.provider` itself, between its before-stamp and its final
/// stamp check, so the "directory changed during barrier" catch is not timing
/// dependent.
#[test]
fn a_setattr_on_provider_mid_flush_poisons_health() {
    let (temp, dir, parent) = fixture();
    fs::create_dir(temp.path().join("run/.provider")).unwrap();
    let provider = File::open(temp.path().join("run/.provider")).unwrap();
    let provider_ino = provider.metadata().unwrap().ino();
    let health = FlushHealth::default();
    let mut fired = false;
    let result = flush_with(&dir, &parent, &health, &mut || Ok(()), &mut |fd| {
        sync(fd)?;
        // When the barrier syncs `.provider`, re-apply its own ownership -- the
        // pre-#101 probe, verbatim. A no-op in effect, but a real SETATTR bumping
        // ctime, which the immediately following stamp check inside `flush_tree`
        // rejects.
        if !fired && fd.metadata().unwrap().ino() == provider_ino {
            fired = true;
            assert!(
                ownership_supported(&provider),
                "a local filesystem honours the SETATTR the old probe issued"
            );
        }
        Ok(())
    });
    assert!(fired, "the barrier reached `.provider`'s sync");
    let failure = result.unwrap_err();
    assert_eq!(failure.kind, ErrorKind::InvalidState);
    assert!(failure.context.contains("changed during barrier"));
    // Sticky: a later clean flush still surfaces the original poisoning.
    assert_eq!(health.check().unwrap_err(), failure);
    assert_eq!(
        flush(&dir, &parent, &health, &mut || Ok(())).unwrap_err(),
        failure
    );
}

/// The complement, and the #101 fix: the *isolated* probe running mid-flush leaves
/// the barrier clean and health untouched. The real `ownership_probe` routine --
/// mkdir `.umbra-probes/<uuid>` under the run parent, SETATTR, rmdir -- runs at the
/// synchronize seam exactly as `open_run`'s bounded closure runs it, yet every one
/// of its operations lands under the run parent's `.umbra-probes`, never in the run
/// directory the barrier is stamping. So the flush succeeds where the `.provider`
/// probe above poisoned it.
#[test]
fn an_isolated_probe_mid_flush_leaves_the_barrier_and_health_clean() {
    let (_temp, dir, parent) = fixture();
    let health = FlushHealth::default();
    let mut probed = false;
    let result = flush_with(&dir, &parent, &health, &mut || Ok(()), &mut |fd| {
        if !probed {
            probed = true;
            assert!(
                ownership_probe(&parent, &dir),
                "a local filesystem honours the isolated probe's SETATTR"
            );
        }
        sync(fd)
    });
    assert!(probed, "the probe ran during the barrier");
    result.expect("an isolated probe cannot disturb the run's barrier");
    health.check().expect("and cannot poison health");
}

/// The isolated probe leaves no residue: after a successful probe the reserved
/// container persists but holds no `<uuid>` directory, and the target it created
/// and removed was a sibling of the run directory on the same device -- never
/// inside the run directory the barrier walks.
#[test]
fn the_isolated_probe_leaves_no_residue_outside_the_run_dir() {
    let (temp, dir, parent) = fixture();
    assert!(
        ownership_probe(&parent, &dir),
        "a local filesystem honours the isolated probe"
    );

    let probes = temp.path().join(".umbra-probes");
    assert!(probes.is_dir(), "the reserved container persists");
    assert_eq!(
        fs::read_dir(&probes).unwrap().count(),
        0,
        "the probe removed its own uuid directory"
    );
    // A sibling of the run directory under the run parent, never inside the run.
    assert!(!temp.path().join("run/.umbra-probes").exists());
    // Same export as the run directory: the dev guard requires it.
    let container = open(&parent, PROBES_DIR, libc::O_RDONLY | libc::O_DIRECTORY, 0).unwrap();
    assert_eq!(
        container.metadata().unwrap().dev(),
        dir.metadata().unwrap().dev(),
        "the probe container shares the run directory's device"
    );
}

/// The probe's cleanup guard directly: `remove_now` removes the `<uuid>` dir and
/// disarms the `Drop` backstop, and a guard whose `remove_now` never ran removes
/// the dir on drop. Mirrors the local backend's `probe_dir_*` test.
#[test]
fn probe_dir_cleanup_removes_the_uuid_dir_and_disarms_drop() {
    let (temp, _dir, parent) = fixture();
    mkdir(&parent, PROBES_DIR, 0o700).unwrap();
    let container = open(&parent, PROBES_DIR, libc::O_RDONLY | libc::O_DIRECTORY, 0).unwrap();
    let name = b".umbra-probe-test".to_vec();
    let uuid_path = temp.path().join(".umbra-probes/.umbra-probe-test");

    mkdir(&container, &name, 0o700).unwrap();
    let guard = ProbeDir::new(&container, name.clone());
    guard.remove_now().unwrap();
    assert!(
        !uuid_path.exists(),
        "explicit cleanup removed the probe dir"
    );
    assert!(
        !guard.armed.get(),
        "a successful removal disarms the Drop backstop"
    );

    // A guard that never had `remove_now` called removes on drop.
    mkdir(&container, &name, 0o700).unwrap();
    {
        let _backstop = ProbeDir::new(&container, name.clone());
    }
    assert!(
        !uuid_path.exists(),
        "the Drop backstop removed the probe dir"
    );
}

/// The dev guard is against the RUN DIRECTORY's device (audit §A.2, matching
/// local's a4), not merely the run parent's: if `<run_parent>/<run_id>` were a
/// mount point on another filesystem, a guard against the parent's device would
/// pass while the probe measured the wrong export and wrongly advertised
/// `STORAGE_OWNERSHIP_FIDELITY_V1`. A real cross-device layout needs a mount, so
/// inject through the `ownership_probe_with` seam a run-dir device the probe target
/// (which lives under the run parent) cannot match, and assert the probe fails
/// closed. `u64::MAX` is guaranteed absent -- device numbers are per-filesystem.
#[test]
fn a_run_dir_device_the_target_cannot_match_fails_the_probe_closed() {
    let (_temp, dir, parent) = fixture();
    // Baseline: with the real run-dir device the probe qualifies on a local fs.
    assert!(
        ownership_probe(&parent, &dir),
        "the probe qualifies when the target shares the run dir's device"
    );
    // Inject a mismatched run-dir device: the probe target can never sit on it, so
    // the dev guard must answer "not qualified".
    assert!(
        !ownership_probe_with(&parent, u64::MAX),
        "a probe target not on the run dir's device must not qualify"
    );
}

/// The probe `open_run` gates `STORAGE_OWNERSHIP_FIDELITY_V1` on.
///
/// It must answer `true` on a store that honours ownership updates -- a tempdir
/// on a local filesystem does -- and it must be a *no-op*, because it runs on
/// every `open_run` against a directory the run needs afterwards.
#[test]
fn the_ownership_probe_answers_yes_on_a_store_that_chowns_and_changes_nothing() {
    let (temp, dir, _parent) = fixture();
    let sub = File::open(temp.path().join("run/sub")).unwrap();
    let before = stat(&dir, b"sub").unwrap();

    assert!(
        ownership_supported(&sub),
        "a local filesystem honours an ownership update, so the probe qualifies it"
    );

    let after = stat(&dir, b"sub").unwrap();
    assert_eq!(
        (after.uid, after.gid, after.mode),
        (before.uid, before.gid, before.mode),
        "the probe sets the ownership the object already had, so nothing moves"
    );
    // And the directory is still usable for everything open_run does next.
    assert_eq!(stat(&sub, b"file").unwrap().len, 5);
}
