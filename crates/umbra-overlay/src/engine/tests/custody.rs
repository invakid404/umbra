use super::*;
use umbra_journal_file::FileJournal;

fn setup() -> (Fixture, JournalOpenRequest, CustodyFinishProof) {
    setup_with_arm(true)
}

fn setup_with_arm(arm: bool) -> (Fixture, JournalOpenRequest, CustodyFinishProof) {
    setup_fixture(Fixture::new(&[]), arm)
}

fn setup_fixture(mut f: Fixture, arm: bool) -> (Fixture, JournalOpenRequest, CustodyFinishProof) {
    let config = f.overlay.config().unwrap();
    let run_id = config.context.run_id;
    let request = JournalOpenRequest {
        control: JournalControlBinding {
            run_id,
            directory: PhysicalPath(config.binding.control.physical_path.clone().unwrap()),
        },
        access: JournalAccess::Writer(JournalWriterAuthority {
            run_id,
            writer_instance_id: "custody-test".into(),
            host_fingerprint: "test".into(),
            supervisor_version: "test".into(),
            acquired_at: 0,
            renewed_at: 0,
            lease_epoch: config.lease.epoch,
            fencing: JournalFencingEvidence::Fenced {
                mechanism: "local writer claim".into(),
                evidence: vec![],
            },
        }),
        format: JournalFormatPolicy {
            readable_versions: vec![1],
            write_version: 1,
        },
    };
    let mut journal = FileJournal::new();
    assert!(journal.open(&request).unwrap().clean);
    f.overlay.journal = Box::new(journal);
    let generation = Uuid::new_v4();
    if arm {
        f.overlay
            .arm_custody(CustodyRecord {
                run_id,
                generation,
                sequence: 1,
                state: CustodyPhase::Armed {
                    capacity: 64,
                    startup_nonce: Uuid::new_v4(),
                    participants: vec![CustodyParticipant {
                        pid: std::process::id(),
                        birth: 1,
                    }],
                },
            })
            .unwrap();
    }
    let proof = CustodyFinishProof {
        generation,
        certificate: [7; 32],
        ledger_final: true,
        provider_collected: true,
        creators_terminal: true,
        cleanup_confirmed: true,
        root_result: 0,
    };
    (f, request, proof)
}

#[test]
fn r2_child_checkpoint_writer() {
    let Ok(encoded) = std::env::var("UMBRA_FIX_299_JOURNAL_REQUEST") else {
        return;
    };
    let request: JournalOpenRequest = umbra_core::provider::decode(encoded.as_bytes()).unwrap();
    let JournalAccess::Writer(authority) = &request.access else {
        panic!("writer required")
    };
    let mut journal = FileJournal::new();
    journal.open(&request).unwrap();
    let sequence = journal
        .append(&JournalRecord {
            format_version: 1,
            sequence: Sequence(0),
            operation_id: OperationId(Uuid::new_v4()),
            writer_epoch: authority.lease_epoch,
            payload: JournalPayload::Custody(CustodyRecord {
                run_id: request.control.run_id,
                generation: Uuid::new_v4(),
                sequence: 1,
                state: CustodyPhase::Armed {
                    capacity: 64,
                    startup_nonce: Uuid::new_v4(),
                    participants: vec![CustodyParticipant {
                        pid: std::process::id(),
                        birth: 1,
                    }],
                },
            }),
        })
        .unwrap();
    journal.flush(sequence).unwrap();
    // Simulate abrupt worker completion: no journal close or Rust destructors.
    std::process::exit(0);
}

#[test]
fn r2_uncertain_armed_append_retains_authority() {
    uncertain_arm(false);
}

#[test]
fn r2_armed_requires_durable_receipt() {
    uncertain_arm(true);
}

fn uncertain_arm(fail_flush: bool) {
    let mut f = Fixture::new(&[]);
    let run_id = f.overlay.config().unwrap().context.run_id;
    {
        let mut log = f.log.lock().unwrap();
        log.fail_custody = !fail_flush;
        log.fail_flush = fail_flush;
    }
    assert!(f
        .overlay
        .arm_custody(CustodyRecord {
            run_id,
            generation: Uuid::new_v4(),
            sequence: 1,
            state: CustodyPhase::Armed {
                capacity: 64,
                startup_nonce: Uuid::new_v4(),
                participants: vec![CustodyParticipant {
                    pid: std::process::id(),
                    birth: 1
                }],
            },
        })
        .is_err());
    assert!(f
        .overlay
        .fail_run(&FailedRunRequest {
            run_id,
            reason: "uncertain append".into(),
            tree_terminated: true,
        })
        .is_err());
    assert!(f.overlay.config.is_some());
    assert!(f
        .overlay
        .storage
        .acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: WriterId("contender".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .is_err());
}

#[test]
fn r2_reopen_armed_before_go() {
    let (mut f, request, _) = setup_with_arm(false);
    f.overlay.journal.close().unwrap();
    let encoded = String::from_utf8(umbra_core::provider::encode(&request).unwrap()).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "engine::tests::custody::r2_child_checkpoint_writer",
            "--nocapture",
        ])
        .env("UMBRA_FIX_299_JOURNAL_REQUEST", encoded)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("checkpoint child exceeded five seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let recovered = reopen(&request);
    assert!(!recovered.clean);
    let mut journal = FileJournal::new();
    journal.open(&request).unwrap();
    f.overlay.journal = Box::new(journal);
    let mut config = f.overlay.config.take().unwrap();
    config.recovery = recovered;
    let base = f.overlay.base.take().unwrap();
    f.overlay.bind(config, base).unwrap();
    assert!(f.overlay.requires_recovery().unwrap());
    assert!(f.overlay.idle().is_err());
}

fn reopen(request: &JournalOpenRequest) -> RecoveryState {
    let mut request = request.clone();
    request.access = JournalAccess::ReadOnly;
    FileJournal::new().open(&request).unwrap()
}

#[test]
fn r2_finish_prepare_retains_authority() {
    let (mut f, request, proof) = setup();
    f.overlay.prepare_finish(proof.clone()).unwrap();
    let state = reopen(&request);
    assert!(!state.clean);
    assert!(matches!(
        state.custody.unwrap().record.state,
        CustodyPhase::FinishPrepared { .. }
    ));
    let run_id = request.control.run_id;
    assert!(
        f.overlay
            .storage
            .acquire_writer(&AcquireWriterRequest {
                run_id,
                writer_id: WriterId("contender".into()),
                takeover: TakeoverPolicy::Refuse,
            })
            .is_err(),
        "PrepareFinish must retain the exclusive claim"
    );
    assert!(
        f.overlay.idle().is_err(),
        "prepared namespaces cannot mutate"
    );
    f.overlay.commit_custody(&proof).unwrap();
    assert!(reopen(&request).clean);
}

#[test]
fn r2_reopen_final_without_wait() {
    let (mut f, request, mut proof) = setup();
    proof.provider_collected = false;
    assert!(f.overlay.prepare_finish(proof).is_err());
    assert!(matches!(
        reopen(&request).custody.unwrap().record.state,
        CustodyPhase::Armed { .. }
    ));
}

#[test]
fn r2_finish_commit_ack_lost() {
    let (mut f, request, proof) = setup();
    f.overlay.prepare_finish(proof.clone()).unwrap();
    let receipt = f.overlay.commit_custody(&proof).unwrap();
    // Discarding the response cannot undo the durable decision.
    let state = reopen(&request);
    assert!(state.clean);
    assert_eq!(state.last_valid_sequence, receipt.completed_through);
    assert!(matches!(
        state.custody.unwrap().record.state,
        CustodyPhase::Complete { .. }
    ));
}

#[test]
fn r2_recovery_cannot_erase_unknown_creator() {
    let (mut f, request, _) = setup();
    let state = reopen(&request);
    let mut config = f.overlay.config.take().unwrap();
    config.recovery = state;
    let base = f.overlay.base.take().unwrap();
    f.overlay.bind(config, base).unwrap();
    assert!(f.overlay.requires_recovery().unwrap());
    assert!(f.overlay.idle().is_err());
    let mut candidate = FileJournal::new();
    let recovered = candidate.open(&request).unwrap();
    assert!(!recovered.clean);
    assert!(
        candidate
            .append(&JournalRecord {
                format_version: 1,
                sequence: Sequence(0),
                operation_id: OperationId(Uuid::new_v4()),
                writer_epoch: f.overlay.config().unwrap().lease.epoch,
                payload: JournalPayload::Lifecycle(JournalLifecycle::RunCompleted {
                    through: Sequence(1)
                }),
            })
            .is_err(),
        "a candidate lease cannot authorize mutation of unresolved custody"
    );
}

#[test]
fn r2_stale_owner_failure_generation() {
    let (mut f, request, proof) = setup();
    let mut stale = f.overlay.custody.as_ref().unwrap().record.clone();
    stale.generation = Uuid::new_v4();
    stale.sequence = 2;
    stale.state = CustodyPhase::Failed {
        reason: "delayed owner receipt".into(),
    };
    assert!(f.overlay.persist_custody(stale).is_err());
    // A rejected stale message cannot change the current generation's outcome.
    f.overlay.prepare_finish(proof.clone()).unwrap();
    f.overlay.commit_custody(&proof).unwrap();
    assert!(reopen(&request).clean);
}

#[test]
fn r2_concurrent_resume_single_writer() {
    let (f, request, _) = setup();
    let run_id = request.control.run_id;
    std::thread::scope(|scope| {
        let path = f._dirs[1].path();
        let handles: Vec<_> = ["resume-a", "resume-b"]
            .into_iter()
            .map(|writer| {
                scope.spawn(move || {
                    let mut contender = LocalStorage::new(path).unwrap();
                    contender
                        .open_run(&OpenRunRequest {
                            run_id,
                            intent: OpenRunIntent::OpenExisting,
                            immutable_base: ImmutableBaseContract {
                                identity: "test-base".into(),
                                fingerprint: vec![1],
                            },
                            policy: StoragePolicy {
                                read_only: false,
                                require_strict_remote_persistence: false,
                                require_kernel_shadow: false,
                                format_version: 1,
                            },
                        })
                        .unwrap();
                    contender
                        .acquire_writer(&AcquireWriterRequest {
                            run_id,
                            writer_id: WriterId(writer.into()),
                            takeover: TakeoverPolicy::Refuse,
                        })
                        .is_err()
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap());
        }
    });
    assert!(!reopen(&request).clean);
}

#[test]
fn r2_reopen_after_admission_timeout() {
    let (mut f, request, proof) = setup();
    let mut failed = f.overlay.custody.as_ref().unwrap().record.clone();
    failed.sequence = 2;
    failed.state = CustodyPhase::Failed {
        reason: "admission acknowledgement timed out".into(),
    };
    f.overlay.fail_custody(failed).unwrap();
    assert!(f.overlay.prepare_finish(proof).is_err());
    let recovered = reopen(&request);
    assert!(!recovered.clean);
    assert!(matches!(
        recovered.custody.unwrap().record.state,
        CustodyPhase::Failed { .. }
    ));
}

#[test]
fn r2_warning_unwind_retains_custody() {
    let (mut f, request, _) = setup();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        panic!("injected warning observer unwind");
    }));
    assert!(unwind.is_err());
    assert!(f
        .overlay
        .fail_run(&FailedRunRequest {
            run_id: request.control.run_id,
            tree_terminated: true,
            reason: "warning observer unwind".into(),
        })
        .is_err());
    assert!(!reopen(&request).clean);
    assert!(
        f.overlay.config.is_some(),
        "failed custody retains writer authority"
    );
}

#[test]
fn r2_complete_reopen_unarmed_finish() {
    complete_reopen_finish(false);
}

#[test]
fn r2_complete_reopen_new_generation() {
    complete_reopen_finish(true);
}

fn complete_reopen_finish(arm_again: bool) {
    let (mut f, mut request, mut proof) = setup();
    let base = f.overlay.base.take().unwrap();
    let old_epoch = f.overlay.config().unwrap().lease.epoch;
    let old_generation = proof.generation;
    f.overlay.prepare_finish(proof.clone()).unwrap();
    f.overlay.commit_custody(&proof).unwrap();
    let run_id = request.control.run_id;
    let mut storage = LocalStorage::new(f._dirs[1].path()).unwrap();
    let binding = storage
        .open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::OpenExisting,
            immutable_base: ImmutableBaseContract {
                identity: "test-base".into(),
                fingerprint: vec![1],
            },
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        })
        .unwrap();
    let lease = storage
        .acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: WriterId("next-invocation".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .unwrap();
    assert!(lease.epoch.0 > old_epoch.0);
    let JournalAccess::Writer(authority) = &mut request.access else {
        unreachable!()
    };
    authority.lease_epoch = lease.epoch;
    authority.writer_instance_id = "next-invocation".into();
    let mut journal = FileJournal::new();
    let recovered = journal.open(&request).unwrap();
    assert!(recovered.clean);
    let prior = recovered.custody.clone().unwrap();
    let mut overlay = Overlay::new(Box::new(storage), Box::new(journal));
    overlay
        .bind(
            SessionConfig {
                context: context(run_id, Some(lease.epoch)),
                binding,
                lease: lease.clone(),
                recovery: recovered,
            },
            base,
        )
        .unwrap();
    if arm_again {
        let mut armed = CustodyRecord {
            run_id,
            generation: Uuid::new_v4(),
            sequence: 1,
            state: CustodyPhase::Armed {
                capacity: 64,
                startup_nonce: Uuid::new_v4(),
                participants: vec![CustodyParticipant {
                    pid: std::process::id(),
                    birth: 1,
                }],
            },
        };
        assert!(CustodyState::transition(Some(&prior), run_id, old_epoch, &armed).is_err());
        armed.generation = old_generation;
        assert!(CustodyState::transition(Some(&prior), run_id, lease.epoch, &armed).is_err());
        armed.generation = Uuid::new_v4();
        proof.generation = armed.generation;
        overlay.arm_custody(armed).unwrap();
    }
    f.overlay = overlay;
    f.run(&FsOp::Mkdir {
        dir: DirRef::Cwd,
        path: bytes(b"continued"),
        mode: 0o750,
    });
    assert!(f.shadow_root.join("continued").is_dir());
    if arm_again {
        f.overlay.prepare_finish(proof.clone()).unwrap();
        f.overlay.commit_custody(&proof).unwrap();
    } else {
        f.overlay
            .finish_run(&FinishRunRequest {
                run_id,
                root_status: Some(ExitStatus::Code(0)),
                processes_exited: 1,
            })
            .unwrap();
    }
    let recovered = reopen(&request);
    assert!(recovered.clean);
    assert_eq!(
        recovered.custody.unwrap().record.generation,
        if arm_again {
            proof.generation
        } else {
            old_generation
        }
    );
}

#[test]
fn r2_arm_rejects_duplicate_complete_phase() {
    let (mut f, request, proof) = setup();
    f.overlay.prepare_finish(proof).unwrap();
    let mut complete = f.overlay.custody.as_ref().unwrap().record.clone();
    complete.sequence += 1;
    complete.state = CustodyPhase::Complete {
        certificate: [7; 32],
    };
    // Keep the session bound while exercising the actual journal transition.
    f.overlay.persist_custody(complete.clone()).unwrap();
    f.overlay.prepared_custody = None;
    assert_eq!(
        f.overlay.arm_custody(complete).unwrap_err().kind,
        ErrorKind::InvalidState
    );
    assert!(matches!(
        f.overlay.custody.as_ref().unwrap().record.state,
        CustodyPhase::Complete { .. }
    ));
    assert_eq!(reopen(&request).custody.unwrap().record.sequence, 3);
}

#[test]
fn r2_prepare_finish_requires_entire_run_receipt() {
    let fixture = Fixture::build(
        &[],
        Setup {
            mismatch: Some(4),
            ..Setup::default()
        },
    );
    let (mut f, request, proof) = setup_fixture(fixture, true);
    assert_eq!(
        f.overlay.prepare_finish(proof.clone()).unwrap_err().kind,
        ErrorKind::ProtocolMismatch
    );
    assert!(f.overlay.prepared_custody.is_none());
    assert!(f.overlay.commit_custody(&proof).is_err());
    assert!(matches!(
        reopen(&request).custody.unwrap().record.state,
        CustodyPhase::Armed { .. }
    ));
    assert!(f
        .overlay
        .storage
        .acquire_writer(&AcquireWriterRequest {
            run_id: request.control.run_id,
            writer_id: WriterId("receipt-contender".into()),
            takeover: TakeoverPolicy::Refuse,
        })
        .is_err());
}
