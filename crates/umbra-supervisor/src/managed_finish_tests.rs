use super::*;
use umbra_core::*;
use umbra_overlay::NamespaceResolver;

struct LostReply {
    state: CustodyState,
    proof: CustodyFinishProof,
    lose_prepare: bool,
    lose_commit: bool,
    corrupt_query: bool,
    queries: usize,
    prepares: usize,
    commits: usize,
}
impl LostReply {
    fn new() -> Self {
        let generation = uuid::Uuid::new_v4();
        Self {
            state: CustodyState {
                writer_epoch: LeaseEpoch(7),
                record: CustodyRecord {
                    run_id: RunId(uuid::Uuid::new_v4()),
                    generation,
                    sequence: 1,
                    state: CustodyPhase::Armed {
                        capacity: 1,
                        startup_nonce: uuid::Uuid::new_v4(),
                        participants: vec![],
                    },
                },
            },
            proof: CustodyFinishProof {
                generation,
                certificate: [5; 32],
                ledger_final: true,
                provider_collected: true,
                creators_terminal: true,
                cleanup_confirmed: true,
                root_result: 0,
            },
            lose_prepare: false,
            lose_commit: false,
            corrupt_query: false,
            queries: 0,
            prepares: 0,
            commits: 0,
        }
    }
}
impl NamespaceResolver for LostReply {
    fn resolve(&mut self, _: &ProcessContext, _: &FsOp) -> Result<ResolvedAction> {
        unreachable!()
    }
}
impl NamespaceSession for LostReply {
    fn prepare(&mut self, _: OperationId, _: &ResolvedAction) -> Result<PreparedAction> {
        unreachable!()
    }
    fn observe_result(&mut self, _: OperationId, _: &OperationOutcome) -> Result<()> {
        unreachable!()
    }
    fn commit(&mut self, _: OperationId) -> Result<CommitReceipt> {
        unreachable!()
    }
    fn abort(&mut self, _: OperationId, _: &AbortReason) -> Result<()> {
        unreachable!()
    }
    fn checkpoint(&mut self, _: &CheckpointRequest) -> Result<Checkpoint> {
        unreachable!()
    }
    fn query_custody(&mut self, run: RunId, generation: uuid::Uuid) -> Result<CustodyState> {
        assert_eq!(run, self.state.record.run_id);
        assert_eq!(generation, self.state.record.generation);
        self.queries += 1;
        let mut state = self.state.clone();
        if self.corrupt_query && self.commits > 0 {
            state.writer_epoch = LeaseEpoch(8);
        }
        Ok(state)
    }
    fn prepare_finish(&mut self, proof: &CustodyFinishProof) -> Result<Sequence> {
        assert_eq!(proof, &self.proof);
        self.prepares += 1;
        self.state.record.state = CustodyPhase::FinishPrepared {
            certificate: proof.certificate,
            root_result: proof.root_result,
        };
        self.state.record.sequence = 2;
        if std::mem::take(&mut self.lose_prepare) {
            return Err(invalid("lost prepare response"));
        }
        Ok(Sequence(2))
    }
    fn commit_custody(&mut self, proof: &CustodyFinishProof) -> Result<FinishRunReceipt> {
        assert_eq!(proof, &self.proof);
        assert!(!matches!(
            self.state.record.state,
            CustodyPhase::Armed { .. }
        ));
        self.commits += 1;
        self.state.record.state = CustodyPhase::Complete {
            certificate: proof.certificate,
        };
        self.state.record.sequence = 3;
        if std::mem::take(&mut self.lose_commit) {
            return Err(invalid("lost commit response"));
        }
        Ok(FinishRunReceipt {
            run_id: self.state.record.run_id,
            durability: DurabilityReceipt {
                run_id: self.state.record.run_id,
                writer_epoch: self.state.writer_epoch,
                scope: FlushScope::EntireRun,
                durability: Durability::Local,
                evidence: vec![],
            },
            completed_through: Sequence(3),
        })
    }
}

#[test]
fn lost_finish_responses_query_exact_generation_and_replay_same_commit() {
    let mut namespace = LostReply::new();
    namespace.lose_prepare = true;
    namespace.lose_commit = true;
    let proof = namespace.proof.clone();
    let mut finish =
        FinishArbitration::new(namespace.state.record.run_id, proof.generation).unwrap();
    let receipt = finish.finish(&mut namespace, &proof).unwrap();
    assert_eq!(receipt.completed_through, Sequence(3));
    assert_eq!(
        (namespace.prepares, namespace.commits, namespace.queries),
        (1, 2, 3)
    );
    finish.finish(&mut namespace, &proof).unwrap();
    assert_eq!(
        (namespace.prepares, namespace.commits, namespace.queries),
        (1, 2, 3)
    );
    let mut changed = proof;
    changed.certificate[0] ^= 1;
    assert!(finish.finish(&mut namespace, &changed).is_err());
}

#[test]
fn changed_writer_epoch_cannot_reconcile_a_lost_finish_response() {
    let mut namespace = LostReply::new();
    namespace.lose_commit = true;
    namespace.corrupt_query = true;
    let proof = namespace.proof.clone();
    let mut finish =
        FinishArbitration::new(namespace.state.record.run_id, proof.generation).unwrap();
    let error = finish.finish(&mut namespace, &proof).unwrap_err();
    assert!(error.to_string().contains("lost commit response"));
    assert_eq!(namespace.queries, 2);
    assert_eq!(namespace.commits, 1, "changed epoch authorized replay");
}

#[test]
fn unconfirmed_provider_never_reaches_namespace_finish() {
    let mut namespace = LostReply::new();
    let mut proof = namespace.proof.clone();
    proof.provider_collected = false;
    let mut finish =
        FinishArbitration::new(namespace.state.record.run_id, proof.generation).unwrap();
    assert!(finish.finish(&mut namespace, &proof).is_err());
    assert_eq!(
        (namespace.queries, namespace.prepares, namespace.commits),
        (0, 0, 0)
    );
}
