//! Grace handling and NFSv4.0 `CLAIM_PREVIOUS` reclaim.
//!
//! # v4.0 only, and that is structural
//!
//! NFSv4.1 ends a reclaim with `RECLAIM_COMPLETE` (RFC 8881 §1.9). NFSv4.0 has no
//! such operation: a client reclaims each open with `CLAIM_PREVIOUS` and the
//! server's grace period ends on its own schedule. This module implements the
//! v4.0 shape and nothing else. It could not implement the v4.1 shape if it tried
//! — [`OpCode`](crate::transport::OpCode) has no variant at or above 40, so
//! `RECLAIM_COMPLETE` is unrepresentable rather than merely unused.
//!
//! # Only what CLAIM_PREVIOUS can actually recover
//!
//! A reclaim re-establishes an open the *same client* held before the
//! interruption. Two consequences are enforced here rather than hoped for:
//!
//! * Only a **confirmed** open is reclaimable. An open whose OPEN_CONFIRM never
//!   landed, or whose owner was poisoned by a lost reply, was never proven to
//!   exist server-side; asking for it back would be asserting something unknown.
//!   Those go straight to a surrender.
//! * The reclaim presents the **object's own filehandle** as the current
//!   filehandle, and its identity is re-proven afterwards. A server that answers
//!   with a different `(fsid, fileid)` has handed back a different object, and
//!   that is a surrender, not a recovery.
//!
//! # Giving up safely is a first-class outcome
//!
//! `NFS4ERR_GRACE` means "not yet"; the reclaim is retried within a bounded
//! attempt budget. `NFS4ERR_NO_GRACE` means the window closed before this open
//! was recovered, and there is no correct way to reconstruct the state — so the
//! open is surrendered with its verbatim status retained. Same for
//! `NFS4ERR_RECLAIM_BAD` and `NFS4ERR_RECLAIM_CONFLICT`, and for an outcome that
//! never arrived at all. Surrender is deliberate and reported; it is never
//! silently upgraded into a fresh OPEN, which would fabricate an open the client
//! never actually held through the interruption.

use crate::error::{FacadeError, Nfs4Status};
use crate::handle::{FileHandle, ObjectIdentity, OpenFile};
use crate::state::open_owner::{OpenOutcome, OpenOwnerRegistry};
use crate::transport::{
    Deadline, DelegationType, OpenClaim, OpenHow, RawTransport, ShareAccess, ShareDeny,
};

/// Default bound on `NFS4ERR_GRACE` retries for one open.
///
/// Bounded because an unbounded retry against a server that never leaves grace is
/// an unbounded stall, and the failure model admits no unbounded waits.
pub const DEFAULT_GRACE_ATTEMPTS: u32 = 8;

/// One open that existed before the interruption and may be reclaimable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimTarget {
    /// Filehandle the open resolved to before the interruption.
    pub handle: FileHandle,
    /// Identity that must still hold after the reclaim.
    pub identity: ObjectIdentity,
    /// Share access to reclaim.
    pub share_access: ShareAccess,
    /// Share deny to reclaim.
    pub share_deny: ShareDeny,
}

impl ReclaimTarget {
    /// Describe a live open as a reclaim target, if it is reclaimable at all.
    ///
    /// Returns `None` for an unconfirmed open or one whose owner is poisoned:
    /// neither was ever proven to exist on the server, so neither may be claimed
    /// back.
    pub fn from_open(file: &OpenFile) -> Option<Self> {
        if !file.is_confirmed().ok()? {
            return None;
        }
        // An owner whose seqid became unknown was never proven to be in the
        // state this reclaim would assert, so it is not a target either.
        file.next_seqid().ok()??;
        let (share_access, share_deny) = file.share().ok()?;
        Some(Self {
            handle: file.handle().clone(),
            identity: file.identity(),
            share_access,
            share_deny,
        })
    }
}

/// Why an open could not be reclaimed. Every variant is terminal for that open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurrenderCause {
    /// `NFS4ERR_NO_GRACE`: the grace window closed before this reclaim landed.
    GraceLifted,
    /// `NFS4ERR_RECLAIM_BAD` or `NFS4ERR_RECLAIM_CONFLICT`: the server will not
    /// grant this reclaim, and no retry can change that.
    ReclaimRefused,
    /// The grace-retry budget was spent while the server was still answering
    /// `NFS4ERR_GRACE`.
    RetriesExhausted,
    /// No answer arrived, so whether the reclaim happened is unknown.
    Indeterminate,
    /// The open was never confirmed, so there was nothing provable to reclaim.
    NeverConfirmed,

    /// The server answered, but with a different object identity.
    IdentityChanged,
}

impl SurrenderCause {
    /// Whether the caller may safely continue with other work.
    ///
    /// Every cause here is a safe stop *for that open*. None of them is a licence
    /// to reconstruct the open by other means.
    pub fn is_safe_stop(self) -> bool {
        true
    }
}

/// One open that was not recovered.
#[derive(Debug)]
pub struct Surrendered {
    /// The target that could not be reclaimed.
    pub target: ReclaimTarget,
    /// Why.
    pub cause: SurrenderCause,
    /// The verbatim failure, when the server or transport produced one.
    pub error: Option<FacadeError>,
}

impl Surrendered {
    /// The server's `NFS4ERR_*` status, when the surrender came from one.
    pub fn status(&self) -> Option<Nfs4Status> {
        self.error.as_ref().and_then(FacadeError::status)
    }
}

/// What a whole reclaim pass achieved.
#[derive(Debug, Default)]
pub struct ReclaimReport {
    /// Opens recovered through `CLAIM_PREVIOUS`.
    pub recovered: Vec<OpenFile>,
    /// Opens deliberately given up, each with its cause.
    pub surrendered: Vec<Surrendered>,
    /// `NFS4ERR_GRACE` retries consumed across the whole pass.
    pub grace_retries: u32,
}

impl ReclaimReport {
    /// Whether every target was recovered.
    pub fn is_complete(&self) -> bool {
        self.surrendered.is_empty()
    }
}

/// A bounded `CLAIM_PREVIOUS` reclaim pass over a set of targets.
#[derive(Debug)]
pub struct ReclaimPlan {
    targets: Vec<ReclaimTarget>,
    unreclaimable: Vec<(ReclaimTarget, SurrenderCause)>,
    grace_attempts: u32,
}

impl ReclaimPlan {
    /// Build a plan from the opens that were live before the interruption.
    ///
    /// Opens that cannot be described as a target — unconfirmed, or with a
    /// poisoned owner — are recorded so the report accounts for every open rather
    /// than quietly dropping the ones that were hardest to reason about.
    pub fn from_opens<'a>(opens: impl IntoIterator<Item = &'a OpenFile>) -> Self {
        let mut targets = Vec::new();
        let mut unreclaimable = Vec::new();
        for file in opens {
            if let Some(target) = ReclaimTarget::from_open(file) {
                targets.push(target);
                continue;
            }
            let Ok((share_access, share_deny)) = file.share() else {
                // The session is closed, so there is nothing left to describe.
                continue;
            };
            // Both causes are terminal, but they are not the same fact and the
            // report should not pretend otherwise: one open was never confirmed,
            // the other was confirmed and then lost track of its seqid.
            let cause = if file.next_seqid().ok().flatten().is_none() {
                SurrenderCause::Indeterminate
            } else {
                SurrenderCause::NeverConfirmed
            };
            unreclaimable.push((
                ReclaimTarget {
                    handle: file.handle().clone(),
                    identity: file.identity(),
                    share_access,
                    share_deny,
                },
                cause,
            ));
        }
        Self {
            targets,
            unreclaimable,
            grace_attempts: DEFAULT_GRACE_ATTEMPTS,
        }
    }

    /// Build a plan from explicit targets.
    pub fn from_targets(targets: Vec<ReclaimTarget>) -> Self {
        Self {
            targets,
            unreclaimable: Vec::new(),
            grace_attempts: DEFAULT_GRACE_ATTEMPTS,
        }
    }

    /// Bound how many `NFS4ERR_GRACE` retries one target may consume.
    pub fn with_grace_attempts(mut self, attempts: u32) -> Self {
        self.grace_attempts = attempts.max(1);
        self
    }

    /// Targets this plan will attempt.
    pub fn targets(&self) -> &[ReclaimTarget] {
        &self.targets
    }

    /// Run the pass.
    ///
    /// Each target gets a fresh open owner: the pre-interruption owner's seqid is
    /// not knowable across a server restart, and NFSv4.0 lets a reclaim establish
    /// a new owner for the same client id.
    pub fn run(
        self,
        registry: &mut OpenOwnerRegistry,
        transport: &mut dyn RawTransport,
        deadline: Deadline,
    ) -> ReclaimReport {
        let mut report = ReclaimReport::default();
        for (target, cause) in self.unreclaimable {
            report.surrendered.push(Surrendered {
                target,
                cause,
                error: None,
            });
        }
        for target in self.targets {
            let outcome = reclaim_one(
                registry,
                transport,
                &target,
                self.grace_attempts,
                deadline,
                &mut report.grace_retries,
            );
            match outcome {
                Ok(file) => report.recovered.push(file),
                Err((cause, error)) => report.surrendered.push(Surrendered {
                    target,
                    cause,
                    error,
                }),
            }
        }
        report
    }
}

type ReclaimFailure = (SurrenderCause, Option<FacadeError>);

fn reclaim_one(
    registry: &mut OpenOwnerRegistry,
    transport: &mut dyn RawTransport,
    target: &ReclaimTarget,
    grace_attempts: u32,
    deadline: Deadline,
    grace_retries: &mut u32,
) -> Result<OpenFile, ReclaimFailure> {
    let mut last: Option<FacadeError> = None;
    for attempt in 0..grace_attempts {
        if attempt > 0 {
            *grace_retries = grace_retries.saturating_add(1);
        }
        let lease = match registry.allocate() {
            Ok(lease) => lease,
            Err(error) => return Err((SurrenderCause::Indeterminate, Some(error))),
        };
        let outcome = registry.open_with_claim(
            lease,
            transport,
            // CLAIM_PREVIOUS names the object by the current filehandle, not by a
            // parent plus a name: the name may no longer resolve to it.
            &target.handle,
            OpenClaim::Previous {
                delegate_type: DelegationType::None,
            },
            OpenHow::NoCreate,
            target.share_access,
            target.share_deny,
            deadline,
        );
        match outcome {
            OpenOutcome::Opened(file) => {
                return if file.identity() == target.identity {
                    Ok(file)
                } else {
                    Err((SurrenderCause::IdentityChanged, None))
                };
            }
            // A reclaim that opened but could not be confirmed has not been
            // proven; it is surrendered rather than carried forward unusable.
            OpenOutcome::Unconfirmed { error, .. } => {
                return Err((SurrenderCause::ReclaimRefused, Some(error)))
            }
            OpenOutcome::Abandoned { error } => {
                return Err((SurrenderCause::Indeterminate, Some(error)))
            }
            OpenOutcome::Rejected { error, .. } => {
                let status = error.status();
                last = Some(error);
                match status {
                    // "Not yet." Retry within the budget.
                    Some(Nfs4Status::GRACE) => continue,
                    Some(Nfs4Status::NO_GRACE) => return Err((SurrenderCause::GraceLifted, last)),
                    Some(Nfs4Status::RECLAIM_BAD) | Some(Nfs4Status::RECLAIM_CONFLICT) => {
                        return Err((SurrenderCause::ReclaimRefused, last))
                    }
                    _ => return Err((SurrenderCause::ReclaimRefused, last)),
                }
            }
        }
    }
    Err((SurrenderCause::RetriesExhausted, last))
}
