//! CLI parsing and concrete, in-process supervisor composition.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Commands.
pub mod commands;
pub mod composition;

use std::path::PathBuf;

use clap::{Args, Parser};
use umbra_core::Result;

/// Umbra's command-line surface. `run` is operational; the run-management
/// commands remain unimplemented and report that explicitly.
#[derive(Debug, Parser)]
#[command(
    name = "umbra",
    version,
    about = "Supervised runs over an explicitly configured provider registry"
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
    /// Legacy local storage directory, retained only so existing invocations
    /// still parse. `run` rejects an explicit value rather than ignoring it:
    /// storage is configured in the provider registry.
    #[arg(long, global = true, value_name = "PATH")]
    pub storage_root: Option<PathBuf>,
}

impl Cli {
    /// Dispatch to the selected command. Commands that are not implemented say
    /// so; none of them prompts, and none reads stdin for configuration.
    pub fn execute(self) -> Result<()> {
        self.command.execute(&self.storage)
    }
}
