//! Worker-only abandonment after independent failure publication.
use super::{invalid, process::identity, worker::WorkerCleanup};
use std::{convert::Infallible, io};
use umbra_core::managed::{Participant, Role, RunIdentity};
use umbra_platform::provider::ManagedCleanup;

/// Created only from a registered worker entry that has accepted durable GO.
/// Custodian roles cannot acquire this capability.
pub struct WorkerBoundary {
    local: Participant,
    run: RunIdentity,
}

impl WorkerBoundary {
    /// Enrolled run identity carried by this worker-only capability.
    pub fn run_identity(&self) -> RunIdentity {
        self.run
    }
    /// Validate shared ownership without borrowing notification state.
    pub fn validates(&self, cleanup: &super::worker::SharedWorkerCleanup) -> bool {
        self.local.role == Role::Worker
            && identity(std::process::id(), Role::Worker).ok() == Some(self.local)
            && cleanup.belongs_to(self.run, self.local)
    }

    /// Fail-stop remains terminal when notification state is borrowed or panics.
    pub fn fail_stop_shared(
        self,
        cleanup: &super::worker::SharedWorkerCleanup,
        failure: &umbra_core::UmbraError,
    ) -> io::Result<Infallible> {
        if !self.validates(cleanup) {
            return Err(invalid("fail-stop outside enrolled worker"));
        }
        let mut cleanup = cleanup.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup.failed(failure);
        }));
        // SAFETY: identity and registered role were checked before borrowing.
        unsafe { libc::_exit(1) }
    }

    pub(super) fn new(local: Participant, run: RunIdentity) -> Self {
        Self { local, run }
    }

    /// Publish failure using the worker's independent routes, then end only
    /// this worker without unwinding storage or provider client destructors.
    /// Parent and provider custody remains with their separate processes.
    pub fn fail_stop(
        self,
        cleanup: &mut WorkerCleanup,
        failure: &umbra_core::UmbraError,
    ) -> io::Result<Infallible> {
        if self.local.role != Role::Worker
            || identity(std::process::id(), Role::Worker)? != self.local
            || !cleanup.belongs_to(self.run, self.local)
        {
            return Err(invalid("fail-stop outside enrolled worker"));
        }
        // Notification failure or panic cannot cause unwinding into the user's
        // provider/session owners. WorkerCleanup uses the original cutoff.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup.failed(failure);
        }));
        // SAFETY: terminates only this verified worker process. No parent or
        // provider handle is signalled, dropped, or claimed as collected.
        unsafe { libc::_exit(1) }
    }
}
