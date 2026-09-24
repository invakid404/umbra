//! File-backed journal: a bounded, CRC32-checksummed append-only framed log and
//! immutable checkpoint publication beneath the injected control directory.
//!
//! Layout, all under `<control>/journal/`:
//!
//! ```text
//! log                     header + framed records
//! checkpoints/<uuid>.json snapshot contents, fsynced before being referenced
//! checkpoint              the reference naming the current snapshot
//! ```
//!
//! Each frame is `length: u32 BE | crc32: u32 BE | payload`, where the payload is
//! the encoded record carrying its **assigned** sequence. Length is checked
//! before any allocation. A frame whose bytes are all present but whose checksum
//! disagrees is corruption and an error; only a demonstrably incomplete final
//! frame is recoverable, by exclusion, and repairing it requires writer
//! authority.
//!
//! Boundaries this backend keeps: accepted appends are not durability, so only
//! [`Journal::flush`] returns a receipt; writer authority is injected by the
//! supervisor and never acquired here; a checkpoint's contents reach disk before
//! its reference does, so recovery cannot select a half-written snapshot; and
//! close surfaces the error from flushing pending records rather than reporting a
//! tidy shutdown over a failed write.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use umbra_core::{
    Checkpoint, CheckpointId, DurableSequence, ErrorKind, JournalAccess, JournalLifecycle,
    JournalOpenRequest, JournalPayload, JournalPendingOperation, JournalRecord,
    JournalTailRecovery, JournalWriterAuthority, LeaseEpoch, RecoveryState, Result, RunId,
    Sequence, UmbraError,
};
use umbra_journal::Journal;

/// Format version this backend writes.
pub const FORMAT_VERSION: u32 = 1;
/// File header identifying the format; a mismatch is not a recoverable tail.
const MAGIC: &[u8; 8] = b"UMBRAJ01";
/// Largest single encoded record. Checked before allocating a payload buffer.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Largest log this backend will replay, bounding recovery work.
pub const MAX_LOG_BYTES: u64 = 512 * 1024 * 1024;

fn error(kind: ErrorKind, operation: &str, context: impl Into<String>) -> UmbraError {
    UmbraError::new(kind, operation, context)
}

fn io_error(operation: &str, path: &Path, e: std::io::Error) -> UmbraError {
    let mut error = UmbraError::new(ErrorKind::Io, operation, format!("{}: {e}", path.display()));
    if let Some(code) = e.raw_os_error() {
        error = error.with_errno(umbra_core::Errno(code));
    }
    error
}

fn corrupt(context: impl Into<String>) -> UmbraError {
    error(ErrorKind::CorruptJournal, "journal_file", context)
}

/// CRC-32 (IEEE 802.3, reflected, init/final 0xFFFFFFFF), computed bitwise.
///
/// A table would be faster; journal frames are small and this keeps the format
/// definition self-contained and dependency-free.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// File-based framed journal. Construction performs no I/O; the control-directory
/// binding and writer authority arrive through [`Journal::open`].
#[derive(Debug, Default)]
pub struct FileJournal {
    session: Option<Box<SessionState>>,
}

#[derive(Debug)]
struct SessionState {
    directory: PathBuf,
    log: PathBuf,
    file: Option<File>,
    run_id: RunId,
    epoch: Option<LeaseEpoch>,
    last_sequence: Sequence,
    durable: Option<DurableSequence>,
    valid_bytes: u64,
    closed: bool,
}

impl FileJournal {
    /// Construct an unopened journal.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&mut self) -> Result<&mut SessionState> {
        let state = self
            .session
            .as_deref_mut()
            .ok_or_else(|| error(ErrorKind::InvalidState, "journal_file", "no open session"))?;
        if state.closed {
            return Err(error(
                ErrorKind::InvalidState,
                "journal_file",
                "session is closed",
            ));
        }
        Ok(state)
    }

    fn writable(&mut self) -> Result<(&mut SessionState, LeaseEpoch)> {
        let state = self.state()?;
        let epoch = state.epoch.ok_or_else(|| {
            error(
                ErrorKind::Denied,
                "journal_file",
                "session is read-only; mutation requires writer authority",
            )
        })?;
        Ok((state, epoch))
    }
}

fn validate_authority(
    request: &JournalOpenRequest,
    authority: &JournalWriterAuthority,
) -> Result<()> {
    if authority.run_id != request.control.run_id {
        return Err(error(
            ErrorKind::InvalidInput,
            "journal_file.open",
            "writer authority names a different run",
        ));
    }
    if authority.writer_instance_id.is_empty() {
        return Err(error(
            ErrorKind::InvalidInput,
            "journal_file.open",
            "writer authority has no instance identity",
        ));
    }
    Ok(())
}

fn control_directory(request: &JournalOpenRequest) -> Result<PathBuf> {
    let bytes = request.control.directory.0.as_bytes();
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
    if !path.is_absolute() {
        return Err(error(
            ErrorKind::InvalidPath,
            "journal_file.open",
            "control binding must be an absolute runtime path",
        ));
    }
    if !path.is_dir() {
        return Err(error(
            ErrorKind::NotFound,
            "journal_file.open",
            format!("control directory {} does not exist", path.display()),
        ));
    }
    Ok(path.join("journal"))
}

/// One decoded frame plus the byte length it occupied.
struct Frame {
    record: JournalRecord,
    bytes: u64,
}

enum FrameOutcome {
    Eof,
    Frame(Frame),
    Incomplete,
}

/// Read a frame, distinguishing intact EOF from an incomplete trailing frame.
fn read_frame(reader: &mut impl Read) -> Result<FrameOutcome> {
    let mut header = [0u8; 8];
    let mut read = 0;
    while read < header.len() {
        match reader.read(&mut header[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(corrupt(format!("reading frame header: {e}"))),
        }
    }
    if read == 0 {
        return Ok(FrameOutcome::Eof);
    }
    if read < header.len() {
        return Ok(FrameOutcome::Incomplete);
    }
    let length = u32::from_be_bytes(header[..4].try_into().expect("4 bytes")) as usize;
    let checksum = u32::from_be_bytes(header[4..].try_into().expect("4 bytes"));
    if length == 0 || length > MAX_RECORD_BYTES {
        return Err(corrupt(format!("frame length {length} is out of bounds")));
    }
    let mut payload = vec![0u8; length];
    let mut read = 0;
    while read < length {
        match reader.read(&mut payload[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(corrupt(format!("reading frame payload: {e}"))),
        }
    }
    if read < length {
        return Ok(FrameOutcome::Incomplete);
    }
    if crc32(&payload) != checksum {
        return Err(corrupt(
            "frame checksum mismatch: the log is corrupt, not merely truncated",
        ));
    }
    let record: JournalRecord = umbra_core::provider::decode(&payload)
        .map_err(|e| corrupt(format!("undecodable frame payload: {e}")))?;
    Ok(FrameOutcome::Frame(Frame {
        record,
        bytes: (8 + length) as u64,
    }))
}

fn encode_frame(record: &JournalRecord) -> Result<Vec<u8>> {
    let payload = umbra_core::provider::encode(record)?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(error(
            ErrorKind::InvalidInput,
            "journal_file.append",
            "record exceeds the maximum frame payload",
        ));
    }
    let mut frame = Vec::with_capacity(payload.len() + 8);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&payload).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn fsync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io_error("journal_file.sync", path, e))
}

impl Journal for FileJournal {
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState> {
        if self.session.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "journal_file.open",
                "a session is already open",
            ));
        }
        if !request.format.readable_versions.contains(&FORMAT_VERSION)
            || request.format.write_version != FORMAT_VERSION
        {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "journal_file.open",
                format!("this backend implements journal format {FORMAT_VERSION} only"),
            ));
        }
        let epoch = match &request.access {
            JournalAccess::ReadOnly => None,
            JournalAccess::Writer(authority) => {
                validate_authority(request, authority)?;
                Some(authority.lease_epoch)
            }
        };
        let directory = control_directory(request)?;
        let log = directory.join("log");
        let writable = epoch.is_some();

        if writable {
            std::fs::create_dir_all(directory.join("checkpoints"))
                .map_err(|e| io_error("journal_file.open", &directory, e))?;
        } else if !directory.is_dir() {
            return Err(error(
                ErrorKind::NotFound,
                "journal_file.open",
                "no journal exists in this control directory",
            ));
        }

        // Establish or verify the header before reading a single frame.
        let mut existing = match File::open(&log) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_error("journal_file.open", &log, e)),
        };
        // Creation may have reached disk before the first header write. With
        // zero bytes there are no records to lose; writer open reinstalls MAGIC.
        if let Some(file) = &existing {
            if file
                .metadata()
                .map_err(|e| io_error("journal_file.open", &log, e))?
                .len()
                == 0
            {
                existing = None;
            }
        }
        let mut valid_bytes = MAGIC.len() as u64;
        let mut last_sequence = Sequence(0);
        let mut pending: BTreeMap<umbra_core::OperationId, JournalPendingOperation> =
            BTreeMap::new();
        // Latched by the replay below, never cleared. See `apply_recovery`.
        let mut recovery_required = false;
        let mut tail = JournalTailRecovery::Intact;

        if let Some(file) = existing.as_mut() {
            let length = file
                .metadata()
                .map_err(|e| io_error("journal_file.open", &log, e))?
                .len();
            if length > MAX_LOG_BYTES {
                return Err(error(
                    ErrorKind::UnsupportedCapability,
                    "journal_file.open",
                    "journal log is full: its size exceeds the replay limit",
                ));
            }
            let mut magic = [0u8; 8];
            file.read_exact(&mut magic)
                .map_err(|_| corrupt("journal log is missing its format header"))?;
            if &magic != MAGIC {
                return Err(corrupt("journal log header does not match this format"));
            }
            let mut reader = BufReader::new(file);
            loop {
                match read_frame(&mut reader)? {
                    FrameOutcome::Eof => break,
                    FrameOutcome::Incomplete => {
                        let discarded = length - valid_bytes;
                        tail = JournalTailRecovery::IncompleteFinalFrame {
                            valid_bytes,
                            discarded_bytes: discarded,
                            // Physical truncation happens below and only with
                            // writer authority; a read-only open reports it.
                            repaired: writable,
                        };
                        break;
                    }
                    FrameOutcome::Frame(frame) => {
                        if frame.record.format_version != FORMAT_VERSION {
                            return Err(corrupt("frame declares an unsupported format version"));
                        }
                        if frame.record.sequence.0 != last_sequence.0 + 1 {
                            return Err(corrupt(format!(
                                "frame sequence {} does not follow {}",
                                frame.record.sequence.0, last_sequence.0
                            )));
                        }
                        last_sequence = frame.record.sequence;
                        valid_bytes += frame.bytes;
                        apply_recovery(&mut pending, &mut recovery_required, &frame.record);
                    }
                }
            }
        }

        let session_file = if writable {
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&log)
                .map_err(|e| io_error("journal_file.open", &log, e))?;
            if existing.is_none() {
                file.write_all(MAGIC)
                    .map_err(|e| io_error("journal_file.open", &log, e))?;
                file.sync_all()
                    .map_err(|e| io_error("journal_file.open", &log, e))?;
                fsync_directory(&directory)?;
            }
            // Drop a demonstrably incomplete final frame so the next append is
            // not written behind garbage. Only the tail is ever removed.
            if !matches!(tail, JournalTailRecovery::Intact) {
                file.set_len(valid_bytes)
                    .map_err(|e| io_error("journal_file.open", &log, e))?;
                file.sync_all()
                    .map_err(|e| io_error("journal_file.open", &log, e))?;
            }
            Some(file)
        } else {
            existing
        };

        let checkpoint = read_checkpoint(&directory)?;
        if let Some(checkpoint) = &checkpoint {
            if checkpoint.run_id != request.control.run_id {
                return Err(corrupt("published checkpoint belongs to a different run"));
            }
        }

        self.session = Some(Box::new(SessionState {
            directory,
            log,
            file: session_file,
            run_id: request.control.run_id,
            epoch,
            last_sequence,
            durable: None,
            valid_bytes,
            closed: false,
        }));

        let clean = recovery_is_clean(&pending, recovery_required, &tail);
        Ok(RecoveryState {
            run_id: request.control.run_id,
            checkpoint,
            last_valid_sequence: last_sequence,
            // Nothing in this session has been flushed yet, and readable frames
            // are not retroactive evidence of a previous flush receipt.
            durable: None,
            pending: pending.into_values().collect(),
            tail,
            // A prior session's own verdict, carried out of the log rather than
            // inferred from it. See the field, and `apply_recovery` for why no
            // amount of looking at `pending` could reconstruct this.
            recovery_required,
            // Anything left prepared-but-uncommitted needs reconciliation by the
            // namespace owner before this run can be called clean.
            clean,
        })
    }

    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        let (state, epoch) = self.writable()?;
        if record.writer_epoch != epoch {
            return Err(error(
                ErrorKind::LeaseLost,
                "journal_file.append",
                "record epoch does not match this session's writer authority",
            ));
        }
        let sequence = Sequence(state.last_sequence.0 + 1);
        let stored = JournalRecord {
            format_version: FORMAT_VERSION,
            // The caller's sequence is not authority to place a frame; the log
            // assigns and persists the position it actually wrote.
            sequence,
            operation_id: record.operation_id,
            writer_epoch: record.writer_epoch,
            payload: record.payload.clone(),
        };
        let frame = encode_frame(&stored)?;
        if frame.len() as u64 > MAX_LOG_BYTES.saturating_sub(state.valid_bytes) {
            return Err(error(
                ErrorKind::UnsupportedCapability,
                "journal_file.append",
                "journal log is full: appending would exceed the replay size limit",
            ));
        }
        let file = state.file.as_mut().expect("writer has a log handle");
        file.seek(SeekFrom::Start(state.valid_bytes))
            .map_err(|e| io_error("journal_file.append", &state.log, e))?;
        file.write_all(&frame)
            .map_err(|e| io_error("journal_file.append", &state.log, e))?;
        state.valid_bytes += frame.len() as u64;
        state.last_sequence = sequence;
        Ok(sequence)
    }

    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        let (state, epoch) = self.writable()?;
        if through.0 > state.last_sequence.0 {
            return Err(error(
                ErrorKind::InvalidInput,
                "journal_file.flush",
                "cannot flush a sequence beyond the end of the log",
            ));
        }
        let file = state.file.as_ref().expect("writer has a log handle");
        file.sync_all()
            .map_err(|e| io_error("journal_file.flush", &state.log, e))?;
        // The receipt covers everything accepted so far, which is at least the
        // requested sequence; a partial fsync is not a thing this backend claims.
        let receipt = DurableSequence {
            run_id: state.run_id,
            writer_epoch: epoch,
            sequence: state.last_sequence,
        };
        state.durable = Some(receipt.clone());
        Ok(receipt)
    }

    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let state = self.state()?;
        let Some(file) = state.file.as_mut() else {
            return Ok(Box::new(std::iter::empty()));
        };
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io_error("journal_file.replay", &state.log, e))?;
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)
            .map_err(|_| corrupt("journal log is missing its format header"))?;
        if &magic != MAGIC {
            return Err(corrupt("journal log header does not match this format"));
        }
        Ok(Box::new(Replay {
            reader: BufReader::new(file),
            after,
            finished: false,
        }))
    }

    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        let (state, epoch) = self.writable()?;
        if checkpoint.run_id != state.run_id {
            return Err(error(
                ErrorKind::InvalidInput,
                "journal_file.checkpoint",
                "checkpoint names a different run",
            ));
        }
        if checkpoint.writer_epoch != epoch {
            return Err(error(
                ErrorKind::LeaseLost,
                "journal_file.checkpoint",
                "checkpoint epoch does not match this session's writer authority",
            ));
        }
        if checkpoint.format_version != FORMAT_VERSION {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "journal_file.checkpoint",
                "unsupported checkpoint format version",
            ));
        }
        let durable = state.durable.as_ref().ok_or_else(|| {
            error(
                ErrorKind::InvalidState,
                "journal_file.checkpoint",
                "publish requires the checkpoint's last committed sequence to be flushed",
            )
        })?;
        if checkpoint.last_committed.sequence.0 > durable.sequence.0 {
            return Err(error(
                ErrorKind::InvalidState,
                "journal_file.checkpoint",
                "checkpoint claims a sequence beyond the durable watermark",
            ));
        }
        let contents = umbra_core::provider::encode(checkpoint)?;
        let snapshot = state
            .directory
            .join("checkpoints")
            .join(format!("{}.json", checkpoint.id.0));
        write_durably(&snapshot, &contents)?;
        fsync_directory(&state.directory.join("checkpoints"))?;
        // Contents are durable before anything points at them, so recovery can
        // never select a half-written snapshot.
        let reference = state.directory.join("checkpoint");
        let temporary = state.directory.join("checkpoint.tmp");
        write_durably(&temporary, checkpoint.id.0.to_string().as_bytes())?;
        std::fs::rename(&temporary, &reference)
            .map_err(|e| io_error("journal_file.checkpoint", &reference, e))?;
        fsync_directory(&state.directory)?;
        Ok(checkpoint.id)
    }

    fn close(&mut self) -> Result<()> {
        let Some(state) = self.session.as_deref_mut() else {
            return Err(error(
                ErrorKind::InvalidState,
                "journal_file.close",
                "no open session",
            ));
        };
        if state.closed {
            return Err(error(
                ErrorKind::InvalidState,
                "journal_file.close",
                "session is already closed",
            ));
        }
        let result = if state.epoch.is_some() {
            // Surface a failed final flush rather than reporting a clean close.
            state
                .file
                .as_ref()
                .expect("writer has a log handle")
                .sync_all()
                .map_err(|e| io_error("journal_file.close", &state.log, e))
        } else {
            Ok(())
        };
        state.closed = true;
        state.file = None;
        result
    }
}

struct Replay<'a> {
    reader: BufReader<&'a mut File>,
    after: Sequence,
    finished: bool,
}

impl Iterator for Replay<'_> {
    type Item = Result<JournalRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        loop {
            match read_frame(&mut self.reader) {
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                Ok(FrameOutcome::Eof) => {
                    self.finished = true;
                    return None;
                }
                // An incomplete final frame ends the stream; it is reported by
                // `open` as recoverable tail damage, not as a silent EOF here.
                Ok(FrameOutcome::Incomplete) => {
                    self.finished = true;
                    return None;
                }
                Ok(FrameOutcome::Frame(frame)) => {
                    if frame.record.sequence.0 > self.after.0 {
                        return Some(Ok(frame.record));
                    }
                }
            }
        }
    }
}

fn write_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| io_error("journal_file.write", path, e))?;
    file.write_all(bytes)
        .map_err(|e| io_error("journal_file.write", path, e))?;
    file.sync_all()
        .map_err(|e| io_error("journal_file.write", path, e))
}

fn read_checkpoint(directory: &Path) -> Result<Option<Checkpoint>> {
    let reference = directory.join("checkpoint");
    let id = match std::fs::read_to_string(&reference) {
        Ok(id) => id,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("journal_file.open", &reference, e)),
    };
    // Older writers could truncate the reference before publishing it. The
    // complete log remains authoritative; recover by replaying without a snapshot.
    if id.trim().is_empty() {
        return Ok(None);
    }
    let snapshot = directory
        .join("checkpoints")
        .join(format!("{}.json", id.trim()));
    let bytes = std::fs::read(&snapshot)
        .map_err(|_| corrupt("checkpoint reference names a missing snapshot"))?;
    Ok(Some(umbra_core::provider::decode(&bytes)?))
}

/// Track prepared-but-uncommitted operations while replaying, and the one
/// lifecycle verdict a reader cannot reconstruct without being told.
///
/// `recovery_required` latches on `JournalLifecycle::RecoveryRequired` and is
/// never cleared. `Overlay::abort` writes that record on its uncorroborated path
/// — the one where it poisons *without* running an unwind — immediately before
/// the `Abort` record ([#65](https://github.com/invakid404/umbra/issues/65),
/// waiver (viii)). The ordering is what makes it safe: a crash between the two
/// leaves the `Prepare` with no terminal record at all, which the classifier in
/// `Overlay::bind` already poisons on.
///
/// Why a latch here and not something `bind` could work out for itself: the
/// `Abort` record below removes the operation from `pending`, so by the time a
/// reader sees the inventory, the evidence that a creation was materialised and
/// never undone is *gone*. That erasure is exactly the defect this carries
/// around. `Lifecycle` used to be the one payload this function ignored
/// entirely.
fn apply_recovery(
    pending: &mut BTreeMap<umbra_core::OperationId, JournalPendingOperation>,
    recovery_required: &mut bool,
    record: &JournalRecord,
) {
    match &record.payload {
        JournalPayload::Prepare { intent } => {
            pending.insert(
                record.operation_id,
                JournalPendingOperation {
                    operation_id: record.operation_id,
                    writer_epoch: record.writer_epoch,
                    prepared_at: record.sequence,
                    intent: intent.clone(),
                    observed_result: None,
                },
            );
        }
        JournalPayload::ObservedResult { outcome } => {
            if let Some(entry) = pending.get_mut(&record.operation_id) {
                entry.observed_result = Some(outcome.clone());
            }
        }
        JournalPayload::Commit | JournalPayload::Abort { .. } => {
            pending.remove(&record.operation_id);
        }
        JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired) => {
            *recovery_required = true;
        }
        JournalPayload::Lifecycle(_) => {}
    }
}

/// Unfinished operations, a damaged tail and a prior session's own
/// recovery-required verdict each require reconciliation.
///
/// The third conjunct is not redundant with the first: the session that writes
/// `RecoveryRequired` also writes the terminal record that empties `pending` of
/// the operation it is about, so a run can be declared unrecoverable and still
/// present a structurally empty, intact inventory.
fn recovery_is_clean(
    pending: &BTreeMap<umbra_core::OperationId, JournalPendingOperation>,
    recovery_required: bool,
    tail: &JournalTailRecovery,
) -> bool {
    pending.is_empty() && !recovery_required && matches!(tail, JournalTailRecovery::Intact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{BytePath, RunId};
    use umbra_core::{
        JournalControlBinding, JournalFencingEvidence, JournalFormatPolicy, JournalIntent,
        ObjectId, OperationId, OperationOutcome, PhysicalPath,
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("umbra-journal-{name}-"))
            .tempdir()
            .unwrap()
    }

    fn request(directory: &Path, run_id: RunId, epoch: u64) -> JournalOpenRequest {
        JournalOpenRequest {
            control: JournalControlBinding {
                run_id,
                directory: PhysicalPath(
                    BytePath::new(directory.as_os_str().as_bytes().to_vec()).unwrap(),
                ),
            },
            access: JournalAccess::Writer(JournalWriterAuthority {
                run_id,
                writer_instance_id: "writer".into(),
                host_fingerprint: "test".into(),
                supervisor_version: "test".into(),
                acquired_at: 0,
                renewed_at: 0,
                lease_epoch: LeaseEpoch(epoch),
                fencing: JournalFencingEvidence::Fenced {
                    mechanism: "test".into(),
                    evidence: vec![],
                },
            }),
            format: JournalFormatPolicy {
                readable_versions: vec![FORMAT_VERSION],
                write_version: FORMAT_VERSION,
            },
        }
    }

    fn record(epoch: u64, id: OperationId, payload: JournalPayload) -> JournalRecord {
        JournalRecord {
            format_version: FORMAT_VERSION,
            sequence: Sequence(0),
            operation_id: id,
            writer_epoch: LeaseEpoch(epoch),
            payload,
        }
    }

    #[test]
    fn append_assigns_sequences_and_replay_returns_them_in_order() {
        let scratch = scratch("append");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 7)).unwrap();
        assert_eq!(state.last_valid_sequence, Sequence(0));
        assert!(state.clean);
        assert_eq!(state.tail, JournalTailRecovery::Intact);

        let first = OperationId(uuid_v4());
        let assigned = journal
            .append(&record(
                7,
                first,
                JournalPayload::Prepare {
                    intent: JournalIntent::Create {
                        object: ObjectId(uuid_v4()),
                        path: BytePath::new(b"/a".to_vec()).unwrap(),
                        directory: false,
                        mode: 0o644,
                    },
                },
            ))
            .unwrap();
        assert_eq!(assigned, Sequence(1));
        assert_eq!(
            journal
                .append(&record(7, first, JournalPayload::Commit))
                .unwrap(),
            Sequence(2)
        );
        let receipt = journal.flush(Sequence(2)).unwrap();
        assert_eq!(receipt.sequence, Sequence(2));
        assert_eq!(receipt.writer_epoch, LeaseEpoch(7));

        let replayed: Vec<_> = journal
            .replay(Sequence(0))
            .unwrap()
            .map(|r| r.unwrap().sequence)
            .collect();
        assert_eq!(replayed, vec![Sequence(1), Sequence(2)]);
        let tail: Vec<_> = journal
            .replay(Sequence(1))
            .unwrap()
            .map(|r| r.unwrap().sequence)
            .collect();
        assert_eq!(tail, vec![Sequence(2)]);
        journal.close().unwrap();
        let mut reopened = FileJournal::new();
        let recovered = reopened.open(&request(dir, run_id, 8)).unwrap();
        assert!(recovered.pending.is_empty());
        assert!(recovered.clean, "a fully committed intact journal is clean");
    }

    #[test]
    fn a_stale_epoch_cannot_append_and_flush_cannot_run_ahead() {
        let scratch = scratch("epoch");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let mut journal = FileJournal::new();
        journal.open(&request(dir, run_id, 3)).unwrap();
        let id = OperationId(uuid_v4());
        assert_eq!(
            journal
                .append(&record(2, id, JournalPayload::Commit))
                .unwrap_err()
                .kind,
            ErrorKind::LeaseLost
        );
        assert_eq!(
            journal.flush(Sequence(9)).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn reopening_recovers_prepared_operations_and_excludes_only_a_torn_tail() {
        let scratch = scratch("recover");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let id = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Truncate {
                            object: ObjectId(uuid_v4()),
                            length: 4,
                        },
                    },
                ))
                .unwrap();
            journal.flush(Sequence(1)).unwrap();
            journal.close().unwrap();
        }
        // Tear the tail by appending a partial frame header.
        let log = dir.join("journal/log");
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&[0, 0, 1]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert!(matches!(
            state.tail,
            JournalTailRecovery::IncompleteFinalFrame { repaired: true, .. }
        ));
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].operation_id, id);
        assert!(!state.clean, "a prepared operation is not a clean state");
        // The torn tail was truncated, so the next append lands at sequence 2.
        assert_eq!(
            journal
                .append(&record(2, id, JournalPayload::Commit))
                .unwrap(),
            Sequence(2)
        );
    }

    #[test]
    fn a_corrupt_interior_frame_is_an_error_not_a_short_replay() {
        let scratch = scratch("corrupt");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            for _ in 0..2 {
                journal
                    .append(&record(1, OperationId(uuid_v4()), JournalPayload::Commit))
                    .unwrap();
            }
            journal.close().unwrap();
        }
        let log = dir.join("journal/log");
        let mut bytes = std::fs::read(&log).unwrap();
        // Flip a byte inside the first frame's payload, leaving lengths intact.
        let offset = MAGIC.len() + 8 + 4;
        bytes[offset] ^= 0xFF;
        std::fs::write(&log, &bytes).unwrap();

        let mut journal = FileJournal::new();
        assert_eq!(
            journal.open(&request(dir, run_id, 1)).unwrap_err().kind,
            ErrorKind::CorruptJournal
        );
    }

    #[test]
    fn checkpoint_publication_requires_a_flushed_sequence() {
        let scratch = scratch("checkpoint");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let mut journal = FileJournal::new();
        journal.open(&request(dir, run_id, 5)).unwrap();
        journal
            .append(&record(5, OperationId(uuid_v4()), JournalPayload::Commit))
            .unwrap();
        let checkpoint = Checkpoint {
            format_version: FORMAT_VERSION,
            id: CheckpointId(uuid_v4()),
            run_id,
            writer_epoch: LeaseEpoch(5),
            last_committed: DurableSequence {
                run_id,
                writer_epoch: LeaseEpoch(5),
                sequence: Sequence(1),
            },
            fingerprints: umbra_core::JournalFingerprints {
                base: "b".into(),
                toolchain: "t".into(),
                agent_provider_id: "a".into(),
                agent_version: "v".into(),
                supervisor_version: "s".into(),
            },
            state: umbra_core::JournalLogicalState {
                format_version: FORMAT_VERSION,
                entries: vec![],
                whiteouts: vec![],
                agent_session_id: "s".into(),
                agent_state_paths: vec![],
                metadata: vec![],
            },
            clean: false,
        };
        assert_eq!(
            journal.write_checkpoint(&checkpoint).unwrap_err().kind,
            ErrorKind::InvalidState
        );
        journal.flush(Sequence(1)).unwrap();
        assert_eq!(
            journal.write_checkpoint(&checkpoint).unwrap(),
            checkpoint.id
        );

        // A fresh open finds the published snapshot through its reference.
        let mut reopened = FileJournal::new();
        let state = reopened.open(&request(dir, run_id, 6)).unwrap();
        assert_eq!(state.checkpoint.unwrap().id, checkpoint.id);
        assert!(!dir.join("journal/checkpoint.tmp").exists());
        // Simulate the empty reference left by an older truncate-based writer.
        std::fs::write(dir.join("journal/checkpoint"), b"").unwrap();
        let mut recovered = FileJournal::new();
        let state = recovered.open(&request(dir, run_id, 7)).unwrap();
        assert!(state.checkpoint.is_none());
        assert_eq!(state.last_valid_sequence, Sequence(1));
        assert_eq!(recovered.replay(Sequence(0)).unwrap().count(), 1);
    }

    #[test]
    fn append_refuses_to_make_the_log_unreplayable() {
        let scratch = scratch("append-bound");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let mut journal = FileJournal::new();
        journal.open(&request(dir, run_id, 1)).unwrap();
        // Place the append cursor at the bound without allocating a huge log.
        journal.session.as_mut().unwrap().valid_bytes = MAX_LOG_BYTES;
        let e = journal
            .append(&record(1, OperationId(uuid_v4()), JournalPayload::Commit))
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::UnsupportedCapability);
        assert_eq!(
            std::fs::metadata(dir.join("journal/log")).unwrap().len(),
            MAGIC.len() as u64
        );
        assert_eq!(journal.session.as_ref().unwrap().last_sequence, Sequence(0));
        // A sparse oversized legacy log reports a capacity limit, not corruption.
        OpenOptions::new()
            .write(true)
            .open(dir.join("journal/log"))
            .unwrap()
            .set_len(MAX_LOG_BYTES + 1)
            .unwrap();
        let mut reopened = FileJournal::new();
        assert_eq!(
            reopened.open(&request(dir, run_id, 2)).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
    }

    #[test]
    fn empty_log_recovers_but_partial_header_remains_corrupt() {
        let scratch = scratch("empty-log");
        let dir = scratch.path();
        let run = RunId(uuid_v4());
        std::fs::create_dir_all(dir.join("journal")).unwrap();
        std::fs::write(dir.join("journal/log"), b"").unwrap();
        let mut journal = FileJournal::new();
        assert!(journal.open(&request(dir, run, 1)).unwrap().clean);
        assert_eq!(std::fs::read(dir.join("journal/log")).unwrap(), MAGIC);
        journal.close().unwrap();
        std::fs::write(dir.join("journal/log"), &MAGIC[..3]).unwrap();
        assert_eq!(
            FileJournal::new()
                .open(&request(dir, run, 2))
                .unwrap_err()
                .kind,
            ErrorKind::CorruptJournal
        );
    }

    #[test]
    fn a_live_session_keeps_its_log_when_the_path_is_replaced() {
        let scratch = scratch("replace-log");
        let dir = scratch.path();
        let run = RunId(uuid_v4());
        let mut journal = FileJournal::new();
        journal.open(&request(dir, run, 1)).unwrap();
        let original = dir.join("original-log");
        std::fs::rename(dir.join("journal/log"), &original).unwrap();
        std::fs::write(dir.join("journal/log"), b"replacement").unwrap();
        journal
            .append(&record(1, OperationId(uuid_v4()), JournalPayload::Commit))
            .unwrap();
        journal.flush(Sequence(1)).unwrap();
        assert_eq!(journal.replay(Sequence(0)).unwrap().count(), 1);
        journal.close().unwrap();
        assert!(std::fs::metadata(original).unwrap().len() > MAGIC.len() as u64);
        assert_eq!(
            std::fs::read(dir.join("journal/log")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn crc32_matches_known_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    // ------------------------------------------------------------------
    // The replay side of [#65](https://github.com/invakid404/umbra/issues/65).
    //
    // `Overlay::bind` now reads `RecoveryState.pending` and poisons the session
    // for any operation whose `Prepare` intent implies a prepare-time creation,
    // instead of refusing to reopen the journal at all. `apply_recovery` is what
    // produces that inventory and it is the only `JournalPayload` parser in the
    // repo, so these units are where "a crash left this operation unfinished"
    // becomes a checked property of real bytes on disk rather than a fixture's
    // assertion about itself. The classification they feed is pinned on the
    // engine side; what is pinned here is the reduction.

    /// A `Prepare` fsynced with no terminal record behind it is exactly what a
    /// crash between `prepare` and `commit`/`abort` leaves, and it survives the
    /// reopen as one pending entry carrying its intent.
    ///
    /// The intent matters, not just the count: it is the whole durable
    /// pre-image #65 turned out to already have. `prepare` appends and fsyncs
    /// this record *before* any arm creates anything, so a `Create` here means
    /// "a creation may have happened and the list that would undo it is gone".
    #[test]
    fn a_prepare_with_no_terminal_record_survives_the_reopen_with_its_intent() {
        let scratch = scratch("pending-intent");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let id = OperationId(uuid_v4());
        let object = ObjectId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Create {
                            object,
                            path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                            directory: false,
                            mode: 0o644,
                        },
                    },
                ))
                .unwrap();
            journal.flush(Sequence(1)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert!(matches!(state.tail, JournalTailRecovery::Intact));
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].operation_id, id);
        assert_eq!(state.pending[0].prepared_at, Sequence(1));
        assert!(
            matches!(&state.pending[0].intent, JournalIntent::Create { object: o, directory: false, .. } if *o == object),
            "the intent is what the reopen classifies, so it must survive verbatim"
        );
        assert!(state.pending[0].observed_result.is_none());
        assert!(!state.clean);
    }

    /// An `ObservedResult` annotates the entry and is *not* terminal.
    ///
    /// This is the shape a crash during rollback leaves under #65's abort
    /// ordering -- `prepare` journaled the intent, `observe_result` journaled
    /// the kernel's verdict, and the `Abort` record was never written because
    /// the undo never completed. The operation must still be pending, which is
    /// what makes the reopen poison by construction rather than by inference
    /// from an absent record.
    #[test]
    fn an_observed_result_annotates_a_pending_operation_without_terminating_it() {
        let scratch = scratch("observed");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let id = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Symlink {
                            object: ObjectId(uuid_v4()),
                            path: BytePath::new(b"/link".to_vec()).unwrap(),
                            target: BytePath::new(b"/target".to_vec()).unwrap(),
                        },
                    },
                ))
                .unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::ObservedResult {
                        outcome: OperationOutcome::Failure(umbra_core::Errno(28)),
                    },
                ))
                .unwrap();
            journal.flush(Sequence(2)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert_eq!(state.pending.len(), 1, "an observed result is not terminal");
        assert_eq!(
            state.pending[0].observed_result,
            Some(OperationOutcome::Failure(umbra_core::Errno(28)))
        );
        assert!(matches!(
            state.pending[0].intent,
            JournalIntent::Symlink { .. }
        ));
        assert!(!state.clean);
    }

    /// Either terminal record ends the operation for a reopen, and the pair is
    /// asserted together because that symmetry is what #65's abort reordering
    /// leans on. `Commit` means the transaction stood; `Abort` now means its
    /// recorded removals are durable, because `Overlay::abort` writes the record
    /// only after a successful unwind *and* a validated storage flush. An empty
    /// inventory is therefore an assertion, not an absence of information.
    #[test]
    fn either_terminal_record_clears_the_operation_from_the_reopen_inventory() {
        for (label, terminal) in [
            ("commit", JournalPayload::Commit),
            (
                "abort",
                JournalPayload::Abort {
                    reason: "KernelRefused(Errno(28))".into(),
                },
            ),
        ] {
            let scratch = scratch(&format!("terminal-{label}"));
            let dir = scratch.path();
            let run_id = RunId(uuid_v4());
            let id = OperationId(uuid_v4());
            {
                let mut journal = FileJournal::new();
                journal.open(&request(dir, run_id, 1)).unwrap();
                journal
                    .append(&record(
                        1,
                        id,
                        JournalPayload::Prepare {
                            intent: JournalIntent::Create {
                                object: ObjectId(uuid_v4()),
                                path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                                directory: true,
                                mode: 0o755,
                            },
                        },
                    ))
                    .unwrap();
                journal
                    .append(&record(
                        1,
                        id,
                        JournalPayload::ObservedResult {
                            outcome: OperationOutcome::Failure(umbra_core::Errno(28)),
                        },
                    ))
                    .unwrap();
                journal.append(&record(1, id, terminal)).unwrap();
                journal.flush(Sequence(3)).unwrap();
                journal.close().unwrap();
            }
            let mut journal = FileJournal::new();
            let state = journal.open(&request(dir, run_id, 2)).unwrap();
            assert!(
                state.pending.is_empty(),
                "{label}: a terminal record ends the operation"
            );
            assert!(state.clean, "{label}: nothing is left to reconcile");
            assert_ne!(
                state.last_valid_sequence,
                Sequence(0),
                "{label}: the journal is not pristine, which a reopen must now tolerate"
            );
        }
    }

    /// Two interleaved transactions, one terminated and one not. The reduction
    /// is per-operation, so the finished one leaves nothing behind and the
    /// unfinished one is reported with its own intent -- which is what lets a
    /// reopen poison for a single bad transaction in a journal full of good
    /// ones.
    #[test]
    fn interleaved_operations_are_reduced_independently_by_operation_id() {
        let scratch = scratch("interleaved");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let committed = OperationId(uuid_v4());
        let stranded = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    committed,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Unlink {
                            object: ObjectId(uuid_v4()),
                            path: BytePath::new(b"/gone".to_vec()).unwrap(),
                            directory: false,
                        },
                    },
                ))
                .unwrap();
            journal
                .append(&record(
                    1,
                    stranded,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Rename {
                            object: ObjectId(uuid_v4()),
                            from: BytePath::new(b"/source".to_vec()).unwrap(),
                            to: BytePath::new(b"/fresh/target".to_vec()).unwrap(),
                        },
                    },
                ))
                .unwrap();
            journal
                .append(&record(1, committed, JournalPayload::Commit))
                .unwrap();
            journal.flush(Sequence(3)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].operation_id, stranded);
        assert!(matches!(
            state.pending[0].intent,
            JournalIntent::Rename { .. }
        ));
        assert!(!state.clean);
    }

    /// The one verdict a reader cannot reconstruct: a prior session declaring
    /// the run unrecoverable.
    ///
    /// `Overlay::abort`'s uncorroborated path writes `RecoveryRequired` and then
    /// an `Abort` over a transaction whose creation it never undid
    /// ([#65](https://github.com/invakid404/umbra/issues/65), waiver (viii)).
    /// The `Abort` is terminal, so the inventory this reopen produces is
    /// **empty** and intact -- structurally indistinguishable from a run that
    /// finished cleanly. The latch is the only thing that tells them apart, which
    /// is why `clean` must go false on it too: `umbra-supervisor`'s own
    /// pristine-journal gate reads `clean` and would otherwise wave this through
    /// before `Overlay::bind` ever saw it.
    #[test]
    fn a_declared_recovery_requirement_survives_an_otherwise_empty_reopen() {
        let scratch = scratch("declared");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let id = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Create {
                            object: ObjectId(uuid_v4()),
                            path: BytePath::new(b"/fresh".to_vec()).unwrap(),
                            directory: false,
                            mode: 0o644,
                        },
                    },
                ))
                .unwrap();
            // The order `abort` uses: the declaration, then the terminal record
            // it qualifies.
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired),
                ))
                .unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Abort {
                        reason: "Cancelled".into(),
                    },
                ))
                .unwrap();
            journal.flush(Sequence(3)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert!(
            state.pending.is_empty(),
            "the Abort record is still terminal -- the inventory cannot carry this"
        );
        assert!(matches!(state.tail, JournalTailRecovery::Intact));
        assert!(
            state.recovery_required,
            "the declaration is what makes an empty inventory readable as 'do not trust this run'"
        );
        assert!(
            !state.clean,
            "a declared run is not clean, however empty its inventory looks"
        );
    }

    /// The other direction, so the latch cannot be mistaken for "any lifecycle
    /// record means trouble". An ordinary run journals `RunCompleted`, and a
    /// reopen of it stays clean.
    #[test]
    fn an_ordinary_lifecycle_record_does_not_declare_a_recovery_requirement() {
        let scratch = scratch("lifecycle-clean");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let id = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    id,
                    JournalPayload::Lifecycle(JournalLifecycle::RunCompleted {
                        through: Sequence(1),
                    }),
                ))
                .unwrap();
            journal.flush(Sequence(1)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert!(!state.recovery_required);
        assert!(state.clean);
        assert_ne!(state.last_valid_sequence, Sequence(0));
    }

    /// The latch is a latch: once declared, nothing later in the log clears it.
    /// A `Commit` for an unrelated transaction after the declaration must not
    /// launder the run.
    #[test]
    fn a_declared_recovery_requirement_is_never_cleared_by_later_records() {
        let scratch = scratch("latch");
        let dir = scratch.path();
        let run_id = RunId(uuid_v4());
        let declared = OperationId(uuid_v4());
        let later = OperationId(uuid_v4());
        {
            let mut journal = FileJournal::new();
            journal.open(&request(dir, run_id, 1)).unwrap();
            journal
                .append(&record(
                    1,
                    declared,
                    JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired),
                ))
                .unwrap();
            journal
                .append(&record(
                    1,
                    later,
                    JournalPayload::Prepare {
                        intent: JournalIntent::Unlink {
                            object: ObjectId(uuid_v4()),
                            path: BytePath::new(b"/gone".to_vec()).unwrap(),
                            directory: false,
                        },
                    },
                ))
                .unwrap();
            journal
                .append(&record(1, later, JournalPayload::Commit))
                .unwrap();
            journal.flush(Sequence(3)).unwrap();
            journal.close().unwrap();
        }
        let mut journal = FileJournal::new();
        let state = journal.open(&request(dir, run_id, 2)).unwrap();
        assert!(state.pending.is_empty());
        assert!(state.recovery_required, "the latch is never cleared");
        assert!(!state.clean);
    }

    fn uuid_v4() -> uuid::Uuid {
        uuid::Uuid::new_v4()
    }
}
