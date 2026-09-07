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
    capabilities: StorageCapabilities,
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
            capabilities,
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
        self.capabilities.clone()
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        match self.call(&Request::OpenRun {
            request: request.clone(),
        })? {
            Response::OpenRun(value) => Ok(value),
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
        validate_request(&self.capabilities, request)?;
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
            Response::CloseRun(value) => Ok(value),
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
        let mut capabilities = std::collections::BTreeSet::new();
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
