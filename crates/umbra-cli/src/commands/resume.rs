//! `umbra resume`: reopen an existing run and report what the last session left.
//!
//! This does not relaunch anything. Reopening classifies the run's journal and
//! says whether it can be reconciled; relaunching from a checkpoint would need
//! checkpoint-based recovery, which is not implemented. Like every other command
//! here, it asks no questions and reads no stdin for configuration.

use std::path::PathBuf;

use clap::Args;
use umbra_core::{ErrorKind, Result, RunId, UmbraError};
use umbra_supervisor::{ResumeOutcome, ResumeSpec, RunPersistence};

use crate::{composition, StorageArgs};

#[derive(Debug, Args)]
/// Resume args.
pub struct ResumeArgs {
    /// Explicit JSON registry of installed provider executables and options.
    /// This is the single source of storage configuration, exactly as for `run`.
    #[arg(long, value_name = "PATH")]
    pub registry: PathBuf,
    #[command(flatten)]
    /// Run.
    pub run: super::RunIdArgs,
    /// Host directory approved as this run's workspace, as it was at creation.
    /// The reopen fingerprints it and the storage backend validates that against
    /// the run's own manifest, so a workspace that has moved on is refused rather
    /// than silently reopened against a different base.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub workspace: PathBuf,
    /// Use explicitly configured local development storage instead of the
    /// default validated NFSv4 mount. Local storage is not remote durability.
    #[arg(long, conflicts_with = "strict_remote")]
    pub local_dev: bool,
    /// Require strict remote durability. No storage provider qualifies this yet,
    /// so this fails deterministically rather than promising it.
    #[arg(long)]
    pub strict_remote: bool,
}

impl ResumeArgs {
    /// Selected storage mode, chosen the same way `run` chooses it.
    pub fn persistence(&self) -> RunPersistence {
        match (self.local_dev, self.strict_remote) {
            (true, _) => RunPersistence::LocalDevelopment,
            (_, true) => RunPersistence::StrictRemote,
            _ => RunPersistence::NfsClientFsync,
        }
    }
}

/// Reopen the named run and report its recovery verdict.
///
/// A run that requires recovery exits nonzero, because a caller scripting this
/// must not read success from a run it cannot use. It is still a *diagnosis*, not
/// a malfunction, so it is reported as a status line and a structured error
/// rather than a failure of the reopen itself — the reopen worked; the run is
/// what is unusable.
pub fn handle(args: ResumeArgs, storage: &StorageArgs) -> Result<()> {
    if storage.storage_root.is_some() {
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            "resume.args",
            "--storage-root does not configure a run; set the storage provider's \
             physical root in the --registry descriptor options instead",
        ));
    }
    let registry = composition::load_registry(&args.registry)?;
    let workspace = std::fs::canonicalize(&args.workspace).map_err(|e| {
        UmbraError::new(
            ErrorKind::InvalidPath,
            "resume.workspace",
            format!("{}: {e}", args.workspace.display()),
        )
    })?;
    if !workspace.is_dir() {
        return Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "resume.workspace",
            format!("workspace {} is not a directory", workspace.display()),
        ));
    }
    let spec = ResumeSpec {
        registry,
        run_id: RunId(args.run.run_id),
        workspace,
        persistence: args.persistence(),
    };
    report(umbra_supervisor::resume(spec)?)
}

/// Status on stderr, verdict in the exit code.
fn report(outcome: ResumeOutcome) -> Result<()> {
    eprintln!(
        "umbra: run {} reopened at writer epoch {}; log reached sequence {}, \
         {} unfinished operation(s)",
        outcome.run_id.0, outcome.writer_epoch.0, outcome.last_valid_sequence.0, outcome.unfinished,
    );
    if !outcome.recovery_required {
        eprintln!("umbra: run {} requires no reconciliation", outcome.run_id.0);
        return Ok(());
    }
    eprintln!(
        "umbra: run {} requires recovery and cannot be served",
        outcome.run_id.0
    );
    // A structured error, so the exit code is nonzero and the reason is one line.
    // Reconciling the run is not implemented, and saying so here is better than a
    // status that reads like something the operator can act on.
    Err(UmbraError::new(
        ErrorKind::InvalidState,
        "resume",
        "run requires recovery: reconciling a reopened run is not implemented",
    ))
}
