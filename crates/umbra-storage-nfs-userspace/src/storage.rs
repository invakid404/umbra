//! The provider: configuration, product admission and the synchronous
//! [`Storage`] surface.
//!
//! A method whose semantics this provider will never offer answers
//! [`ErrorKind::UnsupportedCapability`]; a method that is authorised but not yet
//! implemented answers [`ErrorKind::NotImplemented`] naming the node that owns it.
//! Nothing here reports a success it did not perform.
//!
//! # Admission comes first
//!
//! [`NfsUserspaceStorage::open_run`] establishes a client incarnation, resolves
//! the run's anchors, and then acquires product admission through
//! [`session::Session`](crate::session::Session) **before** it publishes a
//! [`RunBinding`]. A denied session gets an error and no binding, so
//! one-session-one-Umbra is a property of the type flow rather than a rule a
//! caller is asked to respect. `close_run` releases cooperatively; a release that
//! cannot be proven clean keeps the marker held rather than freeing the run over
//! unsettled I/O.

use serde::{Deserialize, Serialize};
use umbra_core::provider::decode;
use umbra_core::{
    BytePath, Durability, ErrorKind, Fencing, FlushRequest, OpenRunRequest, RequestContext, Result,
    RunBinding, StorageAnchor, StorageCapabilities, StorageOperation, StoragePath, StorageRequest,
    StorageResponse, UmbraError, WriterLease,
};
use umbra_storage::{AcquireWriterRequest, DurabilityReceipt, Storage};

use crate::authority::admission::OutstandingIo;
use crate::authority::marker::WriterToken;
use crate::ops::{Operations, OpsContext};
use crate::replay::ReplayLog;
use crate::session::Session;
use crate::state::ProtocolState;
use crate::transport::{Deadline, RawTransport, Verifier, WireProfile};

/// Provider id under which this backend registers. Distinct from the mounted
/// `nfs` adapter, which stays the qualified fallback.
pub const PROVIDER_ID: &str = "nfs-userspace";

/// Storage format version this provider reads and writes.
///
/// Matched to the mounted adapter so an existing run created by `nfs` can be
/// opened sequentially by `nfs-userspace`. Concurrent access by both providers is
/// not in scope: one-session-one-Umbra admission is product-wide.
pub const FORMAT_VERSION: u32 = 1;

/// Server endpoint and run layout for one userspace NFSv4.0 session.
///
/// There is no mount root and no host path: this provider talks to the server
/// itself, so it exposes opaque handles and no `physical_path`. That also means
/// it cannot be selected by `umbra run`, which needs a kernel-visible run root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NfsUserspaceConfig {
    /// Server host as bytes, exactly as configured. No name resolution policy is
    /// implied here.
    pub host: Vec<u8>,
    /// TCP port. NFS conventionally uses 2049; nothing here assumes it.
    pub port: u16,
    /// Export path on the server, relative to the export root filehandle.
    pub export: BytePath,
    /// Export-relative parent containing run-id directories.
    pub run_parent: BytePath,
    /// Single component naming the tracee-visible anchor within each run.
    pub root_anchor: BytePath,
    /// Single component naming the control anchor within each run.
    pub control_anchor: BytePath,
    /// Deadline applied to each COMPOUND submission.
    pub deadline: Deadline,
}

impl NfsUserspaceConfig {
    /// Decode shipped provider options: the JSON encoding of this struct.
    pub fn from_options(options: &[u8]) -> Result<Self> {
        let config: Self = decode(options)?;
        config.validate()?;
        Ok(config)
    }

    /// Reject a configuration that could not name a containable run.
    pub fn validate(&self) -> Result<()> {
        if self.host.is_empty() || self.host.contains(&0) {
            return Err(config_error("host must be nonempty bytes without NUL"));
        }
        if self.port == 0 {
            return Err(config_error("port must be nonzero"));
        }
        // Export and run parent name components below the server's pseudo-root.
        // Reusing the contract's own path rule rejects an absolute path, a dot or
        // parent component and an empty component, so neither can escape the
        // export or name a host path this provider does not have.
        for relative in [&self.export, &self.run_parent] {
            StoragePath::new(StorageAnchor::Root, relative.as_bytes().to_vec())?;
        }
        for anchor in [&self.root_anchor, &self.control_anchor] {
            let bytes = anchor.as_bytes();
            if bytes.is_empty()
                || bytes.contains(&b'/')
                || bytes == b"."
                || bytes == b".."
                || bytes == b".provider"
            {
                return Err(config_error(
                    "anchor must be a single nonreserved component",
                ));
            }
        }
        if self.root_anchor == self.control_anchor {
            return Err(config_error("anchors must be disjoint"));
        }
        if self.deadline.millis == 0 || self.deadline.millis > 60_000 {
            return Err(config_error("deadline must be in 1..=60000 milliseconds"));
        }
        Ok(())
    }
}

fn config_error(context: &str) -> UmbraError {
    UmbraError::new(ErrorKind::InvalidPath, "config", context)
}

/// The node that owns wiring a method, named in its gate error so a reader knows
/// where the work lands rather than seeing a bare "not implemented".
fn gated<T>(operation: &str, owner: &str) -> Result<T> {
    Err(UmbraError::new(
        ErrorKind::NotImplemented,
        operation,
        format!("M1 gate: interfaces are frozen; {owner} wires this method"),
    ))
}

fn unsupported<T>(operation: &str, context: &str) -> Result<T> {
    Err(UmbraError::new(
        ErrorKind::UnsupportedCapability,
        operation,
        context,
    ))
}

/// Userspace NFSv4.0 storage backend.
///
/// Holds the facade seams rather than a connection. `connect` builds an unbound
/// provider whose contract methods report the M1 gate; [`NfsUserspaceStorage::with_facades`]
/// injects a transport and replay log, which is how `raw_state` and
/// `authority_recovery` drive the same code against the fake.
pub struct NfsUserspaceStorage {
    config: NfsUserspaceConfig,
    transport: Option<Box<dyn RawTransport>>,
    replay: Option<Box<dyn ReplayLog>>,
    operations: Option<Operations>,
    /// Umbra-owned NFSv4.0 client state: client id, open owners, seqids, lease.
    state: ProtocolState,
    /// Puts namespace mutations on the wire. Held as a field so a request can
    /// borrow it alongside the transport instead of creating one whose lifetime
    /// would end inside the call that built the context.
    dispatcher: crate::namespace::dispatch::TransportDispatcher,
    /// This provider session's writer identity.
    ///
    /// `OpenRunRequest` names no writer, so the session is the writer: admission
    /// is per Umbra process, which is what one-session-one-Umbra means. A second
    /// process gets a different id and is denied by the marker rather than
    /// sharing this one's authority.
    writer_id: umbra_core::WriterId,
    /// Product admission for the open run. `Some` exactly while a run is open,
    /// because a run that opened without admission is unrepresentable.
    session: Option<Session>,
    /// Serial of the next run session, folded into every issued
    /// [`StorageHandle`](umbra_core::StorageHandle) so a token from a closed run
    /// cannot be replayed against the next one.
    serial: u64,
    /// Why this provider may no longer act on the open run, once that is settled.
    ///
    /// **R1-002.** Losing writer authority is sticky. A failed renewal, a release
    /// whose effect could not be established, or a completed handover all mean a
    /// successor may already be live, and none of them may be retried into
    /// authority by the next call that happens to find `session` populated.
    authority_loss: Option<AuthorityLoss>,
    /// The recovery that could not be settled, once one has been refused.
    ///
    /// **R3-001.** `docs/design/failure-model.md`'s BLOCKED_RECOVERABLE is a
    /// *state*, not an error return: "stop new mutations and quiesce", retain the
    /// marker, the replay data and the diagnostic, and resume only after the
    /// recovery conditions change. Returning a blocked error and then accepting
    /// the next mutation — or publishing a clean release — is none of that.
    recovery_blocked: Option<UmbraError>,
    /// The original failure of a write or barrier that did not complete.
    ///
    /// **R2-003.** `docs/design/failure-model.md`'s "Server EIO / failed stable
    /// write" row: latch the original failure, and do not issue a clean receipt
    /// or release afterwards. Held verbatim, never replaced by a later error, so
    /// a caller reads the status the server actually returned.
    stable_write_failure: Option<UmbraError>,
    /// Calls this provider issued whose server-side effect is not settled.
    ///
    /// **R1-002.** `close_run` used to assert [`OutstandingIo::Excluded`] purely
    /// because `submit` had returned. Withdrawing a local registration does not
    /// prove the server never received the request, so the evidence is collected
    /// here as it happens and the release reports what it actually knows.
    unsettled: Vec<String>,
}

/// Why a provider stopped being able to act on its open run.
///
/// Every variant is terminal for the run: the only way out is `close_run`.
#[derive(Clone, Debug)]
pub enum AuthorityLoss {
    /// Renewal found the marker no longer records this session.
    RenewalFailed(UmbraError),
    /// A release neither provably happened nor provably did not.
    UncertainRelease(UmbraError),
    /// Admission was cooperatively released. The run is no longer this session's.
    Handed,
}

impl AuthorityLoss {
    /// The error a call made after this loss must report.
    fn refuse(&self, operation: &str) -> UmbraError {
        match self {
            Self::RenewalFailed(error) => UmbraError::new(
                ErrorKind::LeaseLost,
                operation,
                format!(
                    "this provider lost writer authority and will not act on the run again \
                     until it is reopened: {error}"
                ),
            ),
            Self::UncertainRelease(error) => UmbraError::new(
                ErrorKind::LeaseLost,
                operation,
                format!(
                    "this provider's admission release could not be proven either way, so a \
                     successor may already hold the run: {error}"
                ),
            ),
            Self::Handed => UmbraError::new(
                ErrorKind::LeaseLost,
                operation,
                "this provider released admission for the run; it holds no authority to act \
                 on it, for reads or for mutations",
            ),
        }
    }
}

impl std::fmt::Debug for NfsUserspaceStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NfsUserspaceStorage")
            .field("config", &self.config)
            .field("transport_bound", &self.transport.is_some())
            .field("replay_bound", &self.replay.is_some())
            .field("run_open", &self.operations.is_some())
            .field("admitted", &self.session.is_some())
            .finish()
    }
}

impl NfsUserspaceStorage {
    /// Validate configuration without opening a connection.
    ///
    /// No transport is bound here. Binding a live NFS transport is `raw_rpc`'s
    /// work and is not authorised at this gate.
    pub fn connect(config: NfsUserspaceConfig) -> Result<Self> {
        config.validate()?;
        let instance = NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = ProtocolState::new(client_identity(&config, instance));
        let config_for_writer = config.clone();
        Ok(Self {
            config,
            transport: None,
            replay: None,
            operations: None,
            state,
            dispatcher: crate::namespace::dispatch::TransportDispatcher::new(),
            writer_id: writer_id(&config_for_writer, instance),
            session: None,
            serial: 0,
            authority_loss: None,
            recovery_blocked: None,
            stable_write_failure: None,
            unsettled: Vec::new(),
        })
    }

    /// Build a provider over supplied facades, real or fake.
    ///
    /// This is the seam that lets downstream nodes develop in parallel: the
    /// consumer code is identical whichever implementation is passed.
    pub fn with_facades(
        config: NfsUserspaceConfig,
        transport: Box<dyn RawTransport>,
        replay: Box<dyn ReplayLog>,
    ) -> Result<Self> {
        config.validate()?;
        transport
            .wire_profile()
            .check()
            .map_err(|error| crate::error::FacadeError::Transport(error).to_umbra("connect"))?;
        let instance = NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = ProtocolState::new(client_identity(&config, instance));
        let config_for_writer = config.clone();
        Ok(Self {
            config,
            transport: Some(transport),
            replay: Some(replay),
            operations: None,
            state,
            dispatcher: crate::namespace::dispatch::TransportDispatcher::new(),
            writer_id: writer_id(&config_for_writer, instance),
            session: None,
            serial: 0,
            authority_loss: None,
            recovery_blocked: None,
            stable_write_failure: None,
            unsettled: Vec::new(),
        })
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &NfsUserspaceConfig {
        &self.config
    }

    /// The wire profile in use. Always NFSv4.0 / TCP / AUTH_SYS.
    pub fn wire_profile(&self) -> WireProfile {
        self.transport
            .as_ref()
            .map_or(WireProfile::V40_TCP_SYS, |transport| {
                transport.wire_profile()
            })
    }

    /// Borrow the transport facade, or report that none is bound.
    pub fn transport(&mut self) -> Result<&mut dyn RawTransport> {
        match self.transport.as_deref_mut() {
            Some(transport) => Ok(transport),
            None => Err(UmbraError::new(
                ErrorKind::StorageUnavailable,
                "transport",
                "no transport facade is bound to this provider",
            )),
        }
    }

    /// Borrow the replay facade, or report that none is bound.
    pub fn replay(&mut self) -> Result<&mut dyn ReplayLog> {
        match self.replay.as_deref_mut() {
            Some(replay) => Ok(replay),
            None => Err(UmbraError::new(
                ErrorKind::StorageUnavailable,
                "replay",
                "no replay facade is bound to this provider",
            )),
        }
    }

    /// The operations surface of the currently open run, if one is open.
    pub fn operations(&self) -> Option<&Operations> {
        self.operations.as_ref()
    }

    /// The Umbra-owned protocol state: client id, open owners, seqids, lease.
    pub fn state(&mut self) -> &mut ProtocolState {
        &mut self.state
    }

    /// Product admission for the open run, when one is open.
    ///
    /// `None` whenever no run is open. There is no state in which a run is open
    /// and this is `None`: `open_run` publishes a binding only after admission is
    /// granted, and `close_run` drops both together.
    pub fn admission(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Borrow the three pieces one request needs, or report what is missing.
    ///
    /// Taken together rather than through three accessors because a request needs
    /// all three at once and each accessor would borrow the whole provider.
    /// `mutations` is always `None` here: a mutation needs open owners from a
    /// confirmed client incarnation, and establishing one is `authority_recovery`'s
    /// work, not this method's.
    fn request(
        &mut self,
        operation: &str,
        key: Option<&umbra_core::IdempotencyKey>,
        operation_id: umbra_core::OperationId,
    ) -> Result<(&Operations, OpsContext<'_>)> {
        // R1-002: a latched authority loss is checked before anything is
        // borrowed, so no call after the loss can reach a dispatch path.
        if let Some(loss) = &self.authority_loss {
            return Err(loss.refuse(operation));
        }
        let deadline = self.config.deadline;
        let operations = self.operations.as_ref().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no run is open on this provider",
            )
        })?;
        let transport = self.transport.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "no transport facade is bound to this provider",
            )
        })?;
        let replay = self.replay.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "no replay facade is bound to this provider",
            )
        })?;
        // Writer authority is exactly the admission `open_run` acquired. Without
        // it there is no mutation context at all, so the operations surface
        // reports the authority gate rather than dispatching.
        let create_verifier = key.map(|key| {
            self.state
                .create_verifiers()
                .verifier_for(key, operation_id)
        });
        let mutations = self.session.as_ref().and_then(|_| {
            self.state
                .incarnation()
                .map(|incarnation| crate::ops::MutationContext {
                    owners: incarnation.open_owners(),
                    create_verifier,
                    namespace: Some(
                        &mut self.dispatcher as &mut dyn crate::namespace::NamespaceDispatcher,
                    ),
                })
        });
        Ok((
            operations,
            OpsContext {
                transport,
                replay,
                mutations,
                deadline,
            },
        ))
    }

    /// Borrow the three pieces an authority call needs.
    ///
    /// Returned together for the same reason as [`Self::request`]: each has to
    /// come out of one `&mut self` without the others being borrowed twice.
    #[allow(clippy::type_complexity)]
    fn authority(
        &mut self,
        operation: &str,
    ) -> Result<(
        &Session,
        &Operations,
        &mut dyn RawTransport,
        &mut ProtocolState,
    )> {
        if let Some(loss) = &self.authority_loss {
            return Err(loss.refuse(operation));
        }
        let session = self.session.as_ref().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no admission is held; open a run first",
            )
        })?;
        let operations = self.operations.as_ref().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no run is open on this provider",
            )
        })?;
        let transport = self.transport.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "no transport facade is bound to this provider",
            )
        })?;
        Ok((session, operations, transport, &mut self.state))
    }

    /// Release the held admission, keeping it on a release that is not clean.
    fn release(&mut self, operation: &str, deadline: Deadline) -> Result<()> {
        // R2-001: a provider that has already lost authority must not publish a
        // release built from its stale proof. `Session::release` overwrites the
        // marker, so doing it here could stamp `Released` at this session's old
        // epoch over evidence that has since changed — destroying exactly the
        // record the failure model requires be preserved and refused. The proof is
        // consumed instead: a session that cannot prove it holds the run holds
        // nothing, and the marker is left exactly as it was found.
        if let Some(loss) = &self.authority_loss {
            let refusal = loss.refuse(operation);
            self.session = None;
            return Err(refusal);
        }
        let Some(session) = self.session.take() else {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no admission is held",
            ));
        };
        let Some(operations) = self.operations.as_ref() else {
            // The run is gone, so the anchors the marker is addressed through are
            // gone too. Putting the session back is the only safe answer: the
            // marker stays held rather than being abandoned in an unknown phase.
            self.session = Some(session);
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no run is open, so the admission marker cannot be addressed",
            ));
        };
        let Some(transport) = self.transport.as_deref_mut() else {
            self.session = Some(session);
            return Err(UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "no transport facade is bound to this provider",
            ));
        };
        // R1-002: the disposition is derived from what this provider actually
        // observed, not from the fact that `submit` returned. One COMPOUND per
        // call and one awaited reply rules out *pending* work, but a call whose
        // retirement was never proven drained, or that died with the connection,
        // may still have been received and acted on by the server.
        let outstanding = match self.unsettled.first() {
            None => OutstandingIo::Excluded,
            Some(first) => OutstandingIo::Unknown {
                detail: format!(
                    "{} call(s) issued by this session have unproven server-side disposition;                      first: {first}",
                    self.unsettled.len()
                ),
            },
        };
        match session.release(
            transport,
            &mut self.state,
            operations.anchors(),
            outstanding,
            operation,
            deadline,
        ) {
            crate::session::SessionRelease::Released(_) => {
                // Admission is gone. The run surface stays addressable only so
                // `close_run` can tear it down; every contract call is refused.
                self.authority_loss = Some(AuthorityLoss::Handed);
                Ok(())
            }
            crate::session::SessionRelease::Retained { session, error } => {
                self.session = Some(session);
                Err(error)
            }
            // The proof was consumed on purpose. Putting a session back here is
            // exactly the revival R1-002 records as unsafe.
            crate::session::SessionRelease::Uncertain(error) => {
                self.authority_loss = Some(AuthorityLoss::UncertainRelease(error.clone()));
                Err(error)
            }
        }
    }

    /// Consult the durable retry journal for `request` (**R1-004**).
    /// Consult the journal for this key, observing nothing (**R3-003**).
    fn journal_lookup(
        &mut self,
        operation: &str,
        request: &StorageRequest,
    ) -> Result<crate::journal::Lookup> {
        let deadline = self.config.deadline;
        let (transport, _, private) = self.journal_scope(operation)?;
        crate::journal::lookup(transport, &private, request, operation, deadline)
    }

    /// Observe the state a fresh or interrupted mutation acts on.
    fn observe_now(
        &mut self,
        operation: &str,
        request: &StorageRequest,
    ) -> Result<crate::ops::Observation> {
        let (operations, mut context) = self.request(
            operation,
            Some(&request.context.idempotency_key),
            request.context.operation_id,
        )?;
        operations.observe(&mut context, request)
    }

    /// Record a fresh intent with its preconditions (**R2-003**).
    fn journal_admit_fresh(
        &mut self,
        operation: &str,
        request: &StorageRequest,
        preconditions: &crate::journal::Preconditions,
    ) -> Result<()> {
        let deadline = self.config.deadline;
        let (transport, owners, private) = self.journal_scope(operation)?;
        crate::journal::admit_fresh(
            transport,
            owners,
            &private,
            request,
            preconditions,
            operation,
            deadline,
        )
    }

    /// Settle an interrupted intent against the state observed now (**R2-003**).
    fn journal_recover(
        &mut self,
        operation: &str,
        request: &StorageRequest,
        recorded: &crate::journal::Preconditions,
        observed: &crate::ops::Observation,
    ) -> Result<crate::journal::Admission> {
        let deadline = self.config.deadline;
        let (transport, owners, private) = self.journal_scope(operation)?;
        crate::journal::recover_interrupted(
            transport, owners, &private, request, recorded, observed, operation, deadline,
        )
    }

    /// Record what a dispatched mutation actually did (**R1-004**).
    fn journal_settle(
        &mut self,
        operation: &str,
        request: &StorageRequest,
        outcome: &Result<StorageResponse>,
    ) -> Result<()> {
        let deadline = self.config.deadline;
        let (transport, owners, private) = self.journal_scope(operation)?;
        crate::journal::settle(
            transport, owners, &private, request, outcome, operation, deadline,
        )
    }

    /// The three pieces the retry journal needs, borrowed together.
    ///
    /// The private anchor is cloned rather than borrowed because `self.operations`
    /// and `self.transport` cannot both be borrowed out of one `&mut self`
    /// otherwise; an `Anchor` is a pinned handle and a run identity, so the clone
    /// names the same object the borrow would have.
    fn journal_scope(
        &mut self,
        operation: &str,
    ) -> Result<(
        &mut dyn RawTransport,
        &mut crate::state::open_owner::OpenOwnerRegistry,
        crate::anchor::Anchor,
    )> {
        let private = self
            .operations
            .as_ref()
            .and_then(|operations| operations.anchors().private())
            .ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    operation,
                    "the run has no .provider directory, so no retry record can be written; a                      mutation without durable replay evidence is refused",
                )
            })?
            .clone();
        let transport = self.transport.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                operation,
                "no transport facade is bound to this provider",
            )
        })?;
        let owners = self
            .state
            .incarnation()
            .ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    operation,
                    "a retry record needs open owners from a confirmed client incarnation",
                )
            })?
            .open_owners();
        Ok((transport, owners, private))
    }

    /// Everything that must hold before a request may have any effect at all.
    ///
    /// **R2-001 / R2-002.** Ordered so the broadest refusal comes first: a
    /// provider that has lost authority answers for nothing, then the run must be
    /// open, then the request must pass the surface's own rules (bounds, run
    /// binding, read-only policy, capability support), and only then is the
    /// presented writer epoch compared against the admitted one.
    ///
    /// Nothing here writes, and nothing here is reachable after the journal has
    /// run. That is the property both findings turn on.
    fn preflight(&self, operation: &str, request: &StorageRequest) -> Result<()> {
        // R2-001: the latch is consulted before the journal, not after it. A
        // provider whose renewal failed must not read a cached answer out of the
        // journal, and must not create index or intent records on the way to
        // discovering it has no authority.
        if let Some(loss) = &self.authority_loss {
            return Err(loss.refuse(operation));
        }
        // R3-001: a blocked recovery is terminal for this run. The original
        // diagnostic is reported verbatim, because the failure model requires the
        // evidence and the diagnosis to be retained, and a fresh generic error
        // would lose the reason the run stopped.
        if request.operation.is_mutation() {
            if let Some(retained) = &self.recovery_blocked {
                return Err(UmbraError::new(
                    ErrorKind::InvalidState,
                    operation,
                    format!(
                        "this run is stopped: an interrupted operation could not be settled \
                         from evidence, and it stays stopped until an operator resolves it. \
                         The original diagnosis was: {retained}"
                    ),
                ));
            }
        }
        // R2-003: a latched write failure stops further mutations. The original
        // error is reported rather than a fresh one, because the failure model
        // requires the first failure to survive: "a later successful flush cannot
        // erase lost-write evidence".
        if request.operation.is_mutation() {
            if let Some(retained) = &self.stable_write_failure {
                return Err(UmbraError::new(
                    ErrorKind::Io,
                    operation,
                    format!(
                        "this run latched a failed write and stops mutating until it is \
                         reopened; the original failure was: {retained}"
                    ),
                ));
            }
        }
        let operations = self.operations.as_ref().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "no run is open on this provider",
            )
        })?;
        // R2-002: the surface's own rules, run here rather than after the journal.
        operations.preflight(request)?;
        // R1-003: the admitted epoch lives on the session, so the check that a
        // request presents the epoch this provider actually holds belongs here.
        // Neither `umbra_storage::validate_request` (which only asks that *some*
        // epoch is present) nor `MutationIdentity::from_context` (which accepts
        // whatever it is handed) compares it against the admission.
        if request.operation.is_mutation() {
            self.check_presented_epoch(operation, &request.context)?;
        }
        Ok(())
    }

    /// Reject a mutation whose writer epoch is not the admitted one (**R1-003**).
    ///
    /// A stale epoch is a request authorised by an admission this provider no
    /// longer holds; a forged one is a request authorised by an admission that
    /// never existed. Neither may reach a dispatch path.
    fn check_presented_epoch(&self, operation: &str, context: &RequestContext) -> Result<()> {
        let Some(session) = self.session.as_ref() else {
            return Err(UmbraError::new(
                ErrorKind::LeaseLost,
                operation,
                "no admission is held; a mutation cannot be authorised",
            ));
        };
        let current = session.admitted().epoch();
        match context.writer_epoch {
            Some(presented) if presented == current => Ok(()),
            Some(presented) => Err(crate::error::FacadeError::Authority(
                crate::error::AuthorityError::StaleEpoch {
                    held: presented,
                    current,
                },
            )
            .to_umbra(operation)),
            None => Err(crate::error::FacadeError::Authority(
                crate::error::AuthorityError::NoWriterEpoch,
            )
            .to_umbra(operation)),
        }
    }

    /// Note a failed request: its disposition, and the write latch it may trip.
    ///
    /// **R2-003.** Called at every stage a mutation can fail — observing the
    /// preconditions, the journal's own round trips, and the dispatch itself —
    /// because an `NFS4ERR_IO` during any of them leaves a write that did not
    /// complete, which is the failure model's "Server EIO / failed stable write"
    /// row whichever call reported it.
    fn note_failure(&mut self, operation: &str, request: &StorageRequest, error: &UmbraError) {
        self.observe_disposition(operation, error);
        // R3-001: a blocked recovery becomes a provider state, and it counts as
        // unsettled work so the release that follows cannot be reported clean.
        // An interrupted intent that could not be settled is precisely "uncertain
        // old I/O" — handing the run on over it is what the failure model forbids.
        if crate::journal::is_blocked(error) && self.recovery_blocked.is_none() {
            self.recovery_blocked = Some(error.clone());
            self.unsettled.push(format!(
                "{operation}: unsettled recovery: {}",
                error.context
            ));
        }
        // R2-003, widened by R3-001: any journalled mutation whose I/O failed —
        // not only a `WriteAt` — latches. A FILE_SYNC journal write for a Rename
        // or Create that fails leaves exactly the same unresolved record, and the
        // old request-kind filter let those through.
        if request.operation.is_mutation()
            && error.kind == ErrorKind::Io
            && self.stable_write_failure.is_none()
        {
            self.stable_write_failure = Some(error.clone());
            self.unsettled.push(format!(
                "{operation}: latched write failure: {}",
                error.context
            ));
        }
    }

    /// Record a call whose server-side effect this provider cannot rule out.
    ///
    /// **R1-002.** Only failures that leave the request possibly *received* count.
    /// A refusal before dispatch (`QueueFull`), a malformed reply, or a deadline
    /// whose retirement was proven drained leave nothing outstanding.
    fn observe_disposition(&mut self, operation: &str, error: &UmbraError) {
        let Some(detail) = unsettled_detail(operation, error) else {
            return;
        };
        self.unsettled.push(detail);
    }
}

/// Whether an error leaves a dispatched request's server-side effect unknown.
///
/// Every transport failure in this crate surfaces as
/// [`ErrorKind::StorageUnavailable`] (`FacadeError::to_umbra`), which covers a
/// lost connection, an elapsed deadline and a cancelled call — each of which can
/// leave a request already received and applied by the server.
///
/// A failure that carries an NFS status is *not* outstanding: the server answered,
/// so the outcome is known even though it is an error. A refusal before dispatch
/// is not outstanding either, but it arrives as `StorageUnavailable` too, so it is
/// counted here. That direction is deliberate: over-reporting keeps the marker
/// held and only costs a cooperative handover, while under-reporting is the
/// two-live-owners failure R1-002 describes.
fn unsettled_detail(operation: &str, error: &UmbraError) -> Option<String> {
    match error.kind {
        ErrorKind::StorageUnavailable => Some(format!("{operation}: {}", error.context)),
        _ => None,
    }
}

/// A discriminator making each provider instance its own NFSv4 client.
///
/// RFC 7530 §9.1.1 identifies a client by its id *string*. Two providers sharing
/// one string are one client to the server, so the second `SETCLIENTID_CONFIRM`
/// with a different verifier is answered `NFS4ERR_CLID_INUSE` — the server
/// correctly refusing to let one client silently displace another's state. Two
/// providers are two clients, so they get two strings.
static NEXT_INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The client identity this provider instance presents in SETCLIENTID.
///
/// Stable for the instance's lifetime, which is what a reconnect needs: a v4.0
/// `CLAIM_PREVIOUS` reclaim inside the server's grace window works only if the
/// reconnecting client presents the same id string it held before.
///
/// It is deliberately *not* stable across processes. The boot verifier changes on
/// restart anyway, so a restarted process is a new incarnation that cannot prove
/// it still owns the old state — and `authority_recovery` denies a restarted
/// process the admission marker for the same reason.
fn client_identity(
    config: &NfsUserspaceConfig,
    instance: u64,
) -> crate::state::client_id::ClientIdentity {
    let label = format!(
        "umbra:{}:{}:{}:{}:{}:{instance}",
        String::from_utf8_lossy(&config.host),
        config.port,
        String::from_utf8_lossy(config.export.as_bytes()),
        String::from_utf8_lossy(config.run_parent.as_bytes()),
        std::process::id(),
    );
    crate::integration::identity_for(&label, boot_verifier())
}

/// A per-process boot verifier.
///
/// Uniqueness matters more than unpredictability: its whole job is to tell the
/// server that this incarnation is not the previous one. Two incarnations that
/// shared a verifier would be indistinguishable to the server's client table.
fn boot_verifier() -> Verifier {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let pid = u64::from(std::process::id());
    Verifier((nanos ^ pid.rotate_left(32)).to_be_bytes())
}

/// The token this session records in the run's admission marker.
///
/// Derived from the run and writer the caller named, so the same session
/// re-presenting itself produces the same token. It is an identity, never a
/// capability: `authority_recovery` denies a held marker *even to the same
/// token*, so possessing this grants nothing on its own.
fn writer_token(run: umbra_core::RunId, writer: &umbra_core::WriterId) -> WriterToken {
    let mut bytes = *run.0.as_bytes();
    for (slot, byte) in bytes
        .iter_mut()
        .zip(writer.0.as_bytes().iter().cycle().take(16))
    {
        *slot ^= byte;
    }
    WriterToken(bytes)
}

/// This provider session's writer identity.
///
/// Endpoint and layout so two providers pointed at different runs are different
/// writers, plus the process id so two processes against the *same* run are too
/// — which is the case admission exists to deny.
fn writer_id(config: &NfsUserspaceConfig, instance: u64) -> umbra_core::WriterId {
    umbra_core::WriterId(format!(
        "{PROVIDER_ID}:{}:{}:{}:{}:{instance}",
        String::from_utf8_lossy(&config.host),
        config.port,
        String::from_utf8_lossy(config.run_parent.as_bytes()),
        std::process::id(),
    ))
}

/// Milliseconds since the epoch, for the lease clock.
///
/// The lease clock measures this session's own idleness so it can renew before
/// the server would drop it. It never decides admission: nothing in
/// [`Session`](crate::session::Session) reads a clock.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl Storage for NfsUserspaceStorage {
    /// Only what an open run can actually meet.
    ///
    /// `Durability::None` and `Fencing::ReadOnly` are the honest floor and stay
    /// there: this provider has qualified no persistence boundary and has no
    /// independent termination verifier, so it must not claim either, and neither
    /// the WRITE/COMMIT verifier flow nor a live NFS lease changes that.
    ///
    /// The finite I/O and page limits are zero until a run is open, because
    /// without a bound transport there is no bound to honour. Advertising a limit
    /// the provider could not meet would fail a caller's request after it had
    /// already been shaped around the claim.
    fn capabilities(&self) -> StorageCapabilities {
        match &self.operations {
            Some(operations) => operations.capabilities(),
            None => StorageCapabilities {
                features: Default::default(),
                durability: Durability::None,
                strict_remote_persistence: false,
                fencing: Fencing::ReadOnly,
                // No kernel-visible path exists: this is a userspace client.
                kernel_shadow: false,
                complete_emulation: false,
                hard_links: false,
                logical_symlinks: false,
                xattrs: false,
                atomic_replace: false,
                atomic_swap: false,
                max_io_bytes: 0,
                max_directory_entries: 0,
            },
        }
    }

    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        if self.operations.is_some() {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "open_run",
                "a run is already open; close it before opening another",
            ));
        }
        let deadline = self.config.deadline;
        let serial = self.serial;
        let config = self.config.clone();
        // A newly admitted run starts with no lost authority and nothing
        // outstanding. `close_run` clears both as well; doing it here too means a
        // run that is opened after a failed close cannot inherit the old verdict.
        self.authority_loss = None;
        self.recovery_blocked = None;
        self.stable_write_failure = None;
        self.unsettled.clear();
        let transport = self.transport.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                "open_run",
                "no transport facade is bound to this provider",
            )
        })?;

        // A confirmed client incarnation first: the admission marker's opens are
        // sequenced through this session's open owners, so admission cannot be
        // taken by a client the server has not confirmed.
        if self.state.incarnation().is_none() {
            let root = transport
                .root_filehandle(deadline)
                .map_err(|error| error.to_umbra("open_run"))?;
            let outcome = self
                .state
                .establish(transport, &root, now_millis(), deadline);
            match outcome.error().cloned() {
                None => {
                    let incarnation = outcome
                        .established()
                        .expect("an outcome carrying no error is established");
                    self.state.adopt(incarnation);
                }
                Some(error) => return Err(error.to_umbra("open_run")),
            }
        }

        // Failure must not publish a partially usable binding, so the surface is
        // built completely before anything is stored on the provider.
        let operations = Operations::open(transport, &config, request, serial, deadline)?;

        // A newly created run gets the mounted adapter's `.provider` state, so
        // the run this provider writes is one that adapter can later open.
        if request.intent == umbra_core::OpenRunIntent::CreateNew {
            let private = operations.anchors().private().ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    "open_run",
                    "a created run must have a .provider directory",
                )
            })?;
            let owners = self
                .state
                .incarnation()
                .ok_or_else(|| {
                    UmbraError::new(
                        ErrorKind::InvalidState,
                        "open_run",
                        "creating run state needs open owners from a confirmed incarnation",
                    )
                })?
                .open_owners();
            crate::anchor::provision_private_state(
                transport,
                owners,
                private,
                request.run_id,
                &request.immutable_base,
                FORMAT_VERSION,
                deadline,
            )?;
        }

        // R1-005: an existing run's own durable evidence is read and validated
        // before admission. `OpenExisting` used to check the caller's format
        // version and nothing else, so a run whose manifest named another base,
        // another run or another format was admitted, and a missing or malformed
        // manifest was indistinguishable from a healthy one.
        //
        // The epoch floor comes from the same read. A run the mounted adapter
        // released cleanly has no marker but may have reached epoch 7; creating a
        // fresh marker at epoch 1 there would regress the run's authority epoch.
        let epoch_floor = if request.intent == umbra_core::OpenRunIntent::CreateNew {
            umbra_core::LeaseEpoch(0)
        } else {
            let private = operations.anchors().private().ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    "open_run",
                    "the run has no .provider directory, so its recorded identity cannot be                      read; opening without one would be an unverified session",
                )
            })?;
            crate::anchor::read_persisted_state(
                transport,
                private,
                request.run_id,
                &request.immutable_base,
                FORMAT_VERSION,
                deadline,
            )?
            .epoch
        };

        // Product admission, before the binding exists. A denial returns here and
        // leaves `self.operations` untouched, so the caller has nothing to use.
        let writer = self.writer_id.clone();
        let session = Session::admit(
            transport,
            &mut self.state,
            operations.anchors(),
            crate::session::Claim {
                run: request.run_id,
                token: writer_token(request.run_id, &writer),
                writer,
                epoch_floor,
            },
            deadline,
        )?;

        let binding = operations.binding();
        self.serial = self.serial.saturating_add(1);
        self.operations = Some(operations);
        self.session = Some(session);
        Ok(binding)
    }

    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        if request.takeover != umbra_core::TakeoverPolicy::Refuse {
            // Expiry is never proof of former-writer termination, and no
            // independent verifier exists, so takeover is refused outright.
            return unsupported(
                "acquire_writer",
                "takeover requires independently confirmed termination",
            );
        }
        // Admission is acquired by `open_run`, not here: a run this provider has
        // open is a run it was already admitted to. This method hands back the
        // lease for that admission, and refuses to invent one for a different
        // run or a different writer.
        let session = self.session.as_ref().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                "acquire_writer",
                "no run is open on this provider, so no admission is held",
            )
        })?;
        let lease = session.lease();
        if lease.run_id != request.run_id {
            return Err(UmbraError::new(
                ErrorKind::InvalidInput,
                "acquire_writer",
                "this provider holds admission for a different run",
            ));
        }
        if lease.writer_id != request.writer_id {
            // The marker names one writer. Handing this lease to a different
            // writer id would report an admission that writer never obtained.
            return Err(UmbraError::new(
                ErrorKind::LeaseLost,
                "acquire_writer",
                "the run's admission marker records a different writer",
            ));
        }
        Ok(lease)
    }

    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        let deadline = self.config.deadline;
        // A lease this session never issued is a rejected argument, not a lost
        // admission: nothing about the durable marker has been observed yet. Only
        // the marker re-proof below can latch authority loss (R1-002).
        {
            let (session, ..) = self.authority("renew_writer")?;
            if !session.owns_lease(lease) {
                return Err(UmbraError::new(
                    ErrorKind::LeaseLost,
                    "renew_writer",
                    "the presented lease is not the one this session holds",
                ));
            }
        }
        let renewed = {
            let (session, operations, transport, state) = self.authority("renew_writer")?;
            session.renew(transport, state, operations.anchors(), lease, deadline)
        };
        match renewed {
            Ok(lease) => Ok(lease),
            // R1-002: a renewal that could not re-prove this session's ownership
            // is terminal. Returning the error and leaving the session installed
            // let the very next mutation build a context and dispatch anyway.
            Err(error) => {
                self.observe_disposition("renew_writer", &error);
                self.authority_loss = Some(AuthorityLoss::RenewalFailed(error.clone()));
                Err(error)
            }
        }
    }

    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        let deadline = self.config.deadline;
        {
            let (session, _, _, _) = self.authority("release_writer")?;
            if session.lease() != *lease {
                return Err(UmbraError::new(
                    ErrorKind::LeaseLost,
                    "release_writer",
                    "the presented lease is not the one this session holds",
                ));
            }
        }
        self.release("release_writer", deadline)
    }

    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        let operation = crate::capability::operation_name(&request.operation);
        // R2-001 / R2-002: one gate, before anything durable happens. This used to
        // be two half-checks in the wrong places — the epoch here, and everything
        // else inside `Operations::execute`, which the journal already ran ahead
        // of. A request naming another run, a mutation on a read-only run, an
        // unsupported operation or an oversized one therefore consumed an
        // operation id and left `GUARDED4`-created index and intent records in
        // `.provider/retries` before being refused.
        self.preflight(operation, request)?;
        // R1-004: every supported mutation goes through the durable retry journal
        // before it reaches the wire. A recorded key is answered from its record
        // and never re-dispatched; a fresh key has its exact request persisted as
        // an intent first, so a lost reply can be reconciled from evidence rather
        // than by re-deriving the answer from the namespace as it stands now.
        //
        // Reads are not journalled: they have no effect to reconcile, and
        // recording one would consume durable space per lookup.
        let journalled = request.operation.is_mutation();
        if journalled {
            // R3-003: the record is consulted *before* anything about the
            // request's target is observed. A settled key's answer is its record;
            // making it depend on resolving the target again meant a completed
            // operation could stop being answerable because an external editor
            // later replaced a path component with a symlink, or made the lookup
            // fail with ACCESS. The journal itself stays readable in both cases.
            //
            // The journal's own round trips can still fail the way a dispatched
            // mutation can, so their disposition is observed too (R1-002).
            let found = match self.journal_lookup(operation, request) {
                Ok(found) => found,
                Err(error) => {
                    self.note_failure(operation, request, &error);
                    return Err(error);
                }
            };
            match found {
                // Settled: answered from the record, target untouched.
                crate::journal::Lookup::Recorded(result) => return result,
                crate::journal::Lookup::Fresh => {
                    // R2-003: the state this mutation is about to act on is
                    // observed before anything is written, and recorded beside
                    // the intent as the precondition evidence a record must carry.
                    let observed = match self.observe_now(operation, request) {
                        Ok(observed) => observed,
                        Err(error) => {
                            self.note_failure(operation, request, &error);
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        self.journal_admit_fresh(operation, request, &observed.evidence)
                    {
                        self.note_failure(operation, request, &error);
                        return Err(error);
                    }
                }
                crate::journal::Lookup::Interrupted(recorded) => {
                    let observed = match self.observe_now(operation, request) {
                        Ok(observed) => observed,
                        Err(error) => {
                            self.note_failure(operation, request, &error);
                            return Err(error);
                        }
                    };
                    let settled = self.journal_recover(operation, request, &recorded, &observed);
                    match settled {
                        Ok(crate::journal::Admission::Recorded(result)) => return result,
                        // The recorded evidence proves the interrupted attempt
                        // never landed, so dispatch proceeds under the same key.
                        Ok(_) => {}
                        Err(error) => {
                            self.note_failure(operation, request, &error);
                            return Err(error);
                        }
                    }
                }
            }
        }

        let outcome = {
            let (operations, mut context) = self.request(
                operation,
                Some(&request.context.idempotency_key),
                request.context.operation_id,
            )?;
            operations.execute(&mut context, request)
        };
        // R1-002: record, as it happens, any call whose server-side effect this
        // provider cannot rule out. `close_run` reads this ledger rather than
        // asserting `Excluded` from the shape of the dispatch loop.
        if let Err(error) = &outcome {
            self.note_failure(operation, request, error);
        }
        if journalled {
            // The outcome is recorded as it happened, a *settled* failure
            // included: a retry of a key that failed must be told the failure
            // rather than allowed to try again under the same identity.
            //
            // An outcome whose server-side disposition is unknown is deliberately
            // NOT settled. Recording "this failed" for a request that may well
            // have been applied would hand a later retry a wrong answer with the
            // authority of a durable record. The intent stays unsettled instead,
            // so the retry is told the operation is indeterminate and needs
            // reconciliation — which is what the replay facade's
            // `Admission::Indeterminate` means.
            let settled = match &outcome {
                Ok(_) => true,
                Err(error) => unsettled_detail(operation, error).is_none(),
            };
            if settled {
                // A journal write that itself fails is reported: the operation may
                // well have taken effect, and returning the result while its
                // record says the attempt is still in flight would leave a retry
                // unable to tell.
                if let Err(error) = self.journal_settle(operation, request, &outcome) {
                    self.note_failure(operation, request, &error);
                    return Err(error);
                }
            }
        }
        outcome
    }

    fn flush(&mut self, _request: &FlushRequest) -> Result<DurabilityReceipt> {
        // A receipt would assert a persistence boundary this provider has not
        // reached. Reporting the gate is the only honest answer.
        gated("flush", "authority-recovery")
    }

    fn close_run(&mut self) -> Result<()> {
        if self.operations.is_none() {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "close_run",
                "no run is open on this provider",
            ));
        }
        let deadline = self.config.deadline;
        // R2-001: a latched authority loss is surrendered, not released. The run
        // is torn down locally — it is unusable either way — but nothing is
        // written to the marker, so a successor's record survives intact. The
        // caller is told, because a close that published no release is not the
        // clean handover a plain `Ok` would imply.
        if self.session.is_some() && self.authority_loss.is_some() {
            let surrendered = self.release("close_run", deadline);
            self.operations = None;
            self.authority_loss = None;
            self.stable_write_failure = None;
            self.unsettled.clear();
            return surrendered;
        }
        // Release admission before dropping the run, so the next session can
        // acquire. A release that cannot be proven clean keeps the marker held
        // and keeps the run open: reporting a clean close over unsettled state
        // would hand the run to a follow-on session that could overlap this one.
        if self.session.is_some() {
            self.release("close_run", deadline)?;
        }
        self.operations = None;
        // The run is gone, and with it everything that was true only of that run:
        // the authority latch (R1-002) and the unsettled-call ledger both belong
        // to the closed run, not to the provider. Carrying either into the next
        // `open_run` would refuse a fresh, properly admitted run.
        self.authority_loss = None;
        self.recovery_blocked = None;
        self.stable_write_failure = None;
        self.unsettled.clear();
        // Every handle this session issued carries its serial, which `open_run`
        // has already advanced, so all of them are now rejected on presentation.
        // A close never implies a flush.
        Ok(())
    }

    fn read_at(
        &mut self,
        context: &RequestContext,
        path: &umbra_core::StoragePath,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize> {
        // Overridden so an unopened run is reported as such. The default helper
        // would fail the advertised-limit check first, which is true but says
        // nothing about the actual problem, and a caller reading `Ok(0)` as EOF
        // is the failure this override exists to keep impossible.
        let response = self.execute(&StorageRequest {
            context: context.clone(),
            operation: StorageOperation::ReadAt {
                path: path.clone(),
                offset,
                len: u32::try_from(out.len()).map_err(|_| {
                    UmbraError::new(
                        ErrorKind::InvalidInput,
                        "read_at",
                        "the output buffer exceeds the largest representable read",
                    )
                })?,
            },
        })?;
        match response {
            StorageResponse::ReadAt(bytes) if bytes.len() <= out.len() => {
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            _ => Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "read_at",
                "unexpected response kind or invalid response bounds",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeReplayLog, FakeTransport};
    use umbra_core::{ImmutableBaseContract, OpenRunIntent, RunId, StoragePolicy, TakeoverPolicy};
    use uuid::Uuid;

    fn config() -> NfsUserspaceConfig {
        NfsUserspaceConfig {
            host: b"127.0.0.1".to_vec(),
            port: 2049,
            export: BytePath::new(b"export".to_vec()).unwrap(),
            run_parent: BytePath::new(b"runs".to_vec()).unwrap(),
            root_anchor: BytePath::new(b"root".to_vec()).unwrap(),
            control_anchor: BytePath::new(b"control".to_vec()).unwrap(),
            deadline: Deadline { millis: 5_000 },
        }
    }

    #[test]
    fn options_round_trip_through_the_provider_encoding() {
        let encoded = umbra_core::provider::encode(&config()).unwrap();
        assert_eq!(
            NfsUserspaceConfig::from_options(&encoded).unwrap(),
            config()
        );
    }

    #[test]
    fn configuration_rejects_unanchorable_layouts() {
        for broken in [
            NfsUserspaceConfig {
                host: Vec::new(),
                ..config()
            },
            NfsUserspaceConfig {
                port: 0,
                ..config()
            },
            NfsUserspaceConfig {
                root_anchor: BytePath::new(b".provider".to_vec()).unwrap(),
                ..config()
            },
            NfsUserspaceConfig {
                control_anchor: BytePath::new(b"root".to_vec()).unwrap(),
                ..config()
            },
            NfsUserspaceConfig {
                deadline: Deadline { millis: 0 },
                ..config()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?} must be rejected");
        }
    }

    #[test]
    fn an_unwired_provider_advertises_nothing_and_names_what_is_missing() {
        let mut storage = NfsUserspaceStorage::connect(config()).unwrap();
        let capabilities = storage.capabilities();
        assert_eq!(capabilities.durability, Durability::None);
        assert_eq!(capabilities.fencing, Fencing::ReadOnly);
        assert!(capabilities.features.is_empty());
        // Without a transport there is no bound to honour, so none is advertised.
        assert_eq!(capabilities.max_io_bytes, 0);
        assert_eq!(capabilities.max_directory_entries, 0);

        let request = OpenRunRequest {
            run_id: RunId(Uuid::nil()),
            intent: OpenRunIntent::OpenExisting,
            immutable_base: ImmutableBaseContract {
                identity: "base".into(),
                fingerprint: vec![1, 2, 3],
            },
            policy: StoragePolicy {
                read_only: true,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: FORMAT_VERSION,
            },
        };
        // The operations surface is wired; what is absent here is the facade it
        // would run over, and the error says exactly that rather than reporting a
        // gate that no longer exists.
        assert_eq!(
            storage.open_run(&request).unwrap_err().kind,
            ErrorKind::StorageUnavailable
        );
        assert_eq!(
            storage.close_run().unwrap_err().kind,
            ErrorKind::InvalidState
        );
        // Writer authority is wired to product admission now, and admission is
        // acquired by `open_run`. With no run open there is no admission to
        // renew or release, and the error says that rather than naming a gate
        // that no longer exists.
        for kind in [
            storage.acquire_writer(&acquire()).unwrap_err().kind,
            storage.renew_writer(&lease()).unwrap_err().kind,
            storage.release_writer(&lease()).unwrap_err().kind,
        ] {
            assert_eq!(kind, ErrorKind::InvalidState);
        }
        // Durability receipts remain unwired: a receipt would assert a
        // persistence boundary this provider has not qualified.
        assert_eq!(
            storage
                .flush(&FlushRequest {
                    context: RequestContext {
                        run_id: RunId(Uuid::nil()),
                        operation_id: umbra_core::OperationId(Uuid::nil()),
                        writer_epoch: None,
                        idempotency_key: umbra_core::IdempotencyKey("k".into()),
                    },
                    scope: umbra_core::FlushScope::Data {
                        objects: Vec::new(),
                    },
                })
                .unwrap_err()
                .kind,
            ErrorKind::NotImplemented
        );
    }

    fn acquire() -> AcquireWriterRequest {
        AcquireWriterRequest {
            run_id: RunId(Uuid::nil()),
            writer_id: umbra_core::WriterId("writer".into()),
            takeover: TakeoverPolicy::Refuse,
        }
    }

    fn lease() -> WriterLease {
        WriterLease {
            run_id: RunId(Uuid::nil()),
            writer_id: umbra_core::WriterId("writer".into()),
            epoch: umbra_core::LeaseEpoch(1),
            renewal_token: Vec::new(),
            renew_after_millis: 1_000,
        }
    }

    #[test]
    fn takeover_is_refused_before_the_gate_is_reached() {
        let mut storage = NfsUserspaceStorage::connect(config()).unwrap();
        let request = AcquireWriterRequest {
            run_id: RunId(Uuid::nil()),
            writer_id: umbra_core::WriterId("writer".into()),
            takeover: TakeoverPolicy::FencePreviousWriter,
        };
        assert_eq!(
            storage.acquire_writer(&request).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
    }

    #[test]
    fn the_same_provider_accepts_a_fake_transport_without_consumer_changes() {
        let mut storage = NfsUserspaceStorage::with_facades(
            config(),
            Box::new(FakeTransport::new()),
            Box::new(FakeReplayLog::default()),
        )
        .unwrap();
        assert_eq!(storage.wire_profile(), WireProfile::V40_TCP_SYS);
        let deadline = storage.config().deadline;
        let root = storage.transport().unwrap().root_filehandle(deadline);
        assert!(root.is_ok());
        assert!(storage.replay().is_ok());
    }

    #[test]
    fn an_unbound_provider_says_so_rather_than_pretending() {
        let mut storage = NfsUserspaceStorage::connect(config()).unwrap();
        assert_eq!(
            storage.transport().err().unwrap().kind,
            ErrorKind::StorageUnavailable
        );
        assert_eq!(
            storage.replay().err().unwrap().kind,
            ErrorKind::StorageUnavailable
        );
    }
}
