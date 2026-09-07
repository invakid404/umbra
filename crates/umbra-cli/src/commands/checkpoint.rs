use umbra_core::Result;

use super::RunIdArgs;
use crate::StorageArgs;

/// Handle.
pub fn handle(args: RunIdArgs, storage: &StorageArgs) -> Result<()> {
    super::not_implemented("checkpoint", &args, storage)
}
