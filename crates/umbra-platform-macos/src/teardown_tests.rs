//! Ordinary-child RSP peer: exercises production teardown without tracing.
use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

struct TestChild {
    child: std::process::Child,
    reaped: bool,
}

impl Drop for TestChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
        }
        // ordered_teardown may already have reaped it with waitpid; still wait
        // on every path, including unwinding from a failed peer assertion.
        let _ = self.child.wait();
    }
}

fn session(pid: i32, rsp: Rsp) -> Session {
    let later_id = TaskId(TaskIdentity {
        native_id: pid as u64,
        generation: 1,
    });
    let task = Arc::new(Task::acquire(std::process::id() as i32).unwrap());
    Session {
        ownership: None,
        id: later_id,
        thread: tid(later_id.0.native_id, 1),
        parent: None,
        stopped: false,
        done: false,
        reaped: false,
        birth: bsd_info(pid).map(|i| (i.pbi_start_tvsec, i.pbi_start_tvusec)),
        torn_down: false,
        exit_evidence: None,
        exit_report: None,
        exec_generation: 0,
        #[cfg(feature = "signal-witness")]
        signal_probe_fired: false,
        signals: crate::signals::Delivery::default(),
        policy_exec_generation: 0,
        initial: false,
        retirement: None,
        retirement_report: None,
        live: Some(Box::new(LiveSession {
            _witness: crate::ownership::Witness::new(later_id.0.native_id, "body"),
            task,
            rsp,
            twin: PathBuf::new(),
            interposer: None,
            spawn: None,
            breaks: crate::ownership::Owned::new(BTreeMap::new(), later_id.0.native_id, "breaks"),
            pending: crate::ownership::Owned::new(BTreeMap::new(), later_id.0.native_id, "pending"),
            waiting: crate::ownership::Owned::new(vec![], later_id.0.native_id, "waiting"),
            entry: crate::ownership::Owned::new(BTreeMap::new(), later_id.0.native_id, "entry"),
            registers: crate::ownership::Owned::new(vec![], later_id.0.native_id, "registers"),
            scratch: crate::ownership::Owned::new(vec![], later_id.0.native_id, "scratch"),
        })),
    }
}

fn packet(peer: &mut std::net::TcpStream, body: &str) {
    let checksum = body.bytes().fold(0u8, u8::wrapping_add);
    write!(peer, "${body}#{checksum:02x}").unwrap();
}

fn run(stopped: bool) -> (TeardownReport, Vec<u8>) {
    run_peer(stopped, None, Some("T02thread:1;"), true)
}

fn run_peer(
    stopped: bool,
    queued: Option<&str>,
    stop_reply: Option<&'static str>,
    collect: bool,
) -> (TeardownReport, Vec<u8>) {
    run_peer_resume(stopped, queued, stop_reply, collect, false)
}

fn run_peer_resume(
    stopped: bool,
    queued: Option<&str>,
    stop_reply: Option<&'static str>,
    collect: bool,
    failed_resume: bool,
) -> (TeardownReport, Vec<u8>) {
    // The native control proves this ordinary child is collectable without RSP.
    let mut control = TestChild {
        child: std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
        reaped: false,
    };
    control.child.kill().unwrap();
    let status = control.child.wait().unwrap();
    control.reaped = true;
    assert!(status.code().is_none());
    let mut child = TestChild {
        child: std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
        reaped: false,
    };
    let pid = child.child.id() as i32;
    let (mut rsp, mut peer) = Rsp::retirement_test_pair();
    peer.set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    if let Some(queued) = queued {
        for reply in queued.split('|') {
            packet(&mut peer, reply);
        }
        rsp.buffer_test_packets(queued.split('|').count());
    }
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_stopped = stopped || queued.is_some();
    let worker = std::thread::spawn(move || {
        let mut stopped = peer_stopped;
        let mut bytes = Vec::new();
        let mut pending = Vec::new();
        while !peer_done.load(Ordering::Acquire) {
            let mut byte = [0];
            match peer.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    bytes.push(byte[0]);
                    if byte[0] == 3 {
                        stopped = stop_reply.is_some();
                        if let Some(reply) = stop_reply {
                            packet(&mut peer, reply);
                        }
                    } else {
                        pending.push(byte[0]);
                        if pending.ends_with(b"$k#6b") {
                            if stopped && collect {
                                // SAFETY: the test still owns this live ordinary child.
                                assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
                                packet(&mut peer, "X09");
                                break;
                            }
                            // Running debugserver discards asynchronous k.
                            pending.clear();
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("scripted peer read: {e}"),
            }
        }
        bytes
    });
    let mut session = session(pid, rsp);
    session.stopped = stopped;
    if failed_resume {
        rsp::SIGNAL_WRITE_FAULT.with(|fault| fault.set(Some((usize::MAX, "completed resume"))));
        assert!(session.continue_run().is_err());
    }
    let report = session.ordered_teardown();
    child.reaped = session.reaped;
    done.store(true, Ordering::Release);
    let bytes = worker.join().unwrap();
    // The production root fallback normally reaps it. Clean up even on oracle failure.
    drop(child);
    eprintln!("scripted teardown: {report:?}; wire={bytes:?}");
    (report, bytes)
}

#[test]
fn running_peer_requires_interrupt_before_k() {
    let (report, bytes) = run(false);
    assert!(
        !report.timed_out,
        "must confirm before killing tracer: {report:?}"
    );
    let interrupt = bytes.iter().position(|b| *b == 3).expect("interrupt");
    let kill = bytes.windows(5).position(|b| b == b"$k#6b").expect("k");
    assert!(interrupt < kill);
    let barrier = report
        .steps
        .iter()
        .position(|s| matches!(s, TeardownStep::Barrier(BarrierReading::Confirmed(_))))
        .unwrap();
    let tracer = report
        .steps
        .iter()
        .position(|s| matches!(s, TeardownStep::TracerKilled))
        .unwrap();
    assert!(barrier < tracer);
}

#[test]
fn stopped_peer_native_control_confirms_before_tracer_kill() {
    let (report, _) = run(true);
    assert!(!report.timed_out, "{report:?}");
}

#[test]
fn darwin_sigstop_interrupt_reply_confirms() {
    let (report, bytes) = run_peer(false, None, Some("T11thread:1;"), true);
    assert!(!report.timed_out && !report.unconfirmed, "{report:?}");
    assert_eq!(bytes, b"\x03$k#6b");
}

#[test]
fn missing_stop_never_sends_k() {
    let (report, bytes) = run_peer(false, None, None, true);
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert_eq!(bytes, [3]);
}

#[test]
fn eof_before_interrupt_records_connection_failure() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let (rsp, peer) = Rsp::retirement_test_pair();
    drop(peer);
    let mut session = session(child.id() as i32, rsp);
    let report = session.ordered_teardown();
    let _ = child.kill();
    let _ = child.wait();
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert!(report
        .steps
        .iter()
        .any(|s| matches!(s, TeardownStep::ConnectionClosed(_))));
    assert!(!report
        .steps
        .iter()
        .any(|s| matches!(s, TeardownStep::KillSent)));
}

#[test]
fn buffered_sigchld_answers_outstanding_resume_without_interrupt() {
    let (report, bytes) = run_peer(false, Some("T14thread:1;"), None, true);
    assert!(!report.timed_out && !report.unconfirmed, "{report:?}");
    assert_eq!(bytes, b"$k#6b");
}

#[test]
fn unsolicited_stop_is_loud() {
    let (report, bytes) = run_peer(true, Some("T14thread:1;"), None, true);
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert!(bytes.is_empty(), "{bytes:?}");
}

#[test]
fn duplicate_buffered_stop_is_loud() {
    let (report, bytes) = run_peer(false, Some("T14thread:1;|T14thread:1;"), None, true);
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert!(bytes.is_empty(), "{bytes:?}");
}

#[test]
fn breakpoint_answers_outstanding_resume() {
    let (report, bytes) = run_peer(false, None, Some("T05thread:1;"), true);
    assert!(!report.timed_out && !report.unconfirmed, "{report:?}");
    assert_eq!(bytes, b"\x03$k#6b");
}

#[test]
fn wrong_process_stop_is_loud() {
    let (report, bytes) = run_peer(false, None, Some("T02thread:pffffffff.1;"), true);
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert_eq!(bytes, [3]);
}

#[test]
fn stop_and_k_do_not_prove_collection() {
    let (report, bytes) = run_peer(false, None, Some("T02thread:1;"), false);
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert_eq!(bytes, b"\x03$k#6b");
    assert!(!report
        .steps
        .iter()
        .any(|step| matches!(step, TeardownStep::Barrier(BarrierReading::Confirmed(_)))));
}

#[test]
fn malformed_teardown_reply_is_rejected() {
    for packet in [
        "T",
        "S02extra",
        "Tzz",
        "T02thread:;",
        "T02thread:xyz;",
        "T02process:ffff;",
        "W00broken",
    ] {
        assert!(teardown_reply(packet, 123).is_err(), "{packet}");
    }
}

#[test]
fn completed_resume_error_accepts_answering_stop() {
    let (report, bytes) = run_peer_resume(true, Some("T05thread:1;"), None, true, true);
    assert!(!report.timed_out && !report.unconfirmed, "{report:?}");
    assert_eq!(bytes, b"$c#63$k#6b");
}
