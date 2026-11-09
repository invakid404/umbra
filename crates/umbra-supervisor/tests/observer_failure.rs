//! Preparation failure must release writer ownership and permit reopening.
#![cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use umbra_core::provider::{encode, ProviderDescriptor, ProviderRegistry};
use umbra_core::{BytePath, RunId, UmbraError};
use umbra_supervisor::run::{CommandLaunch, RunLaunch, RunSpec};
use umbra_supervisor::{ResumeSpec, RunObserver, RunOutcome, RunPersistence, Supervisor};

struct HostilePayload;
impl std::fmt::Display for HostilePayload {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("panic payload must not be formatted");
    }
}
impl Drop for HostilePayload {
    fn drop(&mut self) {
        panic!("panic payload must not be dropped");
    }
}

struct Observer {
    failure: u8,
    id: Arc<Mutex<Option<RunId>>>,
    callbacks: Arc<Mutex<Vec<&'static str>>>,
}
impl RunObserver for Observer {
    fn prepared(&mut self, run_id: RunId) -> umbra_core::Result<()> {
        *self.id.lock().unwrap() = Some(run_id);
        match self.failure {
            1 => panic!("observer failure"),
            2 | 4 | 5 | 6 => {
                return Err(umbra_core::UmbraError {
                    kind: umbra_core::ErrorKind::Io,
                    operation: "test.prepared".into(),
                    context: "observer rejected preparation".into(),
                    errno: None,
                    launch_tree_terminated: false,
                })
            }
            3 => std::panic::panic_any(HostilePayload),
            _ => {}
        }
        Ok(())
    }
    fn finished(&mut self, outcome: &RunOutcome) {
        let _ = outcome;
        self.callbacks.lock().unwrap().push("finished");
    }
    fn teardown_warning(&mut self, error: &UmbraError) {
        let _ = error;
        self.callbacks.lock().unwrap().push("teardown_warning");
    }
}
fn bin(name: &str) -> BytePath {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    let p = p.join(name);
    assert!(p.is_file(), "{}", p.display());
    BytePath::new(p.as_os_str().as_bytes().to_vec()).unwrap()
}
fn descriptor(
    id: &str,
    role: &str,
    exe: BytePath,
    caps: &[&str],
    options: Vec<u8>,
) -> ProviderDescriptor {
    ProviderDescriptor {
        id: id.into(),
        role: role.into(),
        protocol_version: umbra_core::provider::PROTOCOL_VERSION,
        executable: exe,
        capabilities: caps.iter().map(|c| c.to_string()).collect(),
        options,
    }
}
fn registry(root: &Path, platform: bool) -> ProviderRegistry {
    let store = BytePath::new(root.as_os_str().as_bytes().to_vec()).unwrap();
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "storage".to_owned(),
        descriptor(
            "local",
            "storage",
            bin("umbra-storage-local"),
            &["local-development-v1", "experimental-open-rewrite-v1"],
            encode(&store).unwrap(),
        ),
    );
    providers.insert(
        "journal".to_owned(),
        descriptor("file", "journal", bin("umbra-journal-file"), &[], vec![]),
    );
    if platform {
        providers.insert(
            "platform".to_owned(),
            descriptor(
                "macos",
                "platform",
                bin("umbra-platform-macos"),
                &[
                    "sandboxed-stopped-launch-v1",
                    "experimental-syscall-rewrite-v1",
                ],
                vec![],
            ),
        );
    }
    ProviderRegistry {
        providers,
        timeout_ms: 20_000,
    }
}
fn locks(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            locks(&e.path(), out)
        } else if e.file_name() == "writer.lock" {
            out.push(e.path())
        }
    }
}
fn scenario(label: &str, failure: u8) {
    if std::env::var_os("UMBRA_TEST_OBSERVER").is_none()
        && std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none()
    {
        eprintln!(
            "SKIP observer integration: set UMBRA_TEST_OBSERVER or UMBRA_INTEGRATION_REQUIRED"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let store = store.canonicalize().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("input.txt"), b"x").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let id = Arc::new(Mutex::new(None));
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let journal_log = store.join("journal-observed.log");
    let mut observed_registry = registry(&store, true);
    let journal = observed_registry.providers.get_mut("journal").unwrap();
    journal.executable = bin("umbra-fix-227-journal");
    journal.options = format!(
        "{}\n{}",
        if failure == 4 {
            "fail-close"
        } else {
            "observe"
        },
        journal_log.display()
    )
    .into_bytes();
    let storage = observed_registry.providers.get_mut("storage").unwrap();
    storage.executable = bin("umbra-fix-227-storage");
    storage.options = format!(
        "{}\n{}\n{}",
        match failure {
            5 => "release",
            6 => "close-run",
            _ => "observe",
        },
        store.display(),
        journal_log.display()
    )
    .into_bytes();
    let platform_marker = root.path().join("platform-started");
    if failure != 0 {
        let sentinel = root.path().join("platform-sentinel");
        std::fs::copy(bin("umbra-fix-227-platform-sentinel"), &sentinel).unwrap();
        observed_registry
            .providers
            .get_mut("platform")
            .unwrap()
            .executable = BytePath::new(sentinel.as_os_str().as_bytes().to_vec()).unwrap();
    }
    let spec = RunSpec {
        registry: observed_registry,
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(b"/bin/echo".to_vec()).unwrap(),
            argv: vec![b"echo".to_vec(), b"ok".to_vec()],
            environment: Vec::new(),
            cwd: BytePath::new(workspace.as_os_str().as_bytes().to_vec()).unwrap(),
        }),
        workspace: workspace.clone(),
        persistence: RunPersistence::LocalDevelopment,
        experimental: true,
        state_root: PathBuf::from("/nonexistent/umbra-state"),
        observer: Some(Box::new(Observer {
            failure,
            id: id.clone(),
            callbacks: callbacks.clone(),
        })),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| umbra_supervisor::run(spec)));
    match &outcome {
        Err(_) => eprintln!("PROBE {label}: run UNWOUND past run()"),
        Ok(r) => eprintln!("PROBE {label}: run returned {r:?}"),
    }
    let mut found = Vec::new();
    locks(&store, &mut found);
    eprintln!("PROBE {label}: writer.lock count={} {found:?}", found.len());
    let run_id = id.lock().unwrap().expect("prepared reached");
    assert!(
        !platform_marker.exists(),
        "prepared failure must not connect platform or launch a target"
    );
    let journal_events = std::fs::read_to_string(&journal_log).unwrap();
    assert_eq!(
        journal_events
            .lines()
            .filter(|line| *line == "Close")
            .count(),
        1,
        "{journal_events}"
    );
    let terminal: Vec<_> = journal_events
        .lines()
        .filter(|line| ["Close", "Release", "CloseRun"].contains(line))
        .collect();
    assert_eq!(terminal, ["Close", "Release", "CloseRun"]);
    if failure != 0 {
        assert!(!journal_events.lines().any(|line| line == "Completed"));
    }
    let resumed = Supervisor::resume(ResumeSpec {
        registry: registry(&store, false),
        run_id,
        workspace,
        persistence: RunPersistence::LocalDevelopment,
        state_root: PathBuf::from("/nonexistent/umbra-state"),
    });
    eprintln!("{label}: resume -> {resumed:?}");
    assert!(outcome.is_ok(), "prepared panic escaped run");
    let outcome = outcome.unwrap();
    if failure != 0 {
        let error = outcome.expect_err("preparation failure must return an error");
        if failure == 5 {
            assert!(error.context.contains("injected writer release"), "{error}");
        }
        if failure == 6 {
            assert!(error.context.contains("injected storage close"), "{error}");
        }
        if failure == 4 {
            assert!(error.context.contains("injected journal close"), "{error}");
        }
        assert_eq!(
            error.operation,
            if matches!(failure, 2 | 4 | 5 | 6) {
                "test.prepared"
            } else {
                "run.observer"
            }
        );
        assert!(
            callbacks.lock().unwrap().is_empty(),
            "no callbacks after failed preparation"
        );
    } else {
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(*callbacks.lock().unwrap(), vec!["finished"]);
    }
    assert!(found.is_empty(), "writer ownership stranded: {found:?}");
    assert!(resumed.is_ok(), "run cannot reopen: {resumed:?}");
    let resumed = resumed.unwrap();
    assert!(!resumed.recovery_required);
    assert_eq!(resumed.unfinished, 0);
}
#[test]
fn prepared_success_releases_writer() {
    scenario("control", 0);
}
#[test]
fn prepared_panic_returns_error_and_releases_writer() {
    scenario("panic", 1);
}

#[test]
fn prepared_error_returns_error_and_releases_writer() {
    scenario("error", 2);
}
#[test]
fn prepared_hostile_panic_payload_cannot_prevent_cleanup() {
    scenario("hostile-payload", 3);
}

#[test]
fn prepared_error_preserves_journal_close_failure() {
    scenario("close-error", 4);
}

#[test]
fn prepared_error_preserves_writer_release_failure() {
    scenario("release-error", 5);
}
#[test]
fn prepared_error_preserves_storage_close_failure() {
    scenario("storage-close-error", 6);
}
