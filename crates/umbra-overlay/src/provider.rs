//! Typed namespace provider protocol over private local IPC.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use umbra_core::provider::{self as wire, protocol_error, Client, Connection, ProviderDescriptor};

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
    /// Renew the writer lease through the provider's own storage session.
    RenewWriter,
    /// Durably complete a fresh run and release its authority.
    FinishRun {
        /// Request.
        request: FinishRunRequest,
    },
    /// Leave the run explicitly failed, preserving evidence.
    FailRun {
        /// Request.
        request: FailedRunRequest,
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
    /// Renewed writer lease.
    RenewWriter(WriterLease),
    /// Evidence that the run reached its durable end state.
    FinishRun(FinishRunReceipt),
    /// Fail run.
    FailRun(()),
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
    fn renew_writer(&mut self) -> Result<WriterLease> {
        match self.call(&Request::RenewWriter)? {
            Response::RenewWriter(lease) => Ok(lease),
            _ => Err(protocol_error("namespace.renew_writer response mismatch")),
        }
    }
    fn finish_run(&mut self, request: &FinishRunRequest) -> Result<FinishRunReceipt> {
        match self.call(&Request::FinishRun {
            request: request.clone(),
        })? {
            Response::FinishRun(receipt) => Ok(receipt),
            _ => Err(protocol_error("namespace.finish_run response mismatch")),
        }
    }
    fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
        match self.call(&Request::FailRun {
            request: request.clone(),
        })? {
            Response::FailRun(value) => Ok(value),
            _ => Err(protocol_error("namespace.fail_run response mismatch")),
        }
    }
}
/// Construct only this provider's backend after validating the handshake.
pub fn serve_provider<B: NamespaceSession>(
    id: &str,
    factory: impl FnOnce(&[u8]) -> Result<B>,
) -> Result<()> {
    let (connection, backend) = wire::accept(id, "namespace", |options| {
        let backend = factory(options)?;
        let capabilities = std::collections::BTreeSet::new();
        Ok((backend, capabilities))
    })?;
    serve_session(connection, backend)
}
/// Dispatch this role's schema onto an already-handshaken backend.
///
/// Split out of [`serve_provider`] the same way `umbra-platform` splits its
/// session loop: the transport is injectable, so the real dispatch table is
/// exercised over a socketpair without a provider process.
fn serve_session<B: NamespaceSession>(connection: Connection, mut backend: B) -> Result<()> {
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
        Request::RenewWriter => backend.renew_writer().map(Response::RenewWriter),
        Request::FinishRun { request } => backend.finish_run(&request).map(Response::FinishRun),
        Request::FailRun { request } => backend.fail_run(&request).map(Response::FailRun),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use umbra_core::provider::{Hello, Welcome};
    use umbra_core::{
        Durability, DurabilityReceipt, Errno, ErrorKind, ExitStatus, FlushScope, LeaseEpoch, RunId,
        Sequence, UmbraError, WriterId,
    };
    use uuid::Uuid;

    fn run_id() -> RunId {
        RunId(Uuid::from_u128(0x5150))
    }
    fn lease() -> WriterLease {
        WriterLease {
            run_id: run_id(),
            writer_id: WriterId("writer-1".into()),
            epoch: LeaseEpoch(7),
            renewal_token: vec![0xff, 0x00, 0x7f],
            renew_after_millis: 30_000,
        }
    }
    fn finish_request() -> FinishRunRequest {
        FinishRunRequest {
            run_id: run_id(),
            root_status: Some(ExitStatus::Code(0)),
            processes_exited: 3,
        }
    }
    /// The root-never-launched state the field's own doc comment names. `None` is
    /// where an asymmetric serde attribute would break the round-trip, so it gets
    /// its own fixture rather than riding on the `Some` one.
    fn finish_request_without_root() -> FinishRunRequest {
        FinishRunRequest {
            run_id: run_id(),
            root_status: None,
            processes_exited: 0,
        }
    }
    fn finish_receipt() -> FinishRunReceipt {
        FinishRunReceipt {
            run_id: run_id(),
            durability: DurabilityReceipt {
                run_id: run_id(),
                writer_epoch: LeaseEpoch(7),
                scope: FlushScope::EntireRun,
                durability: Durability::Local,
                evidence: b"client-fsync".to_vec(),
            },
            completed_through: Sequence(42),
        }
    }
    fn fail_request() -> FailedRunRequest {
        FailedRunRequest {
            run_id: run_id(),
            reason: "tracee aborted".into(),
            tree_terminated: true,
        }
    }
    fn welcome() -> Welcome {
        Welcome {
            id: "fake-namespace".into(),
            role: "namespace".into(),
            version: wire::PROTOCOL_VERSION,
            capabilities: BTreeSet::new(),
        }
    }

    /// Enough of a backend to reach the three lifecycle methods, recording which
    /// one the server dispatched to so a test can prove the call crossed the
    /// wire rather than being answered by an inherited trait default.
    struct FakeBackend {
        calls: Arc<Mutex<Vec<String>>>,
        fail_with: Option<UmbraError>,
    }
    impl FakeBackend {
        fn new(calls: &Arc<Mutex<Vec<String>>>, fail_with: Option<UmbraError>) -> Self {
            Self {
                calls: calls.clone(),
                fail_with,
            }
        }
        fn record<T>(&mut self, call: String, value: T) -> Result<T> {
            self.calls.lock().expect("recorder").push(call);
            match &self.fail_with {
                Some(error) => Err(error.clone()),
                None => Ok(value),
            }
        }
        fn unused<T>(&self) -> Result<T> {
            Err(protocol_error("not exercised by the lifecycle tests"))
        }
    }
    impl NamespaceResolver for FakeBackend {
        fn resolve(&mut self, _: &ProcessContext, _: &FsOp) -> Result<ResolvedAction> {
            self.unused()
        }
    }
    impl NamespaceSession for FakeBackend {
        fn prepare(&mut self, _: OperationId, _: &ResolvedAction) -> Result<PreparedAction> {
            self.unused()
        }
        fn observe_result(&mut self, _: OperationId, _: &OperationOutcome) -> Result<()> {
            self.unused()
        }
        fn commit(&mut self, _: OperationId) -> Result<CommitReceipt> {
            self.unused()
        }
        fn abort(&mut self, _: OperationId, _: &AbortReason) -> Result<()> {
            self.unused()
        }
        fn checkpoint(&mut self, _: &CheckpointRequest) -> Result<Checkpoint> {
            self.unused()
        }
        fn renew_writer(&mut self) -> Result<WriterLease> {
            self.record("renew_writer".into(), lease())
        }
        fn finish_run(&mut self, request: &FinishRunRequest) -> Result<FinishRunReceipt> {
            let call = format!("finish_run:{}", request.processes_exited);
            self.record(call, finish_receipt())
        }
        fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
            let call = format!("fail_run:{}", request.reason);
            self.record(call, ())
        }
    }

    /// Run the real handshake and dispatcher over a socketpair, and hand back a
    /// `Proxy` speaking to them. `Proxy::connect` spawns a provider process and
    /// `Proxy.client` is private, so this construction only works in-module.
    fn connected(backend: FakeBackend) -> (Proxy, JoinHandle<Result<()>>) {
        let (client_side, server_side) = UnixStream::pair().expect("socketpair");
        let timeout = Duration::from_secs(5);
        let worker = std::thread::spawn(move || {
            let (connection, backend) = wire::accept_connection(
                Connection::new(server_side, timeout),
                "fake-namespace",
                "namespace",
                |_options| Ok((backend, BTreeSet::new())),
            )?;
            serve_session(connection, backend)
        });
        let mut connection = Connection::new(client_side, timeout);
        connection
            .send(&Hello {
                id: "fake-namespace".into(),
                role: "namespace".into(),
                version: wire::PROTOCOL_VERSION,
                required_capabilities: BTreeSet::new(),
                options: Vec::new(),
            })
            .expect("hello");
        let welcome = connection
            .receive::<Result<Welcome>>()
            .expect("welcome frame")
            .expect("welcome");
        assert_eq!(welcome.role, "namespace");
        let proxy = Proxy {
            client: Mutex::new(Client::from_connection(connection, welcome)),
        };
        (proxy, worker)
    }

    /// The proxy closing its connection is how the server loop ends; every test
    /// drops the proxy and expects the loop to report the disconnect.
    fn shut_down(proxy: Proxy, worker: JoinHandle<Result<()>>) {
        drop(proxy);
        assert!(worker.join().expect("server thread").is_err());
    }

    /// The wire codec is `umbra_core::provider::{encode, decode}` (serde_json);
    /// this repo has no second codec, so both directions are checked there.
    #[test]
    fn lifecycle_wire_pairs_round_trip_and_re_encode_byte_identically() {
        let renew = wire::encode(&Request::RenewWriter).unwrap();
        let Request::RenewWriter = wire::decode::<Request>(&renew).unwrap() else {
            panic!("RenewWriter decoded as another request");
        };
        assert_eq!(
            wire::encode(&wire::decode::<Request>(&renew).unwrap()).unwrap(),
            renew
        );

        let finish = wire::encode(&Request::FinishRun {
            request: finish_request(),
        })
        .unwrap();
        let Request::FinishRun { request } = wire::decode::<Request>(&finish).unwrap() else {
            panic!("FinishRun decoded as another request");
        };
        assert_eq!(request.run_id, finish_request().run_id);
        assert_eq!(request.root_status, finish_request().root_status);
        assert_eq!(request.processes_exited, finish_request().processes_exited);
        assert_eq!(
            wire::encode(&Request::FinishRun { request }).unwrap(),
            finish
        );

        let unlaunched = wire::encode(&Request::FinishRun {
            request: finish_request_without_root(),
        })
        .unwrap();
        let Request::FinishRun { request } = wire::decode::<Request>(&unlaunched).unwrap() else {
            panic!("FinishRun decoded as another request");
        };
        assert_eq!(request.root_status, None);
        assert_eq!(request.processes_exited, 0);
        assert_eq!(
            wire::encode(&Request::FinishRun { request }).unwrap(),
            unlaunched
        );

        let fail = wire::encode(&Request::FailRun {
            request: fail_request(),
        })
        .unwrap();
        let Request::FailRun { request } = wire::decode::<Request>(&fail).unwrap() else {
            panic!("FailRun decoded as another request");
        };
        assert_eq!(request.run_id, fail_request().run_id);
        assert_eq!(request.reason, fail_request().reason);
        assert_eq!(request.tree_terminated, fail_request().tree_terminated);
        assert_eq!(wire::encode(&Request::FailRun { request }).unwrap(), fail);

        let renewed = wire::encode(&Response::RenewWriter(lease())).unwrap();
        let Response::RenewWriter(value) = wire::decode::<Response>(&renewed).unwrap() else {
            panic!("RenewWriter decoded as another response");
        };
        assert_eq!(value, lease());
        assert_eq!(
            wire::encode(&Response::RenewWriter(value)).unwrap(),
            renewed
        );

        let receipt = wire::encode(&Response::FinishRun(finish_receipt())).unwrap();
        let Response::FinishRun(value) = wire::decode::<Response>(&receipt).unwrap() else {
            panic!("FinishRun decoded as another response");
        };
        assert_eq!(value.run_id, finish_receipt().run_id);
        assert_eq!(value.durability, finish_receipt().durability);
        assert_eq!(value.completed_through, finish_receipt().completed_through);
        assert_eq!(wire::encode(&Response::FinishRun(value)).unwrap(), receipt);

        let failed = wire::encode(&Response::FailRun(())).unwrap();
        let Response::FailRun(value) = wire::decode::<Response>(&failed).unwrap() else {
            panic!("FailRun decoded as another response");
        };
        assert_eq!(wire::encode(&Response::FailRun(value)).unwrap(), failed);
    }

    #[test]
    fn lifecycle_calls_reach_the_backend_through_the_dispatcher() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (mut proxy, worker) = connected(FakeBackend::new(&calls, None));

        assert_eq!(proxy.renew_writer().unwrap(), lease());
        let receipt = proxy.finish_run(&finish_request()).unwrap();
        assert_eq!(receipt.run_id, finish_receipt().run_id);
        assert_eq!(receipt.durability, finish_receipt().durability);
        assert_eq!(
            receipt.completed_through,
            finish_receipt().completed_through
        );
        proxy.fail_run(&fail_request()).unwrap();

        // Each request selected its own backend method, with its payload intact.
        assert_eq!(
            *calls.lock().unwrap(),
            ["renew_writer", "finish_run:3", "fail_run:tracee aborted"]
        );
        shut_down(proxy, worker);
    }

    #[test]
    fn a_backend_lifecycle_failure_crosses_the_wire_with_every_field_intact() {
        let mut expected = UmbraError::new(
            ErrorKind::LeaseLost,
            "namespace.finish_run",
            "writer epoch advanced before the completion record was durable",
        )
        .with_errno(Errno(13));
        expected.launch_tree_terminated = true;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (mut proxy, worker) = connected(FakeBackend::new(&calls, Some(expected.clone())));

        let error = proxy.finish_run(&finish_request()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::LeaseLost);
        assert_eq!(error.operation, "namespace.finish_run");
        assert_eq!(
            error.context,
            "writer epoch advanced before the completion record was durable"
        );
        assert_eq!(error.errno, Some(Errno(13)));
        assert!(error.launch_tree_terminated);
        assert_eq!(error, expected);
        // The error travelled in the transport envelope, so the session survives it.
        assert_eq!(*calls.lock().unwrap(), ["finish_run:3"]);
        shut_down(proxy, worker);
    }

    #[test]
    fn a_wrong_response_variant_is_refused_for_every_lifecycle_call() {
        let (client_side, server_side) = UnixStream::pair().expect("socketpair");
        let timeout = Duration::from_secs(5);
        // A well-formed server that answers each right request with a wrong
        // response variant: exactly what the proxy's match arms exist to catch.
        // Every answer is a *valid* response of another method, not a malformed
        // frame, so nothing but the variant check can reject it.
        let worker = std::thread::spawn(move || {
            wire::serve(
                Connection::new(server_side, timeout),
                |request: Request| match request {
                    Request::RenewWriter => Ok(Response::Commit(CommitReceipt {
                        operation_id: OperationId(Uuid::from_u128(9)),
                        sequence: Sequence(1),
                    })),
                    // `FailRun` and `Abort` are both `(())`, so this is the
                    // copy-paste a reader cannot see and the compiler accepts.
                    Request::FinishRun { .. } => Ok(Response::Abort(())),
                    Request::FailRun { .. } => Ok(Response::RenewWriter(lease())),
                    _ => Err(protocol_error("unexpected request")),
                },
            )
        });
        let mut proxy = Proxy {
            client: Mutex::new(Client::from_connection(
                Connection::new(client_side, timeout),
                welcome(),
            )),
        };

        for (error, expected) in [
            (
                proxy.renew_writer().unwrap_err(),
                "namespace.renew_writer response mismatch",
            ),
            (
                proxy.finish_run(&finish_request()).unwrap_err(),
                "namespace.finish_run response mismatch",
            ),
            (
                proxy.fail_run(&fail_request()).unwrap_err(),
                "namespace.fail_run response mismatch",
            ),
        ] {
            assert_eq!(error, protocol_error(expected));
            assert_eq!(error.kind, ErrorKind::ProtocolMismatch);
            assert_eq!(error.context, expected);
        }
        shut_down(proxy, worker);
    }
}
