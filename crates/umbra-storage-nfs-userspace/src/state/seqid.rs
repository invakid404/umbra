//! Open- and lock-owner sequence numbers for the phase before an `OpenFile` exists.
//!
//! The frozen handle facade already sequences an open once
//! [`OpenFile`](crate::handle::OpenFile) exists: [`SequencedOp`](crate::handle::SequencedOp)
//! holds the owner's mutex, so a second concurrent sequenced operation for that
//! owner cannot start. But an OPEN carries a seqid *before* any `OpenFile` has
//! been minted, and so does a CLOSE-less owner that never got that far. This
//! module covers exactly that window with the same three rules:
//!
//! 1. One outstanding sequenced operation per owner. Enforced by [`SeqidTicket`]
//!    borrowing the [`OwnerSequence`] mutably, so a second ticket cannot exist.
//! 2. RFC 7530 section 9.1.7 decides whether a failure advances the seqid. The
//!    rule is not re-derived here; it is read from
//!    [`Nfs4Status::holds_seqid`](crate::error::Nfs4Status::holds_seqid), which
//!    the error facade froze beside the status word.
//! 3. An unknown outcome poisons the owner. A lost reply means the seqid the
//!    server observed cannot be inferred, and guessing it desynchronises the
//!    owner permanently. Poisoning is the honest answer and it is what an
//!    unresolved drop produces too.

use crate::error::{AuthorityError, FacadeError, FacadeResult, Nfs4Status};

/// Why an owner sequence became unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoisonReason {
    /// A sequenced operation's outcome was never learned, typically a lost reply.
    OutcomeUnknown,
    /// A sequenced operation guard was dropped without being resolved.
    Unresolved,
}

impl PoisonReason {
    /// Human-readable cause, used verbatim in the authority error.
    pub fn detail(self) -> &'static str {
        match self {
            Self::OutcomeUnknown => {
                "owner sequence poisoned: a sequenced operation's outcome is unknown"
            }
            Self::Unresolved => {
                "owner sequence poisoned: a sequenced operation guard was dropped unresolved"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Ready(u32),
    Poisoned(PoisonReason),
}

/// The seqid counter for one open- or lock-owner before its state is delegated.
///
/// NFSv4.0 scopes a seqid to an owner, not to an open. This type therefore holds
/// the authority for exactly one owner and hands it over — by value — to the
/// [`OpenFile`](crate::handle::OpenFile) that a successful OPEN mints, so the two
/// can never both believe they own the same counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerSequence {
    state: SequenceState,
}

impl OwnerSequence {
    /// A fresh owner starting at seqid zero, as RFC 7530 section 9.1.4 requires
    /// for an owner the server has not seen.
    pub fn fresh() -> Self {
        Self {
            state: SequenceState::Ready(0),
        }
    }

    /// Resume a known owner at `seqid`.
    pub fn resuming_at(seqid: u32) -> Self {
        Self {
            state: SequenceState::Ready(seqid),
        }
    }

    /// The next seqid this owner will put on the wire, when it is usable.
    pub fn next_seqid(&self) -> Option<u32> {
        match self.state {
            SequenceState::Ready(seqid) => Some(seqid),
            SequenceState::Poisoned(_) => None,
        }
    }

    /// Why the owner is unusable, if it is.
    pub fn poison(&self) -> Option<PoisonReason> {
        match self.state {
            SequenceState::Poisoned(reason) => Some(reason),
            SequenceState::Ready(_) => None,
        }
    }

    /// Begin one sequenced operation for this owner.
    ///
    /// The returned guard borrows this sequence mutably for its whole life, which
    /// is what makes a second concurrent sequenced operation for the owner
    /// impossible rather than merely discouraged.
    pub fn issue(&mut self) -> FacadeResult<SeqidTicket<'_>> {
        let issued = match self.state {
            SequenceState::Ready(seqid) => seqid,
            SequenceState::Poisoned(reason) => {
                return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                    reason.detail().into(),
                )))
            }
        };
        Ok(SeqidTicket {
            sequence: self,
            issued,
            resolved: false,
        })
    }
}

/// A borrowed, in-progress sequenced operation for one owner.
///
/// Must be resolved with [`SeqidTicket::commit`], [`SeqidTicket::abort`] or
/// [`SeqidTicket::abandon`]. Dropping it unresolved poisons the owner, so a
/// forgotten error path degrades into "recover this owner" rather than into a
/// silently desynchronised sequence.
#[must_use = "a sequenced operation must be committed, aborted or abandoned; \
              dropping it poisons the owner sequence"]
#[derive(Debug)]
pub struct SeqidTicket<'a> {
    sequence: &'a mut OwnerSequence,
    issued: u32,
    resolved: bool,
}

impl SeqidTicket<'_> {
    /// The seqid to put on the wire for this operation.
    pub fn seqid(&self) -> u32 {
        self.issued
    }

    /// Record a successful outcome and advance the owner.
    pub fn commit(mut self) {
        self.advance();
    }

    /// Record a server failure, applying RFC 7530 section 9.1.7.
    ///
    /// Returns whether the seqid advanced, because a caller that retries needs to
    /// know which seqid the retry carries.
    pub fn abort(mut self, status: Nfs4Status) -> bool {
        if status.holds_seqid() {
            self.resolved = true;
            false
        } else {
            self.advance();
            true
        }
    }

    /// Record an outcome that is genuinely unknown, such as a dropped reply.
    ///
    /// The owner is poisoned. Nothing infers the server-side seqid from silence.
    pub fn abandon(mut self) {
        self.sequence.state = SequenceState::Poisoned(PoisonReason::OutcomeUnknown);
        self.resolved = true;
    }

    fn advance(&mut self) {
        // NFSv4.0 owner seqids wrap at 2^32 with no zero-is-special rule, so
        // wrapping is the protocol behaviour rather than an overflow bug.
        self.sequence.state = SequenceState::Ready(self.issued.wrapping_add(1));
        self.resolved = true;
    }
}

impl Drop for SeqidTicket<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            self.sequence.state = SequenceState::Poisoned(PoisonReason::Unresolved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_owner_starts_at_zero_and_advances_on_success() {
        let mut sequence = OwnerSequence::fresh();
        let ticket = sequence.issue().unwrap();
        assert_eq!(ticket.seqid(), 0);
        ticket.commit();
        assert_eq!(sequence.next_seqid(), Some(1));
    }

    #[test]
    fn the_rfc_seqid_rule_is_read_from_the_error_facade_not_re_derived() {
        let mut sequence = OwnerSequence::fresh();
        assert!(
            !sequence.issue().unwrap().abort(Nfs4Status::BAD_SEQID),
            "a held status leaves the seqid where it was"
        );
        assert_eq!(sequence.next_seqid(), Some(0));
        assert!(
            sequence.issue().unwrap().abort(Nfs4Status::ACCESS),
            "any other status advances it even on failure"
        );
        assert_eq!(sequence.next_seqid(), Some(1));
    }

    #[test]
    fn an_unknown_outcome_poisons_rather_than_guesses() {
        let mut sequence = OwnerSequence::fresh();
        sequence.issue().unwrap().abandon();
        assert_eq!(sequence.next_seqid(), None);
        assert_eq!(sequence.poison(), Some(PoisonReason::OutcomeUnknown));
        assert!(sequence.issue().is_err());
    }

    #[test]
    fn dropping_a_ticket_unresolved_poisons_the_owner() {
        let mut sequence = OwnerSequence::fresh();
        drop(sequence.issue().unwrap());
        assert_eq!(sequence.poison(), Some(PoisonReason::Unresolved));
    }

    #[test]
    fn seqids_wrap_because_the_protocol_wraps() {
        let mut sequence = OwnerSequence::resuming_at(u32::MAX);
        sequence.issue().unwrap().commit();
        assert_eq!(sequence.next_seqid(), Some(0));
    }
}
