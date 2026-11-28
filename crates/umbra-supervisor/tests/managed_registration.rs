//! Refuse unsupported managed construction before validation or acquisition.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use std::{collections::BTreeMap, process::Command};
    use umbra_core::{provider::ProviderRegistry, BytePath};
    use umbra_supervisor::{
        managed_entry::RegisteredRole,
        run::{CommandLaunch, RunLaunch, RunPersistence, RunSpec},
    };
    let case = std::env::args().nth(1);
    if case.is_none() {
        for case in ["unregistered", "malformed"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.arg(case).env_remove("UMBRA_MANAGED_ROLE");
            if case == "malformed" {
                command.env("UMBRA_MANAGED_ROLE", "malformed");
            }
            assert!(command.status().unwrap().success(), "{case}");
        }
        return;
    }
    if case.as_deref() == Some("malformed") {
        assert!(RegisteredRole::enter().is_err());
    }
    // These paths and providers are deliberately unusable. Registration must
    // refuse before configuration validation, storage access or helper spawn.
    let spec = RunSpec {
        registry: ProviderRegistry {
            timeout_ms: 0,
            providers: BTreeMap::new(),
        },
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(b"/missing".to_vec()).unwrap(),
            argv: vec![],
            environment: vec![],
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
        }),
        workspace: "/missing".into(),
        persistence: RunPersistence::LocalDevelopment,
        experimental: false,
        state_root: "/missing".into(),
        observer: None,
    };
    let error = match RegisteredRole::prepare(spec, true, 64) {
        Err(error) => error,
        Ok(_) => panic!("unregistered executable acquired a role"),
    };
    assert!(
        error
            .to_string()
            .contains("executable has not registered managed role dispatch"),
        "{error}"
    );
    println!(
        "managed_registration: {} refused before acquisition",
        case.unwrap()
    );
}
