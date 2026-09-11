//! Shared journal values. Runtime bindings and writer credentials are not log payloads.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BytePath, LeaseEpoch, ObjectId, OperationId, OperationOutcome, PhysicalPath, RunId, Sequence,
};

/// Stable identity of an immutable snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointId(pub Uuid);

/// A backend receipt: records through `sequence` reached its persistence boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableSequence {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: LeaseEpoch,
    /// Monotonic position in the journal.
    pub sequence: Sequence,
}

/// Runtime-only directory on the selected backing store, hidden from tracees.
/// Serialization is for private provider IPC, never for persistent identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalControlBinding {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Directory.
    pub directory: PhysicalPath,
}

/// Required format versions; reject unsupported policies before writable opening.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFormatPolicy {
    /// Readable versions.
    pub readable_versions: Vec<u32>,
    /// Write version.
    pub write_version: u32,
}

/// Evidence supplied by the trusted storage/supervisor owner, not self-authorization.
/// Backends must validate evidence against the actual deployment. Lease expiry or
/// checking an epoch on new RPCs alone cannot fence old writable kernel handles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalFencingEvidence {
    /// Opaque evidence from a mechanism that disables all former-writer mutation.
    Fenced {
        /// Mechanism.
        mechanism: String,
        /// Evidence.
        evidence: Vec<u8>,
    },
    /// Confirmed termination and quiescence where effective fencing is unavailable.
    FormerWriterQuiesced {
        /// Evidence.
        evidence: Vec<u8>,
    },
}

/// Injected single-writer authority; storage owns acquisition, renewal and release.
/// Timestamps are Unix milliseconds and are diagnostic, not proof of safe takeover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalWriterAuthority {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Writer instance id.
    pub writer_instance_id: String,
    /// Host fingerprint.
    pub host_fingerprint: String,
    /// Supervisor version.
    pub supervisor_version: String,
    /// Acquired at.
    pub acquired_at: u64,
    /// Renewed at.
    pub renewed_at: u64,
    /// Lease epoch.
    pub lease_epoch: LeaseEpoch,
    /// Fencing.
    pub fencing: JournalFencingEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Journal access.
pub enum JournalAccess {
    /// Read only.
    ReadOnly,
    /// Writer.
    Writer(JournalWriterAuthority),
}

/// Configuration for one open session; contains no independent lease acquisition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalOpenRequest {
    /// Control.
    pub control: JournalControlBinding,
    /// Access.
    pub access: JournalAccess,
    /// Format.
    pub format: JournalFormatPolicy,
}

/// Process-independent mutation intent, resolved to stable objects and logical paths.
/// Paths are logical absolute byte paths, except symlink targets, which preserve
/// their original bytes (including relative targets). Never persist mount roots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalIntent {
    /// Copy up.
    CopyUp {
        /// Object.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
    },
    /// Create.
    Create {
        /// Object.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Directory.
        directory: bool,
        /// Mode.
        mode: u32,
    },
    /// Rename.
    Rename {
        /// Object.
        object: ObjectId,
        /// From.
        from: BytePath,
        /// To.
        to: BytePath,
    },
    /// Link.
    Link {
        /// Object.
        object: ObjectId,
        /// Existing.
        existing: BytePath,
        /// New path.
        new_path: BytePath,
    },
    /// Symlink.
    Symlink {
        /// Object.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Target.
        target: BytePath,
    },
    /// Unlink.
    Unlink {
        /// Object.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Directory.
        directory: bool,
    },
    /// Write.
    Write {
        /// Object.
        object: ObjectId,
        /// Absolute byte offset from the beginning of the object.
        offset: Option<u64>,
        /// Length.
        length: u64,
    },
    /// Truncate.
    Truncate {
        /// Object.
        object: ObjectId,
        /// Length.
        length: u64,
    },
    /// Chmod.
    Chmod {
        /// Object.
        object: ObjectId,
        /// Mode.
        mode: u32,
    },
    /// Chown.
    ///
    /// Unlike [`JournalIntent::Chmod`], this carries a path. Chmod and the other
    /// object-keyed intents run against an object already in the shadow, so their
    /// identity is stable; a chown may materialise its target as part of itself,
    /// which gives the shadow object a different identity from the `object`
    /// recorded here. The path is what stays resolvable across that change, the
    /// same reason [`JournalIntent::CopyUp`] carries one.
    Chown {
        /// Object. The pre-materialisation identity when `copy_up` is set.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Uid. `None` leaves the owner unchanged.
        uid: Option<u32>,
        /// Gid. `None` leaves the group unchanged.
        gid: Option<u32>,
        /// This operation copied the object up from the immutable base before
        /// changing its ownership, so a reader must expect a shadow object at
        /// `path` whose identity differs from `object`.
        copy_up: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Journal lifecycle.
pub enum JournalLifecycle {
    /// Started.
    Started,
    /// Recovery required.
    RecoveryRequired,
    /// Checkpoint published.
    CheckpointPublished {
        /// Request a logical checkpoint after the caller establishes quiescence.
        checkpoint: CheckpointId,
        /// Through.
        through: Sequence,
    },
    /// A fresh command run finished and its effects were made durable. This is
    /// not a resumable checkpoint and does not authorize writer takeover.
    RunCompleted {
        /// Through.
        through: Sequence,
    },
    /// Clean handoff.
    CleanHandoff {
        /// Request a logical checkpoint after the caller establishes quiescence.
        checkpoint: CheckpointId,
        /// Through.
        through: Sequence,
    },
}

/// Transaction stage. Prepare must be durable before irreversible effects when
/// recovery needs durable intent; abort does not assert that kernel writes vanished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalPayload {
    /// Prepare.
    Prepare {
        /// Intent.
        intent: JournalIntent,
    },
    /// Observed result.
    ObservedResult {
        /// Outcome.
        outcome: OperationOutcome,
    },
    /// Commit.
    Commit,
    /// Abort.
    Abort {
        /// Reason.
        reason: String,
    },
    /// Lifecycle.
    Lifecycle(JournalLifecycle),
}

/// Logical payload; a backend adds bounded frame length and CRC32 framing.
/// `append` assigns the stored sequence and returns it; the input sequence is not
/// authority to overwrite an existing frame. Lifecycle records also have an ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// Format version.
    pub format_version: u32,
    /// Monotonic position in the journal.
    pub sequence: Sequence,
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: LeaseEpoch,
    /// Payload.
    pub payload: JournalPayload,
}

/// Compatibility evidence required when restoring a run on a different machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFingerprints {
    /// Base.
    pub base: String,
    /// Toolchain.
    pub toolchain: String,
    /// Agent provider id.
    pub agent_provider_id: String,
    /// Agent version.
    pub agent_version: String,
    /// Supervisor version.
    pub supervisor_version: String,
}

/// Logical object identity, including hard links sharing an object ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Path interpreted according to the enclosing operation and path type.
    pub path: BytePath,
    /// Object.
    pub object: ObjectId,
}

/// Versioned logical state only. Extension metadata must not contain physical mount
/// roots, pointers, runtime fd numbers, live process state or external credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalLogicalState {
    /// Format version.
    pub format_version: u32,
    /// Entries.
    pub entries: Vec<CheckpointEntry>,
    /// Whiteouts.
    pub whiteouts: Vec<BytePath>,
    /// Agent session id.
    pub agent_session_id: String,
    /// Agent state paths.
    pub agent_state_paths: Vec<BytePath>,
    /// Metadata.
    pub metadata: Vec<u8>,
}

/// Immutable snapshot. `clean` is a supervisor assertion requiring quiescence,
/// finalized transactions and tracee/storage/manifest durability, not just log flush.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Format version.
    pub format_version: u32,
    /// Id.
    pub id: CheckpointId,
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: LeaseEpoch,
    /// Last committed.
    pub last_committed: DurableSequence,
    /// Fingerprints.
    pub fingerprints: JournalFingerprints,
    /// State.
    pub state: JournalLogicalState,
    /// Clean.
    pub clean: bool,
}

/// Unfinished operation for explicit, idempotent namespace reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalPendingOperation {
    /// Stable operation identity used for ordering and reconciliation.
    pub operation_id: OperationId,
    /// Fencing generation for the writer authorizing this operation.
    pub writer_epoch: LeaseEpoch,
    /// Prepared at.
    pub prepared_at: Sequence,
    /// Intent.
    pub intent: JournalIntent,
    /// Observed result.
    pub observed_result: Option<OperationOutcome>,
}

/// Only an incomplete final frame is recoverable by tail exclusion/repair.
/// Complete checksum failure and interior corruption are structured errors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalTailRecovery {
    /// Intact.
    Intact,
    /// Incomplete final frame.
    IncompleteFinalFrame {
        /// Valid bytes.
        valid_bytes: u64,
        /// Discarded bytes.
        discarded_bytes: u64,
        /// Repaired.
        repaired: bool,
    },
}

/// Validated recovery inventory; readable frames do not imply a prior flush receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryState {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Request a logical checkpoint after the caller establishes quiescence.
    pub checkpoint: Option<Checkpoint>,
    /// Last valid sequence.
    pub last_valid_sequence: Sequence,
    /// None when no acknowledged durability boundary can be established.
    pub durable: Option<DurableSequence>,
    /// Pending.
    pub pending: Vec<JournalPendingOperation>,
    /// Tail.
    pub tail: JournalTailRecovery,
    /// False until unfinished effects and an unclean prior session are reconciled.
    pub clean: bool,
}
