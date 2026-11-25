//! Ordered observer requests across the registered executable boundary.
use crate::run::{RunObserver, RunOutcome};
use umbra_core::{ErrorKind, Result, RunId, UmbraError};

/// One callback; acknowledgment is required before worker progress.
#[derive(serde::Serialize, serde::Deserialize)]
pub enum Event {
    /// Platform has adopted stdio.
    StdioAdopted,
    /// Storage is prepared, before target launch.
    Prepared,
    /// Successful durable finish.
    Finished(RunOutcome),
    /// Cleanup diagnostic.
    TeardownWarning(UmbraError),
}

/// Identity and sequence prevent cross-run or repeated callback delivery.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Request {
    /// Storage run identity.
    pub run_id: RunId,
    /// Consecutive request number starting at one.
    pub sequence: u64,
    /// Callback input.
    pub event: Event,
}

/// Exact acknowledgment, retaining structured observer failure.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Response {
    /// Same run as request.
    pub run_id: RunId,
    /// Same sequence as request.
    pub sequence: u64,
    /// Caller callback result.
    pub result: Result<()>,
}

/// Caller-owned callbacks. Protocol or callback failure permanently stops delivery.
pub struct CallerObserver {
    run_id: RunId,
    next: u64,
    observer: Option<Box<dyn RunObserver>>,
    failed: bool,
    adopted: bool,
    prepared: bool,
    finished: bool,
}

impl CallerObserver {
    /// Retain the original callbacks in their original process.
    pub fn new(run_id: RunId, observer: Option<Box<dyn RunObserver>>) -> Self {
        Self {
            run_id,
            next: 1,
            observer,
            failed: false,
            adopted: false,
            prepared: false,
            finished: false,
        }
    }

    /// Deliver only a fresh, ordered request for this run.
    pub fn dispatch(&mut self, request: Request) -> Result<Response> {
        let result = self.deliver(&request);
        if result.is_err() {
            self.failed = true;
        }
        Ok(Response {
            run_id: self.run_id,
            sequence: request.sequence,
            result,
        })
    }

    fn deliver(&mut self, request: &Request) -> Result<()> {
        let invalid = || {
            UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "run.observer",
                "invalid managed callback identity, sequence or phase",
            )
        };
        if self.failed
            || self.finished
            || request.run_id != self.run_id
            || request.sequence != self.next
        {
            return Err(invalid());
        }
        self.next = self.next.checked_add(1).ok_or_else(invalid)?;
        // Mark failed before calling user code so an unwind cannot permit replay.
        self.failed = true;
        match &request.event {
            Event::StdioAdopted => {
                if self.adopted || self.prepared {
                    return Err(invalid());
                }
                self.adopted = true;
                if let Some(observer) = &mut self.observer {
                    observer.stdio_adopted()?;
                }
            }
            Event::Prepared => {
                if !self.adopted || self.prepared {
                    return Err(invalid());
                }
                self.prepared = true;
                if let Some(observer) = &mut self.observer {
                    observer.prepared(self.run_id)?;
                }
            }
            Event::Finished(outcome) => {
                if !self.prepared || outcome.run_id != self.run_id {
                    return Err(invalid());
                }
                self.finished = true;
                if let Some(observer) = &mut self.observer {
                    observer.finished(outcome);
                }
            }
            Event::TeardownWarning(error) => {
                if !self.prepared {
                    return Err(invalid());
                }
                if let Some(observer) = &mut self.observer {
                    observer.teardown_warning(error);
                }
            }
        }
        self.failed = false;
        Ok(())
    }
}
