//! Fake facade: in-memory implementations of the transport and replay seams.
//!
//! `raw_state` and `authority_recovery` develop against these while `raw_rpc`
//! builds the real transport. The seam is the trait, so swapping [`FakeTransport`]
//! for the real implementation changes a constructor argument and nothing else.
//!
//! This is a **shape fake**, not a server model. It answers the operations M1
//! needs with plausible, self-consistent replies and models the few behaviours the
//! acceptance criteria turn on — OPEN_CONFIRM, exclusive-create verifier reuse,
//! short writes, write/commit verifiers, grace and reclaim, cookie invalidation.
//! Everything else answers `NFS4ERR_NOTSUPP` rather than pretending.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroU64;

use umbra_core::{IdempotencyKey, OperationId};

use crate::error::{Nfs4Status, ProtocolError, ReplayError, TransportError};
use crate::handle::{ClientId, FileHandle, ObjectIdentity, Stateid};
use crate::replay::{
    Admission, Backpressure, ReplayBudget, ReplayIntent, ReplayLog, ReplayOutcome, ReplayRecord,
    VerifierMatch,
};
use crate::transport::{
    AttrMask, AttrValues, Attributes, CallToken, ChangeInfo, CloseReply, CommitReply, Compound,
    CompoundReply, ConnectionEpoch, ConnectionState, CreateType, Deadline, DelegationType,
    DirCookie, DirEntry, DirPage, DirVerifier, FaultAction, FaultContext, FaultPlan, FaultPoint,
    Fsid, LockReply, Nfs4Op, Nfs4Time, Nfs4Type, NoFaults, OpCode, OpReply, OpenClaim, OpenHow,
    OpenReply, RawTransport, ReadReply, Retirement, SetClientIdReply, TransportLimits,
    TransportResult, Verifier, WireProfile, WriteReply, WriteVerifier,
};

/// One in-memory object.
#[derive(Clone, Debug)]
struct FakeObject {
    identity: ObjectIdentity,
    kind: Nfs4Type,
    data: Vec<u8>,
    mode: u32,
    children: BTreeMap<Vec<u8>, usize>,
    /// Verifier of the `EXCLUSIVE4` create that made this object, if any.
    create_verifier: Option<Verifier>,
    /// `FATTR4_CHANGE`. Bumped by every mutation of this object, which is what
    /// lets a `change_info4` pair describe a namespace change rather than
    /// repeating a constant.
    change: u64,
    /// `FATTR4_OWNER`, verbatim bytes. Settable, so SETATTR has somewhere to land.
    owner: Vec<u8>,
    /// `FATTR4_OWNER_GROUP`, verbatim bytes.
    owner_group: Vec<u8>,
    /// `FATTR4_TIME_ACCESS`.
    time_access: Nfs4Time,
    /// `FATTR4_TIME_MODIFY`.
    time_modify: Nfs4Time,
}

/// A scriptable in-memory transport implementing [`RawTransport`].
pub struct FakeTransport {
    objects: Vec<FakeObject>,
    next_fileid: u64,
    fsid: Fsid,
    epoch: ConnectionEpoch,
    state: ConnectionState,
    limits: TransportLimits,
    faults: Box<dyn FaultPlan>,
    next_token: u64,
    live: HashSet<u64>,
    verifier: WriteVerifier,
    dir_verifier: DirVerifier,
    next_client_id: u64,
    confirmed_clients: BTreeSet<u64>,
    confirmed_owners: BTreeSet<Vec<u8>>,
    stateid_owners: HashMap<[u8; 12], Vec<u8>>,
    next_stateid: u32,
    /// Whether the server is in its grace window. Controls `CLAIM_PREVIOUS`.
    in_grace: bool,
    /// Byte cap applied to every WRITE, so short writes can be exercised.
    write_cap: Option<u32>,
}

impl std::fmt::Debug for FakeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeTransport")
            .field("objects", &self.objects.len())
            .field("state", &self.state)
            .field("in_grace", &self.in_grace)
            .finish()
    }
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTransport {
    /// A connected fake holding only an empty export root.
    pub fn new() -> Self {
        let fsid = Fsid {
            major: 0x756d_6272,
            minor: 1,
        };
        let root = FakeObject {
            identity: ObjectIdentity { fsid, fileid: 1 },
            kind: Nfs4Type::Directory,
            data: Vec::new(),
            mode: 0o700,
            children: BTreeMap::new(),
            create_verifier: None,
            change: 1,
            owner: b"0".to_vec(),
            owner_group: b"0".to_vec(),
            time_access: Nfs4Time::default(),
            time_modify: Nfs4Time::default(),
        };
        Self {
            objects: vec![root],
            next_fileid: 2,
            fsid,
            epoch: ConnectionEpoch(1),
            state: ConnectionState::Connected(ConnectionEpoch(1)),
            limits: TransportLimits {
                max_inflight: 1,
                max_queue_depth: 8,
                max_reply_bytes: 1024 * 1024,
                default_deadline: Deadline { millis: 5_000 },
            },
            faults: Box::new(NoFaults),
            next_token: 1,
            live: HashSet::new(),
            verifier: WriteVerifier([0xA1; 8]),
            dir_verifier: DirVerifier([0xD1; 8]),
            next_client_id: 100,
            confirmed_clients: BTreeSet::new(),
            confirmed_owners: BTreeSet::new(),
            stateid_owners: HashMap::new(),
            next_stateid: 1,
            in_grace: false,
            write_cap: None,
        }
    }

    /// The export root filehandle.
    pub fn root(&self) -> FileHandle {
        handle_for(0)
    }

    /// Create a directory under `parent`, returning its filehandle.
    pub fn insert_directory(&mut self, parent: &FileHandle, name: &[u8]) -> FileHandle {
        self.insert(parent, name, Nfs4Type::Directory, Vec::new())
    }

    /// Create a regular file under `parent` with `data`.
    pub fn insert_file(&mut self, parent: &FileHandle, name: &[u8], data: Vec<u8>) -> FileHandle {
        self.insert(parent, name, Nfs4Type::Regular, data)
    }

    /// Put the fake into or out of its grace window.
    pub fn set_grace(&mut self, in_grace: bool) {
        self.in_grace = in_grace;
    }

    /// Cap every WRITE at `cap` bytes, producing short writes.
    pub fn set_write_cap(&mut self, cap: Option<u32>) {
        self.write_cap = cap;
    }

    /// Rotate the write/commit verifier, as a server restart would.
    pub fn rotate_write_verifier(&mut self, verifier: WriteVerifier) {
        self.verifier = verifier;
    }

    /// Rotate the READDIR cookie verifier, invalidating outstanding cookies.
    pub fn rotate_dir_verifier(&mut self, verifier: DirVerifier) {
        self.dir_verifier = verifier;
    }

    /// Tokens still registered with the pump. A correct `submit` leaves none.
    pub fn live_calls(&self) -> usize {
        self.live.len()
    }

    fn insert(
        &mut self,
        parent: &FileHandle,
        name: &[u8],
        kind: Nfs4Type,
        data: Vec<u8>,
    ) -> FileHandle {
        let index = self.allocate(kind, data, 0o600, None);
        let parent_index = index_of(parent).expect("fake parent filehandle");
        self.objects[parent_index]
            .children
            .insert(name.to_vec(), index);
        handle_for(index)
    }

    fn allocate(
        &mut self,
        kind: Nfs4Type,
        data: Vec<u8>,
        mode: u32,
        create_verifier: Option<Verifier>,
    ) -> usize {
        let fileid = self.next_fileid;
        self.next_fileid += 1;
        self.objects.push(FakeObject {
            identity: ObjectIdentity {
                fsid: self.fsid,
                fileid,
            },
            kind,
            data,
            mode,
            children: BTreeMap::new(),
            create_verifier,
            change: 1,
            owner: b"0".to_vec(),
            owner_group: b"0".to_vec(),
            time_access: Nfs4Time::default(),
            time_modify: Nfs4Time::default(),
        });
        self.objects.len() - 1
    }

    fn attributes(&self, index: usize, mask: AttrMask) -> Attributes {
        let object = &self.objects[index];
        Attributes {
            returned: mask,
            file_type: mask.contains(AttrMask::TYPE).then_some(object.kind),
            change: mask.contains(AttrMask::CHANGE).then_some(object.change),
            size: mask
                .contains(AttrMask::SIZE)
                .then_some(object.data.len() as u64),
            fsid: mask
                .contains(AttrMask::FSID)
                .then_some(object.identity.fsid),
            fileid: mask
                .contains(AttrMask::FILEID)
                .then_some(object.identity.fileid),
            numlinks: mask.contains(AttrMask::NUMLINKS).then_some(1),
            mode: mask.contains(AttrMask::MODE).then_some(object.mode),
            owner: mask.contains(AttrMask::OWNER).then(|| object.owner.clone()),
            owner_group: mask
                .contains(AttrMask::OWNER_GROUP)
                .then(|| object.owner_group.clone()),
            time_modify: mask
                .contains(AttrMask::TIME_MODIFY)
                .then_some(object.time_modify),
            lease_time: mask.contains(AttrMask::LEASE_TIME).then_some(90),
            rdattr_error: None,
        }
    }

    fn next_stateid(&mut self) -> Stateid {
        let seqid = self.next_stateid;
        self.next_stateid += 1;
        let mut other = [0u8; 12];
        other[..4].copy_from_slice(&seqid.to_be_bytes());
        Stateid { seqid, other }
    }

    fn evaluate(&mut self, call: &Compound) -> CompoundReply {
        let mut results = Vec::with_capacity(call.ops.len());
        let mut current: Option<usize> = None;
        // RENAME names its source directory through the saved filehandle, so the
        // slot is per-COMPOUND state exactly as the current filehandle is.
        let mut saved: Option<usize> = None;
        for (position, op) in call.ops.iter().enumerate() {
            let index = position as u32;
            let context = FaultContext {
                op: op.opcode(),
                index,
                token: None,
            };
            if let FaultAction::Substitute(status) =
                self.faults.decide(FaultPoint::AfterDispatch, context)
            {
                return failure(call, results, status, op.opcode(), index);
            }
            match self.apply(op, &mut current, &mut saved, index) {
                Ok(reply) => results.push(reply),
                Err(error) => {
                    return CompoundReply {
                        tag: call.tag.clone(),
                        results,
                        failure: Some(error),
                    }
                }
            }
        }
        CompoundReply {
            tag: call.tag.clone(),
            results,
            failure: None,
        }
    }

    fn apply(
        &mut self,
        op: &Nfs4Op,
        current: &mut Option<usize>,
        saved: &mut Option<usize>,
        index: u32,
    ) -> Result<OpReply, ProtocolError> {
        let fail = |status: Nfs4Status| ProtocolError {
            status,
            op: op.opcode(),
            index,
        };
        match op {
            Nfs4Op::PutRootFh => {
                *current = Some(0);
                Ok(OpReply::PutRootFh)
            }
            Nfs4Op::PutFh(handle) => {
                let target = index_of(handle)
                    .filter(|i| *i < self.objects.len())
                    .ok_or_else(|| fail(Nfs4Status::BADHANDLE))?;
                *current = Some(target);
                Ok(OpReply::PutFh)
            }
            Nfs4Op::GetFh => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                Ok(OpReply::GetFh(handle_for(target)))
            }
            Nfs4Op::GetAttr(mask) => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                Ok(OpReply::GetAttr(self.attributes(target, *mask)))
            }
            Nfs4Op::Lookup(name) => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[target].kind != Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::NOTDIR));
                }
                let child = *self.objects[target]
                    .children
                    .get(name.as_bytes())
                    .ok_or_else(|| fail(Nfs4Status::NOENT))?;
                *current = Some(child);
                Ok(OpReply::Lookup)
            }
            Nfs4Op::LookupParent => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                let parent = self
                    .objects
                    .iter()
                    .position(|object| object.children.values().any(|child| *child == target))
                    .unwrap_or(0);
                *current = Some(parent);
                Ok(OpReply::LookupParent)
            }
            Nfs4Op::ReadDir {
                cookie,
                verifier,
                max_count,
                attrs,
                ..
            } => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[target].kind != Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::NOTDIR));
                }
                if cookie.0 != 0 && *verifier != self.dir_verifier {
                    return Err(fail(Nfs4Status::BAD_COOKIE));
                }
                let names: Vec<(Vec<u8>, usize)> = self.objects[target]
                    .children
                    .iter()
                    .map(|(name, child)| (name.clone(), *child))
                    .collect();
                let start = cookie.0 as usize;
                if start > names.len() {
                    return Err(fail(Nfs4Status::BAD_COOKIE));
                }
                let room = (*max_count as usize / 128).max(1);
                let end = names.len().min(start + room);
                let entries = names[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, (name, child))| DirEntry {
                        cookie: DirCookie((start + offset + 1) as u64),
                        name: crate::transport::ComponentName::new(name.clone())
                            .expect("fake stores validated names"),
                        attributes: self.attributes(*child, *attrs),
                    })
                    .collect();
                Ok(OpReply::ReadDir(DirPage {
                    verifier: self.dir_verifier,
                    entries,
                    eof: end == names.len(),
                }))
            }
            Nfs4Op::Read { offset, count, .. } => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[target].kind == Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::ISDIR));
                }
                let data = &self.objects[target].data;
                let start = usize::try_from(*offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let end = data.len().min(start.saturating_add(*count as usize));
                Ok(OpReply::Read(ReadReply {
                    data: data[start..end].to_vec(),
                    eof: end == data.len(),
                }))
            }
            Nfs4Op::Write {
                offset,
                stability,
                data,
                ..
            } => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[target].kind == Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::ISDIR));
                }
                let accepted = self
                    .write_cap
                    .map_or(data.len(), |cap| data.len().min(cap as usize));
                let start = usize::try_from(*offset).map_err(|_| fail(Nfs4Status::INVAL))?;
                let object = &mut self.objects[target];
                if object.data.len() < start + accepted {
                    object.data.resize(start + accepted, 0);
                }
                object.data[start..start + accepted].copy_from_slice(&data[..accepted]);
                self.bump(target);
                Ok(OpReply::Write(WriteReply {
                    count: accepted as u32,
                    // The fake models protocol shape, not durability: it echoes the
                    // requested stability so both the UNSTABLE+COMMIT path and the
                    // FILE_SYNC path can be exercised. It is not a durability claim.
                    committed: *stability,
                    verifier: self.verifier,
                }))
            }
            Nfs4Op::Commit { .. } => {
                current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                Ok(OpReply::Commit(CommitReply {
                    verifier: self.verifier,
                }))
            }
            Nfs4Op::Open(args) => {
                let parent = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                let target = match &args.claim {
                    OpenClaim::Previous { .. } => {
                        if !self.in_grace {
                            return Err(fail(Nfs4Status::NO_GRACE));
                        }
                        parent
                    }
                    OpenClaim::Null { name } => {
                        self.open_by_name(parent, name.as_bytes(), &args.how, || {
                            fail(Nfs4Status::EXIST)
                        })?
                    }
                };
                let owner_key = args.owner.as_bytes().to_vec();
                let confirm_required = !self.confirmed_owners.contains(&owner_key);
                let stateid = self.next_stateid();
                self.stateid_owners.insert(stateid.other, owner_key);
                *current = Some(target);
                Ok(OpReply::Open(OpenReply {
                    stateid,
                    confirm_required,
                    change_atomic: true,
                    change_before: 0,
                    change_after: 1,
                    // No callback channel is offered, so no delegation is granted.
                    delegation: DelegationType::None,
                }))
            }
            Nfs4Op::OpenConfirm { stateid, .. } => {
                current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                let owner = self
                    .stateid_owners
                    .get(&stateid.other)
                    .ok_or_else(|| fail(Nfs4Status::BAD_STATEID))?
                    .clone();
                self.confirmed_owners.insert(owner);
                Ok(OpReply::OpenConfirm(*stateid))
            }
            Nfs4Op::Close { stateid, .. } => {
                current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                Ok(OpReply::Close(CloseReply { stateid: *stateid }))
            }
            Nfs4Op::Lock(_) => {
                current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                let stateid = self.next_stateid();
                Ok(OpReply::Lock(LockReply { stateid }))
            }
            Nfs4Op::Locku(args) => {
                current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                Ok(OpReply::Locku(args.lock_stateid))
            }
            Nfs4Op::Renew(client_id) => {
                if !self.confirmed_clients.contains(&client_id.0) {
                    return Err(fail(Nfs4Status::STALE_CLIENTID));
                }
                Ok(OpReply::Renew)
            }
            Nfs4Op::SetClientId(_) => {
                let client_id = ClientId(self.next_client_id);
                self.next_client_id += 1;
                Ok(OpReply::SetClientId(SetClientIdReply {
                    client_id,
                    confirm: Verifier([0xC0; 8]),
                }))
            }
            Nfs4Op::SetClientIdConfirm { client_id, .. } => {
                self.confirmed_clients.insert(client_id.0);
                Ok(OpReply::SetClientIdConfirm)
            }
            Nfs4Op::SaveFh => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                *saved = Some(target);
                Ok(OpReply::SaveFh)
            }
            Nfs4Op::Remove { name } => {
                let parent = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[parent].kind != Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::NOTDIR));
                }
                let victim = *self.objects[parent]
                    .children
                    .get(name.as_bytes())
                    .ok_or_else(|| fail(Nfs4Status::NOENT))?;
                // A non-empty directory is NFS4ERR_NOTEMPTY, never a silent
                // recursive delete.
                if self.objects[victim].kind == Nfs4Type::Directory
                    && !self.objects[victim].children.is_empty()
                {
                    return Err(fail(Nfs4Status::NOTEMPTY));
                }
                let before = self.objects[parent].change;
                self.objects[parent].children.remove(name.as_bytes());
                let after = self.bump(parent);
                // The object itself is deliberately NOT removed from `objects`:
                // an open handle keeps addressing it after its last name is
                // gone, which is the retention property CLOSE settles.
                Ok(OpReply::Remove(ChangeInfo {
                    atomic: true,
                    before,
                    after,
                }))
            }
            Nfs4Op::Rename { old_name, new_name } => {
                let source = saved.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[source].kind != Nfs4Type::Directory
                    || self.objects[target].kind != Nfs4Type::Directory
                {
                    return Err(fail(Nfs4Status::NOTDIR));
                }
                let moving = *self.objects[source]
                    .children
                    .get(old_name.as_bytes())
                    .ok_or_else(|| fail(Nfs4Status::NOENT))?;
                if let Some(existing) = self.objects[target].children.get(new_name.as_bytes()) {
                    // POSIX rename replaces a regular destination, but never
                    // replaces a non-empty directory and never crosses type.
                    let existing = *existing;
                    if self.objects[existing].kind == Nfs4Type::Directory
                        && !self.objects[existing].children.is_empty()
                    {
                        return Err(fail(Nfs4Status::NOTEMPTY));
                    }
                }
                let source_before = self.objects[source].change;
                let target_before = self.objects[target].change;
                self.objects[source].children.remove(old_name.as_bytes());
                self.objects[target]
                    .children
                    .insert(new_name.as_bytes().to_vec(), moving);
                // The moved object keeps its fileid: a rename moves a name, not
                // an object, so every open handle on it stays valid.
                let source_after = self.bump(source);
                let target_after = if source == target {
                    source_after
                } else {
                    self.bump(target)
                };
                Ok(OpReply::Rename {
                    source: ChangeInfo {
                        atomic: true,
                        before: source_before,
                        after: source_after,
                    },
                    target: ChangeInfo {
                        atomic: true,
                        before: target_before,
                        after: target_after,
                    },
                })
            }
            Nfs4Op::Create {
                object_type,
                name,
                attributes,
            } => {
                let parent = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                if self.objects[parent].kind != Nfs4Type::Directory {
                    return Err(fail(Nfs4Status::NOTDIR));
                }
                if self.objects[parent].children.contains_key(name.as_bytes()) {
                    return Err(fail(Nfs4Status::EXIST));
                }
                let before = self.objects[parent].change;
                let kind = match object_type {
                    CreateType::Directory => Nfs4Type::Directory,
                };
                let child = self.allocate(kind, Vec::new(), attributes.mode.unwrap_or(0o700), None);
                let attrset = self.set_attributes(child, attributes)?;
                self.objects[parent]
                    .children
                    .insert(name.as_bytes().to_vec(), child);
                let after = self.bump(parent);
                *current = Some(child);
                Ok(OpReply::Create {
                    info: ChangeInfo {
                        atomic: true,
                        before,
                        after,
                    },
                    attrset,
                })
            }
            Nfs4Op::SetAttr {
                attributes,
                stateid,
            } => {
                let target = current.ok_or_else(|| fail(Nfs4Status::NOFILEHANDLE))?;
                // RFC 7530 §16.32: setting FATTR4_SIZE needs an open stateid with
                // WRITE access. The fake enforces the shape so a caller that
                // truncates with the anonymous stateid is caught here, not on a
                // real server later.
                if attributes.size.is_some()
                    && *stateid == Stateid::ANONYMOUS
                    && self.objects[target].kind == Nfs4Type::Regular
                {
                    return Err(fail(Nfs4Status::BAD_STATEID));
                }
                if self.objects[target].kind == Nfs4Type::Directory && attributes.size.is_some() {
                    return Err(fail(Nfs4Status::ISDIR));
                }
                let attrset = self.set_attributes(target, attributes)?;
                self.bump(target);
                Ok(OpReply::SetAttr(attrset))
            }
        }
    }

    /// Bump an object's change value and report the new one.
    fn bump(&mut self, index: usize) -> u64 {
        let object = &mut self.objects[index];
        object.change += 1;
        object.change
    }

    /// Apply one [`AttrValues`] to an object, reporting the bits actually set.
    ///
    /// The returned mask is built from the fields that landed rather than echoed
    /// from the request, so a caller cannot read a set it did not get.
    fn set_attributes(
        &mut self,
        index: usize,
        values: &AttrValues,
    ) -> Result<AttrMask, ProtocolError> {
        let mut set = AttrMask::default();
        let object = &mut self.objects[index];
        if let Some(size) = values.size {
            let size = usize::try_from(size).map_err(|_| ProtocolError {
                status: Nfs4Status::INVAL,
                op: OpCode::SetAttr,
                index: 0,
            })?;
            object.data.resize(size, 0);
            set = set.union(AttrMask::SIZE);
        }
        if let Some(mode) = values.mode {
            object.mode = mode;
            set = set.union(AttrMask::MODE);
        }
        if let Some(owner) = &values.owner {
            object.owner.clone_from(owner);
            set = set.union(AttrMask::OWNER);
        }
        if let Some(group) = &values.owner_group {
            object.owner_group.clone_from(group);
            set = set.union(AttrMask::OWNER_GROUP);
        }
        if let Some(time) = values.time_access {
            object.time_access = time;
            set = set.union(AttrMask::TIME_ACCESS_SET);
        }
        if let Some(time) = values.time_modify {
            object.time_modify = time;
            set = set.union(AttrMask::TIME_MODIFY_SET);
        }
        Ok(set)
    }

    fn open_by_name(
        &mut self,
        parent: usize,
        name: &[u8],
        how: &OpenHow,
        exists: impl Fn() -> ProtocolError,
    ) -> Result<usize, ProtocolError> {
        let existing = self.objects[parent].children.get(name).copied();
        match (existing, how) {
            (Some(index), OpenHow::NoCreate | OpenHow::Unchecked { .. }) => Ok(index),
            (Some(_), OpenHow::Guarded { .. }) => Err(exists()),
            (Some(index), OpenHow::Exclusive { verifier }) => {
                // Verifier reuse is a recognised replay of the same create; a
                // different verifier on an existing name is a real collision.
                if self.objects[index].create_verifier == Some(*verifier) {
                    Ok(index)
                } else {
                    Err(exists())
                }
            }
            (None, OpenHow::NoCreate) => Err(ProtocolError {
                status: Nfs4Status::NOENT,
                op: OpCode::Open,
                index: 0,
            }),
            (None, how) => {
                let (mode, verifier) = match how {
                    OpenHow::Unchecked { mode } | OpenHow::Guarded { mode } => (*mode, None),
                    OpenHow::Exclusive { verifier } => (0o600, Some(*verifier)),
                    OpenHow::NoCreate => unreachable!("handled above"),
                };
                let index = self.allocate(Nfs4Type::Regular, Vec::new(), mode, verifier);
                self.objects[parent].children.insert(name.to_vec(), index);
                Ok(index)
            }
        }
    }
}

fn handle_for(index: usize) -> FileHandle {
    FileHandle::from_wire((index as u64).to_be_bytes().to_vec())
        .expect("eight bytes is a valid filehandle length")
}

fn index_of(handle: &FileHandle) -> Option<usize> {
    let bytes: [u8; 8] = handle.as_bytes().try_into().ok()?;
    usize::try_from(u64::from_be_bytes(bytes)).ok()
}

fn failure(
    call: &Compound,
    results: Vec<OpReply>,
    status: Nfs4Status,
    op: OpCode,
    index: u32,
) -> CompoundReply {
    CompoundReply {
        tag: call.tag.clone(),
        results,
        failure: Some(ProtocolError { status, op, index }),
    }
}

impl RawTransport for FakeTransport {
    fn wire_profile(&self) -> WireProfile {
        WireProfile::V40_TCP_SYS
    }

    fn limits(&self) -> TransportLimits {
        self.limits
    }

    fn connection(&self) -> ConnectionState {
        self.state
    }

    fn submit(&mut self, call: Compound, _deadline: Deadline) -> TransportResult<CompoundReply> {
        let first = call.ops.first().map_or(OpCode::PutRootFh, Nfs4Op::opcode);
        let context = FaultContext {
            op: first,
            index: 0,
            token: None,
        };
        // Backpressure is applied before the call is registered, never after.
        if self.live.len() as u32 >= self.limits.max_inflight {
            return Err(TransportError::QueueFull {
                depth: self.live.len() as u32,
                capacity: self.limits.max_inflight,
            });
        }
        if let FaultAction::Fail(error) = self.faults.decide(FaultPoint::BeforeDispatch, context) {
            return Err(error);
        }
        let token =
            CallToken::new(NonZeroU64::new(self.next_token).expect("token counter starts at one"));
        self.next_token += 1;
        self.live.insert(token.get());

        let dropped = matches!(
            self.faults.decide(
                FaultPoint::OnDeadline,
                FaultContext {
                    token: Some(token),
                    ..context
                },
            ),
            FaultAction::DropReply
        );
        if dropped {
            // The registration is withdrawn before the error is built, which is
            // the only way to obtain the Retirement the error requires.
            let retirement = self.cancel(token)?;
            return Err(TransportError::DeadlineExpired { retirement });
        }

        let reply = self.evaluate(&call);
        let action = self.faults.decide(
            FaultPoint::BeforeReturn,
            FaultContext {
                token: Some(token),
                ..context
            },
        );
        self.cancel(token)?;
        match action {
            FaultAction::Fail(error) => Err(error),
            FaultAction::RotateVerifier(verifier) => {
                self.verifier = verifier;
                Ok(reply)
            }
            _ => Ok(reply),
        }
    }

    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement> {
        // Withdrawal is synchronous here, so the call is always proven quiesced:
        // either this removed the live registration, or there was none left.
        self.live.remove(&token.get());
        Ok(Retirement::new(token, true))
    }

    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch> {
        self.epoch = ConnectionEpoch(self.epoch.0 + 1);
        self.state = ConnectionState::Connected(self.epoch);
        self.live.clear();
        Ok(self.epoch)
    }

    fn install_faults(&mut self, plan: Box<dyn FaultPlan>) {
        self.faults = plan;
    }
}

/// A fault plan that applies one scripted action at one point, then stops.
#[derive(Debug)]
pub struct ScriptedFault {
    point: FaultPoint,
    op: Option<OpCode>,
    action: Option<FaultAction>,
}

impl ScriptedFault {
    /// Fire `action` once, at `point`, optionally only for operation `op`.
    pub fn once(point: FaultPoint, op: Option<OpCode>, action: FaultAction) -> Box<Self> {
        Box::new(Self {
            point,
            op,
            action: Some(action),
        })
    }
}

impl FaultPlan for ScriptedFault {
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction {
        if point != self.point || self.op.is_some_and(|op| op != context.op) {
            return FaultAction::Proceed;
        }
        self.action.take().unwrap_or(FaultAction::Proceed)
    }
}

/// An in-memory [`ReplayLog`].
///
/// Records are held in memory, so [`RetainedError`](crate::error::RetainedError)
/// values it produces are marked non-durable. Consumers that gate replay on
/// durability therefore behave under the fake exactly as they will against a
/// durable log, instead of accidentally depending on volatile evidence.
#[derive(Debug)]
pub struct FakeReplayLog {
    records: HashMap<IdempotencyKey, ReplayRecord>,
    operations: HashMap<OperationId, IdempotencyKey>,
    verifiers: HashMap<IdempotencyKey, WriteVerifier>,
    payload_bytes: u64,
    budget: ReplayBudget,
}

impl FakeReplayLog {
    /// A log with the supplied budget.
    pub fn new(budget: ReplayBudget) -> Self {
        Self {
            records: HashMap::new(),
            operations: HashMap::new(),
            verifiers: HashMap::new(),
            payload_bytes: 0,
            budget,
        }
    }
}

impl Default for FakeReplayLog {
    fn default() -> Self {
        Self::new(ReplayBudget {
            max_records: 64,
            max_payload_bytes: 4 * 1024 * 1024,
        })
    }
}

impl ReplayLog for FakeReplayLog {
    fn admit(&mut self, intent: &ReplayIntent) -> Result<Admission, ReplayError> {
        if let Some(existing) = self.records.get(&intent.key) {
            if existing.intent != *intent {
                return Err(ReplayError::KeyConflict);
            }
            return Ok(match &existing.outcome {
                Some(outcome) => Admission::Recorded(outcome.clone()),
                None => Admission::Indeterminate,
            });
        }
        if let Some(key) = self.operations.get(&intent.operation) {
            if *key != intent.key {
                return Err(ReplayError::OperationReused);
            }
        }
        let cost = intent.payload.retained_len();
        if self.pressure().would_exceed(cost) {
            return Err(ReplayError::CapacityExhausted {
                records: self.records.len() as u32,
                bytes: self.payload_bytes,
            });
        }
        self.operations.insert(intent.operation, intent.key.clone());
        self.payload_bytes += cost;
        self.records.insert(
            intent.key.clone(),
            ReplayRecord {
                intent: intent.clone(),
                outcome: None,
            },
        );
        Ok(Admission::Fresh)
    }

    fn record(&mut self, key: &IdempotencyKey, outcome: ReplayOutcome) -> Result<(), ReplayError> {
        let record = self
            .records
            .get_mut(key)
            .ok_or(ReplayError::Indeterminate)?;
        if record.outcome.is_some() {
            return Err(ReplayError::KeyConflict);
        }
        record.outcome = Some(outcome);
        Ok(())
    }

    fn get(&self, key: &IdempotencyKey) -> Option<&ReplayRecord> {
        self.records.get(key)
    }

    fn retire(&mut self, key: &IdempotencyKey) -> Result<(), ReplayError> {
        let record = self.records.get(key).ok_or(ReplayError::Indeterminate)?;
        if record.outcome.is_none() {
            // An unsettled record is never dropped to make room.
            return Err(ReplayError::Indeterminate);
        }
        let cost = record.intent.payload.retained_len();
        let operation = record.intent.operation;
        self.payload_bytes = self.payload_bytes.saturating_sub(cost);
        self.records.remove(key);
        self.operations.remove(&operation);
        self.verifiers.remove(key);
        Ok(())
    }

    fn pressure(&self) -> Backpressure {
        Backpressure {
            records: self.records.len() as u32,
            payload_bytes: self.payload_bytes,
            budget: self.budget,
        }
    }

    fn note_write_verifier(&mut self, key: &IdempotencyKey, verifier: WriteVerifier) {
        self.verifiers.insert(key.clone(), verifier);
    }

    fn check_commit_verifier(
        &self,
        key: &IdempotencyKey,
        observed: WriteVerifier,
    ) -> VerifierMatch {
        VerifierMatch::compare(self.verifiers.get(key).copied(), observed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FacadeError;
    use crate::replay::{CompletedWrite, IntentKind, Payload};
    use crate::transport::{ComponentName, OpenArgs, ShareAccess, ShareDeny, Stability};
    use umbra_core::{LeaseEpoch, RunId};
    use uuid::Uuid;

    fn deadline() -> Deadline {
        Deadline { millis: 1_000 }
    }

    fn owner() -> crate::handle::OpenOwner {
        crate::handle::Session::establish(
            crate::handle::SessionId(1),
            ClientId(1),
            ConnectionEpoch(1),
        )
        .open_owner(b"owner".to_vec())
        .expect("a fresh session mints owners")
    }

    #[test]
    fn the_fake_answers_the_shape_helpers_the_real_transport_will() {
        let mut transport = FakeTransport::new();
        let root = transport.root();
        let dir = transport.insert_directory(&root, b"run");
        transport.insert_file(&dir, b"\xffname", b"hello".to_vec());

        let fetched = transport.root_filehandle(deadline()).unwrap();
        assert_eq!(fetched.as_bytes(), root.as_bytes());

        let name = ComponentName::new(b"\xffname".to_vec()).unwrap();
        let (handle, attrs) = transport
            .lookup(&dir, &name, AttrMask::STAT, deadline())
            .unwrap();
        assert_eq!(attrs.size, Some(5));
        let read = transport
            .read(&handle, Stateid::ANONYMOUS, 1, 3, deadline())
            .unwrap();
        assert_eq!(read.data, b"ell");
    }

    #[test]
    fn short_writes_and_verifiers_are_reported_as_they_happen() {
        let mut transport = FakeTransport::new();
        let root = transport.root();
        let file = transport.insert_file(&root, b"data", Vec::new());
        transport.set_write_cap(Some(2));
        let write = transport
            .write(
                &file,
                Stateid::ANONYMOUS,
                0,
                Stability::Unstable,
                vec![1, 2, 3, 4],
                deadline(),
            )
            .unwrap();
        assert_eq!(write.count, 2);
        assert_eq!(write.committed, Stability::Unstable);

        transport.rotate_write_verifier(WriteVerifier([0xFF; 8]));
        let commit = transport.commit(&file, 0, 0, deadline()).unwrap();
        assert_eq!(
            VerifierMatch::compare(Some(write.verifier), commit.verifier),
            VerifierMatch::Changed {
                recorded: write.verifier,
                observed: commit.verifier
            }
        );
    }

    #[test]
    fn exclusive_create_distinguishes_replay_from_collision() {
        let mut transport = FakeTransport::new();
        let root = transport.root();
        let args = |verifier: u8| OpenArgs {
            seqid: 0,
            share_access: ShareAccess::BOTH,
            share_deny: ShareDeny::NONE,
            owner: owner(),
            how: OpenHow::Exclusive {
                verifier: Verifier([verifier; 8]),
            },
            claim: OpenClaim::Null {
                name: ComponentName::new(b"exclusive".to_vec()).unwrap(),
            },
        };
        let (first, handle) = transport.open(&root, args(1), deadline()).unwrap();
        assert!(first.confirm_required, "a fresh owner must confirm");
        assert_eq!(
            transport.open(&root, args(1), deadline()).unwrap().1,
            handle
        );
        let collision = transport.open(&root, args(2), deadline()).unwrap_err();
        assert_eq!(collision.status(), Some(Nfs4Status::EXIST));
    }

    #[test]
    fn claim_previous_is_refused_outside_grace_and_no_v41_reclaim_exists() {
        let mut transport = FakeTransport::new();
        let root = transport.root();
        let args = OpenArgs {
            seqid: 0,
            share_access: ShareAccess::READ,
            share_deny: ShareDeny::NONE,
            owner: owner(),
            how: OpenHow::NoCreate,
            claim: OpenClaim::Previous {
                delegate_type: DelegationType::None,
            },
        };
        assert_eq!(
            transport
                .open(&root, args.clone(), deadline())
                .unwrap_err()
                .status(),
            Some(Nfs4Status::NO_GRACE)
        );
        transport.set_grace(true);
        assert!(transport.open(&root, args, deadline()).is_ok());
    }

    #[test]
    fn a_dropped_reply_retires_its_registration_before_reporting_a_deadline() {
        let mut transport = FakeTransport::new();
        transport.install_faults(ScriptedFault::once(
            FaultPoint::OnDeadline,
            None,
            FaultAction::DropReply,
        ));
        let error = transport.root_filehandle(deadline()).unwrap_err();
        match error {
            FacadeError::Transport(TransportError::DeadlineExpired { retirement }) => {
                assert!(retirement.drained());
            }
            other => panic!("expected a deadline failure, got {other}"),
        }
        assert_eq!(transport.live_calls(), 0, "no registration may survive");
    }

    #[test]
    fn a_recorded_outcome_is_returned_verbatim_and_never_redispatched() {
        let mut log = FakeReplayLog::default();
        let intent = ReplayIntent {
            run_id: RunId(Uuid::nil()),
            operation: OperationId(Uuid::nil()),
            key: IdempotencyKey("write-1".into()),
            epoch: LeaseEpoch(1),
            kind: IntentKind::Namespace {
                operation: "create".into(),
            },
            payload: Payload::Inline(vec![7; 16]),
        };
        assert_eq!(log.admit(&intent).unwrap(), Admission::Fresh);
        assert_eq!(log.admit(&intent).unwrap(), Admission::Indeterminate);
        let outcome = ReplayOutcome::Completed(CompletedWrite {
            count: 16,
            committed: Stability::FileSync,
            verifier: None,
        });
        log.record(&intent.key, outcome.clone()).unwrap();
        assert_eq!(log.admit(&intent).unwrap(), Admission::Recorded(outcome));
        assert_eq!(log.pressure().payload_bytes, 16);
        log.retire(&intent.key).unwrap();
        assert_eq!(log.pressure().payload_bytes, 0);
    }

    #[test]
    fn capacity_exhaustion_refuses_before_dispatch() {
        let mut log = FakeReplayLog::new(ReplayBudget {
            max_records: 1,
            max_payload_bytes: 8,
        });
        let intent = |n: u8| ReplayIntent {
            run_id: RunId(Uuid::nil()),
            operation: OperationId(Uuid::from_u128(n as u128)),
            key: IdempotencyKey(format!("key-{n}")),
            epoch: LeaseEpoch(1),
            kind: IntentKind::Namespace {
                operation: "create".into(),
            },
            payload: Payload::Inline(vec![0; 4]),
        };
        assert_eq!(log.admit(&intent(1)).unwrap(), Admission::Fresh);
        assert!(matches!(
            log.admit(&intent(2)),
            Err(ReplayError::CapacityExhausted { .. })
        ));
    }
}
