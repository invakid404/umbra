//! Provider scaffold: configuration and the synchronous [`Storage`] surface.
//!
//! Every contract method is wired to its facade seam and then stops at the M1
//! gate. A method whose semantics this provider will never offer answers
//! [`ErrorKind::UnsupportedCapability`]; a method that is authorised but not yet
//! implemented answers [`ErrorKind::NotImplemented`] naming the node that owns it.
//! Nothing here reports a success it did not perform.

use serde::{Deserialize, Serialize};
use umbra_core::provider::decode;
use umbra_core::{
    BytePath, Durability, ErrorKind, Fencing, FlushRequest, OpenRunRequest, RequestContext, Result,
    RunBinding, StorageAnchor, StorageCapabilities, StorageOperation, StoragePath, StorageRequest,
    StorageResponse, UmbraError, WriterLease,
};
use umbra_storage::{AcquireWriterRequest, DurabilityReceipt, Storage};

use crate::ops::{Operations, OpsContext};
use crate::replay::ReplayLog;
use crate::transport::{Deadline, RawTransport, WireProfile};

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
    /// Serial of the next run session, folded into every issued
    /// [`StorageHandle`](umbra_core::StorageHandle) so a token from a closed run
    /// cannot be replayed against the next one.
    serial: u64,
}

impl std::fmt::Debug for NfsUserspaceStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NfsUserspaceStorage")
            .field("config", &self.config)
            .field("transport_bound", &self.transport.is_some())
            .field("replay_bound", &self.replay.is_some())
            .field("run_open", &self.operations.is_some())
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
        Ok(Self {
            config,
            transport: None,
            replay: None,
            operations: None,
            serial: 0,
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
        Ok(Self {
            config,
            transport: Some(transport),
            replay: Some(replay),
            operations: None,
            serial: 0,
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

    /// Borrow the three pieces one request needs, or report what is missing.
    ///
    /// Taken together rather than through three accessors because a request needs
    /// all three at once and each accessor would borrow the whole provider.
    /// `mutations` is always `None` here: a mutation needs open owners from a
    /// confirmed client incarnation, and establishing one is `authority_recovery`'s
    /// work, not this method's.
    fn request(&mut self, operation: &str) -> Result<(&Operations, OpsContext<'_>)> {
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
        Ok((
            operations,
            OpsContext {
                transport,
                replay,
                mutations: None,
                deadline,
            },
        ))
    }
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
        let transport = self.transport.as_deref_mut().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::StorageUnavailable,
                "open_run",
                "no transport facade is bound to this provider",
            )
        })?;
        // Failure must not publish a partially usable binding, so the surface is
        // built completely before anything is stored on the provider.
        let operations = Operations::open(transport, &config, request, serial, deadline)?;
        let binding = operations.binding();
        self.serial = self.serial.saturating_add(1);
        self.operations = Some(operations);
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
        gated("acquire_writer", "authority-recovery")
    }

    fn renew_writer(&mut self, _lease: &WriterLease) -> Result<WriterLease> {
        gated("renew_writer", "authority-recovery")
    }

    fn release_writer(&mut self, _lease: &WriterLease) -> Result<()> {
        gated("release_writer", "authority-recovery")
    }

    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        let operation = crate::capability::operation_name(&request.operation);
        let (operations, mut context) = self.request(operation)?;
        operations.execute(&mut context, request)
    }

    fn flush(&mut self, _request: &FlushRequest) -> Result<DurabilityReceipt> {
        // A receipt would assert a persistence boundary this provider has not
        // reached. Reporting the gate is the only honest answer.
        gated("flush", "authority-recovery")
    }

    fn close_run(&mut self) -> Result<()> {
        if self.operations.take().is_none() {
            return Err(UmbraError::new(
                ErrorKind::InvalidState,
                "close_run",
                "no run is open on this provider",
            ));
        }
        // Every handle this session issued carries its serial, which `open_run`
        // has already advanced, so all of them are now rejected on presentation.
        // No writer lease is released here: this provider acquires none, and a
        // close never implies a flush.
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
        // Writer authority, renewal, release and durability receipts still belong
        // to `authority_recovery` and still report their gate.
        for kind in [
            storage.renew_writer(&lease()).unwrap_err().kind,
            storage.release_writer(&lease()).unwrap_err().kind,
        ] {
            assert_eq!(kind, ErrorKind::NotImplemented);
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
