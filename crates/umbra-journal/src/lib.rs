//! Backend-independent durable journal contract.
//!
//! Records and recovery DTOs belong to [`umbra_core`]. Implementations own framing,
//! checksums, persistence and recovery; this crate performs no storage or lease I/O.
//! See the crate README for backend qualification and handoff ordering.
//!
//! The interface supports runtime injection without selecting a backend:
//! ```
//! use umbra_journal::Journal;
//!
//! fn inject<T: Journal + 'static>(backend: T) -> Box<dyn Journal> {
//!     Box::new(backend)
//! }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use umbra_core::{
    Checkpoint, CheckpointId, DurableSequence, JournalOpenRequest, JournalRecord, RecoveryState,
    Result, Sequence,
};

/// Ordered, framed operation log and immutable checkpoint publication.
///
/// A supervisor owns each mutable session. Constructors are backend-specific and
/// live outside this object-safe interface. `Send` permits moving a session to a
/// journal worker; calls remain synchronous and ordered, with bounded work queues.
///
/// Writer authority is injected by the supervisor from storage. Implementations
/// must not acquire an independent competing lease. Every mutation, including
/// tail repair and checkpoint/reference publication, requires current authority.
/// Lease expiry alone never proves the previous writer is dead: takeover requires
/// effective fencing (including old writable kernel handles), or confirmed former
/// writer termination and quiescence. Reject unsafe takeover and stale epochs.
/// On lease loss, report a structured error so the supervisor blocks mutations
/// and quiesces the tracee tree before any further resume.
///
/// Expected failures use [`umbra_core::UmbraError`] categories, preserving context
/// and optional errno. Never turn corruption, disconnects or durability failures
/// into successful empty replay or an automatic fallback to local storage.
pub trait Journal: Send {
    /// Open the supplied runtime control-directory binding and recover valid state.
    ///
    /// Validate run identity, format compatibility, access intent and supplied
    /// writer identity/epoch before any writable opening or repair. Control files
    /// reside on the selected backing store, outside the tracee-visible `root/`.
    /// Runtime mount paths must not become persistent journal/checkpoint identity.
    /// Read-only opens must not repair, append or publish checkpoint files.
    ///
    /// Recover from the last valid checkpoint and validated log frames, exposing
    /// prepared-but-uncommitted operations for reconciliation by the namespace
    /// owner using stable IDs and idempotent storage operations. Recovery must not
    /// infer that a prepared operation committed or that a kernel write was undone.
    /// Report recoverable final-tail damage; interior corruption is an error.
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState>;

    /// Append one framed record and return its assigned sequence.
    ///
    /// Assign a monotonically increasing sequence in the opened run, persisting
    /// that assigned value along with format version, operation ID, writer epoch
    /// and typed payload. Validate the epoch against current writer authority.
    /// A file backend bounds payload length before allocating or writing and adds
    /// a CRC32 checksum. Document sequence assignment and retry/deduplication rules;
    /// an interrupted call must not be retried as though it certainly wrote nothing.
    ///
    /// Success means accepted append, **not durability**. Flush preparation records
    /// before irreversible mutation when crash recovery requires durable intent.
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence>;

    /// Make accepted records through `through` durable at the backing-store boundary.
    ///
    /// Return a receipt covering at least the requested sequence only after the
    /// promised persistence boundary succeeds. Reject a sequence beyond the log;
    /// propagate flush and lease failures without advancing the durable watermark.
    /// This receipt does not establish durability of tracee files or overlay state.
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence>;

    /// Iterate validated records with sequence strictly greater than `after`.
    ///
    /// Yield in increasing sequence order, using bounded buffering rather than
    /// loading the entire log. The iterator borrows this session, preventing
    /// interleaved mutable journal calls. IPC adapters use bounded pages/cursors
    /// and release cursor resources when the iterator is dropped.
    ///
    /// Check format, frame length, checksum and sequence ordering before yielding.
    /// Errors discovered while streaming are iterator errors, never silent EOF.
    /// A demonstrably incomplete final frame may be excluded under documented
    /// recovery rules; a complete frame with a bad checksum or interior corruption
    /// must fail. Physical truncation requires a writable, authorized recovery.
    /// Replay may include accepted records beyond a previously acknowledged durable
    /// watermark; their presence does not retroactively establish a flush receipt.
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>>;

    /// Durably publish an immutable checkpoint and its reference.
    ///
    /// Require its last committed sequence to have already been flushed. Validate
    /// run identity, format, fingerprints and writer authority. Snapshot contents
    /// are logical, process-independent state, never mount roots or live pointers.
    /// Publish snapshot contents durably before their reference so crash recovery
    /// cannot select an incomplete snapshot; return only after both are durable.
    ///
    /// A clean marker additionally requires the supervisor to reject new children
    /// and mutations, quiesce the tree, arrange agent exit/remaining-child handling,
    /// reconcile overlay transactions, and flush tracee files, backing storage,
    /// journal and manifest. Journal success alone does not prove clean handoff.
    /// Release the fenced writer lease only after clean publication succeeds.
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId>;

    /// Finish the session, surfacing errors from flushing pending writer records.
    ///
    /// Read-only close does not write. Never silently discard failed flushes or
    /// publish a clean marker merely because close was requested. Lease release
    /// remains the supervisor/storage owner's responsibility; dropping an object
    /// cannot substitute for observing this method's result.
    fn close(&mut self) -> Result<()>;
}

const _: Option<&dyn Journal> = None;

/// Versioned provider protocol, server harness and trait proxies.
#[cfg(unix)]
pub mod provider;
