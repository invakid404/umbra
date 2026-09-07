//! Runtime assembly from explicitly installed provider executables, with no backend linking.
use umbra_core::{
    provider::{decode, ProviderRegistry},
    Result, RunId,
};
use umbra_supervisor::Supervisor;

/// Load bounded registry data; provider options remain opaque to assembly.
pub fn load_registry(path: &std::path::Path) -> Result<ProviderRegistry> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| {
        umbra_core::UmbraError::new(umbra_core::ErrorKind::Io, "registry.open", e.to_string())
    })?;
    let mut bytes = Vec::new();
    file.take(umbra_core::provider::MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            umbra_core::UmbraError::new(umbra_core::ErrorKind::Io, "registry.read", e.to_string())
        })?;
    let registry: ProviderRegistry = decode(&bytes)?;
    registry.validate()?;
    Ok(registry)
}

/// Connect every required role before handing the contracts to the supervisor.
#[cfg(unix)]
pub fn build_supervisor(run_id: RunId, registry: &ProviderRegistry) -> Result<Supervisor> {
    registry.validate()?;
    let timeout = registry.timeout_ms;
    let platform = umbra_platform::provider::connect(registry.get("platform")?, timeout)?;
    let agent = Box::new(umbra_agent::provider::Proxy::connect(
        registry.get("agent")?,
        timeout,
    )?);
    let namespace: Box<dyn umbra_overlay::NamespaceSession + Send> =
        if let Some(descriptor) = registry.providers.get("namespace") {
            Box::new(umbra_overlay::provider::Proxy::connect(
                descriptor, timeout,
            )?)
        } else {
            let storage = Box::new(umbra_storage::provider::Proxy::connect(
                registry.get("storage")?,
                timeout,
            )?);
            let journal = Box::new(umbra_journal::provider::Proxy::connect(
                registry.get("journal")?,
                timeout,
            )?);
            umbra_overlay::standard_namespace(storage, journal)
        };
    Ok(Supervisor::with_namespace(
        run_id, platform, namespace, agent,
    ))
}

/// Unsupported hosts return an explicit error without importing Unix backend types.
#[cfg(not(unix))]
pub fn build_supervisor(_run_id: RunId, _registry: &ProviderRegistry) -> Result<Supervisor> {
    Err(umbra_core::UmbraError::new(
        umbra_core::ErrorKind::UnsupportedCapability,
        "providers",
        "local provider transport requires Unix",
    ))
}
