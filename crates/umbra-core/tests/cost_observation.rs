//! Cost observation must leave the production startup interval uninterrupted.
#![cfg(all(unix, feature = "cost-witness"))]

#[test]
fn startup_observation_runs_only_after_readiness() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let observer = directory.path().join("observer");
    std::fs::write(
        &observer,
        "#!/bin/sh\nprintf '%s\\n' \"$2\" >> \"$1/invoked\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&observer, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "observation_child"])
        .env("UMBRA_COST_OBSERVER", observer)
        .env("UMBRA_COST_DIRECTORY", directory.path())
        .env("UMBRA_COST_TEST_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("invoked")).unwrap(),
        "role-go-Worker\n",
        "startup invoked a blocking observer before worker readiness"
    );
}

#[test]
fn observation_child() {
    if std::env::var_os("UMBRA_COST_TEST_CHILD").is_none() {
        return;
    }
    for event in [
        "topology-before",
        "topology-pair",
        "helper-bootstrap-created",
        "provider-command-before",
        "provider-command-after",
        "provider-listener-closed",
        "role-go-Guardian",
        "role-go-CleanupOwner",
        "role-go-Worker",
    ] {
        umbra_core::cost_snapshot!(event, None, &[]);
    }
}
