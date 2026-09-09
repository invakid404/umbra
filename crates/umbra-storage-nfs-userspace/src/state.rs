//! Umbra-owned NFSv4.0 protocol state.
//!
//! libnfs is expected to supply raw RPC and XDR. Everything that makes those
//! bytes a *client* — the client id and its incarnation, open and lock owners,
//! stateids, seqids, lease renewal, reconnect, grace and reclaim, and the
//! verifier accounting the replay ledger consumes — is Umbra's, and it lives
//! here. Nothing in this module opens a socket, links a library or knows a wire
//! format: it talks to [`RawTransport`](crate::transport::RawTransport) and to
//! [`ReplayLog`](crate::replay::ReplayLog), and it is developed against the fake
//! implementations of both.
//!
//! # Wire profile
//!
//! NFSv4.0 over TCP with AUTH_SYS. There is no v4.1 code path to disable: the
//! session model — `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`,
//! `RECLAIM_COMPLETE` — has no representation in the frozen transport, so
//! reclaim here is v4.0 `CLAIM_PREVIOUS` and a bounded grace, and could not be
//! anything else.
//!
//! # The state machine
//!
//! ```text
//!  ClientIdentity ──SETCLIENTID──▶ PendingClient ──SETCLIENTID_CONFIRM──▶ ConfirmedClient
//!        ▲                              │                                       │
//!        │                        (lost reply /                            Session ──▶ OwnerLease
//!        │                       STALE_CLIENTID)                                │
//!        └──────────────────────────────┘                                       ▼
//!                                                             OPEN ──▶ [OPEN_CONFIRM] ──▶ OpenFile
//!                                                                                          │
//!                                                          ┌───────────────┬───────────────┤
//!                                                          ▼               ▼               ▼
//!                                                   OPEN_DOWNGRADE       CLOSE      reconnect ──▶
//!                                                                                   CLAIM_PREVIOUS
//! ```
//!
//! Each arrow is a typed transition that consumes its predecessor, so the illegal
//! moves are not caught at runtime — they do not compile. There is no way to
//! renew an unconfirmed client id, to open twice with one owner lease, to use an
//! `OpenFile` after closing it, or to read a stateid that OPEN_CONFIRM never
//! made usable.
//!
//! # Two things this module will not do
//!
//! * **It binds no transport.** Every call goes through the frozen
//!   `RawTransport` trait. The only implementation this crate has is the fake.
//! * **It grants no writer authority.** A live lease is not admission, and an
//!   expired lease is not proof anybody else is gone.
//!   [`LeaseClock::takeover_by_timeout`] refuses, by construction and by test.
//!   One-session-one-Umbra admission belongs to `authority_recovery`.

pub mod client_id;
pub mod lease;
pub mod open_owner;
pub mod reclaim;
pub mod retained_errors;
pub mod seqid;
pub mod verifier;

use crate::error::{AuthorityError, FacadeError, FacadeResult};
use crate::handle::{FileHandle, OpenFile, Session, SessionId};
use crate::state::client_id::{ClientIdentity, ConfirmOutcome, ConfirmedClient};
use crate::state::lease::{EpochVerdict, LeaseClock, RenewOutcome};
use crate::state::open_owner::{LockOwnerRegistry, OpenOwnerRegistry};
use crate::state::reclaim::{ReclaimPlan, ReclaimReport};
use crate::state::retained_errors::RetainedErrorLedger;
use crate::state::verifier::CreateVerifierLedger;
use crate::transport::{ConnectionEpoch, ConnectionState, Deadline, RawTransport};

/// An established client incarnation and everything derived from it.
///
/// This is the value a reconnect replaces wholesale. Grouping the session, the
/// owner registries and the lease together is what makes "state built on an older
/// connection" a thing you can drop rather than a set of fields you must remember
/// to invalidate one by one.
#[derive(Debug)]
pub struct Incarnation {
    client: ConfirmedClient,
    session: Session,
    open_owners: OpenOwnerRegistry,
    lock_owners: LockOwnerRegistry,
    lease: LeaseClock,
}

impl Incarnation {
    /// The confirmed client id.
    pub fn client(&self) -> &ConfirmedClient {
        &self.client
    }

    /// The facade session that mints handles and owners.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The open-owner registry.
    pub fn open_owners(&mut self) -> &mut OpenOwnerRegistry {
        &mut self.open_owners
    }

    /// The lock-owner registry. M1 dispatches no LOCK; see
    /// [`open_owner::LockOwnerLease`].
    pub fn lock_owners(&mut self) -> &mut LockOwnerRegistry {
        &mut self.lock_owners
    }

    /// The lease clock.
    pub fn lease(&mut self) -> &mut LeaseClock {
        &mut self.lease
    }

    /// The connection generation this incarnation was established on.
    pub fn epoch(&self) -> ConnectionEpoch {
        self.client.epoch()
    }
}

/// How establishing a client incarnation ended.
#[derive(Debug)]
pub enum EstablishOutcome {
    /// The client id is confirmed and may hold state.
    Established(Box<Incarnation>),
    /// `SETCLIENTID` or `SETCLIENTID_CONFIRM` was refused, verbatim.
    Refused(FacadeError),
    /// The outcome is unknown. Nothing was established and nothing is assumed.
    Indeterminate(FacadeError),
}

impl EstablishOutcome {
    /// The established incarnation, if there is one.
    pub fn established(self) -> Option<Incarnation> {
        match self {
            Self::Established(incarnation) => Some(*incarnation),
            _ => None,
        }
    }

    /// The verbatim failure, if this outcome carries one.
    pub fn error(&self) -> Option<&FacadeError> {
        match self {
            Self::Established(_) => None,
            Self::Refused(error) | Self::Indeterminate(error) => Some(error),
        }
    }
}

/// The protocol-state owner for one transport context.
///
/// Holds the retained identity across reconnects, the current incarnation when
/// there is one, and the two ledgers — create verifiers and retained errors —
/// whose whole point is to outlive an incarnation.
#[derive(Debug)]
pub struct ProtocolState {
    identity: ClientIdentity,
    incarnation: Option<Incarnation>,
    next_session: u64,
    creates: CreateVerifierLedger,
    retained: RetainedErrorLedger,
}

impl ProtocolState {
    /// Build protocol state for a client identity.
    pub fn new(identity: ClientIdentity) -> Self {
        Self {
            identity,
            incarnation: None,
            next_session: 1,
            creates: CreateVerifierLedger::new(),
            retained: RetainedErrorLedger::new(),
        }
    }

    /// The retained client identity. Stable across reconnects.
    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    /// The current incarnation, when one is established.
    pub fn incarnation(&mut self) -> Option<&mut Incarnation> {
        self.incarnation.as_mut()
    }

    /// The exclusive-create verifier ledger.
    pub fn create_verifiers(&mut self) -> &mut CreateVerifierLedger {
        &mut self.creates
    }

    /// The retained-error ledger.
    pub fn retained_errors(&mut self) -> &mut RetainedErrorLedger {
        &mut self.retained
    }

    /// Establish a client incarnation: `SETCLIENTID` then `SETCLIENTID_CONFIRM`.
    ///
    /// The lease period is read from the export root's `FATTR4_LEASE_TIME`. A
    /// server that will not answer that attribute still yields a working lease,
    /// on the conservative assumption in [`lease::ASSUMED_LEASE_SECONDS`].
    pub fn establish(
        &mut self,
        transport: &mut dyn RawTransport,
        root: &FileHandle,
        now_millis: u64,
        deadline: Deadline,
    ) -> EstablishOutcome {
        let epoch = match transport.connection() {
            ConnectionState::Connected(epoch) => epoch,
            ConnectionState::Broken(epoch) => {
                return EstablishOutcome::Indeterminate(FacadeError::Transport(
                    crate::error::TransportError::Disconnected {
                        epoch,
                        detail: "cannot establish a client id on a broken connection".into(),
                    },
                ))
            }
            ConnectionState::Idle => {
                return EstablishOutcome::Indeterminate(FacadeError::Transport(
                    crate::error::TransportError::Connect(
                        "cannot establish a client id before connecting".into(),
                    ),
                ))
            }
        };

        let pending = match self.identity.set_client_id(transport, epoch, deadline) {
            Ok(pending) => pending,
            Err(error) => {
                return match error.status() {
                    Some(_) => EstablishOutcome::Refused(error),
                    None => EstablishOutcome::Indeterminate(error),
                }
            }
        };
        let client = match pending.confirm(transport, deadline) {
            ConfirmOutcome::Confirmed(client) => client,
            ConfirmOutcome::Indeterminate(error) => return EstablishOutcome::Indeterminate(error),
            other => {
                let error = other
                    .error()
                    .cloned()
                    .unwrap_or_else(|| unreachable!("a non-confirmed outcome carries an error"));
                return EstablishOutcome::Refused(error);
            }
        };

        let lease = match LeaseClock::from_server(transport, root, epoch, now_millis, deadline) {
            Ok(lease) => lease,
            Err(error) => return EstablishOutcome::Indeterminate(error),
        };

        let session_id = SessionId(self.next_session);
        self.next_session = self.next_session.saturating_add(1);
        let session = client.session(session_id);
        // Owner bytes are scoped to the client id, and a re-established client
        // gets a new one. Including the incarnation and the session anyway means
        // a server that still remembers an owner from a previous incarnation
        // under the same id never sees those bytes proposed again.
        let mut prefix = self.identity.id().to_vec();
        prefix.extend_from_slice(b"/i");
        prefix.extend_from_slice(&self.identity.incarnation().to_be_bytes());
        prefix.extend_from_slice(b"/s");
        prefix.extend_from_slice(&session_id.0.to_be_bytes());
        let incarnation = Incarnation {
            open_owners: OpenOwnerRegistry::new(session.clone(), prefix.clone()),
            lock_owners: LockOwnerRegistry::new(client.client_id(), prefix),
            client,
            session,
            lease,
        };
        EstablishOutcome::Established(Box::new(incarnation))
    }

    /// Adopt an established incarnation as the current one.
    ///
    /// [`ProtocolState::establish`] hands the incarnation back rather than hiding
    /// it, because a caller may want to inspect it before committing to it. This
    /// is how it is installed.
    pub fn adopt(&mut self, incarnation: Incarnation) {
        self.incarnation = Some(incarnation);
    }

    /// Renew the lease if the client has been idle long enough to need it.
    ///
    /// Returns `None` when there is nothing to do — no incarnation, or a lease
    /// that ordinary traffic has already kept alive.
    pub fn renew_if_idle(
        &mut self,
        transport: &mut dyn RawTransport,
        now_millis: u64,
        deadline: Deadline,
    ) -> Option<RenewOutcome> {
        let incarnation = self.incarnation.as_mut()?;
        let client = incarnation.client.clone();
        incarnation
            .lease
            .renew_if_idle(transport, &client, now_millis, deadline)
    }

    /// Whether the current incarnation is still on the connection it was built on.
    pub fn observe(&self, transport: &dyn RawTransport) -> Option<EpochVerdict> {
        self.incarnation
            .as_ref()
            .map(|incarnation| incarnation.lease.observe(transport))
    }

    /// Drop the current incarnation because the connection generation changed.
    ///
    /// Returns the opens that were live, so a caller can build a reclaim plan
    /// from them. The retained identity is untouched: the re-established client
    /// presents the same id string and boot verifier, which is what makes the old
    /// state reclaimable at all.
    pub fn invalidate(&mut self) -> Option<Incarnation> {
        self.incarnation.take()
    }

    /// Re-establish the client after a reconnect and reclaim what `CLAIM_PREVIOUS`
    /// allows.
    ///
    /// The opens handed in are the ones that were live before the interruption.
    /// Only the confirmed ones are attempted; the rest are reported as
    /// surrendered. Nothing is re-opened with `CLAIM_NULL` to paper over a failed
    /// reclaim — that would manufacture an open the client did not hold across
    /// the interruption.
    pub fn reestablish_and_reclaim(
        &mut self,
        transport: &mut dyn RawTransport,
        root: &FileHandle,
        previous: &[OpenFile],
        now_millis: u64,
        deadline: Deadline,
    ) -> Result<(Incarnation, ReclaimReport), FacadeError> {
        let plan = ReclaimPlan::from_opens(previous);
        let mut incarnation = match self.establish(transport, root, now_millis, deadline) {
            EstablishOutcome::Established(incarnation) => *incarnation,
            other => {
                return Err(other
                    .error()
                    .cloned()
                    .unwrap_or_else(|| unreachable!("a non-established outcome carries an error")))
            }
        };
        let report = plan.run(&mut incarnation.open_owners, transport, deadline);
        Ok((incarnation, report))
    }

    /// Refuse a takeover request. Always.
    ///
    /// Protocol state has no mechanism that could grant one and no business
    /// owning the question. It is exposed so a caller reaching for "the lease
    /// expired, so I may take over" finds a refusal rather than a gap.
    pub fn takeover(&self) -> FacadeResult<std::convert::Infallible> {
        Err(FacadeError::Authority(AuthorityError::TakeoverRefused))
    }
}
