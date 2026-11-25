//! Serializable staged-run inputs; observer ownership remains with the caller.
use crate::run::{RunLaunch, RunObserver, RunPersistence, RunSpec};
use std::path::PathBuf;
use umbra_core::{provider::ProviderRegistry, ErrorKind, Result, UmbraError};

/// Versioned input carried by registered executable worker startup.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRunInput {
    version: u32,
    run_id: umbra_core::RunId,
    registry: ProviderRegistry,
    launch: RunLaunch,
    workspace: PathBuf,
    persistence: RunPersistence,
    experimental: bool,
    state_root: PathBuf,
}

impl ManagedRunInput {
    /// Move serializable inputs out while retaining callbacks in the caller.
    pub fn split(spec: RunSpec) -> (Self, Option<Box<dyn RunObserver>>) {
        (
            Self {
                version: 1,
                run_id: umbra_core::RunId(uuid::Uuid::new_v4()),
                registry: spec.registry,
                launch: spec.launch,
                workspace: spec.workspace,
                persistence: spec.persistence,
                experimental: spec.experimental,
                state_root: spec.state_root,
            },
            spec.observer,
        )
    }

    /// Encode a bounded startup payload without opening any run resources.
    pub fn encode(&self) -> Result<Vec<u8>> {
        umbra_core::provider::encode(self)
    }

    /// Decode the exact supported schema before staged worker setup.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let input: Self = umbra_core::provider::decode(bytes)?;
        if input.version != 1 {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "managed.input",
                "unsupported startup version",
            ));
        }
        if input.run_id.0.is_nil() {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "managed.input",
                "missing storage run identity",
            ));
        }
        Ok(input)
    }

    /// Caller-created storage identity shared with the relay's Armed record.
    pub fn run_id(&self) -> umbra_core::RunId {
        self.run_id
    }

    /// Resolve the same platform descriptor in cleanup-owner and worker roles.
    pub fn platform(&self) -> Result<&umbra_core::provider::ProviderDescriptor> {
        self.registry.get("platform")
    }

    /// Reconstruct worker inputs with an explicit observer-protocol adapter.
    pub fn into_spec(self, observer: Option<Box<dyn RunObserver>>) -> RunSpec {
        RunSpec {
            registry: self.registry,
            launch: self.launch,
            workspace: self.workspace,
            persistence: self.persistence,
            experimental: self.experimental,
            state_root: self.state_root,
            observer,
        }
    }
}
