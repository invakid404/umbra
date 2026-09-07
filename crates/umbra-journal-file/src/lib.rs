//! File-backed journal scaffold for handoff §9 and buildout specification §7.
//!
//! The intended backend writes a bounded, CRC32-checksummed append-only framed log
//! and immutable snapshots beneath the injected runtime control directory on the
//! selected backing store. Frames carry a format version, assigned monotonic
//! sequence, operation ID, writer epoch, payload length, and typed record payload.
//! Runtime mount paths must never become persistent record identity.
//!
//! Implementation requirements:
//! - Validate format, run identity, access mode, and injected writer authority before
//!   opening files for mutation. Storage owns the lease; this backend acquires none.
//! - Separate accepted appends from durable flush receipts and surface close errors.
//! - Stream validated replay with bounded buffering. Exclude only an incomplete
//!   final frame; complete checksum failures and interior corruption are errors.
//!   Tail repair requires writable access and current writer authority.
//! - Recover prepared-but-uncommitted operations for explicit reconciliation, without
//!   assuming that an interrupted kernel mutation was committed or undone.
//! - Require the checkpoint sequence to be flushed, then durably publish the immutable
//!   snapshot before its reference. Journal persistence alone cannot prove clean
//!   handoff or NFS durability; tracee/storage flushes and fencing remain required.
//!
//! This is a stub: every journal operation returns a structured not-implemented error.
//! It performs no file I/O and supplies no durability or recovery guarantees.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_core::{
    Checkpoint, CheckpointId, DurableSequence, JournalOpenRequest, JournalRecord, RecoveryState,
    Result, Sequence, UmbraError,
};
use umbra_journal::Journal;

/// Placeholder for the file-based framed journal, with no active file session.
///
/// Construction performs no I/O. The control-directory binding and writer authority
/// will be supplied through [`Journal::open`], rather than a storage dependency.
#[derive(Debug, Default)]
pub struct FileJournal {
    _private: (),
}

impl FileJournal {
    /// Construct an unopened journal stub.
    pub fn new() -> Self {
        Self::default()
    }
}

fn not_implemented(operation: &str) -> UmbraError {
    UmbraError::new(
        umbra_core::ErrorKind::NotImplemented,
        operation,
        "file journal is a stub; framed file I/O, recovery, and durability are not implemented",
    )
}

impl Journal for FileJournal {
    fn open(&mut self, _request: &JournalOpenRequest) -> Result<RecoveryState> {
        Err(not_implemented("journal_file.open"))
    }

    fn append(&mut self, _record: &JournalRecord) -> Result<Sequence> {
        Err(not_implemented("journal_file.append"))
    }

    fn flush(&mut self, _through: Sequence) -> Result<DurableSequence> {
        Err(not_implemented("journal_file.flush"))
    }

    fn replay(
        &mut self,
        _after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        Err(not_implemented("journal_file.replay"))
    }

    fn write_checkpoint(&mut self, _checkpoint: &Checkpoint) -> Result<CheckpointId> {
        Err(not_implemented("journal_file.write_checkpoint"))
    }

    fn close(&mut self) -> Result<()> {
        Err(not_implemented("journal_file.close"))
    }
}
