//! Provider-side grants retained through native creation and terminal proof.
//! Dropping a grant never returns capacity to the cleanup owner.

use std::sync::{Arc, Mutex};
use umbra_core::shutdown::Nonce;
use umbra_core::{
    AdmissionError, AdmissionOperation, AdmissionTransport, ErrorKind, Result, TerminalCertificate,
    UmbraError,
};

struct State {
    transport: Box<dyn AdmissionTransport>,
    failed: bool,
}

/// One run's serialized connection to its cleanup-owner admission ledger.
#[derive(Clone)]
pub struct Admission(Arc<Mutex<State>>);

impl Admission {
    /// Adopt an already enrolled independent endpoint before acquiring resources.
    pub fn new(transport: Box<dyn AdmissionTransport>) -> Self {
        Self(Arc::new(Mutex::new(State {
            transport,
            failed: false,
        })))
    }

    fn exchange(&self, operation: AdmissionOperation, deadline: u64) -> Result<u64> {
        self.execute(operation, deadline)?.map_err(refusal)
    }

    fn execute(
        &self,
        operation: AdmissionOperation,
        deadline: u64,
    ) -> Result<std::result::Result<u64, AdmissionError>> {
        let mut state = self.0.lock().map_err(|_| {
            UmbraError::new(
                ErrorKind::InvalidState,
                "admission",
                "poisoned admission owner",
            )
        })?;
        if state.failed {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "admission",
                "prior ambiguous admission transaction",
            ));
        }
        match state.transport.exchange(operation, deadline) {
            Ok(result) => {
                if result == Err(AdmissionError::Invalid) {
                    state.failed = true;
                }
                Ok(result)
            }
            Err(error) => {
                state.failed = true;
                Err(UmbraError::new(
                    ErrorKind::Io,
                    "admission",
                    error.to_string(),
                ))
            }
        }
    }

    /// Obtain capacity before preparation effects. The returned grant still
    /// requires an acknowledged Entering before creation can begin.
    pub fn reserve(&self, deadline: u64) -> Result<Grant> {
        let obligation = Nonce::new_v4();
        self.exchange(AdmissionOperation::Reserve { obligation }, deadline)?;
        Ok(Grant {
            admission: self.clone(),
            obligation,
            resource: Nonce::new_v4(),
            phase: Phase::Reserved,
        })
    }

    /// Serialize freeze before taking the native cleanup census.
    pub fn freeze(&self, deadline: u64) -> Result<u64> {
        self.exchange(AdmissionOperation::Freeze, deadline)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Reserved,
    Entering,
    Entered,
    Retiring,
    Released,
}

/// Capacity obligation retained through a single program's native lifecycle.
/// It owns no native handles and has no automatic release or cancellation.
pub struct Grant {
    admission: Admission,
    obligation: Nonce,
    resource: Nonce,
    phase: Phase,
}

/// One native program's charge, including its debugger and owned task rights.
/// Overlapping independent replacements use a separate program charge.
pub struct ProgramGrant {
    grant: Grant,
    slots: Vec<umbra_core::shutdown::SlotId>,
    retired: bool,
}

impl ProgramGrant {
    /// Bind an acknowledged entry to its first local resource reservation.
    pub fn new(grant: Grant, root: umbra_core::shutdown::SlotId) -> Result<Self> {
        if grant.phase != Phase::Entered {
            return Err(grant.invalid("native group without acknowledged entry"));
        }
        Ok(Self {
            grant,
            slots: vec![root],
            retired: false,
        })
    }

    /// Include a debugger or task right before its acquisition. Late attachment
    /// to a released program is forbidden.
    pub fn attach(&mut self, slot: umbra_core::shutdown::SlotId) -> Result<()> {
        if self.retired || slot.instance != self.slots[0].instance || self.slots.contains(&slot) {
            return Err(self.grant.invalid("invalid native admission attachment"));
        }
        self.slots.push(slot);
        Ok(())
    }

    /// Release only after every member has identity-bound terminal proof and
    /// the creator can no longer return another resource. Missing entries,
    /// exit notifications and partial collection all retain the charge.
    pub fn settle(
        &mut self,
        entries: &[umbra_core::shutdown::Entry],
        creator_terminal: bool,
        deadline: impl FnOnce() -> Result<u64>,
    ) -> Result<bool> {
        if self.retired {
            return Ok(true);
        }
        if !creator_terminal {
            return Ok(false);
        }
        let mut never_created = true;
        for slot in &self.slots {
            let mut matches = entries.iter().filter(|entry| entry.slot.id == *slot);
            let Some(entry) = matches.next() else {
                return Ok(false);
            };
            if matches.next().is_some() || !entry.confirms() {
                return Ok(false);
            }
            never_created &= matches!(
                entry.disposition,
                umbra_core::shutdown::Disposition::NeverCreated
            );
        }
        let deadline = deadline()?;
        if never_created {
            self.grant.creation_failed(deadline)?;
        } else {
            self.grant.collected(deadline)?;
        }
        self.retired = true;
        Ok(true)
    }
}

impl Grant {
    /// Commit entry before the first creation-capable operation. A failed
    /// exchange leaves this grant unusable for acquisition.
    pub fn enter(&mut self, deadline: u64) -> Result<()> {
        if self.phase != Phase::Reserved {
            return Err(self.invalid("duplicate or ambiguous acquisition entry"));
        }
        self.phase = Phase::Entering;
        let result = self.admission.execute(
            AdmissionOperation::Entering {
                obligation: self.obligation,
                resource: self.resource,
            },
            deadline,
        )?;
        if let Err(error) = result {
            if error == AdmissionError::Frozen {
                // An exact refusal proves entry was never committed. This is
                // distinct from an absent acknowledgement.
                self.phase = Phase::Reserved;
            }
            return Err(refusal(error));
        }
        self.phase = Phase::Entered;
        Ok(())
    }

    /// Retire a grant that was never submitted for entry. The caller must have
    /// abandoned every path that could subsequently start this acquisition.
    pub fn cancel_before_entry(&mut self, deadline: u64) -> Result<()> {
        if self.phase != Phase::Reserved {
            return Err(self.invalid("cancellation after entry submission"));
        }
        self.release(TerminalCertificate::CancelledBeforeEntry, deadline)
    }

    /// Retire definitive creation failure after all creation-capable work has
    /// stopped. Unknown or late creation is not evidence for this operation.
    pub fn creation_failed(&mut self, deadline: u64) -> Result<()> {
        if self.phase != Phase::Entered {
            return Err(self.invalid("creation failure without acknowledged entry"));
        }
        self.release(
            TerminalCertificate::NoResource {
                resource: self.resource,
                creator_terminal: true,
            },
            deadline,
        )
    }

    /// Retire only after the program, debugger children and owned rights have
    /// terminal proofs, and every creator for this unit has stopped.
    pub fn collected(&mut self, deadline: u64) -> Result<()> {
        if self.phase != Phase::Entered {
            return Err(self.invalid("collection without acknowledged entry"));
        }
        self.release(
            TerminalCertificate::Collected {
                resource: self.resource,
                creator_terminal: true,
                parent_collected: true,
            },
            deadline,
        )
    }

    fn release(&mut self, certificate: TerminalCertificate, deadline: u64) -> Result<()> {
        self.phase = Phase::Retiring;
        self.admission.exchange(
            AdmissionOperation::Retiring {
                obligation: self.obligation,
            },
            deadline,
        )?;
        self.admission.exchange(
            AdmissionOperation::Release {
                obligation: self.obligation,
                certificate,
            },
            deadline,
        )?;
        self.phase = Phase::Released;
        Ok(())
    }

    fn invalid(&self, reason: &str) -> UmbraError {
        UmbraError::new(ErrorKind::InvalidState, "admission", reason)
    }
}

fn refusal(error: AdmissionError) -> UmbraError {
    match error {
        AdmissionError::Capacity { capacity, current } => UmbraError::new(
            ErrorKind::Denied,
            "admission",
            format!("cleanup capacity C={capacity} exhausted; current={current}"),
        ),
        AdmissionError::Frozen => UmbraError::new(
            ErrorKind::InvalidState,
            "admission",
            "cleanup admission is frozen",
        ),
        AdmissionError::Invalid => UmbraError::new(
            ErrorKind::ProtocolMismatch,
            "admission",
            "owner rejected admission identity or transition",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use umbra_core::{AdmissionLedger, AdmissionRequest};

    struct Model {
        ledger: AdmissionLedger,
        lose_entry_reply: bool,
    }
    struct Transport {
        model: Arc<Mutex<Model>>,
        sequence: u64,
    }
    impl AdmissionTransport for Transport {
        fn exchange(
            &mut self,
            operation: AdmissionOperation,
            _deadline: u64,
        ) -> io::Result<std::result::Result<u64, AdmissionError>> {
            self.sequence += 1;
            let entering = matches!(operation, AdmissionOperation::Entering { .. });
            let mut model = self.model.lock().unwrap();
            let result = model.ledger.apply(AdmissionRequest {
                nonce: Nonce::from_u128(1),
                generation: 1,
                sequence: self.sequence,
                operation,
            });
            if entering && model.lose_entry_reply {
                Err(io::ErrorKind::UnexpectedEof.into())
            } else {
                Ok(result)
            }
        }
    }
    fn model(lose_entry_reply: bool) -> (Admission, Arc<Mutex<Model>>) {
        let model = Arc::new(Mutex::new(Model {
            ledger: AdmissionLedger::new(Nonce::from_u128(1), 1, 1).unwrap(),
            lose_entry_reply,
        }));
        let admission = Admission::new(Box::new(Transport {
            model: model.clone(),
            sequence: 0,
        }));
        (admission, model)
    }

    #[test]
    fn capacity_recycles_only_after_explicit_terminal_proof() {
        let (admission, model) = model(false);
        for _ in 0..3 {
            let mut grant = admission.reserve(u64::MAX).unwrap();
            grant.enter(u64::MAX).unwrap();
            assert!(admission.reserve(u64::MAX).is_err());
            grant.creation_failed(u64::MAX).unwrap();
            assert_eq!(model.lock().unwrap().ledger.current(), 0);
        }
        let grant = admission.reserve(u64::MAX).unwrap();
        drop(grant);
        assert_eq!(model.lock().unwrap().ledger.current(), 1);
        assert!(admission.reserve(u64::MAX).is_err());
    }

    #[test]
    fn native_ledger_retains_program_until_last_owned_right_is_released() {
        use umbra_core::shutdown::{Identity, Proof, ResourceKind};
        let (admission, state) = model(false);
        let ledger = crate::shutdown::Ledger::new();
        let deadline_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = deadline_reads.clone();
        ledger
            .install_admission(
                admission,
                Box::new(move || {
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(u64::MAX)
                }),
            )
            .unwrap();
        let root = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
        let right = ledger
            .reserve(ResourceKind::TaskRight, Some(root.id()), 1, 0)
            .unwrap();
        assert_eq!(state.lock().unwrap().ledger.current(), 1);
        root.acquired(Identity {
            pid: Some(123),
            birth: None,
        })
        .unwrap();
        right
            .acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
        root.confirm(Proof::Accounted {
            evidence: "unit model exact parent wait".into(),
            root_owned: true,
            root_collected: true,
        })
        .unwrap();
        assert_eq!(state.lock().unwrap().ledger.current(), 1);
        right.confirm(Proof::Released).unwrap();
        assert_eq!(state.lock().unwrap().ledger.current(), 0);
        assert_eq!(
            deadline_reads.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "only admission and proven retirement may request a cutoff"
        );
        assert!(ledger
            .reserve(ResourceKind::TaskRight, Some(root.id()), 1, 0)
            .is_err());
    }

    #[test]
    fn frozen_entry_refusal_can_retire_but_lost_entry_reply_cannot() {
        let (admission, state) = model(false);
        let mut grant = admission.reserve(u64::MAX).unwrap();
        admission.freeze(u64::MAX).unwrap();
        assert!(grant.enter(u64::MAX).is_err());
        grant.cancel_before_entry(u64::MAX).unwrap();
        assert_eq!(state.lock().unwrap().ledger.current(), 0);

        let (admission, state) = model(true);
        let mut grant = admission.reserve(u64::MAX).unwrap();
        assert!(grant.enter(u64::MAX).is_err());
        assert!(grant.cancel_before_entry(u64::MAX).is_err());
        assert!(grant.creation_failed(u64::MAX).is_err());
        assert!(admission.reserve(u64::MAX).is_err());
        assert_eq!(state.lock().unwrap().ledger.current(), 1);
    }
}
