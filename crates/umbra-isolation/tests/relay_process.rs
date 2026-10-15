//! #193 relay error transitions, process level (ruling: carried obligations
//! R-7, R-10, record correctness, O5). The test binary plays both roles: a
//! child runs the real relay (`__relay_for_tests`), which spawns this binary
//! again as a **scripted guardian**, so each fault is exact.
#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};
use umbra_isolation::frame::{send, Msg, Read, Reader};
use umbra_isolation::sweep::Kind;

const SCENARIO: &str = "UMBRA_ISOLATION_TEST_SCENARIO";
const RELAY_DIR: &str = "UMBRA_ISOLATION_TEST_RELAY_DIR";
const MARKER: &str = "UMBRA_ISOLATION_TEST_GUARDIAN_MARKER";

fn raw(fd: i32, bytes: &[u8]) {
    // SAFETY: a valid buffer of the stated length.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

/// The scripted guardian. A no-op in an ordinary test run.
#[test]
fn scripted_guardian() {
    let Some(spec) = std::env::var_os(umbra_isolation::ENV_GUARDIAN) else {
        return;
    };
    let fds: Vec<i32> = spec
        .to_str()
        .unwrap()
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let (up, down) = (fds[0], fds[1]);
    let scenario = std::env::var(SCENARIO).unwrap();
    if let Some(marker) = std::env::var_os(MARKER) {
        std::fs::write(marker, b"started").unwrap();
    }
    // SAFETY: getpid has no arguments.
    let me = unsafe { libc::getpid() };
    if scenario == "silent" {
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(0);
    }
    if scenario == "wrong-sid" {
        let _ = send(up, &Msg::Isolated(me + 1));
        std::thread::sleep(Duration::from_secs(10));
        std::process::exit(0);
    }
    send(up, &Msg::Isolated(me)).unwrap();
    let mut r = Reader::new(down);
    match r.next(Instant::now() + Duration::from_secs(2)) {
        Read::Frame(f) => assert!(matches!(Msg::parse(&f), Ok(Msg::Go(_)))),
        other => panic!("{other:?}"),
    }
    send(up, &Msg::Started(me)).unwrap();
    let outcome = |status, kind, record_errno| {
        send(
            up,
            &Msg::Outcome {
                status,
                kind,
                record_errno,
            },
        )
        .unwrap()
    };
    match scenario.as_str() {
        "complete-0" => outcome(0, Kind::Complete, 0),
        "complete-sig9" => outcome(9, Kind::Complete, 0),
        "exit-3" => outcome(3 << 8, Kind::Complete, 0),
        "incomplete" => outcome(0, Kind::Incomplete, 0),
        "record-errno" => outcome(0, Kind::Complete, 28),
        "partial" => {
            raw(up, b"OUTCOME 0 COMP");
            std::thread::sleep(Duration::from_millis(300));
            raw(up, b"LETE 0\n");
        }
        "malformed" => {
            raw(up, b"OUTCOME banana\n");
            std::thread::sleep(Duration::from_secs(10));
        }
        "bad-kind" => {
            raw(up, b"OUTCOME 0 DONE 0\n");
            std::thread::sleep(Duration::from_secs(10));
        }
        "oversized" => {
            raw(up, &vec![b'A'; 4096]);
            std::thread::sleep(Duration::from_secs(10));
        }
        "eof-live" => {
            // SAFETY: closing our own descriptor.
            unsafe { libc::close(up) };
            std::thread::sleep(Duration::from_secs(10));
        }
        "stopped" => {
            // SAFETY: stopping ourselves.
            unsafe { libc::raise(libc::SIGSTOP) };
            std::thread::sleep(Duration::from_secs(10));
        }
        "exit-no-outcome" => {}
        "outcome-then-hang" => {
            outcome(0, Kind::Complete, 0);
            std::thread::sleep(Duration::from_secs(30));
        }
        other => panic!("unknown scenario {other}"),
    }
    std::process::exit(0);
}

/// The relay under test (a child process). A no-op in an ordinary test run.
#[test]
fn relay_entry() {
    let Some(dir) = std::env::var_os(RELAY_DIR) else {
        return;
    };
    let exe = std::env::current_exe().unwrap();
    let args: Vec<OsString> = [
        exe.as_os_str(),
        "--exact".as_ref(),
        "scripted_guardian".as_ref(),
        "--nocapture".as_ref(),
        "--test-threads=1".as_ref(),
    ]
    .iter()
    .map(|s| s.to_os_string())
    .collect();
    umbra_isolation::__relay_for_tests(exe, args, dir.into());
}

struct Run {
    status: std::process::ExitStatus,
    stderr: String,
    record: Vec<serde_json::Value>,
    elapsed: Duration,
}

fn run(scenario: &str) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "relay_entry", "--nocapture", "--test-threads=1"])
        .env(RELAY_DIR, dir.path())
        .env(SCENARIO, scenario)
        .env_remove(umbra_isolation::ENV_GUARDIAN)
        .output()
        .unwrap();
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "exactly one record: {files:?}");
    let name = files[0].file_name().unwrap().to_str().unwrap().to_string();
    assert!(umbra_isolation::record::is_record_name(&name), "{name}");
    let record = std::fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every record line is one JSON object"))
        .collect();
    Run {
        status: out.status,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        record,
        elapsed: started.elapsed(),
    }
}

fn events(r: &Run) -> Vec<String> {
    r.record
        .iter()
        .map(|v| v["event"].as_str().unwrap().to_string())
        .collect()
}

/// R-10: every guardian signal precedes the single reap, and the reap is the
/// record's last guardian event.
fn assert_no_signal_after_reap(r: &Run) {
    let ev = events(r);
    let reaped: Vec<_> = ev
        .iter()
        .enumerate()
        .filter(|(_, e)| *e == "guardian-reaped")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(reaped.len(), 1, "exactly one reap: {ev:?}");
    assert!(
        !ev[reaped[0]..].iter().any(|e| e == "guardian-signal"),
        "a guardian signal after the reap: {ev:?}"
    );
    assert_eq!(
        reaped[0],
        ev.len() - 1,
        "the reap is the record's last line: {ev:?}"
    );
    assert_eq!(
        r.record[reaped[0]]["reaped"], true,
        "the bounded reap reaped the guardian: {:?}",
        r.record[reaped[0]]
    );
    assert_eq!(
        ev.first().map(String::as_str),
        Some("relay-start"),
        "R-1: the record starts first"
    );
}

fn signals_sent(r: &Run) -> usize {
    r.record
        .iter()
        .filter(|v| v["event"] == "guardian-signal" && v["sent"] == true)
        .count()
}

#[test]
fn complete_mirrors_exit_zero() {
    let r = run("complete-0");
    assert_eq!(r.status.code(), Some(0), "{}", r.stderr);
    assert_no_signal_after_reap(&r);
    assert_eq!(signals_sent(&r), 0);
}

#[test]
fn complete_mirrors_exit_code() {
    let r = run("exit-3");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(3), "{}", r.stderr);
}

#[test]
fn complete_mirrors_a_signal() {
    let r = run("complete-sig9");
    assert_no_signal_after_reap(&r);
    assert_eq!(
        r.status.signal(),
        Some(libc::SIGKILL),
        "{:?} {}",
        r.status,
        r.stderr
    );
}

#[test]
fn incomplete_cleanup_is_exit_one_never_zero() {
    let r = run("incomplete");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("run isolation cleanup INCOMPLETE"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_guardian_record_failure_is_exit_one_never_zero() {
    let r = run("record-errno");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr
            .contains("isolation record write failed (guardian errno 28"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_partial_frame_across_timeouts_is_received() {
    let r = run("partial");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(0), "{}", r.stderr);
}

#[test]
fn eof_while_the_guardian_is_live_fails_closed() {
    let r = run("eof-live");
    assert_eq!(r.status.code(), Some(1));
    assert!(r.stderr.contains("guardian channel lost"), "{}", r.stderr);
    assert_eq!(
        signals_sent(&r),
        1,
        "the live guardian is ended by the relay"
    );
    assert!(events(&r).contains(&"relay-fallback".to_string()));
    assert_no_signal_after_reap(&r);
}

#[test]
fn a_malformed_frame_fails_closed() {
    let r = run("malformed");
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("protocol: malformed frame"),
        "{}",
        r.stderr
    );
    assert_no_signal_after_reap(&r);
}

#[test]
fn an_unknown_outcome_kind_fails_closed() {
    let r = run("bad-kind");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("protocol: malformed frame"),
        "{}",
        r.stderr
    );
}

#[test]
fn an_oversized_frame_fails_closed() {
    let r = run("oversized");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(1));
    assert!(r.stderr.contains("oversized frame"), "{}", r.stderr);
}

#[test]
fn a_stopped_guardian_is_ended_and_the_run_swept() {
    let r = run("stopped");
    assert_eq!(r.status.code(), Some(1));
    assert!(r.stderr.contains("guardian stopped"), "{}", r.stderr);
    assert_eq!(signals_sent(&r), 1);
    assert_no_signal_after_reap(&r);
}

#[test]
fn a_wrong_session_claim_fails_closed() {
    let r = run("wrong-sid");
    assert_no_signal_after_reap(&r);
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("guardian did not isolate"),
        "{}",
        r.stderr
    );
}

/// R-10: a guardian that has already exited is never signalled by pid.
#[test]
fn an_exited_guardian_is_never_signalled() {
    let r = run("exit-no-outcome");
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("guardian ended without an outcome"),
        "{}",
        r.stderr
    );
    assert_eq!(signals_sent(&r), 0, "{:?}", r.record);
    assert_no_signal_after_reap(&r);
}

/// R-1: the record must exist before anything is launched. An unusable record
/// directory is exit 1 and no guardian ever starts.
#[test]
fn an_unusable_record_directory_launches_nothing() {
    let scratch = tempfile::tempdir().unwrap();
    let not_a_dir = scratch.path().join("file");
    std::fs::write(&not_a_dir, b"").unwrap();
    let marker = scratch.path().join("guardian-started");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "relay_entry", "--nocapture", "--test-threads=1"])
        .env(RELAY_DIR, &not_a_dir)
        .env(SCENARIO, "complete-0")
        .env(MARKER, &marker)
        .env_remove(umbra_isolation::ENV_GUARDIAN)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("umbra: run isolation failed: cannot create isolation record"),
        "{stderr}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!marker.exists(), "a guardian was started without a record");
}

/// R-6: a guardian that never isolates is given up on at the handshake bound
/// (2 s), killed while it is the relay's unreaped child, and swept.
#[test]
fn a_silent_guardian_is_bounded() {
    let r = run("silent");
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("guardian did not isolate in time"),
        "{}",
        r.stderr
    );
    assert_eq!(signals_sent(&r), 1);
    assert_no_signal_after_reap(&r);
    // 2 s handshake + 1 s kill wait + 3 s sweep bound, plus process start-up.
    assert!(r.elapsed < Duration::from_secs(7), "{:?}", r.elapsed);
}

/// R-6: no blocking wait on a guardian that never exits after its outcome.
#[test]
fn a_guardian_that_never_exits_is_bounded() {
    let r = run("outcome-then-hang");
    assert_eq!(r.status.code(), Some(1));
    assert!(
        r.stderr.contains("guardian did not exit after its outcome"),
        "{}",
        r.stderr
    );
    assert_eq!(signals_sent(&r), 1);
    assert_no_signal_after_reap(&r);
    // 1 s reap bound + 1 s kill wait + 3 s sweep bound, plus start-up.
    assert!(r.elapsed < Duration::from_secs(6), "{:?}", r.elapsed);
}
