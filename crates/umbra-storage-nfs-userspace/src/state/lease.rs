//! Lease renewal through `OP_RENEW`, and the epoch rule that invalidates state.
//!
//! # Renewal is idle-only
//!
//! Any successful stateful operation renews an NFSv4.0 lease implicitly, so a
//! busy client never needs `OP_RENEW`. [`LeaseClock`] therefore tracks the last
//! time the server was known to have seen this client and issues a renewal only
//! once that has gone stale — "idle with a live pump", not a fixed heartbeat.
//! Time is supplied by the caller as a monotonic millisecond counter, so the
//! policy is testable without sleeping and without a clock of its own.
//!
//! # Renewal is not authority
//!
//! A successful `OP_RENEW` proves one thing: the NFS lease is alive. It says
//! nothing about whether this Umbra session is the admitted writer. Just as
//! importantly, a lease that *expires* is not evidence that anyone else may take
//! the state over. [`LeaseClock::takeover_by_timeout`] exists to make that
//! refusal a callable, testable fact rather than a comment: it always returns
//! [`AuthorityError::TakeoverRefused`]. One-session-one-Umbra admission belongs
//! to `authority_recovery`.
//!
//! # Reconnect invalidates
//!
//! The transport contract states plainly that reconnecting does not revalidate
//! protocol state. [`LeaseClock::observe`] compares the transport's
//! [`ConnectionEpoch`] against the one the lease was established on and reports
//! a change, which every holder of client-id, open and lock state must treat as
//! "prove it again", never as "carry on".

use crate::error::{AuthorityError, FacadeError, Nfs4Status};
use crate::handle::FileHandle;
use crate::state::client_id::ConfirmedClient;
use crate::transport::{AttrMask, ConnectionEpoch, ConnectionState, Deadline, RawTransport};

/// Default lease time to assume when the server does not report `FATTR4_LEASE_TIME`.
///
/// RFC 7530 does not mandate a value; 60 seconds is the conservative floor most
/// servers meet or exceed, and assuming a short lease renews too often rather
/// than too late.
pub const ASSUMED_LEASE_SECONDS: u32 = 60;

/// How the lease stands relative to the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseStanding {
    /// The server saw this client recently enough; nothing to do.
    Fresh,
    /// Past the renewal threshold. `OP_RENEW` should be issued while idle.
    DueForRenewal,
    /// The lease period has elapsed with no proof the server saw this client.
    ///
    /// The state may already have been released. It may equally still be intact;
    /// silence proves neither, which is why this is a distinct standing from a
    /// server actually answering `NFS4ERR_EXPIRED`.
    PossiblyExpired,
}

/// What one `OP_RENEW` settled.
#[derive(Debug)]
pub enum RenewOutcome {
    /// The lease is alive. This is not evidence of writer authority.
    Renewed,
    /// The server no longer knows this client id. The client id and every piece
    /// of state derived from it must be re-established before any mutation.
    ClientLost(FacadeError),
    /// The server refused for some other reason, reported verbatim.
    Refused(FacadeError),
    /// No answer arrived. The lease was not proven and nothing advanced.
    Unknown(FacadeError),
}

impl RenewOutcome {
    /// Whether the lease was actually proven alive.
    pub fn is_renewed(&self) -> bool {
        matches!(self, Self::Renewed)
    }

    /// The verbatim failure, if this outcome carries one.
    pub fn error(&self) -> Option<&FacadeError> {
        match self {
            Self::Renewed => None,
            Self::ClientLost(error) | Self::Refused(error) | Self::Unknown(error) => Some(error),
        }
    }
}

/// Whether the connection is still the one the state was built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochVerdict {
    /// Same generation. State established on it is still on its own connection.
    Unchanged(ConnectionEpoch),
    /// A new generation. Every piece of protocol state must be proven again.
    Changed {
        /// Generation the state was established on.
        established: ConnectionEpoch,
        /// Generation now in force.
        current: ConnectionEpoch,
    },
    /// The connection is down. Nothing can be proven until it is back.
    Broken(ConnectionEpoch),
    /// No connection has been established.
    Idle,
}

impl EpochVerdict {
    /// Whether state built on the established epoch may still be used.
    pub fn state_survives(&self) -> bool {
        matches!(self, Self::Unchanged(_))
    }
}

/// Tracks when the server last saw this client, and when to renew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseClock {
    lease_millis: u64,
    renew_after_millis: u64,
    last_seen_millis: u64,
    epoch: ConnectionEpoch,
}

impl LeaseClock {
    /// Build a clock for a lease of `lease_seconds`, established at `now_millis`.
    ///
    /// Renewal is attempted at half the lease, the conventional client margin: it
    /// leaves a whole second attempt inside the lease if the first one is lost.
    pub fn new(lease_seconds: u32, epoch: ConnectionEpoch, now_millis: u64) -> Self {
        let lease_millis = u64::from(lease_seconds.max(1)).saturating_mul(1_000);
        Self {
            lease_millis,
            renew_after_millis: (lease_millis / 2).max(1),
            last_seen_millis: now_millis,
            epoch,
        }
    }

    /// Read `FATTR4_LEASE_TIME` from the export root and build a clock from it.
    ///
    /// A server that does not answer the attribute gets [`ASSUMED_LEASE_SECONDS`],
    /// which renews more often than necessary rather than guessing high.
    pub fn from_server(
        transport: &mut dyn RawTransport,
        root: &FileHandle,
        epoch: ConnectionEpoch,
        now_millis: u64,
        deadline: Deadline,
    ) -> Result<Self, FacadeError> {
        let attributes = transport.getattr(root, AttrMask::LEASE_TIME, deadline)?;
        Ok(Self::new(
            attributes.lease_time.unwrap_or(ASSUMED_LEASE_SECONDS),
            epoch,
            now_millis,
        ))
    }

    /// The lease period in milliseconds.
    pub fn lease_millis(&self) -> u64 {
        self.lease_millis
    }

    /// The connection generation this lease was established on.
    pub fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// Record that the server demonstrably saw this client at `now_millis`.
    ///
    /// Every successful stateful operation should call this: an implicit renewal
    /// is exactly as good as an explicit one, and counting it is what keeps
    /// `OP_RENEW` off the wire for a busy client.
    pub fn note_activity(&mut self, now_millis: u64) {
        self.last_seen_millis = self.last_seen_millis.max(now_millis);
    }

    /// How the lease stands at `now_millis`.
    pub fn standing(&self, now_millis: u64) -> LeaseStanding {
        let idle = now_millis.saturating_sub(self.last_seen_millis);
        if idle >= self.lease_millis {
            LeaseStanding::PossiblyExpired
        } else if idle >= self.renew_after_millis {
            LeaseStanding::DueForRenewal
        } else {
            LeaseStanding::Fresh
        }
    }

    /// Issue `OP_RENEW` if the client has been idle long enough to need it.
    ///
    /// Returns `None` when the lease is still fresh, so a caller can drive this
    /// from an idle tick without deciding the policy itself.
    pub fn renew_if_idle(
        &mut self,
        transport: &mut dyn RawTransport,
        client: &ConfirmedClient,
        now_millis: u64,
        deadline: Deadline,
    ) -> Option<RenewOutcome> {
        match self.standing(now_millis) {
            LeaseStanding::Fresh => None,
            LeaseStanding::DueForRenewal | LeaseStanding::PossiblyExpired => {
                Some(self.renew(transport, client, now_millis, deadline))
            }
        }
    }

    /// Issue `OP_RENEW` unconditionally.
    ///
    /// The renewal is credited only when the server actually answered `NFS4_OK`.
    /// A failure of any kind leaves `last_seen` where it was, so a lease is never
    /// treated as refreshed by an attempt that did not land.
    pub fn renew(
        &mut self,
        transport: &mut dyn RawTransport,
        client: &ConfirmedClient,
        now_millis: u64,
        deadline: Deadline,
    ) -> RenewOutcome {
        if !client.is_valid_for(self.epoch) {
            return RenewOutcome::ClientLost(FacadeError::Authority(
                AuthorityError::IdentityUnproven(
                    "client id was confirmed on a different connection generation".into(),
                ),
            ));
        }
        match transport.renew(client.client_id(), deadline) {
            Ok(()) => {
                self.note_activity(now_millis);
                RenewOutcome::Renewed
            }
            Err(error) => match error.status() {
                Some(Nfs4Status::STALE_CLIENTID) | Some(Nfs4Status::EXPIRED) => {
                    RenewOutcome::ClientLost(error)
                }
                Some(_) => RenewOutcome::Refused(error),
                None => RenewOutcome::Unknown(error),
            },
        }
    }

    /// Compare the transport's connection generation against this lease's.
    pub fn observe(&self, transport: &dyn RawTransport) -> EpochVerdict {
        match transport.connection() {
            ConnectionState::Idle => EpochVerdict::Idle,
            ConnectionState::Broken(epoch) => EpochVerdict::Broken(epoch),
            ConnectionState::Connected(current) if current == self.epoch => {
                EpochVerdict::Unchanged(current)
            }
            ConnectionState::Connected(current) => EpochVerdict::Changed {
                established: self.epoch,
                current,
            },
        }
    }

    /// Adopt a new connection generation after a reconnect and a fresh client id.
    pub fn rebind(&mut self, epoch: ConnectionEpoch, now_millis: u64) {
        self.epoch = epoch;
        self.last_seen_millis = now_millis;
    }

    /// Refuse to treat lease expiry as permission to take state over.
    ///
    /// This always fails, on purpose. An expired lease means this client's own
    /// state may be gone; it is never evidence that some *other* session has
    /// terminated, and NFS has no mechanism that would make it so. Admission
    /// between competing Umbra sessions is `authority_recovery`'s decision and is
    /// not delegated to a timer here.
    pub fn takeover_by_timeout(&self) -> FacadeError {
        FacadeError::Authority(AuthorityError::TakeoverRefused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeTransport, ScriptedFault};
    use crate::state::client_id::ClientIdentity;
    use crate::transport::{FaultAction, FaultPoint, OpCode, Verifier};

    fn deadline() -> Deadline {
        Deadline { millis: 1_000 }
    }

    fn confirmed(transport: &mut FakeTransport) -> ConfirmedClient {
        ClientIdentity::new(b"umbra-m1-lease".to_vec(), Verifier([0x11; 8]))
            .set_client_id(transport, ConnectionEpoch(1), deadline())
            .unwrap()
            .confirm(transport, deadline())
            .confirmed()
            .expect("the fake confirms a fresh incarnation")
    }

    #[test]
    fn a_busy_client_never_renews_and_an_idle_one_does() {
        let mut transport = FakeTransport::new();
        let client = confirmed(&mut transport);
        let mut lease = LeaseClock::new(60, ConnectionEpoch(1), 0);
        assert_eq!(lease.standing(1_000), LeaseStanding::Fresh);
        assert!(lease
            .renew_if_idle(&mut transport, &client, 1_000, deadline())
            .is_none());

        assert_eq!(lease.standing(30_000), LeaseStanding::DueForRenewal);
        assert!(lease
            .renew_if_idle(&mut transport, &client, 30_000, deadline())
            .expect("an idle lease renews")
            .is_renewed());
        assert_eq!(lease.standing(30_500), LeaseStanding::Fresh);

        // An implicit renewal from ordinary traffic counts just as much.
        lease.note_activity(60_000);
        assert_eq!(lease.standing(80_000), LeaseStanding::Fresh);
    }

    #[test]
    fn the_lease_period_comes_from_the_server_when_it_answers() {
        let mut transport = FakeTransport::new();
        let root = transport.root();
        let lease =
            LeaseClock::from_server(&mut transport, &root, ConnectionEpoch(1), 0, deadline())
                .unwrap();
        assert_eq!(lease.lease_millis(), 90_000, "the fake reports 90 seconds");
    }

    #[test]
    fn a_failed_renew_does_not_refresh_the_lease() {
        let mut transport = FakeTransport::new();
        let client = confirmed(&mut transport);
        let mut lease = LeaseClock::new(60, ConnectionEpoch(1), 0);
        transport.install_faults(ScriptedFault::once(
            FaultPoint::OnDeadline,
            None,
            FaultAction::DropReply,
        ));
        let outcome = lease
            .renew_if_idle(&mut transport, &client, 40_000, deadline())
            .expect("an idle lease attempts a renewal");
        assert!(matches!(outcome, RenewOutcome::Unknown(_)));
        assert_eq!(
            lease.standing(40_000),
            LeaseStanding::DueForRenewal,
            "an unanswered renewal refreshes nothing"
        );
    }

    #[test]
    fn a_stale_client_id_is_client_loss_and_not_a_takeover_opportunity() {
        let mut transport = FakeTransport::new();
        let client = confirmed(&mut transport);
        let mut lease = LeaseClock::new(60, ConnectionEpoch(1), 0);
        transport.install_faults(ScriptedFault::once(
            FaultPoint::AfterDispatch,
            Some(OpCode::Renew),
            FaultAction::Substitute(Nfs4Status::EXPIRED),
        ));
        let outcome = lease.renew(&mut transport, &client, 10_000, deadline());
        assert!(matches!(outcome, RenewOutcome::ClientLost(_)));
        assert_eq!(
            outcome.error().and_then(FacadeError::status),
            Some(Nfs4Status::EXPIRED)
        );
        assert!(matches!(
            lease.takeover_by_timeout(),
            FacadeError::Authority(AuthorityError::TakeoverRefused)
        ));
    }

    #[test]
    fn expiry_alone_never_authorises_taking_state_over() {
        let lease = LeaseClock::new(60, ConnectionEpoch(1), 0);
        assert_eq!(lease.standing(120_000), LeaseStanding::PossiblyExpired);
        // The standing is "possibly expired", never "the other side is gone".
        assert!(matches!(
            lease.takeover_by_timeout(),
            FacadeError::Authority(AuthorityError::TakeoverRefused)
        ));
    }

    #[test]
    fn a_reconnect_changes_the_epoch_and_invalidates_the_lease_binding() {
        let mut transport = FakeTransport::new();
        let mut lease = LeaseClock::new(60, ConnectionEpoch(1), 0);
        assert!(lease.observe(&transport).state_survives());
        let next = transport.reconnect().unwrap();
        let verdict = lease.observe(&transport);
        assert!(!verdict.state_survives());
        assert_eq!(
            verdict,
            EpochVerdict::Changed {
                established: ConnectionEpoch(1),
                current: next
            }
        );
        lease.rebind(next, 0);
        assert!(lease.observe(&transport).state_survives());
    }

    #[test]
    fn a_client_id_from_an_older_epoch_cannot_renew() {
        let mut transport = FakeTransport::new();
        let client = confirmed(&mut transport);
        let mut lease = LeaseClock::new(60, ConnectionEpoch(2), 0);
        let outcome = lease.renew(&mut transport, &client, 0, deadline());
        assert!(matches!(outcome, RenewOutcome::ClientLost(_)));
    }
}
