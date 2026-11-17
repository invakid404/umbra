#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;
use umbra_core::*;

fn path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}

#[test]
fn shutdown_native_root_ledger() {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP shutdown_native_root_ledger: requires qualified native provider");
        return;
    }
    let evidence = std::env::var_os("UMBRA_FIX_271_EVIDENCE_FILE")
        .expect("dedicated acquisition evidence path");
    // A whole qualification command may append observations from earlier tests.
    // Keep those bytes and inspect only this serial test's newly appended span.
    let witness_start = std::fs::metadata(&evidence)
        .map(|metadata| metadata.len() as usize)
        .unwrap_or(0);
    // Native control: exact owned terminal collection, including a non-success
    // status. This is independent of the provider's ledger and RSP replies.
    let mut control = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    assert!(control.try_wait().unwrap().is_none());
    control.kill().unwrap();
    assert!(!control.wait().unwrap().success());

    let executable = Path::new(env!("CARGO_BIN_EXE_umbra-platform-macos"));
    let hash = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(executable)
        .output()
        .unwrap();
    assert!(hash.status.success());
    eprintln!(
        "provider identity: {}",
        String::from_utf8_lossy(&hash.stdout)
    );
    let bytes = std::fs::read(executable).unwrap();
    assert!(bytes
        .windows(b"shutdown-witness-v1".len())
        .any(|bytes| bytes == b"shutdown-witness-v1"));
    let descriptor = provider::ProviderDescriptor {
        id: "macos".into(),
        role: "platform".into(),
        protocol_version: provider::PROTOCOL_VERSION,
        executable: path(executable),
        capabilities: Default::default(),
        options: serde_json::to_vec(&umbra_platform_macos::Options::default()).unwrap(),
    };
    let mut platform = umbra_platform::provider::connect(&descriptor, 5000).unwrap();
    let shadow = tempfile::tempdir().unwrap();
    let literal = format!("\"{}\"", shadow.path().display());
    let profile = include_str!("../../../experiments/seatbelt/umbra.sb")
        .replace("{{UMBRA_RUN_ROOT}}", &literal);
    let root = platform
        .control
        .launch(LaunchSpec {
            executable: path(Path::new("/bin/sleep")),
            argv: vec![b"sleep".to_vec(), b"60".to_vec()],
            environment: vec![],
            cwd: path(Path::new("/tmp")),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::Required(
                SandboxProfile::new(
                    SEATBELT_PROFILE_FORMAT,
                    profile.into_bytes(),
                    path(shadow.path()),
                )
                .unwrap(),
            ),
        })
        .unwrap();
    let preliminary = platform
        .control
        .terminate(root, TerminationPolicy::Immediate);
    eprintln!("preliminary termination: {preliminary:?}");
    let closure = platform.shutdown();
    eprintln!(
        "closure proof={} error={:?} final-to-exit={:?}",
        closure.tree_terminated, closure.error, closure.final_to_exit
    );
    assert!(closure.tree_terminated, "{:?}", closure.error);
    assert!(closure.error.is_none(), "{:?}", closure.error);
    assert!(
        closure.final_to_exit.unwrap() <= Duration::from_millis(50),
        "return to design gate: Final-to-exit exceeded 50ms"
    );
    let evidence_text = std::fs::read_to_string(evidence).unwrap();
    let records = evidence_text[witness_start..]
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let constructed = records
        .iter()
        .find(|record| record["event"] == "owner-constructed")
        .unwrap();
    let owner_thread = &constructed["thread"];
    let owner_pid = &constructed["writer_pid"];
    assert!(constructed["writer_instance"].as_str().is_some());
    for name in ["owner-begin", "owner-drop", "owner-terminal", "acquired"] {
        let observations: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == name)
            .collect();
        assert!(
            !observations.is_empty(),
            "missing native affinity event {name}"
        );
        for record in observations {
            assert_eq!(&record["thread"], owner_thread);
            assert_eq!(&record["writer_pid"], owner_pid);
            assert_eq!(record["writer_instance"], constructed["writer_instance"]);
        }
    }
    for name in ["transport-start", "transport-final"] {
        let record = records
            .iter()
            .find(|record| record["event"] == name)
            .unwrap();
        assert_ne!(&record["thread"], owner_thread);
        assert_eq!(&record["writer_pid"], owner_pid);
    }
    let position = |name: &str| {
        records
            .iter()
            .position(|record| record["event"] == name)
            .unwrap()
    };
    assert!(position("owner-terminal") < position("transport-final"));
    let final_record = records
        .iter()
        .find(|record| record["event"] == "final-to-exit")
        .unwrap();
    assert_eq!(final_record["timing_origin"], "final-decoded");
    let transport_final = &records[position("transport-final")];
    assert_eq!(
        transport_final["writer_instance"],
        constructed["writer_instance"]
    );
    assert_eq!(transport_final["nonce"], final_record["nonce"]);
    assert_eq!(
        transport_final["instance"],
        final_record["accepted"]["instance"]
    );
    let accepted: shutdown::Accepted =
        serde_json::from_value(final_record["accepted"].clone()).unwrap();
    let final_state: shutdown::Final =
        serde_json::from_value(final_record["final"].clone()).unwrap();
    assert!(accepted.confirms(&final_state).unwrap());
    let replacement = final_state
        .entries
        .iter()
        .find(|entry| entry.slot.replaces.is_some())
        .expect("exec replacement link");
    assert!(final_state
        .entries
        .iter()
        .any(|entry| Some(entry.slot.id) == replacement.slot.replaces));
    assert!(
        final_state
            .entries
            .iter()
            .filter(|entry| entry.slot.kind != shutdown::ResourceKind::Tracer)
            .all(|entry| entry.identity.as_ref().unwrap().birth.is_some()),
        "retain captured native birth identities"
    );
    // This witness is recorded immediately after each creation return, before
    // the ledger acquisition hook. Equality of two omitted inventories cannot
    // satisfy it; repeated task-right acquisitions retain multiplicity.
    let mut observed = records
        .iter()
        .filter(|record| record["event"] == "acquired")
        .map(|record| {
            (
                record["kind"].as_str().unwrap().to_owned(),
                record["pid"].as_i64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let mut accounted = final_state
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .identity
                .as_ref()
                .and_then(|id| id.pid)
                .map(|pid| (format!("{:?}", entry.slot.kind), i64::from(pid)))
        })
        .collect::<Vec<_>>();
    observed.sort();
    accounted.sort();
    assert_eq!(
        observed, accounted,
        "creation-return witness must match every acquired ledger slot"
    );
    assert_eq!(
        observed.len(),
        4,
        "root tracee, initial and exec task rights, and owned tracer"
    );
}
