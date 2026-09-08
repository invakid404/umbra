//! Reproducible enforced fixture matrix. Build workspace binaries and the C
//! fixture first. Required qualification also needs an existing NFSv4 mount.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{json, Value};
use std::{
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
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
    let registry = json!({"timeout_ms": 5000, "providers": {
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
        let output = command
            .arg("--")
            .arg(&fixture)
            .arg(case)
            .arg(&destination)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{case}: {stderr}");
        let id = stderr
            .lines()
            .find_map(|line| {
                line.strip_prefix("umbra: run ")
                    .and_then(|line| line.strip_suffix(" prepared"))
            })
            .expect("prepared run ID");
        let id = uuid::Uuid::parse_str(id).unwrap();
        let run_dir = store.join(id.to_string());
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
            "PASS {} {case}: {} exact bytes, host absent, lease released, RunCompleted",
            if nfs { "nfs" } else { "local" },
            expected.len()
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
    matrix(true);
}
