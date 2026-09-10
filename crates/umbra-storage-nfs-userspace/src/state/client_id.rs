//! Client-id lifecycle: `SETCLIENTID` then `SETCLIENTID_CONFIRM`, and nothing else.
//!
//! # The lifecycle is a type, not a flag
//!
//! [`ClientIdentity`] is the stable half Umbra owns: the `nfs_client_id4` string
//! and the client's boot verifier. Sending `SETCLIENTID` yields a
//! [`PendingClient`], and the only way to obtain a [`ConfirmedClient`] is to hand
//! a `PendingClient` to a `SETCLIENTID_CONFIRM` that the server accepted. There
//! is no constructor that skips the confirm and no boolean anyone can set, so
//! "renew a client id the server never confirmed" is unrepresentable rather than
//! merely wrong.
//!
//! # The retained client-id verifier
//!
//! NFSv4.0 gives the verifier two distinct jobs and this module keeps them apart.
//!
//! * The **boot verifier** the client sends in `SETCLIENTID` is retained across
//!   reconnects for as long as this process incarnation lives. Presenting the
//!   same `(id, verifier)` pair tells the server "same client, my state should
//!   still be there", which is the precondition for a `CLAIM_PREVIOUS` reclaim
//!   being meaningful at all. Presenting a *different* verifier tells the server
//!   the client restarted and its previous state may be released, so
//!   [`ClientIdentity::restarted`] is the only way to change it and it says in
//!   its name that prior state is being surrendered.
//! * The **confirm verifier** the server returns from `SETCLIENTID` is echoed
//!   verbatim in `SETCLIENTID_CONFIRM`. It is retained in [`PendingClient`] and
//!   nowhere else, because it is meaningless after the confirm.
//!
//! # What this module refuses to do
//!
//! Nothing here treats silence, a lease timeout, or `NFS4ERR_CLID_INUSE` as
//! permission to seize another client's state. `NFS4ERR_CLID_INUSE` is reported
//! as a protocol failure with its verbatim status; the decision about competing
//! writers belongs to `authority_recovery`, not to the protocol-state owner.

use crate::error::{FacadeError, Nfs4Status};
use crate::handle::{ClientId, Session, SessionId};
use crate::transport::{
    CallbackPolicy, ConnectionEpoch, Deadline, RawTransport, SetClientIdArgs, Verifier,
};

/// The stable client identity Umbra owns across reconnects.
///
/// libnfs supplies raw RPC only; the identity string and its verifier are the
/// protocol-state owner's, which is why they are constructed here rather than
/// borrowed from a transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    id: Vec<u8>,
    boot_verifier: Verifier,
    incarnation: u64,
}

impl ClientIdentity {
    /// Build an identity from an `nfs_client_id4` string and a boot verifier.
    ///
    /// The bytes are recorded verbatim; no UTF-8 is assumed and none is imposed.
    pub fn new(id: impl Into<Vec<u8>>, boot_verifier: Verifier) -> Self {
        Self {
            id: id.into(),
            boot_verifier,
            incarnation: 1,
        }
    }

    /// The `nfs_client_id4` bytes.
    pub fn id(&self) -> &[u8] {
        &self.id
    }

    /// The retained boot verifier. Unchanged by reconnect.
    pub fn boot_verifier(&self) -> Verifier {
        self.boot_verifier
    }

    /// Which incarnation of this client string is current.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Declare that the client restarted, surrendering any state the server still
    /// holds for the previous incarnation.
    ///
    /// This is the *only* way the boot verifier changes, and it is deliberately
    /// verbose: a caller reaching for it is telling the server to release state
    /// that a `CLAIM_PREVIOUS` reclaim could otherwise have recovered.
    pub fn restarted(&self, boot_verifier: Verifier) -> Self {
        Self {
            id: self.id.clone(),
            boot_verifier,
            incarnation: self.incarnation.saturating_add(1),
        }
    }

    /// Send `SETCLIENTID`, producing an unconfirmed incarnation.
    ///
    /// `CallbackPolicy::Declined` is not a parameter. M1 runs no callback server,
    /// so advertising one would invite a delegation this client cannot honour.
    pub fn set_client_id(
        &self,
        transport: &mut dyn RawTransport,
        epoch: ConnectionEpoch,
        deadline: Deadline,
    ) -> Result<PendingClient, FacadeError> {
        let reply = transport.set_client_id(
            SetClientIdArgs {
                verifier: self.boot_verifier,
                id: self.id.clone(),
                callback: CallbackPolicy::Declined,
            },
            deadline,
        )?;
        Ok(PendingClient {
            identity: self.clone(),
            client_id: reply.client_id,
            confirm: reply.confirm,
            epoch,
        })
    }
}

/// A client incarnation the server acknowledged but has not confirmed.
///
/// Holds the server's confirm verifier so `SETCLIENTID_CONFIRM` echoes exactly
/// what `SETCLIENTID` returned. No stateful operation accepts this type.
#[must_use = "an unconfirmed client id grants nothing until SETCLIENTID_CONFIRM succeeds"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingClient {
    identity: ClientIdentity,
    client_id: ClientId,
    confirm: Verifier,
    epoch: ConnectionEpoch,
}

impl PendingClient {
    /// The id the server assigned. Usable for diagnostics only until confirmed.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// The confirm verifier that must be echoed back verbatim.
    pub fn confirm_verifier(&self) -> Verifier {
        self.confirm
    }

    /// The identity this incarnation was established for.
    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    /// Send `SETCLIENTID_CONFIRM`.
    ///
    /// Consumes the pending incarnation so a failed confirm cannot be quietly
    /// retried against a stale confirm verifier; the outcome says which pending
    /// value, if any, is still good.
    pub fn confirm(self, transport: &mut dyn RawTransport, deadline: Deadline) -> ConfirmOutcome {
        match transport.set_client_id_confirm(self.client_id, self.confirm, deadline) {
            Ok(()) => ConfirmOutcome::Confirmed(ConfirmedClient {
                identity: self.identity,
                client_id: self.client_id,
                epoch: self.epoch,
            }),
            Err(error) => match error.status() {
                // The server forgot the pending record, so the whole SETCLIENTID
                // must be redone. Retrying the confirm alone cannot help.
                Some(Nfs4Status::STALE_CLIENTID) => ConfirmOutcome::Restart(error),
                // Another client holds this id string with a different verifier.
                // This is reported, never resolved by seizing the id.
                Some(Nfs4Status::CLID_INUSE) => ConfirmOutcome::InUse(error),
                Some(_) => ConfirmOutcome::Retry {
                    pending: self,
                    error,
                },
                // A lost reply leaves the server's view unknown. Nothing is
                // confirmed and nothing is assumed; SETCLIENTID is redone, which
                // is idempotent for an unconfirmed record.
                None => ConfirmOutcome::Indeterminate(error),
            },
        }
    }
}

/// What `SETCLIENTID_CONFIRM` settled.
#[derive(Debug)]
pub enum ConfirmOutcome {
    /// The incarnation is confirmed and may hold state.
    Confirmed(ConfirmedClient),
    /// The server rejected the confirm but the pending record is still valid;
    /// the same confirm verifier may be presented again.
    Retry {
        /// The still-valid pending incarnation.
        pending: PendingClient,
        /// The verbatim server failure.
        error: FacadeError,
    },
    /// The pending record is gone. Start again from `SETCLIENTID`.
    Restart(FacadeError),
    /// `NFS4ERR_CLID_INUSE`: another client holds this identity string.
    ///
    /// This is surfaced, not resolved. Deciding between competing writers is
    /// `authority_recovery`'s admission problem and never a protocol-state one.
    InUse(FacadeError),
    /// The outcome is unknown, typically a lost reply. Nothing advanced.
    Indeterminate(FacadeError),
}

impl ConfirmOutcome {
    /// The confirmed client, if the confirm actually succeeded.
    pub fn confirmed(self) -> Option<ConfirmedClient> {
        match self {
            Self::Confirmed(client) => Some(client),
            _ => None,
        }
    }

    /// The verbatim failure, if this outcome carries one.
    pub fn error(&self) -> Option<&FacadeError> {
        match self {
            Self::Confirmed(_) => None,
            Self::Retry { error, .. }
            | Self::Restart(error)
            | Self::InUse(error)
            | Self::Indeterminate(error) => Some(error),
        }
    }
}

/// A confirmed client incarnation: the only thing that may hold NFS state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedClient {
    identity: ClientIdentity,
    client_id: ClientId,
    epoch: ConnectionEpoch,
}

impl ConfirmedClient {
    /// The confirmed client id.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// The identity this incarnation confirmed.
    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    /// The connection generation this id was confirmed on.
    pub fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// Whether this client id is still valid for `epoch`.
    ///
    /// A client id confirmed on an older connection has not been proven on the
    /// current one. The transport contract is explicit that reconnecting does not
    /// revalidate protocol state, so this is checked rather than assumed.
    pub fn is_valid_for(&self, epoch: ConnectionEpoch) -> bool {
        self.epoch == epoch
    }

    /// Mint a facade session for this confirmed id.
    ///
    /// A session is the only place filehandles, open owners and open state are
    /// created, so the handle facade cannot mint anything for an unconfirmed id.
    pub fn session(&self, session_id: SessionId) -> Session {
        Session::establish(session_id, self.client_id, self.epoch)
    }

    /// Surrender this confirmed id because the connection generation changed.
    ///
    /// Returns the retained identity — same string, same boot verifier — so the
    /// re-established client is recognisably the *same* client and its state is
    /// reclaimable. Nothing about this is a takeover: it re-proves an identity
    /// this process already held.
    pub fn invalidated_by_reconnect(self) -> ClientIdentity {
        self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeTransport;
    use crate::transport::{ConnectionState, FaultAction, FaultPoint};

    fn identity() -> ClientIdentity {
        ClientIdentity::new(b"umbra-m1-protocol-state".to_vec(), Verifier([0x5A; 8]))
    }

    fn deadline() -> Deadline {
        Deadline { millis: 1_000 }
    }

    fn epoch_of(transport: &FakeTransport) -> ConnectionEpoch {
        match transport.connection() {
            ConnectionState::Connected(epoch) | ConnectionState::Broken(epoch) => epoch,
            ConnectionState::Idle => ConnectionEpoch(0),
        }
    }

    #[test]
    fn a_client_id_is_only_usable_after_the_confirm_succeeds() {
        let mut transport = FakeTransport::new();
        let epoch = epoch_of(&transport);
        let pending = identity()
            .set_client_id(&mut transport, epoch, deadline())
            .unwrap();
        // A RENEW for an unconfirmed id is refused by the server, which is what
        // makes the typestate the honest model rather than an extra guard.
        assert_eq!(
            transport
                .renew(pending.client_id(), deadline())
                .unwrap_err()
                .status(),
            Some(Nfs4Status::STALE_CLIENTID)
        );
        let confirmed = pending.confirm(&mut transport, deadline()).confirmed();
        let confirmed = confirmed.expect("the fake confirms a fresh incarnation");
        assert!(transport.renew(confirmed.client_id(), deadline()).is_ok());
    }

    #[test]
    fn the_boot_verifier_is_retained_across_reconnects_and_only_a_restart_changes_it() {
        let first = identity();
        let reconnected = first.clone();
        assert_eq!(first.boot_verifier(), reconnected.boot_verifier());
        assert_eq!(first.incarnation(), reconnected.incarnation());

        let restarted = first.restarted(Verifier([0x77; 8]));
        assert_eq!(restarted.id(), first.id(), "the id string is stable");
        assert_ne!(restarted.boot_verifier(), first.boot_verifier());
        assert_eq!(restarted.incarnation(), 2);
    }

    #[test]
    fn a_dropped_confirm_reply_confirms_nothing() {
        let mut transport = FakeTransport::new();
        let epoch = epoch_of(&transport);
        let pending = identity()
            .set_client_id(&mut transport, epoch, deadline())
            .unwrap();
        let client_id = pending.client_id();
        transport.install_faults(crate::fake::ScriptedFault::once(
            FaultPoint::OnDeadline,
            None,
            FaultAction::DropReply,
        ));
        let outcome = pending.confirm(&mut transport, deadline());
        assert!(matches!(outcome, ConfirmOutcome::Indeterminate(_)));
        assert!(outcome.confirmed().is_none());
        // The server never saw the confirm, so the id still holds no state.
        assert_eq!(
            transport.renew(client_id, deadline()).unwrap_err().status(),
            Some(Nfs4Status::STALE_CLIENTID)
        );
    }

    #[test]
    fn clid_in_use_is_reported_and_never_resolved_by_seizing_the_id() {
        let mut transport = FakeTransport::new();
        let epoch = epoch_of(&transport);
        let pending = identity()
            .set_client_id(&mut transport, epoch, deadline())
            .unwrap();
        transport.install_faults(crate::fake::ScriptedFault::once(
            FaultPoint::AfterDispatch,
            Some(crate::transport::OpCode::SetClientIdConfirm),
            FaultAction::Substitute(Nfs4Status::CLID_INUSE),
        ));
        let outcome = pending.confirm(&mut transport, deadline());
        assert!(matches!(outcome, ConfirmOutcome::InUse(_)));
        assert_eq!(
            outcome.error().and_then(FacadeError::status),
            Some(Nfs4Status::CLID_INUSE),
            "the verbatim NFS4ERR_CLID_INUSE reaches the caller"
        );
    }

    #[test]
    fn a_confirmed_id_from_an_older_connection_is_not_valid_on_a_new_one() {
        let mut transport = FakeTransport::new();
        let epoch = epoch_of(&transport);
        let confirmed = identity()
            .set_client_id(&mut transport, epoch, deadline())
            .unwrap()
            .confirm(&mut transport, deadline())
            .confirmed()
            .unwrap();
        let next = transport.reconnect().unwrap();
        assert!(!confirmed.is_valid_for(next));
        let retained = confirmed.invalidated_by_reconnect();
        assert_eq!(
            retained.boot_verifier(),
            identity().boot_verifier(),
            "re-establishment presents the same client, not a new one"
        );
    }
}
