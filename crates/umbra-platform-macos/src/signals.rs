//! Single-use signal authority. A possibly-sent packet can never be retried.
use umbra_core::{ErrorKind, Result, SignalToken, TaskId, ThreadId, UmbraError};

#[derive(Default)]
pub(crate) struct Delivery {
    sequence: u64,
    token: Option<SignalToken>,
    closed: bool,
}

fn refused() -> UmbraError {
    UmbraError::new(
        ErrorKind::UnsupportedCapability,
        "macos",
        "signal-stop-identity: missing, stale, or consumed delivery authority",
    )
}

impl Delivery {
    pub(crate) fn pending(&self) -> bool {
        self.token.is_some()
    }
    pub(crate) fn closed(&self) -> bool {
        self.closed
    }
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.token = None;
    }
    pub(crate) fn invalidate(&mut self) {
        self.token = None;
    }

    pub(crate) fn issue(
        &mut self,
        task: TaskId,
        thread: ThreadId,
        exec_generation: u64,
        signal: i32,
    ) -> Result<SignalToken> {
        if self.closed || self.pending() {
            return Err(refused());
        }
        self.sequence = self.sequence.checked_add(1).ok_or_else(refused)?;
        let token = SignalToken {
            task,
            thread,
            exec_generation,
            sequence: self.sequence,
            signal,
        };
        self.token = Some(token);
        Ok(token)
    }

    pub(crate) fn send(
        &mut self,
        token: SignalToken,
        task: TaskId,
        thread: ThreadId,
        exec_generation: u64,
        signal: Option<i32>,
        send: impl FnOnce(&str) -> Result<()>,
    ) -> Result<()> {
        if self.closed
            || self.token != Some(token)
            || token.task != task
            || token.thread != thread
            || token.exec_generation != exec_generation
            || signal != Some(token.signal)
        {
            return Err(refused());
        }
        // Close before invoking the transport. Error/unwind leaves it closed.
        self.close();
        send(&format!(
            "vCont;C{:02x}:{:x};c",
            token.signal, token.thread.0.native_id
        ))?;
        self.closed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::TaskIdentity;

    #[test]
    fn f284_delivery_errors() {
        let task = TaskId(TaskIdentity {
            native_id: 41,
            generation: 2,
        });
        let thread = ThreadId(TaskIdentity {
            native_id: 42,
            generation: 2,
        });
        let mut state = Delivery::default();
        let token = state.issue(task, thread, 3, 30).unwrap();
        let mut sends = 0;
        assert!(state
            .send(token, task, thread, 4, Some(30), |_| {
                sends += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(sends, 0, "stale exec must not reach transport");
        assert!(state
            .send(token, task, thread, 3, Some(30), |packet| {
                assert_eq!(packet, "vCont;C1e:2a;c");
                sends += 1;
                Err(UmbraError::new(ErrorKind::Io, "send", "ambiguous write"))
            })
            .is_err());
        assert!(state.closed());
        assert!(state
            .send(token, task, thread, 3, Some(30), |_| {
                sends += 1;
                Ok(())
            })
            .is_err());
        assert!(state.issue(task, thread, 3, 30).is_err());
        assert_eq!(sends, 1);

        let mut success = Delivery::default();
        let token = success.issue(task, thread, 3, 30).unwrap();
        success
            .send(token, task, thread, 3, Some(30), |_| Ok(()))
            .unwrap();
        assert!(!success.closed());
        assert!(success
            .send(token, task, thread, 3, Some(30), |_| panic!("duplicate"))
            .is_err());
        let later = success.issue(task, thread, 3, 30).unwrap();
        assert_ne!(later.sequence, token.sequence);
        success.invalidate();
        assert!(success
            .send(later, task, thread, 3, Some(30), |_| panic!("exec stale"))
            .is_err());
        success.close();
        assert!(success.issue(task, thread, 4, 30).is_err());
    }
}
