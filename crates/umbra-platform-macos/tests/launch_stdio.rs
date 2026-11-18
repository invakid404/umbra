#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::os::unix::net::UnixStream;
use umbra_core::{launch_stdio::LaunchStdio, *};
use umbra_platform::{
    PlatformSession, TraceBackend,
    provider::{Request, Response, serve_provider_on},
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
