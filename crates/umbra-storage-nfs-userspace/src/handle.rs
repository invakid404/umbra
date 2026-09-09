//! Handle facade: byte-owned filehandles and encapsulated open state.
//!
//! # What this facade removes
//!
//! The audited spike represented a filehandle as raw pointers whose validity
//! depended on declaration order in the caller's scope, and asserted a borrow
//! lifetime the types did not carry. Nothing here borrows: a [`FileHandle`] owns
//! its bytes, so there is no pointer to outlive anything.
//!
//! Ordering that a borrow was supposed to express is carried instead by
//! ownership. An [`OpenFile`] holds an `Arc<SessionCore>`, so a session's core
//! cannot be dropped while any open state derived from it is alive. Closing a
//! session marks it closed; a later use of a handle from that session is a
//! deterministic [`AuthorityError::IdentityUnproven`], not undefined behaviour.
//!
//! # Sequencing
//!
//! NFSv4.0 open-owner and lock-owner sequence numbers admit exactly one
//! outstanding sequenced operation per owner, and RFC 7530 section 9.1.7 decides
//! whether a failure advances the seqid. Both rules are enforced by
//! [`OpenFile::sequence`], which hands out a [`SequencedOp`] guard holding the
//! owner's lock. The guard must be resolved with [`SequencedOp::commit`] or
//! [`SequencedOp::abort`]; dropping it unresolved poisons the owner sequence so
//! the next caller is told to recover rather than silently desynchronising.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::error::{AuthorityError, FacadeError, FacadeResult, Nfs4Status};
use crate::transport::{ConnectionEpoch, Fsid, ShareAccess, ShareDeny};

/// Server-assigned client id from SETCLIENTID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClientId(pub u64);

/// NFSv4 `stateid4`: a sequence number plus twelve opaque bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stateid {
    /// Stateid sequence number, advanced by the server.
    pub seqid: u32,
    /// Opaque server-owned identity. Never interpreted.
    pub other: [u8; 12],
}

impl Stateid {
    /// The all-zero special stateid, valid for anonymous reads.
    pub const ANONYMOUS: Self = Self {
        seqid: 0,
        other: [0; 12],
    };
}

/// A byte-owned NFSv4 filehandle.
///
/// Cannot be forged: the constructor is crate-private, so a filehandle only
/// exists because a GETFH reply or a fake produced one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileHandle {
    bytes: Vec<u8>,
}

/// NFSv4.0 caps a filehandle at `NFS4_FHSIZE` bytes.
pub const MAX_FILEHANDLE_BYTES: usize = 128;

impl FileHandle {
    /// Adopt filehandle bytes returned by a server or a fake.
    pub(crate) fn from_wire(bytes: Vec<u8>) -> FacadeResult<Self> {
        if bytes.is_empty() || bytes.len() > MAX_FILEHANDLE_BYTES {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                format!("filehandle of {} bytes is out of range", bytes.len()),
            )));
        }
        Ok(Self { bytes })
    }

    /// Borrow the exact bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false: a zero-length filehandle cannot be constructed.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Stable object identity: `(fsid, fileid)`, never the filehandle bytes.
///
/// A server may return a different filehandle for the same object, and a volatile
/// filehandle may expire outright. Identity therefore lives here. Rename keeps an
/// object's identity; replacing a name produces a different identity, which is
/// how a replaced object is detected instead of assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectIdentity {
    /// Server file system identity.
    pub fsid: Fsid,
    /// Server file identity within that file system.
    pub fileid: u64,
}

/// Open-owner: the client id it is scoped to plus its opaque owner bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OpenOwner {
    client_id: ClientId,
    owner: Vec<u8>,
}

impl OpenOwner {
    /// Mint an open owner. Crate-private so owners cannot be forged by consumers.
    pub(crate) fn new(client_id: ClientId, owner: Vec<u8>) -> Self {
        Self { client_id, owner }
    }

    /// Client id this owner belongs to.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Opaque owner bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.owner
    }
}

/// Identity of one facade session, distinguishing successive incarnations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// The shared core of a facade session.
///
/// Every [`OpenFile`] holds an `Arc` of this, so the core outlives all open state
/// by construction. That is the ownership form of the ordering the spike tried to
/// state as a borrow lifetime.
#[derive(Debug)]
pub struct SessionCore {
    id: SessionId,
    client_id: ClientId,
    epoch: ConnectionEpoch,
    closed: AtomicBool,
}

impl SessionCore {
    /// Build a session core.
    pub(crate) fn new(id: SessionId, client_id: ClientId, epoch: ConnectionEpoch) -> Arc<Self> {
        Arc::new(Self {
            id,
            client_id,
            epoch,
            closed: AtomicBool::new(false),
        })
    }

    /// Session identity.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Client id this session established.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Connection generation this session was established on.
    pub fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// Whether the session has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Mark the session closed. Open state that survives becomes unusable, and
    /// says so, rather than dereferencing anything.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A live facade session: the only place filehandles and open state are minted.
///
/// [`FileHandle`], [`OpenOwner`] and [`OpenFile`] have no public constructors.
/// Bytes become a filehandle only by passing through [`Session::adopt_filehandle`],
/// which validates them and ties them to this session, so a consumer cannot
/// fabricate one from arbitrary bytes. A session itself is Umbra's own construct:
/// Umbra owns client ids, so establishing one is a legitimate public operation.
#[derive(Clone, Debug)]
pub struct Session {
    core: Arc<SessionCore>,
}

impl Session {
    /// Establish a session for a confirmed client id on a connection generation.
    pub fn establish(id: SessionId, client_id: ClientId, epoch: ConnectionEpoch) -> Self {
        Self {
            core: SessionCore::new(id, client_id, epoch),
        }
    }

    /// The shared core every handle derived from this session holds.
    pub fn core(&self) -> &Arc<SessionCore> {
        &self.core
    }

    /// Adopt filehandle bytes returned by the server, validating their length.
    pub fn adopt_filehandle(&self, bytes: Vec<u8>) -> FacadeResult<FileHandle> {
        self.guard()?;
        FileHandle::from_wire(bytes)
    }

    /// Mint an open owner scoped to this session's client id.
    pub fn open_owner(&self, owner: impl Into<Vec<u8>>) -> FacadeResult<OpenOwner> {
        self.guard()?;
        Ok(OpenOwner::new(self.core.client_id(), owner.into()))
    }

    /// Adopt the result of a successful OPEN.
    ///
    /// The returned [`OpenFile`] holds an owning reference to this session's core,
    /// so the core cannot be dropped while the open state is alive.
    pub fn adopt_open(&self, grant: OpenGrant) -> FacadeResult<OpenFile> {
        self.guard()?;
        if grant.owner.client_id() != self.core.client_id() {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                "open owner belongs to a different client id".into(),
            )));
        }
        Ok(OpenFile::new(Arc::clone(&self.core), grant))
    }

    /// Close the session. Surviving open state becomes unusable and says so.
    pub fn close(&self) {
        self.core.close();
    }

    fn guard(&self) -> FacadeResult<()> {
        if self.core.is_closed() {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                "session closed".into(),
            )));
        }
        Ok(())
    }
}

/// State of an open-owner's sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    /// Next seqid to send.
    Ready(u32),
    /// A sequenced operation was abandoned without a recorded outcome. The true
    /// server-side seqid is unknown, so the owner must be re-established.
    Poisoned,
}

#[derive(Debug)]
struct OpenState {
    sequence: SequenceState,
    stateid: Stateid,
    confirmed: bool,
    share_access: ShareAccess,
    share_deny: ShareDeny,
}

/// Everything a successful OPEN establishes about one open file.
///
/// Grouped so [`OpenFile::new`] cannot be called with two same-typed arguments
/// transposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenGrant {
    /// Filehandle the OPEN resolved to.
    pub handle: FileHandle,
    /// Stable identity of that object.
    pub identity: ObjectIdentity,
    /// Open owner the state belongs to.
    pub owner: OpenOwner,
    /// Stateid the server returned.
    pub stateid: Stateid,
    /// Next open-owner seqid to send.
    pub next_seqid: u32,
    /// Whether OPEN_CONFIRM has already completed.
    pub confirmed: bool,
    /// Share access granted.
    pub share_access: ShareAccess,
    /// Share deny granted.
    pub share_deny: ShareDeny,
}

/// One open file: its handle, identity, owner and mutable open state.
///
/// Open state is behind a mutex, which is what makes seqid advance safe under
/// shared access and, more importantly, makes concurrent sequenced operations for
/// one owner impossible rather than merely discouraged.
#[derive(Debug)]
pub struct OpenFile {
    handle: FileHandle,
    identity: ObjectIdentity,
    owner: OpenOwner,
    state: Mutex<OpenState>,
    session: Arc<SessionCore>,
}

impl OpenFile {
    /// Adopt the result of a successful OPEN.
    pub(crate) fn new(session: Arc<SessionCore>, grant: OpenGrant) -> Self {
        Self {
            handle: grant.handle,
            identity: grant.identity,
            owner: grant.owner,
            state: Mutex::new(OpenState {
                sequence: SequenceState::Ready(grant.next_seqid),
                stateid: grant.stateid,
                confirmed: grant.confirmed,
                share_access: grant.share_access,
                share_deny: grant.share_deny,
            }),
            session,
        }
    }

    /// The current filehandle for this open.
    pub fn handle(&self) -> &FileHandle {
        &self.handle
    }

    /// Stable object identity. Unchanged by rename.
    pub fn identity(&self) -> ObjectIdentity {
        self.identity
    }

    /// The open owner.
    pub fn owner(&self) -> &OpenOwner {
        &self.owner
    }

    /// The session this open belongs to.
    pub fn session(&self) -> &Arc<SessionCore> {
        &self.session
    }

    /// Current stateid, or an authority error when the state is unusable.
    pub fn stateid(&self) -> FacadeResult<Stateid> {
        let state = self.lock()?;
        if !state.confirmed {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                "stateid is unconfirmed; OPEN_CONFIRM has not completed".into(),
            )));
        }
        Ok(state.stateid)
    }

    /// Share access and deny bits this open holds.
    pub fn share(&self) -> FacadeResult<(ShareAccess, ShareDeny)> {
        let state = self.lock()?;
        Ok((state.share_access, state.share_deny))
    }

    /// Whether OPEN_CONFIRM has completed for this open.
    pub fn is_confirmed(&self) -> FacadeResult<bool> {
        Ok(self.lock()?.confirmed)
    }

    /// The seqid the next sequenced operation would carry, or `None` when the
    /// owner's sequence is poisoned.
    ///
    /// Reading this does not begin an operation. [`OpenFile::sequence`] is the
    /// only way to obtain a seqid to send, and its guard poisons the owner if it
    /// is dropped unresolved — which makes `sequence` a hazardous way to ask a
    /// question. This is the safe way to ask it.
    pub fn next_seqid(&self) -> FacadeResult<Option<u32>> {
        Ok(match self.lock()?.sequence {
            SequenceState::Ready(seqid) => Some(seqid),
            SequenceState::Poisoned => None,
        })
    }

    /// Begin one sequenced operation for this open owner.
    ///
    /// Holds the owner's lock for the whole operation, so a second sequenced
    /// operation for the same owner cannot start until this one is resolved.
    /// The returned guard must be resolved; dropping it unresolved poisons the
    /// sequence.
    pub fn sequence(&self) -> FacadeResult<SequencedOp<'_>> {
        let state = self.lock()?;
        let issued = match state.sequence {
            SequenceState::Ready(seqid) => seqid,
            SequenceState::Poisoned => {
                return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                    "open-owner sequence poisoned by an unresolved operation".into(),
                )))
            }
        };
        Ok(SequencedOp {
            state,
            issued,
            resolved: false,
        })
    }

    fn lock(&self) -> FacadeResult<MutexGuard<'_, OpenState>> {
        if self.session.is_closed() {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                "session closed; this open state is no longer usable".into(),
            )));
        }
        self.state.lock().map_err(|_| {
            FacadeError::Authority(AuthorityError::IdentityUnproven(
                "open state poisoned by a panicking holder".into(),
            ))
        })
    }
}

/// A borrowed, in-progress sequenced operation for one open owner.
///
/// The lifetime here is real enforcement rather than a comment: the guard cannot
/// outlive the [`OpenFile`] it came from, and holding it holds the owner's lock.
#[must_use = "a sequenced operation must be committed or aborted; dropping it \
              poisons the open-owner sequence"]
#[derive(Debug)]
pub struct SequencedOp<'a> {
    state: MutexGuard<'a, OpenState>,
    issued: u32,
    resolved: bool,
}

impl SequencedOp<'_> {
    /// The seqid to put on the wire for this operation.
    pub fn seqid(&self) -> u32 {
        self.issued
    }

    /// The stateid this operation should carry, confirmed or not.
    pub fn stateid(&self) -> Stateid {
        self.state.stateid
    }

    /// Record a successful outcome: advance the seqid and adopt the new stateid.
    pub fn commit(mut self, stateid: Stateid) {
        self.state.stateid = stateid;
        self.advance();
    }

    /// Record a successful outcome that returns no new stateid, such as RENEW-
    /// adjacent confirmations that only consume a seqid.
    pub fn commit_without_stateid(mut self) {
        self.advance();
    }

    /// Record a successful OPEN_CONFIRM: adopt the stateid and mark it usable.
    pub fn commit_confirmed(mut self, stateid: Stateid) {
        self.state.stateid = stateid;
        self.state.confirmed = true;
        self.advance();
    }

    /// Record a successful OPEN_DOWNGRADE: adopt the stateid and the narrower
    /// share bits the server granted.
    ///
    /// The recorded bits are the server's answer, not the request, because a
    /// downgrade the server declined to apply in full must not be remembered as
    /// though it had been. Widening is impossible through this path: OPEN is the
    /// only operation that grants share bits, so the narrowing is checked by the
    /// caller before the operation is sequenced.
    pub fn commit_downgraded(
        mut self,
        stateid: Stateid,
        share_access: ShareAccess,
        share_deny: ShareDeny,
    ) {
        self.state.stateid = stateid;
        self.state.share_access = share_access;
        self.state.share_deny = share_deny;
        self.advance();
    }

    /// Record a server failure.
    ///
    /// RFC 7530 section 9.1.7 decides the outcome: most failures still advance
    /// the owner seqid, and only the statuses in [`Nfs4Status::holds_seqid`] leave
    /// it where it was. Encoding the rule here keeps every call site correct.
    pub fn abort(mut self, status: Nfs4Status) {
        if status.holds_seqid() {
            self.resolved = true;
        } else {
            self.advance();
        }
    }

    /// Record an outcome that is genuinely unknown, such as a lost reply.
    ///
    /// The seqid the server observed cannot be inferred, so the owner is poisoned
    /// and must be re-established. This is the honest answer, and it is the same
    /// one an unresolved drop produces.
    pub fn abandon(mut self) {
        self.state.sequence = SequenceState::Poisoned;
        self.resolved = true;
    }

    fn advance(&mut self) {
        // NFSv4.0 open-owner seqids wrap at 2^32 with no zero-is-special rule,
        // so wrapping is the protocol behaviour, not an overflow bug.
        self.state.sequence = SequenceState::Ready(self.issued.wrapping_add(1));
        self.resolved = true;
    }
}

impl Drop for SequencedOp<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            self.state.sequence = SequenceState::Poisoned;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::establish(SessionId(1), ClientId(7), ConnectionEpoch(1))
    }

    fn open_file_in(session: &Session) -> OpenFile {
        session
            .adopt_open(OpenGrant {
                handle: session.adopt_filehandle(vec![1, 2, 3]).unwrap(),
                identity: ObjectIdentity {
                    fsid: Fsid { major: 1, minor: 2 },
                    fileid: 42,
                },
                owner: session.open_owner(b"umbra-open-owner".to_vec()).unwrap(),
                stateid: Stateid {
                    seqid: 1,
                    other: [9; 12],
                },
                next_seqid: 0,
                confirmed: true,
                share_access: ShareAccess::BOTH,
                share_deny: ShareDeny::NONE,
            })
            .unwrap()
    }

    fn open_file() -> OpenFile {
        open_file_in(&session())
    }

    #[test]
    fn filehandles_own_their_bytes_and_reject_out_of_range_lengths() {
        assert_eq!(
            FileHandle::from_wire(vec![7, 8]).unwrap().as_bytes(),
            &[7, 8]
        );
        assert!(FileHandle::from_wire(Vec::new()).is_err());
        assert!(FileHandle::from_wire(vec![0; MAX_FILEHANDLE_BYTES + 1]).is_err());
    }

    #[test]
    fn commit_advances_the_seqid_and_adopts_the_stateid() {
        let file = open_file();
        let op = file.sequence().unwrap();
        assert_eq!(op.seqid(), 0);
        let next = Stateid {
            seqid: 2,
            other: [4; 12],
        };
        op.commit(next);
        let op = file.sequence().unwrap();
        assert_eq!(op.seqid(), 1);
        assert_eq!(op.stateid(), next);
        op.commit_without_stateid();
        assert_eq!(file.stateid().unwrap(), next);
    }

    #[test]
    fn abort_follows_the_rfc_seqid_rule_in_both_directions() {
        let file = open_file();
        file.sequence().unwrap().abort(Nfs4Status::BAD_SEQID);
        let held = file.sequence().unwrap();
        assert_eq!(held.seqid(), 0, "a held status does not advance the seqid");
        held.abort(Nfs4Status::ACCESS);
        let advanced = file.sequence().unwrap();
        assert_eq!(advanced.seqid(), 1, "any other status advances it");
        advanced.commit_without_stateid();
    }

    #[test]
    fn an_unresolved_guard_poisons_the_owner_instead_of_desynchronising() {
        let file = open_file();
        drop(file.sequence().unwrap());
        let error = file.sequence().unwrap_err();
        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::IdentityUnproven(_))
        ));
    }

    #[test]
    fn a_lost_reply_is_recorded_as_unknown_not_guessed() {
        let file = open_file();
        file.sequence().unwrap().abandon();
        assert!(file.sequence().is_err());
        assert_eq!(file.next_seqid().unwrap(), None);
    }

    #[test]
    fn next_seqid_inspects_the_owner_without_risking_a_poisoning_drop() {
        let file = open_file();
        assert_eq!(file.next_seqid().unwrap(), Some(0));
        // Asking twice changes nothing, which is the whole point of not going
        // through the guard to find out.
        assert_eq!(file.next_seqid().unwrap(), Some(0));
        file.sequence().unwrap().commit_without_stateid();
        assert_eq!(file.next_seqid().unwrap(), Some(1));
    }

    #[test]
    fn a_closed_session_makes_open_state_unusable_deterministically() {
        let session = session();
        let file = open_file_in(&session);
        session.close();
        assert!(file.stateid().is_err());
        assert!(file.sequence().is_err());
        assert!(session.adopt_filehandle(vec![1]).is_err());
        assert!(session.open_owner(b"another".to_vec()).is_err());
    }

    #[test]
    fn open_state_keeps_its_session_core_alive() {
        let session = session();
        let file = open_file_in(&session);
        let core = Arc::clone(session.core());
        drop(session);
        // The session handle is gone; the open state still holds the core, so
        // nothing it points at can have been freed.
        assert_eq!(Arc::strong_count(&core), 2);
        assert!(file.stateid().is_ok());
    }

    #[test]
    fn an_owner_from_another_client_is_refused() {
        let session = session();
        let other = Session::establish(SessionId(2), ClientId(8), ConnectionEpoch(1));
        let grant = OpenGrant {
            handle: session.adopt_filehandle(vec![1]).unwrap(),
            identity: ObjectIdentity {
                fsid: Fsid { major: 1, minor: 2 },
                fileid: 1,
            },
            owner: other.open_owner(b"owner".to_vec()).unwrap(),
            stateid: Stateid::ANONYMOUS,
            next_seqid: 0,
            confirmed: true,
            share_access: ShareAccess::READ,
            share_deny: ShareDeny::NONE,
        };
        assert!(session.adopt_open(grant).is_err());
    }

    #[test]
    fn a_downgrade_narrows_the_recorded_share_bits_and_advances_the_seqid() {
        let file = open_file();
        assert_eq!(file.share().unwrap(), (ShareAccess::BOTH, ShareDeny::NONE));
        let op = file.sequence().unwrap();
        assert_eq!(op.seqid(), 0);
        let downgraded = Stateid {
            seqid: 3,
            other: [5; 12],
        };
        op.commit_downgraded(downgraded, ShareAccess::READ, ShareDeny::NONE);
        assert_eq!(file.share().unwrap(), (ShareAccess::READ, ShareDeny::NONE));
        assert_eq!(file.stateid().unwrap(), downgraded);
        let next = file.sequence().unwrap();
        assert_eq!(next.seqid(), 1);
        next.commit_without_stateid();
    }

    #[test]
    fn identity_is_the_fsid_fileid_pair_not_the_filehandle_bytes() {
        let renamed = FileHandle::from_wire(vec![9, 9, 9]).unwrap();
        let file = open_file();
        assert_ne!(file.handle().as_bytes(), renamed.as_bytes());
        assert_eq!(file.identity().fileid, 42);
    }
}
