//! Reproducible enforced fixture matrix. Build workspace binaries and the C
//! fixture first. Required qualification also needs an existing NFSv4 mount.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{json, Value};
use std::{
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};
use umbra_core::{
    BytePath, JournalAccess, JournalControlBinding, JournalFormatPolicy, JournalLifecycle,
    JournalOpenRequest, JournalPayload, PhysicalPath, RunId, Sequence,
};
use umbra_journal::Journal;
use umbra_journal_file::FileJournal;

fn input(name: &str) -> Option<std::ffi::OsString> {
    let value = std::env::var_os(name).filter(|v| !v.is_empty());
    assert!(
        value.is_some() || std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none(),
        "required integration needs {name}"
    );
    if value.is_none() {
        eprintln!("SKIP: set {name}");
    }
    value
}

fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

struct RunDirectory(std::path::PathBuf);
impl Drop for RunDirectory {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("fixture cleanup {}: {e}", self.0.display());
            }
        }
    }
}

// Arm cleanup without parsing before the status assertion. On a failed run,
// Drop uses any prepared ID emitted before the error; a preparation failure
// without an ID still reports its original stderr and touches no shared run.
struct RunOutputCleanup<'a> {
    store: &'a Path,
    stderr: &'a str,
}
impl Drop for RunOutputCleanup<'_> {
    fn drop(&mut self) {
        if let Some(id) = prepared_id(self.stderr).and_then(|id| uuid::Uuid::parse_str(id).ok()) {
            drop(RunDirectory(self.store.join(id.to_string())));
        }
    }
}

fn prepared_id(stderr: &str) -> Option<&str> {
    stderr.lines().find_map(|line| {
        line.strip_prefix("umbra: run ")
            .and_then(|line| line.strip_suffix(" prepared"))
    })
}

#[test]
fn failed_status_keeps_diagnostics_and_cleans_only_the_reported_run() {
    let store = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4();
    let run = store.path().join(id.to_string());
    std::fs::create_dir(&run).unwrap();
    for stderr in [
        "mount qualification failed".to_owned(),
        format!("umbra: run {id} prepared\nchild failed"),
    ] {
        let panic = std::panic::catch_unwind(|| {
            let _cleanup = RunOutputCleanup {
                store: store.path(),
                stderr: &stderr,
            };
            panic!("{stderr}");
        })
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<String>().unwrap(), &stderr);
        assert_eq!(run.exists(), !stderr.contains(" prepared"));
    }
    assert!(store.path().exists());
}

fn matrix(nfs: bool) {
    let Some(fixture) = input("UMBRA_TEST_FIXTURE_PATH") else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let scratch = scratch.path().canonicalize().unwrap();
    let store = if nfs {
        let Some(root) = input("UMBRA_TEST_NFS_ROOT") else {
            return;
        };
        std::path::PathBuf::from(root).canonicalize().unwrap()
    } else {
        scratch.join("store")
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = Path::new(env!("CARGO_BIN_EXE_umbra"));
    let bin_dir = binary.parent().unwrap();
    let descriptor = |package: &str, capabilities: &[&str], options: Vec<u8>| {
        let mut value: Value = serde_json::from_slice(
            &std::fs::read(repo.join("crates").join(package).join("provider.json")).unwrap(),
        )
        .unwrap();
        let executable = bin_dir.join(package);
        assert!(
            executable.is_file(),
            "build workspace binaries first: {}",
            executable.display()
        );
        value["executable"] = json!(bytes(&executable));
        value["capabilities"] = json!(capabilities);
        value["options"] = json!(options);
        value
    };
    // NFS fixture cases measure 5–8 seconds after the OS sync barrier work.
    // Give their provider requests headroom while retaining the local budget.
    let timeout_ms = if nfs { 25_000 } else { 5_000 };
    let registry = json!({"timeout_ms": timeout_ms, "providers": {
        "platform": descriptor("umbra-platform-macos", &["sandboxed-stopped-launch-v1", "experimental-syscall-rewrite-v1"], vec![]),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor(if nfs {"umbra-storage-nfs"} else {"umbra-storage-local"},
            &[if nfs {"mounted-nfsv4-v1"} else {"local-development-v1"}, "experimental-open-rewrite-v1"],
            serde_json::to_vec(bytes(&store)).unwrap())
    }});
    let registry_path = scratch.join("registry.json");
    std::fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
    let cases: &[(&str, &[u8])] = &[
        ("open-libc", b"libc\n"),
        ("open-svc", b"libc\n"),
        ("fork-write", b"fork\n"),
        ("posix-spawn-write", b"libc\n"),
        ("exec-write", b"libc\n"),
        ("grandchild-write", b"grandchild\n"),
        ("dup-inherit-write", b"dup\n"),
    ];
    for (case, expected) in cases {
        let workspace = scratch.join(case);
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
        let destination = workspace.join("output");
        let mut command = Command::new(binary);
        command
            .args(["run", "--registry"])
            .arg(&registry_path)
            .arg("--workspace")
            .arg(&workspace)
            .arg("--experimental");
        if !nfs {
            command.arg("--local-dev");
        }
        let started = Instant::now();
        let output = command
            .arg("--")
            .arg(&fixture)
            .arg(case)
            .arg(&destination)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let elapsed = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _output_cleanup = RunOutputCleanup {
            store: &store,
            stderr: &stderr,
        };
        assert!(
            output.status.success(),
            "{case} after {:.3}s: {stderr}",
            elapsed.as_secs_f64()
        );
        let id = prepared_id(&stderr).expect("prepared run ID");
        let id = uuid::Uuid::parse_str(id).unwrap();
        let run_dir = store.join(id.to_string());
        let _cleanup = RunDirectory(run_dir.clone());
        assert!(!destination.exists(), "host destination created for {case}");
        let shadow = run_dir
            .join("root")
            .join(destination.strip_prefix("/").unwrap());
        assert_eq!(std::fs::read(shadow).unwrap(), *expected, "{case}");
        fn no_lease(path: &Path) {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                assert_ne!(entry.file_name(), "writer.lock", "writer lease retained");
                if entry.file_type().unwrap().is_dir() {
                    no_lease(&entry.path());
                }
            }
        }
        no_lease(&run_dir);
        assert!(
            journal_records_completion(&run_dir, RunId(id), case),
            "{case}: no completion record"
        );
        eprintln!(
            "PASS {} {case}: {} exact bytes, host absent, lease released, RunCompleted, elapsed={:.3}s",
            if nfs { "nfs" } else { "local" },
            expected.len(),
            elapsed.as_secs_f64()
        );
        std::fs::remove_dir_all(run_dir).unwrap();
    }
}

#[test]
fn local_fixture_matrix() {
    matrix(false);
}
#[test]
fn nfs_fixture_matrix() {
    // Opt-out for CI environments where a real NFSv4 mount is unavailable
    // (e.g. macOS Sequoia/Tahoe self-hosted runners under launchd: TCC's
    // SystemPolicyNetworkVolumes gate blocks openat on the mount point and
    // installing a bypass PPPC profile requires user-approved MDM). Local
    // dev runs skip this env var so the test still exercises the live
    // mount. A userspace NFSv4 client that removes the local-mount
    // requirement is planned to replace this workaround.
    if std::env::var_os("UMBRA_TEST_SKIP_NFS_MATRIX")
        .filter(|v| !v.is_empty())
        .is_some()
    {
        eprintln!(
            "SKIP nfs_fixture_matrix: UMBRA_TEST_SKIP_NFS_MATRIX set (real NFSv4 mount unavailable in this environment)"
        );
        return;
    }
    matrix(true);
}

#[test]
fn run_directory_is_removed_when_a_case_panics() {
    let root = tempfile::tempdir().unwrap();
    let run = root.path().join("run");
    std::fs::create_dir(&run).unwrap();
    let result = std::panic::catch_unwind(|| {
        let _cleanup = RunDirectory(run.clone());
        panic!("fixture failed");
    });
    assert!(result.is_err());
    assert!(!run.exists());
}

// ---------------------------------------------------------------------------
// Standard utilities, and the exit surfaces a crashing tracee produces.
//
// Two things above this line changed: the imports gained the journal contracts
// this file now decodes with, and `matrix` (`:92-212`) calls
// `journal_records_completion` at `:201` instead of scanning its log for the bytes
// `RunCompleted`, so both completion checks decode through one implementation.
// `matrix`'s provider wiring, registry, case list and every other assertion are
// untouched, and that is why the registry builder below is a second one rather
// than an extraction from it: leaving that setup alone keeps the enforced matrix
// connecting providers exactly as CI has been running it.
// ---------------------------------------------------------------------------

/// Registry naming the real built provider executables, with the storage root
/// carried as the storage descriptor's opaque options.
///
/// `timeout_ms` is the authoritative per-request IPC deadline, so each caller
/// states the headroom its slowest provider request needs.
fn provider_registry(
    scratch: &Path,
    store: &Path,
    nfs: bool,
    timeout_ms: u64,
) -> std::path::PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_umbra")).parent().unwrap();
    let descriptor = |package: &str, capabilities: &[&str], options: Vec<u8>| {
        let mut value: Value = serde_json::from_slice(
            &std::fs::read(repo.join("crates").join(package).join("provider.json")).unwrap(),
        )
        .unwrap();
        let executable = bin_dir.join(package);
        assert!(
            executable.is_file(),
            "build workspace binaries first: {}",
            executable.display()
        );
        value["executable"] = json!(bytes(&executable));
        value["capabilities"] = json!(capabilities);
        value["options"] = json!(options);
        value
    };
    let registry = json!({"timeout_ms": timeout_ms, "providers": {
        "platform": descriptor("umbra-platform-macos", &["sandboxed-stopped-launch-v1", "experimental-syscall-rewrite-v1"], vec![]),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor(if nfs {"umbra-storage-nfs"} else {"umbra-storage-local"},
            &[if nfs {"mounted-nfsv4-v1"} else {"local-development-v1"}, "experimental-open-rewrite-v1"],
            serde_json::to_vec(bytes(store)).unwrap())
    }});
    let path = scratch.join("registry.json");
    std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
    path
}

/// Refuse any `writer.lock` anywhere under a finished run.
fn assert_lease_released(run_dir: &Path, case: &str) {
    for entry in std::fs::read_dir(run_dir).unwrap() {
        let entry = entry.unwrap();
        assert_ne!(
            entry.file_name(),
            "writer.lock",
            "writer lease retained for {case}"
        );
        if entry.file_type().unwrap().is_dir() {
            assert_lease_released(&entry.path(), case);
        }
    }
}

/// Whether a run's journal records the run as completed.
///
/// A substring search over the log decoded nothing, so it accepted a superset
/// name like a hypothetical `RunCompletedWithRecovery` and could not tell a
/// corrupt log from a log without the record. This opens the real `FileJournal`
/// read-only and replays it, so the format header, frame length, checksum and
/// sequence ordering are checked by the backend before any payload is looked at
/// (`umbra-journal/src/lib.rs:86-92` is the contract), and only a decoded
/// `JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })` answers yes.
///
/// Every failure is a panic, never `false`: a missing journal, a framing or
/// checksum error, or a record that will not decode. A log this cannot read is
/// not a log without a completion record, which matters most for the negative
/// assertions in the crash cases — an unreadable journal must not satisfy them.
///
/// `run_id` is required by `JournalControlBinding` and is *not* verified against
/// the log here: a `JournalRecord` carries no run id, and the backend cross-checks
/// identity only for a writer authority (`umbra-journal-file/src/lib.rs:142`) and
/// for checkpoints (`:409`), neither of which a read-only open supplies. What ties
/// this log to this run is the path it was found at.
fn journal_records_completion(run_dir: &Path, run_id: RunId, case: &str) -> bool {
    // The backend appends `journal` to the control directory it is handed
    // (`umbra-journal-file/src/lib.rs:176`), so this is the run's `control`, not
    // the `control/journal` the log itself sits in.
    let control = run_dir.join("control");
    let directory = PhysicalPath(BytePath::new(control.as_os_str().as_bytes().to_vec()).unwrap());
    let mut journal = FileJournal::new();
    journal
        .open(&JournalOpenRequest {
            control: JournalControlBinding { run_id, directory },
            access: JournalAccess::ReadOnly,
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        })
        .unwrap_or_else(|e| panic!("{case}: opening {} read-only: {e}", control.display()));
    // Sequence(0) is the replay floor the rest of the workspace uses; real records
    // start at 1, so this streams the whole log.
    let records = journal
        .replay(Sequence(0))
        .unwrap_or_else(|e| panic!("{case}: replaying {}: {e}", control.display()));
    let completed = records
        .map(|record| record.unwrap_or_else(|e| panic!("{case}: undecodable journal record: {e}")))
        .any(|record| {
            matches!(
                record.payload,
                JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
            )
        });
    completed
}

/// Refuse any change to the seeded workspace.
fn assert_workspace_pristine(workspace: &Path, case: &str) {
    let mut names = std::fs::read_dir(workspace)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, [std::ffi::OsString::from("seed.txt")], "{case}");
    assert_eq!(
        std::fs::read(workspace.join("seed.txt")).unwrap(),
        b"seed\n",
        "{case}"
    );
}

/// What a standard utility must leave behind after one `umbra run`.
///
/// A run is a single launch, so the utilities cannot be chained into one run:
/// each invocation is its own run against its own freshly seeded workspace.
enum Expect {
    /// The path operand is captured in the run's shadow with exactly these
    /// bytes, and is never created on the host.
    Captured(&'static [u8]),
    /// The path operand is captured in the run's shadow **as a directory**, and
    /// is never created on the host.
    ///
    /// A separate variant rather than `Captured(b"")` because a directory has no
    /// bytes to read: `std::fs::read` on one fails, so the empty-file case and
    /// the directory case cannot share an assertion without one of them being
    /// checked for the wrong thing. The distinction is also the point of the
    /// case that uses it -- `mkdir` must leave a directory, and an empty *file*
    /// at that path would be a wrong answer this has to catch.
    CapturedDirectory,
    /// The utility reads an operand that exists, exits zero, and leaves the host
    /// workspace byte-identical. Exiting zero says the operand was found, not
    /// that the overlay is what found it: this operand exists on the host too,
    /// so the case does not distinguish a rewritten open from an unrewritten
    /// one. Which resolver answered is pinned by `Expect::Captured`.
    ///
    /// What the utility *printed* is deliberately not asserted. A tracee
    /// inherits fds 0-2 from the platform provider process, whose stdout the
    /// provider transport sets to `/dev/null`
    /// (`umbra-core/src/provider/transport.rs:322`), so a filter's output is not
    /// observable from here today, and pinning it empty would pin that.
    ///
    /// On its own this cannot tell "the utility ran" from "nothing ran": a
    /// no-op also exits zero and touches nothing. Its discriminating partner is
    /// the [`Expect::Diagnoses`] case for the same utility, which no no-op can
    /// satisfy. Neither leg is meaningful without the other.
    ReadOnly,
    /// The operand does not exist, so the utility fails and names it on stderr.
    ///
    /// That line is the observable this matrix needs: a tracee's stderr *does*
    /// reach the caller even though its stdout does not, so the message proves
    /// which program ran, that it received this exact operand, and that the
    /// operand was absent when it was resolved. Substituting a program that
    /// does nothing prints nothing and fails the case. umbra still reports its
    /// own exit 1.
    ///
    /// Which resolver produced that answer is pinned by `Expect::Captured`, not
    /// by this case: the operand is absent on the host too, so an unrewritten
    /// open composes the same diagnostic.
    Diagnoses,
    /// The utility's path syscall is outside the tracer's intercepted set, so
    /// the operand reaches the kernel unrewritten and the unconditional sandbox
    /// refuses it. The child fails and the operation happens *nowhere*: not on
    /// the host, not in the shadow. Enforcement, not rewriting, contains it.
    ///
    /// `umbra_platform_macos::abi::TRACED_STUBS` is the list of libc stubs that
    /// get a breakpoint, and `abi::path_operands` the syscall numbers whose
    /// operands are rewritten. Bare `mkdir(2)` is now in both
    /// ([#114](https://github.com/invakid404/umbra/issues/114)); it used to be
    /// named here as the example of a call in neither.
    ///
    /// **Those are item names, not line ranges, and the change is deliberate.**
    /// This comment carried `native.rs:464-495` / `abi.rs:95-115`, sixty-odd PRs
    /// out of date. Correcting them to the ranges `prep_workspace` verified
    /// against master produced citations that were stale again *within the same
    /// change*, because adding the stub rows moved both blocks. A line range is
    /// the wrong artifact for a cross-file citation: it is invalidated by edits
    /// that have nothing to do with what it points at, and nothing in the build
    /// checks it. A named item moves with its definition and `cargo doc`
    /// resolves it.
    ///
    /// What still holds is the variant, not that example. The two lists are
    /// finite and deliberately narrow -- only what a shipped utility was
    /// measured to need is in them -- so the bare non-`at` forms nothing has
    /// needed yet are outside both: `unlink`(10), `link`(9), `chmod`(15),
    /// `chown`(16), `rename`(128), `rmdir`(137), `utimes`(138),
    /// `stat`(188)/`stat64`(338), `lstat`(190), `access`(33), `truncate`(200)
    /// and `getattrlist`(220).
    ///
    /// So the variant is kept, and kept **exercised** rather than merely
    /// described: `/bin/rm` reaches the kernel with a bare `unlink`(10), so it
    /// takes the place `mkdir` vacated. A variant whose last case had been
    /// routed would be prose claiming a path nothing walks, which is the same
    /// failure as the stale citations above one layer up.
    ///
    /// `rm`'s footprint is **not** "`unlink` and nothing else" -- an earlier
    /// version of this comment claimed that, and it was wrong. Re-measured
    /// under lldb, post-`main`: two `ioctl`s from `isatty`, six locale opens
    /// each followed by an `fstat`, then `lstat`(190), `access`(33) and
    /// `unlink`(10). The first two of those are *also* bare non-`at` forms from
    /// the inventory above, and they are also refused -- `lstat` and `access`
    /// on an absent-from-both-tables number reach the kernel unrewritten and
    /// read the host, which for this case is harmless because the operand is
    /// on the host anyway. What makes the case discriminating is the `unlink`:
    /// it is the only *mutation*, and the sandbox is what stops it.
    ///
    /// `rm` also does not prompt, and not for the reason first given: BSD `rm`
    /// proceeds because stdin is not a tty (`Stdio::null()`), not because the
    /// seed is mode 0644.
    RefusedByEnforcement,
    /// The utility's syscall **is** routed, and umbra refuses it to the tracee
    /// because this run's storage cannot serve it. The run survives.
    ///
    /// Distinct from [`Expect::RefusedByEnforcement`] in the thing that matters:
    /// there, the operand escapes umbra entirely and Seatbelt contains it; here
    /// umbra decodes the call, resolves it, and answers an errno. The two are
    /// told apart by the message the utility prints -- `Operation not
    /// supported` against `Operation not permitted` -- which is asserted,
    /// because otherwise a regression that turned one into the other would go
    /// unnoticed.
    ///
    /// **This is the case that makes the timestamp asymmetry executable rather
    /// than merely documented.** `touch <existing>` reaches the kernel as
    /// `setattrlistat`(524), which umbra routes on every registry -- but only
    /// `umbra-storage-nfs-userspace` can apply a timestamp. Both matrices in
    /// this file are rewrite-backed, so both answer `ENOTSUP`. The important
    /// half is what does *not* happen: no journal record, no copy-up, no
    /// poisoned session. Reaching the backend's refusal from inside `prepare`
    /// would flush an intent for a change that never happened and stop the run,
    /// which is strictly worse than the pre-routing behaviour, and this case is
    /// what would catch that regression.
    RefusedByBackend,
}

/// `mkdir` + `touch` + `cat` + `rm` + `ls` under `umbra run`, on one backend.
///
/// These are absolute paths: `PATH` is never searched -- `run.validate` refuses
/// a command that is not an absolute executable path.
///
/// **What this matrix cannot prove, stated here because it was once claimed to.**
/// Both backends it runs against are *rewrite-backed*, so a routed `open` is
/// rewritten to a real host path and the descriptor the tracee gets back is a
/// **kernel** descriptor, below the fence. `fstat` on it is therefore resumed
/// untouched and umbra's `fstat` routing is inert here: the `touch <absent>`
/// case exits 0 with or without it. The audit specified that rc-0 as "the
/// `fstat` proof"; it is not one on this registry, and the premise behind it
/// ("`touch` exits 1 without `fstat` routed") holds only where the descriptor
/// is virtual -- the `nfs-userspace` registry.
///
/// So the `fstat` proof lives in
/// `umbra-storage-nfs-userspace/tests/userspace_run.rs` and needs a live NFSv4
/// fixture. What *this* matrix proves about the slice is the `mkdir` route (the
/// shadow directory is attributable to it and nothing else) and the timestamp
/// refusal (`Expect::RefusedByBackend`), both of which are registry-independent.
fn utilities(nfs: bool) {
    // The C fixture is not launched here. Its path is the signal the suite
    // already uses for "this host has workspace binaries built and debugger
    // permission granted" (this file's own `input()` helper, and the fixture
    // build in `ci.yml`'s `native-qualification` job), which is
    // exactly what a real traced launch needs, so these cases inherit it rather
    // than inventing a second provisioning switch.
    if input("UMBRA_TEST_FIXTURE_PATH").is_none() {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let scratch = scratch.path().canonicalize().unwrap();
    let store = if nfs {
        let Some(root) = input("UMBRA_TEST_NFS_ROOT") else {
            return;
        };
        std::path::PathBuf::from(root).canonicalize().unwrap()
    } else {
        scratch.join("store")
    };
    let binary = Path::new(env!("CARGO_BIN_EXE_umbra"));
    let registry_path = provider_registry(&scratch, &store, nfs, if nfs { 25_000 } else { 5_000 });
    // Program, path operand relative to the workspace (empty naming the
    // workspace itself), and what the run must leave behind. Each read utility
    // appears twice, once on an operand that is present and once on one that is
    // absent: the success leg pins that the read succeeded, the failure leg pins
    // that the program ran at all. Neither pins which resolver answered; the
    // touch case does that.
    let cases: &[(&str, &str, Expect)] = &[
        ("/usr/bin/touch", "touched", Expect::Captured(b"")),
        // `touch` on an operand that already exists, which is the other half of
        // the utility and takes a different syscall: `setattrlistat`(524)
        // rather than `open`+`fstat`. Refused here because this matrix is
        // rewrite-backed; served on `nfs-userspace`, where
        // `standard_utilities_run_over_the_userspace_client` asserts the times
        // actually move.
        ("/usr/bin/touch", "seed.txt", Expect::RefusedByBackend),
        // Was `RefusedByEnforcement` until bare `mkdir`(136) joined the stub
        // list and the operand table (#114). One row, which is the whole
        // observable difference that fix makes on a rewrite-backed registry:
        // the directory is now captured in the shadow instead of happening
        // nowhere, and the host is untouched either way.
        ("/bin/mkdir", "made", Expect::CapturedDirectory),
        // Bare `unlink`(10), in neither list. It replaces `mkdir` as this
        // matrix's `RefusedByEnforcement` case: the operand exists on the host,
        // the sandbox refuses the removal, and the file is still there
        // afterwards.
        ("/bin/rm", "seed.txt", Expect::RefusedByEnforcement),
        ("/bin/cat", "seed.txt", Expect::ReadOnly),
        ("/bin/cat", "absent.txt", Expect::Diagnoses),
        ("/bin/ls", "", Expect::ReadOnly),
        ("/bin/ls", "absent", Expect::Diagnoses),
    ];
    for (index, (program, relative, expect)) in cases.iter().enumerate() {
        let case = if relative.is_empty() {
            format!("{program} <workspace>")
        } else {
            format!("{program} {relative}")
        };
        let workspace = scratch.join(format!("utility-{index}"));
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("seed.txt"), b"seed\n").unwrap();
        let operand = if relative.is_empty() {
            workspace.clone()
        } else {
            workspace.join(relative)
        };
        let mut command = Command::new(binary);
        command
            .args(["run", "--registry"])
            .arg(&registry_path)
            .arg("--workspace")
            .arg(&workspace)
            .arg("--experimental");
        if !nfs {
            command.arg("--local-dev");
        }
        let started = Instant::now();
        let output = command
            .arg("--")
            .arg(program)
            .arg(&operand)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let elapsed = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _output_cleanup = RunOutputCleanup {
            store: &store,
            stderr: &stderr,
        };
        assert!(
            !stderr.contains("panicked"),
            "{case} panicked after {:.3}s: {stderr}",
            elapsed.as_secs_f64()
        );
        match expect {
            Expect::Captured(_) | Expect::CapturedDirectory | Expect::ReadOnly => assert!(
                output.status.success(),
                "{case} after {:.3}s: {stderr}",
                elapsed.as_secs_f64()
            ),
            Expect::RefusedByEnforcement | Expect::RefusedByBackend | Expect::Diagnoses => {
                // The child's failure surfaces as umbra's own structured
                // failure rather than as a silent success. These cases cannot
                // separate the taxonomy from passthrough — both utilities exit
                // 1 themselves — so the D4 proof is
                // `a_nonzero_child_does_not_lend_umbra_its_exit_code`, where
                // the child's 41 is visibly not umbra's exit code.
                assert_eq!(output.status.code(), Some(1), "{case}: {stderr}");
                assert!(stderr.contains("ProcessFailed"), "{case}: {stderr}");
                assert!(stderr.contains("run.child"), "{case}: {stderr}");
            }
        }
        let id = uuid::Uuid::parse_str(prepared_id(&stderr).expect("prepared run ID")).unwrap();
        let run_dir = store.join(id.to_string());
        let _cleanup = RunDirectory(run_dir.clone());
        let shadow = run_dir
            .join("root")
            .join(operand.strip_prefix("/").unwrap());
        match expect {
            Expect::Captured(expected) => {
                assert!(!operand.exists(), "host operand created for {case}");
                assert_eq!(std::fs::read(&shadow).unwrap(), *expected, "{case}");
            }
            Expect::CapturedDirectory => {
                assert!(!operand.exists(), "host operand created for {case}");
                let metadata = std::fs::metadata(&shadow)
                    .unwrap_or_else(|e| panic!("{case}: {}: {e}", shadow.display()));
                assert!(
                    metadata.is_dir(),
                    "{case}: the shadow holds {:?}, not a directory",
                    metadata.file_type()
                );
            }
            Expect::RefusedByEnforcement => {
                // "The operation happened nowhere", asserted in both places it
                // could have happened. The workspace check is the stronger half
                // and subsumes the operand-was-not-created form this used to
                // carry: it pins every name in the workspace *and* the seed's
                // bytes, so it catches a removal as well as a creation.
                assert!(!shadow.exists(), "{case} captured a refused operation");
                assert_workspace_pristine(&workspace, &case);
                // Seatbelt's refusal, told apart from umbra's own by the
                // message. A regression that routed `unlink` would change this
                // line, and a regression that stopped routing `setattrlistat`
                // would change the other one.
                assert!(
                    stderr.contains("Operation not permitted"),
                    "{case}: expected the sandbox's refusal on the tracee's stderr: {stderr}"
                );
            }
            Expect::RefusedByBackend => {
                assert!(!shadow.exists(), "{case} materialised a refused operation");
                assert_workspace_pristine(&workspace, &case);
                assert!(
                    stderr.contains("Operation not supported"),
                    "{case}: expected umbra's own ENOTSUP on the tracee's stderr: {stderr}"
                );
            }
            Expect::ReadOnly => assert_workspace_pristine(&workspace, &case),
            Expect::Diagnoses => {
                // The utility names itself, its operand and the verdict. Only
                // the requested program can produce this line.
                let reported = format!(
                    "{}: {}: No such file or directory",
                    Path::new(program).file_name().unwrap().to_string_lossy(),
                    operand.display()
                );
                assert!(
                    stderr.contains(&reported),
                    "{case}: expected {reported:?} on the tracee's stderr: {stderr}"
                );
                assert!(!operand.exists(), "host operand created for {case}");
                assert!(!shadow.exists(), "{case} created the absent operand");
                assert_workspace_pristine(&workspace, &case);
            }
        }
        assert_lease_released(&run_dir, &case);
        assert!(
            journal_records_completion(&run_dir, RunId(id), &case),
            "{case}: no completion record"
        );
        eprintln!(
            "PASS {} {case}: host untouched, lease released, RunCompleted, elapsed={:.3}s",
            if nfs { "nfs" } else { "local" },
            elapsed.as_secs_f64()
        );
        std::fs::remove_dir_all(run_dir).unwrap();
    }
}

#[test]
fn local_utility_matrix() {
    utilities(false);
}

#[test]
fn nfs_utility_matrix() {
    // Same opt-out as `nfs_fixture_matrix`: a real NFSv4 mount is unavailable in
    // CI (`ci.yml:248-255`), and these cases need the same live mount.
    if std::env::var_os("UMBRA_TEST_SKIP_NFS_MATRIX")
        .filter(|v| !v.is_empty())
        .is_some()
    {
        eprintln!(
            "SKIP nfs_utility_matrix: UMBRA_TEST_SKIP_NFS_MATRIX set (real NFSv4 mount unavailable in this environment)"
        );
        return;
    }
    utilities(true);
}

// Raise a signal on this process. Declared rather than adding a `libc`
// dev-dependency for one call in one test helper.
extern "C" {
    fn raise(signal: i32) -> i32;
}

/// The child half of the crash cases: this very test binary, re-executed as the
/// tracee and selected by name with `--exact`.
///
/// Re-exec keeps the crash fixtures inside `crates/umbra-cli`: no new build
/// input, no compiler invocation in a test, and no CI wiring.
///
/// Two variables gate the body, and `crash_case` passes both on the tracee's own
/// command line: `UMBRA_TEST_CRASH_TRACEE` marks the process as the tracee, and
/// `UMBRA_TEST_CRASH` names the outcome. `umbra run` builds the child's
/// environment explicitly and inherits nothing, so neither reaches any other
/// process: an ordinary `cargo test` run treats this as a passing no-op, and so
/// does every other run under `umbra run` in this file. Reaching the body from
/// outside the harness takes setting both by hand.
///
/// The marker is why the outcome variable alone is not the gate. Exporting
/// `UMBRA_TEST_CRASH` while debugging these cases is a plausible thing to do,
/// and on its own it would have exited or signalled the whole test binary rather
/// than one test.
#[test]
fn crash_child() {
    if std::env::var_os("UMBRA_TEST_CRASH_TRACEE").is_none() {
        return;
    }
    let Ok(outcome) = std::env::var("UMBRA_TEST_CRASH") else {
        return;
    };
    match outcome.split_once(':') {
        Some(("signal", number)) => {
            let signal = number.parse().expect("signal number");
            // SAFETY: an FFI call to `raise(3)` with a signal number this test
            // chose. It reads and writes nothing this code owns.
            unsafe { raise(signal) };
            // Reaching this line means the signal was neither fatal nor
            // intercepted, and untraced that is exactly what signal 11 does: a
            // Rust binary starts with a non-default SIGSEGV handler installed on
            // an alternate signal stack, and on a fault it does not recognise
            // that handler restores the default disposition and returns -- while
            // `raise` re-executes nothing, so the process runs on. Measured on
            // this host: a Rust binary's first `raise(11)` returns and its second
            // is fatal, whereas the same call from C, where SIGSEGV is still
            // SIG_DFL, is fatal at once. Signal 9 needs none of this; it cannot
            // be caught.
            //
            // So the SIGSEGV case depends on the platform backend refusing the
            // stop before delivery, which is what the `Crash::Signal` arm below
            // asserts. A backend that forwarded the signal instead would arrive
            // here, and this panic names that rather than leaving a bare exit 101
            // for someone to explain.
            unreachable!("raise({signal}) returned and the process survived");
        }
        Some(("exit", code)) => std::process::exit(code.parse().expect("exit code")),
        other => panic!("UMBRA_TEST_CRASH names no outcome: {other:?}"),
    }
}

/// How the re-executed child ends, and what `umbra run` must then report.
enum Crash {
    /// `std::process::exit(code)` with a code that is neither 0, 1 nor 2, so a
    /// propagated child status would be visible as such.
    Exit(i32),
    /// `raise(signal)` for a signal whose default disposition is fatal.
    Signal(i32),
}

/// Launch the re-executed child and assert the surface `umbra run` presents.
fn crash_case(crash: Crash) {
    if input("UMBRA_TEST_FIXTURE_PATH").is_none() {
        return;
    }
    let (variable, case) = match crash {
        Crash::Exit(code) => (format!("exit:{code}"), format!("exit {code}")),
        Crash::Signal(signal) => (format!("signal:{signal}"), format!("signal {signal}")),
    };
    let scratch = tempfile::tempdir().unwrap();
    let scratch = scratch.path().canonicalize().unwrap();
    let store = scratch.join("store");
    // This binary is rebuilt whenever a test changes, so its signed twin is
    // cold on every build. Give the signing request the same headroom the NFS
    // arm gives its provider calls rather than the 5s local budget.
    let registry_path = provider_registry(&scratch, &store, false, 25_000);
    let workspace = scratch.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let child = std::env::current_exe().unwrap();
    let output = Command::new(Path::new(env!("CARGO_BIN_EXE_umbra")))
        .args(["run", "--registry"])
        .arg(&registry_path)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--experimental", "--local-dev", "--env"])
        .arg(format!("UMBRA_TEST_CRASH={variable}"))
        .args(["--env", "UMBRA_TEST_CRASH_TRACEE=1"])
        .arg("--")
        .arg(&child)
        .args(["--exact", "crash_child"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _output_cleanup = RunOutputCleanup {
        store: &store,
        stderr: &stderr,
    };
    // The whole point of the taxonomy (`umbra-cli/src/main.rs:8-11`): the
    // child's own status never becomes umbra's. A crashed tracee is exit 1 --
    // not 137, not 139, not the signal number, and not the child's code.
    assert_eq!(output.status.code(), Some(1), "{case}: {stderr}");
    assert!(
        output.stdout.is_empty(),
        "{case}: diagnostics belong on stderr: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!stderr.contains("panicked"), "{case} panicked: {stderr}");
    let id = uuid::Uuid::parse_str(prepared_id(&stderr).expect("prepared run ID")).unwrap();
    let run_dir = store.join(id.to_string());
    let _cleanup = RunDirectory(run_dir.clone());
    match crash {
        Crash::Exit(code) => {
            // The `Some(status)` arm of `run`'s `match outcome.root_status`,
            // which builds `ErrorKind::ProcessFailed` under `"run.child"`: a
            // clean teardown around a nonzero root status is a run result, not
            // a malfunction.
            //
            // Named rather than cited by line, like its `Crash::Signal` sibling
            // below. The range this carried (`run.rs:794-798`) was already
            // stale on master -- it lands on journal-provider construction --
            // and the round-1 sweep converted the sibling four lines away while
            // leaving this one, which is the lexical-not-mechanism failure the
            // sweep was for.
            assert!(stderr.contains("ProcessFailed"), "{case}: {stderr}");
            assert!(stderr.contains("run.child"), "{case}: {stderr}");
            assert!(
                stderr.contains(&format!("Code({code})")),
                "{case}: the child's status is named in the diagnostic: {stderr}"
            );
            assert!(
                journal_records_completion(&run_dir, RunId(id), &case),
                "{case}: a reaped tree still finishes the run"
            );
        }
        Crash::Signal(_) => {
            // A fatal signal never reaches that branch. The macOS backend
            // refuses the stop it cannot resume from -- the `tracee stop` /
            // `fatal signal/exception` error at the end of `NativeTracer::stop`
            // -- so the event loop fails and `run` returns that error instead of
            // reaching its `match outcome.root_status`.
            //
            // Named rather than cited by line for the reason the
            // `RefusedByEnforcement` comment above gives: both ranges this
            // sentence used to carry were stale, and one of them was already
            // stale on master.
            assert!(stderr.contains("tracee stop"), "{case}: {stderr}");
            assert!(
                stderr.contains("fatal signal/exception"),
                "{case}: {stderr}"
            );
            assert!(
                !journal_records_completion(&run_dir, RunId(id), &case),
                "{case}: a run that lost its tracee must not be recorded complete"
            );
        }
    }
    // Either way the lease is surrendered rather than left for a takeover.
    assert_lease_released(&run_dir, &case);
    eprintln!("PASS crash {case}: umbra exit 1, lease released");
    std::fs::remove_dir_all(run_dir).unwrap();
}

#[test]
fn a_nonzero_child_does_not_lend_umbra_its_exit_code() {
    crash_case(Crash::Exit(41));
}

#[test]
fn a_sigkilled_child_fails_closed() {
    crash_case(Crash::Signal(9));
}

#[test]
fn a_sigsegv_child_fails_closed() {
    crash_case(Crash::Signal(11));
}
