//! Staged composition of one supervised command run.
//!
//! [`run`] owns the whole order: validate the request and the registry's declared
//! capabilities before opening anything, connect storage, open the run, take the
//! single writer lease, open the journal against `control/`, bind the namespace
//! over those already-open sessions, render enforcement from the run's own root,
//! launch stopped, drive events, and tear down in a fixed order.
//!
//! Two rules shape every branch below. Nothing prompts: a missing capability,
//! provider, mount or permission is a structured error and a nonzero exit, never
//! a question, an installer or a quiet downgrade. And nothing is released or
//! marked complete without evidence: a failure after the lease is taken releases
//! authority only when the supervised tree is provably gone.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use umbra_core::{
    capabilities as caps, provider::ProviderDescriptor, provider::ProviderRegistry,
    AgentLaunchRequest, BytePath, EnvironmentVariable, ErrorKind, ExitStatus, IdempotencyKey,
    LaunchPolicy, LaunchSpec, OperationId, PersistencePolicy, Result, RunId, SandboxRequirement,
    TerminationPolicy, TracedFd, UmbraError, WriterId,
};

use crate::sandbox::{self, SandboxSpec};

/// How a run's persistent state is stored, chosen explicitly by the caller.
///
/// There is no automatic selection and no fallback between these: a run that
/// cannot use the storage it asked for fails, rather than silently persisting
/// somewhere weaker than the caller believes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunPersistence {
    /// Default. A validated, already-mounted NFSv4 export whose durability
    /// boundary is the client fsync. Remote commit is not qualified.
    NfsClientFsync,
    /// Explicitly selected local development storage. No remote durability.
    LocalDevelopment,
    /// Strict remote durability. No provider qualifies this yet, so selecting it
    /// is a deterministic error rather than an unproven promise.
    StrictRemote,
}

/// An explicitly constructed command launch. Nothing is inherited implicitly:
/// argv includes argv[0] and the environment is exactly what the caller built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLaunch {
    /// Absolute executable path; never resolved by scanning PATH.
    pub executable: BytePath,
    /// Complete argument vector, including argv[0].
    pub argv: Vec<Vec<u8>>,
    /// Complete environment; no host variable is inherited unless listed here.
    pub environment: Vec<EnvironmentVariable>,
    /// Logical working directory for the target.
    pub cwd: BytePath,
}

/// What this run supervises.
#[derive(Clone, Debug)]
pub enum RunLaunch {
    /// A directly named command.
    Command(CommandLaunch),
    /// A coding-agent session described by a selected adapter.
    Agent(Box<AgentLaunchRequest>),
}

/// Noninteractive progress reporting.
///
/// This exists so the supervisor never writes to a stream itself and never needs
/// to read one. It reports; it cannot ask.
pub trait RunObserver {
    /// Called once, after preparation succeeds and before the target launches.
    fn prepared(&mut self, run_id: RunId);
    /// Called once when the supervised tree has finished, before teardown result.
    fn finished(&mut self, outcome: &RunOutcome);
}

/// Everything one run needs. Ownership containers, not persisted records.
pub struct RunSpec {
    /// Explicitly installed providers and their opaque options.
    pub registry: ProviderRegistry,
    /// What to supervise.
    pub launch: RunLaunch,
    /// Absolute host directory approved as this run's workspace.
    pub workspace: PathBuf,
    /// Storage selection.
    pub persistence: RunPersistence,
    /// Acknowledgement that this is a bounded experimental tracing mode. It is an
    /// argument, never a runtime question, and it disables nothing.
    pub experimental: bool,
    /// Optional status sink.
    pub observer: Option<Box<dyn RunObserver>>,
}

/// What the supervised tree actually did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// Root process outcome, absent if the root never reported an exit.
    pub root_status: Option<ExitStatus>,
    /// Supervised processes observed exiting, including the root.
    pub processes_exited: u64,
}

fn error(kind: ErrorKind, operation: &str, context: impl Into<String>) -> UmbraError {
    UmbraError::new(kind, operation, context)
}

/// Preserve the failure that actually mattered, with cleanup damage appended.
fn with_cleanup(primary: UmbraError, cleanup: Result<()>) -> UmbraError {
    match cleanup {
        Ok(()) => primary,
        Err(secondary) => {
            let mut combined = primary;
            combined.context = format!("{} (cleanup also failed: {secondary})", combined.context);
            combined
        }
    }
}

/// Require a capability by name from a registry descriptor.
///
/// The declared name is what the transport enforces at handshake time: a
/// provider that does not advertise it cannot connect. Checking the descriptor
/// first means an unqualified configuration fails before anything is opened.
fn require_capability(descriptor: &ProviderDescriptor, name: &str, why: &str) -> Result<()> {
    if descriptor.capabilities.iter().any(|c| c == name) {
        return Ok(());
    }
    Err(error(
        ErrorKind::UnsupportedCapability,
        "run.capabilities",
        format!(
            "provider '{}' (role {}) must be required to advertise '{name}' for {why}; \
             add it to that descriptor's capabilities in the registry, and install a \
             provider build that advertises it",
            descriptor.id, descriptor.role
        ),
    ))
}

fn millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Diagnostic host label only; it is not takeover evidence and proves nothing.
fn host_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unidentified-host".to_owned())
}

/// Validate everything decidable without opening a provider connection.
fn validate(spec: &RunSpec) -> Result<()> {
    spec.registry.validate()?;
    if !spec.experimental {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.validate",
            "this run mode is experimental; pass --experimental to acknowledge it",
        ));
    }
    if spec.persistence == RunPersistence::StrictRemote {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.validate",
            "strict remote persistence is not qualified by any storage provider; \
             use the default NFS client-fsync mode or --local-dev",
        ));
    }
    if !spec.workspace.is_absolute() {
        return Err(error(
            ErrorKind::InvalidPath,
            "run.validate",
            "workspace must be an absolute path",
        ));
    }
    if !spec.workspace.is_dir() {
        return Err(error(
            ErrorKind::InvalidInput,
            "run.validate",
            format!(
                "workspace {} is not an existing directory",
                spec.workspace.display()
            ),
        ));
    }
    match &spec.launch {
        RunLaunch::Command(command) => {
            if !command.executable.is_absolute() {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "run.validate",
                    "the command must be an absolute executable path; PATH is not searched",
                ));
            }
            if command.argv.is_empty() {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "run.validate",
                    "argv must include argv[0]",
                ));
            }
            if command.argv.iter().any(|a| a.contains(&0)) {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "run.validate",
                    "argument contains a NUL byte",
                ));
            }
            for variable in &command.environment {
                if variable.name.is_empty()
                    || variable.name.contains(&b'=')
                    || variable.name.contains(&0)
                    || variable.value.contains(&0)
                {
                    return Err(error(
                        ErrorKind::InvalidInput,
                        "run.validate",
                        "environment names must be nonempty and free of '=' and NUL",
                    ));
                }
            }
            if !command.cwd.is_absolute() {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "run.validate",
                    "the working directory must be absolute",
                ));
            }
        }
        RunLaunch::Agent(_) => {
            return Err(error(
                ErrorKind::NotImplemented,
                "run.validate",
                "agent adapter runs are not implemented; run a command directly",
            ))
        }
    }
    if spec.registry.providers.contains_key("namespace") {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.validate",
            "an alternative namespace provider cannot yet own a run lifecycle \
             (renew/finish/fail are not in its protocol); remove the namespace role \
             and configure storage and journal directly",
        ));
    }
    let storage = spec.registry.get("storage")?;
    let mode = match spec.persistence {
        RunPersistence::NfsClientFsync => caps::STORAGE_MOUNTED_NFSV4_V1,
        RunPersistence::LocalDevelopment => caps::STORAGE_LOCAL_DEVELOPMENT_V1,
        RunPersistence::StrictRemote => unreachable!("rejected above"),
    };
    require_capability(storage, mode, "the selected persistence mode")?;
    require_capability(
        storage,
        caps::STORAGE_OPEN_REWRITE_V1,
        "kernel-visible rewrite targets",
    )?;
    spec.registry.get("journal")?;
    let platform = spec.registry.get("platform")?;
    require_capability(
        platform,
        caps::PLATFORM_SANDBOXED_LAUNCH_V1,
        "installing enforcement before the target's first instruction",
    )?;
    require_capability(
        platform,
        caps::PLATFORM_SYSCALL_REWRITE_V1,
        "redirecting intercepted syscalls into the run's shadow",
    )?;
    Ok(())
}

/// Compose and execute one run.
///
/// Returns `Ok(())` only when the supervised tree finished with a successful root
/// status **and** its data, journal and writer authority were closed cleanly. A
/// nonzero or signalled child after a clean teardown is reported as
/// [`ErrorKind::ProcessFailed`], which is a run result, not a malfunction.
#[cfg(unix)]
pub fn run(spec: RunSpec) -> Result<()> {
    unix::run(spec)
}

/// Unsupported hosts fail explicitly rather than importing a backend.
#[cfg(not(unix))]
pub fn run(_spec: RunSpec) -> Result<()> {
    Err(error(
        ErrorKind::UnsupportedCapability,
        "run",
        "supervised runs require a Unix host with local provider transport",
    ))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::base::{HostReadOnlyBase, WorkspaceInventory};
    use crate::{RunBudget, Supervisor};
    use std::os::unix::ffi::OsStrExt;
    use umbra_core::{
        AcquireWriterRequest, CreateKind, CreateOptions, FailedRunRequest, FinishRunRequest,
        JournalAccess, JournalControlBinding, JournalFencingEvidence, JournalFormatPolicy,
        JournalOpenRequest, JournalWriterAuthority, OpenRunIntent, OpenRunRequest, PhysicalPath,
        RequestContext, StorageAnchor, StoragePath, StoragePolicy, TakeoverPolicy,
    };
    use umbra_overlay::{standard_namespace, NamespaceSession, SessionConfig};
    use umbra_storage::Storage;
    use uuid::Uuid;

    pub fn run(mut spec: RunSpec) -> Result<()> {
        validate(&spec)?;
        let mut observer = spec.observer.take();
        let run_id = RunId(Uuid::new_v4());
        let writer_id = WriterId(Uuid::new_v4().to_string());
        let RunLaunch::Command(command) = spec.launch.clone() else {
            unreachable!("validated above");
        };

        // Fingerprint the approved workspace before anything is opened, so the
        // run's base identity describes the state preparation actually saw.
        let inventory = WorkspaceInventory::capture(&spec.workspace)?;
        let immutable_base = inventory.contract();

        let timeout = spec.registry.timeout_ms;
        let mut storage: Box<dyn Storage> = Box::new(umbra_storage::provider::Proxy::connect(
            spec.registry.get("storage")?,
            timeout,
        )?);

        let policy = StoragePolicy {
            read_only: false,
            // Both remain false: neither guarantee is measured, and requiring
            // them here would only turn an unproven claim into a passing check.
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        };
        let binding = storage.open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::CreateNew,
            immutable_base,
            policy,
        })?;
        if binding.run_id != run_id {
            return Err(with_cleanup(
                error(
                    ErrorKind::ProtocolMismatch,
                    "run.open",
                    "storage opened a different run",
                ),
                storage.close_run(),
            ));
        }

        let lease = match storage.acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: writer_id.clone(),
            // A fresh run has no former writer to displace, and stale-writer
            // takeover is never offered as a recovery affordance.
            takeover: TakeoverPolicy::Refuse,
        }) {
            Ok(lease) => lease,
            Err(e) => return Err(with_cleanup(e, storage.close_run())),
        };

        let context = RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey(format!("run/{}", run_id.0)),
            writer_epoch: Some(lease.epoch),
        };

        // Everything below owns the lease, so every failure path must decide
        // explicitly whether releasing it is provably safe.
        let prepared = (|storage: &mut dyn Storage| -> Result<()> {
            check_renewal_budget(&lease, timeout)?;
            // A writable temporary directory inside the run's own shadow, so the
            // sandbox needs no host /tmp carve-out.
            storage.create(
                &context,
                &StoragePath::new(StorageAnchor::Root, b"tmp".to_vec())?,
                &CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            )?;
            Ok(())
        })(storage.as_mut());
        if let Err(e) = prepared {
            let e = with_cleanup(e, storage.release_writer(&lease));
            return Err(with_cleanup(e, storage.close_run()));
        }

        let control_path = binding
            .control
            .physical_path
            .clone()
            .ok_or_else(|| {
                error(
                    ErrorKind::UnsupportedCapability,
                    "run.journal",
                    "storage exposes no kernel path for the control directory",
                )
            })
            .map_err(|e| {
                let e = with_cleanup(e, storage.release_writer(&lease));
                with_cleanup(e, storage.close_run())
            })?;

        let mut journal: Box<dyn umbra_journal::Journal> = Box::new(
            match umbra_journal::provider::Proxy::connect(spec.registry.get("journal")?, timeout) {
                Ok(journal) => journal,
                Err(e) => {
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    return Err(with_cleanup(e, storage.close_run()));
                }
            },
        );

        let open = JournalOpenRequest {
            control: JournalControlBinding {
                run_id,
                directory: PhysicalPath(control_path),
            },
            access: JournalAccess::Writer(JournalWriterAuthority {
                run_id,
                writer_instance_id: writer_id.0.clone(),
                host_fingerprint: host_label(),
                supervisor_version: env!("CARGO_PKG_VERSION").to_owned(),
                acquired_at: millis_now(),
                renewed_at: millis_now(),
                lease_epoch: lease.epoch,
                // Authority comes from the storage lease we just took; the epoch
                // identifies it. The renewal token is a credential and stays out.
                fencing: JournalFencingEvidence::Fenced {
                    mechanism: "storage-writer-lease-v1".to_owned(),
                    evidence: lease.epoch.0.to_be_bytes().to_vec(),
                },
            }),
            format: JournalFormatPolicy {
                readable_versions: vec![1],
                write_version: 1,
            },
        };
        let recovery = match journal.open(&open) {
            Ok(recovery) => recovery,
            Err(e) => {
                let e = with_cleanup(e, journal.close());
                let e = with_cleanup(e, storage.release_writer(&lease));
                return Err(with_cleanup(e, storage.close_run()));
            }
        };

        // From here the namespace owns storage and journal; failures go through
        // its own fail path so one owner decides what is released.
        let mut namespace = standard_namespace(storage, journal);
        let root_path = binding.root.physical_path.clone();
        let session = SessionConfig {
            binding,
            context,
            recovery,
            lease: lease.clone(),
        };
        let base = Box::new(HostReadOnlyBase::new(inventory.clone()));
        if let Err(e) = namespace.bind(session, base) {
            // Nothing has launched, so the tree is trivially gone.
            return Err(fail(namespace, run_id, e, true));
        }

        let prepared = (|| -> Result<LaunchSpec> {
            let write_root = root_path.ok_or_else(|| {
                error(
                    ErrorKind::UnsupportedCapability,
                    "run.sandbox",
                    "storage exposes no kernel path for this run's root; enforcement \
                     cannot be bound to a root that does not exist for the kernel",
                )
            })?;
            // Seatbelt matches `subpath` against the *resolved* path: measured on
            // macOS, a profile granting `/tmp/<run>/root` denies writes there
            // because `/tmp` is a symlink to `/private/tmp`. Resolving here, in
            // preparation, keeps the renderer free of filesystem knowledge and
            // stops a symlinked store from producing a run that fails at its
            // first write. Rewrite targets need no such treatment: the kernel
            // resolves them the same way before the policy is consulted.
            let write_root = resolved_root(&write_root)?;
            let profile = sandbox::render(&SandboxSpec { write_root })?;
            // Re-check the workspace immediately before launch: preparation must
            // not start a run against a workspace that moved underneath it.
            inventory.verify_unchanged()?;
            Ok(LaunchSpec {
                executable: command.executable.clone(),
                argv: command.argv.clone(),
                environment: environment_for(&command),
                cwd: command.cwd.clone(),
                policy: LaunchPolicy {
                    persistence: match spec.persistence {
                        RunPersistence::NfsClientFsync => PersistencePolicy::NfsClientFsync,
                        RunPersistence::LocalDevelopment => PersistencePolicy::LocalDevelopment,
                        RunPersistence::StrictRemote => unreachable!("rejected above"),
                    },
                    inherited_fds: vec![TracedFd(0), TracedFd(1), TracedFd(2)],
                },
                sandbox: SandboxRequirement::Required(profile),
            })
        })();
        let launch_spec = match prepared {
            Ok(spec) => spec,
            Err(e) => return Err(fail(namespace, run_id, e, true)),
        };

        let platform =
            match umbra_platform::provider::connect(spec.registry.get("platform")?, timeout) {
                Ok(platform) => platform,
                Err(e) => return Err(fail(namespace, run_id, e, true)),
            };
        let capabilities = platform.control.capabilities();
        let architecture = match capabilities.architectures.first() {
            Some(architecture) => architecture.clone(),
            None => {
                return Err(fail(
                    namespace,
                    run_id,
                    error(
                        ErrorKind::UnsupportedCapability,
                        "run.platform",
                        "platform provider advertises no architecture",
                    ),
                    true,
                ))
            }
        };
        let budget = RunBudget {
            // Renew at half the interval so one blocking event read cannot
            // consume the whole budget before the next renewal check.
            renew_after: Duration::from_millis(lease.renew_after_millis / 2),
            cwd: command.cwd.clone(),
            architecture,
            abi: capabilities
                .capabilities
                .iter()
                .next()
                .cloned()
                .unwrap_or_else(|| "provider-negotiated".to_owned()),
        };

        if let Some(observer) = observer.as_mut() {
            observer.prepared(run_id);
        }

        let mut supervisor = Supervisor::with_namespace(run_id, platform, namespace, None);
        if let Err(e) = supervisor.launch_prepared(launch_spec, budget) {
            // A rejected launch never created a process; anything else may have.
            let terminated = matches!(
                e.kind,
                ErrorKind::UnsupportedCapability
                    | ErrorKind::NotImplemented
                    | ErrorKind::InvalidInput
                    | ErrorKind::InvalidPath
                    | ErrorKind::InvalidState
                    | ErrorKind::ProtocolMismatch
            );
            return Err(fail(
                supervisor.into_parts().namespace,
                run_id,
                e,
                terminated,
            ));
        }

        let loop_result = supervisor.run();
        let outcome = RunOutcome {
            run_id,
            root_status: supervisor.root_status(),
            processes_exited: supervisor.processes_exited(),
        };
        // On the success path every process reported its exit, so the tree is gone
        // by observation and this call only reaps provider bookkeeping. On the
        // failure path its result is the only termination evidence we have.
        let reaped = supervisor.terminate_tree(TerminationPolicy::Immediate);
        let mut namespace = supervisor.into_parts().namespace;

        if let Err(primary) = loop_result {
            let terminated = reaped.is_ok();
            return Err(fail(namespace, run_id, primary, terminated));
        }

        if let Some(observer) = observer.as_mut() {
            observer.finished(&outcome);
        }
        namespace.finish_run(&FinishRunRequest {
            run_id,
            root_status: outcome.root_status,
            processes_exited: outcome.processes_exited,
        })?;

        match outcome.root_status {
            Some(ExitStatus::Code(0)) => Ok(()),
            Some(status) => Err(error(
                ErrorKind::ProcessFailed,
                "run.child",
                format!("supervised command finished with {status:?}"),
            )),
            None => Err(error(
                ErrorKind::InvalidState,
                "run.child",
                "the supervised tree ended without a root exit status",
            )),
        }
    }

    /// Leave a run explicitly failed, preserving the original error.
    fn fail(
        mut namespace: Box<dyn NamespaceSession + Send>,
        run_id: RunId,
        primary: UmbraError,
        tree_terminated: bool,
    ) -> UmbraError {
        let cleanup = namespace.fail_run(&FailedRunRequest {
            run_id,
            reason: primary.to_string(),
            tree_terminated,
        });
        with_cleanup(primary, cleanup)
    }

    /// A blocking event read must not be able to outlast the renewal budget.
    fn check_renewal_budget(lease: &umbra_core::WriterLease, timeout_ms: u64) -> Result<()> {
        if lease.renew_after_millis == 0 || lease.renew_after_millis / 2 <= timeout_ms {
            return Err(error(
                ErrorKind::InvalidState,
                "run.lease",
                format!(
                    "writer lease renews every {} ms, which a {timeout_ms} ms provider \
                     deadline could overrun; lower timeout_ms in the registry",
                    lease.renew_after_millis
                ),
            ));
        }
        Ok(())
    }

    /// The launch environment, with writable runtime roots pointed inside the run.
    fn environment_for(command: &CommandLaunch) -> Vec<EnvironmentVariable> {
        let mut environment = command.environment.clone();
        // `/tmp` is logical: the shadow directory created during preparation is
        // where intercepted opens land, so no host /tmp write allowance is needed.
        if !environment.iter().any(|v| v.name == b"TMPDIR") {
            environment.push(EnvironmentVariable {
                name: b"TMPDIR".to_vec(),
                value: b"/tmp".to_vec(),
            });
        }
        environment
    }

    /// Resolve the run root to the path the kernel will match, and require it to
    /// be an existing directory before enforcement is rendered from it.
    fn resolved_root(root: &BytePath) -> Result<BytePath> {
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(root.as_bytes()));
        let resolved = std::fs::canonicalize(&path).map_err(|e| {
            error(
                ErrorKind::InvalidPath,
                "run.sandbox",
                format!("cannot resolve the run root {}: {e}", path.display()),
            )
        })?;
        if !resolved.is_dir() {
            return Err(error(
                ErrorKind::InvalidPath,
                "run.sandbox",
                format!("the run root {} is not a directory", resolved.display()),
            ));
        }
        BytePath::new(resolved.as_os_str().as_bytes().to_vec())
    }
}
