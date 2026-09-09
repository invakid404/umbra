//! Open-owner allocation and the OPEN / OPEN_CONFIRM / CLOSE / DOWNGRADE machine.
//!
//! # One open-owner per open file
//!
//! NFSv4.0 scopes a seqid to an *owner*, and the frozen handle facade puts the
//! sequence inside [`OpenFile`]. Two `OpenFile`s sharing one owner would each
//! believe they own that counter, which is precisely the desynchronisation the
//! facade exists to prevent. This module therefore allocates a distinct open
//! owner for every OPEN and moves the sequence into the `OpenFile` the OPEN
//! mints. An [`OwnerLease`] is consumed by the attempt, so reusing one is not a
//! discipline anybody has to remember — it does not compile.
//!
//! # What "no state advances" means here
//!
//! Every outcome is one of four, and the seqid treatment differs in each:
//!
//! | Outcome | Seqid | Owner |
//! | --- | --- | --- |
//! | [`OpenOutcome::Opened`] | advanced | moved into the `OpenFile` |
//! | [`OpenOutcome::Unconfirmed`] | advanced by OPEN, held or advanced by the failed OPEN_CONFIRM per RFC 7530 §9.1.7 | moved into the `OpenFile`, whose stateid stays unusable |
//! | [`OpenOutcome::Rejected`] | per RFC 7530 §9.1.7 | returned in a reusable lease |
//! | [`OpenOutcome::Abandoned`] | not advanced; the owner is poisoned | burned, never reissued |
//!
//! A lost reply lands in `Abandoned`: the server may or may not have consumed the
//! seqid, so the owner is retired rather than guessed at. That costs one owner
//! name and preserves correctness, which is the trade the failure model asks for.

use std::collections::BTreeSet;

use crate::error::{AuthorityError, FacadeError, FacadeResult};
use crate::handle::{
    ClientId, FileHandle, ObjectIdentity, OpenFile, OpenGrant, OpenOwner, Session, Stateid,
};
use crate::state::seqid::OwnerSequence;
use crate::transport::{
    AttrMask, ComponentName, Deadline, OpenArgs, OpenClaim, OpenHow, RawTransport, ShareAccess,
    ShareDeny,
};

/// An open owner that has been minted but not yet spent on an OPEN.
///
/// Carries its own seqid counter. `open` takes it by value, so one lease drives
/// at most one OPEN attempt and a burned owner cannot be resurrected.
#[must_use = "an allocated open owner must be spent on an OPEN or explicitly released"]
#[derive(Clone, Debug)]
pub struct OwnerLease {
    owner: OpenOwner,
    sequence: OwnerSequence,
}

impl OwnerLease {
    /// The owner bytes this lease will present.
    pub fn owner(&self) -> &OpenOwner {
        &self.owner
    }

    /// The seqid the next attempt will carry, when the owner is still usable.
    pub fn next_seqid(&self) -> Option<u32> {
        self.sequence.next_seqid()
    }
}

/// What one OPEN attempt settled.
#[derive(Debug)]
pub enum OpenOutcome {
    /// The open is established and its stateid is usable.
    Opened(OpenFile),
    /// OPEN succeeded but OPEN_CONFIRM did not.
    ///
    /// The `OpenFile` exists and holds the owner's sequence, but
    /// [`OpenFile::stateid`] keeps refusing until a confirm succeeds, so no
    /// READ or WRITE can be issued against it by accident.
    Unconfirmed {
        /// The unconfirmed open.
        file: OpenFile,
        /// The verbatim failure from OPEN_CONFIRM.
        error: FacadeError,
    },
    /// The server refused the OPEN. The seqid was resolved per RFC 7530 §9.1.7,
    /// so the returned lease is safe to reuse.
    Rejected {
        /// The still-usable lease.
        lease: OwnerLease,
        /// The verbatim failure.
        error: FacadeError,
    },
    /// The outcome is unknown. The owner is burned and nothing advanced.
    Abandoned {
        /// The verbatim failure.
        error: FacadeError,
    },
}

impl OpenOutcome {
    /// The established open, if the attempt produced a usable one.
    pub fn opened(self) -> Option<OpenFile> {
        match self {
            Self::Opened(file) => Some(file),
            _ => None,
        }
    }

    /// The verbatim failure, if this outcome carries one.
    pub fn error(&self) -> Option<&FacadeError> {
        match self {
            Self::Opened(_) => None,
            Self::Unconfirmed { error, .. }
            | Self::Rejected { error, .. }
            | Self::Abandoned { error } => Some(error),
        }
    }
}

/// What one CLOSE settled.
#[derive(Debug)]
pub enum CloseOutcome {
    /// The open is closed. The `OpenFile` was consumed, so it cannot be used again.
    Closed(Stateid),
    /// The server refused the CLOSE and the open is still live.
    Rejected {
        /// The open, handed back because it is still valid.
        file: OpenFile,
        /// The verbatim failure.
        error: FacadeError,
    },
    /// The outcome is unknown. The open's owner is poisoned and the open is gone;
    /// the server-side state persists until the lease expires.
    Abandoned {
        /// The verbatim failure.
        error: FacadeError,
    },
}

/// What an OPEN should do about creation and which name it targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    /// Directory the name lives in. OPEN is always parent-plus-name, never a path.
    pub parent: FileHandle,
    /// Byte-exact name within that directory.
    pub name: ComponentName,
    /// Create disposition.
    pub how: OpenHow,
    /// Share access to request.
    pub share_access: ShareAccess,
    /// Share deny to request. Umbra arbitrates writers itself, so this is
    /// normally [`ShareDeny::NONE`].
    pub share_deny: ShareDeny,
}

/// Mints open owners for one confirmed client, and drives OPEN through them.
#[derive(Debug)]
pub struct OpenOwnerRegistry {
    session: Session,
    prefix: Vec<u8>,
    next_owner: u64,
    burned: BTreeSet<Vec<u8>>,
}

impl OpenOwnerRegistry {
    /// Build a registry that mints owners under `prefix` for `session`.
    pub fn new(session: Session, prefix: impl Into<Vec<u8>>) -> Self {
        Self {
            session,
            prefix: prefix.into(),
            next_owner: 0,
            burned: BTreeSet::new(),
        }
    }

    /// The session owners are minted from.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The client id every owner from this registry is scoped to.
    pub fn client_id(&self) -> ClientId {
        self.session.core().client_id()
    }

    /// Owners retired because their seqid became unknown. Never reissued.
    pub fn burned(&self) -> usize {
        self.burned.len()
    }

    /// Mint a fresh open owner.
    ///
    /// Owner bytes are `<prefix>/<counter>` with the counter big-endian, so two
    /// owners from one registry are distinct by construction and a burned owner's
    /// bytes are never produced again.
    pub fn allocate(&mut self) -> FacadeResult<OwnerLease> {
        let mut bytes = self.prefix.clone();
        bytes.push(b'/');
        bytes.extend_from_slice(&self.next_owner.to_be_bytes());
        self.next_owner = self.next_owner.saturating_add(1);
        Ok(OwnerLease {
            owner: self.session.open_owner(bytes)?,
            sequence: OwnerSequence::fresh(),
        })
    }

    /// Drive one OPEN, and the OPEN_CONFIRM the server may demand, to a settled
    /// outcome.
    pub fn open(
        &mut self,
        lease: OwnerLease,
        transport: &mut dyn RawTransport,
        request: &OpenRequest,
        deadline: Deadline,
    ) -> OpenOutcome {
        self.open_with_claim(
            lease,
            transport,
            &request.parent,
            OpenClaim::Null {
                name: request.name.clone(),
            },
            request.how.clone(),
            request.share_access,
            request.share_deny,
            deadline,
        )
    }

    /// Drive one OPEN under an explicit claim.
    ///
    /// `reclaim` uses this with [`OpenClaim::Previous`]; every other caller goes
    /// through [`OpenOwnerRegistry::open`]. NFSv4.0 has exactly these two claim
    /// types in M1 scope, and `RECLAIM_COMPLETE` has no representation anywhere
    /// in the frozen transport because it is a v4.1 operation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_with_claim(
        &mut self,
        lease: OwnerLease,
        transport: &mut dyn RawTransport,
        current: &FileHandle,
        claim: OpenClaim,
        how: OpenHow,
        share_access: ShareAccess,
        share_deny: ShareDeny,
        deadline: Deadline,
    ) -> OpenOutcome {
        let OwnerLease {
            owner,
            mut sequence,
        } = lease;
        let ticket = match sequence.issue() {
            Ok(ticket) => ticket,
            Err(error) => return OpenOutcome::Abandoned { error },
        };
        let args = OpenArgs {
            seqid: ticket.seqid(),
            share_access,
            share_deny,
            owner: owner.clone(),
            how,
            claim,
        };

        let opened = transport.open(current, args, deadline);
        let (reply, handle) = match opened {
            Ok(reply) => {
                ticket.commit();
                reply
            }
            Err(error) => {
                return match error.status() {
                    // A server answer settles the seqid either way, so the owner
                    // survives and the lease can be reused.
                    Some(status) => {
                        ticket.abort(status);
                        OpenOutcome::Rejected {
                            lease: OwnerLease { owner, sequence },
                            error,
                        }
                    }
                    // No server answer: the seqid the server observed is unknown.
                    None => {
                        ticket.abandon();
                        self.burned.insert(owner.as_bytes().to_vec());
                        OpenOutcome::Abandoned { error }
                    }
                };
            }
        };

        let identity = match object_identity(transport, &handle, deadline) {
            Ok(identity) => identity,
            Err(error) => {
                // The open exists on the server but its identity is unproven, so
                // nothing may be written through it. Safe-stop: the state is
                // dropped and the server releases it when the lease expires.
                self.burned.insert(owner.as_bytes().to_vec());
                return OpenOutcome::Abandoned { error };
            }
        };

        let next_seqid = match sequence.next_seqid() {
            Some(seqid) => seqid,
            None => {
                self.burned.insert(owner.as_bytes().to_vec());
                return OpenOutcome::Abandoned {
                    error: FacadeError::Authority(AuthorityError::IdentityUnproven(
                        "owner sequence was poisoned by the OPEN that just succeeded".into(),
                    )),
                };
            }
        };

        let grant = OpenGrant {
            handle,
            identity,
            owner: owner.clone(),
            stateid: reply.stateid,
            next_seqid,
            confirmed: !reply.confirm_required,
            share_access,
            share_deny,
        };
        let file = match self.session.adopt_open(grant) {
            Ok(file) => file,
            Err(error) => {
                self.burned.insert(owner.as_bytes().to_vec());
                return OpenOutcome::Abandoned { error };
            }
        };

        if !reply.confirm_required {
            return OpenOutcome::Opened(file);
        }
        match confirm(&file, transport, deadline) {
            Ok(()) => OpenOutcome::Opened(file),
            Err(ConfirmFailure::Rejected(error)) => OpenOutcome::Unconfirmed { file, error },
            Err(ConfirmFailure::Abandoned(error)) => {
                self.burned.insert(owner.as_bytes().to_vec());
                OpenOutcome::Abandoned { error }
            }
        }
    }
}

enum ConfirmFailure {
    Rejected(FacadeError),
    Abandoned(FacadeError),
}

/// Run OPEN_CONFIRM for an open the server flagged, through the frozen sequencer.
fn confirm(
    file: &OpenFile,
    transport: &mut dyn RawTransport,
    deadline: Deadline,
) -> Result<(), ConfirmFailure> {
    let op = file.sequence().map_err(ConfirmFailure::Abandoned)?;
    let seqid = op.seqid();
    let stateid = op.stateid();
    match transport.open_confirm(file.handle(), stateid, seqid, deadline) {
        Ok(confirmed) => {
            op.commit_confirmed(confirmed);
            Ok(())
        }
        Err(error) => match error.status() {
            Some(status) => {
                op.abort(status);
                Err(ConfirmFailure::Rejected(error))
            }
            None => {
                op.abandon();
                Err(ConfirmFailure::Abandoned(error))
            }
        },
    }
}

/// Retry OPEN_CONFIRM for an open left in [`OpenOutcome::Unconfirmed`].
pub fn confirm_open(
    file: OpenFile,
    transport: &mut dyn RawTransport,
    deadline: Deadline,
) -> OpenOutcome {
    match confirm(&file, transport, deadline) {
        Ok(()) => OpenOutcome::Opened(file),
        Err(ConfirmFailure::Rejected(error)) => OpenOutcome::Unconfirmed { file, error },
        Err(ConfirmFailure::Abandoned(error)) => OpenOutcome::Abandoned { error },
    }
}

/// Close an open, consuming it so a closed stateid cannot be reused.
pub fn close(file: OpenFile, transport: &mut dyn RawTransport, deadline: Deadline) -> CloseOutcome {
    let outcome = {
        let op = match file.sequence() {
            Ok(op) => op,
            Err(error) => return CloseOutcome::Abandoned { error },
        };
        let seqid = op.seqid();
        let stateid = op.stateid();
        match transport.close(file.handle(), seqid, stateid, deadline) {
            Ok(reply) => {
                op.commit(reply.stateid);
                Ok(reply.stateid)
            }
            Err(error) => match error.status() {
                Some(status) => {
                    op.abort(status);
                    Err((true, error))
                }
                None => {
                    op.abandon();
                    Err((false, error))
                }
            },
        }
    };
    match outcome {
        Ok(stateid) => CloseOutcome::Closed(stateid),
        Err((true, error)) => CloseOutcome::Rejected { file, error },
        Err((false, error)) => CloseOutcome::Abandoned { error },
    }
}

/// A prepared OPEN_DOWNGRADE: the share bits to keep and the seqid to carry.
///
/// Downgrade narrows an open's share bits without closing it. Protocol state
/// decides the bits and owns the seqid; putting the operation on the wire is the
/// transport owner's job, which is why [`downgrade_with`] takes the dispatch as a
/// closure instead of reaching for a helper. The frozen [`Nfs4Op`](crate::transport::Nfs4Op)
/// has no `OpenDowngrade` variant, so today no dispatcher can be supplied by this
/// crate — the seam is here and unbound rather than faked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DowngradeRequest {
    /// Seqid this downgrade must carry.
    pub seqid: u32,
    /// Stateid being downgraded.
    pub stateid: Stateid,
    /// Share access to retain.
    pub share_access: ShareAccess,
    /// Share deny to retain.
    pub share_deny: ShareDeny,
}

/// Narrow an open's share bits, sequencing the operation correctly.
///
/// `dispatch` performs the wire operation and returns the server's stateid. The
/// seqid rules are applied to its result exactly as they are for OPEN and CLOSE:
/// a server failure resolves the seqid per RFC 7530 §9.1.7 and a lost reply
/// poisons the owner.
///
/// The requested bits must be a non-empty subset of what the open holds. An empty
/// access mask is a CLOSE, not a downgrade, and is refused here so a caller
/// cannot silently drop an open's last reference through the wrong operation.
pub fn downgrade_with<D>(
    file: &OpenFile,
    share_access: ShareAccess,
    share_deny: ShareDeny,
    dispatch: D,
) -> FacadeResult<Stateid>
where
    D: FnOnce(DowngradeRequest) -> FacadeResult<Stateid>,
{
    let (held_access, held_deny) = file.share()?;
    if share_access.0 == 0 {
        return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
            "an empty share access is a CLOSE, not an OPEN_DOWNGRADE".into(),
        )));
    }
    if share_access.0 & !held_access.0 != 0 || share_deny.0 & !held_deny.0 != 0 {
        return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
            "OPEN_DOWNGRADE may only narrow the share bits an open already holds".into(),
        )));
    }
    let op = file.sequence()?;
    let request = DowngradeRequest {
        seqid: op.seqid(),
        stateid: op.stateid(),
        share_access,
        share_deny,
    };
    match dispatch(request) {
        Ok(stateid) => {
            op.commit_downgraded(stateid, share_access, share_deny);
            Ok(stateid)
        }
        Err(error) => {
            match error.status() {
                Some(status) => op.abort(status),
                None => op.abandon(),
            }
            Err(error)
        }
    }
}

/// A lock owner and its sequence.
///
/// M1 dispatches no LOCK or LOCKU. The frozen contract admits only NFSv4.0
/// advisory byte-range record locks and states that no M1 consumer path issues
/// them; the M2 locking gate owns that decision. Allocation and sequencing live
/// here so the gate finds a seam rather than an empty file, and so a lock owner
/// is scoped to a client id by construction like every other owner.
#[must_use = "an allocated lock owner must be spent or explicitly released"]
#[derive(Clone, Debug)]
pub struct LockOwnerLease {
    client_id: ClientId,
    owner: Vec<u8>,
    sequence: OwnerSequence,
}

impl LockOwnerLease {
    /// The client id this lock owner is scoped to.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// The opaque lock-owner bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.owner
    }

    /// The seqid the next lock operation would carry.
    pub fn next_seqid(&self) -> Option<u32> {
        self.sequence.next_seqid()
    }

    /// The lock owner's sequence, for the M2 gate that will drive it.
    pub fn sequence_mut(&mut self) -> &mut OwnerSequence {
        &mut self.sequence
    }
}

/// Mints lock owners for one confirmed client.
#[derive(Debug)]
pub struct LockOwnerRegistry {
    client_id: ClientId,
    prefix: Vec<u8>,
    next_owner: u64,
}

impl LockOwnerRegistry {
    /// Build a registry minting lock owners under `prefix`.
    pub fn new(client_id: ClientId, prefix: impl Into<Vec<u8>>) -> Self {
        Self {
            client_id,
            prefix: prefix.into(),
            next_owner: 0,
        }
    }

    /// Mint a fresh lock owner.
    pub fn allocate(&mut self) -> LockOwnerLease {
        let mut owner = self.prefix.clone();
        owner.extend_from_slice(b"/lock/");
        owner.extend_from_slice(&self.next_owner.to_be_bytes());
        self.next_owner = self.next_owner.saturating_add(1);
        LockOwnerLease {
            client_id: self.client_id,
            owner,
            sequence: OwnerSequence::fresh(),
        }
    }
}

fn object_identity(
    transport: &mut dyn RawTransport,
    handle: &FileHandle,
    deadline: Deadline,
) -> FacadeResult<ObjectIdentity> {
    let attributes = transport.getattr(handle, AttrMask::IDENTITY, deadline)?;
    match (attributes.fsid, attributes.fileid) {
        (Some(fsid), Some(fileid)) => Ok(ObjectIdentity { fsid, fileid }),
        _ => Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
            "the server did not return both fsid and fileid; identity cannot be proven".into(),
        ))),
    }
}
