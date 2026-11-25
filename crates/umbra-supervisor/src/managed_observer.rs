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

/// Nonblocking transport on an exclusively transferred managed channel.
#[cfg(target_os = "macos")]
pub struct WorkerTransport {
    channel: umbra_isolation::managed::Channel,
    failed: bool,
}

#[cfg(target_os = "macos")]
impl WorkerTransport {
    /// Adopt an existing channel; never create or duplicate a descriptor.
    pub fn new(channel: umbra_isolation::managed::Channel) -> Self {
        Self {
            channel,
            failed: false,
        }
    }

    /// Keep notification processing active during observer delivery. `progress`
    /// returns the original cleanup cutoff when one exists; it cannot renew it.
    pub fn exchange(
        &mut self,
        request: Request,
        mut progress: impl FnMut() -> Result<Option<u64>>,
    ) -> Result<Response> {
        if self.failed {
            return Err(transport_error("observer transport already failed"));
        }
        self.failed = true;
        self.channel.queue(&request).map_err(transport_error)?;
        let mut deadline = u64::MAX;
        let mut flushed = false;
        loop {
            if let Some(cutoff) = progress()? {
                deadline = deadline.min(cutoff);
            }
            if !flushed {
                flushed = self.channel.flush(deadline).map_err(transport_error)?;
            }
            if flushed {
                if let Some(response) = self
                    .channel
                    .receive::<Response>(deadline)
                    .map_err(transport_error)?
                {
                    if response.run_id != request.run_id || response.sequence != request.sequence {
                        return Err(transport_error("observer acknowledgment mismatch"));
                    }
                    self.failed = response.result.is_err();
                    return Ok(response);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(target_os = "macos")]
fn transport_error(error: impl std::fmt::Display) -> UmbraError {
    UmbraError::new(
        ErrorKind::ProtocolMismatch,
        "run.observer",
        error.to_string(),
    )
}

/// Caller polling side. It owns callbacks and one bounded pending response.
#[cfg(target_os = "macos")]
pub struct CallerTransport {
    channel: umbra_isolation::managed::Channel,
    observer: CallerObserver,
    pending: bool,
    failed: bool,
    deadline: u64,
}

#[cfg(target_os = "macos")]
impl CallerTransport {
    /// Adopt a routed observer channel without additional descriptors.
    pub fn new(channel: umbra_isolation::managed::Channel, observer: CallerObserver) -> Self {
        Self {
            channel,
            observer,
            pending: false,
            failed: false,
            deadline: u64::MAX,
        }
    }

    /// One nonblocking step, sharing the caller runtime's original cutoff.
    pub fn poll(&mut self, cutoff: Option<u64>) -> Result<()> {
        if self.failed {
            return Err(transport_error("caller observer transport already failed"));
        }
        self.failed = true;
        if let Some(cutoff) = cutoff {
            self.deadline = self.deadline.min(cutoff);
        }
        if self.pending {
            if !self.channel.flush(self.deadline).map_err(transport_error)? {
                self.failed = false;
                return Ok(());
            }
            self.pending = false;
        }
        if let Some(request) = self
            .channel
            .receive::<Request>(self.deadline)
            .map_err(transport_error)?
        {
            let response = self.observer.dispatch(request)?;
            self.channel.queue(&response).map_err(transport_error)?;
            self.pending = !self.channel.flush(self.deadline).map_err(transport_error)?;
        }
        self.failed = false;
        Ok(())
    }
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
        let result = if matches!(request.event, Event::StdioAdopted | Event::Prepared) {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.deliver(&request)))
            {
                Ok(result) => result,
                Err(payload) => {
                    std::mem::forget(payload);
                    Err(UmbraError::new(
                        ErrorKind::InvalidState,
                        "run.observer",
                        "managed preparation observer panicked",
                    ))
                }
            }
        } else {
            self.deliver(&request)
        };
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

/// Worker adapter. The exchange must drive transport and cleanup notifications
/// together; it must not create an independent run or inactivity deadline.
pub struct WorkerObserver<F> {
    run_id: RunId,
    next: u64,
    exchange: F,
    failed: bool,
}

impl<F: FnMut(Request) -> Result<Response>> WorkerObserver<F> {
    /// Bind an exclusive request/response exchange to one storage run.
    pub fn new(run_id: RunId, exchange: F) -> Self {
        Self {
            run_id,
            next: 1,
            exchange,
            failed: false,
        }
    }

    fn call(&mut self, event: Event) -> Result<()> {
        let invalid = || {
            UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "run.observer",
                "managed observer exchange failed or acknowledgment mismatched",
            )
        };
        if self.failed {
            return Err(invalid());
        }
        self.failed = true;
        let sequence = self.next;
        self.next = self.next.checked_add(1).ok_or_else(invalid)?;
        let response = (self.exchange)(Request {
            run_id: self.run_id,
            sequence,
            event,
        })?;
        if response.run_id != self.run_id || response.sequence != sequence {
            return Err(invalid());
        }
        response.result?;
        self.failed = false;
        Ok(())
    }
}

impl<F: FnMut(Request) -> Result<Response>> RunObserver for WorkerObserver<F> {
    fn stdio_adopted(&mut self) -> Result<()> {
        self.call(Event::StdioAdopted)
    }
    fn prepared(&mut self, run_id: RunId) -> Result<()> {
        if run_id != self.run_id {
            self.failed = true;
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "run.observer",
                "prepared run mismatch",
            ));
        }
        self.call(Event::Prepared)
    }
    fn finished(&mut self, outcome: &RunOutcome) {
        if let Err(error) = self.call(Event::Finished(outcome.clone())) {
            // These callbacks have no Result return. Preserve the existing
            // unwind behavior so managed ownership cannot report success.
            std::panic::panic_any(error);
        }
    }
    fn teardown_warning(&mut self, error: &UmbraError) {
        if let Err(error) = self.call(Event::TeardownWarning(error.clone())) {
            std::panic::panic_any(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_acknowledgment_stops_further_callbacks() {
        let run = RunId(uuid::Uuid::new_v4());
        let mut calls = 0;
        let mut worker = WorkerObserver::new(run, |request: Request| {
            calls += 1;
            Ok(Response {
                run_id: request.run_id,
                sequence: request.sequence + 1,
                result: Ok(()),
            })
        });
        assert!(worker.stdio_adopted().is_err());
        assert!(worker.prepared(run).is_err());
        assert_eq!(calls, 1);
    }

    struct Panics;
    impl RunObserver for Panics {
        fn prepared(&mut self, _: RunId) -> Result<()> {
            panic!("observer failure")
        }
        fn finished(&mut self, _: &RunOutcome) {
            panic!("unexpected finished")
        }
        fn teardown_warning(&mut self, _: &UmbraError) {
            panic!("unexpected warning")
        }
    }

    #[test]
    fn preparation_panic_is_reported_and_cannot_be_replayed() {
        let run = RunId(uuid::Uuid::new_v4());
        let mut caller = CallerObserver::new(run, Some(Box::new(Panics)));
        let mut worker = WorkerObserver::new(run, |request| caller.dispatch(request));
        worker.stdio_adopted().unwrap();
        let error = worker.prepared(run).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidState);
        assert_eq!(error.operation, "run.observer");
        assert!(worker.prepared(run).is_err());
    }
}
