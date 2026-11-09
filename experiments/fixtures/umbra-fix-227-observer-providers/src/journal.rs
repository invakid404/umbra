//! Observes requests to the real journal and can fail its explicit Close.
use std::io::Write;
use umbra_core::*;
use umbra_journal::Journal;

struct Observed {
    inner: umbra_journal_file::FileJournal,
    log: std::fs::File,
    fail_close: bool,
}
impl Observed {
    fn record(&mut self, text: &str) -> Result<()> {
        writeln!(self.log, "{text}")
            .map_err(|e| UmbraError::new(ErrorKind::Io, "fixture.record", e.to_string()))
    }
}
impl Journal for Observed {
    fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState> {
        self.record("Open")?;
        self.inner.open(request)
    }
    fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
        self.record(
            if matches!(
                record.payload,
                JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
            ) {
                "Completed"
            } else {
                "Append"
            },
        )?;
        self.inner.append(record)
    }
    fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
        self.record("Flush")?;
        self.inner.flush(through)
    }
    fn replay(
        &mut self,
        after: Sequence,
    ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
        self.record("Replay")?;
        self.inner.replay(after)
    }
    fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
        self.record("Checkpoint")?;
        self.inner.write_checkpoint(checkpoint)
    }
    fn close(&mut self) -> Result<()> {
        self.record("Close")?;
        self.inner.close()?;
        if self.fail_close {
            return Err(UmbraError::new(
                ErrorKind::Io,
                "fixture.close",
                "injected journal close",
            ));
        }
        Ok(())
    }
}
fn main() {
    let result = umbra_journal::provider::serve_provider("file", |options| {
        let text = std::str::from_utf8(options).map_err(|e| {
            UmbraError::new(ErrorKind::InvalidInput, "fixture.options", e.to_string())
        })?;
        let (mode, path) = text.split_once('\n').ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidInput,
                "fixture.options",
                "mode and log path required",
            )
        })?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| UmbraError::new(ErrorKind::Io, "fixture.log", e.to_string()))?;
        Ok(Observed {
            inner: umbra_journal_file::FileJournal::new(),
            log,
            fail_close: mode == "fail-close",
        })
    });
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
