#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::{
    collections::BTreeMap,
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
fn fixture(case: &str, expected: &[u8]) {
    let (Some(fixture), Some(root)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        eprintln!("SKIP {case}: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT");
        return;
    };
    let fixture = PathBuf::from(fixture);
    let root = PathBuf::from(root);
    assert!(root.is_absolute());
    let host_dir =
        std::env::temp_dir().join(format!("umbra-rust-fixture-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&host_dir).unwrap();
    let host = host_dir.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    assert!(!host.exists(), "fixture host output already exists");
    assert!(!shadow.exists(), "fixture shadow output already exists");
    let mut tracer = MacosTraceBackend::new(Options {
        timeout_ms: 25_000,
        ..Options::default()
    });
    let process = tracer
        .launch_experimental(LaunchSpec {
            executable: byte_path(&fixture),
            argv: vec![
                fixture.as_os_str().as_bytes().to_vec(),
                case.as_bytes().to_vec(),
                host.as_os_str().as_bytes().to_vec(),
            ],
            environment: vec![],
            cwd: byte_path(&std::env::current_dir().unwrap()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            },
            // Direct tracer coverage, deliberately without enforcement: these
            // cases measure interception, not the sandbox boundary. `umbra run`
            // cannot select this; it always renders and requires a profile.
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let mut live = 1;
    let mut opens = 0;
    let mut threads = BTreeMap::new();
    while live > 0 {
        let event = tracer.next_event().unwrap();
        eprintln!("{case}: {event:?}");
        let resume = match event {
            TraceEvent::ThreadStarted { task, thread } => {
                threads.insert(task, thread);
                Some(thread)
            }
            TraceEvent::Child { .. } => {
                live += 1;
                None
            }
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let op = DarwinArm64Abi
                    .decode_entry(&registers, &mut Memory(&mut tracer, task))
                    .unwrap()
                    .unwrap();
                let FsOp::Open {
                    ref path, flags, ..
                } = op
                else {
                    panic!("unexpected operation")
                };
                assert!(path.is_absolute(), "relative paths are not qualified");
                // Only rewrite write-intent opens to the shadow root; leave
                // read-only opens (library/dyld loads, runtime resource reads)
                // pointing at the host so the tracee can actually start.
                let write_intent = flags.write || flags.append || flags.create || flags.truncate;
                if write_intent {
                    let physical = root.join(
                        Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
                            .strip_prefix("/")
                            .unwrap(),
                    );
                    std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
                    let plan = tracer
                        .prepare_rewrite(thread, &byte_path(&physical), op)
                        .unwrap();
                    for write in &plan.memory_writes {
                        tracer
                            .write_memory(task, write.address, &write.bytes)
                            .unwrap();
                    }
                    DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                    tracer.set_registers(thread, &registers).unwrap();
                    opens += 1;
                }
                Some(thread)
            }
            TraceEvent::SyscallExit { thread, .. } => {
                // Non-rewritten (read-only) opens legitimately return errors
                // (files missing under this test harness); the target write
                // opens are asserted by the final shadow-content check.
                Some(thread)
            }
            TraceEvent::Exec { thread, .. } => Some(thread),
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                live -= 1;
                None
            }
            other => panic!("unexpected event {other:?}"),
        };
        if let Some(thread) = resume {
            tracer
                .resume(ResumeCommand {
                    thread,
                    mode: ResumeMode::Syscall,
                    signal: None,
                })
                .unwrap();
        }
    }
    tracer
        .terminate(process, TerminationPolicy::Immediate)
        .unwrap();
    assert!(opens > 0);
    assert!(!host.exists(), "MISSED {case}: host output exists");
    assert_eq!(
        std::fs::read(&shadow).unwrap(),
        expected,
        "MISSED {case}: shadow content"
    );
    eprintln!("CAPTURED {case}");
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(host_dir).unwrap();
}
#[test]
fn open_libc() {
    fixture("open-libc", b"libc\n")
}
#[test]
fn open_svc() {
    fixture("open-svc", b"libc\n")
}
#[test]
fn fork_write() {
    fixture("fork-write", b"fork\n")
}
#[test]
fn posix_spawn_write() {
    fixture("posix-spawn-write", b"libc\n")
}
#[test]
fn exec_write() {
    fixture("exec-write", b"libc\n")
}
#[test]
fn grandchild_write() {
    fixture("grandchild-write", b"grandchild\n")
}
#[test]
fn dup_inherit_write() {
    fixture("dup-inherit-write", b"dup\n")
}
