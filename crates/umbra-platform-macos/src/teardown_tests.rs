//! Ordinary-child RSP peer: exercises production teardown without tracing.
use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

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
    // The native control proves this ordinary child is collectable without RSP.
    let mut control = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    control.kill().unwrap();
    assert!(control.wait().unwrap().code().is_none());
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let (rsp, mut peer) = Rsp::retirement_test_pair();
    peer.set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let worker = std::thread::spawn(move || {
        let mut stopped = stopped;
        let mut bytes = Vec::new();
        let mut pending = Vec::new();
        while !peer_done.load(Ordering::Acquire) {
            let mut byte = [0];
            match peer.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    bytes.push(byte[0]);
                    if byte[0] == 3 {
                        stopped = true;
                        packet(&mut peer, "T02thread:1;");
                    } else {
                        pending.push(byte[0]);
                        if pending.ends_with(b"$k#6b") {
                            if stopped {
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
    let report = session.ordered_teardown();
    done.store(true, Ordering::Release);
    let bytes = worker.join().unwrap();
    // The production root fallback normally reaps it. Clean up even on oracle failure.
    if !session.reaped {
        let _ = child.kill();
        let _ = child.wait();
    }
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
