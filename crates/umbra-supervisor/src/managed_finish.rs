//! Durable finish arbitration for a managed worker.
use umbra_core::{CustodyFinishProof, ErrorKind, FinishRunReceipt, Result, RunId, UmbraError};
use umbra_overlay::NamespaceSession;

/// Retains one exact terminal proof across ambiguous namespace responses.
pub struct FinishArbitration {
    run: RunId,
    generation: uuid::Uuid,
    proof: Option<CustodyFinishProof>,
    prepared: bool,
    completed: Option<FinishRunReceipt>,
}

impl FinishArbitration {
    /// Bind finalization to the run generation whose Armed record was persisted.
    pub fn new(run: RunId, generation: uuid::Uuid) -> Result<Self> {
        if generation.is_nil() {
            return Err(invalid("missing custody generation"));
        }
        Ok(Self {
            run,
            generation,
            proof: None,
            prepared: false,
            completed: None,
        })
    }

    /// Prepare and commit only the original, fully confirmed proof.
    /// An ambiguous response retains state; a later call sends the identical
    /// request to the namespace's idempotent custody operations.
    pub fn finish(
        &mut self,
        namespace: &mut dyn NamespaceSession,
        proof: &CustodyFinishProof,
    ) -> Result<FinishRunReceipt> {
        if proof.generation != self.generation
            || !proof.ledger_final
            || !proof.provider_collected
            || !proof.creators_terminal
            || !proof.cleanup_confirmed
        {
            return Err(invalid("managed finish lacks terminal collection proof"));
        }
        if self
            .proof
            .as_ref()
            .is_some_and(|original| original != proof)
        {
            return Err(invalid(
                "managed finish proof changed after preparation began",
            ));
        }
        if let Some(receipt) = &self.completed {
            return Ok(receipt.clone());
        }
        self.proof.get_or_insert_with(|| proof.clone());
        if !self.prepared {
            namespace.prepare_finish(proof)?;
            self.prepared = true;
        }
        let receipt = namespace.commit_custody(proof)?;
        if receipt.run_id != self.run {
            return Err(invalid("managed completion receipt names another run"));
        }
        self.completed = Some(receipt.clone());
        Ok(receipt)
    }
}

fn invalid(message: &'static str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, "managed.finish", message)
}
