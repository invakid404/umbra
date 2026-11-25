use super::*;
use umbra_journal_file::FileJournal;

fn setup() -> (Fixture, JournalOpenRequest, CustodyFinishProof) {
    let mut f = Fixture::new(&[]);
    let config = f.overlay.config().unwrap();
    let run_id = config.context.run_id;
    let request = JournalOpenRequest {
        control: JournalControlBinding {
            run_id,
            directory: PhysicalPath(config.binding.control.physical_path.clone().unwrap()),
        },
        access: JournalAccess::Writer(JournalWriterAuthority {
            run_id, writer_instance_id: "custody-test".into(),
            host_fingerprint: "test".into(), supervisor_version: "test".into(),
            acquired_at: 0, renewed_at: 0, lease_epoch: config.lease.epoch,
            fencing: JournalFencingEvidence::Fenced {
                mechanism: "local writer claim".into(), evidence: vec![],
            },
        }),
        format: JournalFormatPolicy { readable_versions: vec![1], write_version: 1 },
    };
    let mut journal = FileJournal::new();
    assert!(journal.open(&request).unwrap().clean);
    f.overlay.journal = Box::new(journal);
    let generation = Uuid::new_v4();
    f.overlay.arm_custody(CustodyRecord {
        run_id, generation, sequence: 1,
        state: CustodyPhase::Armed {
            capacity: 64, startup_nonce: Uuid::new_v4(),
            participants: vec![CustodyParticipant { pid: std::process::id(), birth: 1 }],
        },
    }).unwrap();
    let proof = CustodyFinishProof {
        generation, certificate: [7; 32], ledger_final: true,
        provider_collected: true, creators_terminal: true,
        cleanup_confirmed: true, root_result: 0,
    };
    (f, request, proof)
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
    assert!(matches!(state.custody.unwrap().record.state, CustodyPhase::FinishPrepared { .. }));
    let run_id = request.control.run_id;
    assert!(f.overlay.storage.acquire_writer(&AcquireWriterRequest {
        run_id, writer_id: WriterId("contender".into()), takeover: TakeoverPolicy::Refuse,
    }).is_err(), "PrepareFinish must retain the exclusive claim");
    assert!(f.overlay.idle().is_err(), "prepared namespaces cannot mutate");
    f.overlay.commit_custody(&proof).unwrap();
    assert!(reopen(&request).clean);
}

#[test]
fn r2_reopen_final_without_wait() {
    let (mut f, request, mut proof) = setup();
    proof.provider_collected = false;
    assert!(f.overlay.prepare_finish(proof).is_err());
    assert!(matches!(reopen(&request).custody.unwrap().record.state, CustodyPhase::Armed { .. }));
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
    assert!(matches!(state.custody.unwrap().record.state, CustodyPhase::Complete { .. }));
}
