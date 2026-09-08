//! `umbra run`: parse an explicit request, load the registry, hand it to the
//! supervisor.
//!
//! Nothing in this command asks a question. Every missing prerequisite —
//! registry, provider, capability, mount, permission — becomes a structured
//! error and a nonzero exit. stdin is never read for configuration; it belongs to
//! the supervised program.

use std::{ffi::OsString, os::unix::ffi::OsStrExt, path::PathBuf};

use clap::Args;
use umbra_core::{ErrorKind, Result, RunId, UmbraError};
use umbra_supervisor::{RunObserver, RunOutcome, RunPersistence};

use crate::{composition, StorageArgs};

#[derive(Debug, Args)]
/// Arguments for a supervised run.
pub struct RunArgs {
    /// Explicit JSON registry of installed provider executables and options.
    /// This is the single source of storage configuration for the run.
    #[arg(long, value_name = "PATH")]
    pub registry: PathBuf,
    /// Host directory approved as this run's workspace.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub workspace: PathBuf,
    /// Acknowledge the bounded experimental tracing mode. Required; it disables
    /// no enforcement and grants no unsupported behavior.
    #[arg(long)]
    pub experimental: bool,
    /// Use explicitly configured local development storage instead of the
    /// default validated NFSv4 mount. Local storage is not remote durability.
    #[arg(long, conflicts_with = "strict_remote")]
    pub local_dev: bool,
    /// Require strict remote durability. No storage provider qualifies this yet,
    /// so this fails deterministically rather than promising it.
    #[arg(long)]
    pub strict_remote: bool,
    /// Run a coding agent through the named adapter instead of a raw command.
    /// Omitted means the trailing arguments are the command to run.
    #[arg(long, value_name = "ID")]
    pub agent: Option<String>,
    /// Set one environment variable for the supervised program. The program's
    /// environment is exactly these plus the run's own runtime directories;
    /// nothing is inherited implicitly.
    #[arg(long = "env", value_name = "NAME=VALUE")]
    pub env: Vec<OsString>,
    /// Forward one named variable from this process's environment. Fails if the
    /// variable is unset, rather than passing a silently empty value.
    #[arg(long = "inherit-env", value_name = "NAME")]
    pub inherit_env: Vec<OsString>,
    /// The command to run, after `--`, starting with an absolute executable path.
    #[arg(last = true, value_name = "COMMAND")]
    pub argv: Vec<OsString>,
}

impl RunArgs {
    /// Selected storage mode. The default is validated NFSv4 with client-fsync
    /// durability; local development is never selected implicitly.
    pub fn persistence(&self) -> RunPersistence {
        match (self.local_dev, self.strict_remote) {
            (true, _) => RunPersistence::LocalDevelopment,
            (_, true) => RunPersistence::StrictRemote,
            _ => RunPersistence::NfsClientFsync,
        }
    }
}

/// Report status on stderr so the supervised program keeps stdout for its output.
struct StderrStatus;

impl RunObserver for StderrStatus {
    fn prepared(&mut self, run_id: RunId) {
        eprintln!("umbra: run {} prepared", run_id.0);
    }

    fn finished(&mut self, outcome: &RunOutcome) {
        eprintln!(
            "umbra: run {} finished: {:?} after {} process exits",
            outcome.run_id.0, outcome.root_status, outcome.processes_exited
        );
    }
}

/// Parse, load the registry and execute. Returns the run's result, not the
/// child's exit code: a nonzero child is reported as `ProcessFailed`.
pub fn handle(args: RunArgs, storage: &StorageArgs) -> Result<()> {
    if storage.storage_root.is_some() {
        return Err(invalid(
            "--storage-root does not configure a run; set the storage provider's \
             physical root in the --registry descriptor options instead",
        ));
    }
    if let Some(agent) = &args.agent {
        // Validate the selection rather than silently ignoring it, but do not
        // build a half-populated session request for an unimplemented adapter.
        let registry = composition::load_registry(&args.registry)?;
        let descriptor = registry.get("agent")?;
        if &descriptor.id != agent {
            return Err(invalid(format!(
                "registry agent provider is '{}', not '{agent}'",
                descriptor.id
            )));
        }
        return Err(UmbraError::new(
            ErrorKind::NotImplemented,
            "run.agent",
            "agent adapter sessions are not implemented; run a command after `--`",
        ));
    }
    if args.argv.is_empty() {
        return Err(invalid(
            "a command is required: umbra run --registry <PATH> --experimental \
             -- /absolute/path/to/program [args...]",
        ));
    }
    let registry = composition::load_registry(&args.registry)?;
    let spec = composition::run_spec(args, registry, Some(Box::new(StderrStatus)))?;
    umbra_supervisor::run(spec)
}

/// Owned bytes for a Unix argument; no lossy UTF-8 conversion occurs.
pub(crate) fn os_bytes(value: &OsString) -> Vec<u8> {
    value.as_os_str().as_bytes().to_vec()
}

pub(crate) fn invalid(context: impl Into<String>) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidInput, "run.validate", context)
}
