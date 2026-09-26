//! Typed storage provider protocol over private local IPC.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use umbra_core::provider::{self as wire, protocol_error, Client, ProviderDescriptor};

/// Owned requests for protocol version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Capabilities.
    Capabilities {},
    /// Open run.
    OpenRun {
        /// Request.
        request: OpenRunRequest,
    },
    /// Acquire writer.
    AcquireWriter {
        /// Request.
        request: AcquireWriterRequest,
    },
    /// Renew writer.
    RenewWriter {
        /// Lease.
        lease: WriterLease,
    },
    /// Release writer.
    ReleaseWriter {
        /// Lease.
        lease: WriterLease,
    },
    /// Execute.
    Execute {
        /// Request.
        request: StorageRequest,
    },
    /// Flush.
    Flush {
        /// Request.
        request: FlushRequest,
    },
    /// Close run.
    CloseRun {},
}
/// Method-tagged successful responses; errors travel in the transport envelope.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Capabilities.
    Capabilities(StorageCapabilities),
    /// Open run.
    OpenRun(RunBinding),
    /// Acquire writer.
    AcquireWriter(WriterLease),
    /// Renew writer.
    RenewWriter(WriterLease),
    /// Release writer.
    ReleaseWriter(()),
    /// Execute.
    Execute(StorageResponse),
    /// Flush.
    Flush(DurabilityReceipt),
    /// Close run.
    CloseRun(()),
}
/// Synchronous proxy owning its provider process and connection.
pub struct Proxy {
    client: Mutex<Client>,
    advertised: Advertised,
}

/// What a proxy answers `capabilities()` with, and when that changes.
///
/// # The rule
///
/// **A proxy advertises the backend's answer for whatever the backend currently
/// has open**: the connect-time answer while no run is open, and the run
/// binding's own capabilities while one is. `RunBinding::capabilities` is
/// defined as the backend's answer for that run, so adopting it is not a second
/// opinion -- it is the same value the backend would give if asked again, taken
/// from a reply the caller already has.
///
/// # Why it has to move
///
/// A backend whose finite limits exist only for an open run. `nfs-userspace`
/// advertises `max_io_bytes: 0` before `open_run` -- deliberately, because
/// without a bound transport there is no bound it could honour -- and the local
/// `check_io` in every `read_at`/`write_at` default method reads this value.
/// Holding the connect-time answer for the life of the proxy refused every byte
/// of I/O on an open run, with a bounds error naming a limit the run did not
/// have.
///
/// # What else moves with it, deliberately
///
/// The `features` set moves too, and that is a behaviour change on the backends
/// that qualify per run rather than per connection. `umbra-storage-local` and
/// `umbra-storage-nfs` set their ownership and parent-identity flags immediately
/// *before* building the binding, precisely so `capabilities()` reports them --
/// so an open run over either now advertises names the connect-time answer did
/// not, and `Overlay`'s ownership-carry decision reads `capabilities()` live.
///
/// That is the intended reading of those names: each one's doc says it is
/// qualified "against a live store, never from configuration alone", and a run is
/// what supplies the live store. The stale connect-time cache was the defect. It
/// is called out here rather than left to be discovered because it changes what
/// two shared backends do, not only what the new one does.
#[derive(Clone, Debug)]
struct Advertised {
    /// The connect-time answer. Restored on close, so a proxy never keeps
    /// advertising a closed run's limits.
    unbound: StorageCapabilities,
    /// What `capabilities()` returns now.
    current: StorageCapabilities,
}

impl Advertised {
    fn new(unbound: StorageCapabilities) -> Self {
        Self {
            current: unbound.clone(),
            unbound,
        }
    }
    /// Adopt an opened run's own answer.
    fn adopt(&mut self, binding: &RunBinding) {
        self.current = binding.capabilities.clone();
    }
    /// Go back to the connect-time answer.
    fn restore(&mut self) {
        self.current = self.unbound.clone();
    }
}
impl Proxy {
    /// Validate provider registration and establish a private protocol session.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        descriptor.validate("storage")?;
        let mut client = Client::connect(descriptor, timeout_ms)?;
        let capabilities = match client.call(&Request::Capabilities {})? {
            Response::Capabilities(caps) => caps,
            _ => return Err(protocol_error("capabilities response mismatch")),
        };
        Ok(Self {
            client: Mutex::new(client),
            advertised: Advertised::new(capabilities),
        })
    }
    fn call(&self, request: &Request) -> Result<Response> {
        self.client
            .lock()
            .map_err(|_| protocol_error("poisoned provider connection"))?
            .call(request)
    }
}
impl Storage for Proxy {
    fn capabilities(&self) -> StorageCapabilities {
        self.advertised.current.clone()
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        match self.call(&Request::OpenRun {
            request: request.clone(),
        })? {
            Response::OpenRun(value) => {
                // A failed open changes nothing, so the unbound answer stays in
                // force; see `Advertised` for the rule and for what moves with it.
                self.advertised.adopt(&value);
                Ok(value)
            }
            _ => Err(protocol_error("storage.open_run response mismatch")),
        }
    }
    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        match self.call(&Request::AcquireWriter {
            request: request.clone(),
        })? {
            Response::AcquireWriter(value) => Ok(value),
            _ => Err(protocol_error("storage.acquire_writer response mismatch")),
        }
    }
    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        match self.call(&Request::RenewWriter {
            lease: lease.clone(),
        })? {
            Response::RenewWriter(value) => Ok(value),
            _ => Err(protocol_error("storage.renew_writer response mismatch")),
        }
    }
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        match self.call(&Request::ReleaseWriter {
            lease: lease.clone(),
        })? {
            Response::ReleaseWriter(value) => Ok(value),
            _ => Err(protocol_error("storage.release_writer response mismatch")),
        }
    }
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        validate_request(&self.advertised.current, request)?;
        match self.call(&Request::Execute {
            request: request.clone(),
        })? {
            Response::Execute(value) => Ok(value),
            _ => Err(protocol_error("storage.execute response mismatch")),
        }
    }
    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        match self.call(&Request::Flush {
            request: request.clone(),
        })? {
            Response::Flush(value) => Ok(value),
            _ => Err(protocol_error("storage.flush response mismatch")),
        }
    }
    fn close_run(&mut self) -> Result<()> {
        match self.call(&Request::CloseRun {})? {
            Response::CloseRun(value) => {
                // Back to the unbound answer: a closed run's limits are not this
                // proxy's to advertise, and a caller that read them would be told
                // a run it no longer has could still take I/O.
                self.advertised.restore();
                Ok(value)
            }
            _ => Err(protocol_error("storage.close_run response mismatch")),
        }
    }
}
/// Construct only this provider's backend after validating the handshake.
pub fn serve_provider<B: Storage>(
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<B>,
) -> Result<()> {
    let (connection, mut backend) = wire::accept(id, "storage", |options| {
        let backend = factory(options)?;
        let actual = backend.capabilities();
        // The open feature set is the extension point; the two legacy boolean
        // names are kept so existing registries keep resolving.
        let mut capabilities = actual.features.clone();
        if actual.strict_remote_persistence {
            capabilities.insert("strict_remote_persistence".into());
        }
        if actual.kernel_shadow {
            capabilities.insert("kernel_shadow".into());
        }
        Ok((backend, capabilities))
    })?;
    wire::serve(connection, |request| match request {
        Request::Capabilities {} => Ok(Response::Capabilities(backend.capabilities())),
        Request::OpenRun { request } => backend.open_run(&request).map(Response::OpenRun),
        Request::AcquireWriter { request } => backend
            .acquire_writer(&request)
            .map(Response::AcquireWriter),
        Request::RenewWriter { lease } => backend.renew_writer(&lease).map(Response::RenewWriter),
        Request::ReleaseWriter { lease } => {
            backend.release_writer(&lease).map(Response::ReleaseWriter)
        }
        Request::Execute { request } => {
            validate_request(&backend.capabilities(), &request)?;
            backend.execute(&request).map(Response::Execute)
        }
        Request::Flush { request } => backend.flush(&request).map(Response::Flush),
        Request::CloseRun {} => backend.close_run().map(Response::CloseRun),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{Durability, Fencing, RunId, RuntimeDirectoryBinding, StorageHandle};

    fn capabilities(max_io_bytes: u32, feature: Option<&str>) -> StorageCapabilities {
        StorageCapabilities {
            features: feature.into_iter().map(str::to_owned).collect(),
            durability: Durability::Local,
            strict_remote_persistence: false,
            fencing: Fencing::ReadOnly,
            kernel_shadow: false,
            complete_emulation: false,
            hard_links: false,
            logical_symlinks: false,
            xattrs: false,
            atomic_replace: false,
            atomic_swap: false,
            max_io_bytes,
            max_directory_entries: 0,
        }
    }

    fn binding(capabilities: StorageCapabilities) -> RunBinding {
        let anchor = RuntimeDirectoryBinding {
            handle: StorageHandle(vec![1]),
            physical_path: None,
        };
        RunBinding {
            run_id: RunId(uuid::Uuid::nil()),
            root: anchor.clone(),
            control: anchor,
            capabilities,
            admitted_writer: None,
        }
    }

    /// The adopted answer is the binding's, whole -- limits *and* features.
    ///
    /// `max_io_bytes` is why the adoption exists at all: `nfs-userspace`
    /// advertises zero with no run open, and the local `check_io` in `read_at`
    /// and `write_at` reads this value, so a stale cache refused every routed
    /// byte. The feature set moves with it deliberately; see `Advertised`.
    #[test]
    fn an_opened_run_is_advertised_with_that_run_s_own_capabilities() {
        let mut advertised = Advertised::new(capabilities(0, None));
        assert_eq!(advertised.current.max_io_bytes, 0);
        assert!(advertised.current.features.is_empty());

        advertised.adopt(&binding(capabilities(4096, Some("ownership-fidelity-v1"))));
        assert_eq!(advertised.current.max_io_bytes, 4096);
        assert!(advertised
            .current
            .features
            .contains("ownership-fidelity-v1"));
    }

    /// Closing goes back to the connect-time answer rather than keeping the
    /// run's. A caller reading a closed run's limits would be told a run it no
    /// longer has could still take I/O.
    #[test]
    fn closing_a_run_restores_the_connect_time_answer() {
        let mut advertised = Advertised::new(capabilities(0, None));
        advertised.adopt(&binding(capabilities(4096, Some("ownership-fidelity-v1"))));
        advertised.restore();
        assert_eq!(advertised.current.max_io_bytes, 0);
        assert!(advertised.current.features.is_empty());
        assert_eq!(advertised.current, advertised.unbound);
    }

    /// A second run's answer replaces the first's; adoption is not cumulative.
    #[test]
    fn a_later_run_replaces_the_earlier_run_s_answer() {
        let mut advertised = Advertised::new(capabilities(0, None));
        advertised.adopt(&binding(capabilities(4096, Some("ownership-fidelity-v1"))));
        advertised.adopt(&binding(capabilities(64, None)));
        assert_eq!(advertised.current.max_io_bytes, 64);
        assert!(
            advertised.current.features.is_empty(),
            "a feature the second run did not qualify must not survive from the first"
        );
    }
}
