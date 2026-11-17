#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;
use umbra_core::*;

fn path(path: &Path) -> BytePath { BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap() }

#[test]
fn shutdown_native_root_ledger() {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP shutdown_native_root_ledger: requires qualified native provider");
        return;
    }
    assert!(cfg!(feature = "shutdown-witness"), "qualification requires shutdown-witness in this invocation");
    let evidence = std::env::var_os("UMBRA_FIX_271_EVIDENCE_FILE").expect("dedicated acquisition evidence path");
    assert!(!Path::new(&evidence).exists(), "fresh reading, never overwrite");
    // Native control: exact owned terminal collection, including a non-success
    // status. This is independent of the provider's ledger and RSP replies.
    let mut control = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
    assert!(control.try_wait().unwrap().is_none());
    control.kill().unwrap();
    assert!(!control.wait().unwrap().success());

    let executable = Path::new(env!("CARGO_BIN_EXE_umbra-platform-macos"));
    let hash = std::process::Command::new("/usr/bin/shasum").args(["-a", "256"]).arg(executable).output().unwrap();
    assert!(hash.status.success());
    eprintln!("provider identity: {}", String::from_utf8_lossy(&hash.stdout));
    let bytes = std::fs::read(executable).unwrap();
    assert!(bytes.windows(b"shutdown-witness-v1".len()).any(|bytes| bytes == b"shutdown-witness-v1"));
    let descriptor = provider::ProviderDescriptor {
        id: "macos".into(), role: "platform".into(), protocol_version: provider::PROTOCOL_VERSION,
        executable: path(executable), capabilities: Default::default(),
        options: serde_json::to_vec(&umbra_platform_macos::Options::default()).unwrap(),
    };
    let mut platform = umbra_platform::provider::connect(&descriptor, 5000).unwrap();
    let root = platform.control.launch(LaunchSpec {
        executable: path(Path::new("/bin/sleep")), argv: vec![b"sleep".to_vec(), b"60".to_vec()],
        environment: vec![], cwd: path(Path::new("/tmp")),
        policy: LaunchPolicy { persistence: PersistencePolicy::LocalDevelopment,
            inherited_fds: vec![], interpose: false, descriptor_limit: None },
        sandbox: SandboxRequirement::UnsandboxedExperiment,
    }).unwrap();
    let preliminary = platform.control.terminate(root, TerminationPolicy::Immediate);
    eprintln!("preliminary termination: {preliminary:?}");
    let closure = platform.shutdown();
    eprintln!("closure proof={} error={:?} final-to-exit={:?}", closure.tree_terminated, closure.error, closure.final_to_exit);
    assert!(closure.tree_terminated, "{:?}", closure.error);
    assert!(closure.error.is_none(), "{:?}", closure.error);
    assert!(closure.final_to_exit.unwrap() <= Duration::from_millis(50), "return to design gate: Final-to-exit exceeded 50ms");
    let records = std::fs::read_to_string(evidence).unwrap().lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
    let final_record = records.iter().find(|record| record["event"] == "final-to-exit").unwrap();
    let accepted: shutdown::Accepted = serde_json::from_value(final_record["accepted"].clone()).unwrap();
    let final_state: shutdown::Final = serde_json::from_value(final_record["final"].clone()).unwrap();
    assert!(accepted.confirms(&final_state).unwrap());
    // This witness is recorded immediately after each creation return, before
    // the ledger acquisition hook. Equality of two omitted inventories cannot
    // satisfy it; repeated task-right acquisitions retain multiplicity.
    let mut observed = records.iter().filter(|record| record["event"] == "acquired")
        .map(|record| (record["kind"].as_str().unwrap().to_owned(), record["pid"].as_i64().unwrap()))
        .collect::<Vec<_>>();
    let mut accounted = final_state.entries.iter().filter_map(|entry| {
        entry.identity.as_ref().and_then(|id| id.pid).map(|pid| (format!("{:?}", entry.slot.kind), i64::from(pid)))
    }).collect::<Vec<_>>();
    observed.sort(); accounted.sort();
    assert_eq!(observed, accounted, "creation-return witness must match every acquired ledger slot");
    assert_eq!(observed.len(), 3, "root tracee, task right, and owned tracer");
}
