use super::{managed_admission, native_order, path};
use std::{
    os::unix::ffi::OsStrExt,
    time::{Duration, Instant},
};
use umbra_core::*;
use umbra_platform::{TraceBackend, TraceControl};
use umbra_platform_macos::{BarrierReading, MacosTraceBackend, Options, TeardownStep};

pub fn run() {
    if std::env::var_os("UMBRA_INTEGRATION_REQUIRED").is_none() {
        eprintln!("SKIP native emergency: qualified host required");
        return;
    }
    let order = native_order::Observation::start().expect("fresh order directory");
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("family.c");
    let program = dir.path().join("family");
    // Automatic child collection keeps this deliberately unavailable observation
    // from leaving a zombie when the original parent is subsequently ended.
    std::fs::write(
        &source,
        r#"
#include <signal.h>
#include <unistd.h>
int main(void) {
    if (signal(SIGCHLD, SIG_IGN) == SIG_ERR) return 10;
    if (fork() < 0) return 11;
    for (;;) pause();
}
"#,
    )
    .unwrap();
    assert!(std::process::Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .status()
        .unwrap()
        .success());
    let mut backend = MacosTraceBackend::new(Options {
        twin_cache: Some(dir.path().join("twins")),
        timeout_ms: Some(30_000),
        ..Options::default()
    });
    let (_, service) = managed_admission::install(&mut backend);
    let root = backend
        .launch_experimental(LaunchSpec {
            executable: path(&program),
            argv: vec![program.as_os_str().as_bytes().to_vec()],
            environment: vec![],
            cwd: path(dir.path()),
            policy: LaunchPolicy {
                persistence: PersistencePolicy::LocalDevelopment,
                inherited_fds: vec![],
                interpose: false,
                descriptor_limit: None,
            },
            sandbox: SandboxRequirement::UnsandboxedExperiment,
        })
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(30);
    let child = loop {
        assert!(Instant::now() < limit, "fork observation bound");
        let event = match backend.poll_event(Duration::from_millis(10)).unwrap() {
            umbra_platform::EventPoll::Idle => continue,
            umbra_platform::EventPoll::Event(event) => event,
        };
        let thread = match event {
            TraceEvent::Child { child, .. } => break child.0 .0.native_id as i32,
            TraceEvent::Exec { thread, .. }
            | TraceEvent::ThreadStarted { thread, .. }
            | TraceEvent::SyscallEntry { thread, .. }
            | TraceEvent::SyscallExit { thread, .. } => thread,
            other => panic!("unexpected family event: {other:?}"),
        };
        backend
            .resume(ResumeCommand {
                thread,
                mode: ResumeMode::Syscall,
                signal: None,
            })
            .unwrap();
    };
    std::fs::write(
        order.directory().join(format!("withhold-terminal-{child}")),
        b"withhold",
    )
    .unwrap();
    let result = backend.terminate(root, TerminationPolicy::Immediate);
    let reports = backend.teardown_reports().to_vec();
    let start = backend.begin_shutdown(shutdown::Nonce::new_v4()).unwrap();
    drop(backend);
    let umbra_platform::shutdown::Start::Local { accepted, ledger } = start else {
        panic!("native ledger required")
    };
    let final_state = ledger.final_snapshot().unwrap();
    let confirmed = accepted.confirms(&final_state).unwrap();
    drop(ledger);
    let remaining = service.join().unwrap();
    assert!(
        result.is_err(),
        "unavailable confirmation must remain failed"
    );
    assert!(!confirmed, "later cleanup erased original failure");
    let report = reports
        .iter()
        .find(|r| r.pid == child)
        .expect("child teardown");
    assert!(report.timed_out && report.unconfirmed, "{report:?}");
    assert!(report
        .steps
        .iter()
        .any(|s| matches!(s, TeardownStep::KillSent)));
    assert!(report.steps.iter().any(|s| matches!(s, TeardownStep::Barrier(BarrierReading::Unconfirmed(reason)) if reason.contains("test withheld"))));
    assert!(report
        .steps
        .iter()
        .any(|s| matches!(s, TeardownStep::TracerReaped(true))));
    assert!(
        report.elapsed >= Duration::from_millis(1500),
        "original allocation did not expire"
    );
    eprintln!("native emergency: {report:?}; outstanding={remaining}");
    order.finish_case(&final_state, Some(child));
}
