#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{json, Value};
use std::{os::unix::ffi::OsStrExt, path::Path, process::Command};

fn idle(seconds: u32, cpu: bool) {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bin = Path::new(env!("CARGO_BIN_EXE_umbra"));
    let program = if cpu {
        let program = root.join("cpu");
        assert!(Command::new("cc")
            .args(["-O2", "-o"])
            .arg(&program)
            .arg(repo.join("experiments/fixtures/umbra-fix-262-cpu.c"))
            .status()
            .unwrap()
            .success());
        program
    } else {
        "/bin/sleep".into()
    };
    let native = Command::new(&program)
        .arg(seconds.to_string())
        .output()
        .unwrap();
    eprintln!("native seconds={seconds} cpu={cpu}: {}", native.status);
    assert!(native.status.success());
    let workspace = root.join("workspace");
    let store = root.join("store");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&store).unwrap();
    let descriptor = |name: &str, capabilities: &[&str], options: Vec<u8>| {
        let mut d: Value = serde_json::from_slice(
            &std::fs::read(repo.join("crates").join(name).join("provider.json")).unwrap(),
        )
        .unwrap();
        let executable = bin.parent().unwrap().join(name);
        assert!(
            executable.is_file(),
            "build provider: {}",
            executable.display()
        );
        d["executable"] = json!(executable.as_os_str().as_bytes());
        d["capabilities"] = json!(capabilities);
        d["options"] = json!(options);
        d
    };
    // Omit timeout_ms: this regression exercises the production default.
    let registry = json!({"providers": {
        "platform": descriptor("umbra-platform-macos", &["sandboxed-stopped-launch-v1", "experimental-syscall-rewrite-v1"], serde_json::to_vec(&json!({"timeout_ms":60000})).unwrap()),
        "journal": descriptor("umbra-journal-file", &[], vec![]),
        "storage": descriptor("umbra-storage-local", &["local-development-v1", "experimental-open-rewrite-v1"], serde_json::to_vec(store.as_os_str().as_bytes()).unwrap())
    }});
    let path = root.join("registry.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let out = Command::new(bin)
        .args(["run", "--registry"])
        .arg(path)
        .arg("--workspace")
        .arg(workspace)
        .arg("--state-dir")
        .arg(root.join("state"))
        .args(["--experimental", "--local-dev", "--"])
        .arg(program)
        .arg(seconds.to_string())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "supervised seconds={seconds} cpu={cpu}: {}\n{stderr}",
        out.status
    );
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Some(Code(0))"), "{stderr}");
}

#[test]
fn fix_262_short_sleep() {
    idle(1, false);
}
#[test]
fn fix_262_default_deadline_sleep() {
    idle(8, false);
}
#[test]
fn fix_262_cpu_quiet_interval() {
    idle(8, true);
}
