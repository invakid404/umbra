//! Shared ownership of the existing journal for a terminal recovery marker.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use umbra_core::*;
use umbra_journal::Journal;

#[derive(Clone)]
pub(crate) struct TerminalJournal {
    journal: Arc<Mutex<Box<dyn Journal>>>,
    epoch: LeaseEpoch,
}

impl TerminalJournal {
    pub(crate) fn wrap(journal: Box<dyn Journal>, epoch: LeaseEpoch) -> (Box<dyn Journal>, Self) {
        let terminal = Self { journal: Arc::new(Mutex::new(journal)), epoch };
        (Box::new(terminal.clone()), terminal)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Box<dyn Journal>>> {
        self.journal.lock().map_err(|_| UmbraError::new(
            ErrorKind::InvalidState, "run.shutdown-journal", "journal owner poisoned"))
    }

    pub(crate) fn recovery_required(&self) -> Result<()> {
        let mut journal = self.lock()?;
        let sequence = journal.append(&JournalRecord {
            format_version: 1,
            sequence: Sequence(0),
            operation_id: OperationId(uuid::Uuid::new_v4()),
            writer_epoch: self.epoch,
            payload: JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired),
        })?;
        journal.flush(sequence)?;
        Ok(())
    }
}

impl Journal for TerminalJournal {
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState> {
        self.lock()?.open(request)
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        self.lock()?.append(record)
    }
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        self.lock()?.flush(through)
    }
    fn replay(&mut self, after: Sequence) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        // Replay retains the mutable borrow of this forwarding session. The
        // terminal handle is private and used only after supervision stops.
        Ok(Box::new(Replay { owner: self.clone(), after, page: VecDeque::new(), done: false }))
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        self.lock()?.write_checkpoint(checkpoint)
    }
    fn close(&mut self) -> Result<()> { self.lock()?.close() }
}

struct Replay {
    owner: TerminalJournal,
    after: Sequence,
    page: VecDeque<Result<JournalRecord>>,
    done: bool,
}
impl Iterator for Replay {
    type Item = Result<JournalRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.page.is_empty() && !self.done {
            let result = (|| {
                let mut journal = self.owner.lock()?;
                let page = journal.replay(self.after)?.take(128).collect::<VecDeque<_>>();
                Ok(page)
            })();
            match result {
                Ok(page) => { self.done = page.len() < 128; self.page = page; }
                Err(error) => { self.done = true; return Some(Err(error)); }
            }
        }
        let next = self.page.pop_front()?;
        match &next {
            Ok(record) => self.after = record.sequence,
            Err(_) => { self.done = true; self.page.clear(); }
        }
        Some(next)
    }
}
