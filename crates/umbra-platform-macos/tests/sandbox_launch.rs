#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! Qualification for the installed-sandbox launch boundary.
//!
//! Two things must both hold for `sandboxed-stopped-launch-v1` to be honest:
//! `launch` returns only once the target is stopped past the installer's exec,
//! and the policy is genuinely in force at that point. The second is proven the
//! only way it can be — by *not* rewriting a write and observing the kernel deny
//! it. A launch that merely fails would prove nothing, so the same fixture and
//! path are exercised in both directions.

use std::{
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use umbra_core::*;
use umbra_platform::{SyscallAbi, TraceBackend, TraceControl, TraceMemory};
use umbra_platform_macos::{DarwinArm64Abi, MacosTraceBackend, Options};

fn byte_path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}

struct Memory<'a>(&'a mut MacosTraceBackend, TaskId);
impl TraceMemory for Memory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        self.0.read_memory(self.1, address, out)
    }
}

/// The same policy shape the supervisor renders: read the host, write only under
/// one run root. Built here because a backend may not depend on the supervisor.
fn profile(write_root: &Path) -> SandboxProfile {
    let source = format!(
        "(version 1)\n(deny default)\n(allow file-read*)\n\
         (allow file-write* (subpath \"{}\"))\n\
         (allow file-write-data (literal \"/dev/null\"))\n\
         (allow process-fork)\n(allow process-exec)\n\
         (allow signal (target self) (target children))\n(allow sysctl-read)\n\
         (allow mach-priv-task-port (target same-sandbox))\n(allow network*)\n\
         (allow mach-lookup (global-name \"com.apple.system.logger\") \
         (global-name \"com.apple.system.notification_center\") \
         (global-name \"com.apple.bsd.dirhelper\") (global-name \"com.apple.lsd.mapdb\") \
         (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.trustd.agent\") \
         (global-name \"com.apple.mDNSResponder\"))\n",
        write_root.display()
    );
    SandboxProfile::new(
        SEATBELT_PROFILE_FORMAT,
        source.into_bytes(),
        byte_path(write_root),
    )
    .unwrap()
}

struct Outcome {
    first_event_after_launch: TraceEvent,
    rewrites: usize,
    root_status: Option<ExitStatus>,
    host_exists: bool,
    shadow: Option<Vec<u8>>,
}

fn drive(case: &str, fixture: &Path, root: &Path, rewrite: bool) -> Outcome {
    let host_dir = std::env::temp_dir().join(format!(
        "umbra-sandbox-{}-{case}-{}",
        std::process::id(),
        u32::from(rewrite)
    ));
    let _ = std::fs::remove_dir_all(&host_dir);
    std::fs::create_dir_all(&host_dir).unwrap();
    let host = host_dir.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    let _ = std::fs::remove_file(&shadow);

    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 40_000,
        ..Options::default()
    });
    let process = tracer
        .launch(LaunchSpec {
            executable: byte_path(fixture),
            argv: vec![
                fixture.as_os_str().as_bytes().to_vec(),
                case.as_bytes().to_vec(),
                host.as_os_str().as_bytes().to_vec(),
            ],
            environment: vec![],
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::NfsClientFsync,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            },
            sandbox: SandboxRequirement::Required(profile(root)),
        })
        .expect("sandboxed launch reaches the target exec boundary");

    let mut live = 1;
    let mut rewrites = 0;
    let mut root_status = None;
    let mut first = None;
    while live > 0 {
        let event = tracer.next_event().expect("event");
        if first.is_none() {
            first = Some(event.clone());
        }
        let resume = match event {
            TraceEvent::ThreadStarted { thread, .. } => Some(thread),
            TraceEvent::Child { .. } => {
                live += 1;
                None
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let decoded = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .expect("decode");
                if let Some(FsOp::Open {
                    ref path, flags, ..
                }) = decoded
                {
                    let write_intent =
                        flags.write || flags.append || flags.create || flags.truncate;
                    if write_intent && rewrite {
                        let physical = root.join(
                            Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
                                .strip_prefix("/")
                                .unwrap(),
                        );
                        std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
                        let prepared = FsOp::Open {
                            dir: DirRef::Cwd,
                            path: path.clone(),
                            flags,
                            mode: 0o644,
                        };
                        let plan = tracer
                            .prepare_rewrite(thread, &byte_path(&physical), prepared)
                            .expect("prepare rewrite");
                        for write in &plan.memory_writes {
                            tracer
                                .write_memory(task, write.address, &write.bytes)
                                .unwrap();
                        }
                        DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                        tracer.set_registers(thread, &registers).unwrap();
                        rewrites += 1;
                    }
                }
                Some(thread)
            }
            TraceEvent::SyscallExit { thread, .. } => Some(thread),
            TraceEvent::Exit { task, status } => {
                if task == process.0 {
                    root_status = Some(status);
                }
                live -= 1;
                None
            }
            TraceEvent::ThreadExited { .. } | TraceEvent::Signal { .. } => None,
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .expect("resume");
        }
    }
    let _ = tracer.terminate(process, TerminationPolicy::Immediate);

    let outcome = Outcome {
        first_event_after_launch: first.expect("at least one event"),
        rewrites,
        root_status,
        host_exists: host.exists(),
        shadow: std::fs::read(&shadow).ok(),
    };
    let _ = std::fs::remove_file(&shadow);
    let _ = std::fs::remove_dir_all(&host_dir);
    outcome
}

fn inputs() -> Option<(PathBuf, PathBuf)> {
    let (Some(fixture), Some(root)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        eprintln!("SKIP: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return None;
    };
    Some((PathBuf::from(fixture), PathBuf::from(root)))
}

#[test]
fn launch_returns_with_the_target_stopped_past_the_installer_exec() {
    let Some((fixture, root)) = inputs() else {
        return;
    };
    let outcome = drive("open-libc", &fixture, &root, true);
    // The installer's own startup was consumed by the backend: the first event
    // the caller sees is the stopped target, not the bootstrap's syscalls.
    assert!(
        matches!(
            outcome.first_event_after_launch,
            TraceEvent::ThreadStarted { .. }
        ),
        "expected a stopped target, got {:?}",
        outcome.first_event_after_launch
    );
    assert_eq!(outcome.rewrites, 1);
    assert_eq!(outcome.root_status, Some(ExitStatus::Code(0)));
    assert!(
        !outcome.host_exists,
        "the host destination must stay absent"
    );
    assert_eq!(outcome.shadow.as_deref(), Some(b"libc\n".as_slice()));
}

#[test]
fn an_unrewritten_write_outside_the_run_root_is_denied_by_the_installed_policy() {
    let Some((fixture, root)) = inputs() else {
        return;
    };
    // Identical fixture and destination, with interception deliberately doing
    // nothing. Only the installed policy can stop this write.
    let outcome = drive("open-libc", &fixture, &root, false);
    assert_eq!(outcome.rewrites, 0);
    assert_eq!(
        outcome.root_status,
        Some(ExitStatus::Code(1)),
        "the fixture must fail because the kernel refused its open"
    );
    assert!(
        !outcome.host_exists,
        "the host destination must stay absent"
    );
    assert_eq!(outcome.shadow, None);
}
