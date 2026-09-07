#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use umbra_cli::Cli;

fn main() -> ExitCode {
    match Cli::parse().execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
