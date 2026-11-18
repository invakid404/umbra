#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::os::unix::net::UnixStream;
use umbra_core::{launch_stdio::LaunchStdio, *};
use umbra_platform::{
    provider::{serve_provider_on, Request, Response},
    PlatformSession, TraceBackend,
};
use umbra_platform_macos::{DarwinArm64Abi, MacosTraceBackend, Options};

fn streams() -> LaunchStdio {
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap();
    LaunchStdio::new([
        fd.try_clone().unwrap().into(),
        fd.try_clone().unwrap().into(),
        fd.into(),
    ])
    .unwrap()
}
fn spec() -> LaunchSpec {
    LaunchSpec {
        executable: BytePath::new(b"/usr/bin/true".to_vec()).unwrap(),
        argv: vec![b"true".to_vec()],
        environment: vec![],
        cwd: BytePath::new(b"/".to_vec()).unwrap(),
        policy: LaunchPolicy {
            persistence: PersistencePolicy::LocalDevelopment,
            inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
            interpose: false,
            descriptor_limit: None,
        },
        sandbox: SandboxRequirement::Required(
            SandboxProfile::new(
                SEATBELT_PROFILE_FORMAT,
                b"(version 1)\n(deny default)\n".to_vec(),
                BytePath::new(b"/tmp".to_vec()).unwrap(),
            )
            .unwrap(),
        ),
    }
}
fn rejected(kind: u8) -> LaunchSpec {
    let mut s = spec();
    if kind == 0 {
        s.cwd = BytePath::new(b"relative".to_vec()).unwrap();
    } else {
        s.sandbox = SandboxRequirement::UnsandboxedExperiment;
    }
    s
}
fn consumed(error: UmbraError) {
    assert_eq!(error.kind, ErrorKind::InvalidState);
    assert_eq!(error.operation, "platform.stdio");
    assert_eq!(error.context, "launch stdio already consumed");
}
fn qualified() -> bool {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_some() {
        true
    } else {
        eprintln!("SKIP launch stdio: set UMBRA_INTEGRATION_REQUIRED");
        false
    }
}
#[test]
fn adopted_stdio_rejected_launch_cannot_retry_direct() {
    if !qualified() {
        return;
    }
    for kind in 0..3 {
        let mut backend = MacosTraceBackend::new(Options::default());
        backend.adopt_stdio(streams()).unwrap();
        if kind == 2 {
            assert!(backend.launch_experimental(spec()).is_err());
        } else {
            assert!(backend.launch(rejected(kind)).is_err());
        }
        let result = backend.launch(spec());
        // Drop owns cleanup even if the broken implementation spawned a root.
        drop(backend);
        consumed(result.expect_err("second launch must never spawn"));
    }
}
#[test]
fn adopted_stdio_rejected_launch_cannot_retry_ipc() {
    if !qualified() {
        return;
    }
    use umbra_core::provider::{Client, Connection, Hello, Welcome};
    for kind in 0..2 {
        let (left, right) = UnixStream::pair().unwrap();
        let timeout = std::time::Duration::from_secs(20);
        let worker = std::thread::spawn(move || {
            serve_provider_on(Connection::new(right, timeout), "macos", |_| {
                Ok(PlatformSession {
                    control: Box::new(MacosTraceBackend::default()),
                    abi: Box::new(DarwinArm64Abi),
                })
            })
        });
        let mut connection = Connection::new(left, timeout);
        connection
            .send(&Hello {
                id: "macos".into(),
                role: "platform".into(),
                version: umbra_core::provider::PROTOCOL_VERSION,
                required_capabilities: [umbra_core::launch_stdio::CAPABILITY.into()].into(),
                options: vec![],
            })
            .unwrap();
        let welcome = connection.receive::<Result<Welcome>>().unwrap().unwrap();
        let mut client = Client::from_connection(connection, welcome);
        assert!(matches!(
            client
                .call_with_stdio::<_, Response>(&Request::AdoptStdio, streams())
                .unwrap(),
            Response::Unit
        ));
        let first: Result<Response> = client.call(&Request::Launch(rejected(kind)));
        assert!(first.is_err());
        let second: Result<Response> = client.call(&Request::Launch(spec()));
        drop(client);
        assert!(worker.join().unwrap().is_err());
        match second {
            Err(e) => consumed(e),
            Ok(_) => panic!("second launch must never spawn"),
        }
    }
}

/// The sentinel is an owned provider-side copy, not a caller fd discarded
/// before the provider starts. Use a high slot so startup cannot reuse it.
#[test]
fn provider_owned_stdio_handle_is_absent_from_tracee() {
    if !qualified() {
        return;
    }
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use umbra_platform::TraceControl;
    let fixture = std::env::var("UMBRA_STDIO_FIXTURE").expect("stdio fixture required");
    let file = std::fs::File::open("/dev/null").unwrap();
    let high = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
    assert!(high >= 200);
    let owned = unsafe { OwnedFd::from_raw_fd(high) };
    let stdio = LaunchStdio::new([owned, file.try_clone().unwrap().into(), file.into()]).unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut launch = spec();
    launch.executable = BytePath::new(fixture.as_bytes().to_vec()).unwrap();
    launch.argv = vec![
        fixture.into_bytes(),
        b"sentinel".to_vec(),
        high.to_string().into_bytes(),
    ];
    launch.sandbox = SandboxRequirement::Required(
        SandboxProfile::new(
            SEATBELT_PROFILE_FORMAT,
            include_str!("../../../experiments/seatbelt/umbra.sb")
                .replace(
                    "{{UMBRA_RUN_ROOT}}",
                    &format!("\"{}\"", root.path().display()),
                )
                .into_bytes(),
            BytePath::new(root.path().to_str().unwrap().as_bytes().to_vec()).unwrap(),
        )
        .unwrap(),
    );
    let mut backend = MacosTraceBackend::default();
    backend.adopt_stdio(stdio).unwrap();
    // Deliberately model a provider handle missing F1. This test isolates
    // root-spawn CLOEXEC_DEFAULT (F2), not the per-handle admission fence.
    assert_eq!(unsafe { libc::fcntl(high, libc::F_SETFD, 0) }, 0);
    assert_eq!(unsafe { libc::fcntl(high, libc::F_GETFD) }, 0);
    let process = backend
        .launch(launch)
        .expect("sentinel reaches target exec");
    loop {
        let thread = match backend.next_event().unwrap() {
            TraceEvent::ThreadStarted { thread, .. }
            | TraceEvent::Exec { thread, .. }
            | TraceEvent::SyscallEntry { thread, .. }
            | TraceEvent::SyscallExit { thread, .. } => thread,
            TraceEvent::Exit { status, .. } => {
                assert_eq!(
                    status,
                    ExitStatus::Code(0),
                    "provider fd leaked into tracee"
                );
                break;
            }
            event => panic!("unexpected sentinel event: {event:?}"),
        };
        backend
            .resume(ResumeCommand {
                thread,
                mode: ResumeMode::Syscall,
                signal: None,
            })
            .unwrap();
    }
    backend
        .terminate(process, TerminationPolicy::Immediate)
        .unwrap();
    eprintln!("provider-held fd {high} absent from successfully executed tracee");
}

#[test]
fn rejected_launch_releases_adopted_writers_while_provider_lives() {
    if !qualified() {
        return;
    }
    use std::io::Read;
    for missing_executable in [false, true] {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let stdio = LaunchStdio::new([
            writer.try_clone().unwrap().into(),
            writer.try_clone().unwrap().into(),
            writer.into(),
        ])
        .unwrap();
        let mut backend = MacosTraceBackend::default();
        backend.adopt_stdio(stdio).unwrap();
        let mut launch = rejected(0);
        if missing_executable {
            launch = spec();
            launch.executable =
                BytePath::new(b"/nonexistent/umbra-stdio-fixture".to_vec()).unwrap();
        }
        assert!(backend.launch(launch).is_err());
        assert_eq!(
            reader.read(&mut [0]).unwrap(),
            0,
            "adopted writers survived rejection"
        );
        consumed(backend.launch(spec()).unwrap_err());
    }
}

/// Observe actual helper images after exec but before they can exit. The only
/// additional spawn flag is START_SUSPENDED; deliberately no default-close flag.
#[test]
fn adopted_endpoint_identity_is_absent_from_helper_images() {
    if !qualified() {
        return;
    }
    use std::os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    };
    use std::{ffi::CString, process::Command};
    struct Child(libc::pid_t);
    impl Drop for Child {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                let mut status = 0;
                while libc::waitpid(self.0, &mut status, 0) < 0 {
                    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                        break;
                    }
                }
            }
        }
    }
    fn identities(pid: libc::pid_t) -> Vec<(u64, u64)> {
        let output = Command::new("/usr/sbin/lsof")
            .args(["-nP", "-a", "-p", &pid.to_string(), "-FfDi"])
            .output()
            .unwrap();
        assert!(output.status.success(), "lsof: {:?}", output);
        let text = String::from_utf8(output.stdout).unwrap();
        let mut dev = None;
        let mut numeric = false;
        let mut found = Vec::new();
        for line in text.lines() {
            if let Some(fd) = line.strip_prefix('f') {
                numeric = fd.parse::<u32>().is_ok();
                dev = None;
            } else if let Some(value) = line.strip_prefix('D') {
                dev = Some(u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap());
            } else if let Some(value) = line.strip_prefix('i') {
                if numeric {
                    if let Some(dev) = dev {
                        found.push((dev, value.parse().unwrap()));
                    }
                }
            }
        }
        found
    }
    let file = tempfile::tempfile().unwrap();
    let metadata = file.metadata().unwrap();
    let identity = (metadata.dev(), metadata.ino());
    assert!(
        identities(std::process::id() as i32).contains(&identity),
        "census must recognize fstat identity"
    );
    let stdio = LaunchStdio::new(std::array::from_fn(|_| {
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(fd >= 200);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }))
    .unwrap();
    drop(file);
    let mut backend = MacosTraceBackend::default();
    backend.adopt_stdio(stdio).unwrap();
    let mut leaked = Vec::new();
    // The pre-root helpers that run with adopted copies still alive. Codesign
    // covers twin signing, twin verification and interposer verification.
    for executable in ["/usr/bin/shasum", "/usr/bin/codesign"] {
        let path = CString::new(executable).unwrap();
        let mut pid = 0;
        unsafe {
            let mut attr = std::mem::zeroed();
            assert_eq!(libc::posix_spawnattr_init(&mut attr), 0);
            assert_eq!(
                libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_START_SUSPENDED as i16),
                0
            );
            let args = [path.as_ptr() as *mut libc::c_char, std::ptr::null_mut()];
            let env = [std::ptr::null_mut()];
            let result = libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                std::ptr::null(),
                &attr,
                args.as_ptr(),
                env.as_ptr(),
            );
            libc::posix_spawnattr_destroy(&mut attr);
            assert_eq!(result, 0, "spawn {executable}");
        }
        let child = Child(pid);
        let absent = !identities(pid).contains(&identity);
        eprintln!(
            "helper={executable} fstat_dev={} fstat_ino={} absent={absent}",
            identity.0, identity.1
        );
        if !absent {
            leaked.push(executable);
        }
        drop(child);
    }
    drop(backend);
    assert!(
        leaked.is_empty(),
        "adopted endpoint leaked to helper images: {leaked:?}"
    );
}

#[test]
fn overlapping_standard_sources_are_preserved() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if std::env::var_os("UMBRA_TEST_LOW_STDIO").is_some() {
        // This subprocess exclusively owns its standard slots. The caller's
        // [stdout, stdin, stderr] mapping must survive the destination overlap.
        let adopted = LaunchStdio::new(unsafe {
            [
                OwnedFd::from_raw_fd(1),
                OwnedFd::from_raw_fd(0),
                OwnedFd::from_raw_fd(2),
            ]
        })
        .unwrap();
        if adopted.descriptors().iter().any(|fd| fd.as_raw_fd() <= 2) {
            std::process::exit(91);
        }
        for (fd, bytes) in adopted.descriptors()[..2]
            .iter()
            .zip([b"stdout source", b"stdin source".as_slice()])
        {
            let mut file = std::fs::File::from(fd.try_clone().unwrap());
            file.set_len(0).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(bytes).unwrap();
        }
        std::process::exit(0);
    }
    let mut input = tempfile::tempfile().unwrap();
    let mut output = tempfile::tempfile().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "overlapping_standard_sources_are_preserved",
            "--nocapture",
        ])
        .env("UMBRA_TEST_LOW_STDIO", "1")
        .stdin(input.try_clone().unwrap())
        .stdout(output.try_clone().unwrap())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "overlapping descriptor control: {status}");
    for (file, expected) in [
        (&mut input, b"stdin source".as_slice()),
        (&mut output, b"stdout source".as_slice()),
    ] {
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
    }
}
