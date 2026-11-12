#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::{os::unix::ffi::OsStrExt, path::Path, process::Command, time::Duration};
use umbra_core::*;
use umbra_platform::{EventPoll, TraceBackend, TraceControl};
use umbra_platform_macos::{MacosTraceBackend, Options};
fn path(p: &Path) -> BytePath {
    BytePath::new(p.as_os_str().as_bytes().to_vec()).unwrap()
}

#[test]
fn idle_across_fork_park_and_exec() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("tree");
    assert!(Command::new("cc")
        .args(["-O1", "-o"])
        .arg(&program)
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../experiments/fixtures/umbra-fix-262-tree.c")
        )
        .status()
        .unwrap()
        .success());
    assert!(
        Command::new(&program).status().unwrap().success(),
        "native control"
    );
    let mut backend = MacosTraceBackend::new(Options {
        timeout_ms: 15000,
        ..Options::default()
    });
    let root = backend
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
    let (mut live, mut children, mut execs, mut idle_before, mut idle_child, mut idle_exec) =
        (1, 0, 0, 0, 0, 0);
    while live > 0 {
        let event = match backend.poll_event(Duration::from_millis(10)).unwrap() {
            EventPoll::Idle => {
                if execs > 0 {
                    idle_exec += 1;
                } else if children > 0 {
                    idle_child += 1;
                } else {
                    idle_before += 1;
                }
                continue;
            }
            EventPoll::Event(event) => event,
        };
        let resume = match event {
            TraceEvent::ThreadStarted { thread, .. }
            | TraceEvent::SyscallEntry { thread, .. }
            | TraceEvent::SyscallExit { thread, .. } => Some(thread),
            TraceEvent::Child { .. } => {
                live += 1;
                children += 1;
                None
            }
            TraceEvent::Exec { thread, .. } => {
                execs += 1;
                Some(thread)
            }
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected {other:?}"),
        };
        if let Some(thread) = resume {
            backend
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    backend
        .terminate(root, TerminationPolicy::Immediate)
        .unwrap();
    assert_eq!(children, 4);
    assert!(execs >= 1);
    assert!(
        idle_before > 0 && idle_child > 0 && idle_exec > 0,
        "idle counts {idle_before}/{idle_child}/{idle_exec}"
    );
}

#[test]
fn absolute_watchdog_expires_across_idle_replies() {
    let mut backend = MacosTraceBackend::new(Options {
        timeout_ms: 8000,
        ..Options::default()
    });
    let root = backend
        .launch_experimental(LaunchSpec {
            executable: path(Path::new("/bin/sleep")),
            argv: vec![b"sleep".to_vec(), b"20".to_vec()],
            environment: vec![],
            cwd: path(Path::new("/tmp")),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let mut idle = 0;
    let failure = loop {
        match backend.poll_event(Duration::from_millis(50)) {
            Ok(EventPoll::Idle) => idle += 1,
            Ok(EventPoll::Event(
                TraceEvent::ThreadStarted { thread, .. }
                | TraceEvent::SyscallEntry { thread, .. }
                | TraceEvent::SyscallExit { thread, .. }
                | TraceEvent::Exec { thread, .. },
            )) => {
                backend
                    .resume(ResumeCommand {
                        thread,
                        mode: ResumeMode::Syscall,
                        signal: None,
                    })
                    .unwrap();
            }
            Ok(other) => panic!("unexpected {other:?}"),
            Err(error) => break error,
        }
    };
    let _ = backend.terminate(root, TerminationPolicy::Immediate);
    assert!(idle > 0);
    assert!(failure.to_string().contains("watchdog"), "{failure}");
}
