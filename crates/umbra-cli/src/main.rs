#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use umbra_cli::Cli;

/// Exit codes: 0 on a clean run, 2 for a clap usage diagnostic, and 1 for every
/// structured failure, including a supervised program that exited nonzero. A
/// child's own exit code is deliberately not propagated: 0 would then mean
/// "the child succeeded", which says nothing about whether its writes persisted.
fn main() -> ExitCode {
    // #193: a guardian process (spawned by a relay below) runs here and never
    // returns. Before argument parsing, so nothing else runs in it first.
    umbra_isolation::guardian_entry();
    let cli = Cli::parse();
    // #193: a supervised run started in launchd's session moves into a fresh
    // session; this process becomes its relay and never returns. Anywhere
    // else (every terminal session included) this does nothing.
    if cli.starts_a_supervised_run() {
        umbra_isolation::isolate_run();
    }
    match cli.execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
