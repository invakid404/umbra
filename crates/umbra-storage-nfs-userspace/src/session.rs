//! Product admission for one provider session: one-session-one-Umbra, wired.
//!
//! `authority_recovery` built [`AdmissionControl`] over the frozen facades and
//! left binding it to a provider as `m1_integrate`'s seam. This is that binding.
//!
//! # The rule this module keeps
//!
//! A run cannot open without admission. Not "acquires a lease it may lose", not
//! "acquires one if a writer intent was declared" — [`Session::admit`] runs
//! before [`NfsUserspaceStorage::open_run`](crate::storage::NfsUserspaceStorage)
//! publishes any binding, and a denial means no binding exists to use. The
//! marker is the source of truth, it lives on the server, and the only thing
//! that ever admits a second session is a release the first one recorded.
//!
//! Nothing here consults a clock. There is no code path from elapsed time to
//! admission, and a restarted process presenting its predecessor's own token is
//! denied exactly like a stranger — that rule is `authority_recovery`'s and this
//! module does not soften it.

use umbra_core::{ErrorKind, LeaseEpoch, Result, RunId, UmbraError, WriterId, WriterLease};

use crate::anchor::{component, RunAnchors};
use crate::authority::admission::{
    AdmissionControl, AdmissionOutcome, AdmissionRequest, Admitted, OutstandingIo, ReleaseOutcome,
};
use crate::authority::marker::{AdmissionPhase, WriterToken};
use crate::authority::server_marker::ServerMarkerStore;
use crate::state::ProtocolState;
use crate::transport::{Deadline, RawTransport};

/// Name of the durable admission marker inside the run's `.provider` directory.
///
/// Byte-identical to the mounted adapter's, because a run written by one and
/// opened by the other must read the same marker rather than each keeping a
/// private one that the other cannot see.
pub const MARKER_NAME: &[u8] = b"writer.lock";

/// How long a caller should wait before renewing.
///
/// Renewal keeps the session's own liveness honest. It is emphatically **not**
/// permission for another writer to take over when it lapses: nothing in this
/// crate reads a missed renewal as a release.
pub const RENEW_AFTER_MILLIS: u64 = 30_000;

/// The admission a provider session holds while a run is open.
#[derive(Debug)]
pub struct Session {
    admitted: Admitted,
}

impl Session {
    /// Acquire admission for `run` over the run's durable marker.
    ///
    /// Fails, rather than opening read-only or waiting, when another session
    /// holds it. The caller must not publish a run binding on a failure.
    pub fn admit(
        transport: &mut dyn RawTransport,
        state: &mut ProtocolState,
        anchors: &RunAnchors,
        run: RunId,
        writer: WriterId,
        token: WriterToken,
        deadline: Deadline,
    ) -> Result<Self> {
        let private = anchors.private().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                "open_run",
                "the run has no .provider directory, so its admission marker cannot be \
                 read; opening without one would be an unadmitted session",
            )
        })?;
        let parent = private.pin().handle().clone();
        let name = component(MARKER_NAME.to_vec())?;
        let owners = state
            .incarnation()
            .ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    "open_run",
                    "admission needs open owners from a confirmed client incarnation",
                )
            })?
            .open_owners();

        let store = ServerMarkerStore::new(transport, owners, parent, name, deadline);
        let mut control = AdmissionControl::new(run, store);
        match control.acquire(&AdmissionRequest::cooperative(writer, token)) {
            AdmissionOutcome::Admitted(admitted) => Ok(Self { admitted }),
            AdmissionOutcome::Denied {
                holder,
                holder_epoch,
                error,
            } => Err(denied(holder, holder_epoch, error)),
            AdmissionOutcome::Refused(error) => Err(error.to_umbra("open_run")),
        }
    }

    /// The proof this session holds.
    pub fn admitted(&self) -> &Admitted {
        &self.admitted
    }

    /// The contract lease this admission corresponds to.
    ///
    /// The renewal token is the marker token, so a renewal proves the caller is
    /// the writer the marker records rather than merely holding a lease struct.
    pub fn lease(&self) -> WriterLease {
        WriterLease {
            run_id: self.admitted.run(),
            writer_id: self.admitted.writer().clone(),
            epoch: self.admitted.epoch(),
            renewal_token: self.admitted.token().as_bytes().to_vec(),
            renew_after_millis: RENEW_AFTER_MILLIS,
        }
    }

    /// Whether `lease` is the one this session actually holds.
    ///
    /// Separate from [`Self::renew`] because the two failures are different
    /// facts: a lease this session never issued is a bad *argument*, while a
    /// marker that no longer records this session is lost *authority*. Only the
    /// second is terminal, so a caller latching authority loss (**R1-002**) has
    /// to be able to tell them apart before it renews.
    pub fn owns_lease(&self, lease: &WriterLease) -> bool {
        lease.run_id == self.admitted.run()
            && lease.writer_id == *self.admitted.writer()
            && lease.epoch == self.admitted.epoch()
            && lease.renewal_token == self.admitted.token().as_bytes()
    }

    /// Re-prove that the durable marker still records this session.
    ///
    /// Reads the marker and compares it against the held proof. It never writes,
    /// never advances the epoch and never consults a clock, so a renewal cannot
    /// manufacture authority — it can only confirm or refuse the authority the
    /// marker already records.
    pub fn renew(
        &self,
        transport: &mut dyn RawTransport,
        state: &mut ProtocolState,
        anchors: &RunAnchors,
        lease: &WriterLease,
        deadline: Deadline,
    ) -> Result<WriterLease> {
        if !self.owns_lease(lease) {
            return Err(UmbraError::new(
                ErrorKind::LeaseLost,
                "renew_writer",
                "the presented lease is not the one this session holds",
            ));
        }

        let mut control = self.control(transport, state, anchors, "renew_writer", deadline)?;
        let marker = control
            .inspect()
            .map_err(|error| error.to_umbra("renew_writer"))?
            .ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::LeaseLost,
                    "renew_writer",
                    "the admission marker is gone; this session can no longer prove it is \
                     the writer",
                )
            })?;

        if marker.token() != self.admitted.token()
            || marker.epoch() != self.admitted.epoch()
            || marker.phase() != AdmissionPhase::Held
        {
            return Err(UmbraError::new(
                ErrorKind::LeaseLost,
                "renew_writer",
                format!(
                    "the marker now records epoch {} in phase {:?}; this session holds epoch {}",
                    marker.epoch().0,
                    marker.phase(),
                    self.admitted.epoch().0
                ),
            ));
        }
        Ok(self.lease())
    }

    /// Release admission cooperatively, advancing the epoch by exactly one.
    ///
    /// `outstanding` is the caller's honest statement about its own in-flight
    /// work. `Unknown` keeps the marker held, because a follow-on session that
    /// acquired over unsettled I/O could overlap it.
    pub fn release(
        self,
        transport: &mut dyn RawTransport,
        state: &mut ProtocolState,
        anchors: &RunAnchors,
        outstanding: OutstandingIo,
        operation: &str,
        deadline: Deadline,
    ) -> SessionRelease {
        let mut control = match self.control(transport, state, anchors, operation, deadline) {
            Ok(control) => control,
            // Addressing the marker failed before anything was written, so the
            // marker is untouched and this session provably still holds it.
            Err(error) => {
                return SessionRelease::Retained {
                    session: self,
                    error,
                }
            }
        };
        match control.release(self.admitted.clone(), outstanding) {
            ReleaseOutcome::Released { epoch } => SessionRelease::Released(epoch),
            ReleaseOutcome::Retained { admitted, error } => SessionRelease::Retained {
                session: Self {
                    admitted: *admitted,
                },
                error: error.to_umbra(operation),
            },
            // R1-002: the proof is deliberately *not* handed back. A release whose
            // effect cannot be established may already have let a successor in, so
            // this session stops rather than resuming authority over it.
            ReleaseOutcome::Uncertain { error } => {
                SessionRelease::Uncertain(error.to_umbra(operation))
            }
        }
    }

    fn control<'t>(
        &self,
        transport: &'t mut dyn RawTransport,
        state: &'t mut ProtocolState,
        anchors: &RunAnchors,
        operation: &str,
        deadline: Deadline,
    ) -> Result<AdmissionControl<ServerMarkerStore<'t>>> {
        let private = anchors.private().ok_or_else(|| {
            UmbraError::new(
                ErrorKind::InvalidState,
                operation,
                "the run has no .provider directory to read its admission marker from",
            )
        })?;
        let parent = private.pin().handle().clone();
        let name = component(MARKER_NAME.to_vec())?;
        let owners = state
            .incarnation()
            .ok_or_else(|| {
                UmbraError::new(
                    ErrorKind::InvalidState,
                    operation,
                    "the client incarnation is gone; admission cannot be re-proven",
                )
            })?
            .open_owners();
        Ok(AdmissionControl::new(
            self.admitted.run(),
            ServerMarkerStore::new(transport, owners, parent, name, deadline),
        ))
    }
}

/// What [`Session::release`] settled.
///
/// Three outcomes and not two, because "the release write failed" and "the
/// release may or may not have happened" are different facts with different safe
/// answers. Only [`Self::Retained`] carries a usable session back (**R1-002**).
#[derive(Debug)]
pub enum SessionRelease {
    /// The marker durably records the cooperative release.
    Released(LeaseEpoch),
    /// The release provably did not happen; this session still holds admission.
    Retained {
        /// The session, still admitted.
        session: Session,
        /// Why the release was refused.
        error: UmbraError,
    },
    /// The release could not be proven either way. Admission is consumed and the
    /// caller must stop; reconciliation is an operator action, not a retry.
    Uncertain(UmbraError),
}

/// A denial names the session that actually holds the run.
///
/// Reported as [`ErrorKind::LeaseLost`] rather than a generic failure so a caller
/// can tell "someone else owns this run" from "the server is unreachable", and
/// the message carries the holder because a denial that will not say who holds it
/// is unactionable.
fn denied(
    holder: Option<WriterId>,
    holder_epoch: LeaseEpoch,
    error: crate::error::FacadeError,
) -> UmbraError {
    let who = holder.map_or_else(
        || "an unnamed writer (the marker carries only a legacy token)".to_owned(),
        |writer| format!("writer {writer:?}"),
    );
    UmbraError::new(
        ErrorKind::LeaseLost,
        "open_run",
        format!(
            "one-session-one-Umbra: this run is held by {who} at epoch {}; \
             admission is granted only by a release the holder records, never by elapsed \
             time ({error})",
            holder_epoch.0
        ),
    )
}
