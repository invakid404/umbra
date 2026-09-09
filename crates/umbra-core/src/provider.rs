//! Runtime provider registration and shared bounded framing. Role schemas live in trait crates.
use crate::{BytePath, ErrorKind, Result, UmbraError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Common transport version 2 adds required sandbox launch policy and rewrite messages.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum encoded frame size, checked before allocating a payload.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// Explicit installed executable; runtime paths/options never enter checkpoints.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    /// Id.
    pub id: String,
    /// Role.
    pub role: String,
    /// Wire protocol version required for this connection.
    pub protocol_version: u32,
    /// Explicit executable path; never resolved by scanning PATH.
    pub executable: BytePath,
    #[serde(default)]
    /// Supported behavior advertised by the provider; qualification is required.
    pub capabilities: BTreeSet<String>,
    #[serde(default)]
    /// Configuration or behavior options defined by the enclosing contract.
    pub options: Vec<u8>,
}

/// Explicit role selection, with no PATH discovery or closed backend identities.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRegistry {
    /// Providers.
    pub providers: BTreeMap<String, ProviderDescriptor>,
    #[serde(default = "default_timeout")]
    /// Timeout ms.
    pub timeout_ms: u64,
}
fn default_timeout() -> u64 {
    5000
}
impl ProviderRegistry {
    /// Validate.
    pub fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0 || self.timeout_ms > 60_000 {
            return Err(protocol_error("timeout must be in 1..=60000 milliseconds"));
        }
        for (role, descriptor) in &self.providers {
            descriptor.validate(role)?;
        }
        Ok(())
    }
    /// Get.
    pub fn get(&self, role: &str) -> Result<&ProviderDescriptor> {
        self.providers
            .get(role)
            .ok_or_else(|| protocol_error(format!("missing provider role: {role}")))
    }
}
impl ProviderDescriptor {
    /// Validate.
    pub fn validate(&self, role: &str) -> Result<()> {
        if self.id.is_empty()
            || self.role != role
            || self.protocol_version != PROTOCOL_VERSION
            || !self.executable.is_absolute()
        {
            return Err(protocol_error(
                "invalid provider identity, role, version, or executable path",
            ));
        }
        Ok(())
    }
}

/// Initial configuration; the server verifies role/version before constructing a backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    /// Id.
    pub id: String,
    /// Role.
    pub role: String,
    /// Version.
    pub version: u32,
    /// Required capabilities.
    pub required_capabilities: BTreeSet<String>,
    /// Configuration or behavior options defined by the enclosing contract.
    pub options: Vec<u8>,
}

/// Validated identity and actual qualified capabilities from a provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Welcome {
    /// Id.
    pub id: String,
    /// Role.
    pub role: String,
    /// Version.
    pub version: u32,
    /// Supported behavior advertised by the provider; qualification is required.
    pub capabilities: BTreeSet<String>,
}

/// Transport envelopes carry owned role-schema bytes, never Rust objects or pointers.
#[derive(Debug, Serialize, Deserialize)]
pub enum Frame {
    /// Request.
    Request {
        /// Id.
        id: u64,
        /// Payload.
        payload: Vec<u8>,
    },
    /// Response.
    Response {
        /// Id.
        id: u64,
        /// Result.
        result: Result<Vec<u8>>,
    },
    /// Callback.
    Callback {
        /// Id.
        id: u64,
        /// Token.
        token: u64,
        /// Payload.
        payload: Vec<u8>,
    },
    /// Callback result.
    CallbackResult {
        /// Id.
        id: u64,
        /// Token.
        token: u64,
        /// Result.
        result: Result<Vec<u8>>,
    },
}

/// Protocol error.
pub fn protocol_error(context: impl Into<String>) -> UmbraError {
    UmbraError::new(ErrorKind::ProtocolMismatch, "provider", context)
}
/// Encode.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|e| protocol_error(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(protocol_error("encoded value exceeds frame limit"));
    }
    Ok(bytes)
}
/// Decode.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(protocol_error("payload exceeds frame limit"));
    }
    serde_json::from_slice(bytes).map_err(|e| protocol_error(e.to_string()))
}

#[cfg(unix)]
mod transport;
#[cfg(unix)]
pub use transport::*;

/// Connection information passed as opaque namespace-provider options.
/// The alternative engine owns these role sessions; assembly does not create duplicates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamespaceConnections {
    /// Storage.
    pub storage: ProviderDescriptor,
    /// Journal.
    pub journal: ProviderDescriptor,
    /// Timeout ms.
    pub timeout_ms: u64,
    /// Configuration or behavior options defined by the enclosing contract.
    pub options: Vec<u8>,
}
