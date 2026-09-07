//! Exercise the same provider connection factories used by supervisor assembly.
use crate::composition::{build_supervisor, load_registry};
use clap::Args;
use umbra_core::{Result, RunId};

#[derive(Debug, Args)]
/// Provider args.
pub struct ProviderArgs {
    /// Explicit JSON registry containing installed executable paths and opaque options.
    #[arg(long)]
    pub registry: std::path::PathBuf,
    /// Check only this role; omit to assemble all required roles through the supervisor.
    #[arg(long)]
    pub role: Option<String>,
}

/// Handle.
pub fn handle(args: ProviderArgs) -> Result<()> {
    let registry = load_registry(&args.registry)?;
    if let Some(role) = args.role {
        check_role(&registry, &role)?;
        println!("connected {role} provider {}", registry.get(&role)?.id);
    } else {
        let supervisor = build_supervisor(RunId(uuid::Uuid::new_v4()), &registry)?;
        println!(
            "provider assembly ready: {:?}",
            supervisor.state().lifecycle
        );
    }
    Ok(())
}
#[cfg(unix)]
fn check_role(registry: &umbra_core::provider::ProviderRegistry, role: &str) -> Result<()> {
    let descriptor = registry.get(role)?;
    let timeout = registry.timeout_ms;
    // Roles describe contracts, not a closed list of backend implementations.
    match role {
        "platform" => {
            umbra_platform::provider::connect(descriptor, timeout)?;
        }
        "storage" => {
            umbra_storage::provider::Proxy::connect(descriptor, timeout)?;
        }
        "journal" => {
            umbra_journal::provider::Proxy::connect(descriptor, timeout)?;
        }
        "agent" => {
            umbra_agent::provider::Proxy::connect(descriptor, timeout)?;
        }
        "namespace" => {
            umbra_overlay::provider::Proxy::connect(descriptor, timeout)?;
        }
        _ => {
            return Err(umbra_core::provider::protocol_error(
                "unknown provider role",
            ))
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_role(_registry: &umbra_core::provider::ProviderRegistry, _role: &str) -> Result<()> {
    Err(umbra_core::UmbraError::new(
        umbra_core::ErrorKind::UnsupportedCapability,
        "providers",
        "local provider transport requires Unix",
    ))
}
