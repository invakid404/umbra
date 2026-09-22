//! Runtime provider registration and shared bounded framing. Role schemas live in trait crates.
use crate::{BytePath, ErrorKind, Result, UmbraError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Common transport version 2 adds required sandbox launch policy and rewrite messages.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum encoded frame size, checked before allocating a payload.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// Default per-request IPC deadline, in milliseconds, when a registry omits
/// `timeout_ms`. This is the authoritative bound on every provider call (see
/// [`Connection`]): storage backends size their in-backend `PROBE_TIMEOUT` and
/// `LAYOUT_TIMEOUT` to fit inside it, and each backend compile-time-asserts that
/// they, plus a framing margin, do not exceed it.
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
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
    /// Authoritative per-request IPC deadline, in milliseconds, applied to every
    /// provider call on the connection. Defaults to [`DEFAULT_TIMEOUT_MS`] when
    /// omitted and must lie in `1..=60000` (see [`ProviderRegistry::validate`]).
    ///
    /// It is the outer bound on the whole request: a storage backend's own
    /// in-backend bounds (`PROBE_TIMEOUT` + `LAYOUT_TIMEOUT` + a framing margin) are
    /// sized to sum inside the default so their own outcome surfaces before the
    /// transport gives up. A value below roughly that sum (~4.5s) shadows those
    /// in-backend bounds, making this deadline win instead.
    pub timeout_ms: u64,
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_matches_the_authoritative_constant() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 5000);
        assert_eq!(default_timeout(), DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn a_registry_without_timeout_ms_deserializes_to_the_default_deadline() {
        let registry: ProviderRegistry = serde_json::from_str(r#"{"providers":{}}"#).unwrap();
        assert_eq!(registry.timeout_ms, DEFAULT_TIMEOUT_MS);
    }
}
