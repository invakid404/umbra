//! Namespace transaction and checkpoint values shared by providers and supervisor.
use crate::{
    CheckpointId, DurabilityReceipt, ExitStatus, JournalFingerprints, JournalLogicalState,
    OperationId, ResolvedAction, RunId, Sequence, UmbraError,
};
use serde::{Deserialize, Serialize};

/// Runtime executable action after journaled preparation; never persistent identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedAction {
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Action.
    pub action: ResolvedAction,
}

/// Confirmation of committed namespace metadata, distinct from a durability receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitReceipt {
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Monotonic position in the journal.
    pub sequence: Sequence,
}

/// Why an operation must be reconciled or abandoned; does not assert rollback.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AbortReason {
    /// Cancelled.
    Cancelled,
    /// Failed.
    Failed(UmbraError),
    /// Recovery required.
    RecoveryRequired(String),
}

/// Logical checkpoint input excluding physical roots and live process state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointRequest {
    /// Id.
    pub id: CheckpointId,
    /// Fingerprints.
    pub fingerprints: JournalFingerprints,
    /// State.
    pub state: JournalLogicalState,
}

/// Ask the namespace owner to durably complete a run and release its authority.
///
/// This is the fresh-command-run end state, not a resumable clean checkpoint: it
/// records that the supervised tree finished and that the run's data and journal
/// reached their persistence boundary. It publishes no checkpoint and authorizes
/// no takeover.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishRunRequest {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Normalized root-process outcome, absent when the root never launched.
    pub root_status: Option<ExitStatus>,
    /// Supervised processes observed exiting, including the root.
    pub processes_exited: u64,
}

/// Evidence that a run's data, journal and writer authority reached their end
/// state. A receipt is returned only after every one of those steps succeeded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishRunReceipt {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Backing-store durability boundary actually reached for this run's data.
    pub durability: DurabilityReceipt,
    /// Journal sequence of the durable completion record.
    pub completed_through: Sequence,
}

/// Ask the namespace owner to leave a run explicitly failed.
///
/// No completion record is written and no checkpoint is published. The writer
/// lease is released only when `tree_terminated` proves that nothing can still
/// mutate the run; otherwise the writer marker is deliberately retained so a
/// later writer cannot take over an uncertain run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedRunRequest {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Diagnostic reason preserved alongside the primary error.
    pub reason: String,
    /// Independently established evidence that no tracee remains that could write.
    pub tree_terminated: bool,
}
