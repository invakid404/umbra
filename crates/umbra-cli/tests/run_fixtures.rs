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
    /// The utility's path syscall is outside the tracer's intercepted set.
    /// `umbra-platform-macos/src/native.rs:464-495` lists the libc stubs that get
    /// a breakpoint and `abi.rs:95-115` the syscall numbers whose operands are
    /// rewritten: `mkdirat` is in both, bare `mkdir(2)` is in neither. The
    /// operand therefore reaches the kernel unrewritten and the unconditional
    /// sandbox refuses it, so the child fails and the operation happens
    /// *nowhere*: not on the host, not in the shadow. Enforcement, not
    /// rewriting, is what contains this case.
    RefusedByEnforcement,
}

/// `mkdir` + `touch` + `cat` + `ls` under `umbra run`, on one backend.
///
/// These are absolute paths: `PATH` is never searched
/// (`umbra-supervisor/src/run.rs:364-370`).
fn utilities(nfs: bool) {
    // The C fixture is not launched here. Its path is the signal the suite
    // already uses for "this host has workspace binaries built and debugger
    // permission granted" (`run_fixtures.rs:13-23`, `ci.yml:265-268`), which is
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
        ("/bin/mkdir", "made", Expect::RefusedByEnforcement),
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
            Expect::Captured(_) | Expect::ReadOnly => assert!(
                output.status.success(),
                "{case} after {:.3}s: {stderr}",
                elapsed.as_secs_f64()
            ),
            Expect::RefusedByEnforcement | Expect::Diagnoses => {
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
            Expect::RefusedByEnforcement => {
                assert!(!operand.exists(), "host operand created for {case}");
                assert!(!shadow.exists(), "{case} captured a refused operation");
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
            // The nonzero-status arm of `run`'s root-status match
            // (`umbra-supervisor/src/run.rs:794-798`, under the match at
            // `:792`): a clean teardown around a nonzero root status is a run
            // result, not a malfunction.
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
            // refuses the stop it cannot resume from
            // (`umbra-platform-macos/src/native.rs:1440-1443`), so the event
            // loop fails and `run` reports the platform's error
            // (`umbra-supervisor/src/run.rs:769-772`) instead of a root status.
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
