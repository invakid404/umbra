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
        let log = std::fs::read(run_dir.join("control/journal/log")).unwrap();
        assert!(
            log.windows(b"RunCompleted".len())
                .any(|w| w == b"RunCompleted"),
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
