//! CLI parsing and concrete, in-process supervisor composition.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Commands.
pub mod commands;
pub mod composition;

use std::path::PathBuf;

use clap::{Args, Parser};
use umbra_core::Result;

/// Umbra's parsing skeleton. Operational commands are not implemented yet.
#[derive(Debug, Parser)]
#[command(
    name = "umbra",
    version,
    about = "Supervised agent runs (CLI skeleton)"
)]
pub struct Cli {
    #[command(flatten)]
    /// Storage.
    pub storage: StorageArgs,
    #[command(subcommand)]
    /// Command.
    pub command: commands::Command,
}

#[derive(Debug, Args)]
/// Storage args.
pub struct StorageArgs {
    /// Local development storage directory; does not provide remote persistence.
    #[arg(long, global = true, default_value = ".umbra", value_name = "PATH")]
    pub storage_root: PathBuf,
}

impl Cli {
    /// Dispatch only to argument-reporting stubs; do not construct or open storage.
    pub fn execute(self) -> Result<()> {
        self.command.execute(&self.storage)
    }
}
