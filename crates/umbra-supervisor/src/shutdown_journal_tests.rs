mod shutdown_journal {
    use super::*;
    use umbra_journal::Journal;
    use umbra_journal_file::FileJournal;
    use umbra_overlay::{Overlay, SessionConfig, StorageBase};
    use umbra_storage::Storage;
    use umbra_storage_local::LocalStorage;

    pub(super) struct ScriptedControl(pub Option<umbra_platform::shutdown::Closure>, pub bool);
    impl TraceBackend for ScriptedControl {
        fn launch(&mut self, _: LaunchSpec) -> Result<ProcessHandle> {
            let mut error = UmbraError::new(ErrorKind::Io, "test.launch", "partial launch failed");
            error.launch_tree_terminated = true; // A legacy flag is not the ledger proof.
            Err(error)
        }
        fn next_event(&mut self) -> Result<TraceEvent> {
            unreachable!()
        }
        fn poll_event(&mut self, _: std::time::Duration) -> Result<umbra_platform::EventPoll> {
            unreachable!()
        }
        fn read_memory(&mut self, _: TaskId, _: u64, _: &mut [u8]) -> Result<()> {
            unreachable!()
        }
        fn write_memory(&mut self, _: TaskId, _: u64, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn registers(&mut self, _: ThreadId) -> Result<RegisterSet> {
            unreachable!()
        }
        fn set_registers(&mut self, _: ThreadId, _: &RegisterSet) -> Result<()> {
            unreachable!()
        }
        fn resume(&mut self, _: ResumeCommand) -> Result<()> {
            Ok(())
        }
    }
    impl TraceControl for ScriptedControl {
        fn capabilities(&self) -> PlatformCapabilities {
            PlatformCapabilities::default()
        }
        fn quiesce(&mut self, _: ProcessHandle) -> Result<QuiescedTree> {
            unreachable!()
        }
        fn terminate(&mut self, _: ProcessHandle, _: TerminationPolicy) -> Result<()> {
            if self.1 {
                Err(UmbraError::new(
                    ErrorKind::Io,
                    "test.terminate",
                    "preliminary termination failed",
                ))
            } else {
                Ok(())
            }
        }
        fn begin_shutdown(
            &mut self,
            _: umbra_core::shutdown::Nonce,
        ) -> Result<umbra_platform::shutdown::Start> {
            Ok(umbra_platform::shutdown::Start::Remote(
                self.0.take().unwrap(),
            ))
        }
    }
    pub(super) fn confirmed() -> umbra_platform::shutdown::Closure {
        umbra_platform::shutdown::Closure {
            tree_terminated: true,
            error: None,
            final_to_exit: None,
        }
    }
    struct Spy {
        inner: Box<dyn NamespaceSession + Send>,
        failed: std::sync::Arc<std::sync::Mutex<Vec<FailedRunRequest>>>,
    }
    impl NamespaceResolver for Spy {
        fn resolve(&mut self, context: &ProcessContext, op: &FsOp) -> Result<ResolvedAction> {
            self.inner.resolve(context, op)
        }
    }
    impl NamespaceSession for Spy {
        fn prepare(&mut self, id: OperationId, action: &ResolvedAction) -> Result<PreparedAction> {
            self.inner.prepare(id, action)
        }
        fn observe_result(&mut self, id: OperationId, outcome: &OperationOutcome) -> Result<()> {
            self.inner.observe_result(id, outcome)
        }
        fn commit(&mut self, id: OperationId) -> Result<CommitReceipt> {
            self.inner.commit(id)
        }
        fn abort(&mut self, id: OperationId, reason: &AbortReason) -> Result<()> {
            self.inner.abort(id, reason)
        }
        fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint> {
            self.inner.checkpoint(request)
        }
        fn finish_run(&mut self, request: &FinishRunRequest) -> Result<FinishRunReceipt> {
            self.inner.finish_run(request)
        }
        fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
            self.failed.lock().unwrap().push(request.clone());
            self.inner.fail_run(request)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Fault {
        Preliminary,
        None,
        MarkerAppend,
        MarkerFlush,
        CompletionAppend,
        CompletionFlush,
        Close,
    }
    struct FaultJournal {
        inner: FileJournal,
        fault: Fault,
        last: Option<JournalPayload>,
    }
    impl Journal for FaultJournal {
        fn open(&mut self, request: &JournalOpenRequest) -> Result<RecoveryState> {
            self.inner.open(request)
        }
        fn append(&mut self, record: &JournalRecord) -> Result<Sequence> {
            self.last = Some(record.payload.clone());
            if (self.fault == Fault::MarkerAppend
                && matches!(
                    record.payload,
                    JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired)
                ))
                || (self.fault == Fault::CompletionAppend
                    && matches!(
                        record.payload,
                        JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
                    ))
            {
                return Err(UmbraError::new(
                    ErrorKind::Io,
                    "test.journal.append",
                    "injected append failure",
                ));
            }
            self.inner.append(record)
        }
        fn flush(&mut self, through: Sequence) -> Result<DurableSequence> {
            if (self.fault == Fault::MarkerFlush
                && matches!(
                    self.last,
                    Some(JournalPayload::Lifecycle(
                        JournalLifecycle::RecoveryRequired
                    ))
                ))
                || (self.fault == Fault::CompletionFlush
                    && matches!(
                        self.last,
                        Some(JournalPayload::Lifecycle(
                            JournalLifecycle::RunCompleted { .. }
                        ))
                    ))
            {
                self.fault = Fault::None;
                return Err(UmbraError::new(
                    ErrorKind::Io,
                    "test.journal.flush",
                    "injected flush failure after append",
                ));
            }
            self.inner.flush(through)
        }
        fn replay(
            &mut self,
            after: Sequence,
        ) -> Result<Box<dyn Iterator<Item = Result<JournalRecord>> + Send + '_>> {
            self.inner.replay(after)
        }
        fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<CheckpointId> {
            self.inner.write_checkpoint(checkpoint)
        }
        fn close(&mut self) -> Result<()> {
            self.inner.close()?;
            if self.fault == Fault::Close {
                return Err(UmbraError::new(
                    ErrorKind::Io,
                    "test.journal.close",
                    "injected post-durable close failure",
                ));
            }
            Ok(())
        }
    }
    struct Observer(
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    );
    impl crate::run::RunObserver for Observer {
        fn prepared(&mut self, _: RunId) -> Result<()> {
            Ok(())
        }
        fn finished(&mut self, _: &crate::run::RunOutcome) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn teardown_warning(&mut self, error: &UmbraError) {
            self.1.lock().unwrap().push(error.to_string());
        }
    }

    fn storage(dir: &std::path::Path, run_id: RunId) -> (LocalStorage, RunBinding, WriterLease) {
        let mut storage = LocalStorage::new(dir).unwrap();
        let binding = storage
            .open_run(&OpenRunRequest {
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
            })
            .unwrap();
        let lease = storage
            .acquire_writer(&AcquireWriterRequest {
                run_id,
                writer_id: WriterId("shutdown-test".into()),
                takeover: TakeoverPolicy::Refuse,
            })
            .unwrap();
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
    fn read_journal(
        closure: Option<umbra_platform::shutdown::Closure>,
        loop_error: bool,
        proof: bool,
        success: bool,
    ) {
        read_journal_fault(closure, loop_error, proof, success, Fault::None);
    }
    fn read_journal_fault(
        closure: Option<umbra_platform::shutdown::Closure>,
        loop_error: bool,
        proof: bool,
        success: bool,
        fault: Fault,
    ) {
        read_journal_case(closure, loop_error, proof, success, fault, false);
    }
    fn read_journal_case(
        closure: Option<umbra_platform::shutdown::Closure>,
        loop_error: bool,
        proof: bool,
        success: bool,
        fault: Fault,
        partial_launch: bool,
    ) {
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        let mut supervisor = supervisor();
        if let Some(closure) = closure {
            supervisor.platform.control =
                Box::new(ScriptedControl(Some(closure), fault == Fault::Preliminary));
        }
        let run_id = supervisor.state.run_id;
        let (mut base, base_binding, base_lease) = storage(base_dir.path(), RunId(Uuid::new_v4()));
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
        let (journal, terminal) = crate::shutdown_journal::TerminalJournal::wrap(
            Box::new(FaultJournal {
                inner: journal,
                fault,
                last: None,
            }),
            lease.epoch,
        );
        supervisor.terminal_journal = Some(terminal);
        let mut namespace = Overlay::new(Box::new(shadow), journal);
        namespace
            .bind(
                SessionConfig {
                    context: context(run_id, Some(lease.epoch)),
                    recovery,
                    binding,
                    lease,
                },
                Box::new(base),
            )
            .unwrap();
        let failed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        supervisor.namespace = Box::new(Spy {
            inner: Box::new(namespace),
            failed: failed.clone(),
        });
        supervisor
            .handle_event(TraceEvent::Exit {
                task: task(1),
                status: ExitStatus::Code(0),
            })
            .unwrap();
        let primary = if loop_error {
            Err(UmbraError::new(
                ErrorKind::Io,
                "test.loop",
                "primary loop failure",
            ))
        } else {
            Ok(())
        };
        let notifications = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut observer: Option<Box<dyn crate::run::RunObserver>> =
            Some(Box::new(Observer(notifications.clone(), warnings.clone())));
        let result = if partial_launch {
            supervisor.state.lifecycle = RunLifecycle::Created;
            supervisor.state.processes = Default::default();
            supervisor.live = 0;
            let cwd = BytePath::new(b"/".to_vec()).unwrap();
            crate::run::supervise_prepared(
                supervisor,
                LaunchSpec {
                    executable: cwd.clone(),
                    argv: vec![],
                    environment: vec![],
                    cwd: cwd.clone(),
                    policy: LaunchPolicy {
                        persistence: PersistencePolicy::LocalDevelopment,
                        inherited_fds: vec![],
                        interpose: false,
                        descriptor_limit: None,
                    },
                    sandbox: SandboxRequirement::UnsandboxedExperiment,
                },
                RunBudget {
                    renew_after: std::time::Duration::from_secs(1),
                    cwd,
                    architecture: Architecture::Aarch64,
                    abi: "test".into(),
                    descriptor_floor: None,
                },
                &mut observer,
            )
        } else {
            crate::run::finish_supervision(supervisor, primary, &mut observer)
        };

        request.access = JournalAccess::ReadOnly;
        let mut reopened = FileJournal::new();
        let recovery = reopened.open(&request).unwrap();
        let records = reopened
            .replay(Sequence(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        reopened.close().unwrap();
        let completed = records.iter().any(|record| {
            matches!(
                record.payload,
                JournalPayload::Lifecycle(JournalLifecycle::RunCompleted { .. })
            )
        });
        eprintln!("result={result:?}; completed={completed}; recovery={recovery:?}");
        let completion_visible = success || matches!(fault, Fault::CompletionFlush | Fault::Close);
        let recovery_marker = !success && matches!(fault, Fault::None | Fault::MarkerFlush);
        assert_eq!(
            completed, completion_visible,
            "actual persisted completion at {fault:?}"
        );
        assert_eq!(recovery.clean, !recovery_marker);
        assert_eq!(recovery.recovery_required, recovery_marker);
        assert_eq!(
            notifications.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(success)
        );
        assert_eq!(result.is_ok(), success);
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), usize::from(fault == Fault::Preliminary));
        if fault == Fault::Preliminary {
            assert!(warnings[0].contains("preliminary termination failed"));
        }
        let failures = failed.lock().unwrap();
        assert_eq!(failures.len(), usize::from(!success));
        if !success {
            assert_eq!(failures[0].tree_terminated, proof);
        }
        if loop_error {
            assert_eq!(result.as_ref().unwrap_err().operation, "test.loop");
        }
        if partial_launch {
            assert_eq!(result.as_ref().unwrap_err().operation, "test.launch");
        }
        if matches!(fault, Fault::MarkerAppend | Fault::MarkerFlush) {
            let error = result.unwrap_err();
            assert_eq!(error.operation, "platform.shutdown");
            assert!(error.context.contains("test.journal."));
        }
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
        closure.error = Some(UmbraError::new(
            ErrorKind::Io,
            "provider.exit",
            "owned exit unavailable",
        ));
        read_journal(Some(closure), false, true, false);
    }
    #[test]
    fn shutdown_preserves_loop_primary() {
        let closure = umbra_platform::shutdown::Closure::failed(UmbraError::new(
            ErrorKind::Io,
            "closure",
            "missing final",
        ));
        read_journal(Some(closure), true, false, false);
    }

    #[test]
    fn shutdown_terminal_journal_failures() {
        for fault in [
            Fault::CompletionAppend,
            Fault::CompletionFlush,
            Fault::Close,
        ] {
            read_journal_fault(Some(confirmed()), false, true, false, fault);
        }
    }
    #[test]
    fn shutdown_recovery_marker_failure() {
        for fault in [Fault::MarkerAppend, Fault::MarkerFlush] {
            read_journal_fault(None, false, false, false, fault);
        }
    }
    #[test]
    fn shutdown_malformed_ledger() {
        use umbra_core::shutdown::*;
        for mutation in 0..6 {
            let ledger = umbra_platform::shutdown::Ledger::new();
            let slot = ledger.reserve(ResourceKind::Tracer, None, 1, 0).unwrap();
            slot.acquired(Identity {
                pid: Some(123),
                birth: None,
            })
            .unwrap();
            slot.confirm(Proof::Collected {
                status: "exit 0".into(),
            })
            .unwrap();
            let mut accepted = ledger
                .freeze(Nonce::new_v4(), std::time::Duration::ZERO)
                .unwrap();
            let mut final_state = ledger.final_snapshot().unwrap();
            match mutation {
                0 => final_state.entries.clear(),
                1 => final_state.entries.push(final_state.entries[0].clone()),
                2 => final_state.entries[0].slot.id.sequence += 1,
                3 => accepted.high_water += 1,
                4 => final_state.generation += 1,
                5 => final_state.entries[0].disposition = Disposition::Reserved,
                _ => unreachable!(),
            }
            let closure = umbra_platform::shutdown::Closure::from_final(&accepted, &final_state);
            read_journal(Some(closure), false, false, false);
        }
    }

    #[test]
    fn shutdown_loop_primary_and_partial_launch() {
        for proof in [false, true] {
            let closure = if proof {
                confirmed()
            } else {
                umbra_platform::shutdown::Closure::failed(UmbraError::new(
                    ErrorKind::Io,
                    "platform.shutdown",
                    "invalidated IPC after acquisition",
                ))
            };
            read_journal_case(Some(closure), false, proof, false, Fault::None, true);
        }
    }
    #[test]
    fn shutdown_final_confirmation_supersedes_preliminary_failure() {
        read_journal_fault(Some(confirmed()), false, true, true, Fault::Preliminary);
    }
    #[test]
    fn shutdown_confirmed_tree_does_not_erase_loop_error() {
        read_journal(Some(confirmed()), true, true, false);
    }
}
