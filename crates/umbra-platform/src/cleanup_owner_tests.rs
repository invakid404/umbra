use super::*;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Instant;

fn marker(dir: &str, mode: &str, name: &str) -> String {
    format!("{dir}/{mode}.{name}")
}
fn await_file(path: &str) {
    let limit = Instant::now() + Duration::from_secs(4);
    while !Path::new(path).exists() {
        assert!(Instant::now() < limit, "missing {path}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn response(c: &mut Connection, id: u64, r: Response) {
    let _ = c.send(&Frame::Response {
        id,
        result: wire::encode(&r),
    });
}
fn child(dir: &str, mode: &str) {
    let mut stream = UnixStream::connect(format!("{dir}/{mode}.sock")).unwrap();
    stream.write_all(b"R").unwrap();
    let mut c = Connection::new(stream, Duration::from_secs(3));
    if !matches!(
        mode,
        "drop" | "panic" | "session_drop" | "shutdown_panic" | "control"
    ) {
        let Frame::Request { id, payload } = c.receive().unwrap() else {
            panic!("begin")
        };
        let Request::BeginShutdown { nonce } = wire::decode(&payload).unwrap() else {
            panic!("begin")
        };
        let ledger = Ledger::new();
        let root = crate::shutdown::collected_model_root(&ledger);
        let slot = ledger
            .reserve(ResourceKind::Tracer, Some(root), 1, 0)
            .unwrap();
        slot.acquired(Identity {
            pid: Some(std::process::id() as i32),
            birth: None,
        })
        .unwrap();
        if mode != "unconfirmed_final" {
            slot.confirm(Proof::Collected {
                status: "model".into(),
            })
            .unwrap();
        }
        let mut accepted = ledger.freeze(nonce, Duration::from_secs(1)).unwrap();
        match mode {
            "begin_eof" => c.invalidate(),
            "begin_timeout" => std::thread::sleep(Duration::from_millis(250)),
            "phase" => response(
                &mut c,
                id,
                Response::ShutdownPoll(Poll::Pending { progress: 0 }),
            ),
            "wrong_id" => response(&mut c, id + 1, Response::ShutdownAccepted(accepted)),
            "malformed_response" => {
                let _ = c.send(&Frame::Response {
                    id,
                    result: Ok(vec![255]),
                });
            }
            "provider_error" => {
                let _ = c.send(&Frame::Response {
                    id,
                    result: Err(protocol_error("injected provider error")),
                });
            }
            "nonce" => {
                accepted.nonce = Nonce::new_v4();
                response(&mut c, id, Response::ShutdownAccepted(accepted));
            }
            "invalid_accept" => {
                accepted.high_water += 1;
                response(&mut c, id, Response::ShutdownAccepted(accepted));
            }
            _ => {
                if mode == "deadline" {
                    accepted.remaining_millis = 0;
                }
                response(&mut c, id, Response::ShutdownAccepted(accepted.clone()));
                let Frame::Request { id, .. } = c.receive().unwrap() else {
                    panic!("poll")
                };
                match mode {
                    "poll_eof" => c.invalidate(),
                    "poll_timeout" => std::thread::sleep(Duration::from_millis(250)),
                    "poll_phase" => response(&mut c, id, Response::ShutdownAccepted(accepted)),
                    "backwards" => {
                        response(
                            &mut c,
                            id,
                            Response::ShutdownPoll(Poll::Pending { progress: 2 }),
                        );
                        let Frame::Request { id, .. } = c.receive().unwrap() else {
                            panic!("second poll")
                        };
                        response(
                            &mut c,
                            id,
                            Response::ShutdownPoll(Poll::Pending { progress: 1 }),
                        );
                    }
                    "deadline" => response(
                        &mut c,
                        id,
                        Response::ShutdownPoll(Poll::Pending { progress: 0 }),
                    ),
                    _ => {
                        let mut final_state = ledger.final_snapshot().unwrap();
                        if mode == "invalid_final" {
                            final_state.generation += 1;
                        }
                        response(&mut c, id, Response::ShutdownPoll(Poll::Final(final_state)));
                    }
                }
            }
        }
    } else {
        let _: umbra_core::Result<Frame> = c.receive();
    }
    std::fs::write(marker(dir, mode, "started"), b"started").unwrap();
    // No live resource child exists until the harness permits collection.
    // An early provider stop therefore fails without orphaning a test child.
    await_file(&marker(dir, mode, "release"));
    let mut resource = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let resource_pid = resource.id();
    let status = resource.wait().unwrap();
    assert!(status.success(), "resource parent wait: {status}");
    std::fs::write(
        marker(dir, mode, "collected"),
        format!(
            "provider={} resource={resource_pid} status={status}",
            std::process::id()
        ),
    )
    .unwrap();
    std::fs::write(marker(dir, mode, "finished"), b"finished").unwrap();
}

#[test]
fn r2_endpoint_child() {
    let Ok(mode) = std::env::var("UMBRA_FIX299_ENDPOINT_CHILD") else {
        return;
    };
    let dir = std::env::var("UMBRA_FIX299_ENDPOINT_DIR").unwrap();
    child(&dir, &mode);
}

fn endpoint(mode: &str) {
    let dir = std::env::temp_dir().join(format!("umbra-fix-299b-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_owned();
    let socket = format!("{dir}/{mode}.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["r2_endpoint_child", "--nocapture"])
        .env("UMBRA_FIX299_ENDPOINT_DIR", &dir)
        .env("UMBRA_FIX299_ENDPOINT_CHILD", mode)
        .spawn()
        .unwrap();
    let pid = process.id();
    let limit = Instant::now() + Duration::from_secs(4);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < limit);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("{e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut ready = [0];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    let mut connection = Connection::new(stream, Duration::from_millis(100));
    let release_dir = dir.clone();
    let release_mode = mode.to_owned();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(marker(&release_dir, &release_mode, "release"), b"release").unwrap();
    });
    if mode == "control" {
        connection.invalidate();
        let until = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = process.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(status.success());
        await_file(&marker(&dir, mode, "finished"));
        eprintln!("ENDPOINT {mode} pid={pid} parent_wait={status} cleanup_finished=true");
        release.join().unwrap();
        assert!(Path::new(&marker(&dir, mode, "collected")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let provider_wait = Client::observe_owned_exit(pid);
    let mut client = Client::from_owned_connection(
        connection,
        wire::Welcome {
            id: "plan-probe".into(),
            role: "platform".into(),
            version: wire::PROTOCOL_VERSION,
            capabilities: Default::default(),
        },
        process,
    );
    let mut outcome = String::from("abandoned");
    match mode {
        "drop" => drop(client),
        "panic" => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _owned = client;
                panic!("injected direct unwind");
            }));
        }
        "session_drop" | "shutdown_panic" => {
            let shared = Rc::new(std::cell::RefCell::new(client));
            let session = PlatformSession {
                control: Box::new(super::super::super::Control {
                    client: shared.clone(),
                    capabilities: Default::default(),
                    timeout_ms: 100,
                }),
                abi: Box::new(super::super::super::Abi {
                    client: shared.clone(),
                }),
            };
            if mode == "session_drop" {
                drop(shared);
                drop(session);
            } else {
                // Leave the borrow flag set without retaining a third owner.
                // After ABI destruction, Control owns the only Rc and its
                // borrow panics inside PlatformSession::shutdown's catcher.
                std::mem::forget(shared.borrow_mut());
                drop(shared);
                let closure = session.shutdown();
                assert!(closure.error.is_some());
                outcome = closure.error.unwrap().to_string();
                assert!(outcome.contains("shutdown owner panicked"), "{outcome}");
            }
        }
        _ => {
            let closure = close_client(&mut client, Nonce::new_v4());
            assert!(closure.error.is_some(), "{mode}");
            assert!(!closure.tree_terminated, "{mode}");
            outcome = closure.error.unwrap().to_string();
            drop(client);
        }
    }
    release.join().unwrap();
    let started = Path::new(&marker(&dir, mode, "started")).exists();
    let finished = Path::new(&marker(&dir, mode, "finished")).exists();
    eprintln!(
        "ENDPOINT {mode} pid={pid} cleanup_started={started} cleanup_finished={finished} outcome={outcome}"
    );
    let collected = Path::new(&marker(&dir, mode, "collected")).exists();
    std::fs::remove_dir_all(&dir).unwrap();
    let waited = *provider_wait.lock().unwrap();
    assert!(waited.is_some(), "{mode}: missing provider parent wait");
    assert!(
        waited.unwrap().success(),
        "{mode}: provider did not exit successfully"
    );
    assert!(started, "{mode}: missing CleanupStarted");
    assert!(collected, "{mode}: missing actual resource parent wait");
    assert!(finished, "{mode}: missing CleanupFinished");
}

macro_rules! endpoint_tests {
    ($($name:ident => $mode:literal),* $(,)?) => { $(
        #[test]
        fn $name() { endpoint($mode); }
    )* };
}
endpoint_tests! {
    r2_cleanup_eof_control => "control",
    r2_cleanup_before_kill_phase => "phase",
    r2_cleanup_before_kill_nonce => "nonce",
    r2_cleanup_before_kill_begin_eof => "begin_eof",
    r2_cleanup_before_kill_poll_eof => "poll_eof",
    r2_cleanup_before_kill_deadline => "deadline",
    r2_cleanup_before_kill_invalid_accept => "invalid_accept",
    r2_cleanup_before_kill_backwards => "backwards",
    r2_cleanup_before_kill_poll_phase => "poll_phase",
    r2_cleanup_before_kill_invalid_final => "invalid_final",
    r2_cleanup_before_kill_begin_timeout => "begin_timeout",
    r2_cleanup_before_kill_poll_timeout => "poll_timeout",
    r2_cleanup_before_kill_drop => "drop",
    r2_cleanup_before_kill_panic => "panic",
    r2_cleanup_before_kill_session_drop => "session_drop",
    r2_cleanup_before_kill_shutdown_panic => "shutdown_panic",
    r2_cleanup_before_kill_unconfirmed_final => "unconfirmed_final",
    r2_cleanup_before_kill_wrong_id => "wrong_id",
    r2_cleanup_before_kill_malformed_response => "malformed_response",
    r2_cleanup_before_kill_provider_error => "provider_error",
}
