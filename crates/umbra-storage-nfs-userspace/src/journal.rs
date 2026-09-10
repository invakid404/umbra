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

use umbra_core::{
    ErrorKind, IdempotencyKey, OperationId, Result, StorageRequest, StorageResponse, UmbraError,
};

use crate::anchor::{component, Anchor};
use crate::crud::{read_anonymous, CreateDisposition, OpenObject};
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

/// What the journal decided about a request.
#[derive(Debug)]
pub enum Admission {
    /// No record existed. The intent is now durably recorded; dispatch may go
    /// ahead, and [`settle`] must be called with whatever happens.
    Fresh,
    /// This key already settled. The recorded outcome is the answer, forever.
    Recorded(Result<StorageResponse>),
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

/// The on-disk record: the request as issued, and its outcome once known.
type Record = (StorageRequest, Option<Result<StorageResponse>>);

/// Consult the journal, and record the intent when the request is new.
///
/// Returns [`Admission::Recorded`] without dispatching when this key already
/// settled, and refuses rather than dispatching when a record exists whose
/// request differs, when the operation id was already used under another key, or
/// when a previous attempt recorded an intent that never settled.
pub fn admit(
    transport: &mut dyn RawTransport,
    owners: &mut OpenOwnerRegistry,
    private: &Anchor,
    request: &StorageRequest,
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
        return match outcome {
            Some(result) => Ok(Admission::Recorded(result)),
            // An intent with no result is a previous attempt that never settled.
            // Dispatching again could repeat a non-idempotent effect, so this
            // needs reconciliation rather than a retry.
            None => Err(UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "a previous attempt at this idempotency key recorded its intent and never \
                 recorded an outcome; the operation is indeterminate and requires \
                 reconciliation before it can be retried",
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

    // Index first, then the intent. Both are `GUARDED4`, so a racing contender is
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
