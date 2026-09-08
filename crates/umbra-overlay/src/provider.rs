//! Typed namespace provider protocol over private local IPC.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use umbra_core::provider::{self as wire, protocol_error, Client, ProviderDescriptor};

/// Owned requests for protocol version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Read logical bytes without a native syscall buffer layout.
    ReadAt {
        /// Logical path beneath the tracee root anchor.
        path: StoragePath,
        /// Absolute byte offset.
        offset: u64,
        /// Bounded output size.
        len: u32,
    },
    /// Enumerate a merged directory snapshot.
    List {
        /// Logical directory beneath the tracee root anchor.
        path: StoragePath,
        /// Session-bound continuation.
        cursor: Option<umbra_core::ListCursor>,
        /// Maximum number of entries.
        limit: u32,
    },
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
    /// Read the logical symlink target verbatim.
    ReadLink {
        /// Logical path beneath the root anchor.
        path: StoragePath,
    },
    /// Read logical metadata.
    Stat {
        /// Logical path beneath the root anchor.
        path: StoragePath,
        /// Follow the final symlink.
        follow: bool,
    },
    /// Bind the next native readlink or readlinkat buffer.
    SetReadLinkBuffer {
        /// Tracee virtual address.
        address: u64,
        /// Output capacity, without a NUL terminator.
        len: u32,
    },
}
/// Method-tagged successful responses; errors travel in the transport envelope.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Read bytes.
    ReadAt(Vec<u8>),
    /// Merged directory page.
    List(umbra_core::DirectoryPage),
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
    /// Logical target bytes.
    ReadLink(BytePath),
    /// Logical object metadata.
    Stat(umbra_core::BlobStat),
    /// Readlink buffer bound.
    SetReadLinkBuffer(()),
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
    fn read_link(&mut self, path: &StoragePath) -> Result<BytePath> {
        match self.call(&Request::ReadLink { path: path.clone() })? {
            Response::ReadLink(target) => Ok(target),
            _ => Err(protocol_error("namespace.read_link response mismatch")),
        }
    }
    fn stat(&mut self, path: &StoragePath, follow: bool) -> Result<umbra_core::BlobStat> {
        match self.call(&Request::Stat {
            path: path.clone(),
            follow,
        })? {
            Response::Stat(stat) => Ok(stat),
            _ => Err(protocol_error("namespace.stat response mismatch")),
        }
    }
    fn set_readlink_buffer(&mut self, address: u64, len: u32) -> Result<()> {
        match self.call(&Request::SetReadLinkBuffer { address, len })? {
            Response::SetReadLinkBuffer(()) => Ok(()),
            _ => Err(protocol_error(
                "namespace.readlink_buffer response mismatch",
            )),
        }
    }
    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize> {
        if out.len() > umbra_core::MAX_IO_BYTES || offset.checked_add(out.len() as u64).is_none() {
            return Err(protocol_error("namespace read exceeds bounds"));
        }
        match self.call(&Request::ReadAt {
            path: path.clone(),
            offset,
            len: out.len() as u32,
        })? {
            Response::ReadAt(bytes) if bytes.len() <= out.len() => {
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            _ => Err(protocol_error("namespace.read_at response mismatch")),
        }
    }
    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&umbra_core::ListCursor>,
        limit: u32,
    ) -> Result<umbra_core::DirectoryPage> {
        match self.call(&Request::List {
            path: path.clone(),
            cursor: cursor.cloned(),
            limit,
        })? {
            Response::List(page) if page.entries.len() <= limit as usize => Ok(page),
            _ => Err(protocol_error("namespace.list response mismatch")),
        }
    }
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
        Request::ReadLink { path } => backend.read_link(&path).map(Response::ReadLink),
        Request::Stat { path, follow } => backend.stat(&path, follow).map(Response::Stat),
        Request::SetReadLinkBuffer { address, len } => backend
            .set_readlink_buffer(address, len)
            .map(Response::SetReadLinkBuffer),
        Request::ReadAt { path, offset, len } => {
            if len as usize > umbra_core::MAX_IO_BYTES || offset.checked_add(len as u64).is_none() {
                return Err(protocol_error("namespace read exceeds bounds"));
            }
            let mut bytes = vec![0; len as usize];
            let count = backend.read_at(&path, offset, &mut bytes)?;
            if count > bytes.len() {
                return Err(protocol_error("namespace read returned excessive bytes"));
            }
            bytes.truncate(count);
            Ok(Response::ReadAt(bytes))
        }
        Request::List {
            path,
            cursor,
            limit,
        } => backend
            .list(&path, cursor.as_ref(), limit)
            .map(Response::List),
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
