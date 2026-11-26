//! Worker-only custody of namespace finalization.
use std::mem::ManuallyDrop;
use umbra_core::{CustodyFinishProof, ErrorKind, FinishRunReceipt, Result, RunId, UmbraError};
use umbra_isolation::managed::{fail_stop::WorkerBoundary, worker::SharedWorkerCleanup};
use umbra_overlay::NamespaceSession;

/// Keeps namespace destructors behind durable managed completion.
/// Abandonment ends the registered worker; its parents retain process custody.
pub struct RunOwner {
    generation: uuid::Uuid,
    run: umbra_core::managed::RunIdentity,
    namespace: Option<ManuallyDrop<Box<dyn NamespaceSession>>>,
    boundary: Option<WorkerBoundary>,
    cleanup: SharedWorkerCleanup,
    finish: crate::managed_finish::FinishArbitration,
    failure_reporter: Option<crate::managed_observer::Completion>,
}

impl RunOwner {
    /// Persist custody through the authoritative namespace and adopt the
    /// registered worker's runtime only after durable GO. From the first
    /// persistence attempt, no failure may unwind namespace destructors.
    pub fn arm_worker(
        run: RunId,
        namespace: Box<dyn NamespaceSession>,
        startup: &mut umbra_isolation::managed::worker_startup::WorkerStartup,
    ) -> Result<(Self, umbra_core::provider::Client, SharedWorkerCleanup)> {
        use umbra_core::managed::Role;
        use umbra_isolation::managed::{boot_nanos, process::identity};
        let local = identity(std::process::id(), Role::Worker)
            .map_err(|e| UmbraError::new(ErrorKind::InvalidState, "managed.arm", e.to_string()))?;
        if startup.entry.role != Role::Worker
            || !startup.entry.inventory().is_some_and(|inventory| {
                inventory
                    .members
                    .iter()
                    .any(|member| member.participant == local)
            })
            || startup.client.is_none()
        {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "managed.arm",
                "namespace arming outside registered worker",
            ));
        }
        let mut namespace = ManuallyDrop::new(namespace);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut generation = None;
            loop {
                let ready = startup.arm_and_accept_go(|record, deadline| {
                    if record.run_id != run || boot_nanos()? >= deadline {
                        return Err(std::io::Error::other(
                            "namespace custody identity or deadline mismatch",
                        ));
                    }
                    generation = Some(record.generation);
                    namespace.arm_custody(record).map_err(std::io::Error::other)
                })?;
                if ready {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let generation =
                generation.ok_or_else(|| std::io::Error::other("namespace was not armed here"))?;
            let (client, cleanup, boundary) = startup.take_runtime()?;
            if !boundary.validates(&cleanup) {
                return Err(std::io::Error::other("worker cleanup identity mismatch"));
            }
            let finish = crate::managed_finish::FinishArbitration::new(run, generation)
                .map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>((generation, client, cleanup, boundary, finish))
        }));
        let (generation, client, cleanup, boundary, finish) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(_)) | Err(_) => {
                let _ = startup.entry.fail_stop_worker_startup();
                std::process::abort();
            }
        };
        let owner = Self {
            generation,
            run: boundary.run_identity(),
            namespace: Some(namespace),
            boundary: Some(boundary),
            cleanup: cleanup.clone(),
            finish,
            failure_reporter: None,
        };
        Ok((owner, client, cleanup))
    }

    /// Install one best-effort final error exchange without releasing custody.
    pub fn set_failure_reporter(&mut self, reporter: crate::managed_observer::Completion) {
        self.failure_reporter = Some(reporter);
    }

    /// Notify cleanup before delivering the original failure under its cutoff.
    /// Reporter errors and unwinds cannot permit namespace destruction.
    pub fn report_failure(&mut self, failure: &UmbraError) {
        use umbra_platform::provider::ManagedCleanup;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cleanup.failed(failure);
            if let Some(report) = self.failure_reporter.take() {
                let _ = report(Err(failure.clone()));
            }
        }));
        if let Err(payload) = result {
            std::mem::forget(payload);
        }
    }

    /// Borrow namespace operations while the run owner retains finalization.
    pub fn namespace(&mut self) -> &mut (dyn NamespaceSession + 'static) {
        let namespace: &mut Box<dyn NamespaceSession> =
            self.namespace.as_mut().expect("active run owner");
        namespace.as_mut()
    }

    /// Borrow namespace state without transferring destructor ownership.
    pub fn namespace_ref(&self) -> &(dyn NamespaceSession + 'static) {
        self.namespace.as_ref().expect("active run owner").as_ref()
    }

    /// Release namespace destruction only after the matching durable receipt.
    pub fn finish_terminal(
        &mut self,
        terminal: &umbra_platform::shutdown::ManagedTerminal,
        root_result: i32,
    ) -> Result<FinishRunReceipt> {
        use sha2::{Digest, Sha256};
        if terminal.run() != self.run {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "managed.finish",
                "collection belongs to another run",
            ));
        }
        let mut digest = Sha256::new();
        digest.update(b"umbra-managed-finish-v1");
        digest.update(self.generation.as_bytes());
        digest.update(root_result.to_le_bytes());
        digest.update(terminal.encode()?);
        self.finish(&CustodyFinishProof {
            generation: self.generation,
            certificate: digest.finalize().into(),
            ledger_final: true,
            provider_collected: true,
            creators_terminal: true,
            cleanup_confirmed: true,
            root_result,
        })
    }

    /// Release namespace destruction only after the matching durable receipt.
    fn finish(&mut self, proof: &CustodyFinishProof) -> Result<FinishRunReceipt> {
        let namespace: &mut Box<dyn NamespaceSession> =
            self.namespace.as_mut().ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    "managed.owner",
                    "already completed",
                )
            })?;
        let receipt = self.finish.finish(namespace.as_mut(), proof)?;
        self.boundary.take();
        drop(ManuallyDrop::into_inner(self.namespace.take().unwrap()));
        Ok(receipt)
    }
}

impl Drop for RunOwner {
    fn drop(&mut self) {
        if let Some(boundary) = self.boundary.take() {
            let failure = UmbraError::new(
                ErrorKind::InvalidState,
                "managed.owner",
                "managed run abandoned before durable completion",
            );
            self.report_failure(&failure);
            let _ = self.cleanup.fail_stop(boundary, &failure);
            // Returning means the registered-worker invariant was violated.
            // Never fall through to namespace destruction.
            std::process::abort();
        }
    }
}
