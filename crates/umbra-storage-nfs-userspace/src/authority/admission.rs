//! One-session-one-Umbra admission: acquiring, denying and cooperatively
//! releasing writer authority.
//!
//! # The rule
//!
//! A `Held` marker denies every acquirer. Not "denies until a lease elapses",
//! not "denies unless the token matches" — denies. Nothing in this module reads
//! a clock, and there is no code path from elapsed time to admission.
//!
//! The token-match case is worth stating because it looks like a safe exception
//! and is not. A restarted process presenting the token its predecessor wrote has
//! proven only that it can read a file. It has not proven the predecessor is
//! dead, and `docs/design/failure-model.md` is explicit that "opaque bytes alone
//! are not proof of termination" and that a crashed controller's marker is not
//! taken by M1 or M2. So a crashed owner's own restart is denied exactly like a
//! stranger's, the run stops as `BLOCKED_RECOVERABLE`, and the marker, the replay
//! data and the diagnostic are all retained for the authorised cooperative or
//! operator recovery the failure model defers this to.
//!
//! The one transition that *is* legitimate is a release the previous owner
//! actually recorded. That advances the epoch by exactly one.
//!
//! # Releasing
//!
//! A release that lets uncertain old I/O overlap a new owner is worse than no
//! release at all, so [`AdmissionControl::release`] demands
//! [`OutstandingIo::Excluded`]. [`OutstandingIo::Unknown`] hands the [`Admitted`]
//! proof straight back: the session keeps authority and stays blocked rather
//! than publishing a handover it cannot stand behind.

use umbra_core::{LeaseEpoch, RunId, TakeoverPolicy, WriterId};

use crate::error::{AuthorityError, FacadeError};

use super::marker::{
    AdmissionMarker, AdmissionPhase, ExclusiveCreate, MarkerError, MarkerStore, WriterToken,
};

/// What a session presents when asking for admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionRequest {
    /// Who is asking.
    pub writer: WriterId,
    /// The token this session would record in the marker.
    pub token: WriterToken,
    /// The storage contract's takeover policy. Only [`TakeoverPolicy::Refuse`]
    /// is supported in M1; the other two are answered, not ignored.
    pub takeover: TakeoverPolicy,
}

impl AdmissionRequest {
    /// A request that never asks for takeover — the only shape M1 admits.
    pub fn cooperative(writer: WriterId, token: WriterToken) -> Self {
        Self {
            writer,
            token,
            takeover: TakeoverPolicy::Refuse,
        }
    }
}

/// Proof that this session holds admission for one run at one epoch.
///
/// The constructor is private to this module, so an `Admitted` cannot be forged:
/// every value came from a marker this process actually created or legitimately
/// succeeded to. Anything requiring writer authority takes one of these rather
/// than a `bool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    run: RunId,
    writer: WriterId,
    token: WriterToken,
    epoch: LeaseEpoch,
}

impl Admitted {
    /// The run this admission is scoped to.
    pub fn run(&self) -> RunId {
        self.run
    }

    /// The admitted writer.
    pub fn writer(&self) -> &WriterId {
        &self.writer
    }

    /// The token recorded in the durable marker.
    pub fn token(&self) -> WriterToken {
        self.token
    }

    /// The epoch this admission was granted at.
    pub fn epoch(&self) -> LeaseEpoch {
        self.epoch
    }
}

/// What [`AdmissionControl::acquire`] settled.
#[derive(Debug)]
pub enum AdmissionOutcome {
    /// Admission granted. The marker durably records this session as the owner.
    Admitted(Admitted),
    /// Another session holds admission. No takeover was attempted.
    Denied {
        /// The recorded owner, absent when a legacy marker records only a token.
        holder: Option<WriterId>,
        /// The epoch the recorded owner holds.
        holder_epoch: LeaseEpoch,
        /// The verbatim authority failure, always [`AuthorityError::AdmissionRefused`].
        error: FacadeError,
    },
    /// Admission could not be decided: unreadable evidence, a regressed epoch, a
    /// takeover request, or a store failure. Every case is a safe stop.
    Refused(FacadeError),
}

impl AdmissionOutcome {
    /// The proof, when admission was granted.
    pub fn admitted(self) -> Option<Admitted> {
        match self {
            Self::Admitted(admitted) => Some(admitted),
            _ => None,
        }
    }

    /// The verbatim failure, when this outcome carries one.
    pub fn error(&self) -> Option<&FacadeError> {
        match self {
            Self::Admitted(_) => None,
            Self::Denied { error, .. } | Self::Refused(error) => Some(error),
        }
    }
}

/// Whether old I/O can be ruled out at release time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutstandingIo {
    /// Every call this session issued is proven withdrawn or settled.
    Excluded,
    /// Some call's disposition is unknown, so a new owner could overlap it.
    Unknown {
        /// What could not be ruled out, recorded verbatim.
        detail: String,
    },
}

/// What [`AdmissionControl::release`] settled.
#[derive(Debug)]
pub enum ReleaseOutcome {
    /// The marker durably records a cooperative release at this epoch.
    Released {
        /// The epoch the released ownership held.
        epoch: LeaseEpoch,
    },
    /// Release refused; this session still holds admission.
    ///
    /// Either outstanding I/O could not be excluded, or the release write
    /// failed. Both keep the marker readable as held, which is what stops a
    /// follow-on session from acquiring over uncertain state.
    Retained {
        /// The proof, handed back so the session can carry on or stop blocked.
        admitted: Box<Admitted>,
        /// The verbatim reason.
        error: FacadeError,
    },
}

/// Admission control over one run's durable marker.
///
/// The epoch ladder is kept here rather than read fresh each time: an epoch that
/// went *backwards* between two reads means the durable evidence was rewritten
/// underneath this process, which is a safe stop and not a retry.
#[derive(Debug)]
pub struct AdmissionControl<S: MarkerStore> {
    store: S,
    run: RunId,
    highest_epoch_seen: LeaseEpoch,
}

impl<S: MarkerStore> AdmissionControl<S> {
    /// Admission control for `run` over `store`.
    pub fn new(run: RunId, store: S) -> Self {
        Self {
            store,
            run,
            highest_epoch_seen: LeaseEpoch(0),
        }
    }

    /// The highest epoch this control has ever observed in the marker.
    pub fn highest_epoch_seen(&self) -> LeaseEpoch {
        self.highest_epoch_seen
    }

    /// The underlying store, for a caller inspecting durable evidence.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The underlying store, mutably.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Read the marker as it currently stands, without changing it.
    pub fn inspect(&mut self) -> Result<Option<AdmissionMarker>, FacadeError> {
        match self.store.read()? {
            None => Ok(None),
            Some(bytes) => AdmissionMarker::decode(&bytes)
                .map(Some)
                .map_err(|error| error.to_facade()),
        }
    }

    /// Ask for admission.
    ///
    /// No clock is consulted anywhere in this call. The only way to be admitted
    /// over an existing marker is for that marker to record a release the
    /// previous owner performed.
    pub fn acquire(&mut self, request: &AdmissionRequest) -> AdmissionOutcome {
        if let Some(refusal) = unsupported_takeover(&request.takeover) {
            return AdmissionOutcome::Refused(refusal);
        }

        let fresh = AdmissionMarker::new(
            request.token,
            request.writer.clone(),
            LeaseEpoch(1),
            AdmissionPhase::Held,
        );
        let encoded = match fresh.encode() {
            Ok(bytes) => bytes,
            Err(error) => return AdmissionOutcome::Refused(error.to_facade()),
        };
        match self.store.create_exclusive(&encoded) {
            Err(error) => return AdmissionOutcome::Refused(error),
            Ok(ExclusiveCreate::Created) => {
                self.highest_epoch_seen = LeaseEpoch(1);
                return AdmissionOutcome::Admitted(Admitted {
                    run: self.run,
                    writer: request.writer.clone(),
                    token: request.token,
                    epoch: LeaseEpoch(1),
                });
            }
            Ok(ExclusiveCreate::Exists) => {}
        }

        let existing = match self.inspect() {
            Err(error) => return AdmissionOutcome::Refused(error),
            // The create said the name exists, so a read finding nothing means
            // the evidence changed underneath this call. That is a contradiction,
            // not an invitation to retry the create.
            Ok(None) => {
                return AdmissionOutcome::Refused(FacadeError::Authority(
                    AuthorityError::IdentityUnproven(
                        "marker existed for the exclusive create and was absent on read".into(),
                    ),
                ))
            }
            Ok(Some(marker)) => marker,
        };

        if existing.epoch().0 < self.highest_epoch_seen.0 {
            return AdmissionOutcome::Refused(
                MarkerError::EpochRegressed {
                    seen: self.highest_epoch_seen,
                    observed: existing.epoch(),
                }
                .to_facade(),
            );
        }
        self.highest_epoch_seen = existing.epoch();

        match existing.phase() {
            AdmissionPhase::Held => AdmissionOutcome::Denied {
                holder: existing.writer().cloned(),
                holder_epoch: existing.epoch(),
                error: FacadeError::Authority(AuthorityError::AdmissionRefused(format!(
                    "run {} is held by {} at {:?}; a held marker is never taken over on a timeout",
                    self.run.0,
                    existing
                        .writer()
                        .map_or_else(|| "an unnamed legacy writer".to_owned(), |id| id.0.clone()),
                    existing.epoch(),
                ))),
            },
            AdmissionPhase::Released => {
                let Some(next) = existing.epoch().0.checked_add(1) else {
                    return AdmissionOutcome::Refused(FacadeError::Authority(
                        AuthorityError::IdentityUnproven(
                            "admission epoch would overflow; refusing rather than wrapping".into(),
                        ),
                    ));
                };
                let epoch = LeaseEpoch(next);
                let claimed = AdmissionMarker::new(
                    request.token,
                    request.writer.clone(),
                    epoch,
                    AdmissionPhase::Held,
                );
                let encoded = match claimed.encode() {
                    Ok(bytes) => bytes,
                    Err(error) => return AdmissionOutcome::Refused(error.to_facade()),
                };
                if let Err(error) = self.store.overwrite(&encoded) {
                    return AdmissionOutcome::Refused(error);
                }
                self.highest_epoch_seen = epoch;
                AdmissionOutcome::Admitted(Admitted {
                    run: self.run,
                    writer: request.writer.clone(),
                    token: request.token,
                    epoch,
                })
            }
        }
    }

    /// Release admission cooperatively on a graceful shutdown.
    ///
    /// Consumes the proof: after a successful release this session has no value
    /// that could authorise a further mutation.
    pub fn release(&mut self, admitted: Admitted, outstanding: OutstandingIo) -> ReleaseOutcome {
        if let OutstandingIo::Unknown { detail } = &outstanding {
            return ReleaseOutcome::Retained {
                error: FacadeError::Authority(AuthorityError::AdmissionRefused(format!(
                    "release withheld: outstanding I/O could not be excluded ({detail})"
                ))),
                admitted: Box::new(admitted),
            };
        }
        let released = AdmissionMarker::new(
            admitted.token,
            admitted.writer.clone(),
            admitted.epoch,
            AdmissionPhase::Released,
        );
        let encoded = match released.encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                return ReleaseOutcome::Retained {
                    admitted: Box::new(admitted),
                    error: error.to_facade(),
                }
            }
        };
        match self.store.overwrite(&encoded) {
            Ok(()) => ReleaseOutcome::Released {
                epoch: admitted.epoch,
            },
            Err(error) => ReleaseOutcome::Retained {
                admitted: Box::new(admitted),
                error,
            },
        }
    }

    /// Refuse a takeover. Always.
    ///
    /// Mirrors `state::lease::LeaseClock::takeover_by_timeout` and
    /// `state::ProtocolState::takeover`, so a caller reaching for takeover from
    /// any of the three layers finds the same refusal rather than a gap in one of
    /// them.
    pub fn takeover(&self) -> FacadeError {
        FacadeError::Authority(AuthorityError::TakeoverRefused)
    }
}

/// Why a takeover policy other than [`TakeoverPolicy::Refuse`] cannot be served.
fn unsupported_takeover(policy: &TakeoverPolicy) -> Option<FacadeError> {
    match policy {
        TakeoverPolicy::Refuse => None,
        // The storage contract's own documentation says opaque bytes are not
        // proof of termination, and M1 has no independent way to verify any.
        TakeoverPolicy::ConfirmedTermination { .. } => {
            Some(FacadeError::Authority(AuthorityError::TakeoverRefused))
        }
        // Fencing is M3. Refusing here is what keeps the deferral honest.
        TakeoverPolicy::FencePreviousWriter => {
            Some(FacadeError::Authority(AuthorityError::TakeoverRefused))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::marker::MemoryMarkerStore;
    use crate::error::ErrorClass;
    use uuid::Uuid;

    fn run() -> RunId {
        RunId(Uuid::from_u128(0x5150))
    }

    fn request(label: &str, token: u8) -> AdmissionRequest {
        AdmissionRequest::cooperative(WriterId(label.into()), WriterToken([token; 16]))
    }

    #[test]
    fn the_first_acquirer_is_admitted_at_epoch_one() {
        let mut control = AdmissionControl::new(run(), MemoryMarkerStore::empty());
        let admitted = control
            .acquire(&request("a", 1))
            .admitted()
            .expect("first acquirer wins");
        assert_eq!(admitted.epoch(), LeaseEpoch(1));
        assert_eq!(admitted.run(), run());
    }

    #[test]
    fn a_held_marker_denies_even_the_same_token() {
        let mut control = AdmissionControl::new(run(), MemoryMarkerStore::empty());
        control.acquire(&request("a", 1)).admitted().expect("first");
        // Same writer, same token: a restarted process claiming its predecessor's
        // identity proves nothing about the predecessor's termination.
        let outcome = control.acquire(&request("a", 1));
        let AdmissionOutcome::Denied { holder_epoch, .. } = &outcome else {
            panic!("expected denial, got {outcome:?}");
        };
        assert_eq!(*holder_epoch, LeaseEpoch(1));
        assert_eq!(
            outcome.error().expect("denial carries an error").class(),
            ErrorClass::SafeStop
        );
    }

    #[test]
    fn a_cooperative_release_lets_the_next_session_in_at_the_next_epoch() {
        let mut store = MemoryMarkerStore::empty();
        // The first control goes out of scope entirely, the way a process that
        // shut down gracefully does: everything the next one concludes has to
        // come from the durable marker, not from a ladder it inherited.
        {
            let mut first = AdmissionControl::new(run(), &mut store);
            let admitted = first.acquire(&request("a", 1)).admitted().expect("first");
            assert!(matches!(
                first.release(admitted, OutstandingIo::Excluded),
                ReleaseOutcome::Released {
                    epoch: LeaseEpoch(1)
                }
            ));
        }

        let mut second = AdmissionControl::new(run(), &mut store);
        let next = second
            .acquire(&request("b", 2))
            .admitted()
            .expect("released marker admits the follow-on session");
        assert_eq!(next.epoch(), LeaseEpoch(2), "epoch advances by exactly one");
    }

    #[test]
    fn release_is_withheld_while_old_io_cannot_be_excluded() {
        let mut control = AdmissionControl::new(run(), MemoryMarkerStore::empty());
        let admitted = control.acquire(&request("a", 1)).admitted().expect("first");
        let outcome = control.release(
            admitted,
            OutstandingIo::Unknown {
                detail: "one WRITE reply never arrived".into(),
            },
        );
        let ReleaseOutcome::Retained { admitted, .. } = outcome else {
            panic!("expected the release to be withheld");
        };
        assert_eq!(admitted.epoch(), LeaseEpoch(1));
        assert_eq!(
            control
                .inspect()
                .expect("read")
                .expect("marker present")
                .phase(),
            AdmissionPhase::Held,
            "the marker must still read as held"
        );
    }

    #[test]
    fn every_takeover_policy_other_than_refuse_is_refused() {
        for policy in [
            TakeoverPolicy::ConfirmedTermination {
                evidence: b"pid 4242 exited".to_vec(),
            },
            TakeoverPolicy::FencePreviousWriter,
        ] {
            let mut control = AdmissionControl::new(run(), MemoryMarkerStore::empty());
            let outcome = control.acquire(&AdmissionRequest {
                takeover: policy.clone(),
                ..request("a", 1)
            });
            assert!(
                matches!(
                    outcome,
                    AdmissionOutcome::Refused(FacadeError::Authority(
                        AuthorityError::TakeoverRefused
                    ))
                ),
                "{policy:?} must be refused, got {outcome:?}"
            );
            assert_eq!(
                control.inspect().expect("read"),
                None,
                "a refused takeover must not create a marker"
            );
        }
    }

    #[test]
    fn a_regressed_epoch_is_a_safe_stop_not_a_retry() {
        let mut store = MemoryMarkerStore::empty();
        let mut control = AdmissionControl::new(run(), &mut store);
        // Climb the ladder to epoch 3 through three legitimate transitions.
        for (index, (label, token)) in [("a", 1u8), ("b", 2), ("c", 3)].into_iter().enumerate() {
            let admitted = control
                .acquire(&request(label, token))
                .admitted()
                .unwrap_or_else(|| panic!("acquire {label}"));
            assert_eq!(admitted.epoch(), LeaseEpoch(index as u64 + 1));
            control.release(admitted, OutstandingIo::Excluded);
        }
        assert_eq!(control.highest_epoch_seen(), LeaseEpoch(3));

        // An external actor restores an older marker underneath the process.
        let stale = AdmissionMarker::new(
            WriterToken([9; 16]),
            WriterId("stale".into()),
            LeaseEpoch(1),
            AdmissionPhase::Released,
        );
        control
            .store_mut()
            .overwrite(&stale.encode().expect("encode"))
            .expect("rewrite the durable evidence");

        let outcome = control.acquire(&request("d", 4));
        assert!(
            matches!(
                outcome,
                AdmissionOutcome::Refused(FacadeError::Authority(
                    AuthorityError::IdentityUnproven(_)
                ))
            ),
            "a marker that went backwards must stop, not admit: {outcome:?}"
        );
    }

    #[test]
    fn a_store_failure_refuses_rather_than_admitting() {
        let mut store = MemoryMarkerStore::empty();
        store.fail_next(FacadeError::Transport(
            crate::error::TransportError::Connect("no route".into()),
        ));
        let mut control = AdmissionControl::new(run(), store);
        let outcome = control.acquire(&request("a", 1));
        assert!(
            matches!(outcome, AdmissionOutcome::Refused(_)),
            "{outcome:?}"
        );
    }
}
