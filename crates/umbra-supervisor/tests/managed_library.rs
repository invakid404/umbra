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
    if let Some(mut role) = RegisteredRole::enter().unwrap() {
        if let Err(error) = execute(&mut role) {
            eprintln!("managed library helper: {error}");
            std::process::exit(1);
        }
        return;
    }
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP: managed_library requires native qualification");
        return;
    }
    let mode = std::env::args().nth(1).unwrap_or_else(|| "success".into());
    assert!(["success", "missing", "prepared_error", "prepared_panic"].contains(&mode.as_str()));
    let capacity: u64 = std::env::args()
        .nth(2)
        .map(|value| value.parse().unwrap())
        .unwrap_or(64);
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
        mode: String,
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
            if self.mode == "prepared_panic" {
                panic!("managed library prepared sentinel");
            }
            if self.mode == "prepared_error" {
                return Err(umbra_core::UmbraError::new(
                    umbra_core::ErrorKind::InvalidState,
                    "library.observer",
                    "managed library prepared sentinel",
                ));
            }
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
    let executable = if mode == "missing" {
        workspace
            .join("missing-program")
            .as_os_str()
            .as_bytes()
            .to_vec()
    } else {
        b"/usr/bin/true".to_vec()
    };
    let spec = RunSpec {
        registry: ProviderRegistry {
            timeout_ms: 25_000,
            providers,
        },
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(executable.clone()).unwrap(),
            argv: vec![executable],
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
            mode: mode.clone(),
        })),
    };
    let began = std::time::Instant::now();
    let mut run = RegisteredRole::prepare(spec, true, capacity).unwrap();
    let result = execute(&mut run);
    let elapsed = began.elapsed();
    let bound = if mode == "success" {
        26.35 + 4.0 * capacity as f64
    } else {
        30.6 + 8.0 * capacity as f64
    };
    assert!(
        elapsed.as_secs_f64() <= bound,
        "managed run exceeded D4: {elapsed:?} > {bound}s"
    );
    if mode == "success" {
        result.unwrap();
        assert_eq!(*events.borrow(), ["stdio", "prepared", "finished"]);
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match mode.as_str() {
            "missing" => "twin source",
            "prepared_error" => "managed library prepared sentinel",
            "prepared_panic" => "managed preparation observer panicked",
            _ => unreachable!(),
        };
        assert!(
            error.contains(expected),
            "original failure missing: {error}"
        );
        assert!(!events.borrow().contains(&"finished"));
    }
    println!("managed_library: {mode} capacity={capacity} elapsed={elapsed:?} passed");
}
