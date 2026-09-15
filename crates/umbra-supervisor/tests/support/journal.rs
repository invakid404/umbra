//! A purely in-memory [`Journal`] for the integration harness.
//!
//! The harness is an end-to-end *transaction-verdict* test, not an end-to-end
//! *run* test, and this file is why: nothing here can fail, nothing here reaches
//! a durability boundary, and `flush` returns a receipt the engine's own
//! validation accepts by construction. Do not add a durability assertion against
//! it — a journal that cannot fail proves nothing about ordering or persistence.
//! `umbra-journal-file`'s `FileJournal` is the object that boundary belongs to,
//! and swapping it in is a `Harness` constructor change plus one dev-dep.
//!
//! Deliberately smaller than `umbra-overlay`'s own `MemoryJournal`: the
//! `fail_prepare` / `fail_commit` / `fail_close` injection switches and the
//! `closes` counter are stripped, because no test here needs them and an unused
//! switch is an invitation to assert vacuously.
//!
//! The appended records are retained rather than counted, so the first test that
//! wants journal *shape* — "a reconciled abort journals `Prepare`,
//! `ObservedResult`, `Abort` and no `Commit`" is the obvious next one — only has
//! to hand a clone of `records` back out through `Harness`. Exposing that handle
//! before an assertion reads it would ship a dead accessor, so it is not
//! exposed yet.

use umbra_core::{
    Checkpoint, CheckpointId, DurableSequence, JournalOpenRequest, JournalRecord,
    JournalTailRecovery, LeaseEpoch, RecoveryState, Result, RunId, Sequence,
};
use umbra_journal::Journal;

/// The recovery inventory `Overlay::bind` validates.
///
/// Hand-built rather than read back from `Journal::open`, because `bind` never
/// calls `open`: it takes the inventory through `SessionConfig::recovery` and
/// checks it against the binding, the lease and its own refusal to reopen a
/// nonempty journal. An empty, intact, clean state is the only one `bind`
/// accepts today.
pub fn recovery(run_id: RunId) -> RecoveryState {
    RecoveryState {
        run_id,
        checkpoint: None,
        last_valid_sequence: Sequence(0),
        durable: None,
        pending: vec![],
        tail: JournalTailRecovery::Intact,
        clean: true,
    }
}

/// An ordered log in a `Vec`, with no framing, no checksums and no I/O.
pub struct MemoryJournal {
    records: Vec<JournalRecord>,
    run: RunId,
    epoch: LeaseEpoch,
}

impl MemoryJournal {
    /// Bind the log to the run and writer epoch whose receipts it must echo.
    ///
    /// `Overlay::flush_journal` rejects a receipt naming a different run or
    /// epoch, so these are not decoration: they are what makes `flush` valid.
    pub fn new(run: RunId, epoch: LeaseEpoch) -> Self {
        Self {
            records: Vec::new(),
            run,
            epoch,
        }
    }
}

impl Journal for MemoryJournal {
    fn open(&mut self, _: &JournalOpenRequest) -> Result<RecoveryState> {
        Ok(recovery(self.run))
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        let sequence = Sequence(self.records.len() as u64 + 1);
        let mut record = record.clone();
        record.sequence = sequence;
        self.records.push(record);
        Ok(sequence)
    }
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        assert!(
            through.0 <= self.records.len() as u64,
            "a flush beyond the log is an engine bug, not an injectable failure"
        );
        Ok(DurableSequence {
            run_id: self.run,
            writer_epoch: self.epoch,
            sequence: through,
        })
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.sequence.0 > after.0)
            .cloned()
            .map(Ok)
            .collect();
        Ok(Box::new(records.into_iter()))
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        Ok(checkpoint.id)
    }
    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
