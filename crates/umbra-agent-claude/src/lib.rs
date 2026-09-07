//! Claude Code adapter scaffold for declarative, supervised launch and resume.
//!
//! Configuration and temporary directories use logical paths. Project identity must
//! stay stable across physical checkout moves. A process wrapper must re-enter or
//! notify supervision; cooperative wrapping alone does not prove child capture.
//! Future plans must pin the exact CLI version, disable updates, retain session
//! history, and record session identity. Credentials remain external.
//!
//! No CLI behavior is qualified yet: this stub advertises no capabilities and all
//! fallible adapter methods return [`UmbraError::not_implemented`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_agent::{
    Agent, AgentCapabilities, AgentEvent, AgentLaunchPlan, AgentLaunchRequest, AgentResumeRequest,
    AgentSession, AgentSessionUpdate, AgentStopPlan,
};
use umbra_core::{Result, UmbraError};

/// Logical configuration and persisted session-history directory.
pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
/// Stable project-history bucket identity, independent of physical checkout paths.
pub const CLAUDE_CODE_PROJECT_DIR_NAME: &str = "CLAUDE_CODE_PROJECT_DIR_NAME";
/// Logical temporary directory within the supervised namespace.
pub const CLAUDE_CODE_TMPDIR: &str = "CLAUDE_CODE_TMPDIR";
/// Cooperative background-process wrapper that must re-enter or notify supervision.
pub const CLAUDE_CODE_PROCESS_WRAPPER: &str = "CLAUDE_CODE_PROCESS_WRAPPER";

/// Known configuration keys; their presence does not imply implemented support.
pub const CONFIGURATION_KEYS: [&str; 4] = [
    CLAUDE_CONFIG_DIR,
    CLAUDE_CODE_PROJECT_DIR_NAME,
    CLAUDE_CODE_TMPDIR,
    CLAUDE_CODE_PROCESS_WRAPPER,
];

/// Stateless Claude Code adapter stub. Construction performs no I/O.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeAgent;

impl Agent for ClaudeAgent {
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    fn launch_plan(&self, _request: &AgentLaunchRequest) -> Result<AgentLaunchPlan> {
        Err(UmbraError::not_implemented("claude.launch_plan"))
    }

    fn resume_plan(
        &self,
        _request: &AgentResumeRequest,
        _session: &AgentSession,
    ) -> Result<AgentLaunchPlan> {
        Err(UmbraError::not_implemented("claude.resume_plan"))
    }

    fn observe(&mut self, _event: &AgentEvent) -> Result<Option<AgentSessionUpdate>> {
        Err(UmbraError::not_implemented("claude.observe"))
    }

    fn stop_plan(&self, _session: &AgentSession) -> Result<AgentStopPlan> {
        Err(UmbraError::not_implemented("claude.stop_plan"))
    }
}
