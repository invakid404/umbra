//! Same disposable library workload for the observational base and managed head.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;
    use umbra_core::{provider::ProviderRegistry, BytePath};
    use umbra_supervisor::run::{CommandLaunch, RunLaunch, RunPersistence, RunSpec};

    let Some(directory) = std::env::var_os("UMBRA_COST_LIBRARY_SETUP") else {
        eprintln!("D5 library fixture requires an explicit disposable setup");
        return;
    };
    #[cfg(feature = "cost-library-managed")]
    {
        use umbra_supervisor::managed_entry::{execute, RegisteredRole};
        if let Some(mut role) = RegisteredRole::enter().unwrap() {
            execute(&mut role).unwrap();
            return;
        }
    }
    let directory = PathBuf::from(directory);
    let registry: ProviderRegistry =
        umbra_core::provider::decode(&std::fs::read(directory.join("registry.json")).unwrap())
            .unwrap();
    let executable = b"/usr/bin/true".to_vec();
    let workspace = directory.join("workspace");
    let spec = RunSpec {
        registry,
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(executable.clone()).unwrap(),
            argv: vec![executable],
            environment: vec![],
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
        }),
        workspace,
        persistence: RunPersistence::LocalDevelopment,
        experimental: true,
        state_root: directory.join("state"),
        observer: None,
    };
    umbra_core::cost_snapshot!("library-before-prepare", None, &[]);
    #[cfg(feature = "cost-library-managed")]
    {
        use umbra_supervisor::managed_entry::{execute, RegisteredRole};
        let mut role = RegisteredRole::prepare(spec, true, 64).unwrap();
        if std::env::var_os("UMBRA_COST_LIBRARY_ABORT").is_some() {
            role.cleanup_failure().unwrap();
        } else {
            execute(&mut role).unwrap();
        }
        drop(role);
    }
    #[cfg(not(feature = "cost-library-managed"))]
    umbra_supervisor::run::run(spec).unwrap();
    umbra_core::cost_snapshot!("library-after-return", None, &[]);
}
