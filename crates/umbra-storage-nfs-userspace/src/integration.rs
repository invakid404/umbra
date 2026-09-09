//! The `raw_rpc` ↔ `raw_state` join.
//!
//! `raw_state` developed its protocol state machine against
//! [`FakeTransport`](crate::fake::FakeTransport); `raw_rpc` shipped
//! [`LibnfsRawTransport`](crate::transport::raw::LibnfsRawTransport). Both
//! satisfy the frozen [`RawTransport`], so joining them is a choice of backend
//! and nothing else — no state-machine code changes, and neither implementer's
//! public surface moves.
//!
//! This module makes that choice explicit and injectable. A [`StateSession`]
//! owns one transport and one [`ProtocolState`] driven over it. The transport is
//! held as `Box<dyn RawTransport>`, which is exactly the seam the state machine
//! already consumes (`&mut dyn RawTransport`), so the same scenario code runs
//! against either backend and a test can assert that it did.
//!
//! # What this deliberately does not do
//!
//! It does **not** reach `Storage`. `NfsUserspaceStorage` still binds no
//! transport and still answers `NotImplemented`; binding a live transport into
//! the provider — and exposing it to `storage-ops` or `authority-recovery` — is
//! `m1_integrate`'s seam, deferred by the M1 scope-lock. Nothing here advertises
//! a capability, opens a run, or persists anything.
//!
//! The live backend is behind the off-by-default `transport-raw` feature, so a
//! default build of this module compiles the fake path only and needs no native
//! toolchain.

use crate::error::FacadeResult;
use crate::fake::FakeTransport;
use crate::handle::FileHandle;
use crate::state::client_id::ClientIdentity;
use crate::state::{EstablishOutcome, ProtocolState};
use crate::transport::{
    ConnectionEpoch, ConnectionState, Deadline, RawTransport, TransportResult, Verifier,
};

/// Which [`RawTransport`] implementation a session is running over.
///
/// Carried so a test that drives both backends through one code path can prove
/// which one actually answered, rather than inferring it from a side effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// [`FakeTransport`] — the in-memory shape fake. No socket, no I/O.
    Fake,
    /// `LibnfsRawTransport` — real NFSv4.0/TCP/AUTH_SYS over libnfs raw RPC.
    Libnfs,
}

impl Backend {
    /// Whether this backend actually speaks to a server.
    ///
    /// The distinction is load-bearing in evidence: a criterion met only under
    /// the fake is a shape check, not an acceptance result.
    pub fn is_live(self) -> bool {
        matches!(self, Backend::Libnfs)
    }
}

/// One protocol-state session over one transport.
///
/// The transport and the state machine are held together because they share a
/// lifetime in practice: a [`ConnectionEpoch`] change invalidates the state
/// built on it, and keeping the two in one value means a caller cannot renew a
/// lease against a transport the state never saw.
pub struct StateSession {
    backend: Backend,
    transport: Box<dyn RawTransport>,
    state: ProtocolState,
}

impl std::fmt::Debug for StateSession {
    /// `RawTransport` is not `Debug`, and a trait object cannot be printed
    /// without one, so the observable session facts are reported instead.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateSession")
            .field("backend", &self.backend)
            .field("connection", &self.transport.connection())
            .finish()
    }
}

impl StateSession {
    /// Build a session over an arbitrary transport.
    ///
    /// This is the injection point. `over_fake` and `over_libnfs` are the two
    /// callers that matter, but taking a boxed trait object keeps the seam open
    /// for a future backend without another constructor.
    pub fn new(
        backend: Backend,
        transport: Box<dyn RawTransport>,
        identity: ClientIdentity,
    ) -> Self {
        Self {
            backend,
            transport,
            state: ProtocolState::new(identity),
        }
    }

    /// A session over the in-memory fake.
    ///
    /// Takes the fake by value so a test can pre-script it (write caps, verifier
    /// rotation, grace) before the state machine sees it.
    pub fn over_fake(fake: FakeTransport, identity: ClientIdentity) -> Self {
        Self::new(Backend::Fake, Box::new(fake), identity)
    }

    /// A session over a live libnfs raw-RPC transport.
    ///
    /// The wire profile is checked inside `LibnfsRawTransport::connect` before
    /// any socket work, so a non-v4.0/TCP/AUTH_SYS configuration is refused here
    /// without reaching the network.
    #[cfg(feature = "transport-raw")]
    pub fn over_libnfs(
        config: crate::transport::raw::RawTransportConfig,
        identity: ClientIdentity,
    ) -> TransportResult<Self> {
        let transport = crate::transport::raw::LibnfsRawTransport::connect(config)?;
        Ok(Self::new(Backend::Libnfs, Box::new(transport), identity))
    }

    /// Which implementation is answering.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The transport, for operations the state machine does not own (READ,
    /// READDIR, WRITE, COMMIT and filehandle plumbing).
    pub fn transport(&mut self) -> &mut dyn RawTransport {
        &mut *self.transport
    }

    /// The protocol state.
    pub fn state(&mut self) -> &mut ProtocolState {
        &mut self.state
    }

    /// The transport, read-only, for the observations that do not drive it.
    ///
    /// [`ProtocolState::observe`] compares connection generations and needs no
    /// mutable access; handing out a shared reference keeps that call from
    /// requiring the session to be borrowed mutably twice.
    pub fn transport_ref(&self) -> &dyn RawTransport {
        &*self.transport
    }

    /// Whether the current incarnation is still on the connection it was built
    /// on.
    ///
    /// Expressed here rather than left to the caller because both halves are
    /// needed at once and both borrows are shared: reaching for `state()` and
    /// `transport_ref()` separately would borrow the session mutably and
    /// immutably in one expression, which does not compile.
    pub fn observe(&self) -> Option<crate::state::lease::EpochVerdict> {
        self.state.observe(&*self.transport)
    }

    /// Both halves at once.
    ///
    /// The state machine's driver methods take `&mut ProtocolState` *and*
    /// `&mut dyn RawTransport` in the same call, which two separate accessors
    /// cannot supply without borrowing the session mutably twice. This is the
    /// one place that split is expressed, so a caller never has to disassemble
    /// the session to make a transition.
    pub fn split(&mut self) -> (&mut ProtocolState, &mut dyn RawTransport) {
        (&mut self.state, &mut *self.transport)
    }

    /// The connection generation the transport is currently on.
    pub fn epoch(&self) -> Option<ConnectionEpoch> {
        match self.transport.connection() {
            ConnectionState::Connected(epoch) | ConnectionState::Broken(epoch) => Some(epoch),
            ConnectionState::Idle => None,
        }
    }

    /// Read the export root filehandle (`PUTROOTFH; GETFH`).
    pub fn root(&mut self, deadline: Deadline) -> FacadeResult<FileHandle> {
        self.transport.root_filehandle(deadline)
    }

    /// Establish a client incarnation and adopt it.
    ///
    /// This is the whole SETCLIENTID / SETCLIENTID_CONFIRM lifecycle running
    /// over whichever backend the session was built with.
    pub fn establish(
        &mut self,
        root: &FileHandle,
        now_millis: u64,
        deadline: Deadline,
    ) -> EstablishOutcome {
        self.state
            .establish(&mut *self.transport, root, now_millis, deadline)
    }

    /// Establish and adopt in one step, returning whether it took.
    pub fn establish_and_adopt(
        &mut self,
        root: &FileHandle,
        now_millis: u64,
        deadline: Deadline,
    ) -> Result<(), crate::error::FacadeError> {
        match self.establish(root, now_millis, deadline) {
            EstablishOutcome::Established(incarnation) => {
                self.state.adopt(*incarnation);
                Ok(())
            }
            other => Err(other
                .error()
                .cloned()
                .unwrap_or_else(|| unreachable!("a non-established outcome carries an error"))),
        }
    }

    /// Force a fresh connection generation.
    ///
    /// Every outstanding call is withdrawn before the socket goes. The returned
    /// epoch strictly advances, which is what makes state built on the previous
    /// generation rejectable rather than silently reused.
    pub fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.transport.reconnect()
    }
}

/// A client identity suitable for a test or a fixture session.
///
/// The id string carries the caller's label so two sessions against one server
/// are distinct client ids, and the boot verifier is supplied rather than
/// derived so a caller can model a client restart (a new verifier) versus a
/// reconnect (the same one) deliberately.
pub fn identity_for(label: &str, boot_verifier: Verifier) -> ClientIdentity {
    let mut id = b"umbra/nfs-userspace/".to_vec();
    id.extend_from_slice(label.as_bytes());
    ClientIdentity::new(id, boot_verifier)
}
