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
    /// Identity of the directory the mutation was addressed to.
    pub parent: Option<ObjectIdentity>,
    /// Identity of the target before dispatch. `None` means it did not exist.
    pub target: Option<ObjectIdentity>,
    /// For a rename, the destination's parent.
    pub destination_parent: Option<ObjectIdentity>,
    /// For a rename, the destination's identity before dispatch.
    pub destination: Option<ObjectIdentity>,
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
#[allow(clippy::too_many_arguments)]
pub fn admit(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    request: &StorageRequest,
    preconditions: &Preconditions,
    observed: &Preconditions,
    operation: &str,
    deadline: Deadline,
) -> Result<Admission> {
    let retries = retries_directory(transport, private, deadline)?;
    let key = key_name(&request.context.idempotency_key);
    let key_component = component(key.clone().into_bytes())?;

    if let Some(bytes) = read_record(transport, &retries, &key_component, deadline)? {
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
            return Ok(Admission::Recorded(result));
        }

        // R2-003: an intent with no result is an interrupted attempt. The
        // failure model forbids answering that with "requires reconciliation"
        // when the window is one this provider supports, so the recorded
        // preconditions and the server's current state decide it instead.
        let recorded = read_preconditions(
            transport,
            &retries,
            &request.context.idempotency_key,
            operation,
            deadline,
        )?;
        let Some(recorded) = recorded else {
            // A legacy record: written before this provider recorded
            // preconditions, or by an adapter that does not. There is no
            // before-state to compare, so the failure model's rule for a legacy
            // ambiguous intent applies — stop, retaining the evidence.
            return Err(blocked(
                operation,
                "this run holds an interrupted intent for the key with no recorded \
                 preconditions, so whether it took effect cannot be established from \
                 evidence. The record is retained and the run is blocked rather than \
                 guessed or replayed.",
            ));
        };
        return match recover(&request.operation, &recorded, observed) {
            Recovery::NotApplied => Ok(Admission::Redispatch),
            Recovery::Applied => {
                // The effect is on the server. Settling from the observed state
                // is the recovery: the key stops being indeterminate, and later
                // retries are answered from the record like any settled one.
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
                    "an interrupted intent for this key cannot be settled from evidence: \
                     {why}. The record and the server state are both retained and the run \
                     is blocked rather than guessed."
                ),
            )),
        };
    }

    // A fresh key. The operation id must be fresh too, or two different keys
    // would be claiming one operation identity.
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
    )?;
    Ok(Admission::Fresh)
}

/// A safe stop with the evidence retained (`BLOCKED_RECOVERABLE`).
///
/// **R2-003.** Distinct from "requires reconciliation": that phrasing stood in
/// for recovery the failure model requires be *implemented*. This is the answer
/// for the cases the model does put in BLOCKED_RECOVERABLE — a legacy intent with
/// no evidence, or evidence that contradicts every deterministic outcome.
fn blocked(operation: &str, detail: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, operation, detail.to_owned())
}

/// Decide, from evidence, whether an interrupted mutation took effect.
///
/// `recorded` is what the state was before dispatch; `observed` is what it is
/// now. Neither is a guess: both come from the server's own identities, so an
/// external replacement is visible as a changed identity rather than hidden
/// behind an unchanged pathname.
///
/// The operations split into two classes. A *detectable* mutation changes
/// presence or identity in a way the before/after pair settles. An *idempotent*
/// one — an absolute write, an absolute attribute set, `mkdir -p` — produces the
/// same state whether it ran once or twice, so proving the target is still the
/// same object is enough to re-dispatch safely.
fn recover(
    operation: &StorageOperation,
    recorded: &Preconditions,
    observed: &Preconditions,
) -> Recovery {
    // The parent must still be the same directory in every case. If it is not,
    // the mutation's addressing is no longer meaningful.
    if recorded.parent.is_some() && recorded.parent != observed.parent {
        return Recovery::Ambiguous(format!(
            "the parent directory was {:?} before dispatch and is {:?} now",
            recorded.parent, observed.parent
        ));
    }

    match operation {
        // --- creations: absent before, present after -----------------------
        StorageOperation::Create { .. } => match (recorded.target, observed.target) {
            (None, None) => Recovery::NotApplied,
            (None, Some(_)) => Recovery::Applied,
            (Some(before), Some(now)) if before == now => Recovery::NotApplied,
            (before, now) => Recovery::Ambiguous(format!(
                "the create target was {before:?} before dispatch and is {now:?} now"
            )),
        },

        // --- removals: present before, absent after ------------------------
        StorageOperation::Unlink { .. } | StorageOperation::RemoveDirectory { .. } => {
            match (recorded.target, observed.target) {
                (Some(_), None) => Recovery::Applied,
                (Some(before), Some(now)) if before == now => Recovery::NotApplied,
                (None, None) => {
                    // It was already gone before dispatch, so the request would
                    // have failed NOENT either way; re-running reproduces that
                    // answer without repeating an effect.
                    Recovery::NotApplied
                }
                (before, now) => Recovery::Ambiguous(format!(
                    "the removal target was {before:?} before dispatch and is {now:?} now"
                )),
            }
        }

        // --- rename: the source's identity moves to the destination --------
        StorageOperation::Rename { .. } => {
            let moved = recorded.target;
            match (observed.target, observed.destination) {
                // The source is gone and the destination now holds the object
                // that was at the source: the rename landed.
                (None, destination) if destination.is_some() && destination == moved => {
                    Recovery::Applied
                }
                // The source still holds the same object: nothing moved.
                (Some(now), _) if Some(now) == moved => Recovery::NotApplied,
                (source, destination) => Recovery::Ambiguous(format!(
                    "the rename source was {moved:?} before dispatch; the source is {source:?} \
                     and the destination {destination:?} now"
                )),
            }
        }

        // --- idempotent given a stable target ------------------------------
        //
        // An absolute write of the same bytes at the same offset, an absolute
        // attribute set, an absolute truncation and `mkdir -p` all converge:
        // running them a second time produces the state the first run aimed at.
        // What must be proven is only that the object is still the same one.
        StorageOperation::WriteAt { .. }
        | StorageOperation::SetMetadata { .. }
        | StorageOperation::Truncate { .. } => match (recorded.target, observed.target) {
            (Some(before), Some(now)) if before == now => Recovery::NotApplied,
            (None, None) => Recovery::NotApplied,
            (before, now) => Recovery::Ambiguous(format!(
                "the target was {before:?} before dispatch and is {now:?} now, so repeating an \
                 absolute mutation could act on a different object"
            )),
        },
        StorageOperation::CreateParents { .. } => Recovery::NotApplied,

        // Everything else is refused by the capability surface long before a
        // record exists, so an interrupted intent naming one is not a window
        // this provider supports.
        _ => Recovery::Ambiguous(
            "the recorded operation is not one this provider journals".to_owned(),
        ),
    }
}

/// Build the response a recovered effect settles to.
///
/// Only the operations whose effect is *detectable* reach here, and each of them
/// has an answer that can be read from the state rather than reconstructed: a
/// create answers with the object that now exists, a removal with its typed unit
/// response, a rename with the object now at the destination.
fn recovered_response(
    operation: &StorageOperation,
    observed: &Preconditions,
    label: &str,
) -> Result<StorageResponse> {
    match operation {
        StorageOperation::Unlink { .. } => Ok(StorageResponse::Unlinked),
        StorageOperation::RemoveDirectory { .. } => Ok(StorageResponse::DirectoryRemoved),
        // A create or rename answers with an object result, which needs the
        // object's current stat. The provider supplies it, because the journal
        // holds no operations surface; it is passed back through `observed`.
        _ => Err(blocked(
            label,
            &format!(
                "the interrupted operation took effect, but this journal cannot rebuild its \
                 reply from identities alone (target {:?}); the effect is recorded and the \
                 run is blocked rather than answered with a fabricated result",
                observed.target
            ),
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
