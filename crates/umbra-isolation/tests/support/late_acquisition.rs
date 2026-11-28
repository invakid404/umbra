//! A held acquisition returns across the original tail. Only the actual
//! creator waits for its child; the outer census never supplies that proof.
use std::{
    io,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use umbra_core::{
    managed::{CleanupPath, Ticket},
    shutdown::{Identity, Nonce, Proof, ResourceKind},
    AdmissionError, AdmissionLedger, AdmissionOperation, AdmissionRequest, AdmissionTransport,
};
use umbra_platform::{admission::Admission, shutdown::Ledger};

struct Transport {
    owner: Arc<Mutex<AdmissionLedger>>,
    clock: super::SharedClock,
    sequence: u64,
}

impl AdmissionTransport for Transport {
    fn exchange(
        &mut self,
        operation: AdmissionOperation,
        deadline: u64,
    ) -> io::Result<Result<u64, AdmissionError>> {
        let _scope = self.clock.install();
        if super::boot_nanos()? >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.sequence += 1;
        Ok(self.owner.lock().unwrap().apply(AdmissionRequest {
            nonce: Nonce::from_u128(1),
            generation: 1,
            sequence: self.sequence,
            operation,
        }))
    }
}

struct OwnedChild(Child);
impl OwnedChild {
    fn spawn() -> Self {
        Self(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Unwind protection belongs to the actual parent. A panic still fails
        // the test; this wait is never used to certify a production receipt.
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

pub fn run(created_before_tail: bool) {
    let ticket = Ticket::new(super::boot_nanos().unwrap(), CleanupPath::Failed, 1).unwrap();
    let clock = super::SharedClock::new(ticket.started);
    let owner = Arc::new(Mutex::new(
        AdmissionLedger::new(Nonce::from_u128(1), 1, 1).unwrap(),
    ));
    let ledger = Ledger::new();
    ledger
        .install_admission(
            Admission::new(Box::new(Transport {
                owner: owner.clone(),
                clock: clock.clone(),
                sequence: 0,
            })),
            Box::new(move || Ok(ticket.deadline)),
        )
        .unwrap();
    let slot = ledger.reserve(ResourceKind::Tracee, None, 1, 0).unwrap();
    let initial_charge = owner.lock().unwrap().current();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let creator = std::thread::spawn(move || {
        let mut child = created_before_tail.then(OwnedChild::spawn);
        ready_tx.send(child.as_ref().map(|c| c.0.id())).unwrap();
        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let child = child.get_or_insert_with(OwnedChild::spawn);
        let participant = super::identity(child.0.id(), super::Role::Provider).unwrap();
        let acquisition = slot.acquired(Identity {
            pid: Some(child.0.id() as i32),
            birth: Some(participant.birth),
        });
        acquired_tx.send(acquisition.is_ok()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(child.0.stdin.take());
        let status = child.0.wait().unwrap();
        let retirement = slot.confirm(Proof::Accounted {
            evidence: format!("creator parent wait: {status}"),
            root_owned: true,
            root_collected: status.success(),
        });
        (status, acquisition, retirement)
    });
    let early_pid = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    ledger.freeze_admission().unwrap();
    let accepted = ledger
        .freeze(
            Nonce::new_v4(),
            Duration::from_nanos(ticket.deadline - ticket.started),
        )
        .unwrap();
    clock.advance_to(ticket.cutoffs(1).unwrap().complete + 1);
    // The creator has not returned an identity. No PID from this inventory
    // can justify collection, whether the primitive has created a child yet.
    let tail = ledger.final_snapshot().unwrap();
    let tail_confirmed = accepted.confirms(&tail);
    let tail_charge = owner.lock().unwrap().current();
    let new_acquisition_rejected = ledger.reserve(ResourceKind::Tracee, None, 2, 0).is_err();
    resume_tx.send(()).unwrap();
    let acquired = acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let late = ledger.final_snapshot();
    finish_tx.send(()).unwrap();
    let (status, acquisition, retirement) = creator.join().unwrap();

    // Semantic assertions follow the real parent wait, also under mutations.
    assert_eq!(early_pid.is_some(), created_before_tail);
    assert_eq!(
        initial_charge, 1,
        "creator entered without a capacity charge"
    );
    assert_eq!(tail.entries.len(), 1, "expired reservation was erased");
    assert!(tail.entries[0].identity.is_none());
    assert!(
        !tail_confirmed.unwrap(),
        "empty outer census falsely confirmed the creator"
    );
    assert_eq!(
        tail_charge, 1,
        "unknown creator lost its charge at the tail"
    );
    assert!(new_acquisition_rejected);
    assert!(acquired, "{acquisition:?}");
    assert!(!accepted.confirms(&late.unwrap()).unwrap());
    assert!(status.success(), "actual parent wait failed: {status}");
    assert!(
        retirement.is_err(),
        "late release renewed the original cutoff"
    );
    assert_eq!(owner.lock().unwrap().current(), 1);
    assert!(
        ledger.final_snapshot().is_err(),
        "late proof erased failed admission"
    );
    assert!(
        !accepted.confirms(&tail).unwrap(),
        "late wait rewrote the tail result"
    );
}
