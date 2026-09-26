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

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt;

use umbra_core::{
    capabilities as caps, provider::ProviderDescriptor, provider::ProviderRegistry,
    AgentLaunchRequest, BytePath, EnvironmentVariable, ErrorKind, ExitStatus, IdempotencyKey,
    JournalAccess, JournalControlBinding, JournalFormatPolicy, JournalOpenRequest,
    JournalTailRecovery, LaunchPolicy, LaunchSpec, LeaseEpoch, OperationId, PersistencePolicy,
    PhysicalPath, Result, RunId, SandboxRequirement, Sequence, StorageAnchor, StoragePath,
    TerminationPolicy, TracedFd, UmbraError, WriterId,
};

use crate::sandbox::{self, SandboxSpec};

fn select_architecture(
    architectures: &[umbra_core::Architecture],
) -> Result<umbra_core::Architecture> {
    match architectures {
        [architecture] => Ok(architecture.clone()),
        _ => Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "run.platform",
            "platform must advertise exactly one architecture until architecture negotiation is supported",
        )),
    }
}

/// Match ABI identities to the negotiated architecture, ignoring unrelated ABIs.
fn select_abi(
    names: &std::collections::BTreeSet<String>,
    architecture: &umbra_core::Architecture,
) -> Result<String> {
    let suffix = match architecture {
        umbra_core::Architecture::Aarch64 => "-arm64",
        umbra_core::Architecture::X86_64 => "-x86_64",
        umbra_core::Architecture::Unsupported(_) => {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "run.platform",
                "unsupported ABI architecture",
            ))
        }
    };
    let mut matching = names.iter().filter(|name| {
        name.rsplit_once("-abi-v").is_some_and(|(prefix, version)| {
            prefix.ends_with(suffix)
                && prefix.len() > suffix.len()
                && !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit())
        })
    });
    match (matching.next(), matching.next()) {
        (Some(abi), None) => Ok(abi.clone()),
        _ => Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "run.platform",
            "platform must advertise exactly one ABI identity for the negotiated architecture",
        )),
    }
}

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
    /// Provider bookkeeping failed after all processes reported exit.
    fn teardown_warning(&mut self, error: &UmbraError);
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
    /// Host directory holding per-run state that cannot live in the store.
    ///
    /// Used **only** by a routed run -- storage exposing no kernel-visible path.
    /// Every other backend keeps its journal inside its own run directory, and
    /// this is not consulted for one. See [`host_state`].
    pub state_root: PathBuf,
    /// Optional status sink.
    pub observer: Option<Box<dyn RunObserver>>,
}

/// Everything reopening an existing run needs.
///
/// Deliberately not a `RunSpec`: there is nothing to launch. A reopen opens the
/// run's storage and journal, lets `bind` classify what the last session left,
/// and closes again. `workspace` is still required and still fingerprinted,
/// because a run's base identity is part of what `OpenExisting` validates — a
/// backend that finds a manifest naming another base refuses the open rather than
/// admitting an unverified session.
pub struct ResumeSpec {
    /// Explicitly installed providers and their opaque options.
    pub registry: ProviderRegistry,
    /// The run to reopen. Run enumeration is out of scope; the caller knows this.
    pub run_id: RunId,
    /// Absolute host directory approved as this run's workspace, as at creation.
    pub workspace: PathBuf,
    /// Storage selection.
    pub persistence: RunPersistence,
    /// The same host state root the run was created with. See [`RunSpec::state_root`].
    pub state_root: PathBuf,
}

/// What reopening an existing run found.
///
/// `Ok(ResumeOutcome)` means the reopen itself succeeded — storage opened, writer
/// authority reacquired, journal replayed, `bind` reached a verdict. It does
/// **not** mean the run is usable: that is `recovery_required`, and a caller that
/// ignores it is ignoring the whole point of the reopen. `Err` is reserved for a
/// reopen that could not be performed at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeOutcome {
    /// Stable run identity; independent of the physical mount root.
    pub run_id: RunId,
    /// `bind` classified this run as requiring recovery and poisoned the session.
    ///
    /// Either a previous session declared it by journaling
    /// `JournalLifecycle::RecoveryRequired`, or the reopen inventory still held a
    /// transaction that cannot be discarded — see `reconcilable_on_reopen`.
    ///
    /// **There is a third outcome, and it is an `Err` rather than a value here:**
    /// a reopen that cannot find this run's journal evidence *refuses*. For a run
    /// whose journal lives outside its store — the only shape that has one — a
    /// missing log would otherwise open as a fresh journal, which has nothing
    /// pending, which would report `false` for a run that may have crashed
    /// mid-transaction. `verify_journal_binding` turns that into a refusal by
    /// requiring the log's own first record to carry the journal identity the
    /// store holds, which a freshly created log does not have.
    ///
    /// **Scoped precisely, because the scope is what makes the claim true.**
    /// `false` means "classified, and no reconciliation is required" for a log
    /// that is present, opens with this run's identity, and whose tail is intact
    /// (`resume` refuses a torn one separately). It does **not** survive a log
    /// silently truncated to a shorter valid prefix: nothing here knows how far
    /// the run reached, and persisting that is barrier work this slice does not
    /// do. That residue is stated in `umbra-storage-nfs-userspace`'s README.
    pub recovery_required: bool,
    /// Unfinished operations the reopen inventory still carried.
    pub unfinished: usize,
    /// How far the reopened log reached.
    pub last_valid_sequence: Sequence,
    /// The writer epoch this reopen acquired. Strictly above every epoch the run
    /// has used before; see `resume`'s note on where that is enforced.
    pub writer_epoch: LeaseEpoch,
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
/// The admission checks that describe the **run**, not the launch.
///
/// Shared by `validate` and `resume` rather than restated in each, and that is
/// the point rather than a tidy-up: restating them is how two clauses went
/// missing in two consecutive review rounds -- the persistence capability, then
/// the namespace-role refusal. A reopen must not accept a configuration `run`
/// would reject, and the only durable way to keep that true is for there to be
/// one list. A third omission is now a compile-time impossibility rather than
/// something the next reviewer has to notice.
///
/// What is deliberately **not** here is everything that describes launching: the
/// `--experimental` acknowledgement, the `RunLaunch` shape, storage's
/// `experimental-open-rewrite-v1` (it qualifies kernel-visible rewrite targets)
/// and the two platform capabilities. A reopen resolves nothing, rewrites
/// nothing, launches nothing and never connects the platform role, so requiring
/// those would turn an unqualified claim into a passing check -- the shape this
/// codebase refuses elsewhere. `validate` adds them; `resume` does not.
///
/// `operation` labels the caller so an error from a `umbra resume` invocation
/// does not claim to come from `run`.
fn admit_run(
    registry: &ProviderRegistry,
    workspace: &Path,
    persistence: RunPersistence,
    operation: &str,
) -> Result<()> {
    registry.validate()?;
    if persistence == RunPersistence::StrictRemote {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            operation,
            "strict remote persistence is not qualified by any storage provider; \
             use the default NFS client-fsync mode or --local-dev",
        ));
    }
    if !workspace.is_absolute() {
        return Err(error(
            ErrorKind::InvalidPath,
            operation,
            "workspace must be an absolute path",
        ));
    }
    if !workspace.is_dir() {
        return Err(error(
            ErrorKind::InvalidInput,
            operation,
            format!(
                "workspace {} is not an existing directory",
                workspace.display()
            ),
        ));
    }
    // Every registry that configures a namespace role is refused, before anything
    // is opened. The protocol now carries the lifecycle calls, but this
    // supervisor still builds its own `standard_namespace` over the storage and
    // journal roles and never opens a namespace descriptor, so admitting one
    // would silently discard it -- on the reopen path exactly as on the run path.
    // The capability check runs first so an unqualified descriptor is named as
    // such; it is the forward-looking gate rather than the thing keeping the run
    // out. A descriptor's `capabilities` are operator-written registry JSON, so
    // declaring the name is a claim, not evidence, and it does not admit the run.
    if let Some(namespace) = registry.providers.get("namespace") {
        require_capability(
            namespace,
            caps::NAMESPACE_RUN_LIFECYCLE_V1,
            "owning the run lifecycle (renew, finish and fail) for an alternative \
             namespace provider",
        )?;
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.capabilities",
            format!(
                "provider '{}' declares '{}', but this supervisor does not yet route a \
                 run to a configured namespace provider: it always binds its own overlay \
                 over the storage and journal roles. Remove the namespace role and \
                 configure storage and journal directly",
                namespace.id,
                caps::NAMESPACE_RUN_LIFECYCLE_V1
            ),
        ));
    }
    let storage = registry.get("storage")?;
    // One mode, two clients. `NfsClientFsync` names what the *run* promises -- an
    // NFSv4-backed run with no strict-remote durability claim -- and both a
    // validated kernel mount and a qualified userspace client deliver it. They
    // are separate capability names because they are separate measurements:
    // `mounted-nfsv4-v1` asserts a mount this host validated, and a userspace
    // client has none to validate. Accepting either here is not a widening of the
    // gate: each is still earned by a live probe in its own backend, and a
    // provider that probed nothing advertises neither.
    //
    // A userspace client's own durability is *stronger* than the name promises --
    // a matched-verifier COMMIT rather than a client fsync -- so the run
    // under-claims rather than over-claims, which is the safe direction.
    let accepted: &[&str] = match persistence {
        RunPersistence::NfsClientFsync => &[
            caps::STORAGE_MOUNTED_NFSV4_V1,
            caps::STORAGE_USERSPACE_NFSV4_V1,
        ],
        RunPersistence::LocalDevelopment => &[caps::STORAGE_LOCAL_DEVELOPMENT_V1],
        RunPersistence::StrictRemote => unreachable!("rejected above"),
    };
    if !accepted
        .iter()
        .any(|name| storage.capabilities.iter().any(|declared| declared == name))
    {
        // Report the first name, which is the ordinary one, and list the
        // alternative rather than hiding it behind a generic message.
        // `e.operation`, not `operation`: `require_capability` stamps
        // `"run.capabilities"`, which is the narrower and more useful label, and
        // it is what the same failure carries at every other call site in this
        // file. Substituting the caller's label here made one capability refusal
        // report a different origin from its siblings purely because it had an
        // alternative name to list.
        return require_capability(storage, accepted[0], "the selected persistence mode").map_err(
            |e| {
                if accepted.len() > 1 {
                    error(
                        e.kind,
                        &e.operation,
                        format!("{} (or '{}')", e.context, accepted[1]),
                    )
                } else {
                    e
                }
            },
        );
    }
    registry.get("journal")?;
    Ok(())
}

fn validate(spec: &RunSpec) -> Result<()> {
    spec.registry.validate()?;
    if !spec.experimental {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.validate",
            "this run mode is experimental; pass --experimental to acknowledge it",
        ));
    }
    admit_run(
        &spec.registry,
        &spec.workspace,
        spec.persistence,
        "run.validate",
    )?;
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
    // The launch-only half. Everything above the `match` is in `admit_run`,
    // which `resume` shares; these describe rewriting and launching, which a
    // reopen does neither of.
    let storage = spec.registry.get("storage")?;
    let platform = spec.registry.get("platform")?;
    // Two ways a tracee's file operations can reach the run's storage, and a
    // registry declares exactly one of them. They are alternatives rather than a
    // fallback pair: a backend with no kernel-visible path cannot be handed a
    // rewrite target at all, and a backend that has one needs no interposer.
    if declares_routing(storage) {
        require_capability(
            platform,
            caps::PLATFORM_INTERPOSE_V1,
            "routing a tracee's file operations through umbra, for storage that exposes \
             no kernel path to rewrite a syscall operand to",
        )?;
    } else {
        require_capability(
            storage,
            caps::STORAGE_OPEN_REWRITE_V1,
            "kernel-visible rewrite targets",
        )?;
    }
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

/// Whether this storage descriptor declares that its runs must be routed.
///
/// A declaration, and only ever used to *admit* a run: what actually decides the
/// mode is the run binding's own absent `physical_path`, checked after `open_run`
/// against this answer. Registry capabilities are operator-written JSON, so
/// declaring the name is a claim rather than evidence -- and a claim that
/// disagrees with the binding stops the run rather than picking a side.
fn declares_routing(descriptor: &ProviderDescriptor) -> bool {
    descriptor
        .capabilities
        .iter()
        .any(|name| name == caps::STORAGE_USERSPACE_ROUTING_V1)
}

/// Per-run host state for a routed run, and the reason it exists at all.
///
/// A routed run's storage has no kernel-visible path, and two things still need
/// one: umbra's own journal, whose `JournalControlBinding` carries a
/// `PhysicalPath`, and the Seatbelt profile's single write allowance, which names
/// a path the kernel resolves. Neither is the tracee's data -- that goes to the
/// store through the userspace client -- so neither is a weakening of where the
/// run's contents live.
///
/// The two live in sibling directories under `<state_root>/<run-id>/`, and the
/// separation is load-bearing rather than tidy: `host/` is the only path the
/// tracee may write, and the journal must not be inside anything the tracee can
/// write. A single directory serving both would put umbra's own log inside the
/// tracee's write grant.
struct HostState {
    /// `<state_root>/<run-id>/journal` -- the journal's control directory.
    journal: PathBuf,
    /// `<state_root>/<run-id>/host` -- the tracee's *only* host write allowance.
    ///
    /// **What this is: a narrowing.** The Seatbelt template carries exactly one
    /// write rule and it names a real path; a kernel-path run points it at the
    /// store's own run root, and a routed run has no such path, so it points at
    /// an empty per-run directory instead. That is strictly less than the grant a
    /// kernel-path run receives, and nothing in the tracee's logical namespace
    /// resolves to it.
    ///
    /// **What it is not: a detector for anything that escaped routing.** It
    /// catches a write that names *this exact absolute path* and nothing else;
    /// every other host write is refused by the profile and leaves nothing here
    /// to find. The fixture suite asserts it is empty, and what that assertion
    /// means is "this run did not use its one host write allowance" -- not "no
    /// operation reached the host".
    host: PathBuf,
}

/// Where one routed run's host state lives. Pure: creates nothing.
///
/// Separated from [`create_host_state`] so a *reopen* can name the directories it
/// expects without bringing them into being. A `resume` that creates them and
/// then refuses leaves two empty directories behind for a run it declined to
/// touch -- and, worse, creates the very empty journal directory whose
/// existence-without-a-log this fence exists to refuse.
fn host_state(state_root: &Path, run_id: RunId, operation: &str) -> Result<HostState> {
    if !state_root.is_absolute() {
        return Err(error(
            ErrorKind::InvalidPath,
            operation,
            "the host state root must be an absolute path",
        ));
    }
    let run = state_root.join(run_id.0.to_string());
    Ok(HostState {
        journal: run.join("journal"),
        host: run.join("host"),
    })
}

/// Bring one routed run's host state into being. Only `run` calls this.
fn create_host_state(state: &HostState, operation: &str) -> Result<()> {
    for directory in [&state.journal, &state.host] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|e| {
                error(
                    ErrorKind::Io,
                    operation,
                    format!("creating {}: {e}", directory.display()),
                )
            })?;
    }
    Ok(())
}

/// Where the journal-instance nonce lives in the store, under `control/`.
///
/// `control/` is umbra's own anchor: the overlay refuses a tracee operation that
/// names it (`lookup`, "control is not tracee-visible"), so nothing the
/// supervised program does can reach this object.
fn nonce_path() -> Result<StoragePath> {
    StoragePath::new(StorageAnchor::Control, b"journal-id".to_vec())
}

/// The descriptor fence a routed run applies, and the floor of umbra's own
/// virtual descriptor range.
///
/// One constant for both halves, because they are one boundary: the platform
/// lowers the tracee's `RLIMIT_NOFILE` -- soft and hard -- to this value before
/// the target execs, and the supervisor allocates routed descriptors at or above
/// it. Two constants could drift into overlapping, which is a wrong-object read
/// rather than a refusal.
///
/// It is deliberately *not* read from the platform provider. The supervisor has
/// to know the floor to allocate against it, and the platform has to know it to
/// fence with it; making it a supervisor constant carried in `LaunchPolicy` means
/// the platform is told the value rather than asked for it, so there is one
/// source and the two cannot disagree.
const DESCRIPTOR_FENCE: u32 = 4096;

/// One request identity per storage call, derived from the run's own.
///
/// A backend may bind an operation id to the first idempotency key it was used
/// with and refuse a second, different request under it -- which is exactly right,
/// and exactly what a single `RequestContext` reused across a `create` and its
/// following `write_at` runs into. `Overlay::context` derives per request for the
/// same reason; this is the direct-caller equivalent, and the run's own key still
/// seeds it so every request stays attributable to the run that made it.
#[cfg(unix)]
fn derived(context: &umbra_core::RequestContext, label: &str) -> umbra_core::RequestContext {
    let mut derived = context.clone();
    derived.operation_id = OperationId(uuid::Uuid::new_v4());
    derived.idempotency_key = IdempotencyKey(format!("{}/{label}", context.idempotency_key.0));
    derived
}

/// Mint this run's journal-instance nonce and record it in the store.
///
/// The store half only. [`seal_journal`] puts the same value **inside the log**,
/// and the two together are the fence: see [`verify_journal_binding`] for why a
/// value beside the log was not enough.
#[cfg(unix)]
fn publish_nonce(
    storage: &mut dyn umbra_storage::Storage,
    context: &umbra_core::RequestContext,
    operation: &str,
) -> Result<uuid::Uuid> {
    let nonce = uuid::Uuid::new_v4();
    let bytes = nonce.to_string().into_bytes();
    let path = nonce_path()?;
    storage.create(
        &derived(context, "nonce/create"),
        &path,
        &umbra_core::CreateOptions {
            kind: umbra_core::CreateKind::File,
            mode: 0o600,
        },
    )?;
    let mut written = 0usize;
    while written < bytes.len() {
        let count = storage.write_at(
            &derived(context, &format!("nonce/write/{written}")),
            &path,
            written as u64,
            &bytes[written..],
        )?;
        if count == 0 {
            return Err(error(
                ErrorKind::Io,
                operation,
                "storage made no progress writing the journal nonce",
            ));
        }
        written += count;
    }
    Ok(nonce)
}

/// Write the run's nonce **into its log**, as the log's first record.
///
/// `JournalLifecycle::Started` carries no fields, so the nonce rides in the
/// record's `operation_id`. That is deliberate and it is the whole mechanism:
/// there is no operation at run-open to identify, the id has to be *some* value,
/// and making it the journal-instance identity means the log itself says which
/// instance it is. **Do not "tidy" this into `Uuid::new_v4()`** -- that turns the
/// fence below back into something a freshly created log satisfies.
///
/// Flushed before it returns, so the binding is durable before the tracee runs.
#[cfg(unix)]
fn seal_journal(
    journal: &mut dyn umbra_journal::Journal,
    nonce: uuid::Uuid,
    epoch: LeaseEpoch,
) -> Result<()> {
    let sequence = journal.append(&umbra_core::JournalRecord {
        format_version: 1,
        sequence: Sequence(0),
        operation_id: OperationId(nonce),
        writer_epoch: epoch,
        payload: umbra_core::JournalPayload::Lifecycle(umbra_core::JournalLifecycle::Started),
    })?;
    journal.flush(sequence)?;
    Ok(())
}

/// Require that the log at this state root **is** the log this run created.
///
/// **Never answers "no journal was found here, so nothing needs recovery".** That
/// is the whole reason this exists: a writer `journal.open` against an
/// existing-but-empty directory establishes a *fresh* log, which has nothing
/// pending, which makes `Overlay::bind` decline to poison, which makes
/// `requires_recovery()` answer `false` -- for a run that may have crashed
/// mid-transaction. Missing evidence and matching evidence are different answers
/// and this returns the difference.
///
/// **The comparison is against the log, not against a file beside it, and the
/// difference is load-bearing.** An earlier version wrote the nonce to a
/// `journal-id` sidecar and compared that. Measured, it left the fail-open wide
/// open: removing the log alone -- a partial loss of the state directory, a
/// truncating restore, an operator clearing "the big file" -- left the sidecar
/// matching, so the reopen established a fresh log and reported "requires no
/// reconciliation", exit 0. The nonce now lives in the log's own first record,
/// which a fresh log does not have and cannot invent.
///
/// Read through a **second, read-only** journal session, before the writer open:
/// opening as a writer is what creates the fresh log, so the absence has to be
/// observed while it is still observable. `FileJournal::open` refuses a second
/// session on one instance, hence a separate connection rather than an
/// open/close/open on the caller's.
#[cfg(unix)]
fn verify_journal_binding(
    storage: &mut dyn umbra_storage::Storage,
    context: &umbra_core::RequestContext,
    descriptor: &ProviderDescriptor,
    timeout_ms: u64,
    state: &HostState,
    run_id: RunId,
) -> Result<()> {
    let refuse = |detail: String| {
        error(
            ErrorKind::InvalidState,
            "resume.journal",
            format!(
                "this run's journal evidence was not found: {detail}. A run journalled \
                 outside its store is single-host -- reopening it needs the store and the \
                 log together, and the log lives on the host that created the run. This is \
                 a refusal, not a verdict: it does not mean the run needs no recovery",
            ),
        )
    };
    let path = nonce_path()?;
    let stat = storage
        .stat(&derived(context, "nonce/stat"), &path)
        .map_err(|e| refuse(format!("the store holds no nonce for it ({e})")))?;
    if stat.len == 0 || stat.len > 128 {
        return Err(refuse(
            "the store's nonce is empty or implausibly long".to_owned(),
        ));
    }
    let mut stored = vec![0u8; stat.len as usize];
    let read = storage
        .read_at(&derived(context, "nonce/read"), &path, 0, &mut stored)
        .map_err(|e| refuse(format!("the store's nonce could not be read ({e})")))?;
    stored.truncate(read);
    let stored = std::str::from_utf8(&stored)
        .ok()
        .and_then(|text| uuid::Uuid::parse_str(text).ok())
        .ok_or_else(|| refuse("the store's nonce is not a journal identity".to_owned()))?;

    if !state.journal.is_dir() {
        return Err(refuse(format!(
            "{} does not exist, so this host holds no log for it",
            state.journal.display()
        )));
    }
    let directory = PhysicalPath(BytePath::new(
        state.journal.as_os_str().as_bytes().to_vec(),
    )?);
    let mut reader: Box<dyn umbra_journal::Journal> = Box::new(
        umbra_journal::provider::Proxy::connect(descriptor, timeout_ms)
            .map_err(|e| refuse(format!("the journal provider would not start ({e})")))?,
    );
    let sealed = (|| -> Result<()> {
        reader
            .open(&JournalOpenRequest {
                control: JournalControlBinding { run_id, directory },
                access: JournalAccess::ReadOnly,
                format: JournalFormatPolicy {
                    readable_versions: vec![1],
                    write_version: 1,
                },
            })
            .map_err(|e| refuse(format!("its log could not be opened read-only ({e})")))?;
        let first = reader
            .replay(Sequence(0))
            .map_err(|e| refuse(format!("its log could not be replayed ({e})")))?
            .next()
            .transpose()
            .map_err(|e| refuse(format!("its first record will not decode ({e})")))?
            .ok_or_else(|| {
                refuse(
                    "its log is empty, so it is a journal this reopen would have created \
                     rather than the one the run wrote"
                        .to_owned(),
                )
            })?;
        match first.payload {
            umbra_core::JournalPayload::Lifecycle(umbra_core::JournalLifecycle::Started)
                if first.operation_id.0 == stored => {}
            _ => {
                return Err(refuse(
                    "its log does not open with this run's journal identity, so it belongs \
                     to a different journal instance"
                        .to_owned(),
                ))
            }
        }
        Ok(())
    })();
    // Close the reader whatever the verdict; a refusal must not leave a provider
    // process behind.
    with_cleanup_unit(sealed, reader.close())
}

/// Preserve a primary verdict while still reporting a failed cleanup.
#[cfg(unix)]
fn with_cleanup_unit(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match primary {
        Err(e) => Err(with_cleanup(e, cleanup)),
        Ok(()) => cleanup,
    }
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

/// Reopen an existing run and report what the last session left behind.
///
/// Opens with [`OpenRunIntent::OpenExisting`], reacquires writer authority,
/// replays the journal and binds. `bind` classifies the recovery inventory
/// ([#65](https://github.com/invakid404/umbra/issues/65)); this reports its
/// verdict and then closes everything down again. Nothing is launched and no
/// completion record is written, so a reopen of a healthy run leaves it exactly
/// as it found it, one writer epoch later.
#[cfg(unix)]
pub fn resume(spec: ResumeSpec) -> Result<ResumeOutcome> {
    unix::resume(spec)
}

/// Unsupported hosts fail explicitly rather than importing a backend.
#[cfg(not(unix))]
pub fn resume(_spec: ResumeSpec) -> Result<ResumeOutcome> {
    Err(error(
        ErrorKind::UnsupportedCapability,
        "resume",
        "reopening a run requires a Unix host with local provider transport",
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
        // The mode, decided by evidence: whether this run *has* kernel-visible
        // paths, not whether its registry said so. `validate` already admitted the
        // run on the declaration; this refuses the two ways the declaration and
        // the binding can disagree, because either one silently changes where the
        // tracee's writes land.
        let routed = binding.root.physical_path.is_none();
        if routed != declares_routing(spec.registry.get("storage")?) {
            return Err(with_cleanup(
                error(
                    ErrorKind::ProtocolMismatch,
                    "run.open",
                    if routed {
                        "storage exposes no kernel path for this run, but its registry \
                         descriptor does not declare '{}'; add it, or install a backend \
                         that binds kernel paths"
                    } else {
                        "storage exposes kernel paths for this run, but its registry \
                         descriptor declares '{}'; remove it"
                    }
                    .replace("{}", caps::STORAGE_USERSPACE_ROUTING_V1),
                ),
                storage.close_run(),
            ));
        }
        let host_state = if routed {
            match host_state(&spec.state_root, run_id, "run.state")
                .and_then(|state| create_host_state(&state, "run.state").map(|()| state))
            {
                Ok(state) => Some(state),
                Err(e) => return Err(with_cleanup(e, storage.close_run())),
            }
        } else {
            None
        };

        // Who this run's writer *is*, decided by the backend where the backend has
        // already decided it. A backend that admits in `open_run` has written an
        // identity into a durable marker before publishing anything, and asking it
        // to acquire under a different one is asking it to report an admission that
        // writer never obtained -- which it refuses, correctly. Minting one here is
        // the fallback, for the backends that admit at this call and record
        // whatever identity they are told.
        let writer_id = binding
            .admitted_writer
            .clone()
            .unwrap_or_else(|| WriterId(Uuid::new_v4().to_string()));
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

        // The journal's control directory comes from configuration, not from
        // `binding.control.physical_path`, only when storage exposes no kernel
        // path. `local` and kernel-`nfs` are unaffected and still bind the store's
        // own control directory.
        //
        // This is not a neutral relocation, and the limit is load-bearing:
        //
        // 1. A run journalled outside its store CANNOT be resumed on another
        //    host. Reopening needs the store *and* the log; the log is on this
        //    host's disk. `resume` refuses such an attempt rather than reporting a
        //    verdict.
        //
        // 2. The refusal is what makes (1) safe, and it is not optional. A writer
        //    `journal.open` against an existing-but-empty directory establishes a
        //    FRESH log (`umbra-journal-file`: `create_dir_all` + missing log => new
        //    header). A fresh log has nothing pending, so `Overlay::bind` does not
        //    poison and `requires_recovery()` answers `false`. Without the nonce
        //    check below, a crashed run whose log was merely *not found* would be
        //    reported healthy -- the silent `false` that `requires_recovery`'s own
        //    contract refuses, and the fail-open #65 was built to remove.
        //
        // So a nonce is minted per run, written to the store's `control/`, and
        // written **into the log itself** as its first record -- see
        // `seal_journal`. A reopen requires both, and the in-log half is what
        // makes the check mean anything: an earlier version compared the store's
        // copy against a `journal-id` file sitting *beside* the log, and measured,
        // removing the log alone left that sidecar matching, so the reopen
        // established a fresh log and reported "requires no reconciliation".
        //
        // What this closes, exactly, is the **vanished log**: a log whose first
        // record is not this run's identity, or which has no records at all, is
        // refused. What it does **not** close is a log truncated to a shorter but
        // still-valid prefix -- that needs the run's reached sequence persisted
        // somewhere the log cannot forge, which is barrier territory and
        // deliberately not in this slice. `JournalTailRecovery` still catches a
        // *torn* tail, and `resume` refuses that too.
        //
        // Do NOT "fix" a refusal here by creating the directory, seeding a nonce,
        // or relaxing the comparison, and do not give `seal_journal`'s record a
        // fresh operation id. Each turns a missing-evidence refusal back into a
        // clean verdict. The only real fix is to move the journal into the store,
        // which needs `JournalControlBinding` to carry something other than a
        // `PhysicalPath`; until then this limit is the honest behaviour.
        let control_path = match (&host_state, binding.control.physical_path.clone()) {
            (Some(state), _) => BytePath::new(state.journal.as_os_str().as_bytes().to_vec())
                .map_err(|e| {
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    with_cleanup(e, storage.close_run())
                })?,
            (None, Some(path)) => path,
            (None, None) => {
                let e = error(
                    ErrorKind::UnsupportedCapability,
                    "run.journal",
                    "storage exposes no kernel path for the control directory",
                );
                let e = with_cleanup(e, storage.release_writer(&lease));
                return Err(with_cleanup(e, storage.close_run()));
            }
        };

        // Mint and publish the nonce *before* the journal is opened. A crash
        // between the two leaves a store nonce with no log, which the reopen
        // refuses -- the fail-closed direction. Doing it after would leave a log
        // with no nonce, which is the same refusal, so the order is chosen for the
        // third case: a failure while writing the store copy must not leave a
        // usable-looking journal behind. `seal_journal` below puts the same value
        // inside the log.
        let journal_nonce = match &host_state {
            Some(_) => match publish_nonce(storage.as_mut(), &context, "run.journal") {
                Ok(nonce) => Some(nonce),
                Err(e) => {
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    return Err(with_cleanup(e, storage.close_run()));
                }
            },
            None => None,
        };

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

        // CreateNew must produce a fresh journal. Anything else is a storage /
        // journal disagreement, not recovery this new-run path can reconcile.
        if !recovery.clean
            || !recovery.pending.is_empty()
            || !matches!(recovery.tail, JournalTailRecovery::Intact)
            || recovery.last_valid_sequence != Sequence(0)
            || recovery.checkpoint.is_some()
        {
            let e = error(
                ErrorKind::InvalidState,
                "run.journal",
                "CreateNew returned a non-fresh journal",
            );
            let e = with_cleanup(e, journal.close());
            let e = with_cleanup(e, storage.release_writer(&lease));
            return Err(with_cleanup(e, storage.close_run()));
        }

        // Seal the log with this run's journal identity, as its first record, and
        // flush it before anything else touches the journal. The store already
        // holds the same value; the pair is what a reopen checks, and putting one
        // half *inside* the log is what stops a freshly created log from
        // satisfying the check. See `verify_journal_binding`.
        //
        // After the freshness gate above, deliberately: that gate reads the
        // recovery state `open` produced, and this record would make a CreateNew
        // journal look non-fresh to it.
        if let Some(nonce) = journal_nonce {
            if let Err(e) = seal_journal(journal.as_mut(), nonce, lease.epoch) {
                let e = with_cleanup(e, journal.close());
                let e = with_cleanup(e, storage.release_writer(&lease));
                return Err(with_cleanup(e, storage.close_run()));
            }
        }

        // From here the namespace owns storage and journal; failures go through
        // its own fail path so one owner decides what is released.
        let mut namespace = standard_namespace(storage, journal);
        // A routed run's tracee-visible root is not on this host at all, so the
        // profile's one write allowance names an empty per-run directory instead.
        // It is not where the run's data goes -- every routed operation reaches
        // the store through the userspace client -- and it is strictly narrower
        // than the store-root grant a kernel-path run receives. It is a
        // narrowing, not a detector: see `HostState::host` for what asserting it
        // stays empty does and does not establish.
        let root_path = match &host_state {
            Some(state) => BytePath::new(state.host.as_os_str().as_bytes().to_vec()).ok(),
            None => binding.root.physical_path.clone(),
        };
        let interpose = host_state.is_some();
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
                    interpose,
                    // Set together with `interpose` and only with it. The fence is
                    // what makes the interposer's virtual descriptors disjoint
                    // from the kernel's for the tracee's whole lifetime; applying
                    // it to a run that routes nothing would cap a tracee's
                    // descriptors for no reason at all.
                    descriptor_limit: interpose.then_some(DESCRIPTOR_FENCE),
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
        let architecture = match select_architecture(&capabilities.architectures) {
            Ok(architecture) => architecture,
            Err(e) => return Err(fail(namespace, run_id, e, true)),
        };
        let abi = match select_abi(&capabilities.capabilities, &architecture) {
            Ok(abi) => abi,
            Err(e) => return Err(fail(namespace, run_id, e, true)),
        };
        let budget = RunBudget {
            // Renewal is serviced at every provider boundary, including ABI
            // memory reads. Half the lease adds headroom; the admitted floor
            // avoids near-zero renewal intervals between those boundaries.
            renew_after: renewal_interval(lease.renew_after_millis),
            cwd: command.cwd.clone(),
            architecture,
            abi,
            descriptor_floor: interpose.then_some(DESCRIPTOR_FENCE),
        };

        if let Some(observer) = observer.as_mut() {
            observer.prepared(run_id);
        }

        let mut supervisor = Supervisor::with_namespace(run_id, platform, namespace, None);
        if let Err(e) = supervisor.launch_prepared(launch_spec, budget) {
            // The backend's explicit cleanup evidence survives launch_prepared
            // and IPC. Error category alone says nothing about a spawned tree.
            let terminated = e.launch_tree_terminated;
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

        if let Err(e) = reaped {
            if let Some(observer) = observer.as_mut() {
                observer.teardown_warning(&e);
            } else {
                tracing::warn!(error = %e, "provider teardown bookkeeping failed");
            }
        }
        if let Some(observer) = observer.as_mut() {
            observer.finished(&outcome);
        }
        if let Err(e) = namespace.finish_run(&FinishRunRequest {
            run_id,
            root_status: outcome.root_status,
            processes_exited: outcome.processes_exited,
        }) {
            return Err(fail(namespace, run_id, e, true));
        }

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
    /// Reopen an existing run, classify it, and close it again.
    ///
    /// The opening sequence is `run`'s, minus everything about launching, with
    /// three differences that are the whole of the reopen contract:
    ///
    /// 1. `OpenRunIntent::OpenExisting` rather than `CreateNew`. No new variant
    ///    was needed: `OpenRunRequest` already carries `run_id`, all four
    ///    backends already implement this intent, and adding a third variant
    ///    would have been a compile error in `umbra-storage-tar`, whose match on
    ///    `intent` is exhaustive with no wildcard.
    /// 2. A journal gate of its own, narrower than `run`'s. `run`'s gate refuses
    ///    *any* non-fresh journal and is correct for what it guards -- a
    ///    `CreateNew` that came back non-fresh is a storage/journal disagreement,
    ///    not recovery. That gate is untouched. This path instead refuses exactly
    ///    what `bind` refuses -- a checkpoint, or a torn tail -- and lets `bind`
    ///    classify the rest, because classifying the rest is what #65 built.
    /// 3. Nothing launches, and no completion record is written. Teardown is
    ///    `fail_run` with `tree_terminated: true`, which is honest here rather
    ///    than pessimistic: no tree was ever started, so there is provably none
    ///    left. It is also the only teardown a *poisoned* session accepts --
    ///    `finish_run` refuses through `idle()`, and a poisoned run must not be
    ///    recorded as having completed cleanly.
    ///
    /// **The writer epoch is not managed here, and deliberately so.** Every
    /// backend already advances it across a reopen, by two different routes:
    /// `umbra-storage-{local,nfs,tar}` read the run's persisted `epoch` file in
    /// `acquire_writer`, `checked_add(1)`, and write it back durably on every
    /// acquisition, so a reopen gets a fresh epoch because acquisition always
    /// does; `umbra-storage-nfs-userspace` mints nothing in `acquire_writer` and
    /// instead derives an `epoch_floor` in `open_run` that is `LeaseEpoch(0)` for
    /// `CreateNew` and the run's own persisted epoch otherwise, then admits at
    /// `floor + 1` and treats a marker below the floor as the epoch regression it
    /// is. Adding a bump here would be a second mechanism racing four existing
    /// ones. The acquired epoch is reported in `ResumeOutcome` instead.
    pub fn resume(spec: ResumeSpec) -> Result<ResumeOutcome> {
        // The same admission `run` performs, by **calling** the same code rather
        // than restating it. Two clauses went missing across two review rounds
        // when this path re-derived them -- the persistence capability, then the
        // namespace-role refusal -- and each time the symptom was that a registry
        // `run` refuses was accepted here. `admit_run` is the one list, and what
        // it deliberately leaves to `validate` (the launch-only capabilities and
        // the `--experimental` acknowledgement) is argued at its own definition.
        admit_run(
            &spec.registry,
            &spec.workspace,
            spec.persistence,
            "resume.validate",
        )?;
        let run_id = spec.run_id;

        // The same fingerprint the run was created under. `OpenExisting`
        // validates it against the run's own manifest, so a workspace that has
        // moved on is refused at the backend rather than silently reopened.
        let inventory = WorkspaceInventory::capture(&spec.workspace)?;
        let immutable_base = inventory.contract();

        let timeout = spec.registry.timeout_ms;
        let mut storage: Box<dyn Storage> = Box::new(umbra_storage::provider::Proxy::connect(
            spec.registry.get("storage")?,
            timeout,
        )?);

        let policy = StoragePolicy {
            read_only: false,
            require_strict_remote_persistence: false,
            require_kernel_shadow: false,
            format_version: 1,
        };
        let binding = storage.open_run(&OpenRunRequest {
            run_id,
            intent: OpenRunIntent::OpenExisting,
            immutable_base,
            policy,
        })?;
        if binding.run_id != run_id {
            return Err(with_cleanup(
                error(
                    ErrorKind::ProtocolMismatch,
                    "resume.open",
                    "storage opened a different run",
                ),
                storage.close_run(),
            ));
        }

        // See `run`: a backend that admitted in `open_run` names the writer, and
        // this reopen acquires under that name rather than one of its own.
        let writer_id = binding
            .admitted_writer
            .clone()
            .unwrap_or_else(|| WriterId(Uuid::new_v4().to_string()));
        let lease = match storage.acquire_writer(&AcquireWriterRequest {
            run_id,
            writer_id: writer_id.clone(),
            // Same rule as a fresh run: expiry is not proof the former writer is
            // gone, so takeover is never offered as a recovery affordance. A run
            // whose previous writer still holds authority is refused here, which
            // is what stops two supervisors reconciling the same tree.
            takeover: TakeoverPolicy::Refuse,
        }) {
            Ok(lease) => lease,
            Err(e) => return Err(with_cleanup(e, storage.close_run())),
        };
        let writer_epoch = lease.epoch;

        let context = RequestContext {
            run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey(format!("resume/{}", run_id.0)),
            writer_epoch: Some(lease.epoch),
        };

        // The reopen half of the conditional journal directory `run` establishes.
        // Its comment states the limit and why the nonce below is not optional;
        // this is the site that enforces it.
        let routed = binding.root.physical_path.is_none();
        let control_path = if routed {
            let state = match host_state(&spec.state_root, run_id, "resume.state") {
                Ok(state) => state,
                Err(e) => {
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    return Err(with_cleanup(e, storage.close_run()));
                }
            };
            // Before the journal is opened, deliberately, and nothing has been
            // created yet either. Opening it as a writer against an empty
            // directory *creates* a fresh log, and a fresh log cannot be told
            // from an intact one afterwards -- so the evidence has to be checked
            // while its absence is still observable, and a reopen that refuses
            // must leave the state root exactly as it found it.
            if let Err(e) = verify_journal_binding(
                storage.as_mut(),
                &context,
                spec.registry.get("journal")?,
                timeout,
                &state,
                run_id,
            ) {
                let e = with_cleanup(e, storage.release_writer(&lease));
                return Err(with_cleanup(e, storage.close_run()));
            }
            match BytePath::new(state.journal.as_os_str().as_bytes().to_vec()) {
                Ok(path) => path,
                Err(e) => {
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    return Err(with_cleanup(e, storage.close_run()));
                }
            }
        } else {
            match binding.control.physical_path.clone() {
                Some(path) => path,
                None => {
                    let e = error(
                        ErrorKind::UnsupportedCapability,
                        "resume.journal",
                        "storage exposes no kernel path for the control directory",
                    );
                    let e = with_cleanup(e, storage.release_writer(&lease));
                    return Err(with_cleanup(e, storage.close_run()));
                }
            }
        };

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

        // The reopen gate, and it is narrower than `run`'s on purpose. A
        // checkpoint belongs to checkpoint-based recovery, which is not
        // implemented; a torn tail may have eaten the very terminal record the
        // classification reads, and `open` has already truncated those bytes. Both
        // are "don't know", and `bind` refuses both for the same reasons. Nothing
        // else is refused here: a nonempty inventory, a non-zero
        // `last_valid_sequence` and an unclean state are precisely what this path
        // exists to classify rather than reject.
        if recovery.checkpoint.is_some() || !matches!(recovery.tail, JournalTailRecovery::Intact) {
            let e = error(
                ErrorKind::UnsupportedCapability,
                "resume.journal",
                "reopening a run with a checkpoint or a torn tail requires M1.5 reconciliation",
            );
            let e = with_cleanup(e, journal.close());
            let e = with_cleanup(e, storage.release_writer(&lease));
            return Err(with_cleanup(e, storage.close_run()));
        }

        let unfinished = recovery.pending.len();
        let last_valid_sequence = recovery.last_valid_sequence;

        let mut namespace = standard_namespace(storage, journal);
        let session = SessionConfig {
            binding,
            context,
            recovery,
            lease,
        };
        let base = Box::new(HostReadOnlyBase::new(inventory));
        if let Err(e) = namespace.bind(session, base) {
            // A refused bind owns nothing yet, and nothing launched.
            return Err(fail(namespace, run_id, e, true));
        }

        // The verdict, read from the bound session rather than re-derived. `bind`
        // returns `Ok` either way and poisons on a recovery-required journal --
        // that is what makes a run in this state openable and inspectable at all,
        // and it is why this is a field on the outcome rather than an `Err`.
        let recovery_required = match namespace.requires_recovery() {
            Ok(verdict) => verdict,
            Err(e) => return Err(fail(namespace, run_id, e, true)),
        };

        // Close it down again. Nothing launched, so `tree_terminated` is true by
        // construction rather than by assumption, and a reopen must not leave a
        // completion record behind on a run it did not complete.
        let closed = namespace.fail_run(&FailedRunRequest {
            run_id,
            reason: if recovery_required {
                "reopened for inspection: run requires recovery".to_owned()
            } else {
                "reopened for inspection: no reconciliation required".to_owned()
            },
            tree_terminated: true,
        });
        // `fail_run` is the teardown, not a verdict. Its own failure is a real
        // error -- the journal or the writer marker did not come down cleanly --
        // and it must not be swallowed just because the classification succeeded.
        closed?;

        Ok(ResumeOutcome {
            run_id,
            recovery_required,
            unfinished,
            last_valid_sequence,
            writer_epoch,
        })
    }

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

    /// Avoid a renewal round-trip on every fast provider boundary.
    const MIN_RENEWAL_MILLIS: u64 = 100;

    fn renewal_interval(lease_millis: u64) -> Duration {
        Duration::from_millis((lease_millis / 2).max(MIN_RENEWAL_MILLIS))
    }

    /// Each provider boundary services renewal; retain timeout headroom even
    /// when the minimum interval raises the usual half-lease schedule.
    fn check_renewal_budget(lease: &umbra_core::WriterLease, timeout_ms: u64) -> Result<()> {
        let interval = renewal_interval(lease.renew_after_millis).as_millis();
        if timeout_ms == 0
            || lease.renew_after_millis / 2 <= timeout_ms
            || interval + u128::from(timeout_ms) >= u128::from(lease.renew_after_millis)
        {
            return Err(error(
                ErrorKind::InvalidState,
                "run.lease",
                format!(
                    "writer lease renews every {} ms; timeout_ms must be positive and \
                     leave renewal headroom with a {MIN_RENEWAL_MILLIS} ms minimum interval \
                     (configured timeout_ms: {timeout_ms})",
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

    #[test]
    fn renewal_floor_preserves_headroom_and_zero_timeouts_are_refused() {
        let mut lease = umbra_core::WriterLease {
            run_id: RunId(Uuid::new_v4()),
            writer_id: WriterId("test".into()),
            epoch: umbra_core::LeaseEpoch(1),
            renewal_token: vec![],
            renew_after_millis: 3,
        };
        assert!(check_renewal_budget(&lease, 0).is_err());
        assert!(check_renewal_budget(&lease, 1).is_err());
        lease.renew_after_millis = 101;
        assert!(check_renewal_budget(&lease, 1).is_err());
        lease.renew_after_millis = 150;
        assert!(check_renewal_budget(&lease, 1).is_ok());
        assert_eq!(renewal_interval(150), Duration::from_millis(100));
        lease.renew_after_millis = 10_000;
        assert!(check_renewal_budget(&lease, 5000).is_err());
        assert!(check_renewal_budget(&lease, 4999).is_ok());
        assert_eq!(renewal_interval(10_000), Duration::from_secs(5));
    }
}

#[cfg(test)]
mod abi_tests {
    use super::*;
    #[test]
    fn architecture_advertisement_must_be_unambiguous() {
        use umbra_core::Architecture::{Aarch64, X86_64};
        assert_eq!(select_architecture(&[Aarch64]).unwrap(), Aarch64);
        for architectures in [vec![], vec![Aarch64, X86_64], vec![X86_64, Aarch64]] {
            assert_eq!(
                select_architecture(&architectures).unwrap_err().kind,
                ErrorKind::UnsupportedCapability
            );
        }
    }
    #[test]
    fn universal_provider_selects_the_negotiated_architecture() {
        let mut names = ["aaa-feature", "darwin-arm64-abi-v1", "darwin-x86_64-abi-v1"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        assert_eq!(
            select_abi(&names, &umbra_core::Architecture::Aarch64).unwrap(),
            "darwin-arm64-abi-v1"
        );
        assert_eq!(
            select_abi(&names, &umbra_core::Architecture::X86_64).unwrap(),
            "darwin-x86_64-abi-v1"
        );
        names.insert("darwin-arm64-abi-v2".into());
        assert!(select_abi(&names, &umbra_core::Architecture::Aarch64).is_err());
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use std::collections::BTreeMap;
    use umbra_core::provider::PROTOCOL_VERSION;

    fn descriptor(id: &str, role: &str, capabilities: &[&str]) -> ProviderDescriptor {
        ProviderDescriptor {
            id: id.into(),
            role: role.into(),
            protocol_version: PROTOCOL_VERSION,
            executable: BytePath::new(b"/usr/bin/true".to_vec()).unwrap(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            options: Vec::new(),
        }
    }

    /// Every role a local-development run needs, each declaring exactly the
    /// capabilities its own check requires. Without a namespace role this spec
    /// validates, which is what makes the namespace refusal below meaningful
    /// rather than an artefact of an incomplete registry.
    fn qualified_providers() -> BTreeMap<String, ProviderDescriptor> {
        let mut providers = BTreeMap::new();
        providers.insert(
            "storage".to_owned(),
            descriptor(
                "storage-local",
                "storage",
                &[
                    caps::STORAGE_LOCAL_DEVELOPMENT_V1,
                    caps::STORAGE_OPEN_REWRITE_V1,
                ],
            ),
        );
        providers.insert(
            "journal".to_owned(),
            descriptor("journal-file", "journal", &[]),
        );
        providers.insert(
            "platform".to_owned(),
            descriptor(
                "platform-host",
                "platform",
                &[
                    caps::PLATFORM_SANDBOXED_LAUNCH_V1,
                    caps::PLATFORM_SYSCALL_REWRITE_V1,
                ],
            ),
        );
        providers
    }

    fn spec_with(providers: BTreeMap<String, ProviderDescriptor>) -> RunSpec {
        RunSpec {
            registry: ProviderRegistry {
                providers,
                timeout_ms: 5_000,
            },
            // Unused by these cases: every one of them is over a backend that
            // binds kernel paths, so nothing consults the host state root.
            state_root: std::path::PathBuf::from("/nonexistent/umbra-state"),
            launch: RunLaunch::Command(CommandLaunch {
                executable: BytePath::new(b"/bin/echo".to_vec()).unwrap(),
                argv: vec![b"echo".to_vec()],
                environment: Vec::new(),
                cwd: BytePath::new(b"/".to_vec()).unwrap(),
            }),
            workspace: std::env::temp_dir(),
            persistence: RunPersistence::LocalDevelopment,
            experimental: true,
            observer: None,
        }
    }

    fn with_namespace(capabilities: &[&str]) -> RunSpec {
        let mut providers = qualified_providers();
        providers.insert(
            "namespace".to_owned(),
            descriptor("alt-namespace", "namespace", capabilities),
        );
        spec_with(providers)
    }

    /// The control for the refusal below: this configuration is otherwise
    /// complete and accepted, so a failure there can only come from the
    /// namespace role.
    #[test]
    fn a_qualified_registry_without_a_namespace_role_validates() {
        validate(&spec_with(qualified_providers())).expect("qualified registry validates");
    }

    /// The guard's whole job: no namespace-role registry may be admitted, whatever
    /// the descriptor declares about itself.
    #[test]
    fn every_namespace_role_registry_is_refused_whatever_it_declares() {
        // Undeclared. The capability gate answers first and names what is missing.
        let missing = validate(&with_namespace(&[])).unwrap_err();
        assert_eq!(missing.kind, ErrorKind::UnsupportedCapability);
        assert_eq!(missing.operation, "run.capabilities");
        assert!(
            missing.context.contains(caps::NAMESPACE_RUN_LIFECYCLE_V1),
            "{missing}"
        );
        assert!(
            missing
                .context
                .contains("owning the run lifecycle (renew, finish and fail)"),
            "{missing}"
        );
        assert!(missing.context.contains("alt-namespace"), "{missing}");
        // The old hardcoded protocol-gap wording is gone.
        assert!(
            !missing.context.contains("are not in its protocol"),
            "{missing}"
        );

        // Declared. `capabilities` is operator-written registry JSON, so the
        // descriptor's claim about itself is not admission: `unix::run` always
        // binds `standard_namespace`, and an admitted descriptor would be
        // silently discarded while the operator believed otherwise.
        let declared = validate(&with_namespace(&[caps::NAMESPACE_RUN_LIFECYCLE_V1])).unwrap_err();
        assert_eq!(declared.kind, ErrorKind::UnsupportedCapability);
        assert_eq!(declared.operation, "run.capabilities");
        assert!(
            declared
                .context
                .contains("does not yet route a run to a configured namespace provider"),
            "{declared}"
        );
        assert!(declared.context.contains("alt-namespace"), "{declared}");

        // Declaring more than asked for is not a way around it either. `kind` and
        // `operation` are the same on both branches of the guard, so this case has
        // to assert the routing message to prove it reached the second one rather
        // than tripping the capability gate on its way past.
        let over_declared = validate(&with_namespace(&[
            caps::NAMESPACE_RUN_LIFECYCLE_V1,
            caps::STORAGE_LOCAL_DEVELOPMENT_V1,
            caps::PLATFORM_SANDBOXED_LAUNCH_V1,
        ]))
        .unwrap_err();
        assert_eq!(over_declared.kind, ErrorKind::UnsupportedCapability);
        assert_eq!(over_declared.operation, "run.capabilities");
        assert!(
            over_declared
                .context
                .contains("does not yet route a run to a configured namespace provider"),
            "{over_declared}"
        );
    }
}
