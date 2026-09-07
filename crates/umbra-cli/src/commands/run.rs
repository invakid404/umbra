use std::{ffi::OsString, path::PathBuf};

use clap::Args;
use umbra_core::Result;

use crate::StorageArgs;

#[derive(Debug, Args)]
/// Run args.
pub struct RunArgs {
    /// Agent provider ID; bundled adapters are codex and claude.
    #[arg(long, default_value = "codex", value_name = "ID")]
    pub agent: String,
    /// Logical workspace directory for the agent.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub workspace: PathBuf,
    /// Agent arguments, preserved as OS strings after `--`.
    #[arg(last = true, value_name = "AGENT_ARGS")]
    pub agent_args: Vec<OsString>,
}

/// Handle.
pub fn handle(args: RunArgs, storage: &StorageArgs) -> Result<()> {
    super::not_implemented("run", &args, storage)
}
