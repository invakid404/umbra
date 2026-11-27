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
    /// Final worker return value, distinct from successful-finish callbacks.
    RunResult(Result<()>),
    /// Guardian's actual worker wait receipt, after managed and session cleanup.
    WorkerExit(i32),
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
        mut wait: impl FnMut(&umbra_isolation::managed::Channel, u64) -> Result<()>,
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
                    self.failed = false;
                    return Ok(response);
                }
            }
            wait(&self.channel, deadline)?;
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

/// Parent forwarding on the existing child and parent bootstrap channels.
/// Each poll leaves time for the parent's independent cleanup guard.
#[cfg(target_os = "macos")]
pub struct Forwarder {
    child: umbra_isolation::managed::Channel,
    parent: umbra_isolation::managed::Channel,
    run_id: RunId,
    sequence: u64,
    phase: u8,
    deadline: u64,
    failed: bool,
    terminal: bool,
}

#[cfg(target_os = "macos")]
impl Forwarder {
    /// Close the child exchange while retaining only a possible terminal report.
    pub fn into_exit_reporter(self) -> ExitReporter {
        ExitReporter {
            allowed: self.phase == 0 && self.child.at_frame_boundary(),
            completed: self.terminal,
            parent: self.parent,
            run_id: self.run_id,
            sequence: self.sequence,
        }
    }
}

/// Guardian's existing parent channel after the worker exchange has closed.
#[cfg(target_os = "macos")]
pub struct ExitReporter {
    allowed: bool,
    completed: bool,
    parent: umbra_isolation::managed::Channel,
    run_id: RunId,
    sequence: u64,
}

#[cfg(target_os = "macos")]
impl ExitReporter {
    /// Publish a failed worker's actual wait receipt after collection and sweep.
    /// An interrupted callback cannot be replaced with a new terminal message.
    /// This makes one nonblocking flush attempt; it allocates no new wait budget.
    pub fn report_worker_exit(&mut self, status: i32, deadline: u64) -> Result<()> {
        // A final worker result already forwarded retains its structured error.
        if self.completed {
            return Ok(());
        }
        if !self.allowed {
            return Err(transport_error("worker exit interrupted an observer frame"));
        }
        self.parent
            .queue(&Request {
                run_id: self.run_id,
                sequence: self.sequence,
                event: Event::WorkerExit(status),
            })
            .map_err(transport_error)?;
        if !self.parent.flush(deadline).map_err(transport_error)? {
            return Err(transport_error("worker exit report would block"));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Forwarder {
    /// Adopt only the channels for the observer's worker-to-caller path.
    pub fn new(
        run_id: RunId,
        child: umbra_isolation::managed::Channel,
        parent: umbra_isolation::managed::Channel,
    ) -> Self {
        Self {
            child,
            parent,
            run_id,
            sequence: 1,
            phase: 0,
            deadline: u64::MAX,
            failed: false,
            terminal: false,
        }
    }

    /// Borrow the active phase's socket and retained cutoff for the parent wait.
    pub fn wait_interest(&self) -> Option<(std::os::fd::BorrowedFd<'_>, bool, u64)> {
        if self.failed {
            return None;
        }
        let channel = match self.phase {
            0 | 3 => &self.child,
            1 | 2 => &self.parent,
            4 => return None,
            _ => unreachable!(),
        };
        let (fd, writing) = channel.wait_interest();
        Some((fd, writing, self.deadline))
    }

    /// Advance at most one request/response phase under the original cutoff.
    pub fn poll(&mut self, cutoff: Option<u64>) -> Result<()> {
        if self.failed {
            return Err(transport_error("observer forwarder already failed"));
        }
        if self.phase == 4 {
            return Ok(());
        }
        self.failed = true;
        if let Some(cutoff) = cutoff {
            self.deadline = self.deadline.min(cutoff);
        }
        match self.phase {
            0 => {
                if let Some(request) = self
                    .child
                    .receive::<Request>(self.deadline)
                    .map_err(transport_error)?
                {
                    if request.run_id != self.run_id || request.sequence != self.sequence {
                        return Err(transport_error("forwarded observer request mismatch"));
                    }
                    self.terminal =
                        matches!(request.event, Event::RunResult(_) | Event::WorkerExit(_));
                    self.parent.queue(&request).map_err(transport_error)?;
                    self.phase = 1;
                }
            }
            1 => {
                if self.parent.flush(self.deadline).map_err(transport_error)? {
                    self.phase = 2;
                }
            }
            2 => {
                if let Some(response) = self
                    .parent
                    .receive::<Response>(self.deadline)
                    .map_err(transport_error)?
                {
                    if response.run_id != self.run_id || response.sequence != self.sequence {
                        return Err(transport_error("forwarded observer response mismatch"));
                    }
                    self.child.queue(&response).map_err(transport_error)?;
                    self.phase = 3;
                }
            }
            3 => {
                if self.child.flush(self.deadline).map_err(transport_error)? {
                    self.sequence = self
                        .sequence
                        .checked_add(1)
                        .ok_or_else(|| transport_error("observer sequence overflow"))?;
                    self.phase = if self.terminal { 4 } else { 0 };
                }
            }
            _ => unreachable!(),
        }
        self.failed = false;
        Ok(())
    }
}

/// Caller polling side. It owns callbacks and one bounded pending response.
#[cfg(target_os = "macos")]
pub struct CallerTransport {
    channel: umbra_isolation::managed::Channel,
    observer: CallerObserver,
    pending: bool,
    failed: bool,
    deadline: u64,
    complete: bool,
}

#[cfg(target_os = "macos")]
impl CallerTransport {
    /// Guardian wait receipt; never inferred from a helper's exit signal.
    pub fn worker_status_handle(&self) -> std::rc::Rc<std::cell::Cell<Option<i32>>> {
        self.observer.worker_status.clone()
    }
    /// Observe the final return without moving callback ownership.
    pub fn result_handle(&self) -> std::rc::Rc<std::cell::RefCell<Option<Result<()>>>> {
        self.observer.result_handle()
    }

    /// Adopt a routed observer channel without additional descriptors.
    pub fn new(channel: umbra_isolation::managed::Channel, observer: CallerObserver) -> Self {
        Self {
            channel,
            observer,
            pending: false,
            failed: false,
            deadline: u64::MAX,
            complete: false,
        }
    }

    /// A completed response needs no further readiness after its bytes drain.
    pub fn wait_interest(&self) -> Option<(std::os::fd::BorrowedFd<'_>, bool, u64)> {
        if self.failed || (self.complete && !self.pending) {
            return None;
        }
        let (fd, writing) = self.channel.wait_interest();
        Some((fd, writing, self.deadline))
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
        if self.complete {
            self.failed = false;
            return Ok(());
        }
        if let Some(request) = self
            .channel
            .receive::<Request>(self.deadline)
            .map_err(transport_error)?
        {
            let terminal = matches!(request.event, Event::RunResult(_) | Event::WorkerExit(_));
            let response = self.observer.dispatch(request)?;
            self.complete = terminal && response.result.is_ok();
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
    result: std::rc::Rc<std::cell::RefCell<Option<Result<()>>>>,
    worker_status: std::rc::Rc<std::cell::Cell<Option<i32>>>,
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
            result: Default::default(),
            worker_status: Default::default(),
        }
    }

    /// Final worker result; it is evidence only alongside successful parent waits.
    pub fn result_handle(&self) -> std::rc::Rc<std::cell::RefCell<Option<Result<()>>>> {
        self.result.clone()
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
        let terminal = matches!(request.event, Event::RunResult(_) | Event::WorkerExit(_));
        if ((self.failed || self.finished) && !terminal)
            || self.result.borrow().is_some()
            || self.worker_status.get().is_some()
            || request.run_id != self.run_id
            || request.sequence != self.next
        {
            return Err(invalid());
        }
        self.next = self.next.checked_add(1).ok_or_else(invalid)?;
        if let Event::WorkerExit(status) = request.event {
            use std::os::unix::process::ExitStatusExt;
            let status_value = std::process::ExitStatus::from_raw(status);
            if status_value.success()
                || (status_value.code().is_none() && status_value.signal().is_none())
            {
                return Err(invalid());
            }
            self.worker_status.set(Some(status));
            return Ok(());
        }
        if let Event::RunResult(result) = &request.event {
            if result.is_ok() && (self.failed || !self.finished) {
                return Err(invalid());
            }
            *self.result.borrow_mut() = Some(result.clone());
            return Ok(());
        }
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
            Event::RunResult(_) | Event::WorkerExit(_) => unreachable!(),
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

/// Shared adapter retains the same sequence through the worker's final return.
pub struct SharedObserver<F>(pub std::rc::Rc<std::cell::RefCell<WorkerObserver<F>>>);
impl<F: FnMut(Request) -> Result<Response>> RunObserver for SharedObserver<F> {
    fn stdio_adopted(&mut self) -> Result<()> {
        self.0.borrow_mut().stdio_adopted()
    }
    fn prepared(&mut self, run_id: RunId) -> Result<()> {
        self.0.borrow_mut().prepared(run_id)
    }
    fn finished(&mut self, outcome: &RunOutcome) {
        self.0.borrow_mut().finished(outcome);
    }
    fn teardown_warning(&mut self, error: &UmbraError) {
        self.0.borrow_mut().teardown_warning(error);
    }
}

/// One final send after staged worker execution returns.
pub type Completion = Box<dyn FnOnce(Result<()>) -> Result<()>>;

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

    /// A failed callback still permits reporting the failed run, never success.
    pub fn complete(&mut self, result: Result<()>) -> Result<()> {
        if self.failed && result.is_ok() {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "run.observer",
                "success after observer failure",
            ));
        }
        self.failed = false;
        self.call(Event::RunResult(result))
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
    #[test]
    fn worker_wait_receipt_is_terminal_and_cannot_report_success() {
        let run = RunId(uuid::Uuid::new_v4());
        for status in [0, 0x137f] {
            let mut caller = CallerObserver::new(run, None);
            assert!(caller
                .dispatch(Request {
                    run_id: run,
                    sequence: 1,
                    event: Event::WorkerExit(status),
                })
                .unwrap()
                .result
                .is_err());
            assert_eq!(caller.worker_status.get(), None);
        }
        let mut caller = CallerObserver::new(run, None);
        caller
            .dispatch(Request {
                run_id: run,
                sequence: 1,
                event: Event::WorkerExit(9),
            })
            .unwrap()
            .result
            .unwrap();
        assert_eq!(caller.worker_status.get(), Some(9));
        assert!(caller
            .dispatch(Request {
                run_id: run,
                sequence: 2,
                event: Event::WorkerExit(9),
            })
            .unwrap()
            .result
            .is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn observer_wait_interest_follows_pending_response() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use umbra_isolation::managed::Channel;
        let (child, _child_peer) = UnixStream::pair().unwrap();
        let (parent, _parent_peer) = UnixStream::pair().unwrap();
        let child_fd = child.as_raw_fd();
        let parent_fd = parent.as_raw_fd();
        let mut forward = super::Forwarder::new(
            umbra_core::RunId(uuid::Uuid::new_v4()),
            Channel::new(child).unwrap(),
            Channel::new(parent).unwrap(),
        );
        assert_eq!(forward.wait_interest().unwrap().0.as_raw_fd(), child_fd);
        forward.parent.queue(&42u64).unwrap();
        forward.phase = 1;
        let (fd, writing, cutoff) = forward.wait_interest().unwrap();
        assert_eq!(fd.as_raw_fd(), parent_fd);
        assert!(writing);
        assert_eq!(cutoff, u64::MAX);
        forward.deadline = 123;
        assert_eq!(forward.wait_interest().unwrap().2, 123);
        forward.phase = 4;
        assert!(forward.wait_interest().is_none());
    }

    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn channel_exchange_delivers_final_result_before_clean_eof() {
        use std::os::unix::net::UnixStream;
        use umbra_isolation::managed::{boot_nanos, Channel};
        let run = RunId(uuid::Uuid::new_v4());
        let (worker, caller) = UnixStream::pair().unwrap();
        let deadline = boot_nanos().unwrap() + 5_000_000_000;
        let parent = std::thread::spawn(move || {
            let observer = CallerObserver::new(run, None);
            let result = observer.result_handle();
            let mut caller = CallerTransport::new(Channel::new(caller).unwrap(), observer);
            while !caller.complete || caller.pending {
                caller.poll(Some(deadline)).unwrap();
                std::thread::yield_now();
            }
            assert!(matches!(result.borrow().as_ref(), Some(Ok(()))));
            // Completed exchange must not turn EOF into a protocol failure.
            caller.poll(Some(deadline)).unwrap();
        });
        let mut transport = WorkerTransport::new(Channel::new(worker).unwrap());
        let mut worker = WorkerObserver::new(run, |request| {
            transport.exchange(
                request,
                || Ok(Some(deadline)),
                |channel, cutoff| channel.wait_until(cutoff).map_err(transport_error),
            )
        });
        worker.stdio_adopted().unwrap();
        worker.prepared(run).unwrap();
        worker.finished(&RunOutcome {
            run_id: run,
            root_status: Some(umbra_core::ExitStatus::Code(0)),
            processes_exited: 1,
        });
        worker.complete(Ok(())).unwrap();
        drop(transport);
        parent.join().unwrap();
    }

    #[test]
    fn success_result_requires_finished_callback_and_is_not_replayable() {
        let run = RunId(uuid::Uuid::new_v4());
        let mut caller = CallerObserver::new(run, None);
        let response = caller
            .dispatch(Request {
                run_id: run,
                sequence: 1,
                event: Event::RunResult(Ok(())),
            })
            .unwrap();
        assert!(response.result.is_err());
        assert!(caller.result_handle().borrow().is_none());
        let failure = UmbraError::new(ErrorKind::InvalidState, "test", "failed run");
        caller
            .dispatch(Request {
                run_id: run,
                sequence: 2,
                event: Event::RunResult(Err(failure)),
            })
            .unwrap()
            .result
            .unwrap();
        assert!(caller
            .dispatch(Request {
                run_id: run,
                sequence: 3,
                event: Event::RunResult(Ok(()))
            })
            .unwrap()
            .result
            .is_err());
    }

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
