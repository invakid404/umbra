//! Stub Codex adapter for declarative, supervised launch and session resumption.
//!
//! Configuration describes logical state roots and a pinned CLI invocation. Common
//! HOME, TMPDIR, XDG roots and workspace cwd come from the shared launch request.
//! Session IDs, exact versions, roots and versioned metadata belong in `AgentSession`.
//! No process is launched and no capability is qualified by this stub.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use serde::{Deserialize, Serialize};

use umbra_agent::{
    Agent, AgentCapabilities, AgentEvent, AgentLaunchPlan, AgentLaunchRequest, AgentResumeRequest,
    AgentSession, AgentSessionUpdate, AgentStopPlan,
};
use umbra_core::{BytePath, Result, UmbraError};

/// Open provider identifier for runtime registration.
pub const PROVIDER_ID: &str = "codex";
/// Configuration/state root, including the `sessions` transcript directory.
pub const CODEX_HOME: &str = "CODEX_HOME";
/// Separately persisted SQLite state root.
pub const CODEX_SQLITE_HOME: &str = "CODEX_SQLITE_HOME";
/// Keys used in `AgentConfigPaths::adapter_roots` and the planned environment.
pub const CONFIGURATION_KEYS: &[&str] = &[CODEX_HOME, CODEX_SQLITE_HOME];

/// Interactive CLI command prefixes, excluding the configured executable/argv[0].
///
/// Launch shape: `codex [launch arguments] [prompt]`.
/// Resume shape: `codex resume [resume arguments] <recorded session ID> [prompt]`.
/// Resume must use the exact `AgentSession::session_id`, never a picker or `--last`.
/// Exact flags and compatibility still require qualification for the pinned version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCommandShape {
    /// Launch prefix.
    pub launch_prefix: Vec<Vec<u8>>,
    /// Resume prefix.
    pub resume_prefix: Vec<Vec<u8>>,
}

impl Default for CodexCommandShape {
    fn default() -> Self {
        Self {
            launch_prefix: Vec::new(),
            resume_prefix: vec![b"resume".to_vec()],
        }
    }
}

/// Non-secret configuration for the intended Codex launch/resume implementation.
///
/// All paths are logical byte paths, never physical backing-store mount roots.
/// Plans must reconcile these values with the shared request and recorded session;
/// construction alone does not validate them or prepare any state directories.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexConfig {
    /// Explicit executable path, resolved within the supervised namespace.
    pub executable: BytePath,
    /// Exact CLI version to validate and persist, not a version range.
    pub agent_version: String,
    /// Maps to `CODEX_HOME`; retain its session transcripts for handoff.
    pub codex_home: BytePath,
    /// Maps to `CODEX_SQLITE_HOME`; must be explicitly persisted for handoff.
    pub codex_sqlite_home: BytePath,
    /// Command.
    pub command: CodexCommandShape,
    /// Required to be true for resumable runs; vendor settings are not yet applied.
    pub preserve_history: bool,
    /// Required to be true for resumable runs; vendor settings are not yet applied.
    pub automatic_updates_disabled: bool,
}

/// Codex-specific adapter state. Process control remains with the supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexAgent {
    /// Config.
    pub config: CodexConfig,
}

impl CodexAgent {
    /// Store configuration without I/O, launching, or claiming version validation.
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }
}

impl Agent for CodexAgent {
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    fn launch_plan(&self, _request: &AgentLaunchRequest) -> Result<AgentLaunchPlan> {
        Err(UmbraError::not_implemented("codex.launch_plan"))
    }

    fn resume_plan(
        &self,
        _request: &AgentResumeRequest,
        _session: &AgentSession,
    ) -> Result<AgentLaunchPlan> {
        Err(UmbraError::not_implemented("codex.resume_plan"))
    }

    fn observe(&mut self, _event: &AgentEvent) -> Result<Option<AgentSessionUpdate>> {
        Err(UmbraError::not_implemented("codex.observe"))
    }

    fn stop_plan(&self, _session: &AgentSession) -> Result<AgentStopPlan> {
        Err(UmbraError::not_implemented("codex.stop_plan"))
    }
}
