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
    /// Start a supervised run over an explicit provider registry.
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

/// Where a *routed* run keeps the per-run state that cannot live in its store.
///
/// A run over storage with no kernel-visible path still needs two host paths:
/// umbra's own journal, and the single directory the enforcement profile grants
/// the tracee. Neither holds the run's data — that goes to the store through the
/// userspace client — but both must be somewhere the kernel can name, and both
/// must be findable again by `umbra resume`, so the location is an argument
/// rather than a temporary directory.
///
/// A run over storage that *does* expose kernel paths never reads this: its
/// journal lives in the store's own `control/`, exactly as before.
///
/// **`run` and `resume` must be given the same value.** A reopen that cannot
/// find the log refuses rather than reporting that the run is healthy, so a
/// mismatch is a loud failure — but it is still a failure the caller can avoid.
pub fn default_state_root() -> std::path::PathBuf {
    // Beside the twin cache the tracer already creates, for the same reason: it
    // is per-user, umbra-owned, and needs no setup step. `HOME` is unset in few
    // enough environments that falling back to the current directory is a worse
    // answer than an explicit `--state-dir`, so this yields a relative path that
    // `run` refuses as non-absolute and names.
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join("Library/Caches/umbra/runs")
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

/// Report an unimplemented command without echoing its arguments: argv and
/// environment can carry paths and secrets that have no business in a log line.
fn not_implemented(
    command: &'static str,
    _args: &impl std::fmt::Debug,
    _storage: &StorageArgs,
) -> Result<()> {
    Err(UmbraError::not_implemented(command))
}
