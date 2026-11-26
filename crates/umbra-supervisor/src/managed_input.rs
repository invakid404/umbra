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
    /// Build the same-executable dispatch payload before any run resource opens.
    pub fn registered_launch(
        &self,
        external_caller: bool,
        capacity: u64,
    ) -> Result<umbra_core::managed::LaunchConfiguration> {
        let launch = umbra_core::managed::LaunchConfiguration {
            run: umbra_core::managed::RunIdentity {
                nonce: uuid::Uuid::new_v4(),
                generation: 1,
            },
            capacity,
            dispatch: "umbra_run_v1".to_owned(),
            payload: umbra_core::provider::encode(&(external_caller, self.encode()?))?,
        };
        launch.validate().map_err(|message| {
            UmbraError::new(ErrorKind::ProtocolMismatch, "managed.input", message)
        })?;
        Ok(launch)
    }

    /// Decode only the registered run schema, preserving the original storage ID.
    pub fn from_registered(
        launch: &umbra_core::managed::LaunchConfiguration,
    ) -> Result<(bool, Self)> {
        launch.validate().map_err(|message| {
            UmbraError::new(ErrorKind::ProtocolMismatch, "managed.input", message)
        })?;
        if launch.dispatch != "umbra_run_v1" {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "managed.input",
                "unregistered executable dispatch",
            ));
        }
        let (external, bytes): (bool, Vec<u8>) = umbra_core::provider::decode(&launch.payload)?;
        Ok((external, Self::decode(&bytes)?))
    }

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

    /// Registry RPC timeout shared by the owner handshake and worker client.
    pub fn timeout_ms(&self) -> u64 {
        self.registry.timeout_ms
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
