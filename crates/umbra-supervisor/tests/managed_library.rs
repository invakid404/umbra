//! Native registered-library invocation with caller-owned callbacks.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        os::unix::ffi::OsStrExt,
        path::Path,
        rc::Rc,
    };
    use umbra_core::{
        BytePath, RunId,
        provider::{PROTOCOL_VERSION, ProviderDescriptor, ProviderRegistry, encode},
    };
    use umbra_supervisor::{
        managed_entry::{RegisteredRole, execute},
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
    assert!(
        [
            "success",
            "missing",
            "prepared_error",
            "prepared_panic",
            "provider_death",
            "armed_reply_loss",
            "startup_abort",
            "missing_provider"
        ]
        .contains(&mode.as_str())
    );
    #[cfg(not(feature = "probe-managed-arm-reply-loss"))]
    if mode == "armed_reply_loss" {
        panic!("armed_reply_loss requires its compile-time fault");
    }
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
    if mode == "missing_provider" {
        providers.get_mut("platform").unwrap().executable = BytePath::new(
            workspace
                .join("missing-provider")
                .as_os_str()
                .as_bytes()
                .to_vec(),
        )
        .unwrap();
    }
    struct Observer {
        caller: u32,
        events: Rc<RefCell<Vec<&'static str>>>,
        run: Rc<Cell<Option<RunId>>>,
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
            self.run.set(Some(run));
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
            if self.mode == "provider_death" {
                stop_owned_provider(self.caller);
            }
            Ok(())
        }
        fn finished(&mut self, outcome: &RunOutcome) {
            assert_eq!(std::process::id(), self.caller);
            assert_eq!(Some(outcome.run_id), self.run.get());
            assert_eq!(outcome.root_status, Some(umbra_core::ExitStatus::Code(0)));
            self.events.borrow_mut().push("finished");
        }
        fn teardown_warning(&mut self, error: &umbra_core::UmbraError) {
            panic!("{error}");
        }
    }
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed_run = Rc::new(Cell::new(None));
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
            run: observed_run.clone(),
            mode: mode.clone(),
        })),
    };
    let began = std::time::Instant::now();
    let mut run = RegisteredRole::prepare(spec, true, capacity).unwrap();
    let result = if mode == "startup_abort" {
        run.cleanup_failure()
    } else {
        execute(&mut run)
    };
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
    if mode == "armed_reply_loss" {
        assert!(result.is_err(), "lost Armed response authorized a run");
        assert!(events.borrow().is_empty(), "callback before durable GO");
        let entries = std::fs::read_dir(&store)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            // LocalStorage keeps its capability probes beside run directories.
            .filter(|entry| entry.file_name() != ".umbra-probes")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1, "expected exactly one persisted run");
        let run_id =
            RunId(uuid::Uuid::parse_str(entries[0].file_name().to_str().unwrap()).unwrap());
        inspect_custody(&store, run_id, false);
    } else if mode == "startup_abort" || mode == "missing_provider" {
        if mode == "startup_abort" {
            result.unwrap();
        } else {
            assert!(result.is_err(), "missing provider startup succeeded");
        }
        assert!(
            events.borrow().is_empty(),
            "callback before startup readiness"
        );
        assert_eq!(
            std::fs::read_dir(&store).unwrap().count(),
            0,
            "storage acquired before GO"
        );
    } else if mode == "success" {
        result.unwrap();
        assert_eq!(*events.borrow(), ["stdio", "prepared", "finished"]);
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match mode.as_str() {
            "missing" => Some("twin source"),
            "prepared_error" => Some("managed library prepared sentinel"),
            "prepared_panic" => Some("managed preparation observer panicked"),
            "provider_death" => None,
            _ => unreachable!(),
        };
        if let Some(expected) = expected {
            assert!(
                error.contains(expected),
                "original failure missing: {error}"
            );
        } else {
            assert_eq!(*events.borrow(), ["stdio", "prepared"]);
            eprintln!("provider death retained failure: {error}");
        }
        assert!(!events.borrow().contains(&"finished"));
    }
    if let Some(run_id) = observed_run.get() {
        inspect_custody(&store, run_id, mode == "success");
    }
    println!("managed_library: {mode} capacity={capacity} elapsed={elapsed:?} passed");
}

#[cfg(target_os = "macos")]
fn inspect_custody(store: &std::path::Path, run_id: umbra_core::RunId, complete: bool) {
    use std::os::unix::ffi::OsStrExt;
    use umbra_core::{
        BytePath, CustodyPhase, JournalAccess, JournalControlBinding, JournalFormatPolicy,
        JournalOpenRequest, PhysicalPath,
    };
    use umbra_journal::Journal;
    let control = store.join(run_id.0.to_string()).join("control");
    let mut journal = umbra_journal_file::FileJournal::new();
    let replay = journal
        .open(&JournalOpenRequest {
            control: JournalControlBinding {
                run_id,
                directory: PhysicalPath(
                    BytePath::new(control.as_os_str().as_bytes().to_vec()).unwrap(),
                ),
            },
            access: JournalAccess::ReadOnly,
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        })
        .unwrap();
    let custody = replay
        .custody
        .expect("managed run lost durable Armed custody");
    assert_eq!(custody.record.run_id, run_id);
    assert_eq!(
        matches!(custody.record.state, CustodyPhase::Complete { .. }),
        complete,
        "whole-run outcome disagrees with durable custody: {:?}",
        custody.record.state
    );
    if !complete {
        assert!(custody.recovery_only(), "failed run became writable");
    }
    journal.close().unwrap();
}

#[cfg(target_os = "macos")]
fn stop_owned_provider(caller: u32) {
    use std::collections::BTreeMap;
    use umbra_core::managed::Role;
    use umbra_isolation::managed::process::{identity_with_parent, signal_current};
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let processes: BTreeMap<u32, (u32, String)> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse().ok()?,
                (
                    fields.next()?.parse().ok()?,
                    fields.collect::<Vec<_>>().join(" "),
                ),
            ))
        })
        .collect();
    let candidates: Vec<_> = processes
        .iter()
        .filter(|(_, (_, command))| command.ends_with("/umbra-platform-macos"))
        .filter(|(pid, _)| {
            let mut next = **pid;
            for _ in 0..8 {
                let Some((parent, _)) = processes.get(&next) else {
                    return false;
                };
                if *parent == caller {
                    return true;
                }
                next = *parent;
            }
            false
        })
        .collect();
    assert_eq!(
        candidates.len(),
        1,
        "expected one provider in this caller's tree"
    );
    let (&pid, (parent, _)) = candidates[0];
    let (provider, actual_parent) = identity_with_parent(pid, Role::Provider).unwrap();
    assert_eq!(actual_parent, *parent);
    // The signal validates the captured birth and kernel audit identity again.
    signal_current(provider, libc::SIGKILL).unwrap();
}
