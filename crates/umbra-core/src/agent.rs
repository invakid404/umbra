//! Shared declarative agent plans and versioned persistent session records.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{BytePath, ExitStatus, LaunchSpec};

/// Open capability names qualified by the selected provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// Supported.
    pub supported: BTreeSet<String>,
}

/// Logical roots common to every agent. These are never physical mount bindings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfigPaths {
    /// Home.
    pub home: BytePath,
    /// Tmp dir.
    pub tmp_dir: BytePath,
    /// Xdg config home.
    pub xdg_config_home: BytePath,
    /// Xdg cache home.
    pub xdg_cache_home: BytePath,
    /// Xdg state home.
    pub xdg_state_home: BytePath,
    /// Open names for configuration, database, history, and other adapter roots.
    /// Adapters map these names to their qualified CLI environment settings.
    pub adapter_roots: BTreeMap<String, BytePath>,
}

/// Requirements for a new session at a pinned agent version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchRequest {
    /// Provider id.
    pub provider_id: String,
    /// Agent version.
    pub agent_version: String,
    /// Requested executable, logical workspace, arguments, environment, and policy.
    pub launch: LaunchSpec,
    /// Config paths.
    pub config_paths: AgentConfigPaths,
    /// Opaque, non-secret provider configuration; interpreted only by the adapter.
    pub options: BTreeMap<String, Vec<u8>>,
}

/// Destination requirements. The recorded session remains authoritative for identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResumeRequest {
    /// Launch.
    pub launch: AgentLaunchRequest,
}

/// A logical directory the supervisor must prepare before executing a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStateDirectory {
    /// Path interpreted according to the enclosing operation and path type.
    pub path: BytePath,
    /// Retain in the run shadow for handoff (including databases and transcripts).
    pub required_for_resume: bool,
}

/// Declarative command executed only through supervision after validation.
/// Credentials remain external; do not embed secrets in this plan or metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchPlan {
    /// Launch.
    pub launch: LaunchSpec,
    /// Config paths.
    pub config_paths: AgentConfigPaths,
    /// State directories.
    pub state_directories: Vec<AgentStateDirectory>,
    /// Must be true for a resumable run; the adapter configures the qualified CLI.
    pub automatic_updates_disabled: bool,
}

/// Adapter-owned, non-secret metadata with an explicit format version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetadata {
    /// Format version.
    pub format_version: u32,
    /// Payload.
    pub payload: Vec<u8>,
}

/// Persistent, process-independent identity for a resumable agent session.
///
/// Serialize this record through Serde, retaining session_id exactly (no UUID
/// parsing, case folding, or "latest session" alias). The supervisor validates the
/// record and publishes it durably with the run checkpoint. Unknown format versions
/// or incompatible provider/agent versions must be rejected before resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Format version.
    pub format_version: u32,
    /// Provider id.
    pub provider_id: String,
    /// Agent version.
    pub agent_version: String,
    /// Session id.
    pub session_id: String,
    /// Workspace.
    pub workspace: BytePath,
    /// Config paths.
    pub config_paths: AgentConfigPaths,
    /// State directories.
    pub state_directories: Vec<AgentStateDirectory>,
    /// Stable project-history identity when the agent uses project-scoped sessions.
    pub project_id: Option<String>,
    /// Metadata.
    pub metadata: AgentMetadata,
}

/// A proposed complete record replacement, validated and persisted by the supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionUpdate {
    /// Session.
    pub session: AgentSession,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Agent output stream.
pub enum AgentOutputStream {
    /// Stdout.
    Stdout,
    /// Stderr.
    Stderr,
}

/// Evidence delivered by supervision, never an adapter's untraced reader.
/// The caller enforces negotiated byte limits before allocation and delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// Output.
    Output {
        /// Stream.
        stream: AgentOutputStream,
        /// Owned bytes; no UTF-8 conversion is implied.
        bytes: Vec<u8>,
    },
    /// Session evidence.
    SessionEvidence {
        /// Source.
        source: String,
        /// Owned bytes; no UTF-8 conversion is implied.
        bytes: Vec<u8>,
    },
    /// Started.
    Started,
    /// Exited.
    Exited(ExitStatus),
}

/// Orderly actions interpreted by the supervisor, never executed by the adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStopAction {
    /// Close stdin.
    CloseStdin,
    /// Write stdin.
    WriteStdin(Vec<u8>),
    /// Interrupt.
    Interrupt,
    /// Terminate.
    Terminate,
}

/// Bounded orderly shutdown; expiry returns control to supervisor termination policy.
/// Successful execution alone is not evidence of durable handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStopPlan {
    /// Actions.
    pub actions: Vec<AgentStopAction>,
    /// Grace period ms.
    pub grace_period_ms: u64,
}
