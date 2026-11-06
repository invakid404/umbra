//! Test-only CLI composition with native export preparation before tracee launch.
use clap::Parser;
use std::{
    process::{Command, ExitCode, Stdio},
    time::{Duration, Instant},
};
use umbra_core::{RunId, UmbraError};
use umbra_supervisor::{RunObserver, RunOutcome};
struct Prepared;
impl RunObserver for Prepared {
    fn prepared(&mut self, run_id: RunId) {
        // The UUID comes from the supervisor, never from arbitrary shell input.
        // The parent is the run root already created by provider admission.
        let path = format!("/umbra/runs/{}/root/dst", run_id.0);
        let mut child = Command::new("docker")
            .args(["exec", "umbra-m1-transport-raw-ganesha", "sh", "-c",
                "set -eu; test -d \"${1%/*}\"; test ! -e \"$1\"; printf 'C16 incumbent must survive unchanged\\n' > \"$1\"; chmod 666 \"$1\"",
                "c16-seed", &path])
            .stdin(Stdio::null()).spawn().expect("native export seed spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("native seed wait") {
                assert!(status.success(), "native export seed failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("native export seed deadline; no tracee launched");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!("C16 incumbent seeded before launch: {path}");
        eprintln!("umbra: run {} prepared", run_id.0);
    }
    fn finished(&mut self, outcome: &RunOutcome) {
        eprintln!(
            "umbra: run {} finished: {:?} after {} process exits",
            outcome.run_id.0, outcome.root_status, outcome.processes_exited
        );
    }
    fn teardown_warning(&mut self, error: &UmbraError) {
        eprintln!("provider teardown warning: {error}");
    }
}
fn main() -> ExitCode {
    umbra_isolation::guardian_entry();
    let cli = umbra_cli::Cli::parse();
    assert!(cli.storage.storage_root.is_none());
    assert!(cli.starts_a_supervised_run());
    umbra_isolation::isolate_run();
    let umbra_cli::commands::Command::Run(args) = cli.command else {
        unreachable!()
    };
    assert!(args.agent.is_none());
    let result = umbra_cli::composition::load_registry(&args.registry)
        .and_then(|registry| {
            umbra_cli::composition::run_spec(args, registry, Some(Box::new(Prepared)))
        })
        .and_then(umbra_supervisor::run);
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
