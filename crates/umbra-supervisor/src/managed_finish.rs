//! Durable finish arbitration for a managed worker.
use umbra_core::{
    CustodyFinishProof, CustodyPhase, ErrorKind, FinishRunReceipt, LeaseEpoch, Result, RunId,
    UmbraError,
};
use umbra_overlay::NamespaceSession;

/// Retains one exact terminal proof across ambiguous namespace responses.
pub struct FinishArbitration {
    run: RunId,
    generation: uuid::Uuid,
    proof: Option<CustodyFinishProof>,
    epoch: Option<LeaseEpoch>,
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
            epoch: None,
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
        if self.epoch.is_none() {
            let state = namespace.query_custody(self.run, self.generation)?;
            if state.record.run_id != self.run
                || state.record.generation != self.generation
                || !matches!(state.record.state, CustodyPhase::Armed { .. })
            {
                return Err(invalid(
                    "managed finish is not bound to its Armed generation",
                ));
            }
            self.epoch = Some(state.writer_epoch);
        }
        if !self.prepared {
            let prepared = namespace.prepare_finish(proof).and_then(|sequence| {
                if cfg!(feature = "probe-managed-finish-reply-loss") {
                    Err(invalid("injected lost PrepareFinish response"))
                } else {
                    Ok(sequence)
                }
            });
            if let Err(primary) = prepared {
                self.reconcile(namespace, proof, false, primary)?;
            }
            self.prepared = true;
        }
        let committed = namespace.commit_custody(proof).and_then(|receipt| {
            if cfg!(feature = "probe-managed-finish-reply-loss") {
                Err(invalid("injected lost CommitCustody response"))
            } else {
                Ok(receipt)
            }
        });
        let receipt = match committed {
            Ok(receipt) => receipt,
            Err(primary) => {
                self.reconcile(namespace, proof, true, primary.clone())?;
                // One exact replay retrieves the retained receipt. It cannot
                // introduce a new certificate or repeat terminal side effects.
                namespace.commit_custody(proof).map_err(|retry| {
                    let mut error = primary;
                    error
                        .context
                        .push_str(&format!("; exact commit replay: {retry}"));
                    error
                })?
            }
        };
        if receipt.run_id != self.run
            || receipt.durability.run_id != self.run
            || Some(receipt.durability.writer_epoch) != self.epoch
        {
            return Err(invalid(
                "managed completion receipt names another run or writer",
            ));
        }
        self.completed = Some(receipt.clone());
        Ok(receipt)
    }

    fn reconcile(
        &self,
        namespace: &mut dyn NamespaceSession,
        proof: &CustodyFinishProof,
        committed: bool,
        mut primary: UmbraError,
    ) -> Result<()> {
        let result = (|| {
            let state = namespace.query_custody(self.run, self.generation)?;
            if state.record.run_id != self.run
                || state.record.generation != self.generation
                || Some(state.writer_epoch) != self.epoch
            {
                return Err(invalid("custody reconciliation identity changed"));
            }
            let matches = match state.record.state {
                CustodyPhase::FinishPrepared {
                    certificate,
                    root_result,
                } if !committed => {
                    certificate == proof.certificate && root_result == proof.root_result
                }
                CustodyPhase::Complete { certificate } => certificate == proof.certificate,
                _ => false,
            };
            if !matches {
                return Err(invalid(
                    "custody reconciliation lacks the exact durable decision",
                ));
            }
            Ok(())
        })();
        result.map_err(|error| {
            primary
                .context
                .push_str(&format!("; custody reconciliation: {error}"));
            primary
        })
    }
}

fn invalid(message: &'static str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidState, "managed.finish", message)
}

#[cfg(test)]
#[path = "managed_finish_tests.rs"]
mod tests;
