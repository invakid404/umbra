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
        provider::{encode, ProviderDescriptor, ProviderRegistry, PROTOCOL_VERSION},
        BytePath, RunId,
    };
    use umbra_supervisor::{
        managed_entry::{execute, RegisteredRole},
        run::{CommandLaunch, RunLaunch, RunObserver, RunOutcome, RunPersistence, RunSpec},
    };
    if let Some(mut role) = RegisteredRole::enter().unwrap() {
        let result = execute(&mut role);
        #[cfg(feature = "test-managed-construction-failure")]
        check_construction_collection(&result);
        if let Err(error) = result {
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
    assert!([
        "success",
        "owner_construction",
        "missing_storage",
        "relay_construction",
        "slow_preparation",
        "failed_preparation",
        "finish_reply_loss",
        "missing",
        "prepared_error",
        "prepared_panic",
        "provider_death",
        "r2_failed_closure_reopen",
        "r2_native_preaccepted_work_cancel",
        "owner_death",
        "armed_reply_loss",
        "startup_abort",
        "missing_provider",
        "readiness_timeout",
        "malformed_offer",
        "r2_direct_drop_failed_result"
    ]
    .contains(&mode.as_str()));
    #[cfg(not(feature = "probe-native-setup-cancel"))]
    if mode == "r2_native_preaccepted_work_cancel" {
        panic!("native setup cancellation requires its compile-time probe");
    }
    if mode == "r2_native_preaccepted_work_cancel" {
        let directory = std::path::PathBuf::from(
            std::env::var_os("UMBRA_NATIVE_SETUP_PROBE").expect("missing native probe directory"),
        );
        std::fs::write(directory.join("caller.pid"), std::process::id().to_string()).unwrap();
    }
    #[cfg(not(feature = "probe-managed-arm-reply-loss"))]
    if mode == "armed_reply_loss" {
        panic!("armed_reply_loss requires its compile-time fault");
    }
    #[cfg(not(feature = "probe-managed-offer-generation"))]
    if mode == "malformed_offer" {
        panic!("malformed_offer requires its compile-time fault");
    }
    #[cfg(not(feature = "probe-managed-finish-reply-loss"))]
    if mode == "finish_reply_loss" {
        panic!("finish_reply_loss requires its compile-time fault");
    }
    #[cfg(not(feature = "probe-managed-owner-drop"))]
    if mode == "r2_direct_drop_failed_result" {
        panic!("r2_direct_drop_failed_result requires its compile-time probe");
    }
    #[cfg(not(feature = "test-slow-namespace-preparation"))]
    if mode == "slow_preparation" {
        panic!("slow preparation requires its compile-time test hook");
    }
    #[cfg(not(feature = "test-failed-namespace-preparation"))]
    if mode == "failed_preparation" {
        panic!("failed preparation requires its compile-time test hook");
    }
    let expects_success =
        ["success", "finish_reply_loss", "slow_preparation"].contains(&mode.as_str());
    let capacity: u64 = std::env::args()
        .nth(2)
        .map(|value| value.parse().unwrap())
        .unwrap_or(64);
    let temp = tempfile::tempdir().unwrap();
    let construction_record = temp.path().join("construction");
    if ["owner_construction", "relay_construction"].contains(&mode.as_str()) {
        #[cfg(not(feature = "test-managed-construction-failure"))]
        panic!("construction cases require test-managed-construction-failure");
        // This test entry is single-threaded and helpers have not been spawned.
        #[cfg(feature = "test-managed-construction-failure")]
        unsafe {
            std::env::set_var(
                "UMBRA_TEST_CONSTRUCTION_FAILURE",
                mode.split('_').next().unwrap(),
            );
            std::env::set_var("UMBRA_TEST_CONSTRUCTION_RECORD", &construction_record);
        }
    }
    let drop_returned = temp.path().join("owner-drop-returned");
    if mode == "r2_direct_drop_failed_result" {
        // SAFETY: this harness-free caller has not started threads or children.
        unsafe { std::env::set_var("UMBRA_FIX299_OWNER_DROP_RETURNED", &drop_returned) };
    }
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
    let routed_custody = std::env::var_os("UMBRA_CUSTODY_STORAGE_DESCRIPTOR");
    if let Some(path) = &routed_custody {
        assert_eq!(mode, "r2_failed_closure_reopen");
        let storage: ProviderDescriptor =
            umbra_core::provider::decode(&std::fs::read(path).unwrap()).unwrap();
        assert!(storage
            .capabilities
            .contains("experimental-userspace-routing-v1"));
        providers.insert("storage".into(), storage);
        providers
            .get_mut("platform")
            .unwrap()
            .capabilities
            .insert("experimental-userspace-interpose-v1".into());
    }
    if std::env::var_os("UMBRA_CUSTODY_REGISTRY_REQUIRED").is_some() {
        assert!(
            routed_custody.is_some(),
            "routed custody descriptor required"
        );
    }
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
    if mode == "readiness_timeout" {
        use std::os::unix::fs::PermissionsExt;
        let silent = workspace.join("silent-provider");
        // exec leaves exactly one provider process for its actual owner to wait.
        std::fs::write(&silent, b"#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o700)).unwrap();
        providers.get_mut("platform").unwrap().executable =
            BytePath::new(silent.as_os_str().as_bytes().to_vec()).unwrap();
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
            if matches!(
                self.mode.as_str(),
                "provider_death" | "owner_death" | "r2_failed_closure_reopen"
            ) {
                stop_owned_role(self.caller, self.mode == "owner_death");
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
    let mut spec = RunSpec {
        registry: ProviderRegistry {
            // Routed storage advertises a 30 s renewal interval; each RPC
            // must leave the same renewal headroom as the routed CLI fixture.
            timeout_ms: if routed_custody.is_some() {
                12_000
            } else {
                25_000
            },
            providers,
        },
        launch: RunLaunch::Command(CommandLaunch {
            executable: BytePath::new(executable.clone()).unwrap(),
            argv: vec![executable],
            environment: vec![],
            cwd: BytePath::new(b"/".to_vec()).unwrap(),
        }),
        workspace,
        persistence: if routed_custody.is_some() {
            RunPersistence::NfsClientFsync
        } else {
            RunPersistence::LocalDevelopment
        },
        experimental: true,
        state_root: temp.path().join("state"),
        observer: Some(Box::new(Observer {
            caller: std::process::id(),
            events: events.clone(),
            run: observed_run.clone(),
            mode: mode.clone(),
        })),
    };
    let base_error = if mode == "missing_storage" {
        spec.registry
            .providers
            .get_mut("storage")
            .unwrap()
            .executable = BytePath::new(
            temp.path()
                .join("missing-storage")
                .as_os_str()
                .as_bytes()
                .to_vec(),
        )
        .unwrap();
        Some(
            umbra_supervisor::run::run(RunSpec {
                registry: spec.registry.clone(),
                launch: spec.launch.clone(),
                workspace: spec.workspace.clone(),
                persistence: spec.persistence,
                experimental: spec.experimental,
                state_root: spec.state_root.clone(),
                observer: None,
            })
            .expect_err("unarmed storage setup must fail"),
        )
    } else {
        None
    };
    let began = std::time::Instant::now();
    umbra_core::cost_snapshot!("library-before-prepare", None, &[]);
    let mut run = RegisteredRole::prepare(spec, true, capacity).unwrap();
    let result = if mode == "startup_abort" {
        run.cleanup_failure()
    } else {
        execute(&mut run)
    };
    let elapsed = began.elapsed();
    let bound = if expects_success {
        26.35 + 4.0 * capacity as f64
    } else {
        30.6 + 8.0 * capacity as f64
    };
    assert!(
        elapsed.as_secs_f64() <= bound,
        "managed run exceeded D4: {elapsed:?} > {bound}s"
    );
    if let Some(expected) = base_error {
        let error = result.expect_err("missing storage must fail");
        let actual = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<umbra_core::UmbraError>())
            .expect("managed path must return the original structured storage error");
        assert_eq!(actual.kind, expected.kind);
        assert_eq!(actual.operation, expected.operation);
        assert_eq!(actual.context, expected.context);
        assert!(events.borrow().is_empty());
        return;
    }
    if ["owner_construction", "relay_construction"].contains(&mode.as_str()) {
        assert!(result.is_err(), "construction failure became success");
        let checked = std::fs::read_to_string(construction_record.with_extension("checked"))
            .expect("construction hook must execute and its real parent must verify collection");
        assert!(checked.contains("test hook:"));
        assert!(checked.contains("construction failed"));
        assert!(events.borrow().is_empty());
        return;
    }
    if mode == "armed_reply_loss" || mode == "r2_direct_drop_failed_result" {
        assert!(
            result.is_err(),
            "interrupted custody authorized a successful run"
        );
        assert!(
            events.borrow().is_empty(),
            "callback after interrupted owner initialization"
        );
        if mode == "r2_direct_drop_failed_result" {
            r2_direct_drop_failed_result(&drop_returned);
        }
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
        inspect_custody(&store, run_id, false, false);
    } else if [
        "startup_abort",
        "missing_provider",
        "readiness_timeout",
        "malformed_offer",
        "failed_preparation",
    ]
    .contains(&mode.as_str())
    {
        if mode == "startup_abort" {
            result.unwrap();
        } else {
            assert!(result.is_err(), "invalid provider startup succeeded");
        }
        if mode == "failed_preparation" {
            assert!(elapsed >= std::time::Duration::from_secs(12));
        }
        if mode == "readiness_timeout" {
            assert!(
                elapsed >= std::time::Duration::from_secs(2),
                "silent provider failed before the shared readiness cutoff"
            );
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
    } else if expects_success {
        result.unwrap();
        if mode == "slow_preparation" {
            assert!(elapsed >= std::time::Duration::from_millis(2300));
        }
        assert_eq!(*events.borrow(), ["stdio", "prepared", "finished"]);
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match mode.as_str() {
            "missing" => Some("twin source"),
            "prepared_error" => Some("managed library prepared sentinel"),
            "prepared_panic" => Some("managed preparation observer panicked"),
            "provider_death"
            | "owner_death"
            | "r2_failed_closure_reopen"
            | "r2_native_preaccepted_work_cancel" => None,
            _ => unreachable!(),
        };
        if let Some(expected) = expected {
            assert!(
                error.contains(expected),
                "original failure missing: {error}"
            );
        } else {
            assert_eq!(*events.borrow(), ["stdio", "prepared"]);
            eprintln!("{mode} retained failure: {error}");
        }
        assert!(!events.borrow().contains(&"finished"));
    }
    if mode == "r2_failed_closure_reopen" {
        r2_failed_closure_reopen(
            &if routed_custody.is_some() {
                temp.path().join("state")
            } else {
                store.clone()
            },
            observed_run
                .get()
                .expect("failed closure never reached prepared"),
            routed_custody.is_some(),
        );
    } else if let Some(run_id) = observed_run.get() {
        inspect_custody(&store, run_id, expects_success, false);
    }
    drop(run);
    umbra_core::cost_snapshot!("library-after-return", None, &[]);
    println!("managed_library: {mode} capacity={capacity} elapsed={elapsed:?} passed");
}

#[cfg(all(target_os = "macos", feature = "test-managed-construction-failure"))]
fn check_construction_collection(result: &std::io::Result<()>) {
    let Some(path) = std::env::var_os("UMBRA_TEST_CONSTRUCTION_RECORD") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let Ok(record) = std::fs::read_to_string(&path) else {
        return;
    };
    let ids: Vec<u32> = record
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    if ids[0] != std::process::id() {
        return;
    }
    let error = result
        .as_ref()
        .expect_err("construction hook must stay failed");
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let observed = unsafe {
        libc::waitid(
            libc::P_PID,
            ids[1],
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    assert!(
        observed == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        "registered parent must have collected the construction-failure child"
    );
    std::fs::write(path.with_extension("checked"), error.to_string()).unwrap();
}

#[cfg(target_os = "macos")]
fn r2_failed_closure_reopen(store: &std::path::Path, run_id: umbra_core::RunId, routed: bool) {
    inspect_custody(store, run_id, false, routed);
}

#[cfg(target_os = "macos")]
fn r2_direct_drop_failed_result(marker: &std::path::Path) {
    assert!(
        marker.with_extension("entered").exists(),
        "direct-drop probe never reached an enrolled owner after GO"
    );
    assert!(
        !marker.exists(),
        "direct RunOwner drop returned instead of ending the failed worker"
    );
}

#[cfg(target_os = "macos")]
fn inspect_custody(
    store: &std::path::Path,
    run_id: umbra_core::RunId,
    complete: bool,
    routed: bool,
) {
    use std::os::unix::ffi::OsStrExt;
    use umbra_core::{
        BytePath, CustodyPhase, JournalAccess, JournalControlBinding, JournalFormatPolicy,
        JournalOpenRequest, PhysicalPath,
    };
    use umbra_journal::Journal;
    let control = store
        .join(run_id.0.to_string())
        .join(if routed { "journal" } else { "control" });
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
fn stop_owned_role(caller: u32, owner: bool) {
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
    let target = if owner {
        let (owner, guardian) = identity_with_parent(actual_parent, Role::CleanupOwner).unwrap();
        assert_eq!(
            Some(guardian),
            processes.get(&actual_parent).map(|(parent, _)| *parent)
        );
        owner
    } else {
        provider
    };
    // The signal validates the captured birth and kernel audit identity again.
    signal_current(target, libc::SIGKILL).unwrap();
}
