//! The durable retry journal every supported mutation goes through.
//!
//! # What this is for (R1-004)
//!
//! The failure model requires that enough identity and payload be persisted
//! **before** a mutation is dispatched to rebuild its outcome without the RPC
//! reply cache. Round 1 found that requirement unmet on the public path: the only
//! `ReplayLog` implementation was in-memory, `MutationJournal` was never reached
//! by any `Storage` method, and namespace mutations dispatched straight to the
//! wire with no intent recorded and no existing key consulted. A retried
//! `Rename(A, B)` therefore re-looked-up `A`, found `NOENT`, and reported a
//! failure for an operation that had already succeeded.
//!
//! # The record
//!
//! Byte-compatible with the mounted `nfs` adapter, whose encoding
//! `tests/goldens/retry-*.json` pins:
//!
//! * `.provider/retries/key-<hex>` holds `[request, result]`, where `result` is
//!   `null` while the operation is in flight and `{"Ok":…}` or `{"Err":…}` once it
//!   has settled. `<hex>` is the idempotency key's bytes, lowercase hex.
//! * `.provider/retries/op-<uuid>` holds the key file's name, so reusing one
//!   operation id under a second idempotency key is caught rather than silently
//!   creating a second intent.
//!
//! Both are created with `GUARDED4`, so two contenders racing the same key cannot
//! both record an intent, and both are written `FILE_SYNC4`, because a record that
//! is not on stable storage before dispatch is not evidence.
//!
//! # What it deliberately does not claim
//!
//! Writing a record `FILE_SYNC4` asks the server for stability; it does not
//! qualify a persistence boundary. The provider still advertises
//! `Durability::None`, and `flush` still reports its gate. This closes the M1
//! replay obligation — exact request, exact payload and exact settled result,
//! recorded durably around the dispatch — and nothing more.

use serde::{Deserialize, Serialize};
use umbra_core::{
    ErrorKind, IdempotencyKey, OperationId, Result, StorageOperation, StorageRequest,
    StorageResponse, UmbraError,
};

use crate::anchor::{component, Anchor};
use crate::crud::{read_anonymous, CreateDisposition, OpenObject};
use crate::handle::ObjectIdentity;
use crate::identity::PinnedObject;
use crate::layout;
use crate::state::open_owner::{CloseOutcome, OpenOwnerRegistry};
use crate::transport::{
    AttrValues, Compound, Deadline, Nfs4Op, OpReply, RawTransport, ShareAccess, Stability,
};

/// Mode retry records are created with, matching the mounted adapter's layout.
const RECORD_MODE: u32 = 0o600;

/// Upper bound on one retry record.
///
/// A record holds the request, which for `WriteAt` holds the payload, so the
/// bound is the transport's own I/O bound plus room for the envelope. A record
/// larger than this is refused rather than partially read: half a record decodes
/// as nothing and would look like a fresh key.
const MAX_RECORD_BYTES: u32 = 8 * 1024 * 1024;

/// One object's identity *and* its content state, as evidence.
///
/// **R3-002.** Identity alone proves which object is behind a name; it proves
/// nothing about whether an intervening write changed it. `FATTR4_CHANGE` is the
/// attribute NFSv4 defines for exactly that question — it must differ whenever
/// the object's data or metadata changed — so a recovery that compares it can
/// tell "nothing has happened here" from "something did, and I cannot tell
/// whether it was me".
///
/// `size` and `time_modify` are carried alongside as corroboration rather than as
/// the decision: a server whose change attribute is a coarse timestamp can report
/// an unchanged `change` across two writes inside its resolution, and a differing
/// size or mtime catches that.
///
/// The write verifier is deliberately *not* here. It is a field of a WRITE or
/// COMMIT reply, not an attribute a pre-dispatch GETATTR can obtain, so recording
/// it before the mutation is not possible; what it would prove — that the server
/// restarted and may have dropped unstable data — is proven instead by the
/// WRITE/COMMIT comparison on the ordinary path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEvidence {
    /// Server identity: which object this is.
    pub identity: ObjectIdentity,
    /// `FATTR4_CHANGE`: whether anything about it has changed.
    pub change: Option<u64>,
    /// `FATTR4_SIZE`.
    pub size: Option<u64>,
    /// `FATTR4_TIME_MODIFY`, as nanoseconds.
    pub modified_nanos: Option<i128>,
}

impl ObjectEvidence {
    /// Whether this is the same object, in the same content state, as `other`.
    ///
    /// A missing `change` on either side is not treated as agreement: unknown is
    /// not equal, because the whole point is to refuse to guess.
    #[must_use]
    pub fn unchanged_from(&self, other: &Self) -> bool {
        if self.identity != other.identity {
            return false;
        }
        match (self.change, other.change) {
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
        self.size == other.size && self.modified_nanos == other.modified_nanos
    }
}

/// The state a mutation was dispatched against.
///
/// **R2-003.** `docs/design/failure-model.md` lists "object/parent identities and
/// preconditions" among the evidence a record must carry, and says an incomplete
/// operation "requires proof of before/after state". Without that proof there is
/// nothing to recover *from*: the request's pathname is not evidence of which
/// object it named, because an external replacement can put a different object
/// behind the same name.
///
/// Written to a sidecar (`pre-<hex>`) rather than into the record itself, so
/// `key-<hex>` stays byte-identical to the encoding the mounted adapter writes
/// and `tests/goldens/retry-*.json` pins. A record with no sidecar is a legacy
/// record: recoverable evidence is absent, and the failure model says such an
/// intent stops rather than being guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preconditions {
    /// The directory the mutation was addressed to.
    pub parent: Option<ObjectEvidence>,
    /// The target before dispatch. `None` means it did not exist.
    pub target: Option<ObjectEvidence>,
    /// For a rename, the destination's parent.
    pub destination_parent: Option<ObjectEvidence>,
    /// For a rename, the destination before dispatch. `None` means it was free.
    pub destination: Option<ObjectEvidence>,
}

/// What a recovery decided about an interrupted operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// The server state proves the effect did not happen. Dispatching again under
    /// the same key repeats nothing.
    NotApplied,
    /// The server state proves the effect happened.
    Applied,
    /// The state matches neither the recorded before nor the expected after.
    ///
    /// An external writer changed something under the operation, so no
    /// deterministic answer exists. The failure model puts this in
    /// BLOCKED_RECOVERABLE, with the evidence retained.
    Ambiguous(String),
}

/// What the journal decided about a request.
#[derive(Debug)]
pub enum Admission {
    /// No record existed. The intent is now durably recorded; dispatch may go
    /// ahead, and [`settle`] must be called with whatever happens.
    Fresh,
    /// This key already settled. The recorded outcome is the answer, forever.
    Recorded(Result<StorageResponse>),
    /// An earlier attempt recorded its intent and was interrupted, and the
    /// server's current state proves the effect never landed.
    ///
    /// **R2-003.** Dispatch proceeds under the same key. This is the recovery the
    /// failure model requires for a supported crash window, in place of the
    /// blanket "requires reconciliation" refusal it explicitly forbids.
    Redispatch,
}

/// The `.provider/retries` directory of one open run.
fn retries_directory(
    transport: &mut dyn RawTransport,
    private: &Anchor,
    deadline: Deadline,
) -> Result<PinnedObject> {
    crate::anchor::descend_directory(
        transport,
        private.pin(),
        &component(layout::RETRIES_DIR)?,
        deadline,
    )
}

/// The record file name for an idempotency key.
fn key_name(key: &IdempotencyKey) -> String {
    let hex: String = key
        .0
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{}{hex}", layout::RETRY_KEY_PREFIX)
}

/// The index file name for an operation id.
fn operation_name(operation: OperationId) -> String {
    format!("{}{}", layout::RETRY_OP_PREFIX, operation.0)
}

/// Prefix of the precondition sidecar this provider writes beside a record.
///
/// Deliberately not one of the two names the mounted adapter knows: an adapter
/// that does not understand it simply ignores it, and this provider treats its
/// absence as a legacy record rather than as corruption.
const PRECONDITION_PREFIX: &str = "pre-";

/// The precondition sidecar name for an idempotency key.
fn precondition_name(key: &IdempotencyKey) -> String {
    let hex: String = key
        .0
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{PRECONDITION_PREFIX}{hex}")
}

/// The on-disk record: the request as issued, and its outcome once known.
type Record = (StorageRequest, Option<Result<StorageResponse>>);

/// Consult the journal, and record the intent when the request is new.
///
/// Returns [`Admission::Recorded`] without dispatching when this key already
/// settled, and refuses rather than dispatching when a record exists whose
/// request differs, when the operation id was already used under another key, or
/// when a previous attempt recorded an intent that never settled.
/// What consulting the journal found, before any state is observed.
///
/// **R3-003.** Splitting the lookup from the admission is the whole point.
/// `Storage::execute` used to observe the request's current target namespace for
/// *every* mutation and only then consult the record, so a completed key whose
/// target path had since been replaced by a symlink — or had become unreadable —
/// failed during observation and never reached its own recorded success. A
/// settled key's answer is the record; the namespace has nothing to say about it.
#[derive(Debug)]
pub enum Lookup {
    /// This key already settled. Return the recorded outcome; observe nothing.
    Recorded(Result<StorageResponse>),
    /// No record exists. A fresh intent must be written, which needs the
    /// preconditions observed first.
    Fresh,
    /// An interrupted intent, with the evidence recorded beside it. Recovery
    /// needs the current state to compare against.
    Interrupted(Box<Preconditions>),
}

/// Consult the journal for this request's key, without touching its target.
pub fn lookup(
    transport: &mut dyn RawTransport,
    private: &Anchor,
    request: &StorageRequest,
    operation: &str,
    deadline: Deadline,
) -> Result<Lookup> {
    let retries = retries_directory(transport, private, deadline)?;
    let key_component = component(key_name(&request.context.idempotency_key).into_bytes())?;

    let Some(bytes) = read_record(transport, &retries, &key_component, deadline)? else {
        return Ok(Lookup::Fresh);
    };
    let (previous, outcome): Record = serde_json::from_slice(&bytes).map_err(|error| {
        UmbraError::new(
            ErrorKind::CorruptJournal,
            operation,
            format!(
                "the retry record for this idempotency key does not decode ({error}); it is \
                 retained on the server and the request is refused rather than re-dispatched"
            ),
        )
    })?;
    if previous != *request {
        // The same key naming a different request is the caller's error, and
        // answering it with the recorded outcome would report the result of an
        // operation it never asked for.
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            operation,
            "this idempotency key was already used for a different request",
        ));
    }
    if let Some(result) = outcome {
        return Ok(Lookup::Recorded(result));
    }

    // An intent with no result is an interrupted attempt. The failure model
    // forbids answering that with "requires reconciliation" when the window is
    // one this provider supports, so the recorded preconditions and the server's
    // current state decide it instead.
    let recorded = read_preconditions(
        transport,
        &retries,
        &request.context.idempotency_key,
        operation,
        deadline,
    )?;
    let Some(recorded) = recorded else {
        // A legacy record: written before this provider recorded preconditions,
        // or by an adapter that does not. There is no before-state to compare, so
        // the failure model's rule for a legacy ambiguous intent applies — stop,
        // retaining the evidence.
        return Err(blocked(
            operation,
            "this run holds an interrupted intent for the key with no recorded \
             preconditions, so whether it took effect cannot be established from \
             evidence. The record is retained and the run is blocked rather than \
             guessed or replayed.",
        ));
    };
    Ok(Lookup::Interrupted(Box::new(recorded)))
}

/// Settle an interrupted intent against the state observed now.
#[allow(clippy::too_many_arguments)]
pub fn recover_interrupted(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    request: &StorageRequest,
    recorded: &Preconditions,
    observed: &crate::ops::Observation,
    operation: &str,
    deadline: Deadline,
) -> Result<Admission> {
    let retries = retries_directory(transport, private, deadline)?;
    let key_component = component(key_name(&request.context.idempotency_key).into_bytes())?;
    match recover(&request.operation, recorded, &observed.evidence) {
        Recovery::NotApplied => Ok(Admission::Redispatch),
        Recovery::Applied => {
            // The effect is on the server. Settling from the observed state is
            // the recovery: the key stops being indeterminate, and later retries
            // are answered from the record like any settled one.
            let response = recovered_response(&request.operation, observed, operation)?;
            let record: Record = (request.clone(), Some(Ok(response.clone())));
            let encoded = encode(&record, operation)?;
            replace_record(
                transport,
                owners,
                &retries,
                &key_component,
                &encoded,
                operation,
                deadline,
            )?;
            Ok(Admission::Recorded(Ok(response)))
        }
        Recovery::Ambiguous(why) => Err(blocked(
            operation,
            &format!(
                "an interrupted intent for this key cannot be settled from evidence: {why}. \
                 The record and the server state are both retained and the run is blocked \
                 rather than guessed."
            ),
        )),
    }
}

/// Record a fresh intent, with its preconditions, before anything is dispatched.
pub fn admit_fresh(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    request: &StorageRequest,
    preconditions: &Preconditions,
    operation: &str,
    deadline: Deadline,
) -> Result<()> {
    let retries = retries_directory(transport, private, deadline)?;
    let key = key_name(&request.context.idempotency_key);
    let key_component = component(key.clone().into_bytes())?;

    // The operation id must be fresh too, or two different keys would be
    // claiming one operation identity.
    let operation_component = component(operation_name(request.context.operation_id).into_bytes())?;
    if read_record(transport, &retries, &operation_component, deadline)?.is_some() {
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            operation,
            "this operation id was already used under a different idempotency key",
        ));
    }

    // R2-010: encode before anything is created, so an over-large record is
    // refused with the journal untouched rather than after its index exists.
    let intent: Record = (request.clone(), None);
    let encoded = encode(&intent, operation)?;

    // R2-003: the preconditions go down before the intent, so a record that
    // exists always has evidence beside it rather than the other way round.
    let precondition_component =
        component(precondition_name(&request.context.idempotency_key).into_bytes())?;
    let precondition_bytes = serde_json::to_vec(preconditions).map_err(|error| {
        UmbraError::new(
            ErrorKind::InvalidState,
            operation,
            format!("the preconditions could not be encoded: {error}"),
        )
    })?;
    write_new_record(
        transport,
        owners,
        &retries,
        &precondition_component,
        &precondition_bytes,
        operation,
        deadline,
    )?;

    // Index next, then the intent. Both are `GUARDED4`, so a racing contender is
    // refused by the server rather than by a local read.
    write_new_record(
        transport,
        owners,
        &retries,
        &operation_component,
        key.as_bytes(),
        operation,
        deadline,
    )?;
    write_new_record(
        transport,
        owners,
        &retries,
        &key_component,
        &encoded,
        operation,
        deadline,
    )
}

/// Marker every blocked-recoverable stop carries in its context.
///
/// **R3-001.** The provider has to recognise these to enter its own stopped
/// state, and matching on prose would be a guess. This is the recognisable token;
/// it reads as a state name to a human too, which is the point.
pub const BLOCKED_RECOVERABLE: &str = "BLOCKED_RECOVERABLE:";

/// A safe stop with the evidence retained (`BLOCKED_RECOVERABLE`).
///
/// **R2-003.** Distinct from "requires reconciliation": that phrasing stood in
/// for recovery the failure model requires be *implemented*. This is the answer
/// for the cases the model does put in BLOCKED_RECOVERABLE — a legacy intent with
/// no evidence, or evidence that contradicts every deterministic outcome.
///
/// **R3-001.** Returning this error is not, by itself, the state the failure model
/// describes. `NfsUserspaceStorage` latches it: see `AuthorityLoss`'s sibling
/// `recovery_blocked`, which stops later mutations and denies a clean release.
fn blocked(operation: &str, detail: &str) -> UmbraError {
    UmbraError::new(
        ErrorKind::InvalidState,
        operation,
        format!("{BLOCKED_RECOVERABLE} {detail}"),
    )
}

/// Whether an error is one of this module's blocked-recoverable stops.
#[must_use]
pub fn is_blocked(error: &UmbraError) -> bool {
    error.context.starts_with(BLOCKED_RECOVERABLE)
}

/// Decide, from evidence, whether an interrupted mutation took effect.
///
/// `recorded` is what the state was before dispatch; `observed` is what it is
/// now. Neither is a guess: both come from the server's own identities and change
/// attributes, so an external replacement *or an external edit* is visible rather
/// than hidden behind an unchanged pathname.
///
/// **R3-002.** The previous version decided from identity alone, which is not
/// enough in two ways it got wrong. The rename arm ignored the recorded
/// destination entirely, so an interrupted `Rename(A, B)` whose destination `B`
/// had since been replaced by an external client was re-dispatched, and the
/// ordinary replacing RENAME destroyed the replacement. The in-place arms treated
/// an unchanged `fsid`/`fileid` as proof that nothing had happened, so an
/// interrupted `WriteAt` was replayed over an external writer's edit to the same
/// object. Identity says *which object exists*, never *whether it changed*.
///
/// Every arm now either proves its answer from before/after evidence or gives up
/// safely. "I cannot tell" is an outcome, not a reason to pick the convenient one.
fn recover(
    operation: &StorageOperation,
    recorded: &Preconditions,
    observed: &Preconditions,
) -> Recovery {
    // The parent must still be the same directory in every case: if it is not,
    // the mutation's addressing is no longer meaningful, whatever else holds.
    if let Err(why) = parent_identity("the parent directory", recorded.parent, observed.parent) {
        return Recovery::Ambiguous(why);
    }

    match operation {
        // --- creations: the create verifier is the proof, not the name -----
        //
        // **R4-002.** This arm used to read "the name was absent, the name is
        // present, therefore our create landed" and settle a `Created` response
        // built from whatever object it found. Two things break that.
        //
        // A supported create is two steps: `EXCLUSIVE4` OPEN, then the SETATTR
        // that applies the requested mode (R1-007). An interruption between them
        // leaves a file with the server's default mode, and declaring the
        // operation Applied stored a success for an operation that had not
        // finished. And the name may hold somebody else's object entirely — a
        // directory, even — which our file-create could never have produced.
        //
        // Presence is not proof, but the protocol already carries one.
        // `EXCLUSIVE4` exists precisely so a replayed create can be recognised:
        // the verifier is derived deterministically from the operation id
        // (`state::verifier::create_verifier_for`), which the durable record
        // holds, so a redispatch presents the *same* verifier the first attempt
        // used. The server answers success if that verifier is the one it stored
        // for the object — our create, replayed — and `NFS4ERR_EXIST` if it is
        // not. Redispatching also re-runs the SETATTR, which is what completes a
        // partial create rather than papering over it.
        //
        // So recovery never settles a create from observation. It hands the
        // decision to the server, which is the only party that can make it.
        StorageOperation::Create { .. } => match (recorded.target, observed.target) {
            // R4-003: concluding "redispatch" means asserting the namespace is
            // still the one this create was addressed against. Where the name is
            // *still absent*, a parent whose change attribute moved is a
            // directory something else happened in, and this decision cannot
            // attribute that to the interrupted operation.
            //
            // **R5-P2.** The gate stops there. Where the name is now *present*,
            // the parent's change attribute has necessarily moved if the
            // interrupted create is what put it there: RFC 7530 sec 5.8.1.4
            // requires a conforming server to report a different `FATTR4_CHANGE`
            // once the directory has changed, and creating a name changes the
            // directory. Demanding an unchanged parent there is demanding
            // evidence that a *successful* create rules out, so the arm was
            // reachable only against a test backend that forgot to bump the
            // parent on OPEN — which is the defect this finding names.
            //
            // Dropping it guesses nothing. `NotApplied` means "redispatch", and
            // the redispatch presents the same `EXCLUSIVE4` verifier the first
            // attempt used: the server answers success only if the object behind
            // that name is the one this operation created, and `NFS4ERR_EXIST`
            // otherwise, which `storage::execute` latches as a safe give-up. The
            // verifier is the discriminator, and it is conclusive; the parent's
            // change attribute never was.
            (None, None)
                if parent_unchanged("the parent directory", recorded.parent, observed.parent)
                    .is_err() =>
            {
                Recovery::Ambiguous(
                    parent_unchanged("the parent directory", recorded.parent, observed.parent)
                        .unwrap_err(),
                )
            }
            (None, None) => Recovery::NotApplied,
            (None, Some(_)) => Recovery::NotApplied,
            // It already existed before dispatch, unchanged: the request would
            // have failed the same way again, and re-running reproduces that
            // answer without repeating an effect.
            (Some(before), Some(now)) if now.unchanged_from(&before) => Recovery::NotApplied,
            (before, now) => Recovery::Ambiguous(format!(
                "the create target was {:?} before dispatch and is {:?} now",
                before.map(|e| e.identity),
                now.map(|e| e.identity)
            )),
        },

        // --- removals: present before, absent after ------------------------
        //
        // R3-002: absence alone is not the answer. The reviewer's point is that
        // it does not establish that *this* request is what removed it. What the
        // evidence can establish is the contradictory case: a target that
        // vanished while its parent directory's change attribute stood still is
        // not a state any removal produced, so it is refused rather than read as
        // success. Where the parent did change, the postcondition this request
        // asked for holds, and `Unlinked` asserts that the name is gone — not who
        // removed it.
        StorageOperation::Unlink { .. } | StorageOperation::RemoveDirectory { .. } => {
            match (recorded.target, observed.target) {
                (Some(_), None) => match (recorded.parent, observed.parent) {
                    (Some(before), Some(now)) if before.change == now.change => {
                        Recovery::Ambiguous(
                            "the removal target is gone but its parent directory's change \
                             attribute did not move, which no removal produces"
                                .to_owned(),
                        )
                    }
                    _ => Recovery::Applied,
                },
                // R4-003: "nothing was removed" asserts the directory is
                // untouched, so the directory's own change attribute is part of
                // the proof. (The *Applied* arm above is the documented
                // exception: a parent whose change attribute moved is the
                // signature a removal leaves, not a contradiction of it.)
                (Some(before), Some(now)) if now.unchanged_from(&before) => {
                    match parent_unchanged("the parent directory", recorded.parent, observed.parent)
                    {
                        Ok(()) => Recovery::NotApplied,
                        Err(why) => Recovery::Ambiguous(why),
                    }
                }
                (None, None) => {
                    // It was already gone before dispatch, so the request would
                    // have failed NOENT either way; re-running reproduces that
                    // answer without repeating an effect.
                    match parent_unchanged("the parent directory", recorded.parent, observed.parent)
                    {
                        Ok(()) => Recovery::NotApplied,
                        Err(why) => Recovery::Ambiguous(why),
                    }
                }
                (before, now) => Recovery::Ambiguous(format!(
                    "the removal target was {:?} before dispatch and is {:?} now",
                    before.map(|e| e.identity),
                    now.map(|e| e.identity)
                )),
            }
        }

        // --- rename: the source's identity moves to the destination --------
        //
        // R3-002: the recorded destination is consulted. Re-dispatching an
        // ordinary replacing RENAME over a destination that changed under the
        // interruption destroys whatever replaced it.
        StorageOperation::Rename { .. } => {
            if let Err(why) = parent_identity(
                "the rename destination's parent",
                recorded.destination_parent,
                observed.destination_parent,
            ) {
                return Recovery::Ambiguous(why);
            }
            let moved = recorded.target.map(|e| e.identity);
            let source_now = observed.target;
            let destination_now = observed.destination;

            // The source is gone and the destination holds the object that was at
            // the source: the rename landed.
            if source_now.is_none()
                && destination_now.map(|e| e.identity) == moved
                && moved.is_some()
            {
                return Recovery::Applied;
            }

            // Nothing moved: the source is still the same object in the same
            // state, *and* the destination is still exactly what was recorded.
            let source_still_there = match (recorded.target, source_now) {
                (Some(before), Some(now)) => now.unchanged_from(&before),
                _ => false,
            };
            let destination_untouched = match (recorded.destination, destination_now) {
                (None, None) => true,
                (Some(before), Some(now)) => now.unchanged_from(&before),
                _ => false,
            };
            if source_still_there && destination_untouched {
                // **R4-003.** Both endpoint names looking untouched is not enough
                // to conclude that nothing happened. A rename mutates *two*
                // directories, so their change attributes are part of this
                // operation's before-state; round 3 recorded them and then
                // compared only identities. An interrupted rename that applied,
                // followed by external namespace activity that restored the
                // observed names, presents exactly the endpoint state this branch
                // reads as "not applied" — and re-dispatching it would move the
                // object a second time.
                for (label, before, now) in [
                    (
                        "the rename source's parent",
                        recorded.parent,
                        observed.parent,
                    ),
                    (
                        "the rename destination's parent",
                        recorded.destination_parent,
                        observed.destination_parent,
                    ),
                ] {
                    if let Err(why) = parent_unchanged(label, before, now) {
                        return Recovery::Ambiguous(why);
                    }
                }
                return Recovery::NotApplied;
            }
            Recovery::Ambiguous(format!(
                "the rename cannot be settled: source was {:?} and is {:?}; destination was \
                 {:?} and is {:?}",
                recorded.target.map(|e| e.identity),
                source_now.map(|e| e.identity),
                recorded.destination.map(|e| e.identity),
                destination_now.map(|e| e.identity)
            ))
        }

        // --- in-place mutations: absolute, but only over an unchanged object -
        //
        // R3-002: an absolute write of the same bytes at the same offset, an
        // absolute attribute set and an absolute truncation all converge *if
        // nothing else touched the object*. Replaying one over an external
        // writer's edit does not converge, it overwrites. Proving the object is
        // byte-for-byte in the state it was dispatched against is what makes the
        // replay safe; anything else is a safe give-up.
        StorageOperation::WriteAt { .. }
        | StorageOperation::SetMetadata { .. }
        | StorageOperation::Truncate { .. } => match (recorded.target, observed.target) {
            (Some(before), Some(now)) if now.unchanged_from(&before) => Recovery::NotApplied,
            (Some(before), Some(now)) if before.identity == now.identity => {
                Recovery::Ambiguous(format!(
                    "the target is the same object but its state moved (change {:?} -> {:?}, \
                     size {:?} -> {:?}); replaying an absolute mutation would overwrite \
                     whatever changed it, and whether this request is what changed it cannot \
                     be established",
                    before.change, now.change, before.size, now.size
                ))
            }
            (None, None) => Recovery::NotApplied,
            (before, now) => Recovery::Ambiguous(format!(
                "the target was {:?} before dispatch and is {:?} now, so repeating an absolute \
                 mutation could act on a different object",
                before.map(|e| e.identity),
                now.map(|e| e.identity)
            )),
        },

        // `mkdir -p` converges: every component it would create is a directory
        // whose presence is the whole postcondition. The parent identity check
        // above is what keeps it addressed to the same place.
        StorageOperation::CreateParents { .. } => Recovery::NotApplied,

        // Everything else is refused by the capability surface long before a
        // record exists, so an interrupted intent naming one is not a window
        // this provider supports.
        _ => Recovery::Ambiguous(
            "the recorded operation is not one this provider journals".to_owned(),
        ),
    }
}

/// The parent must be the same object. Nothing else is asserted here.
fn parent_identity(
    label: &str,
    recorded: Option<ObjectEvidence>,
    observed: Option<ObjectEvidence>,
) -> std::result::Result<(), String> {
    match (recorded, observed) {
        (None, None) => Ok(()),
        (Some(before), Some(now)) if before.identity == now.identity => Ok(()),
        (Some(before), Some(now)) => Err(format!(
            "{label} was {:?} before dispatch and is {:?} now",
            before.identity, now.identity
        )),
        _ => Err(format!(
            "{label} was resolvable before dispatch and is not now, or the reverse"
        )),
    }
}

/// The parent must be the same object *in the same state*.
///
/// **R4-003.** Round 3 recorded `FATTR4_CHANGE`, size and mtime for both parents
/// and then compared only their identities, so a namespace edit that moved a
/// directory's change attribute was invisible to the decision.
///
/// This guards the *redispatch* conclusion specifically, which is the dangerous
/// one: saying "nothing happened, run it again" asserts the namespace is still
/// the one the operation was addressed against. A directory's change attribute
/// moves on any mutation within it, so this cannot distinguish the interrupted
/// operation's own effect from a sibling edit — and the failure model forbids
/// reading an absent conflict signal as proof of no conflict, so an unexplained
/// signal is a safe give-up.
///
/// Two conclusions deliberately do *not* consult it, each for a stated reason:
///
/// * a removal or rename settled as **Applied**, where a moved parent change
///   attribute is the signature the operation leaves rather than a contradiction;
/// * an **in-place** mutation, whose postcondition is entirely within one object.
///   A write, truncate or attribute set does not touch its parent directory, so
///   the object's own change attribute is a complete before/after proof and a
///   sibling's edit in the same directory is genuinely irrelevant to it.
fn parent_unchanged(
    label: &str,
    recorded: Option<ObjectEvidence>,
    observed: Option<ObjectEvidence>,
) -> std::result::Result<(), String> {
    parent_identity(label, recorded, observed)?;
    match (recorded, observed) {
        (None, None) => Ok(()),
        (Some(before), Some(now)) if now.unchanged_from(&before) => Ok(()),
        (Some(before), Some(now)) => Err(format!(
            "{label} is the same directory but its state moved (change {:?} -> {:?}); something \
             was mutated in it since dispatch, and this decision cannot attribute that to the \
             interrupted operation",
            before.change, now.change
        )),
        _ => Err(format!(
            "{label} was resolvable before dispatch and is not now, or the reverse"
        )),
    }
}

/// Build the response a recovered effect settles to./// Build the response a recovered effect settles to.
///
/// **R3-001.** This used to block for creates and renames, because the journal
/// holds no operations surface and could not rebuild an `ObjectResult` from
/// identities. It does not have to: the observation that proved the effect landed
/// already read the object, so the result travels with it. A recovery that can
/// answer, answers; only a genuinely unanswerable case blocks.
fn recovered_response(
    operation: &StorageOperation,
    observed: &crate::ops::Observation,
    label: &str,
) -> Result<StorageResponse> {
    match operation {
        StorageOperation::Unlink { .. } => Ok(StorageResponse::Unlinked),
        StorageOperation::RemoveDirectory { .. } => Ok(StorageResponse::DirectoryRemoved),
        // R4-002: a create is never settled as Applied — the exclusive verifier
        // decides it on redispatch — so reaching here means the decision table
        // and this builder have drifted apart. Blocking is the safe answer to
        // that, and it is never a fabricated success.
        StorageOperation::Create { .. } | StorageOperation::CreateParents { .. } => Err(blocked(
            label,
            "a create is settled by replaying its exclusive-create verifier, never by \
             observing that its name exists; reaching this point means the recovery \
             decision and the response builder disagree, so the run is blocked rather \
             than answered",
        )),
        // A rename's effect is at the destination, which is where the object now
        // is.
        StorageOperation::Rename { .. } => observed
            .destination_result
            .clone()
            .map(StorageResponse::Renamed)
            .ok_or_else(|| {
                blocked(
                    label,
                    "the interrupted rename took effect but the moved object could not be read \
                     back at its destination, so no result can be reported for it; the record \
                     is retained and the run is blocked rather than answered with a \
                     fabricated result",
                )
            }),
        StorageOperation::WriteAt { .. }
        | StorageOperation::SetMetadata { .. }
        | StorageOperation::Truncate { .. } => Err(blocked(
            label,
            "an interrupted in-place mutation is never settled as applied; it is either \
             replayed over a provably unchanged object or refused",
        )),
        _ => Err(blocked(
            label,
            "the recorded operation is not one this provider journals",
        )),
    }
}

/// Read the precondition sidecar for a key, or `None` for a legacy record.
fn read_preconditions(
    transport: &mut dyn RawTransport,
    retries: &PinnedObject,
    key: &IdempotencyKey,
    operation: &str,
    deadline: Deadline,
) -> Result<Option<Preconditions>> {
    let name = component(precondition_name(key).into_bytes())?;
    let Some(bytes) = read_record(transport, retries, &name, deadline)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        UmbraError::new(
            ErrorKind::CorruptJournal,
            operation,
            format!(
                "the recorded preconditions for this key do not decode ({error}); they are \
                 retained on the server and the run is blocked rather than recovered from \
                 evidence that cannot be read"
            ),
        )
    })
}

/// Record what a dispatched request actually did.
///
/// The outcome is written as it happened. A failure is recorded exactly like a
/// success, because a retry of a failed key must be told the failure rather than
/// be allowed to try again under the same identity.
pub fn settle(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    request: &StorageRequest,
    outcome: &Result<StorageResponse>,
    operation: &str,
    deadline: Deadline,
) -> Result<()> {
    let retries = retries_directory(transport, private, deadline)?;
    let key_component = component(key_name(&request.context.idempotency_key).into_bytes())?;
    let record: Record = (request.clone(), Some(outcome.clone()));
    let encoded = encode(&record, operation)?;
    replace_record(
        transport,
        owners,
        &retries,
        &key_component,
        &encoded,
        operation,
        deadline,
    )
}

/// Encode a record, refusing one too large to be written and read back whole.
///
/// **R2-010.** Checked *before* any index or intent reaches the server, so a
/// request whose record cannot round-trip is refused while the journal is still
/// untouched rather than after half of it exists.
fn encode(record: &Record, operation: &str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(record).map_err(|error| {
        UmbraError::new(
            ErrorKind::InvalidState,
            operation,
            format!("a retry record could not be encoded: {error}"),
        )
    })?;
    if bytes.len() > MAX_RECORD_BYTES as usize {
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            operation,
            format!(
                "this request encodes to a {}-byte retry record, past the {MAX_RECORD_BYTES}-byte \
                 bound this journal can write and read back whole; it is refused before any \
                 record is created",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// Read one record whole, or `None` when the name is absent.
///
/// Absence is `NFS4ERR_NOENT` and nothing else. A lookup or read that *failed* is
/// propagated: reporting it as absence would make a fresh intent out of a record
/// that may well exist, which is the whole failure this journal prevents.
///
/// **R2-010.** This used to issue one `READ` for `MAX_RECORD_BYTES` and hand back
/// whatever came out, which is wrong twice over. NFSv4 `READ` may return fewer
/// bytes than asked for without being at end of file, and the raw transport caps
/// every reply at `limits().max_reply_bytes` — 1 MiB on the loopback profile —
/// while the provider advertises writes up to 1 MiB, whose JSON byte-array
/// encoding is several times larger. A perfectly valid record could therefore
/// come back as a prefix, and `serde_json` would call the prefix corrupt: the
/// provider would report `CorruptJournal` for a record it wrote itself.
///
/// So the record is read in bounded chunks until the server reports EOF, with the
/// total capped. A short reply is a normal step, not a frame; only a complete
/// frame that fails to decode is corruption.
fn read_record(
    transport: &mut dyn RawTransport,
    retries: &PinnedObject,
    name: &crate::transport::ComponentName,
    deadline: Deadline,
) -> Result<Option<Vec<u8>>> {
    let pinned = match crate::anchor::descend_file(transport, retries, name, deadline) {
        Ok(pinned) => pinned,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    // One chunk never exceeds what the transport will decode, so a chunk is never
    // refused for being too large to reply to.
    let chunk = u32::try_from(transport.limits().max_reply_bytes)
        .unwrap_or(u32::MAX)
        .clamp(1, MAX_RECORD_BYTES);

    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let remaining = MAX_RECORD_BYTES.saturating_sub(bytes.len() as u32);
        if remaining == 0 {
            // The record on the server is larger than any record this provider
            // writes. Refusing beats decoding a truncation as a whole frame.
            return Err(UmbraError::new(
                ErrorKind::CorruptJournal,
                "retry",
                format!(
                    "the retry record exceeds the {MAX_RECORD_BYTES}-byte bound before end of \
                     file; it is not a record this provider wrote and is refused rather than \
                     read as a truncated one"
                ),
            ));
        }
        let want = chunk.min(remaining);
        let reply = read_anonymous(transport, &pinned, bytes.len() as u64, want, deadline)
            .map_err(|error| error.to_umbra("retry"))?;
        let progressed = !reply.data.is_empty();
        bytes.extend_from_slice(&reply.data);
        if reply.eof {
            break;
        }
        if !progressed {
            // Not at end of file and no bytes: the server is not advancing, and
            // looping forever is not an answer. This is an incomplete transport
            // result, deliberately distinct from a corrupt frame.
            return Err(UmbraError::new(
                ErrorKind::StorageUnavailable,
                "retry",
                format!(
                    "the server returned no bytes and no end-of-file at offset {} of the retry \
                     record; the record could not be read whole and its state is unknown",
                    bytes.len()
                ),
            ));
        }
    }
    Ok(Some(bytes))
}

/// `GUARDED4` create and write one record whole.
fn write_new_record(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    retries: &PinnedObject,
    name: &crate::transport::ComponentName,
    bytes: &[u8],
    operation: &str,
    deadline: Deadline,
) -> Result<()> {
    let open = OpenObject::open(
        owners,
        transport,
        retries,
        name,
        CreateDisposition::CreateNew { mode: RECORD_MODE },
        ShareAccess::WRITE,
        deadline,
    )
    .map_err(|error| error.to_umbra(operation))?;
    write_whole(transport, open, bytes, operation, deadline)
}

/// Open an existing record and replace its contents exactly.
fn replace_record(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    retries: &PinnedObject,
    name: &crate::transport::ComponentName,
    bytes: &[u8],
    operation: &str,
    deadline: Deadline,
) -> Result<()> {
    let open = OpenObject::open(
        owners,
        transport,
        retries,
        name,
        CreateDisposition::OpenExisting,
        ShareAccess::WRITE,
        deadline,
    )
    .map_err(|error| error.to_umbra(operation))?;
    write_whole(transport, open, bytes, operation, deadline)
}

/// Write `bytes` at offset zero and set the file's size to match.
///
/// The explicit `SETATTR` of `FATTR4_SIZE` is what makes "replace" mean replace:
/// a shorter record written over a longer one would otherwise leave a tail that
/// turns the file into something that decodes as neither.
fn write_whole(
    transport: &mut dyn RawTransport,
    open: OpenObject,
    bytes: &[u8],
    operation: &str,
    deadline: Deadline,
) -> Result<()> {
    let stateid = match open.stateid() {
        Ok(stateid) => stateid,
        Err(error) => {
            let _ = open.close(transport, deadline);
            return Err(error.to_umbra(operation));
        }
    };
    let handle = open.handle().clone();
    // FILE_SYNC4: the record is the evidence a recovery reads, so it is on stable
    // storage before this call returns or it is not evidence.
    let result = (|transport: &mut dyn RawTransport| -> Result<()> {
        // R2-010: a short WRITE is a real answer, not a failure, so the record is
        // written in as many round trips as the server needs. Previously a single
        // short write was reported as an error, leaving a truncated record that
        // decodes as nothing and reads as a fresh key on the next attempt.
        let mut written = 0usize;
        while written < bytes.len() {
            let reply = transport
                .write(
                    &handle,
                    stateid,
                    written as u64,
                    Stability::FileSync,
                    bytes[written..].to_vec(),
                    deadline,
                )
                .map_err(|error| error.to_umbra(operation))?;
            if reply.count == 0 {
                // No progress and no error: looping forever is not an answer.
                return Err(UmbraError::new(
                    ErrorKind::Io,
                    operation,
                    format!(
                        "the server accepted no bytes at offset {written} of a {}-byte retry \
                         record; the record is incomplete on the server",
                        bytes.len()
                    ),
                ));
            }
            written += reply.count as usize;
        }
        let truncate = AttrValues {
            size: Some(bytes.len() as u64),
            ..AttrValues::default()
        };
        let reply = transport
            .submit(
                Compound::new(
                    *b"retrysz",
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::SetAttr {
                            stateid,
                            attributes: truncate,
                        },
                    ],
                ),
                deadline,
            )
            .map_err(|error| crate::error::FacadeError::Transport(error).to_umbra(operation))?;
        match reply.expect(1).map_err(|error| error.to_umbra(operation))? {
            OpReply::SetAttr(_) => Ok(()),
            other => Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                operation,
                format!("expected a SETATTR reply for the record size, got {other:?}"),
            )),
        }
    })(transport);
    // The open is closed whichever way the body went. A body failure is the more
    // informative answer; a close failure only wins when the body succeeded.
    let closed = open.close(transport, deadline);
    match (result, closed) {
        (Err(error), _) => Err(error),
        (Ok(()), CloseOutcome::Closed(_)) => Ok(()),
        (Ok(()), CloseOutcome::Rejected { error, .. })
        | (Ok(()), CloseOutcome::Abandoned { error }) => Err(error.to_umbra(operation)),
    }
}
