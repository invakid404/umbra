//! Preparation failure must release writer ownership and permit reopening.
#![cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use umbra_core::provider::{ProviderDescriptor, ProviderRegistry, encode};
use umbra_core::{BytePath, RunId, UmbraError};
use umbra_supervisor::run::{CommandLaunch, RunLaunch, RunSpec};
use umbra_supervisor::{ResumeSpec, RunObserver, RunOutcome, RunPersistence, Supervisor};

struct Observer {
    fail: bool,
    id: Arc<Mutex<Option<RunId>>>,
    callbacks: Arc<Mutex<Vec<&'static str>>>,
}
impl RunObserver for Observer {
    fn prepared(&mut self, run_id: RunId) {
        *self.id.lock().unwrap() = Some(run_id);
        if self.fail {
            panic!("probe observer failure");
        }
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
fn scenario(label: &str, fail: bool) {
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
    let spec = RunSpec {
        registry: registry(&store, true),
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
            fail,
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
    if fail {
        assert!(outcome.is_err(), "preparation failure must return an error");
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
    scenario("control", false);
}
#[test]
fn prepared_panic_returns_error_and_releases_writer() {
    scenario("failure", true);
}
