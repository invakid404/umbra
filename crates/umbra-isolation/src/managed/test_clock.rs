//! Scoped clock injection for multi-process protocol tests.
//!
//! The development dependency enables this feature for tests. An ordinary role
//! entry never installs a clock, so it always uses the OS clock.
use std::{cell::RefCell, io, rc::Rc};
use umbra_core::managed::{Participant, Ticket};

/// Read-only production guard state for shared-clock runtime qualification.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuardState {
    /// Original minimum, or no caller cleanup origin.
    pub ticket: Option<Ticket>,
    /// Deadline passed to notification reads, including the unbounded sentinel.
    pub receive_deadline: u64,
    /// Sticky protocol failure.
    pub failed: bool,
    /// Whether the original total window expired.
    pub expired: bool,
    /// Original stage cutoffs already emitted by the production polling loop.
    pub stage_events: Vec<u64>,
    /// Routes whose active publication has not received an ACK or failed.
    pub pending_publication: Vec<Participant>,
}

type Clock = Rc<dyn Fn() -> io::Result<u64>>;
thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

/// Restores the previous clock on the installing thread. It cannot cross threads.
pub struct Scope {
    prior: Option<Clock>,
}

/// Install an explicit test clock on this role's control thread. A multi-process
/// harness supplies the same atomic clock file to every role, rather than giving
/// each notification or process a new time origin.
pub fn install(clock: impl Fn() -> io::Result<u64> + 'static) -> Scope {
    Scope {
        prior: CLOCK.with(|slot| slot.replace(Some(Rc::new(clock)))),
    }
}

pub(super) fn read() -> Option<io::Result<u64>> {
    CLOCK
        .with(|slot| slot.borrow().clone())
        .map(|clock| clock())
}

impl Drop for Scope {
    fn drop(&mut self) {
        CLOCK.with(|slot| slot.replace(self.prior.take()));
    }
}
