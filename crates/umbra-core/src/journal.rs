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
    /// and the shadow object's identity may then differ from the `object`
    /// recorded here. It need not: materialising a logical symlink preserves the
    /// base identity, so `copy_up` can be set while `object` still names the
    /// object the kernel chowns. The path is what stays resolvable either way,
    /// the same reason [`JournalIntent::CopyUp`] carries one.
    Chown {
        /// Object. The pre-materialisation identity when `copy_up` is set, which
        /// the materialised object may or may not still carry.
        object: ObjectId,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Uid. `None` leaves the owner unchanged.
        uid: Option<u32>,
        /// Gid. `None` leaves the group unchanged.
        gid: Option<u32>,
        /// This operation copied the object up from the immutable base before
        /// changing its ownership, so a reader must resolve `path` rather than
        /// assume `object` still locates what was chowned.
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
    /// A prior session journaled
    /// [`JournalLifecycle::RecoveryRequired`]: it declared this run unrecoverable
    /// and did not undo what it had already materialised.
    ///
    /// Distinct from `clean`, and the distinction is the whole reason this field
    /// exists. `clean` is *derived* from the shape of the inventory — unfinished
    /// operations, or a damaged tail — so it answers "is there something here to
    /// reconcile". This is *declared*: a session that could not account for a
    /// transaction's effects said so before it stopped, and it is not derivable
    /// from anything else in this struct, because the same session then wrote the
    /// terminal record that removes that transaction from `pending`. Recovering
    /// it from the inventory is therefore impossible by construction, which is
    /// what makes a carrier necessary rather than merely convenient
    /// ([#65](https://github.com/invakid404/umbra/issues/65), waiver (viii)).
    ///
    /// Deliberately no `#[serde(default)]`. This struct crosses the journal
    /// provider IPC as `Response::Open`, and a default would make a *missing*
    /// field decode as `false` — an older provider, whose replay cannot see the
    /// record at all, would silently report a run as recoverable. Without one the
    /// decode fails and the caller gets `ProtocolMismatch`, which is the repo's
    /// stated stance for a peer built against an older crate: handshake, then
    /// fail to decode. The absence of the attribute is load-bearing.
    pub recovery_required: bool,
    /// False until unfinished effects and an unclean prior session are reconciled.
    pub clean: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{decode, encode};
    use crate::{CheckpointId, Errno, ErrorKind, OperationOutcome};

    /// The journal log's on-disk bytes, pinned literally, for every
    /// `JournalPayload` variant.
    ///
    /// [#65](https://github.com/invakid404/umbra/issues/65) made a crashed run's
    /// journal *readable* -- `Overlay::bind` classifies `RecoveryState.pending`
    /// instead of refusing it -- and did so with **no new payload variant and no
    /// `FORMAT_VERSION` bump**, because `prepare` already fsyncs its `Prepare`
    /// record before any creating arm runs and that record is the durable
    /// pre-image. This test is the proof obligation that comes with that claim:
    /// the format is unchanged, so a journal written before #65 replays under a
    /// binary built after it, and vice versa.
    ///
    /// Why literals and not a round-trip. Both directions of a round-trip run
    /// through the same derived `Serialize`/`Deserialize`, so between them they
    /// catch an *asymmetric* change and nothing else; renaming a field,
    /// reordering a variant or retagging an enum stays green while invalidating
    /// every log on disk. Same argument, same remedy as
    /// `umbra-overlay`'s `lifecycle_wire_pairs_round_trip_and_re_encode_byte_identically`
    /// -- but for bytes that outlive a process rather than bytes that cross one
    /// wire, so the consequence of drift is a log that cannot be read rather than
    /// a handshake that fails.
    ///
    /// There is no version to hide behind, and that is deliberate. serde rejects
    /// an unknown enum variant, `umbra-journal-file`'s `read_frame` maps that to
    /// `CorruptJournal`, and a frame whose `format_version` disagrees is rejected
    /// outright -- so a format change is fail-closed either way. It is just
    /// fail-closed reported as *corruption*, which is the wrong word for "written
    /// by a newer umbra". Changing a literal here therefore means deciding to
    /// bump `FORMAT_VERSION` (`umbra-journal-file`, the bare `format_version: 1`
    /// in `umbra-overlay`'s `Overlay::record`, and the policy in the
    /// supervisor's `run.rs`), not merely re-running the test.
    #[test]
    fn every_journal_payload_encodes_to_the_bytes_the_log_already_holds() {
        let operation_id = OperationId(Uuid::from_u128(0x5150));
        let object = ObjectId(Uuid::from_u128(0x2323));
        let framed = |payload: JournalPayload| JournalRecord {
            format_version: 1,
            sequence: Sequence(7),
            operation_id,
            writer_epoch: LeaseEpoch(3),
            payload,
        };
        // `path` is a `BytePath`, which serializes as `Vec<u8>` rather than a
        // string: paths are bytes, not UTF-8, and the array form is what makes
        // that true on disk as well as in the type.
        let cases: [(JournalPayload, &[u8]); 5] = [
            (
                JournalPayload::Prepare {
                    intent: JournalIntent::Create {
                        object,
                        path: BytePath::new(b"/f".to_vec()).unwrap(),
                        directory: false,
                        mode: 0o644,
                    },
                },
                br#"{"format_version":1,"sequence":7,"operation_id":"00000000-0000-0000-0000-000000005150","writer_epoch":3,"payload":{"Prepare":{"intent":{"Create":{"object":"00000000-0000-0000-0000-000000002323","path":[47,102],"directory":false,"mode":420}}}}}"#,
            ),
            (
                JournalPayload::ObservedResult {
                    outcome: OperationOutcome::Failure(Errno(28)),
                },
                br#"{"format_version":1,"sequence":7,"operation_id":"00000000-0000-0000-0000-000000005150","writer_epoch":3,"payload":{"ObservedResult":{"outcome":{"Failure":28}}}}"#,
            ),
            (
                JournalPayload::Commit,
                br#"{"format_version":1,"sequence":7,"operation_id":"00000000-0000-0000-0000-000000005150","writer_epoch":3,"payload":"Commit"}"#,
            ),
            (
                JournalPayload::Abort {
                    reason: "KernelRefused(Errno(28))".into(),
                },
                br#"{"format_version":1,"sequence":7,"operation_id":"00000000-0000-0000-0000-000000005150","writer_epoch":3,"payload":{"Abort":{"reason":"KernelRefused(Errno(28))"}}}"#,
            ),
            (
                JournalPayload::Lifecycle(JournalLifecycle::RunCompleted {
                    through: Sequence(7),
                }),
                br#"{"format_version":1,"sequence":7,"operation_id":"00000000-0000-0000-0000-000000005150","writer_epoch":3,"payload":{"Lifecycle":{"RunCompleted":{"through":7}}}}"#,
            ),
        ];
        for (payload, golden) in cases {
            let record = framed(payload);
            let encoded = encode(&record).unwrap();
            assert_eq!(
                std::str::from_utf8(&encoded).unwrap(),
                std::str::from_utf8(golden).unwrap(),
                "the journal format changed; see this test's note before updating it"
            );
            let decoded: JournalRecord = decode(golden).unwrap();
            assert_eq!(
                decoded, record,
                "a log already on disk must still read back"
            );
            assert_eq!(encode(&decoded).unwrap(), encoded);
        }
    }

    /// Every `JournalIntent` variant, byte-pinned.
    ///
    /// Split from the payload test above only because the payload frame would
    /// repeat identically ten times; the argument is the same one and so is the
    /// consequence of a diff here. `JournalIntent` is the *durable pre-image*
    /// [#65](https://github.com/invakid404/umbra/issues/65) turned out to already
    /// have: `Overlay::bind` reads these back out of a crashed run's journal and
    /// decides from the variant alone whether the session can be trusted. A
    /// rename of `Symlink::target` would leave every round-trip test green while
    /// making every on-disk journal carrying a symlink prepare undecodable — and
    /// `Symlink` is the one intent the design argues cannot be rebuilt from
    /// durable state at all, so it is the worst one to lose.
    ///
    /// `Chmod` and `Link` are here despite no `prepare` arm producing them. They
    /// are decodable frames a foreign or future writer can put on disk, which is
    /// precisely why `replay_must_poison` gives them a named arm, and a format
    /// they cannot be decoded from is a `CorruptJournal` where the truth is
    /// "written by a newer umbra".
    #[test]
    fn every_journal_intent_encodes_to_the_bytes_the_log_already_holds() {
        let object = ObjectId(Uuid::from_u128(0x2323));
        let p = |b: &[u8]| BytePath::new(b.to_vec()).unwrap();
        let cases: [(JournalIntent, &[u8]); 10] = [
            (
                JournalIntent::CopyUp { object, path: p(b"/c") },
                br#"{"CopyUp":{"object":"00000000-0000-0000-0000-000000002323","path":[47,99]}}"#,
            ),
            (
                JournalIntent::Create { object, path: p(b"/f"), directory: false, mode: 0o644 },
                br#"{"Create":{"object":"00000000-0000-0000-0000-000000002323","path":[47,102],"directory":false,"mode":420}}"#,
            ),
            (
                JournalIntent::Rename { object, from: p(b"/a"), to: p(b"/b") },
                br#"{"Rename":{"object":"00000000-0000-0000-0000-000000002323","from":[47,97],"to":[47,98]}}"#,
            ),
            (
                JournalIntent::Link { object, existing: p(b"/e"), new_path: p(b"/n") },
                br#"{"Link":{"object":"00000000-0000-0000-0000-000000002323","existing":[47,101],"new_path":[47,110]}}"#,
            ),
            (
                // A relative target, kept byte-exact rather than normalised, per
                // `JournalIntent`'s own note.
                JournalIntent::Symlink { object, path: p(b"/l"), target: p(b"../t") },
                br#"{"Symlink":{"object":"00000000-0000-0000-0000-000000002323","path":[47,108],"target":[46,46,47,116]}}"#,
            ),
            (
                JournalIntent::Unlink { object, path: p(b"/u"), directory: true },
                br#"{"Unlink":{"object":"00000000-0000-0000-0000-000000002323","path":[47,117],"directory":true}}"#,
            ),
            (
                JournalIntent::Write { object, offset: Some(4096), length: 17 },
                br#"{"Write":{"object":"00000000-0000-0000-0000-000000002323","offset":4096,"length":17}}"#,
            ),
            (
                JournalIntent::Truncate { object, length: 9 },
                br#"{"Truncate":{"object":"00000000-0000-0000-0000-000000002323","length":9}}"#,
            ),
            (
                JournalIntent::Chmod { object, mode: 0o600 },
                br#"{"Chmod":{"object":"00000000-0000-0000-0000-000000002323","mode":384}}"#,
            ),
            (
                // An absent `gid` is `null` on the wire, not an omitted key.
                JournalIntent::Chown { object, path: p(b"/o"), uid: Some(1000), gid: None, copy_up: true },
                br#"{"Chown":{"object":"00000000-0000-0000-0000-000000002323","path":[47,111],"uid":1000,"gid":null,"copy_up":true}}"#,
            ),
        ];
        for (intent, golden) in cases {
            let encoded = encode(&intent).unwrap();
            assert_eq!(
                std::str::from_utf8(&encoded).unwrap(),
                std::str::from_utf8(golden).unwrap(),
                "the journal format changed; see the payload test's note before updating it"
            );
            assert_eq!(&decode::<JournalIntent>(golden).unwrap(), &intent);
        }
    }

    /// Every `JournalLifecycle` variant, byte-pinned, for the same reason.
    ///
    /// `RunCompleted` is the run's terminal "completed cleanly, through here"
    /// evidence and the only one the engine writes today; the other four are
    /// readable frames the replay path must keep decoding, and two of them carry
    /// a `CheckpointId` whose format is what a checkpoint-bearing recovery would
    /// be read through.
    #[test]
    fn every_journal_lifecycle_encodes_to_the_bytes_the_log_already_holds() {
        let checkpoint = CheckpointId(Uuid::from_u128(0x99));
        let through = Sequence(7);
        let cases: [(JournalLifecycle, &[u8]); 5] = [
            (JournalLifecycle::Started, br#""Started""#),
            (JournalLifecycle::RecoveryRequired, br#""RecoveryRequired""#),
            (
                JournalLifecycle::CheckpointPublished { checkpoint, through },
                br#"{"CheckpointPublished":{"checkpoint":"00000000-0000-0000-0000-000000000099","through":7}}"#,
            ),
            (
                JournalLifecycle::RunCompleted { through },
                br#"{"RunCompleted":{"through":7}}"#,
            ),
            (
                JournalLifecycle::CleanHandoff { checkpoint, through },
                br#"{"CleanHandoff":{"checkpoint":"00000000-0000-0000-0000-000000000099","through":7}}"#,
            ),
        ];
        for (lifecycle, golden) in cases {
            let encoded = encode(&lifecycle).unwrap();
            assert_eq!(
                std::str::from_utf8(&encoded).unwrap(),
                std::str::from_utf8(golden).unwrap(),
                "the journal format changed; see the payload test's note before updating it"
            );
            assert_eq!(&decode::<JournalLifecycle>(golden).unwrap(), &lifecycle);
        }
    }

    /// `RecoveryState` has no `#[serde(default)]` on `recovery_required`, and the
    /// absence of that attribute is load-bearing.
    ///
    /// This struct crosses the journal provider IPC as `Response::Open`
    /// (`umbra-journal`), so the field is a **wire-shape** change even though it
    /// is not an on-disk one. A default would make a payload from an older
    /// provider -- one whose replay cannot see a `RecoveryRequired` record at all
    /// -- decode as `recovery_required: false`, and a run a previous session
    /// declared unrecoverable would be reported as recoverable. Fail-open, and
    /// invisible, because nothing about a missing attribute shows up in a diff.
    ///
    /// Without it the decode fails and the caller gets `ProtocolMismatch`, which
    /// is the repo's stated stance for a peer built against an older crate:
    /// handshake, then fail to decode. `PROTOCOL_VERSION` is frozen at 2 and is
    /// deliberately not a second line of defence here.
    ///
    /// The opposite direction needs no test: serde ignores unknown fields, so an
    /// older *client* decodes a newer provider's payload and drops the field --
    /// and that client's `bind` predates #65, so it refuses any journal that is
    /// not pristine. Fail-closed by a different route.
    #[test]
    fn a_recovery_state_without_the_declared_verdict_refuses_to_decode() {
        let run_id = RunId(Uuid::from_u128(0x5150));
        let full = RecoveryState {
            run_id,
            checkpoint: None,
            last_valid_sequence: Sequence(4),
            durable: None,
            pending: vec![],
            tail: JournalTailRecovery::Intact,
            recovery_required: false,
            clean: true,
        };
        let encoded = encode(&full).unwrap();
        assert_eq!(&decode::<RecoveryState>(&encoded).unwrap(), &full);

        // The same payload as an older peer would send it: every other field
        // present, this one absent.
        let older: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        let mut older = older.as_object().unwrap().clone();
        assert!(
            older.remove("recovery_required").is_some(),
            "the field must be on the wire in the first place"
        );
        let older = serde_json::to_vec(&older).unwrap();
        let refused = decode::<RecoveryState>(&older).unwrap_err();
        assert_eq!(
            refused.kind,
            ErrorKind::ProtocolMismatch,
            "a missing verdict must fail closed, never default to 'recoverable'"
        );
    }

    /// The exhaustiveness `Overlay::replay_must_poison` relies on, asserted from
    /// this side too.
    ///
    /// That classifier is a wildcard-free `match` over this enum, so adding a
    /// variant is a compile error there -- which is the whole mechanism by which
    /// #65 avoided needing a durable record of "this intent creates". The
    /// enforcement is `rustc`'s and nothing here adds to it.
    ///
    /// **This test is documentation, not a guard, and the distinction matters.**
    /// The array below is hand-written, so an eleventh variant leaves it green
    /// and the length assertion's message can never fire; reading it as a check
    /// on the inventory would be reading it as exactly the kind of false
    /// invariant #65 exists to remove. What it does do is name every variant in
    /// one place a reviewer can compare against the classifier's arms, and hold
    /// `Link` and `Chmod` -- which no `prepare` arm produces, and which the
    /// classifier therefore poisons on -- where they cannot be quietly dropped as
    /// dead weight. The byte-pinning of each variant is two tests above; that one
    /// does fail on a rename.
    #[test]
    fn the_intent_inventory_the_replay_classifier_answers_for_is_named_in_one_place() {
        let object = ObjectId(Uuid::from_u128(1));
        let path = BytePath::new(b"/p".to_vec()).unwrap();
        let intents = [
            JournalIntent::CopyUp {
                object,
                path: path.clone(),
            },
            JournalIntent::Create {
                object,
                path: path.clone(),
                directory: false,
                mode: 0,
            },
            JournalIntent::Rename {
                object,
                from: path.clone(),
                to: path.clone(),
            },
            JournalIntent::Link {
                object,
                existing: path.clone(),
                new_path: path.clone(),
            },
            JournalIntent::Symlink {
                object,
                path: path.clone(),
                target: path.clone(),
            },
            JournalIntent::Unlink {
                object,
                path: path.clone(),
                directory: false,
            },
            JournalIntent::Write {
                object,
                offset: None,
                length: 0,
            },
            JournalIntent::Truncate { object, length: 0 },
            JournalIntent::Chmod { object, mode: 0 },
            JournalIntent::Chown {
                object,
                path,
                uid: None,
                gid: None,
                copy_up: false,
            },
        ];
        // Not a guard -- see the note above. The number is here so a diff that
        // adds a variant without adding it below is visible in review.
        assert_eq!(intents.len(), 10);
        for intent in &intents {
            let bytes = encode(intent).unwrap();
            assert_eq!(&decode::<JournalIntent>(&bytes).unwrap(), intent);
        }
    }
}
