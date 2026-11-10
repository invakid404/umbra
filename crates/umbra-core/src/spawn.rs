//! Single-use spawn authorization vocabulary.
use crate::{TaskId, ThreadId};
use serde::{Deserialize, Serialize};

/// Identity of one stopped parent's spawn operation; never reused after exec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpawnToken {
    /// Provider run generation.
    pub run_generation: u64,
    /// Parent task, including its acquisition generation.
    pub parent: TaskId,
    /// Parent image generation.
    pub exec_generation: u64,
    /// Issuing thread, including its generation.
    pub thread: ThreadId,
    /// Monotonic sequence within the provider run.
    pub sequence: u64,
}

/// A captured action; unsupported opcodes retain their caller-order position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnAction {
    /// Close one kernel descriptor in the child.
    Close(i32),
    /// Duplicate a kernel descriptor onto another kernel descriptor number.
    Dup2(i32, i32),
    /// Explicitly retain a kernel descriptor across spawn.
    Inherit(i32),
    /// Refused opcode retained until ordered supervisor validation.
    Unsupported(i32),
}

impl SpawnAction {
    /// Exact refusal for an unsupported opcode, shared by policy and execution.
    pub fn unsupported_diagnostic(&self) -> Option<String> {
        let Self::Unsupported(opcode) = self else {
            return None;
        };
        let name = match opcode {
            0 => "OPEN".into(),
            4 => "FILEPORT_DUP2".into(),
            5 => "CHDIR".into(),
            6 => "FCHDIR".into(),
            other => other.to_string(),
        };
        Some(format!("spawn unsupported action {name}"))
    }
}

/// Immutable policy accompanying the platform's captured bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnPlan {
    /// SHA-256 of the normalized attribute and action snapshot.
    pub digest: [u8; 32],
    /// Ordered kernel actions; supervisor validates both sources and destinations.
    pub actions: Vec<SpawnAction>,
    /// Drop all logical descriptors; virtual explicit inheritance is refused.
    pub close_on_exec_default: bool,
}

/// Supervisor response bound to a particular snapshot and logical-table epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAuthorization {
    /// The single operation authorized.
    pub token: SpawnToken,
    /// Must match the captured plan's digest.
    pub digest: [u8; 32],
    /// Supervisor's logical-table epoch at validation.
    pub table_generation: u64,
}
