//! Create, read, write and commit against a pinned open object.
//!
//! # The open object is the unit of coherence
//!
//! An [`OpenObject`] is the frozen [`OpenFile`] plus the byte name it was opened
//! under. The name is recorded for diagnostics and is **never** re-resolved: every
//! READ, WRITE and COMMIT addresses [`OpenFile::handle`]. That single rule is what
//! delivers the handle guarantee the pinned syscall matrix states —
//!
//! * an in-place edit by another writer is visible on the next read, because the
//!   handle still names the object that was edited;
//! * a rename does not retarget the handle, because no name is consulted;
//! * a pathname replacement does not retarget it either, so the handle keeps
//!   answering for the original object while a fresh lookup finds the replacement;
//! * an unlink leaves the handle usable, and the object survives until CLOSE.
//!
//! # WRITE stability is recorded as it happened
//!
//! [`OpenObject::write`] records a durable intent through the replay facade
//! *before* dispatch, returns the count and stability the server actually reached
//! rather than the ones requested, and hands back a [`WriteTicket`]. An `UNSTABLE`
//! write is not durable until [`OpenObject::commit`] observes the same verifier
//! the WRITE returned; a changed verifier is
//! [`ReplayError::VerifierChanged`](crate::error::ReplayError::VerifierChanged),
//! which means the bytes must be rewritten from the retained payload. A
//! `FILE_SYNC` write owes no COMMIT and records no verifier, so a later
//! [`VerifierMatch::Unknown`] can never be misread as durability.

use umbra_core::{IdempotencyKey, LeaseEpoch, OperationId, RequestContext, RunId};

use crate::error::{AuthorityError, FacadeError, FacadeResult, RetainedError, TransportError};
use crate::handle::{FileHandle, ObjectIdentity, OpenFile, Stateid};
use crate::identity::PinnedObject;
use crate::replay::{
    Admission, CompletedWrite, IntentKind, Payload, ReplayIntent, ReplayLog, ReplayOutcome,
    VerifierMatch,
};
use crate::state::open_owner::{close, CloseOutcome, OpenOutcome, OpenOwnerRegistry, OpenRequest};
use crate::state::verifier::{check_commit, note_write, WriteRecord};
use crate::transport::{
    ComponentName, Deadline, OpenHow, RawTransport, ReadReply, ShareAccess, ShareDeny, Stability,
    Verifier,
};

/// How an OPEN should treat an existing or absent name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateDisposition {
    /// `OPEN4_NOCREATE`. An absent name is `NFS4ERR_NOENT`.
    OpenExisting,
    /// `UNCHECKED4`: open the name, creating it if absent.
    OpenOrCreate {
        /// Mode applied when the object is created.
        mode: u32,
    },
    /// `GUARDED4`: create the name, and fail if it already exists.
    ///
    /// This is what the contract's `Create` means — "exclusively create a new
    /// object; an existing name is `AlreadyExists`".
    CreateNew {
        /// Mode applied to the new object.
        mode: u32,
    },
    /// `EXCLUSIVE4`: create with a verifier that identifies *this* create.
    ///
    /// The verifier is the whole idempotency story: presenting the same one again
    /// is a recognised replay of the same create, and a different one on an
    /// existing name is a real collision. Derive it from the caller's operation
    /// identity with
    /// [`create_verifier_for`](crate::state::verifier::create_verifier_for), or
    /// take it from the
    /// [`CreateVerifierLedger`](crate::state::verifier::CreateVerifierLedger), so
    /// it survives the crash that made the retry necessary.
    CreateExclusive {
        /// The create verifier this attempt presents.
        verifier: Verifier,
    },
}

impl CreateDisposition {
    fn how(self) -> OpenHow {
        match self {
            Self::OpenExisting => OpenHow::NoCreate,
            Self::OpenOrCreate { mode } => OpenHow::Unchecked { mode },
            Self::CreateNew { mode } => OpenHow::Guarded { mode },
            Self::CreateExclusive { verifier } => OpenHow::Exclusive { verifier },
        }
    }

    /// Whether this disposition can bring a new object into existence.
    pub fn creates(self) -> bool {
        !matches!(self, Self::OpenExisting)
    }
}

/// The four identities every mutation carries.
///
/// Bundled so a call site cannot transpose two same-typed UUIDs, and constructed
/// from the contract's [`RequestContext`] so the writer epoch is checked in one
/// place instead of at every mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationIdentity {
    /// Run the mutation belongs to.
    pub run_id: RunId,
    /// Operation identity, unique per request.
    pub operation: OperationId,
    /// Idempotency key a retry is matched on.
    pub key: IdempotencyKey,
    /// Writer epoch that authorised the mutation.
    pub epoch: LeaseEpoch,
}

impl MutationIdentity {
    /// Read the identities out of a request context.
    ///
    /// A context with no writer epoch is [`AuthorityError::NoWriterEpoch`]: this
    /// provider never mutates on unproven authority, and an NFS lease is not
    /// evidence of Umbra writer authority.
    pub fn from_context(context: &RequestContext) -> FacadeResult<Self> {
        let epoch = context
            .writer_epoch
            .ok_or(FacadeError::Authority(AuthorityError::NoWriterEpoch))?;
        Ok(Self {
            run_id: context.run_id,
            operation: context.operation_id,
            key: context.idempotency_key.clone(),
            epoch,
        })
    }
}

/// One open file, pinned to the object it proved at OPEN time.
#[derive(Debug)]
pub struct OpenObject {
    file: OpenFile,
    opened_as: ComponentName,
}

impl OpenObject {
    /// Drive one OPEN and adopt the result.
    ///
    /// NFSv4 opens by parent filehandle plus name, never by path, so a caller
    /// cannot open something the anchor walk did not reach.
    pub fn open(
        owners: &mut OpenOwnerRegistry,
        transport: &mut dyn RawTransport,
        parent: &PinnedObject,
        name: &ComponentName,
        disposition: CreateDisposition,
        share_access: ShareAccess,
        deadline: Deadline,
    ) -> FacadeResult<Self> {
        if !parent.is_directory() {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                "an OPEN was addressed to a parent that is not a directory".into(),
            )));
        }
        let lease = owners.allocate()?;
        let request = OpenRequest {
            parent: parent.handle().clone(),
            name: name.clone(),
            how: disposition.how(),
            share_access,
            // Umbra arbitrates writers through its own admission, so the NFS
            // share reservation is never used as the admission mechanism.
            share_deny: ShareDeny::NONE,
        };
        match owners.open(lease, transport, &request, deadline) {
            OpenOutcome::Opened(file) => Ok(Self {
                file,
                opened_as: name.clone(),
            }),
            other => Err(other
                .error()
                .cloned()
                .unwrap_or_else(|| unreachable!("a non-opened outcome carries an error"))),
        }
    }

    /// The stable identity this open proved. Unchanged by rename or replacement.
    pub fn identity(&self) -> ObjectIdentity {
        self.file.identity()
    }

    /// The filehandle every operation on this open addresses.
    pub fn handle(&self) -> &FileHandle {
        self.file.handle()
    }

    /// The name this object was opened under.
    ///
    /// Diagnostic only. Nothing in this crate re-resolves it, which is exactly
    /// why a rename or a replacement cannot retarget the open.
    pub fn opened_as(&self) -> &ComponentName {
        &self.opened_as
    }

    /// The frozen open state, for callers that need its sequencer.
    pub fn file(&self) -> &OpenFile {
        &self.file
    }

    /// The stateid this open's I/O carries.
    pub fn stateid(&self) -> FacadeResult<Stateid> {
        self.file.stateid()
    }

    /// Read from the opened object.
    ///
    /// A short reply is a real answer, not an error: the server may return fewer
    /// bytes than asked for, and `eof` distinguishes "that is all there is" from
    /// "ask again".
    pub fn read(
        &self,
        transport: &mut dyn RawTransport,
        offset: u64,
        count: u32,
        deadline: Deadline,
    ) -> FacadeResult<ReadReply> {
        let stateid = self.stateid()?;
        transport.read(self.file.handle(), stateid, offset, count, deadline)
    }

    /// Write to the opened object, recording a durable intent first.
    ///
    /// Returns the ticket a later [`OpenObject::commit`] needs. A recorded outcome
    /// for the same key is returned verbatim without dispatching again, which is
    /// what makes a retry safe for a non-idempotent effect.
    pub fn write(
        &self,
        transport: &mut dyn RawTransport,
        replay: &mut dyn ReplayLog,
        identity: &MutationIdentity,
        write: WriteAt,
        deadline: Deadline,
    ) -> FacadeResult<WriteTicket> {
        let WriteAt {
            offset,
            stability,
            data,
        } = write;
        let requested = data.len();
        let intent = ReplayIntent {
            run_id: identity.run_id,
            operation: identity.operation,
            key: identity.key.clone(),
            epoch: identity.epoch,
            kind: IntentKind::Write {
                object: self.identity(),
                offset,
                stability,
            },
            payload: Payload::Inline(data.clone()),
        };
        // Backpressure and replay identity are both settled before dispatch, which
        // is the ordering the failure model requires: a full buffer must refuse
        // the operation, never lose the evidence for one already sent.
        match replay.admit(&intent).map_err(FacadeError::Replay)? {
            Admission::Fresh => {}
            Admission::Recorded(ReplayOutcome::Completed(completed)) => {
                return Ok(WriteTicket {
                    key: identity.key.clone(),
                    offset,
                    requested,
                    record: WriteRecord {
                        count: completed.count,
                        committed: completed.committed,
                        // A completed record without a verifier was stable on
                        // arrival; `needs_commit` is false, so the placeholder is
                        // never compared against anything.
                        verifier: completed
                            .verifier
                            .unwrap_or(crate::transport::WriteVerifier([0; 8])),
                    },
                    replayed: true,
                });
            }
            Admission::Recorded(ReplayOutcome::Failed(retained)) => {
                return Err(retained.error().clone())
            }
            Admission::Indeterminate => {
                return Err(FacadeError::Replay(
                    crate::error::ReplayError::Indeterminate,
                ))
            }
        }

        let stateid = self.stateid()?;
        let reply = match transport.write(
            self.file.handle(),
            stateid,
            offset,
            stability,
            data,
            deadline,
        ) {
            Ok(reply) => reply,
            Err(error) => {
                // The failure becomes the settled answer for this key. Recording
                // it volatile is honest: an in-memory log does not survive the
                // restart a durable one would.
                let _ = replay.record(
                    &identity.key,
                    ReplayOutcome::Failed(RetainedError::record(
                        identity.operation,
                        identity.key.clone(),
                        error.clone(),
                        false,
                    )),
                );
                return Err(error);
            }
        };
        let record = WriteRecord::from_reply(&reply);
        let noted = note_write(replay, &identity.key, &record);
        replay
            .record(
                &identity.key,
                ReplayOutcome::Completed(CompletedWrite {
                    count: record.count,
                    committed: record.committed,
                    verifier: noted.then_some(record.verifier),
                }),
            )
            .map_err(FacadeError::Replay)?;
        Ok(WriteTicket {
            key: identity.key.clone(),
            offset,
            requested,
            record,
            replayed: false,
        })
    }

    /// Commit an unstable write and prove the server did not lose it.
    ///
    /// A write the server already committed at `DATA_SYNC` or `FILE_SYNC` owes no
    /// COMMIT and returns immediately. An `UNSTABLE` write is durable only when
    /// COMMIT returns the verifier the WRITE did.
    pub fn commit(
        &self,
        transport: &mut dyn RawTransport,
        replay: &dyn ReplayLog,
        ticket: &WriteTicket,
        deadline: Deadline,
    ) -> FacadeResult<VerifierMatch> {
        if !ticket.record.needs_commit() {
            return Ok(VerifierMatch::Match);
        }
        let reply = transport.commit(
            self.file.handle(),
            ticket.offset,
            ticket.record.count,
            deadline,
        )?;
        let verdict = check_commit(replay, &ticket.key, &reply);
        verdict.into_result()?;
        Ok(verdict)
    }

    /// Close the open, consuming it so a closed stateid cannot be reused.
    ///
    /// This is the point at which the server may release an object whose last name
    /// was removed while the open was live.
    pub fn close(self, transport: &mut dyn RawTransport, deadline: Deadline) -> CloseOutcome {
        close(self.file, transport, deadline)
    }
}

/// One positional write, grouped so its three parameters travel together.
///
/// The same shape the frozen transport uses for
/// [`ReadDirRequest`](crate::transport::ReadDirRequest), and for the same reason:
/// an offset, a stability and a payload cannot be transposed at a call site when
/// they arrive as one named value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteAt {
    /// Absolute byte offset.
    pub offset: u64,
    /// Stability to request. `UNSTABLE` owes a later COMMIT.
    pub stability: Stability,
    /// Owned payload.
    pub data: Vec<u8>,
}

/// What one WRITE achieved, and what a COMMIT for it must prove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteTicket {
    key: IdempotencyKey,
    offset: u64,
    requested: usize,
    record: WriteRecord,
    replayed: bool,
}

impl WriteTicket {
    /// Bytes the server accepted. Never rounded up to the request.
    pub fn count(&self) -> u32 {
        self.record.count
    }

    /// Whether the server accepted fewer bytes than were offered.
    pub fn is_short(&self) -> bool {
        self.record.is_short(self.requested)
    }

    /// Stability the server actually reached.
    pub fn committed(&self) -> Stability {
        self.record.committed
    }

    /// Whether this write still owes a COMMIT before it is durable.
    pub fn needs_commit(&self) -> bool {
        self.record.needs_commit()
    }

    /// Whether the answer came from the replay ledger rather than the wire.
    pub fn is_replayed(&self) -> bool {
        self.replayed
    }

    /// Absolute offset the write targeted.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The idempotency key this write is the settled answer for.
    pub fn key(&self) -> &IdempotencyKey {
        &self.key
    }
}

/// Read an object without holding an open state.
///
/// The all-zero special stateid authorises an anonymous read, which is what a
/// `Stat`-adjacent read of a path wants: no open-owner is minted, no seqid is
/// consumed, and nothing has to be closed. Writes never take this path — a write
/// needs a real open stateid, and a caller that has none has no authority to
/// write.
pub fn read_anonymous(
    transport: &mut dyn RawTransport,
    object: &PinnedObject,
    offset: u64,
    count: u32,
    deadline: Deadline,
) -> FacadeResult<ReadReply> {
    transport.read(object.handle(), Stateid::ANONYMOUS, offset, count, deadline)
}

/// What [`read_whole`] recovered, and whether that is the whole object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeRead {
    /// Bytes accumulated, never more than the caller's `limit`.
    pub data: Vec<u8>,
    /// Whether end of file was reached inside `limit`.
    ///
    /// `false` means the object has more bytes than the caller allowed. It is a
    /// *bound* answer, not a failure: callers whose format has a fixed maximum
    /// treat it as "this is not a file I wrote" and refuse with their own
    /// diagnosis, which keeps the refusal prose where the format lives.
    pub complete: bool,
}

/// Read one whole small object into memory, following short replies to EOF.
///
/// **F04 / F07.** A single `READ` is not a whole file.
/// [RFC 7530 §16.25.4](https://www.rfc-editor.org/rfc/rfc7530.html#section-16.25.4)
/// lets a server return fewer bytes than requested *without* setting `eof` — it
/// is a legal reply, not a fault — so a reader that decodes the first reply as
/// the entire object turns a healthy file into a corrupt-file refusal. Two
/// readers in this crate made exactly that mistake against two different files
/// that gate a run opening at all: `.provider/epoch` and the admission marker.
///
/// The loop advances the offset by what actually arrived and asks only for the
/// capacity that remains, so it can neither overrun `limit` nor re-read bytes it
/// already holds. A reply that carries no bytes *and* no end of file is refused
/// rather than looped on: the server is not advancing, and a reader that spun
/// there would hang a run open on a file it cannot finish.
///
/// `stateid` is the caller's: an anonymous read for a path that holds no open
/// state, and a real open stateid where one is held.
pub fn read_whole(
    transport: &mut dyn RawTransport,
    handle: &FileHandle,
    stateid: Stateid,
    limit: u32,
    deadline: Deadline,
) -> FacadeResult<WholeRead> {
    // One chunk never exceeds what the transport will decode, so a chunk is
    // never refused for being too large to reply to.
    //
    // **R2-02.** The budget is not all payload. The reply carries the echoed
    // [`READ_TAG`](crate::transport::READ_TAG) too, and the raw decoder charges
    // both against the same figure, so asking for the whole budget asks for a
    // reply that cannot fit inside it: under a 128-byte budget a full reply costs
    // 132 and is refused as malformed. `max_read_payload` is what remains.
    let available = transport.limits().max_read_payload();
    if available == 0 {
        // No chunk size makes progress here, and there is nothing to be gained by
        // dispatching to find that out. Rounding back up to one byte would issue
        // exactly the over-budget request the reservation prevents, and asking for
        // zero bytes would loop forever without advancing.
        return Err(FacadeError::Transport(TransportError::Malformed(format!(
            "the transport's {}-byte reply budget leaves no room for READ data \
             beside the {}-byte COMPOUND tag it charges against the same bound",
            transport.limits().max_reply_bytes,
            crate::transport::READ_TAG.len()
        ))));
    }
    let chunk = u32::try_from(available).unwrap_or(u32::MAX).min(limit);

    let mut data: Vec<u8> = Vec::new();
    loop {
        let read = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let remaining = limit.saturating_sub(read);
        if remaining == 0 {
            // At the bound with no end of file yet. Whether that is acceptable
            // is the caller's format question, not this loop's.
            return Ok(WholeRead {
                data,
                complete: false,
            });
        }
        let want = chunk.min(remaining);
        let reply = transport.read(handle, stateid, u64::from(read), want, deadline)?;
        let progressed = !reply.data.is_empty();
        // A server that answers with more than it was asked for is not one this
        // reader can account for: taking the excess would silently exceed the
        // bound the caller set.
        if reply.data.len() > want as usize {
            return Err(FacadeError::Transport(TransportError::Malformed(format!(
                "READ returned {} bytes for a {want}-byte request at offset {read}",
                reply.data.len()
            ))));
        }
        data.extend_from_slice(&reply.data);
        if reply.eof {
            return Ok(WholeRead {
                data,
                complete: true,
            });
        }
        if !progressed {
            return Err(FacadeError::Transport(TransportError::Malformed(format!(
                "READ returned no bytes and no end of file at offset {read}; the object \
                 could not be read whole"
            ))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ReplayError;
    use crate::fixture;
    use crate::state::open_owner::CloseOutcome;
    use crate::state::verifier::create_verifier_for;
    use crate::transport::{Deadline, WriteVerifier};
    use umbra_core::OperationId;
    use uuid::Uuid;

    fn deadline() -> Deadline {
        Deadline { millis: 5_000 }
    }

    fn root(fake: &mut crate::fake::FakeTransport, layout: &fixture::Layout) -> PinnedObject {
        PinnedObject::pin(fake, layout.root.clone(), deadline()).expect("pin the root anchor")
    }

    fn name(bytes: &[u8]) -> ComponentName {
        ComponentName::new(bytes.to_vec()).expect("component")
    }

    fn open(
        owners: &mut OpenOwnerRegistry,
        fake: &mut crate::fake::FakeTransport,
        parent: &PinnedObject,
        component: &[u8],
        disposition: CreateDisposition,
        access: ShareAccess,
    ) -> FacadeResult<OpenObject> {
        OpenObject::open(
            owners,
            fake,
            parent,
            &name(component),
            disposition,
            access,
            deadline(),
        )
    }

    fn identity(key: &str) -> MutationIdentity {
        MutationIdentity::from_context(&fixture::context(key, Some(1))).expect("an epoch")
    }

    #[test]
    fn a_mutation_without_a_writer_epoch_is_refused_before_anything_happens() {
        let error = MutationIdentity::from_context(&fixture::context("k", None)).unwrap_err();
        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::NoWriterEpoch)
        ));
    }

    #[test]
    fn an_exclusive_create_replays_under_its_verifier_and_collides_under_another() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mine = create_verifier_for(OperationId(Uuid::from_u128(1)));
        let theirs = create_verifier_for(OperationId(Uuid::from_u128(2)));
        assert_ne!(mine, theirs);

        let first = open(
            &mut owners,
            &mut fake,
            &parent,
            b"exclusive",
            CreateDisposition::CreateExclusive { verifier: mine },
            ShareAccess::BOTH,
        )
        .expect("the create succeeds");

        // The same verifier again is a recognised replay of the same create, not
        // a second object and not a collision.
        let replay = open(
            &mut owners,
            &mut fake,
            &parent,
            b"exclusive",
            CreateDisposition::CreateExclusive { verifier: mine },
            ShareAccess::BOTH,
        )
        .expect("a replay of my own create is recognised");
        assert_eq!(replay.identity(), first.identity());

        // A different verifier on an existing name is a genuine collision.
        let error = open(
            &mut owners,
            &mut fake,
            &parent,
            b"exclusive",
            CreateDisposition::CreateExclusive { verifier: theirs },
            ShareAccess::BOTH,
        )
        .unwrap_err();
        assert_eq!(error.status(), Some(crate::error::Nfs4Status::EXIST));
    }

    #[test]
    fn a_guarded_create_refuses_an_existing_name_and_open_existing_refuses_an_absent_one() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        assert!(open(
            &mut owners,
            &mut fake,
            &parent,
            b"fresh",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .is_ok());
        let error = open(
            &mut owners,
            &mut fake,
            &parent,
            b"fresh",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .unwrap_err();
        assert_eq!(error.status(), Some(crate::error::Nfs4Status::EXIST));

        let error = open(
            &mut owners,
            &mut fake,
            &parent,
            b"absent",
            CreateDisposition::OpenExisting,
            ShareAccess::READ,
        )
        .unwrap_err();
        assert_eq!(error.status(), Some(crate::error::Nfs4Status::NOENT));
        assert!(CreateDisposition::OpenExisting.creates().eq(&false));
        assert!(CreateDisposition::CreateNew { mode: 0 }.creates());
    }

    #[test]
    fn a_short_write_is_reported_as_it_happened_and_never_rounded_up() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        fake.set_write_cap(Some(3));
        let file = open(
            &mut owners,
            &mut fake,
            &parent,
            b"short",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .expect("create");
        let ticket = file
            .write(
                &mut fake,
                &mut replay,
                &identity("short-write"),
                WriteAt {
                    offset: 0,
                    stability: Stability::Unstable,
                    data: b"0123456789".to_vec(),
                },
                deadline(),
            )
            .expect("the server accepted a prefix");
        assert_eq!(ticket.count(), 3);
        assert!(ticket.is_short());
        assert!(!ticket.is_replayed());
        assert_eq!(ticket.offset(), 0);
    }

    #[test]
    fn an_unstable_write_is_durable_only_when_the_commit_verifier_matches() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        let file = open(
            &mut owners,
            &mut fake,
            &parent,
            b"unstable",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .expect("create");
        let ticket = file
            .write(
                &mut fake,
                &mut replay,
                &identity("unstable-1"),
                WriteAt {
                    offset: 0,
                    stability: Stability::Unstable,
                    data: b"payload".to_vec(),
                },
                deadline(),
            )
            .expect("write");
        assert!(ticket.needs_commit());
        assert_eq!(ticket.committed(), Stability::Unstable);
        assert_eq!(
            file.commit(&mut fake, &replay, &ticket, deadline())
                .expect("an unrotated verifier commits"),
            VerifierMatch::Match
        );

        // A server that lost its unstable data returns a different verifier. The
        // bytes must be rewritten from the retained payload, and saying so is a
        // typed replay error rather than a generic I/O failure.
        let second = file
            .write(
                &mut fake,
                &mut replay,
                &identity("unstable-2"),
                WriteAt {
                    offset: 8,
                    stability: Stability::Unstable,
                    data: b"more".to_vec(),
                },
                deadline(),
            )
            .expect("write");
        fake.rotate_write_verifier(WriteVerifier([0x77; 8]));
        let error = file
            .commit(&mut fake, &replay, &second, deadline())
            .unwrap_err();
        assert!(matches!(
            error,
            FacadeError::Replay(ReplayError::VerifierChanged { .. })
        ));
    }

    #[test]
    fn a_file_sync_write_owes_no_commit_and_records_no_verifier() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        let file = open(
            &mut owners,
            &mut fake,
            &parent,
            b"stable",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .expect("create");
        let ticket = file
            .write(
                &mut fake,
                &mut replay,
                &identity("stable-1"),
                WriteAt {
                    offset: 0,
                    stability: Stability::FileSync,
                    data: b"payload".to_vec(),
                },
                deadline(),
            )
            .expect("write");
        assert!(!ticket.needs_commit());
        // Recording a verifier for a write that never owes a COMMIT would turn a
        // later `Unknown` into something a caller could misread as durability.
        assert_eq!(
            replay.check_commit_verifier(ticket.key(), WriteVerifier([0; 8])),
            VerifierMatch::Unknown
        );
        assert_eq!(
            file.commit(&mut fake, &replay, &ticket, deadline())
                .expect("nothing to prove"),
            VerifierMatch::Match
        );
    }

    #[test]
    fn a_settled_key_is_answered_from_the_ledger_without_dispatching_again() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        let file = open(
            &mut owners,
            &mut fake,
            &parent,
            b"retried",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .expect("create");
        let identity = identity("retried-write");
        let request = || WriteAt {
            offset: 0,
            stability: Stability::Unstable,
            data: b"payload".to_vec(),
        };
        let first = file
            .write(&mut fake, &mut replay, &identity, request(), deadline())
            .expect("write");
        assert_eq!(first.count(), 7);
        assert!(!first.is_replayed());

        // Make a real dispatch impossible to mistake for a replay: a second trip
        // to the wire would now report zero bytes.
        fake.set_write_cap(Some(0));
        let second = file
            .write(&mut fake, &mut replay, &identity, request(), deadline())
            .expect("the recorded outcome is the answer");
        assert!(second.is_replayed());
        assert_eq!(second.count(), 7, "a retry returns the settled count");
    }

    #[test]
    fn reusing_a_key_with_a_different_payload_is_refused() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        let file = open(
            &mut owners,
            &mut fake,
            &parent,
            b"conflict",
            CreateDisposition::CreateNew { mode: 0o600 },
            ShareAccess::BOTH,
        )
        .expect("create");
        let identity = identity("one-key");
        file.write(
            &mut fake,
            &mut replay,
            &identity,
            WriteAt {
                offset: 0,
                stability: Stability::Unstable,
                data: b"first".to_vec(),
            },
            deadline(),
        )
        .expect("write");
        let error = file
            .write(
                &mut fake,
                &mut replay,
                &identity,
                WriteAt {
                    offset: 0,
                    stability: Stability::Unstable,
                    data: b"second".to_vec(),
                },
                deadline(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            FacadeError::Replay(ReplayError::KeyConflict)
        ));
    }

    #[test]
    fn an_in_place_edit_by_another_writer_is_visible_through_an_open_handle() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        let mut replay = fixture::replay();
        fake.insert_file(&layout.root, b"shared", b"before".to_vec());

        let reader = open(
            &mut owners,
            &mut fake,
            &parent,
            b"shared",
            CreateDisposition::OpenExisting,
            ShareAccess::READ,
        )
        .expect("open for reading");
        let pinned = reader.identity();
        assert_eq!(
            reader
                .read(&mut fake, 0, 32, deadline())
                .expect("read")
                .data,
            b"before"
        );

        // A second open of the same object stands in for another writer editing
        // it in place. The fake is one server, so both opens address one object.
        let writer = open(
            &mut owners,
            &mut fake,
            &parent,
            b"shared",
            CreateDisposition::OpenExisting,
            ShareAccess::WRITE,
        )
        .expect("open for writing");
        assert_eq!(writer.identity(), pinned, "one name, one object");
        writer
            .write(
                &mut fake,
                &mut replay,
                &identity("in-place"),
                WriteAt {
                    offset: 0,
                    stability: Stability::FileSync,
                    data: b"after!".to_vec(),
                },
                deadline(),
            )
            .expect("write");

        // The first handle is still bound to the same object and sees the edit.
        assert_eq!(
            reader
                .read(&mut fake, 0, 32, deadline())
                .expect("read")
                .data,
            b"after!"
        );
        assert_eq!(reader.identity(), pinned);
    }

    #[test]
    fn a_pathname_replacement_leaves_an_open_handle_on_the_original_object() {
        let (mut fake, layout) = fixture::server();
        let parent = root(&mut fake, &layout);
        let mut owners = fixture::owners();
        fake.insert_file(&layout.root, b"target", b"original".to_vec());

        let held = open(
            &mut owners,
            &mut fake,
            &parent,
            b"target",
            CreateDisposition::OpenExisting,
            ShareAccess::READ,
        )
        .expect("open");
        let original = held.identity();
        assert_eq!(held.opened_as().as_bytes(), b"target");

        // Rebind the name to a different object, which is what an atomic
        // pathname replacement leaves behind.
        fake.insert_file(&layout.root, b"target", b"replacement".to_vec());

        // The open is not retargeted: it still reads the original bytes and still
        // reports the original identity, because nothing re-resolves the name.
        assert_eq!(
            held.read(&mut fake, 0, 32, deadline()).expect("read").data,
            b"original"
        );
        assert_eq!(held.identity(), original);

        // A new open of the same name finds the replacement, with its own identity.
        let fresh = open(
            &mut owners,
            &mut fake,
            &parent,
            b"target",
            CreateDisposition::OpenExisting,
            ShareAccess::READ,
        )
        .expect("open");
        assert_ne!(fresh.identity(), original);
        assert_eq!(
            fresh.read(&mut fake, 0, 32, deadline()).expect("read").data,
            b"replacement"
        );

        // The retained object survives until CLOSE consumes the open state.
        assert!(matches!(
            held.close(&mut fake, deadline()),
            CloseOutcome::Closed(_)
        ));
    }

    #[test]
    fn an_anonymous_read_needs_no_open_state() {
        let (mut fake, layout) = fixture::server();
        let handle = fake.insert_file(&layout.root, b"plain", b"bytes".to_vec());
        let pin = PinnedObject::pin(&mut fake, handle, deadline()).expect("pin");
        let reply = read_anonymous(&mut fake, &pin, 0, 16, deadline()).expect("read");
        assert_eq!(reply.data, b"bytes");
        assert!(reply.eof);
    }
}
