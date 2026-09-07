//! Declarative coding-agent adapters for supervised, resumable runs.
//!
//! An adapter describes how a qualified agent version uses logical configuration,
//! state, and workspace paths. It never creates or controls a process itself.
//! The supervisor validates each plan, prepares state through the namespace service,
//! installs enforcement, and passes the resulting launch specification to tracing.
//!
//! Persistent session records and launch DTOs belong to [`umbra_core`]. Session
//! identity must survive moving the backing store to another physical mount point.
//! Persist the open provider ID, exact agent version, stable session ID, logical
//! state paths, and versioned adapter metadata; keep credentials and runtime mount
//! bindings out of that record.
//!
//! See the crate README for the adapter implementation and qualification walkthrough.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_core::Result;
pub use umbra_core::{
    AgentCapabilities, AgentConfigPaths, AgentEvent, AgentLaunchPlan, AgentLaunchRequest,
    AgentMetadata, AgentOutputStream, AgentResumeRequest, AgentSession, AgentSessionUpdate,
    AgentStateDirectory, AgentStopAction, AgentStopPlan,
};

/// Object-safe contract for an agent adapter selected by an open provider ID.
///
/// All process creation, signal delivery, and supervision belong to the supervisor.
/// Methods return owned descriptions or session observations without changing the
/// supervisor's run state. Unsupported behavior and incompatible session formats
/// must return structured [`umbra_core::UmbraError`] values, never silently start a
/// fresh session or launch an unsupervised process.
///
/// Configuration is expressed using logical byte paths in the shared DTOs. Common
/// HOME, TMPDIR, and XDG roots and adapter-specific configuration/state roots must
/// remain stable across handoff. Cooperative environment settings do not replace
/// tracing or the independent fail-closed sandbox.
pub trait Agent: Send {
    /// Report only capabilities qualified for the adapter's pinned agent version.
    fn capabilities(&self) -> AgentCapabilities;

    /// Describe a new session's argv, environment, logical cwd, and state directories.
    ///
    /// Include every root required for configuration, transcript/history retention,
    /// databases, and temporary files, with automatic updates disabled for the run.
    /// Do not create directories or spawn the agent here: namespace preparation and
    /// enforcement must complete before the supervisor executes the plan.
    fn launch_plan(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan>;

    /// Describe a new supervised process resuming the exact recorded session.
    ///
    /// Validate provider identity, exact agent version, session format and versioned
    /// metadata before returning a plan. Preserve the recorded session ID, logical
    /// state roots, project identity, and necessary history. Physical mount-root
    /// changes must not change those identities. Missing or incompatible evidence
    /// is an explicit error, never permission to select the newest session instead.
    fn resume_plan(
        &self,
        request: &AgentResumeRequest,
        session: &AgentSession,
    ) -> Result<AgentLaunchPlan>;

    /// Interpret bounded supervised output/session evidence or a lifecycle outcome.
    ///
    /// Return a proposed update for the supervisor to validate and persist. `None`
    /// means the event supplies no session update. Do not fabricate a session ID on
    /// discovery failure or treat an agent exit as proof of a durable checkpoint.
    fn observe(&mut self, event: &AgentEvent) -> Result<Option<AgentSessionUpdate>>;

    /// Describe an orderly stop for the recorded session without executing it.
    ///
    /// The supervisor coordinates quiescence, remaining descendants, transaction
    /// reconciliation, and durability before publishing a clean handoff checkpoint.
    fn stop_plan(&self, session: &AgentSession) -> Result<AgentStopPlan>;
}

// Keep dyn compatibility checked in every build without requiring a concrete backend.
const _: Option<&dyn Agent> = None;

/// Versioned provider protocol, server harness and trait proxy.
#[cfg(unix)]
pub mod provider;
