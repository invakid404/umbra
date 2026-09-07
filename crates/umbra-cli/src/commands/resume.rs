use std::{ffi::OsString, path::PathBuf};

use clap::Args;
use umbra_core::Result;

use crate::StorageArgs;

#[derive(Debug, Args)]
/// Resume args.
pub struct ResumeArgs {
    #[command(flatten)]
    /// Run.
    pub run: super::RunIdArgs,
    /// Logical workspace directory for the resumed agent.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub workspace: PathBuf,
    /// Additional agent arguments, preserved as OS strings after `--`.
    #[arg(last = true, value_name = "AGENT_ARGS")]
    pub agent_args: Vec<OsString>,
}

/// Handle.
pub fn handle(args: ResumeArgs, storage: &StorageArgs) -> Result<()> {
    super::not_implemented("resume", &args, storage)
}
