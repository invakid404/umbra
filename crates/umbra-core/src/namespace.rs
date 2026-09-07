//! Namespace transaction and checkpoint values shared by providers and supervisor.
use crate::{
    CheckpointId, JournalFingerprints, JournalLogicalState, OperationId, ResolvedAction, Sequence,
    UmbraError,
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
