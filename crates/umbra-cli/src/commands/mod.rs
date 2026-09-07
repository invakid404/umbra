/// Request a logical checkpoint after the caller establishes quiescence.
pub mod checkpoint;
/// Inspect.
pub mod inspect;
pub mod providers;
/// Resume.
pub mod resume;
/// Run.
pub mod run;
/// Stop.
pub mod stop;

use clap::{Args, Subcommand};
use umbra_core::{Result, UmbraError};
use uuid::Uuid;

use crate::StorageArgs;

#[derive(Debug, Subcommand)]
/// Command.
pub enum Command {
    /// Validate explicitly configured provider connections without starting a run.
    Providers(providers::ProviderArgs),
    /// Start a supervised agent run (not implemented).
    Run(run::RunArgs),
    /// Stop a supervised run (not implemented).
    Stop(RunIdArgs),
    /// Checkpoint a supervised run (not implemented).
    Checkpoint(RunIdArgs),
    /// Resume an existing run and its recorded agent session (not implemented).
    Resume(resume::ResumeArgs),
    /// Inspect an existing run (not implemented).
    Inspect(RunIdArgs),
}

#[derive(Debug, Args)]
/// Run id args.
pub struct RunIdArgs {
    /// Stable run UUID.
    #[arg(value_name = "RUN_ID")]
    pub run_id: Uuid,
}

impl Command {
    /// Execute.
    pub fn execute(self, storage: &StorageArgs) -> Result<()> {
        match self {
            Self::Providers(args) => providers::handle(args),
            Self::Run(args) => run::handle(args, storage),
            Self::Stop(args) => stop::handle(args, storage),
            Self::Checkpoint(args) => checkpoint::handle(args, storage),
            Self::Resume(args) => resume::handle(args, storage),
            Self::Inspect(args) => inspect::handle(args, storage),
        }
    }
}

fn not_implemented(
    command: &'static str,
    args: &impl std::fmt::Debug,
    storage: &StorageArgs,
) -> Result<()> {
    println!("{command}: {args:?}, {storage:?}");
    Err(UmbraError::not_implemented(command))
}
