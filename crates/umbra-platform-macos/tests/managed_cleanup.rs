#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! Native managed-admission qualification; requires debugger permission.
use std::{os::unix::ffi::OsStrExt, path::Path, sync::atomic::Ordering};
use umbra_core::shutdown::Nonce;
use umbra_core::*;
use umbra_platform::{TraceBackend, TraceControl};
use umbra_platform_macos::{MacosTraceBackend, Options};
#[cfg(feature = "managed-endpoint-probe")]
#[path = "support/endpoint_family.rs"]
mod endpoint_family;
#[path = "support/managed_admission.rs"]
mod managed_admission;

fn path(value: &Path) -> BytePath {
    BytePath::new(value.as_os_str().as_bytes().to_vec()).unwrap()
}
fn qualified() -> bool {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_some() {
        return true;
    }
    eprintln!("SKIP: managed_cleanup requires UMBRA_INTEGRATION_REQUIRED and debugger permission");
    false
}
#[test]
fn r2_native_capacity_refusal_before_preparation() {
    if !qualified() {
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let root = cache.path().canonicalize().unwrap();
    let profile = include_str!("../../../experiments/seatbelt/umbra.sb")
        .replace("{{UMBRA_RUN_ROOT}}", &format!("\"{}\"", root.display()));
    let mut backend = MacosTraceBackend::new(Options {
        twin_cache: Some(cache.path().to_owned()),
        timeout_ms: Some(30_000),
        ..Options::default()
    });
    let (_count, service) = managed_admission::install_with_capacity(&mut backend, 1, true);
    let launch = backend.launch(LaunchSpec {
        executable: path(Path::new("/usr/bin/true")),
        argv: vec![b"/usr/bin/true".to_vec()],
        environment: vec![],
        cwd: path(&std::env::current_dir().unwrap()),
        policy: LaunchPolicy {
            persistence: PersistencePolicy::NfsClientFsync,
            inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            interpose: false,
            descriptor_limit: None,
        },
        sandbox: SandboxRequirement::Required(
            SandboxProfile::new(SEATBELT_PROFILE_FORMAT, profile.into_bytes(), path(&root))
                .unwrap(),
        ),
    });
    let error = launch.unwrap_err();
    assert_eq!(error.operation, "admission");
    assert!(error.to_string().contains("capacity"));
    assert_eq!(
        std::fs::read_dir(cache.path()).unwrap().count(),
        0,
        "capacity refusal must precede cache/signing preparation"
    );
    drop(backend);
    assert_eq!(
        service.join().unwrap(),
        1,
        "preexisting charge must remain retained"
    );
}

#[test]
fn r2_native_admission_root_parent_collection() {
    root_acquisition(64);
}

#[test]
fn r2_native_acquisition_ledger() {
    // Root startup may overlap an exec replacement right with the original
    // program charge. C=1 refusal before preparation is checked separately.
    root_acquisition(64);
}

fn root_acquisition(capacity: u64) {
    root_acquisition_observe(capacity, |_| Ok(()));
}

#[cfg(feature = "managed-endpoint-probe")]
#[test]
fn r2_native_debugserver_endpoint_inventory() {
    use std::{os::fd::AsRawFd, os::unix::net::UnixStream};
    use umbra_platform_macos::endpoint_probe::{inherited_sockets, Probe};
    if !qualified() {
        return;
    }
    let sentinel = UnixStream::pair().unwrap();
    let probe = Probe::new().unwrap();
    root_acquisition_observe(64, move |handle| {
        let report = probe.finish().map_err(|e| e.to_string())?;
        if report.sockets.len() < 2 || report.sockets.iter().any(|s| s.retained_before_exec) {
            return Err(format!("debugger pre-exec endpoint inventory: {report:?}"));
        }
        let parent =
            inherited_sockets(std::process::id(), &report.sockets).map_err(|e| e.to_string())?;
        if parent.len() < 2 {
            return Err("libproc did not identify the live parent admission endpoints".into());
        }
        let debugger = inherited_sockets(report.pid, &report.sockets).map_err(|e| e.to_string())?;
        let root_pid = u32::try_from(handle.0 .0.native_id).map_err(|e| e.to_string())?;
        let root = inherited_sockets(root_pid, &report.sockets).map_err(|e| e.to_string())?;
        if !debugger.is_empty() || !root.is_empty() {
            return Err(format!(
                "inherited endpoints: debugger={debugger:?} root={root:?}"
            ));
        }
        positive_inheritance(sentinel.0.as_raw_fd(), &report.sockets)?;
        drop(sentinel);
        eprintln!("native endpoint identities: {report:?}; parent={parent:?}; debugger/root empty");
        Ok(())
    });
    endpoint_family::run();
}

#[cfg(feature = "managed-endpoint-probe")]
fn positive_inheritance(
    fd: std::os::fd::RawFd,
    expected: &[umbra_platform_macos::endpoint_probe::Socket],
) -> std::result::Result<(), String> {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    let mut command = Command::new("/bin/cat");
    command.stdin(Stdio::piped()).stdout(Stdio::null());
    // Positive inspection control: only this owned sentinel crosses exec.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let found = umbra_platform_macos::endpoint_probe::inherited_sockets(child.id(), expected);
    drop(child.stdin.take());
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() || !found.map_err(|e| e.to_string())?.contains(&fd) {
        return Err("libproc failed the cross-process inherited-socket positive control".into());
    }
    Ok(())
}

fn root_acquisition_observe(
    capacity: u64,
    observe: impl FnOnce(ProcessHandle) -> std::result::Result<(), String>,
) {
    if !qualified() {
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let root = cache.path().canonicalize().unwrap();
    let profile = include_str!("../../../experiments/seatbelt/umbra.sb")
        .replace("{{UMBRA_RUN_ROOT}}", &format!("\"{}\"", root.display()));
    let mut backend = MacosTraceBackend::new(Options {
        twin_cache: Some(cache.path().to_owned()),
        timeout_ms: Some(30_000),
        ..Options::default()
    });
    let (count, service) = managed_admission::install_with_capacity(&mut backend, capacity, false);
    let launch = backend.launch(LaunchSpec {
        executable: path(Path::new("/usr/bin/true")),
        argv: vec![b"/usr/bin/true".to_vec()],
        environment: vec![],
        cwd: path(&std::env::current_dir().unwrap()),
        policy: LaunchPolicy {
            persistence: PersistencePolicy::NfsClientFsync,
            inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            interpose: false,
            descriptor_limit: None,
        },
        sandbox: SandboxRequirement::Required(
            SandboxProfile::new(SEATBELT_PROFILE_FORMAT, profile.into_bytes(), path(&root))
                .unwrap(),
        ),
    });
    let handle = match launch {
        Ok(handle) => handle,
        Err(error) => {
            drop(backend);
            panic!("native launch failed: {error}");
        }
    };
    let observed = observe(handle);
    let live_charge = count.load(Ordering::Acquire);
    let start = backend.begin_shutdown(Nonce::new_v4()).unwrap();
    drop(backend);
    let umbra_platform::shutdown::Start::Local { accepted, ledger } = start else {
        panic!("native local custody required")
    };
    let final_state = ledger.final_snapshot().unwrap();
    let confirmed = accepted.confirms(&final_state).unwrap();
    drop(ledger);
    let remaining = service.join().unwrap();
    // Assertions follow actual native teardown and service termination.
    observed.unwrap();
    assert!(
        live_charge > 0 && live_charge <= capacity,
        "native program and replacement rights must retain bounded charges"
    );
    for kind in [
        umbra_core::shutdown::ResourceKind::Tracee,
        umbra_core::shutdown::ResourceKind::TaskRight,
        umbra_core::shutdown::ResourceKind::Tracer,
    ] {
        assert!(
            final_state.entries.iter().any(|entry| {
                entry.slot.kind == kind && entry.identity.is_some() && entry.confirms()
            }),
            "native acquisition omitted terminal inventory for {kind:?}"
        );
    }
    assert!(confirmed);
    assert_eq!(
        remaining, 0,
        "all native obligations must be collected before capacity release"
    );
}

#[test]
fn r2_native_admission_packed_spawn_collection() {
    if !qualified() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("spawn.c");
    let program = dir.path().join("spawn");
    std::fs::write(&source, "#include <spawn.h>\n#include <sys/wait.h>\nextern char **environ;\nint main(void){posix_spawnattr_t attr; if(posix_spawnattr_init(&attr))return 4; pid_t p; char *a[]={\"true\",0}; int s; if(posix_spawn(&p,\"/usr/bin/true\",0,&attr,a,environ))return 2; if(waitpid(p,&s,0)!=p)return 3; return !WIFEXITED(s)||WEXITSTATUS(s);}").unwrap();
    assert!(std::process::Command::new("cc")
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new(&program)
        .status()
        .unwrap()
        .success());
    let mut backend = MacosTraceBackend::new(Options {
        twin_cache: Some(dir.path().join("twins")),
        timeout_ms: Some(30_000),
        ..Options::default()
    });
    let (_count, service) = managed_admission::install(&mut backend);
    backend
        .launch_experimental(LaunchSpec {
            executable: path(&program),
            argv: vec![program.as_os_str().as_bytes().to_vec()],
            environment: vec![],
            cwd: path(dir.path()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let (mut live, mut prepared, mut children) = (1, 0, 0);
    while live > 0 {
        let event = match backend
            .poll_event(std::time::Duration::from_millis(10))
            .unwrap()
        {
            umbra_platform::EventPoll::Idle => continue,
            umbra_platform::EventPoll::Event(event) => event,
        };
        let resume = match event {
            TraceEvent::SpawnPrepare { token, plan } => {
                prepared += 1;
                Some((
                    token.thread,
                    ResumeMode::SpawnAuthorize(SpawnAuthorization {
                        token,
                        digest: plan.digest,
                        table_generation: 1,
                    }),
                ))
            }
            TraceEvent::SpawnChild { authorization, .. } => {
                children += 1;
                live += 1;
                Some((
                    authorization.token.thread,
                    ResumeMode::SpawnChildReady(authorization),
                ))
            }
            TraceEvent::ThreadStarted { thread, .. }
            | TraceEvent::SyscallEntry { thread, .. }
            | TraceEvent::SyscallExit { thread, .. }
            | TraceEvent::Exec { thread, .. } => Some((thread, ResumeMode::Syscall)),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected packed-spawn event: {other:?}"),
        };
        if let Some((thread, mode)) = resume {
            backend
                .resume(ResumeCommand {
                    thread,
                    mode,
                    signal: None,
                })
                .unwrap();
        }
    }
    assert_eq!((prepared, children), (1, 1));
    let start = backend.begin_shutdown(Nonce::new_v4()).unwrap();
    drop(backend);
    let umbra_platform::shutdown::Start::Local { accepted, ledger } = start else {
        panic!("native custody required")
    };
    assert!(accepted
        .confirms(&ledger.final_snapshot().unwrap())
        .unwrap());
    drop(ledger);
    assert_eq!(service.join().unwrap(), 0);
}
