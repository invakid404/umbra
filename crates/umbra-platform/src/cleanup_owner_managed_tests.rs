//! The provider's real parent runs Owner/Service in a separate process.
use super::*;
use std::process::{Child, Command};
use umbra_core::managed::{Participant, Role, RunIdentity};
use umbra_isolation::managed::{
    owner::{Owner, Service},
    process::identity,
    publication::Route,
    worker::{ManagedCleanup as AdapterCleanup, SharedWorkerCleanup, WorkerCleanup},
    Channel,
};

// The platform unit-test crate and isolation's platform dependency have distinct
// trait identities. Forward only that boundary; the production adapter owns all
// ticket publication, failure retention and receipt validation.
#[derive(Clone)]
struct Cleanup(SharedWorkerCleanup);
impl ManagedCleanup for Cleanup {
    fn provider_receipt(&self) -> Option<umbra_core::managed::ProviderReceipt> {
        AdapterCleanup::provider_receipt(&self.0)
    }
    fn start(&mut self) -> umbra_core::Result<Instant> {
        AdapterCleanup::start(&mut self.0)
    }
    fn failed(&mut self, error: &umbra_core::UmbraError) {
        AdapterCleanup::failed(&mut self.0, error);
    }
    fn collect_provider(&mut self) -> umbra_core::Result<Duration> {
        AdapterCleanup::collect_provider(&mut self.0)
    }
    fn abandon(&mut self) {
        AdapterCleanup::abandon(&mut self.0);
    }
}

struct ManagedClient {
    _client: Client,
    cleanup: Cleanup,
}
impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.cleanup.abandon();
    }
}

fn accept(listener: &UnixListener) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "endpoint startup accept");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn spawn(dir: &str, mode: &str, test: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([test, "--nocapture"])
        .env("UMBRA_FIX299_ENDPOINT_DIR", dir)
        .env("UMBRA_FIX299_ENDPOINT_CHILD", mode)
        .spawn()
        .unwrap()
}

#[test]
fn r2_endpoint_owner() {
    let Ok(dir) = std::env::var("UMBRA_FIX299_ENDPOINT_DIR") else {
        return;
    };
    let mode = std::env::var("UMBRA_FIX299_ENDPOINT_CHILD").unwrap();
    let (run, worker): (RunIdentity, Participant) =
        wire::decode(&std::fs::read(format!("{dir}/identity")).unwrap()).unwrap();
    let mut control = UnixStream::connect(format!("{dir}/owner")).unwrap();
    let process = spawn(&dir, &mode, "r2_endpoint_child");
    let provider = identity(process.id(), Role::Provider).unwrap();
    control.write_all(&provider.pid.to_le_bytes()).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let cancellation = loop {
        if let Some(stream) = umbra_core::managed_handoff::receive(&control).unwrap() {
            break stream;
        }
        assert!(Instant::now() < until, "owner RPC descriptor handoff");
        std::thread::sleep(Duration::from_millis(1));
    };
    let owner = Owner::new(
        identity(std::process::id(), Role::CleanupOwner).unwrap(),
        run,
        1,
        process,
        cancellation,
        vec![],
    )
    .unwrap_or_else(|(error, ..)| panic!("{error}"));
    let mut service = Service::new(owner, worker, provider, Channel::new(control).unwrap())
        .unwrap_or_else(|failure| panic!("{}", failure.0));
    while !service.poll().unwrap() {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Service may finish only after its actual Child wait and receipt delivery.
    // Record the actual wait separately from success-only terminal evidence.
    let mut process = service.into_owner().into_child();
    let status = process.try_wait().unwrap().expect("service parent wait");
    use std::os::unix::process::ExitStatusExt;
    std::fs::write(
        format!("{dir}/provider-wait"),
        wire::encode(&(run, provider, status.into_raw())).unwrap(),
    )
    .unwrap();
}

pub(super) fn endpoint(mode: &str) {
    if mode == "control" {
        return super::endpoint(mode);
    }
    let dir = Path::new("/tmp").join(format!("umbra-fix-299b-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_owned();
    let listener = UnixListener::bind(format!("{dir}/s")).unwrap();
    let owner_listener = UnixListener::bind(format!("{dir}/owner")).unwrap();
    let run = RunIdentity {
        nonce: uuid::Uuid::new_v4(),
        generation: 1,
    };
    let worker = identity(std::process::id(), Role::Worker).unwrap();
    std::fs::write(
        format!("{dir}/identity"),
        wire::encode(&(run, worker)).unwrap(),
    )
    .unwrap();
    let mut owner_process = spawn(&dir, mode, "r2_endpoint_owner");
    let owner = identity(owner_process.id(), Role::CleanupOwner).unwrap();
    let mut control = accept(&owner_listener);
    control
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut pid = [0; 4];
    control.read_exact(&mut pid).unwrap();
    let provider = identity(u32::from_le_bytes(pid), Role::Provider).unwrap();
    let mut stream = accept(&listener);
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut ready = [0];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    assert!(umbra_core::managed_handoff::send(&control, &stream).unwrap());
    let mut cleanup = Cleanup(SharedWorkerCleanup::new(
        WorkerCleanup::new(
            run,
            worker,
            owner,
            provider,
            1,
            vec![Route::new(owner, Channel::new(control).unwrap()).unwrap()],
        )
        .unwrap(),
    ));
    let client = Client::from_connection(
        Connection::new(stream, Duration::from_millis(100)),
        wire::Welcome {
            id: "plan-probe".into(),
            role: "platform".into(),
            version: wire::PROTOCOL_VERSION,
            capabilities: Default::default(),
        },
    );
    let release_dir = dir.clone();
    let release_mode = mode.to_owned();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(marker(&release_dir, &release_mode, "release"), b"release").unwrap();
    });
    let mut outcome = String::from("abandoned");
    match mode {
        "drop" | "panic" => {
            let owned = ManagedClient {
                _client: client,
                cleanup: cleanup.clone(),
            };
            if mode == "drop" {
                drop(owned);
            } else {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _owned = owned;
                    panic!("injected managed unwind");
                }));
                assert!(result.is_err());
            }
        }
        "session_drop" | "shutdown_panic" => {
            let shared = Rc::new(std::cell::RefCell::new(client));
            let session = PlatformSession {
                control: Box::new(super::super::super::super::Control {
                    managed: Some(Box::new(cleanup.clone())),
                    client: shared.clone(),
                    capabilities: Default::default(),
                    timeout_ms: 100,
                }),
                abi: Box::new(super::super::super::super::Abi {
                    client: shared.clone(),
                }),
            };
            if mode == "session_drop" {
                drop(shared);
                drop(session);
            } else {
                std::mem::forget(shared.borrow_mut());
                drop(shared);
                let closure = session.shutdown();
                assert!(closure.error.is_some());
                outcome = closure.error.unwrap().to_string();
                assert!(outcome.contains("shutdown owner panicked"), "{outcome}");
            }
        }
        _ => {
            let mut client = client;
            let closure = close_managed_client(&mut client, Nonce::new_v4(), &mut cleanup);
            assert!(closure.error.is_some(), "{mode}");
            assert!(!closure.tree_terminated, "{mode}");
            outcome = closure.error.unwrap().to_string();
        }
    }
    // A successful parent wait cannot erase the protocol or abandonment failure.
    let failure = cleanup.collect_provider().expect_err("lost failure");
    assert!(
        failure
            .to_string()
            .contains("managed cleanup retained a failure"),
        "{mode}: collection did not reach a validated receipt: {failure}"
    );
    assert!(
        cleanup.provider_receipt().is_none(),
        "{mode}: failed run exposed success-only terminal evidence"
    );
    let until = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = owner_process.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < until, "cleanup owner parent wait");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(status.success(), "{mode}: cleanup owner exit {status}");
    let (wait_run, wait_provider, provider_status): (RunIdentity, Participant, i32) =
        wire::decode(&std::fs::read(format!("{dir}/provider-wait")).unwrap()).unwrap();
    assert_eq!(wait_run, run);
    assert_eq!(wait_provider, provider);
    assert_eq!(
        provider_status, 0,
        "{mode}: provider did not exit successfully"
    );
    release.join().unwrap();
    let started = Path::new(&marker(&dir, mode, "started")).exists();
    let collected = Path::new(&marker(&dir, mode, "collected")).exists();
    let finished = Path::new(&marker(&dir, mode, "finished")).exists();
    eprintln!(
        "ENDPOINT {mode} pid={} cleanup_started={started} cleanup_finished={finished} provider_wait={provider_status} owner_wait={status} outcome={outcome}",
        provider.pid
    );
    assert!(started, "{mode}: missing CleanupStarted");
    assert!(collected, "{mode}: missing actual resource parent wait");
    assert!(finished, "{mode}: missing CleanupFinished");
    std::fs::remove_dir_all(&dir).unwrap();
}
