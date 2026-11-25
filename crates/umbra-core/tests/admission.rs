use umbra_core::*;
use uuid::Uuid;

fn request(sequence: u64, operation: AdmissionOperation) -> AdmissionRequest {
    AdmissionRequest {
        nonce: Uuid::from_u128(1),
        generation: 1,
        sequence,
        operation,
    }
}

fn ledger() -> AdmissionLedger {
    AdmissionLedger::new(Uuid::from_u128(1), 1, 1).unwrap()
}

#[test]
fn r2_grant_visible_before_acquire() {
    let mut ledger = ledger();
    let grant = request(
        1,
        AdmissionOperation::Reserve {
            obligation: Uuid::from_u128(2),
        },
    );
    assert_eq!(ledger.apply(grant.clone()), Ok(1));
    // The receiver lost the reply: retransmission cannot allocate a second slot.
    assert_eq!(ledger.apply(grant), Ok(1));
    assert_eq!(
        ledger.apply(request(
            2,
            AdmissionOperation::Reserve {
                obligation: Uuid::from_u128(3),
            }
        )),
        Err(AdmissionError::Capacity {
            capacity: 1,
            current: 1
        })
    );
    assert_eq!(ledger.current(), 1);
}

#[test]
fn r2_freeze_races_grant() {
    for grant_first in [false, true] {
        let mut ledger = ledger();
        let reserve = AdmissionOperation::Reserve {
            obligation: Uuid::from_u128(2),
        };
        if grant_first {
            assert_eq!(ledger.apply(request(1, reserve)), Ok(1));
            assert_eq!(ledger.apply(request(2, AdmissionOperation::Freeze)), Ok(1));
        } else {
            assert_eq!(ledger.apply(request(1, AdmissionOperation::Freeze)), Ok(0));
            assert_eq!(
                ledger.apply(request(2, reserve)),
                Err(AdmissionError::Frozen)
            );
        }
        assert_eq!(
            ledger.apply(request(
                3,
                AdmissionOperation::Entering {
                    obligation: Uuid::from_u128(2),
                    resource: Uuid::from_u128(3),
                }
            )),
            Err(AdmissionError::Frozen)
        );
        assert_eq!(ledger.current(), u64::from(grant_first));
    }
}

#[test]
fn r2_release_requires_terminal_proof() {
    let mut ledger = ledger();
    let obligation = Uuid::from_u128(2);
    let resource = Uuid::from_u128(3);
    ledger
        .apply(request(1, AdmissionOperation::Reserve { obligation }))
        .unwrap();
    ledger
        .apply(request(
            2,
            AdmissionOperation::Entering {
                obligation,
                resource,
            },
        ))
        .unwrap();
    ledger
        .apply(request(3, AdmissionOperation::Retiring { obligation }))
        .unwrap();
    assert_eq!(
        ledger.apply(request(
            4,
            AdmissionOperation::Release {
                obligation,
                certificate: TerminalCertificate::Collected {
                    resource,
                    creator_terminal: true,
                    parent_collected: false,
                },
            }
        )),
        Err(AdmissionError::Invalid)
    );
    assert_eq!(ledger.current(), 1);
    assert!(ledger.failed());
    assert_eq!(
        ledger.apply(request(
            5,
            AdmissionOperation::Release {
                obligation,
                certificate: TerminalCertificate::Collected {
                    resource,
                    creator_terminal: true,
                    parent_collected: true,
                },
            }
        )),
        Ok(0)
    );
    assert!(ledger.failed(), "eventual collection cannot clear failure");
}

#[test]
fn r2_absent_stale_partial_inventory() {
    for (nonce, generation, sequence) in [
        (Uuid::nil(), 1, 1),
        (Uuid::from_u128(1), 2, 1),
        (Uuid::from_u128(1), 1, u64::MAX),
    ] {
        let mut ledger = ledger();
        let mut malformed = request(
            1,
            AdmissionOperation::Reserve {
                obligation: Uuid::from_u128(2),
            },
        );
        malformed.nonce = nonce;
        malformed.generation = generation;
        malformed.sequence = sequence;
        assert_eq!(ledger.apply(malformed), Err(AdmissionError::Invalid));
        assert!(ledger.failed());
        assert_eq!(ledger.current(), 0);
        assert_eq!(
            ledger.apply(request(
                1,
                AdmissionOperation::Reserve {
                    obligation: Uuid::from_u128(2),
                }
            )),
            Err(AdmissionError::Frozen)
        );
    }
}
