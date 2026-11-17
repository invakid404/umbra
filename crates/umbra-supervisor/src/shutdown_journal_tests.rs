mod shutdown_journal {
    use super::*;
    use umbra_journal::Journal;
    use umbra_journal_file::FileJournal;
    use umbra_overlay::{Overlay, SessionConfig, StorageBase};
    use umbra_storage::Storage;
    use umbra_storage_local::LocalStorage;

    pub(super) struct ScriptedControl(pub Option<umbra_platform::shutdown::Closure>);
    impl TraceBackend for ScriptedControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> { unreachable!() }
        fn next_event(&mut self) -> Result<TraceEvent> { unreachable!() }
        fn poll_event(&mut self, _: std::time::Duration) -> Result<umbra_platform::EventPoll> { unreachable!() }
        fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> { unreachable!() }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> { unreachable!() }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> { unreachable!() }
        fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> { unreachable!() }
        fn resume(&mut self, _: ResumeCommand) -> Result<()> { Ok(()) }
    }
    impl TraceControl for ScriptedControl {
        fn capabilities(&self) -> PlatformCapabilities { PlatformCapabilities::default() }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> { unreachable!() }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> { Ok(()) }
        fn begin_shutdown(&mut self, _: umbra_core::shutdown::Nonce) -> Result<umbra_platform::shutdown::Start> {
            Ok(umbra_platform::shutdown::Start::Remote(self.0.take().unwrap()))
        }
    }
    pub(super) fn confirmed() -> umbra_platform::shutdown::Closure {
        umbra_platform::shutdown::Closure { tree_terminated: true, error: None, final_to_exit: None }
    }
    struct Spy {
        inner: Box<dyn NamespaceSession + Send>,
        failed: std::sync::Arc<std::sync::Mutex<Vec<FailedRunRequest>>>,
    }
    impl NamespaceResolver for Spy {
        fn resolve(&mut self, context: &ProcessContext, op: &FsOp) -> Result<ResolvedAction> { self.inner.resolve(context, op) }
    }
    impl NamespaceSession for Spy {
        fn prepare(&mut self, id: OperationId, action: &ResolvedAction) -> Result<PreparedAction> { self.inner.prepare(id, action) }
        fn observe_result(&mut self, id: OperationId, outcome: &OperationOutcome) -> Result<()> { self.inner.observe_result(id, outcome) }
        fn commit(&mut self, id: OperationId) -> Result<CommitReceipt> { self.inner.commit(id) }
        fn abort(&mut self, id: OperationId, reason: &AbortReason) -> Result<()> { self.inner.abort(id, reason) }
        fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint> { self.inner.checkpoint(request) }
        fn finish_run(&mut self, request: &FinishRunRequest) -> Result<FinishRunReceipt> { self.inner.finish_run(request) }
        fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
            self.failed.lock().unwrap().push(request.clone());
            self.inner.fail_run(request)
        }
    }

    fn storage(dir: &std::path::Path, run_id: RunId) -> (LocalStorage, RunBinding, WriterLease) {
        let mut storage = LocalStorage::new(dir).unwrap();
        let binding = storage.open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "shutdown-test".into(),
                fingerprint: vec![1],
            },
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        }).unwrap();
        let lease = storage.acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: WriterId("shutdown-test".into()),
            takeover: TakeoverPolicy::Refuse,
        }).unwrap();
        (storage, binding, lease)
    }

    fn context(run_id: RunId, writer_epoch: Option<LeaseEpoch>) -> RequestContext {
        RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
            writer_epoch,
        }
    }

    // Successful preliminary Terminate is deliberately insufficient. Fake has
    // no provider shutdown proof. The journal is fresh, not pre-poisoned.
    fn read_journal(closure: Option<umbra_platform::shutdown::Closure>, loop_error: bool, proof: bool, success: bool) {
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        let mut supervisor = supervisor();
        if let Some(closure) = closure {
            supervisor.platform.control = Box::new(ScriptedControl(Some(closure)));
        }
        let run_id = supervisor.state.run_id;
        let (mut base, base_binding, base_lease) =
            storage(base_dir.path(), RunId(Uuid::new_v4()));
        base.release_writer(&base_lease).unwrap();
        let base_context = context(base_binding.run_id, None);
        let base = StorageBase::new(Box::new(base), base_binding, base_context).unwrap();
        let (shadow, binding, lease) = storage(shadow_dir.path(), run_id);
        let mut request = JournalOpenRequest {
            control: JournalControlBinding {
                run_id,
                directory: PhysicalPath(binding.control.physical_path.clone().unwrap()),
            },
            access: JournalAccess::Writer(JournalWriterAuthority {
                run_id,
                writer_instance_id: "shutdown-test".into(),
                host_fingerprint: "test".into(),
                supervisor_version: "test".into(),
                acquired_at: 0,
                renewed_at: 0,
                lease_epoch: lease.epoch,
                fencing: JournalFencingEvidence::Fenced {
                    mechanism: "owned-test-directory".into(),
                    evidence: vec![],
                },
            }),
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        };
        let mut journal = FileJournal::new();
        let recovery = journal.open(&request).unwrap();
        assert!(recovery.clean);
        let (journal, terminal) = crate::shutdown_journal::TerminalJournal::wrap(Box::new(journal), lease.epoch);
        supervisor.terminal_journal = Some(terminal);
        let mut namespace = Overlay::new(Box::new(shadow), journal);
        namespace.bind(SessionConfig {
            context: context(run_id, Some(lease.epoch)),
            recovery,
            binding,
            lease,
        }, Box::new(base)).unwrap();
        let failed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        supervisor.namespace = Box::new(Spy { inner: Box::new(namespace), failed: failed.clone() });
        supervisor.handle_event(TraceEvent::Exit {
            task: task(1),
            status: ExitStatus::Code(0),
        }).unwrap();
        let primary = if loop_error { Err(UmbraError::new(ErrorKind::Io, "test.loop", "primary loop failure")) } else { Ok(()) };
        let result = crate::run::finish_supervision(supervisor, primary, &mut None);

        request.access = JournalAccess::ReadOnly;
        let mut reopened = FileJournal::new();
        let recovery = reopened.open(&request).unwrap();
        let records = reopened.replay(Sequence(0)).unwrap()
            .collect::<Result<Vec<_>>>().unwrap();
        reopened.close().unwrap();
        let completed = records.iter().any(|record| matches!(
            record.payload,
            JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
        ));
        eprintln!("result={result:?}; completed={completed}; recovery={recovery:?}");
        assert_eq!(completed, success, "completion requires full closure");
        assert_eq!(recovery.clean, success);
        assert_eq!(recovery.recovery_required, !success);
        assert_eq!(result.is_ok(), success);
        let failures = failed.lock().unwrap();
        assert_eq!(failures.len(), usize::from(!success));
        if !success { assert_eq!(failures[0].tree_terminated, proof); }
        if loop_error { assert_eq!(result.unwrap_err().operation, "test.loop"); }
    }
    #[test]
    fn shutdown_missing_final_cannot_complete_real_journal() {
        read_journal(None, false, false, false);
    }
    #[test]
    fn shutdown_confirmed_completes_real_journal() {
        read_journal(Some(confirmed()), false, true, true);
    }
    #[test]
    fn shutdown_exit_failure_keeps_tree_proof() {
        let mut closure = confirmed();
        closure.error = Some(UmbraError::new(ErrorKind::Io, "provider.exit", "owned exit unavailable"));
        read_journal(Some(closure), false, true, false);
    }
    #[test]
    fn shutdown_preserves_loop_primary() {
        let closure = umbra_platform::shutdown::Closure::failed(UmbraError::new(ErrorKind::Io, "closure", "missing final"));
        read_journal(Some(closure), true, false, false);
    }

}
