//! Shared platform lifecycle values; no native tracing implementation types.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Architecture, ProcessHandle, TaskId};

/// Qualified runtime support reported by a platform provider.
///
/// Capability names are open strings, not backend identities. Consumers must
/// reject missing required capabilities; a declaration alone is not qualification
/// evidence. The provider negotiates concrete ABI encodings for these architectures.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Architectures.
    pub architectures: Vec<Architecture>,
    /// Supported behavior advertised by the provider; qualification is required.
    /// ABI identities use `<platform>-<arch>-abi-v<decimal version>`; a run
    /// requires exactly one such name, independent of other capability names.
    pub capabilities: BTreeSet<String>,
}

/// Runtime acknowledgement that the entire supervised tree is stopped.
///
/// The backend rejects new children and mutations before returning this value,
/// and keeps every listed task at a known transaction boundary until an explicit
/// control operation. This is not a persistent checkpoint or a migration handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuiescedTree {
    /// Process.
    pub process: ProcessHandle,
    /// Tasks.
    pub tasks: Vec<TaskId>,
}

/// How to terminate a supervised tree while retaining the enforcement boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminationPolicy {
    /// Allow orderly exit for at most this duration, then forcibly terminate all
    /// remaining descendants. Enforcement remains active throughout the grace period.
    Graceful {
        /// Timeout ms.
        timeout_ms: u64,
    },
    /// Forcibly terminate the complete tree without an orderly-exit grace period.
    Immediate,
}
