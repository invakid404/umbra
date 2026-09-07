//! Typed namespace provider protocol over private local IPC.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use umbra_core::provider::{self as wire, protocol_error, Client, ProviderDescriptor};

/// Owned requests for protocol version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Resolve.
    Resolve {
        /// Context associated with this value or operation.
        context: ProcessContext,
        /// Operation.
        operation: FsOp,
    },
    /// Prepare.
    Prepare {
        /// Operation.
        operation: OperationId,
        /// Action.
        action: ResolvedAction,
    },
    /// Observe result.
    ObserveResult {
        /// Operation.
        operation: OperationId,
        /// Result.
        result: OperationOutcome,
    },
    /// Commit.
    Commit {
        /// Operation.
        operation: OperationId,
    },
    /// Abort.
    Abort {
        /// Operation.
        operation: OperationId,
        /// Reason.
        reason: AbortReason,
    },
    /// Checkpoint.
    Checkpoint {
        /// Request.
        request: CheckpointRequest,
    },
}
/// Method-tagged successful responses; errors travel in the transport envelope.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Resolve.
    Resolve(ResolvedAction),
    /// Prepare.
    Prepare(PreparedAction),
    /// Observe result.
    ObserveResult(()),
    /// Commit.
    Commit(CommitReceipt),
    /// Abort.
    Abort(()),
    /// Checkpoint.
    Checkpoint(Box<Checkpoint>),
}
/// Synchronous proxy owning its provider process and connection.
pub struct Proxy {
    client: Mutex<Client>,
}
impl Proxy {
    /// Validate provider registration and establish a private protocol session.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        descriptor.validate("namespace")?;
        let client = Client::connect(descriptor, timeout_ms)?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
    fn call(&self, request: &Request) -> Result<Response> {
        self.client
            .lock()
            .map_err(|_| protocol_error("poisoned provider connection"))?
            .call(request)
    }
}
impl NamespaceResolver for Proxy {
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp) -> Result<ResolvedAction> {
        match self.call(&Request::Resolve {
            context: context.clone(),
            operation: operation.clone(),
        })? {
            Response::Resolve(value) => Ok(value),
            _ => Err(protocol_error("namespace.resolve response mismatch")),
        }
    }
}
impl NamespaceSession for Proxy {
    fn prepare(
        &mut self,
        operation: OperationId,
        action: &ResolvedAction,
    ) -> Result<PreparedAction> {
        match self.call(&Request::Prepare {
            operation,
            action: action.clone(),
        })? {
            Response::Prepare(value) => Ok(value),
            _ => Err(protocol_error("namespace.prepare response mismatch")),
        }
    }
    fn observe_result(&mut self, operation: OperationId, result: &OperationOutcome) -> Result<()> {
        match self.call(&Request::ObserveResult {
            operation,
            result: result.clone(),
        })? {
            Response::ObserveResult(value) => Ok(value),
            _ => Err(protocol_error("namespace.observe_result response mismatch")),
        }
    }
    fn commit(&mut self, operation: OperationId) -> Result<CommitReceipt> {
        match self.call(&Request::Commit { operation })? {
            Response::Commit(value) => Ok(value),
            _ => Err(protocol_error("namespace.commit response mismatch")),
        }
    }
    fn abort(&mut self, operation: OperationId, reason: &AbortReason) -> Result<()> {
        match self.call(&Request::Abort {
            operation,
            reason: reason.clone(),
        })? {
            Response::Abort(value) => Ok(value),
            _ => Err(protocol_error("namespace.abort response mismatch")),
        }
    }
    fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint> {
        match self.call(&Request::Checkpoint {
            request: request.clone(),
        })? {
            Response::Checkpoint(value) => Ok(*value),
            _ => Err(protocol_error("namespace.checkpoint response mismatch")),
        }
    }
}
/// Construct only this provider's backend after validating the handshake.
pub fn serve_provider<B: NamespaceSession>(
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<B>,
) -> Result<()> {
    let (connection, mut backend) = wire::accept(id, "namespace", |options| {
        let backend = factory(options)?;
        let capabilities = std::collections::BTreeSet::new();
        Ok((backend, capabilities))
    })?;
    wire::serve(connection, |request| match request {
        Request::Resolve { context, operation } => {
            backend.resolve(&context, &operation).map(Response::Resolve)
        }
        Request::Prepare { operation, action } => {
            backend.prepare(operation, &action).map(Response::Prepare)
        }
        Request::ObserveResult { operation, result } => backend
            .observe_result(operation, &result)
            .map(Response::ObserveResult),
        Request::Commit { operation } => backend.commit(operation).map(Response::Commit),
        Request::Abort { operation, reason } => {
            backend.abort(operation, &reason).map(Response::Abort)
        }
        Request::Checkpoint { request } => backend
            .checkpoint(&request)
            .map(|checkpoint| Response::Checkpoint(Box::new(checkpoint))),
    })
}
