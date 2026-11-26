//! Native registered-library invocation with caller-owned callbacks.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use std::{cell::RefCell, collections::BTreeMap, os::unix::ffi::OsStrExt, path::Path, rc::Rc};
    use umbra_core::{
        provider::{encode, ProviderDescriptor, ProviderRegistry, PROTOCOL_VERSION},
        BytePath, RunId,
    };
    use umbra_supervisor::{
        managed_entry::{execute, RegisteredRole},
        run::{CommandLaunch, RunLaunch, RunObserver, RunOutcome, RunPersistence, RunSpec},
    };
    match RegisteredRole::enter().unwrap() {
        Some(mut role) => {
            if let Err(error) = execute(&mut role) {
                eprintln!("managed library helper: {error}");
                std::process::exit(1);
            }
            return;
        }
        None => {}
    }
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP: managed_library requires native qualification");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let store = temp.path().join("store");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&store).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let descriptor =
        |id: &str, role: &str, package: &str, capabilities: &[&str], options| ProviderDescriptor {
            id: id.into(),
            role: role.into(),
            protocol_version: PROTOCOL_VERSION,
            executable: BytePath::new(
                root.join("target/debug")
                    .join(package)
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
            )
            .unwrap(),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            options,
        };
    let mut providers = BTreeMap::new();
    providers.insert(
        "storage".into(),
        descriptor(
            "local",
            "storage",
            "umbra-storage-local",
            &["local-development-v1", "experimental-open-rewrite-v1"],
            encode(&BytePath::new(store.as_os_str().as_bytes().to_vec()).unwrap()).unwrap(),
        ),
    );
    providers.insert(
        "journal".into(),
        descriptor("file", "journal", "umbra-journal-file", &[], vec![]),
    );
    providers.insert(
        "platform".into(),
        descriptor(
            "macos",
            "platform",
            "umbra-platform-macos",
            &[
                "sandboxed-stopped-launch-v1",
                "experimental-syscall-rewrite-v1",
            ],
            br#"{"timeout_ms":20000}"#.to_vec(),
        ),
    );
    struct Observer {
        caller: u32,
        events: Rc<RefCell<Vec<&'static str>>>,
        run: Option<RunId>,
    }
    impl RunObserver for Observer {
        fn stdio_adopted(&mut self) -> umbra_core::Result<()> {
            assert_eq!(std::process::id(), self.caller);
            self.events.borrow_mut().push("stdio");
            Ok(())
        }
        fn prepared(&mut self, run: RunId) -> umbra_core::Result<()> {
            assert_eq!(std::process::id(), self.caller);
            self.run = Some(run);
            self.events.borrow_mut().push("prepared");
            Ok(())
        }
        fn finished(&mut self, outcome: &RunOutcome) {
            assert_eq!(std::process::id(), self.caller);
            assert_eq!(Some(outcome.run_id), self.run);
            assert_eq!(outcome.root_status, Some(umbra_core::ExitStatus::Code(0)));
            self.events.borrow_mut().push("finished");
        }
        fn teardown_warning(&mut self, error: &umbra_core::UmbraError) {
            panic!("{error}");
        }
    }
    let events = Rc::new(RefCell::new(Vec::new()));
    let spec = RunSpec {
        registry: ProviderRegistry {
            timeout_ms: 25_000,
            providers,
        },
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(b"/bin/true".to_vec()).unwrap(),
            argv: vec![b"/bin/true".to_vec()],
            environment: vec![],
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
        }),
        workspace,
        persistence: RunPersistence::LocalDevelopment,
        experimental: true,
        state_root: temp.path().join("state"),
        observer: Some(Box::new(Observer {
            caller: std::process::id(),
            events: events.clone(),
            run: None,
        })),
    };
    let mut run = RegisteredRole::prepare(spec, true, 64).unwrap();
    execute(&mut run).unwrap();
    assert_eq!(*events.borrow(), ["stdio", "prepared", "finished"]);
    println!("managed_library: caller callbacks and complete registered run passed");
}
