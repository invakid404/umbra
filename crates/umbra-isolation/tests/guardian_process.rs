//! #193 guardian, process level (ruling: record correctness, R-4 no-caller
//! reporting, O5 inputs). The test binary plays three roles: a **fake relay**
//! (a child process of the test, so the guardian's inherited stderr can be
//! captured), the **real guardian** (`guardian_entry`, spawned by the fake
//! relay into a new session exactly as the relay does), and a scripted
//! **supervisor** (the guardian spawns this binary again with the same
//! arguments, as it spawns `umbra`).
//!
//! The record is a FIFO the fake relay reads: closing the read end makes every
//! later record write fail with EPIPE, a deterministic mid-run record failure.
#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::time::{Duration, Instant};
use umbra_isolation::frame::{send, Msg, Read, Reader};
use umbra_isolation::spawn::{close, environment, pipe, spawn};
use umbra_isolation::sweep::Kind;
use umbra_isolation::ENV_GUARDIAN;

const SCENARIO: &str = "UMBRA_ISOLATION_TEST_GUARDIAN_SCENARIO";
const FAKE_RELAY: &str = "UMBRA_ISOLATION_TEST_FAKE_RELAY";
/// Where the scripted supervisor records that it ran (set by the fake relay).
const MARK: &str = "UMBRA_ISOLATION_TEST_SUPERVISOR_MARK";

/// The supervisor role, from kernel facts, not from the environment: this
/// process's parent leads the session this process is in (the guardian, which
/// the relay spawns with `POSIX_SPAWN_SETSID`), and the parent is this same
/// test binary. An ordinary test run's parent is cargo or a shell, never this
/// binary.
fn started_by_a_guardian() -> bool {
    // SAFETY: getppid and getsid take no memory arguments.
    let (parent, sid, parent_sid) = unsafe {
        (
            libc::getppid(),
            libc::getsid(0),
            libc::getsid(libc::getppid()),
        )
    };
    if parent <= 1 || parent_sid != parent || sid != parent {
        return false;
    }
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is valid for its stated length.
    let n = unsafe { libc::proc_pidpath(parent, path.as_mut_ptr().cast(), path.len() as u32) };
    if n <= 0 {
        return false;
    }
    use std::os::unix::ffi::OsStrExt;
    let parent_exe = std::path::Path::new(std::ffi::OsStr::from_bytes(&path[..n as usize]));
    let me = std::env::current_exe().unwrap();
    parent_exe.canonicalize().ok() == me.canonicalize().ok()
}

/// Guardian and scripted supervisor. A no-op in an ordinary test run.
#[test]
fn guardian_or_supervisor() {
    umbra_isolation::guardian_entry();
    // Not a guardian: the supervisor role, if the guardian started us.
    if !started_by_a_guardian() {
        return;
    }
    // SAFETY: getpid, getppid and getsid take no memory arguments.
    let (me, parent, sid) = unsafe { (libc::getpid(), libc::getppid(), libc::getsid(0)) };
    if let Some(mark) = std::env::var_os(MARK) {
        std::fs::write(mark, format!("{me} {parent} {sid}")).unwrap();
    }
    if std::env::var(SCENARIO)
        .unwrap()
        .ends_with("supervisor-exits")
    {
        std::thread::sleep(Duration::from_millis(500));
    } else {
        std::thread::sleep(Duration::from_secs(30));
    }
    std::process::exit(0);
}

fn line(text: String) {
    println!("FAKE-RELAY {text}");
}

/// The fake relay. A no-op in an ordinary test run.
#[test]
fn fake_relay() {
    if std::env::var_os(FAKE_RELAY).is_none() {
        return;
    }
    let scenario = std::env::var(SCENARIO).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (up_r, up_w) = pipe().unwrap();
    let (down_r, down_w) = pipe().unwrap();
    let (life_r, life_w) = pipe().unwrap();
    let exe = std::env::current_exe().unwrap();
    let args: Vec<OsString> = [
        exe.as_os_str(),
        "--exact".as_ref(),
        "guardian_or_supervisor".as_ref(),
        "--nocapture".as_ref(),
    ]
    .iter()
    .map(|s| s.to_os_string())
    .collect();
    let mark = dir.path().join("supervisor.mark");
    let env = environment(
        &[FAKE_RELAY],
        &[
            (ENV_GUARDIAN, format!("{up_w},{down_r},{life_r}")),
            (MARK, mark.to_str().unwrap().to_string()),
        ],
    );
    let g = spawn(&exe, &args, &env, true, &[up_w, down_r, life_r]).unwrap();
    close(up_w);
    close(down_r);
    close(life_r);
    let mut reader = Reader::new(up_r);
    let next = |reader: &mut Reader| match reader.next(Instant::now() + Duration::from_secs(5)) {
        Read::Frame(f) => Msg::parse(&f).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(next(&mut reader), Msg::Isolated(g));
    let fifo = dir.path().join("record.jsonl");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    // SAFETY: as above; a nonblocking reader so the guardian's open succeeds.
    let record_r = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    assert!(record_r >= 0);
    send(down_w, &Msg::Go(fifo.clone())).unwrap();
    let Msg::Started(supervisor) = next(&mut reader) else {
        panic!("no STARTED")
    };
    // SAFETY: getsid has no memory arguments.
    line(format!(
        "supervisor={supervisor} supervisor_sid={}",
        unsafe { libc::getsid(supervisor) }
    ));
    line(format!("guardian={g}"));
    // The supervisor found its role from kernel facts; it says so here.
    let end = Instant::now() + Duration::from_secs(3);
    while std::fs::metadata(&mark).map_or(true, |m| m.len() == 0) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    line(format!(
        "supervisor_mark={}",
        std::fs::read_to_string(&mark).unwrap_or_else(|_| "absent".into())
    ));
    let mut bytes = vec![0u8; 65536];
    // SAFETY: a valid buffer; read what the guardian wrote so far.
    let n = unsafe { libc::read(record_r, bytes.as_mut_ptr().cast(), bytes.len()) };
    line(format!(
        "record_before={}",
        String::from_utf8_lossy(&bytes[..n.max(0) as usize])
            .trim()
            .replace('\n', "|")
    ));
    if scenario.starts_with("record-fails") {
        close(record_r);
    }
    match scenario.as_str() {
        "record-fails-relay-dies" | "relay-dies" => close(life_w),
        _ => {
            let outcome = next(&mut reader);
            line(format!("outcome={outcome:?}"));
        }
    }
    let mut st = 0;
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: a valid out-pointer; `g` is our child.
        if unsafe { libc::waitpid(g, &mut st, libc::WNOHANG) } == g {
            break;
        }
        assert!(Instant::now() < end, "the guardian did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    line(format!("guardian_exit={}", libc::WEXITSTATUS(st)));
    // SAFETY: kill(pid, 0) only probes.
    line(format!(
        "supervisor_alive={}",
        unsafe { libc::kill(supervisor, 0) } == 0
    ));
    if !scenario.starts_with("record-fails") {
        // SAFETY: a valid buffer.
        let n = unsafe { libc::read(record_r, bytes.as_mut_ptr().cast(), bytes.len()) };
        line(format!(
            "record_after={}",
            String::from_utf8_lossy(&bytes[..n.max(0) as usize])
                .trim()
                .replace('\n', "|")
        ));
    }
    std::process::exit(0);
}

struct Seen {
    stdout: String,
    stderr: String,
}

impl Seen {
    fn get(&self, key: &str) -> String {
        let prefix = format!("FAKE-RELAY {key}=");
        self.stdout
            .lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no {key}:\n{}\n{}", self.stdout, self.stderr))
            .to_string()
    }
}

fn run(scenario: &str) -> Seen {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fake_relay", "--nocapture", "--test-threads=1"])
        .env(FAKE_RELAY, "1")
        .env(SCENARIO, scenario)
        .env_remove(ENV_GUARDIAN)
        .env_remove(MARK)
        .output()
        .unwrap();
    let seen = Seen {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    assert!(
        out.status.success(),
        "fake relay failed:\n{}\n{}",
        seen.stdout,
        seen.stderr
    );
    seen
}

/// The guardian's child took the supervisor role (role found from kernel
/// facts, not the environment): it is the reported supervisor, its parent is
/// the guardian, and it is in the guardian's session.
fn assert_supervisor_ran(seen: &Seen) {
    let supervisor = seen.get("supervisor");
    let supervisor = supervisor.split_once(' ').unwrap().0;
    let g = seen.get("guardian");
    assert_eq!(
        seen.get("supervisor_mark"),
        format!("{supervisor} {g} {g}"),
        "the supervisor role was not taken by the guardian's child"
    );
}

fn assert_supervisor_in_guardian_session(seen: &Seen) {
    let s = seen.get("supervisor");
    let (pid, sid) = s.split_once(" supervisor_sid=").unwrap();
    assert_ne!(pid, sid, "the supervisor does not lead a session");
    assert!(seen
        .get("record_before")
        .contains("\"event\":\"supervisor-started\""));
}

/// Normal path: the supervisor exits, the guardian sweeps COMPLETE and
/// reports it with no record failure, then exits 0.
#[test]
fn supervisor_exit_is_swept_and_reported() {
    let seen = run("supervisor-exits");
    assert_supervisor_ran(&seen);
    assert_supervisor_in_guardian_session(&seen);
    assert_eq!(
        seen.get("outcome"),
        format!(
            "{:?}",
            Msg::Outcome {
                status: 0,
                kind: Kind::Complete,
                record_errno: 0
            }
        )
    );
    assert_eq!(seen.get("guardian_exit"), "0");
    assert!(seen
        .get("record_after")
        .contains("\"why\":\"supervisor-exit\""));
}

/// A record write that fails mid-run is reported in OUTCOME (the relay then
/// exits 1 under O5), and the guardian itself exits non-zero.
#[test]
fn a_mid_run_record_failure_reaches_the_outcome() {
    let seen = run("record-fails-supervisor-exits");
    assert_supervisor_ran(&seen);
    assert_eq!(
        seen.get("outcome"),
        format!(
            "{:?}",
            Msg::Outcome {
                status: 0,
                kind: Kind::Complete,
                record_errno: libc::EPIPE
            }
        )
    );
    assert_eq!(seen.get("guardian_exit"), "1");
}

/// Relay death: the guardian sweeps its session (the supervisor dies) and
/// records it.
#[test]
fn relay_death_sweeps_the_session() {
    let seen = run("relay-dies");
    assert_supervisor_ran(&seen);
    assert_eq!(seen.get("guardian_exit"), "0");
    assert_eq!(seen.get("supervisor_alive"), "false");
    let after = seen.get("record_after");
    assert!(
        after.contains("\"event\":\"relay-death\"") && after.contains("\"outcome\":\"COMPLETE\""),
        "{after}"
    );
    assert!(!seen.stderr.contains("relay gone"), "{}", seen.stderr);
}

/// R-4: relay dead and record unwritable. The inherited stderr is the only
/// report left, and the guardian exits non-zero; the session is still swept.
#[test]
fn no_caller_and_no_record_reports_on_stderr() {
    let seen = run("record-fails-relay-dies");
    assert_supervisor_ran(&seen);
    assert_eq!(seen.get("guardian_exit"), "1");
    assert_eq!(seen.get("supervisor_alive"), "false");
    assert!(
        seen.stderr.contains(&format!(
            "umbra: run guardian: relay gone, cleanup COMPLETE, isolation record write failed (errno {})",
            libc::EPIPE
        )),
        "{}",
        seen.stderr
    );
}
