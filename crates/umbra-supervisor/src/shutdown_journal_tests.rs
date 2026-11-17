mod shutdown_journal {
    use super::*;
    use umbra_journal::Journal;
    use umbra_journal_file::FileJournal;
    use umbra_overlay::{Overlay, SessionConfig, StorageBase};
    use umbra_storage::Storage;
    use umbra_storage_local::LocalStorage;

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
    #[test]
    fn shutdown_missing_final_cannot_complete_real_journal() {
        let base_dir = tempfile::tempdir().unwrap();
        let shadow_dir = tempfile::tempdir().unwrap();
        let mut supervisor = supervisor();
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
        let mut namespace = Overlay::new(Box::new(shadow), Box::new(journal));
        namespace.bind(SessionConfig {
            context: context(run_id, Some(lease.epoch)),
            recovery,
            binding,
            lease,
        }, Box::new(base)).unwrap();
        supervisor.namespace = Box::new(namespace);
        supervisor.handle_event(TraceEvent::Exit {
            task: task(1),
            status: ExitStatus::Code(0),
        }).unwrap();
        let result = crate::run::finish_supervision(supervisor, Ok(()), &mut None);

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
        assert!(!completed, "missing closure proof must not persist RunCompleted");
        assert!(!recovery.clean, "closure failure requires durable recovery state");
        assert!(result.is_err(), "missing closure proof must fail the run");
    }
}
