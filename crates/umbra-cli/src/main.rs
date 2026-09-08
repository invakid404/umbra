#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use umbra_cli::Cli;

/// Exit codes: 0 on a clean run, 2 for a clap usage diagnostic, and 1 for every
/// structured failure, including a supervised program that exited nonzero. A
/// child's own exit code is deliberately not propagated: 0 would then mean
/// "the child succeeded", which says nothing about whether its writes persisted.
fn main() -> ExitCode {
    match Cli::parse().execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
