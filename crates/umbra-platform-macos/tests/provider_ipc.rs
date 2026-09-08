#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::{
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};
use umbra_core::{
    provider::{self as wire, Client, Connection, Hello, Welcome},
    *,
};
use umbra_platform::{
    provider::{serve_provider_on, Request, Response},
    PlatformSession, SyscallAbi, TraceMemory,
};
use umbra_platform_macos::{abi, DarwinArm64Abi, MacosTraceBackend, Options};

fn byte_path(path: &Path) -> BytePath {
    BytePath::new(path.as_os_str().as_bytes().to_vec()).unwrap()
}
fn call(client: &mut Client, request: Request) -> Response {
    client.call(&request).unwrap()
}
fn unit(client: &mut Client, request: Request) {
    assert!(matches!(call(client, request), Response::Unit));
}
struct Memory<'a> {
    client: &'a mut Client,
    task: TaskId,
    reads: &'a mut usize,
}
impl TraceMemory for Memory<'_> {
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<()> {
        match self.client.call(&Request::ReadMemory {
            task: self.task,
            address,
            len: out.len() as u32,
        })? {
            Response::Bytes(bytes) if bytes.len() == out.len() => {
                out.copy_from_slice(&bytes);
                *self.reads += 1;
                Ok(())
            }
            _ => Err(wire::protocol_error("read_memory response")),
        }
    }
}

#[test]
fn open_libc_provider_ipc() {
    let (Some(fixture), Some(root)) = (
        std::env::var_os("UMBRA_TEST_FIXTURE_PATH"),
        std::env::var_os("UMBRA_TEST_REDIRECT_ROOT"),
    ) else {
        eprintln!(
            "SKIP open-libc provider IPC: set UMBRA_TEST_FIXTURE_PATH and UMBRA_TEST_REDIRECT_ROOT"
        );
        return;
    };
    let fixture = PathBuf::from(fixture);
    let root = PathBuf::from(root);
    assert!(root.is_absolute());
    let host_dir = std::env::temp_dir().join(format!("umbra-ipc-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&host_dir).unwrap();
    let host = host_dir.join("output");
    let shadow = root.join(host.strip_prefix("/").unwrap());
    assert!(!host.exists(), "fixture host output already exists");
    assert!(!shadow.exists(), "fixture shadow output already exists");

    let (left, right) = UnixStream::pair().unwrap();
    let timeout = Duration::from_secs(30);
    let worker = std::thread::spawn(move || {
        // Same handshake and dispatch as serve_provider("macos", ...), with
        // the private connection injected instead of read from process args.
        serve_provider_on(Connection::new(right, timeout), "macos", |options| {
            let options: Options =
                serde_json::from_slice(options).map_err(|e| wire::protocol_error(e.to_string()))?;
            Ok(PlatformSession {
                control: Box::new(MacosTraceBackend::new(options)),
                abi: Box::new(DarwinArm64Abi),
            })
        })
    });
    let mut connection = Connection::new(left, timeout);
    connection
        .send(&Hello {
            id: "macos".into(),
            role: "platform".into(),
            version: wire::PROTOCOL_VERSION,
            required_capabilities: ["darwin-arm64-abi-v1".into()].into_iter().collect(),
            options: serde_json::to_vec(&Options {
                timeout_ms: 25_000,
                ..Options::default()
            })
            .unwrap(),
        })
        .unwrap();
    let welcome = connection.receive::<Result<Welcome>>().unwrap().unwrap();
    assert_eq!(welcome.id, "macos");
    assert_eq!(welcome.role, "platform");
    assert_eq!(welcome.version, wire::PROTOCOL_VERSION);
    assert!(welcome.capabilities.contains("darwin-arm64-abi-v1"));
    let mut client = Client::from_connection(connection, welcome);
    let Response::Capabilities(capabilities) = call(&mut client, Request::Capabilities) else {
        panic!("capabilities response");
    };
    assert_eq!(capabilities.architectures, vec![Architecture::Aarch64]);
    assert_eq!(capabilities.capabilities, client.welcome.capabilities);
    eprintln!("IPC Capabilities: {capabilities:?}");
    let spec = LaunchSpec {
        executable: byte_path(&fixture),
        argv: vec![
            fixture.as_os_str().as_bytes().to_vec(),
            b"open-libc".to_vec(),
            host.as_os_str().as_bytes().to_vec(),
        ],
        environment: vec![],
        cwd: byte_path(&std::env::current_dir().unwrap()),
        policy: LaunchPolicy {
            persistence: PersistencePolicy::LocalDevelopment,
            inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
        },
        // Protocol coverage for the unenforced experiment path. A required
        // profile is rejected below, because installation is not implemented.
        sandbox: SandboxRequirement::UnsandboxedExperiment,
    };
    for invalid in 0..5 {
        let mut rejected = spec.clone();
        match invalid {
            0 => rejected.policy.persistence = PersistencePolicy::StrictRemote,
            1 => rejected.policy.inherited_fds.push(TracedFd(3)),
            2 => rejected.executable = BytePath::new(b"relative".to_vec()).unwrap(),
            3 => rejected.cwd = BytePath::new(b"relative".to_vec()).unwrap(),
            _ => rejected.argv.clear(),
        }
        let result: Result<Response> = client.call(&Request::Launch(rejected));
        assert!(result.is_err(), "invalid launch policy {invalid} accepted");
    }
    // A profile whose write root is the filesystem root is refused before any
    // process is created: enforcement that grants everything is not enforcement.
    let mut overbroad = spec.clone();
    overbroad.sandbox = SandboxRequirement::Required(
        SandboxProfile::new(
            SEATBELT_PROFILE_FORMAT,
            b"(version 1)\n(deny default)\n".to_vec(),
            BytePath::new(b"/".to_vec()).unwrap(),
        )
        .unwrap(),
    );
    let refused: Result<Response> = client.call(&Request::Launch(overbroad));
    let Err(refused) = refused else {
        panic!("a sandbox profile granting the filesystem root must not launch");
    };
    assert_eq!(refused.kind, ErrorKind::InvalidPath);
    // Both qualified capabilities are advertised over the handshake.
    assert!(client
        .welcome
        .capabilities
        .contains(umbra_core::capabilities::PLATFORM_SANDBOXED_LAUNCH_V1));
    assert!(client
        .welcome
        .capabilities
        .contains(umbra_core::capabilities::PLATFORM_SYSCALL_REWRITE_V1));
    let Response::Process(process) = call(&mut client, Request::Launch(spec.clone())) else {
        panic!("launch response");
    };
    eprintln!("IPC Launch: {process:?}");
    let stale = ProcessHandle(TaskId(TaskIdentity {
        native_id: process.0 .0.native_id,
        generation: process.0 .0.generation + 1,
    }));
    let stale_result: Result<Response> = client.call(&Request::Quiesce(stale));
    assert_eq!(stale_result.err().unwrap().kind, ErrorKind::StaleHandle);
    for _ in 0..2 {
        let Response::Quiesced(tree) = call(&mut client, Request::Quiesce(process)) else {
            panic!("quiesce response");
        };
        assert_eq!(
            tree,
            QuiescedTree {
                process,
                tasks: vec![process.0]
            }
        );
    }
    let mut events = 0;
    let mut reads = 0;
    let mut resumes = 0;
    let mut opens = 0;
    let mut saved_scratch = None;
    let mut interrupted = false;
    loop {
        let Response::Event(event) = call(&mut client, Request::NextEvent) else {
            panic!("event response");
        };
        events += 1;
        let thread = match event {
            TraceEvent::ThreadStarted { thread, .. } => thread,
            TraceEvent::SyscallEntry {
                task,
                thread,
                mut registers,
            } => {
                let Response::Quiesced(tree) = call(&mut client, Request::Quiesce(process)) else {
                    panic!("quiesce at syscall entry");
                };
                assert_eq!(tree.tasks, vec![task]);
                let op = DarwinArm64Abi
                    .decode_entry(
                        &registers,
                        &mut Memory {
                            client: &mut client,
                            task,
                            reads: &mut reads,
                        },
                    )
                    .unwrap()
                    .unwrap();
                let FsOp::Open {
                    ref path, flags, ..
                } = op
                else {
                    panic!("unexpected operation");
                };
                assert!(path.is_absolute(), "relative paths are not qualified");
                if flags.write || flags.append || flags.create || flags.truncate {
                    let physical = root.join(
                        Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
                            .strip_prefix("/")
                            .unwrap(),
                    );
                    std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
                    // The IPC protocol has no allocator. Borrow stopped-thread stack
                    // memory below Darwin's 128-byte red zone only for this syscall,
                    // then restore it at SyscallExit before any user code executes.
                    let physical = byte_path(&physical);
                    let len = physical.as_bytes().len() + 1;
                    let address = (abi::get(&registers, 31)
                        .unwrap()
                        .checked_sub(128 + len as u64)
                        .unwrap())
                        & !15;
                    let mut saved = vec![0; len];
                    Memory {
                        client: &mut client,
                        task,
                        reads: &mut reads,
                    }
                    .read(address, &mut saved)
                    .unwrap();
                    let plan = abi::prepare_path(&registers, address, &physical, op).unwrap();
                    for write in &plan.memory_writes {
                        unit(
                            &mut client,
                            Request::WriteMemory {
                                task,
                                address: write.address,
                                bytes: write.bytes.clone(),
                            },
                        );
                    }
                    DarwinArm64Abi.apply_rewrite(&mut registers, &plan).unwrap();
                    unit(
                        &mut client,
                        Request::SetRegisters {
                            thread,
                            regs: registers,
                        },
                    );
                    assert!(saved_scratch.replace((task, address, saved)).is_none());
                    opens += 1;
                }
                thread
            }
            TraceEvent::SyscallExit { thread, .. } => {
                if let Some((task, address, bytes)) = saved_scratch.take() {
                    unit(
                        &mut client,
                        Request::WriteMemory {
                            task,
                            address,
                            bytes,
                        },
                    );
                }
                thread
            }
            TraceEvent::Exit { status, .. } => {
                assert_eq!(status, ExitStatus::Code(0));
                break;
            }
            other => panic!("unexpected event {other:?}"),
        };
        unit(
            &mut client,
            Request::Resume(ResumeCommand {
                thread,
                mode: ResumeMode::Syscall,
                signal: None,
            }),
        );
        resumes += 1;
        if saved_scratch.is_some() {
            // A rewritten open is running toward its return gate. Even if its
            // trap has arrived, it is not a completed transaction until decoded.
            let result: Result<Response> = client.call(&Request::Quiesce(process));
            assert_eq!(result.err().unwrap().operation, "quiesce");
        }
        if !interrupted {
            // Exercise a live RSP interruption, including a racing syscall trap.
            let Response::Quiesced(tree) = call(&mut client, Request::Quiesce(process)) else {
                panic!("running quiesce response");
            };
            assert_eq!(tree.tasks, vec![process.0]);
            assert!(matches!(
                call(&mut client, Request::Registers(thread)),
                Response::Registers(_)
            ));
            let premature: Result<Response> = client.call(&Request::Resume(ResumeCommand {
                thread,
                mode: ResumeMode::Syscall,
                signal: None,
            }));
            assert!(premature.is_err(), "a queued stop must not be skipped");
            interrupted = true;
        }
    }
    let Response::Quiesced(tree) = call(&mut client, Request::Quiesce(process)) else {
        panic!("exited quiesce response");
    };
    assert!(tree.tasks.is_empty());
    unit(
        &mut client,
        Request::Terminate {
            process,
            policy: TerminationPolicy::Immediate,
        },
    );
    let relaunch: Result<Response> = client.call(&Request::Launch(spec));
    assert!(
        relaunch.is_err(),
        "backend must remain single-run after termination"
    );
    assert!(opens > 0);
    assert!(reads > 0 && resumes > 0 && events > 0);
    assert!(!host.exists(), "MISSED open-libc: host output exists");
    assert_eq!(
        std::fs::read(&shadow).unwrap(),
        b"libc\n",
        "MISSED open-libc: shadow content"
    );
    eprintln!("IPC NextEvent={events}; ReadMemory={reads} success; WriteMemory/SetRegisters={opens} rewrites; Resume={resumes} success; Quiesce=stopped/running/entry/exited success, in-flight return rejected; Terminate=success");
    eprintln!("CAPTURED open-libc provider IPC");
    drop(client);
    assert!(
        worker.join().unwrap().is_err(),
        "disconnect ends provider loop"
    );
    std::fs::remove_file(shadow).unwrap();
    std::fs::remove_dir(host_dir).unwrap();
}
