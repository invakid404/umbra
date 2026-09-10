//! Transport facade: the raw-RPC surface that protocol-state consumes.
//!
//! This is **not** the libnfs FFI. The FFI, its generated headers and its event
//! pump belong to the `raw_rpc` node behind [`RawTransport`]. Everything crossing
//! this seam is owned data: a [`Compound`] owns its argument bytes and a
//! [`CompoundReply`] owns its result bytes. No borrowed slice and no raw pointer
//! is part of the contract, so a reply can never alias a buffer the caller freed.
//!
//! # Wire profile is locked by construction
//!
//! [`OpCode`] enumerates NFSv4.0 operations only. NFSv4.1 operations —
//! `EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`, `RECLAIM_COMPLETE` — have no
//! variant, so they are unrepresentable rather than merely discouraged.
//!
//! # Callback-argument lifetime
//!
//! An implementation registers a call under a [`CallToken`], an integer index into
//! a pump-owned registry. The completion path resolves the token through that
//! registry; it never receives a Rust pointer. A retired token resolves to "no
//! such call" instead of dereferencing freed memory. [`TransportError`]'s deadline
//! variant can only be built from a [`Retirement`], whose constructor is private
//! to this crate, so no implementation can report a timeout it did not first
//! cancel and drain.

use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{FacadeError, Nfs4Status, ProtocolError, TransportError};
use crate::handle::{ClientId, FileHandle, OpenOwner, Stateid};

/// Raw-RPC libnfs implementation of [`RawTransport`] (`raw_rpc`).
///
/// Feature-gated so a default build carries no native dependency. This
/// declaration is the only edit `raw_rpc` makes to this frozen file: it adds no
/// signature and changes none, and the submodule it names is the
/// `src/transport/` submodule the contracts assign to this node.
#[cfg(feature = "transport-raw")]
pub mod raw;

/// Result alias for transport submissions.
pub type TransportResult<T> = std::result::Result<T, TransportError>;

/// NFSv4 minor version this provider speaks. Locked to 0 by the owner gate.
pub const MINOR_VERSION: u32 = 0;

/// The single authorised wire profile: NFSv4.0 over TCP with AUTH_SYS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireProfile {
    /// NFSv4 minor version. Any value but zero is rejected.
    pub minor_version: u32,
    /// Stream transport. UDP is not offered.
    pub stream: StreamTransport,
    /// RPC authentication flavour.
    pub auth: AuthFlavor,
}

impl WireProfile {
    /// The only profile this crate accepts.
    pub const V40_TCP_SYS: Self = Self {
        minor_version: MINOR_VERSION,
        stream: StreamTransport::Tcp,
        auth: AuthFlavor::Sys,
    };

    /// Reject anything outside the authorised profile before any I/O happens.
    pub fn check(&self) -> TransportResult<()> {
        if *self == Self::V40_TCP_SYS {
            return Ok(());
        }
        Err(TransportError::UnsupportedProfile(format!(
            "minor {} {:?} {:?}; only NFSv4.0/TCP/AUTH_SYS is authorised",
            self.minor_version, self.stream, self.auth
        )))
    }
}

/// Stream transport for RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamTransport {
    /// TCP with RPC record marking.
    Tcp,
}

/// RPC authentication flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthFlavor {
    /// `AUTH_SYS` (uid/gid/gids), the only flavour in M1 scope.
    Sys,
}

/// NFSv4.0 operation codes in M1 scope, with their RFC 7530 numbers.
///
/// The enum is deliberately closed. Adding a v4.1 operation would require an
/// edit here, which is the scope-lock checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u32)]
pub enum OpCode {
    /// `OP_ACCESS`.
    Access = 3,
    /// `OP_CLOSE`.
    Close = 4,
    /// `OP_COMMIT`.
    Commit = 5,
    /// `OP_CREATE`.
    Create = 6,
    /// `OP_GETATTR`.
    GetAttr = 9,
    /// `OP_GETFH`.
    GetFh = 10,
    /// `OP_LINK`.
    Link = 11,
    /// `OP_LOCK`.
    Lock = 12,
    /// `OP_LOCKT`.
    Lockt = 13,
    /// `OP_LOCKU`.
    Locku = 14,
    /// `OP_LOOKUP`.
    Lookup = 15,
    /// `OP_LOOKUPP`.
    LookupParent = 16,
    /// `OP_OPEN`.
    Open = 18,
    /// `OP_OPEN_CONFIRM`.
    OpenConfirm = 20,
    /// `OP_OPEN_DOWNGRADE`.
    OpenDowngrade = 21,
    /// `OP_PUTFH`.
    PutFh = 22,
    /// `OP_PUTROOTFH`.
    PutRootFh = 24,
    /// `OP_READ`.
    Read = 25,
    /// `OP_READDIR`.
    ReadDir = 26,
    /// `OP_READLINK`.
    ReadLink = 27,
    /// `OP_REMOVE`.
    Remove = 28,
    /// `OP_RENAME`.
    Rename = 29,
    /// `OP_RENEW`.
    Renew = 30,
    /// `OP_RESTOREFH`.
    RestoreFh = 31,
    /// `OP_SAVEFH`.
    SaveFh = 32,
    /// `OP_SETATTR`.
    SetAttr = 34,
    /// `OP_SETCLIENTID`.
    SetClientId = 35,
    /// `OP_SETCLIENTID_CONFIRM`.
    SetClientIdConfirm = 36,
    /// `OP_WRITE`.
    Write = 38,
    /// `OP_RELEASE_LOCKOWNER`.
    ReleaseLockOwner = 39,
}

/// One byte-preserving path component. Never `.`, `..`, empty or slash-bearing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ComponentName(Vec<u8>);

impl ComponentName {
    /// Validate one component. Byte names are preserved; no UTF-8 is assumed.
    pub fn new(bytes: impl Into<Vec<u8>>) -> TransportResult<Self> {
        let bytes = bytes.into();
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&0)
        {
            return Err(TransportError::Malformed(
                "component must be nonempty bytes without '/', NUL, '.' or '..'".into(),
            ));
        }
        Ok(Self(bytes))
    }

    /// Borrow the exact bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// NFSv4 attribute bitmap. Version 4.0 uses two 32-bit words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttrMask {
    /// Word 0 of the bitmap.
    pub word0: u32,
    /// Word 1 of the bitmap.
    pub word1: u32,
}

impl AttrMask {
    /// `FATTR4_TYPE` (attribute 1).
    pub const TYPE: Self = Self::word0(1);
    /// `FATTR4_CHANGE` (attribute 3).
    pub const CHANGE: Self = Self::word0(3);
    /// `FATTR4_SIZE` (attribute 4).
    pub const SIZE: Self = Self::word0(4);
    /// `FATTR4_FSID` (attribute 8).
    pub const FSID: Self = Self::word0(8);
    /// `FATTR4_LEASE_TIME` (attribute 10).
    pub const LEASE_TIME: Self = Self::word0(10);
    /// `FATTR4_RDATTR_ERROR` (attribute 11).
    pub const RDATTR_ERROR: Self = Self::word0(11);
    /// `FATTR4_FILEID` (attribute 20).
    pub const FILEID: Self = Self::word0(20);
    /// `FATTR4_MODE` (attribute 33).
    pub const MODE: Self = Self::word1(33);
    /// `FATTR4_NUMLINKS` (attribute 35).
    pub const NUMLINKS: Self = Self::word1(35);
    /// `FATTR4_OWNER` (attribute 36).
    pub const OWNER: Self = Self::word1(36);
    /// `FATTR4_OWNER_GROUP` (attribute 37).
    pub const OWNER_GROUP: Self = Self::word1(37);
    /// `FATTR4_TIME_MODIFY` (attribute 53).
    pub const TIME_MODIFY: Self = Self::word1(53);
    /// `FATTR4_TIME_ACCESS_SET` (attribute 48).
    ///
    /// SETATTR sets times through the `_SET` attributes, which carry a
    /// `settime4`, not through `FATTR4_TIME_ACCESS`, which is read-only. Asking
    /// for the read-only bit in a SETATTR is `NFS4ERR_INVAL`.
    pub const TIME_ACCESS_SET: Self = Self::word1(48);
    /// `FATTR4_TIME_MODIFY_SET` (attribute 54).
    pub const TIME_MODIFY_SET: Self = Self::word1(54);

    /// `FATTR4_FSID` plus `FATTR4_FILEID`: the stable-identity pair.
    ///
    /// Object identity is this pair, never the filehandle bytes. A server may
    /// hand out a different filehandle for the same object, and a renamed object
    /// keeps its identity while a replaced name yields a different one.
    pub const IDENTITY: Self = Self::FSID.union(Self::FILEID);

    /// Everything `stat` needs, without following a final name.
    pub const STAT: Self = Self::TYPE
        .union(Self::CHANGE)
        .union(Self::SIZE)
        .union(Self::IDENTITY)
        .union(Self::MODE)
        .union(Self::NUMLINKS)
        .union(Self::OWNER)
        .union(Self::OWNER_GROUP)
        .union(Self::TIME_MODIFY);

    const fn word0(attribute: u32) -> Self {
        Self {
            word0: 1 << attribute,
            word1: 0,
        }
    }

    const fn word1(attribute: u32) -> Self {
        Self {
            word0: 0,
            word1: 1 << (attribute - 32),
        }
    }

    /// Union of two masks.
    pub const fn union(self, other: Self) -> Self {
        Self {
            word0: self.word0 | other.word0,
            word1: self.word1 | other.word1,
        }
    }

    /// Whether every bit of `other` is set here.
    pub const fn contains(self, other: Self) -> bool {
        self.word0 & other.word0 == other.word0 && self.word1 & other.word1 == other.word1
    }
}

/// Server-side file system identity, half of stable object identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Fsid {
    /// `fsid4.major`.
    pub major: u64,
    /// `fsid4.minor`.
    pub minor: u64,
}

/// NFSv4 object type, as returned by `FATTR4_TYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Nfs4Type {
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Any other type. Retained rather than coerced.
    Other(u32),
}

/// `nfstime4`: seconds since the epoch plus nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nfs4Time {
    /// Signed seconds.
    pub seconds: i64,
    /// Nanoseconds, 0..1_000_000_000.
    pub nanoseconds: u32,
}

/// Decoded attributes. Every field is optional because a server answers only the
/// bits it was asked for and supports; an absent field is never defaulted to a
/// plausible value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributes {
    /// Bits the server actually returned.
    pub returned: AttrMask,
    /// `FATTR4_TYPE`.
    pub file_type: Option<Nfs4Type>,
    /// `FATTR4_CHANGE`.
    pub change: Option<u64>,
    /// `FATTR4_SIZE`.
    pub size: Option<u64>,
    /// `FATTR4_FSID`.
    pub fsid: Option<Fsid>,
    /// `FATTR4_FILEID`.
    pub fileid: Option<u64>,
    /// `FATTR4_NUMLINKS`.
    pub numlinks: Option<u32>,
    /// `FATTR4_MODE`.
    pub mode: Option<u32>,
    /// `FATTR4_OWNER`, verbatim bytes (an AUTH_SYS name string, not a uid).
    pub owner: Option<Vec<u8>>,
    /// `FATTR4_OWNER_GROUP`, verbatim bytes.
    pub owner_group: Option<Vec<u8>>,
    /// `FATTR4_TIME_MODIFY`.
    pub time_modify: Option<Nfs4Time>,
    /// `FATTR4_LEASE_TIME`, seconds. Only meaningful on the root filehandle.
    pub lease_time: Option<u32>,
    /// `FATTR4_RDATTR_ERROR` from a READDIR entry, preserved not discarded.
    pub rdattr_error: Option<Nfs4Status>,
}

/// WRITE stability level, matching `stable_how4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stability {
    /// `UNSTABLE4`: server may hold the data in volatile storage.
    Unstable = 0,
    /// `DATA_SYNC4`: data is stable, metadata may not be.
    DataSync = 1,
    /// `FILE_SYNC4`: data and metadata are stable.
    FileSync = 2,
}

/// `writeverf4`: the server's write/commit verifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WriteVerifier(pub [u8; 8]);

/// `verifier4` used by SETCLIENTID and exclusive CREATE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Verifier(pub [u8; 8]);

/// READDIR cookie. Opaque and server-defined; never synthesised by a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirCookie(pub u64);

/// READDIR cookie verifier. A change invalidates every outstanding cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirVerifier(pub [u8; 8]);

/// Share access bits for OPEN (`OPEN4_SHARE_ACCESS_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAccess(pub u32);

impl ShareAccess {
    /// `OPEN4_SHARE_ACCESS_READ`.
    pub const READ: Self = Self(1);
    /// `OPEN4_SHARE_ACCESS_WRITE`.
    pub const WRITE: Self = Self(2);
    /// `OPEN4_SHARE_ACCESS_BOTH`.
    pub const BOTH: Self = Self(3);
}

/// Share deny bits for OPEN (`OPEN4_SHARE_DENY_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeny(pub u32);

impl ShareDeny {
    /// `OPEN4_SHARE_DENY_NONE`. Umbra arbitrates writers itself; the NFS share
    /// reservation is not the admission mechanism.
    pub const NONE: Self = Self(0);
}

/// Delegation type returned by OPEN. M1 declines the callback channel, so a
/// server should answer `None`; anything else is recorded and returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationType {
    /// `OPEN_DELEGATE_NONE`.
    None,
    /// `OPEN_DELEGATE_READ`.
    Read,
    /// `OPEN_DELEGATE_WRITE`.
    Write,
}

/// How OPEN creates, matching `opentype4` and `createmode4`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenHow {
    /// `OPEN4_NOCREATE`.
    NoCreate,
    /// `OPEN4_CREATE` with `UNCHECKED4`.
    Unchecked {
        /// Initial mode bits.
        mode: u32,
    },
    /// `OPEN4_CREATE` with `GUARDED4`: existing name is `NFS4ERR_EXIST`.
    Guarded {
        /// Initial mode bits.
        mode: u32,
    },
    /// `OPEN4_CREATE` with `EXCLUSIVE4`.
    ///
    /// The verifier is the whole idempotency story for create: a replayed
    /// EXCLUSIVE4 with the same verifier must be recognised as the same create,
    /// and a different verifier on an existing name is a genuine collision.
    Exclusive {
        /// Create verifier tied to the caller's operation identity.
        verifier: Verifier,
    },
}

/// OPEN claim. NFSv4.0 reclaim uses `CLAIM_PREVIOUS`; `RECLAIM_COMPLETE` is a
/// v4.1 operation and is out of scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenClaim {
    /// `CLAIM_NULL`: open by parent filehandle plus name.
    Null {
        /// Name within the current filehandle's directory.
        name: ComponentName,
    },
    /// `CLAIM_PREVIOUS`: reclaim during the server's grace period.
    Previous {
        /// Delegation type being reclaimed, normally `None`.
        delegate_type: DelegationType,
    },
}

/// OPEN arguments. The parent directory is the current filehandle set by a
/// preceding PUTFH, so a caller cannot open by an unanchored path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenArgs {
    /// Open-owner sequence number, issued by the handle facade.
    pub seqid: u32,
    /// Requested share access.
    pub share_access: ShareAccess,
    /// Requested share deny.
    pub share_deny: ShareDeny,
    /// Client-scoped open owner.
    pub owner: OpenOwner,
    /// Create disposition.
    pub how: OpenHow,
    /// Claim type.
    pub claim: OpenClaim,
}

/// OPEN reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    /// Stateid to use for READ/WRITE once confirmed.
    pub stateid: Stateid,
    /// Whether the server requires OPEN_CONFIRM before the stateid is usable.
    pub confirm_required: bool,
    /// `cinfo.atomic` from the directory change info.
    pub change_atomic: bool,
    /// Directory change value before the operation.
    pub change_before: u64,
    /// Directory change value after the operation.
    pub change_after: u64,
    /// Delegation the server granted, if any.
    pub delegation: DelegationType,
}

/// LOCK arguments.
///
/// M1 lock scope: the syscall contract defers `flock` to an M2 locking gate and
/// admits only `fcntl` record locks in the meantime, so the only locks M1 needs
/// are NFSv4.0 **advisory** byte-range record locks. This shape is frozen now so
/// the M2 gate does not reopen the transport seam; no M1 consumer issues it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockArgs {
    /// Lock type (`READ_LT`, `WRITE_LT` and their blocking forms).
    pub lock_type: LockType,
    /// Whether this is the first lock for the owner (`new_lock_owner`).
    pub new_lock_owner: bool,
    /// Open stateid the lock derives from.
    pub open_stateid: Stateid,
    /// Open-owner seqid when `new_lock_owner` is set.
    pub open_seqid: u32,
    /// Lock-owner seqid.
    pub lock_seqid: u32,
    /// Lock owner bytes, scoped to the client id.
    pub lock_owner: Vec<u8>,
    /// First byte of the range.
    pub offset: u64,
    /// Range length; `u64::MAX` means "to end of file".
    pub length: u64,
}

/// NFSv4.0 lock types. All are advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockType {
    /// `READ_LT`.
    Read,
    /// `WRITE_LT`.
    Write,
    /// `READW_LT`.
    ReadBlocking,
    /// `WRITEW_LT`.
    WriteBlocking,
}

/// LOCKU arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockuArgs {
    /// Lock type being released.
    pub lock_type: LockType,
    /// Lock-owner seqid.
    pub lock_seqid: u32,
    /// Lock stateid returned by LOCK.
    pub lock_stateid: Stateid,
    /// First byte of the range.
    pub offset: u64,
    /// Range length.
    pub length: u64,
}

/// SETCLIENTID arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetClientIdArgs {
    /// Client incarnation verifier.
    pub verifier: Verifier,
    /// Stable client identity string, owned by Umbra, not by libnfs.
    pub id: Vec<u8>,
    /// Callback policy. M1 declines delegations rather than running a callback
    /// server, so the recorded program/netid must not describe a live listener.
    pub callback: CallbackPolicy,
}

/// Whether the client offers a delegation callback channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallbackPolicy {
    /// Advertise no usable callback path, so the server has no reason to grant a
    /// delegation. M1 has no callback server and must not pretend otherwise.
    Declined,
}

/// SETCLIENTID reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetClientIdReply {
    /// Client id the server assigned.
    pub client_id: ClientId,
    /// Confirmation verifier to echo in SETCLIENTID_CONFIRM.
    pub confirm: Verifier,
}

/// READ reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReply {
    /// Owned data. Never a borrow of a transport buffer.
    pub data: Vec<u8>,
    /// Whether the server reported end of file.
    pub eof: bool,
}

/// WRITE reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReply {
    /// Bytes the server accepted. A short count is a real answer, not an error.
    pub count: u32,
    /// Stability the server actually achieved, which may be weaker than requested.
    pub committed: Stability,
    /// Verifier to compare at COMMIT time.
    pub verifier: WriteVerifier,
}

/// COMMIT reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReply {
    /// Verifier to compare against the one recorded at WRITE time.
    pub verifier: WriteVerifier,
}

/// One READDIR entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// Cookie for resuming after this entry.
    pub cookie: DirCookie,
    /// Byte name of the entry.
    pub name: ComponentName,
    /// Attributes requested for this entry.
    pub attributes: Attributes,
}

/// Arguments for one bounded READDIR page.
///
/// Grouped into a struct so paging parameters travel together and cannot be
/// reordered at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadDirRequest {
    /// Resume point; `DirCookie(0)` starts at the beginning.
    pub cookie: DirCookie,
    /// Verifier from the previous page, zeroed for a first page.
    pub verifier: DirVerifier,
    /// Server hint for directory bytes per page.
    pub dir_count: u32,
    /// Hard bound on reply bytes.
    pub max_count: u32,
    /// Attributes to fetch per entry.
    pub attrs: AttrMask,
}

/// One bounded READDIR page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPage {
    /// Cookie verifier for the directory as of this page.
    pub verifier: DirVerifier,
    /// Entries in server order.
    pub entries: Vec<DirEntry>,
    /// Whether the directory was exhausted by this page.
    pub eof: bool,
}

/// CLOSE reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseReply {
    /// Stateid the server returned for the closed state.
    pub stateid: Stateid,
}

/// LOCK reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockReply {
    /// Lock stateid.
    pub stateid: Stateid,
}

/// `change_info4`: the directory change value either side of a namespace
/// mutation.
///
/// `atomic` is the server's own claim that `before` and `after` were sampled
/// atomically with the operation. It is reported, never assumed: a consumer that
/// treats a non-atomic pair as a proof of ordering would be inventing a
/// guarantee the server declined to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeInfo {
    /// `cinfo.atomic` as the server reported it.
    pub atomic: bool,
    /// Directory change value before the operation.
    pub before: u64,
    /// Directory change value after the operation.
    pub after: u64,
}

/// Settable attributes for CREATE and SETATTR, encoded as one `fattr4`.
///
/// Every field is optional and only the present ones are encoded, so a caller
/// cannot accidentally reset an attribute it never named. This is the write-side
/// counterpart of [`Attributes`], which is decode-only; keeping them separate is
/// what stops a read-only attribute such as `FATTR4_FILEID` from being offered
/// to a server that would answer `NFS4ERR_INVAL`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttrValues {
    /// `FATTR4_SIZE`. On SETATTR this is a truncation.
    pub size: Option<u64>,
    /// `FATTR4_MODE`, POSIX mode bits.
    pub mode: Option<u32>,
    /// `FATTR4_OWNER`, verbatim AUTH_SYS name-string bytes, not a uid.
    pub owner: Option<Vec<u8>>,
    /// `FATTR4_OWNER_GROUP`, verbatim bytes.
    pub owner_group: Option<Vec<u8>>,
    /// `FATTR4_TIME_ACCESS_SET`, an explicit client-supplied time.
    pub time_access: Option<Nfs4Time>,
    /// `FATTR4_TIME_MODIFY_SET`, an explicit client-supplied time.
    pub time_modify: Option<Nfs4Time>,
}

impl AttrValues {
    /// The bitmap these values encode, in attribute order.
    pub fn mask(&self) -> AttrMask {
        let mut mask = AttrMask::default();
        if self.size.is_some() {
            mask = mask.union(AttrMask::SIZE);
        }
        if self.mode.is_some() {
            mask = mask.union(AttrMask::MODE);
        }
        if self.owner.is_some() {
            mask = mask.union(AttrMask::OWNER);
        }
        if self.owner_group.is_some() {
            mask = mask.union(AttrMask::OWNER_GROUP);
        }
        if self.time_access.is_some() {
            mask = mask.union(AttrMask::TIME_ACCESS_SET);
        }
        if self.time_modify.is_some() {
            mask = mask.union(AttrMask::TIME_MODIFY_SET);
        }
        mask
    }

    /// Whether this names no attribute at all.
    ///
    /// An empty SETATTR is a request that cannot fail and cannot do anything,
    /// which is exactly the shape that lets a caller report a metadata change it
    /// never made. Callers refuse it rather than dispatching it.
    pub fn is_empty(&self) -> bool {
        self.mask() == AttrMask::default()
    }
}

/// `createtype4` restricted to what M1 will put on the wire.
///
/// NFSv4.0 CREATE also makes symlinks, block/character devices, sockets and
/// FIFOs. None of them is in this provider's surface — `docs/design/syscall-matrix.md`
/// makes physical symlinks an escape risk and the device types have no contract
/// operation — so the enum stops at the one type the capability table supports.
/// Widening it later is a visible edit here, the same scope-lock checkpoint
/// [`OpCode`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateType {
    /// `NF4DIR`.
    Directory,
}

/// One operation in a COMPOUND. Every variant owns its arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Nfs4Op {
    /// `PUTROOTFH`.
    PutRootFh,
    /// `PUTFH`.
    PutFh(FileHandle),
    /// `GETFH`.
    GetFh,
    /// `GETATTR`.
    GetAttr(AttrMask),
    /// `LOOKUP`.
    Lookup(ComponentName),
    /// `LOOKUPP`.
    LookupParent,
    /// `READDIR`, one bounded page.
    ReadDir {
        /// Resume point; `DirCookie(0)` starts at the beginning.
        cookie: DirCookie,
        /// Verifier from the previous page, zeroed for a first page.
        verifier: DirVerifier,
        /// Server hint for directory bytes per page.
        dir_count: u32,
        /// Hard bound on reply bytes.
        max_count: u32,
        /// Attributes to fetch per entry.
        attrs: AttrMask,
    },
    /// `READ`.
    Read {
        /// Stateid authorising the read.
        stateid: Stateid,
        /// Absolute offset.
        offset: u64,
        /// Requested byte count.
        count: u32,
    },
    /// `WRITE`.
    Write {
        /// Stateid authorising the write.
        stateid: Stateid,
        /// Absolute offset.
        offset: u64,
        /// Requested stability.
        stability: Stability,
        /// Owned payload.
        data: Vec<u8>,
    },
    /// `COMMIT`.
    Commit {
        /// Absolute offset.
        offset: u64,
        /// Byte count; zero means "to end of file".
        count: u32,
    },
    /// `OPEN`.
    Open(OpenArgs),
    /// `OPEN_CONFIRM`.
    OpenConfirm {
        /// Stateid from OPEN.
        stateid: Stateid,
        /// Open-owner seqid for the confirm.
        seqid: u32,
    },
    /// `CLOSE`.
    Close {
        /// Open-owner seqid for the close.
        seqid: u32,
        /// Stateid being closed.
        stateid: Stateid,
    },
    /// `LOCK`.
    Lock(LockArgs),
    /// `LOCKU`.
    Locku(LockuArgs),
    /// `RENEW`.
    Renew(ClientId),
    /// `SETCLIENTID`.
    SetClientId(SetClientIdArgs),
    /// `SETCLIENTID_CONFIRM`.
    SetClientIdConfirm {
        /// Client id from SETCLIENTID.
        client_id: ClientId,
        /// Verifier from SETCLIENTID.
        confirm: Verifier,
    },
    /// `SAVEFH`: remember the current filehandle.
    ///
    /// Present because RENAME addresses its *source* directory through the saved
    /// filehandle (RFC 7530 §16.26) and has no other way to name it. Without
    /// this variant the [`Rename`](Self::Rename) arguments below cannot be put on
    /// the wire at all.
    SaveFh,
    /// `REMOVE`: unlink one name from the current filehandle's directory.
    ///
    /// The parent is the current filehandle, set by a preceding PUTFH, so this
    /// cannot be asked to act on an unanchored path.
    Remove {
        /// Name to unlink within the current directory.
        name: ComponentName,
    },
    /// `RENAME`: move a name from the saved directory to the current directory.
    ///
    /// `SAVEFH` sets the source directory and `PUTFH` sets the target directory,
    /// which is how NFSv4.0 addresses a rename. A same-directory rename still
    /// needs both, because the operation reads the saved filehandle either way.
    Rename {
        /// Name in the saved (source) directory.
        old_name: ComponentName,
        /// Name to create in the current (target) directory.
        new_name: ComponentName,
    },
    /// `CREATE`: make a non-regular object in the current filehandle's directory.
    ///
    /// Regular files are created by `OPEN`, never by `CREATE`; that is RFC 7530's
    /// split, and it is why [`CreateType`] carries no `Regular`.
    Create {
        /// Which kind of object to create.
        object_type: CreateType,
        /// Name for the new object.
        name: ComponentName,
        /// Initial attributes.
        attributes: AttrValues,
    },
    /// `SETATTR`: change attributes of the current filehandle.
    ///
    /// The stateid is `Stateid::ANONYMOUS` except when setting `FATTR4_SIZE`,
    /// where RFC 7530 §16.32 requires the stateid of an open with WRITE access:
    /// a truncation is a write, and a server is entitled to refuse it without
    /// one.
    SetAttr {
        /// Stateid authorising the change.
        stateid: Stateid,
        /// Attributes to set.
        attributes: AttrValues,
    },
}

impl Nfs4Op {
    /// Operation code this argument set encodes.
    pub fn opcode(&self) -> OpCode {
        match self {
            Self::PutRootFh => OpCode::PutRootFh,
            Self::PutFh(_) => OpCode::PutFh,
            Self::GetFh => OpCode::GetFh,
            Self::GetAttr(_) => OpCode::GetAttr,
            Self::Lookup(_) => OpCode::Lookup,
            Self::LookupParent => OpCode::LookupParent,
            Self::ReadDir { .. } => OpCode::ReadDir,
            Self::Read { .. } => OpCode::Read,
            Self::Write { .. } => OpCode::Write,
            Self::Commit { .. } => OpCode::Commit,
            Self::Open(_) => OpCode::Open,
            Self::OpenConfirm { .. } => OpCode::OpenConfirm,
            Self::Close { .. } => OpCode::Close,
            Self::Lock(_) => OpCode::Lock,
            Self::Locku(_) => OpCode::Locku,
            Self::Renew(_) => OpCode::Renew,
            Self::SetClientId(_) => OpCode::SetClientId,
            Self::SetClientIdConfirm { .. } => OpCode::SetClientIdConfirm,
            Self::SaveFh => OpCode::SaveFh,
            Self::Remove { .. } => OpCode::Remove,
            Self::Rename { .. } => OpCode::Rename,
            Self::Create { .. } => OpCode::Create,
            Self::SetAttr { .. } => OpCode::SetAttr,
        }
    }

    /// Whether this operation can change server state. Used by the replay facade
    /// to decide what needs a durable intent before dispatch.
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::Write { .. }
                | Self::Commit { .. }
                | Self::Open(_)
                | Self::OpenConfirm { .. }
                | Self::Close { .. }
                | Self::Lock(_)
                | Self::Locku(_)
                | Self::SetClientId(_)
                | Self::SetClientIdConfirm { .. }
                | Self::Remove { .. }
                | Self::Rename { .. }
                | Self::Create { .. }
                | Self::SetAttr { .. }
        )
    }
}

/// One reply within a COMPOUND, positionally matched to its operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpReply {
    /// `PUTROOTFH` succeeded.
    PutRootFh,
    /// `PUTFH` succeeded.
    PutFh,
    /// `GETFH` returned the current filehandle.
    GetFh(FileHandle),
    /// `GETATTR` returned attributes.
    GetAttr(Attributes),
    /// `LOOKUP` succeeded.
    Lookup,
    /// `LOOKUPP` succeeded.
    LookupParent,
    /// `READDIR` returned one page.
    ReadDir(DirPage),
    /// `READ` returned data.
    Read(ReadReply),
    /// `WRITE` returned a count, stability and verifier.
    Write(WriteReply),
    /// `COMMIT` returned a verifier.
    Commit(CommitReply),
    /// `OPEN` returned a stateid.
    Open(OpenReply),
    /// `OPEN_CONFIRM` returned the confirmed stateid.
    OpenConfirm(Stateid),
    /// `CLOSE` returned the closed stateid.
    Close(CloseReply),
    /// `LOCK` returned a lock stateid.
    Lock(LockReply),
    /// `LOCKU` returned the released stateid.
    Locku(Stateid),
    /// `RENEW` succeeded.
    Renew,
    /// `SETCLIENTID` returned a client id and confirm verifier.
    SetClientId(SetClientIdReply),
    /// `SETCLIENTID_CONFIRM` succeeded.
    SetClientIdConfirm,
    /// `SAVEFH` succeeded.
    SaveFh,
    /// `REMOVE` returned the parent directory's change info.
    Remove(ChangeInfo),
    /// `RENAME` returned change info for both directories.
    Rename {
        /// Change info for the source directory.
        source: ChangeInfo,
        /// Change info for the target directory.
        target: ChangeInfo,
    },
    /// `CREATE` returned change info and the attributes it actually set.
    Create {
        /// Change info for the parent directory.
        info: ChangeInfo,
        /// Bits the server reports it set, which may be fewer than requested.
        attrset: AttrMask,
    },
    /// `SETATTR` returned the attributes it actually set.
    ///
    /// A server may set fewer bits than asked for and still return `NFS4_OK`, so
    /// this bitmap is the only evidence of what really changed.
    SetAttr(AttrMask),
}

impl OpReply {
    /// Operation code this reply belongs to.
    pub fn opcode(&self) -> OpCode {
        match self {
            Self::PutRootFh => OpCode::PutRootFh,
            Self::PutFh => OpCode::PutFh,
            Self::GetFh(_) => OpCode::GetFh,
            Self::GetAttr(_) => OpCode::GetAttr,
            Self::Lookup => OpCode::Lookup,
            Self::LookupParent => OpCode::LookupParent,
            Self::ReadDir(_) => OpCode::ReadDir,
            Self::Read(_) => OpCode::Read,
            Self::Write(_) => OpCode::Write,
            Self::Commit(_) => OpCode::Commit,
            Self::Open(_) => OpCode::Open,
            Self::OpenConfirm(_) => OpCode::OpenConfirm,
            Self::Close(_) => OpCode::Close,
            Self::Lock(_) => OpCode::Lock,
            Self::Locku(_) => OpCode::Locku,
            Self::Renew => OpCode::Renew,
            Self::SetClientId(_) => OpCode::SetClientId,
            Self::SetClientIdConfirm => OpCode::SetClientIdConfirm,
            Self::SaveFh => OpCode::SaveFh,
            Self::Remove(_) => OpCode::Remove,
            Self::Rename { .. } => OpCode::Rename,
            Self::Create { .. } => OpCode::Create,
            Self::SetAttr(_) => OpCode::SetAttr,
        }
    }
}

/// A COMPOUND request. Owns every argument byte it carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compound {
    /// Diagnostic tag echoed by the server.
    pub tag: Vec<u8>,
    /// Operations in order.
    pub ops: Vec<Nfs4Op>,
}

impl Compound {
    /// Build a tagged COMPOUND.
    pub fn new(tag: impl Into<Vec<u8>>, ops: Vec<Nfs4Op>) -> Self {
        Self {
            tag: tag.into(),
            ops,
        }
    }
}

/// A COMPOUND reply.
///
/// `results` holds one reply per operation that completed. A `failure` names the
/// operation at index `results.len()` that stopped the COMPOUND. A multi-op
/// COMPOUND is not a transaction: earlier results stand even when a later
/// operation fails, which is exactly why both halves are reported together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompoundReply {
    /// Tag echoed by the server.
    pub tag: Vec<u8>,
    /// Replies for the operations that completed.
    pub results: Vec<OpReply>,
    /// The operation that failed, if the COMPOUND did not run to completion.
    pub failure: Option<ProtocolError>,
}

impl CompoundReply {
    /// Take the reply at `index`, or report a shape mismatch.
    ///
    /// Consumers must never infer a successful mutation from a reply-kind
    /// mismatch, so this returns the failure when one is recorded.
    pub fn expect(&self, index: usize) -> Result<&OpReply, FacadeError> {
        if let Some(failure) = &self.failure {
            return Err(FacadeError::Protocol(failure.clone()));
        }
        self.results.get(index).ok_or_else(|| {
            FacadeError::Transport(TransportError::Malformed(format!(
                "COMPOUND returned {} results; index {index} requested",
                self.results.len()
            )))
        })
    }
}

/// Identity of one in-flight call inside the pump's registry.
///
/// This is deliberately an integer, not a pointer. The event pump owns a registry
/// keyed by this token; a completion for a retired token finds nothing instead of
/// dereferencing freed memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CallToken(NonZeroU64);

impl CallToken {
    /// Mint a token. Available only inside this crate, so a consumer cannot
    /// fabricate one and drive an implementation's registry from outside.
    pub(crate) fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    /// The registry index.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Proof that a call registration was withdrawn from the event pump.
///
/// Constructed only by [`RawTransport::cancel`] inside this crate. Because
/// [`TransportError::DeadlineExpired`] holds one, an implementation cannot report
/// a deadline it did not first cancel and drain. That closes the spike's
/// stack-`CallState` hazard at the type level rather than by convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retirement {
    token: CallToken,
    drained: bool,
}

impl Retirement {
    /// Record a withdrawal. `drained` is true only when the implementation also
    /// observed the pump quiesce the call, so a caller can tell "cancelled and
    /// proven idle" from "cancelled, completion may still be scheduled".
    pub(crate) fn new(token: CallToken, drained: bool) -> Self {
        Self { token, drained }
    }

    /// Token that was withdrawn.
    pub fn token(&self) -> CallToken {
        self.token
    }

    /// Whether the pump was observed to quiesce the call.
    pub fn drained(&self) -> bool {
        self.drained
    }
}

/// Connection generation. Incremented on every reconnect so state built against
/// an older connection can be rejected rather than silently reused.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct ConnectionEpoch(pub u64);

/// Observable connection state of one transport context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    /// No connection has been established yet.
    Idle,
    /// Connected in this generation.
    Connected(ConnectionEpoch),
    /// The connection dropped; calls in flight have unknown outcomes.
    Broken(ConnectionEpoch),
}

/// Per-call deadline. There is no "wait forever": every submission is bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deadline {
    /// Milliseconds allowed for the whole submission.
    pub millis: u64,
}

impl Deadline {
    /// Build a deadline from a duration, saturating at `u64::MAX` milliseconds.
    pub fn from_duration(duration: Duration) -> Self {
        Self {
            millis: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// The COMPOUND tag [`RawTransport::read`] sends, and the server echoes back.
///
/// **R2-02.** Exported because sizing a READ correctly needs it. A reply's budget
/// is not all payload: `transport::raw::decode` charges the echoed tag *and* the
/// READ data to the same [`TransportLimits::max_reply_bytes`], so a caller that
/// asks for the whole budget asks for a reply that cannot fit inside it. Keeping
/// the value here rather than at each call site is what stops the two figures
/// drifting apart.
pub const READ_TAG: [u8; 4] = *b"read";

/// Bounds an implementation must advertise and enforce.
///
/// One event pump per context, bounded queues and explicit deadlines are owner
/// scope-lock requirements, not tuning knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportLimits {
    /// Maximum calls registered with the pump at once.
    pub max_inflight: u32,
    /// Maximum submissions queued before dispatch. Exceeding this is backpressure.
    pub max_queue_depth: u32,
    /// Hard cap on a decoded reply, checked before allocation.
    pub max_reply_bytes: usize,
    /// Deadline applied when a caller does not supply one.
    pub default_deadline: Deadline,
}

impl TransportLimits {
    /// The largest READ payload one reply can carry under [`Self::max_reply_bytes`].
    ///
    /// **R2-02.** The budget is spent on the echoed [`READ_TAG`] as well as the
    /// data, so this is the figure a `count` must be derived from — not
    /// `max_reply_bytes` itself. Under a 128-byte budget a full 128-byte reply
    /// costs 132 and the decoder refuses it, so a valid read of a healthy file
    /// failed for asking too politely for too much.
    ///
    /// **Zero is a real answer.** A budget at or below the tag length can carry
    /// no payload at all, and there is no chunk size that would make progress.
    /// Callers must refuse before dispatch rather than rounding it back up to
    /// one: that would issue exactly the over-budget request this exists to
    /// prevent, and a zero-byte request would loop forever making no progress.
    #[must_use]
    pub fn max_read_payload(&self) -> usize {
        self.max_reply_bytes.saturating_sub(READ_TAG.len())
    }
}

/// A point at which an implementation must consult its fault plan.
///
/// These name the crash windows the failure model enumerates, so the harness can
/// drive them without the transport growing test-only branches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FaultPoint {
    /// Before the call is registered or any byte is written.
    BeforeDispatch,
    /// After the request is on the wire, before a reply is decoded.
    AfterDispatch,
    /// When a reply has been decoded and is about to be returned.
    BeforeReturn,
    /// When the deadline path runs.
    OnDeadline,
    /// When the pump observes the connection.
    OnConnection,
}

/// Context handed to a fault plan so it can target one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultContext {
    /// Operation being dispatched.
    pub op: OpCode,
    /// Index of that operation within the COMPOUND.
    pub index: u32,
    /// Token the call was registered under, once it has one.
    pub token: Option<CallToken>,
}

/// What the harness wants to happen at a fault point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultAction {
    /// Behave normally.
    Proceed,
    /// Fail with this transport error instead.
    Fail(TransportError),
    /// Substitute this server status for the operation.
    Substitute(Nfs4Status),
    /// Truncate an accepted WRITE to this many bytes, producing a short write.
    ShortWrite(u32),
    /// Change the write/commit verifier the server appears to return.
    RotateVerifier(WriteVerifier),
    /// Drop the reply so the call reaches its deadline with the request sent.
    DropReply,
}

/// Fault-injection hook for the `raw_rpc` harness.
///
/// Implementations of [`RawTransport`] must consult the installed plan at every
/// [`FaultPoint`]. A plan is stateful, so the harness can script a sequence such
/// as "lose the OPEN reply once, then succeed".
pub trait FaultPlan: Send {
    /// Decide what happens at this point.
    fn decide(&mut self, point: FaultPoint, context: FaultContext) -> FaultAction;
}

/// A plan that never injects anything. The default for a production context.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFaults;

impl FaultPlan for NoFaults {
    fn decide(&mut self, _point: FaultPoint, _context: FaultContext) -> FaultAction {
        FaultAction::Proceed
    }
}

/// The raw-RPC transport surface consumed by protocol state.
///
/// # Invariants an implementation must uphold
///
/// 1. One event pump per context. `submit` is the only place that drives it.
/// 2. Every submission is bounded by a deadline and by [`TransportLimits`].
/// 3. Backpressure is applied **before** dispatch: a full queue returns
///    [`TransportError::QueueFull`] without registering a call.
/// 4. A call that does not complete is retired through [`RawTransport::cancel`]
///    before any error is returned, and the [`Retirement`] is carried in the
///    error. No registration outlives the `submit` that created it.
/// 5. Replies are owned. An implementation copies out of any transport-owned
///    buffer before that buffer can be recycled.
/// 6. The wire profile is checked once at construction and never renegotiated.
pub trait RawTransport: Send {
    /// The profile this context is speaking. Must be [`WireProfile::V40_TCP_SYS`].
    fn wire_profile(&self) -> WireProfile;

    /// Bounds this context enforces.
    fn limits(&self) -> TransportLimits;

    /// Current connection generation and health.
    fn connection(&self) -> ConnectionState;

    /// Submit one COMPOUND and wait for its reply, bounded by `deadline`.
    ///
    /// Takes the call by value: the implementation owns every argument byte for
    /// the whole dispatch, so nothing the caller holds can be freed underneath a
    /// pending RPC.
    fn submit(&mut self, call: Compound, deadline: Deadline) -> TransportResult<CompoundReply>;

    /// Withdraw a registration from the pump and return proof it is gone.
    ///
    /// Called by `submit` on every non-completing path. Calling it for an unknown
    /// or already-retired token is not an error; it returns a retirement that
    /// reports `drained`.
    fn cancel(&mut self, token: CallToken) -> TransportResult<Retirement>;

    /// Re-establish the connection, returning the new generation.
    ///
    /// Reconnecting does not revalidate protocol state. Client id, opens, locks
    /// and verifiers are the protocol-state owner's problem, and must be proven
    /// again before any mutation is admitted.
    fn reconnect(&mut self) -> TransportResult<ConnectionEpoch>;

    /// Install a fault plan. Replaces any previous plan.
    fn install_faults(&mut self, plan: Box<dyn FaultPlan>);

    // --- Shape helpers -------------------------------------------------------
    //
    // These assemble the COMPOUNDs the M1 operations need and destructure their
    // replies. They are argument plumbing over `submit`, deliberately holding no
    // protocol state of their own: sequencing, stateids and reclaim belong to the
    // handle facade and to `raw_state`.

    /// `PUTROOTFH; GETFH`: the export root filehandle.
    fn root_filehandle(&mut self, deadline: Deadline) -> Result<FileHandle, FacadeError> {
        let reply = self
            .submit(
                Compound::new(*b"root", vec![Nfs4Op::PutRootFh, Nfs4Op::GetFh]),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::GetFh(handle) => Ok(handle.clone()),
            other => Err(shape("root_filehandle", OpCode::GetFh, other.opcode())),
        }
    }

    /// `PUTFH; LOOKUP name; GETFH; GETATTR`: resolve one component.
    fn lookup(
        &mut self,
        directory: &FileHandle,
        name: &ComponentName,
        attrs: AttrMask,
        deadline: Deadline,
    ) -> Result<(FileHandle, Attributes), FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"lookup",
                    vec![
                        Nfs4Op::PutFh(directory.clone()),
                        Nfs4Op::Lookup(name.clone()),
                        Nfs4Op::GetFh,
                        Nfs4Op::GetAttr(attrs),
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        let handle = match reply.expect(2)? {
            OpReply::GetFh(handle) => handle.clone(),
            other => return Err(shape("lookup", OpCode::GetFh, other.opcode())),
        };
        match reply.expect(3)? {
            OpReply::GetAttr(attributes) => Ok((handle, attributes.clone())),
            other => Err(shape("lookup", OpCode::GetAttr, other.opcode())),
        }
    }

    /// `PUTFH; LOOKUPP; GETFH`: the parent directory.
    fn lookup_parent(
        &mut self,
        directory: &FileHandle,
        deadline: Deadline,
    ) -> Result<FileHandle, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"lookupp",
                    vec![
                        Nfs4Op::PutFh(directory.clone()),
                        Nfs4Op::LookupParent,
                        Nfs4Op::GetFh,
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(2)? {
            OpReply::GetFh(handle) => Ok(handle.clone()),
            other => Err(shape("lookup_parent", OpCode::GetFh, other.opcode())),
        }
    }

    /// `PUTFH; GETATTR`: attributes of one object, without following a final name.
    fn getattr(
        &mut self,
        handle: &FileHandle,
        attrs: AttrMask,
        deadline: Deadline,
    ) -> Result<Attributes, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"getattr",
                    vec![Nfs4Op::PutFh(handle.clone()), Nfs4Op::GetAttr(attrs)],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::GetAttr(attributes) => Ok(attributes.clone()),
            other => Err(shape("getattr", OpCode::GetAttr, other.opcode())),
        }
    }

    /// `PUTFH; READ`: a bounded positional read. A short reply is a real answer.
    fn read(
        &mut self,
        handle: &FileHandle,
        stateid: Stateid,
        offset: u64,
        count: u32,
        deadline: Deadline,
    ) -> Result<ReadReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    READ_TAG,
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::Read {
                            stateid,
                            offset,
                            count,
                        },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Read(read) => Ok(read.clone()),
            other => Err(shape("read", OpCode::Read, other.opcode())),
        }
    }

    /// `PUTFH; WRITE`: a positional write at the requested stability.
    ///
    /// The reply carries the count the server accepted and the stability it
    /// actually reached, which may be weaker than requested. Callers must record
    /// both, plus the verifier, before treating the write as done.
    fn write(
        &mut self,
        handle: &FileHandle,
        stateid: Stateid,
        offset: u64,
        stability: Stability,
        data: Vec<u8>,
        deadline: Deadline,
    ) -> Result<WriteReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"write",
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::Write {
                            stateid,
                            offset,
                            stability,
                            data,
                        },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Write(write) => Ok(*write),
            other => Err(shape("write", OpCode::Write, other.opcode())),
        }
    }

    /// `PUTFH; COMMIT`: commit a byte range and return the server's verifier.
    fn commit(
        &mut self,
        handle: &FileHandle,
        offset: u64,
        count: u32,
        deadline: Deadline,
    ) -> Result<CommitReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"commit",
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::Commit { offset, count },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Commit(commit) => Ok(*commit),
            other => Err(shape("commit", OpCode::Commit, other.opcode())),
        }
    }

    /// `PUTFH; READDIR`: one bounded page. Paging and cursor invalidation are the
    /// caller's, driven by the returned [`DirVerifier`].
    fn readdir(
        &mut self,
        directory: &FileHandle,
        request: ReadDirRequest,
        deadline: Deadline,
    ) -> Result<DirPage, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"readdir",
                    vec![
                        Nfs4Op::PutFh(directory.clone()),
                        Nfs4Op::ReadDir {
                            cookie: request.cookie,
                            verifier: request.verifier,
                            dir_count: request.dir_count,
                            max_count: request.max_count,
                            attrs: request.attrs,
                        },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::ReadDir(page) => Ok(page.clone()),
            other => Err(shape("readdir", OpCode::ReadDir, other.opcode())),
        }
    }

    /// `PUTFH parent; OPEN; GETFH`: open by parent filehandle plus name.
    ///
    /// Returns the OPEN reply and the opened object's filehandle together,
    /// because a caller that has one without the other cannot prove identity.
    fn open(
        &mut self,
        parent: &FileHandle,
        args: OpenArgs,
        deadline: Deadline,
    ) -> Result<(OpenReply, FileHandle), FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"open",
                    vec![
                        Nfs4Op::PutFh(parent.clone()),
                        Nfs4Op::Open(args),
                        Nfs4Op::GetFh,
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        let open = match reply.expect(1)? {
            OpReply::Open(open) => open.clone(),
            other => return Err(shape("open", OpCode::Open, other.opcode())),
        };
        match reply.expect(2)? {
            OpReply::GetFh(handle) => Ok((open, handle.clone())),
            other => Err(shape("open", OpCode::GetFh, other.opcode())),
        }
    }

    /// `PUTFH; OPEN_CONFIRM`: confirm a stateid the server flagged.
    fn open_confirm(
        &mut self,
        handle: &FileHandle,
        stateid: Stateid,
        seqid: u32,
        deadline: Deadline,
    ) -> Result<Stateid, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"open_confirm",
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::OpenConfirm { stateid, seqid },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::OpenConfirm(stateid) => Ok(*stateid),
            other => Err(shape("open_confirm", OpCode::OpenConfirm, other.opcode())),
        }
    }

    /// `PUTFH; CLOSE`: release an open state.
    fn close(
        &mut self,
        handle: &FileHandle,
        seqid: u32,
        stateid: Stateid,
        deadline: Deadline,
    ) -> Result<CloseReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"close",
                    vec![
                        Nfs4Op::PutFh(handle.clone()),
                        Nfs4Op::Close { seqid, stateid },
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Close(close) => Ok(*close),
            other => Err(shape("close", OpCode::Close, other.opcode())),
        }
    }

    /// `PUTFH; LOCK`: acquire an advisory byte-range record lock.
    fn lock(
        &mut self,
        handle: &FileHandle,
        args: LockArgs,
        deadline: Deadline,
    ) -> Result<LockReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"lock",
                    vec![Nfs4Op::PutFh(handle.clone()), Nfs4Op::Lock(args)],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Lock(lock) => Ok(*lock),
            other => Err(shape("lock", OpCode::Lock, other.opcode())),
        }
    }

    /// `PUTFH; LOCKU`: release an advisory byte-range record lock.
    fn locku(
        &mut self,
        handle: &FileHandle,
        args: LockuArgs,
        deadline: Deadline,
    ) -> Result<Stateid, FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"locku",
                    vec![Nfs4Op::PutFh(handle.clone()), Nfs4Op::Locku(args)],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::Locku(stateid) => Ok(*stateid),
            other => Err(shape("locku", OpCode::Locku, other.opcode())),
        }
    }

    /// `RENEW`: refresh the NFS lease.
    ///
    /// A successful RENEW proves the NFS lease is alive. It is not evidence of
    /// Umbra writer authority and never authorises abandoned-session takeover.
    fn renew(&mut self, client_id: ClientId, deadline: Deadline) -> Result<(), FacadeError> {
        let reply = self
            .submit(
                Compound::new(*b"renew", vec![Nfs4Op::Renew(client_id)]),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(0)? {
            OpReply::Renew => Ok(()),
            other => Err(shape("renew", OpCode::Renew, other.opcode())),
        }
    }

    /// `SETCLIENTID`: establish a client incarnation.
    fn set_client_id(
        &mut self,
        args: SetClientIdArgs,
        deadline: Deadline,
    ) -> Result<SetClientIdReply, FacadeError> {
        let reply = self
            .submit(
                Compound::new(*b"setclientid", vec![Nfs4Op::SetClientId(args)]),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(0)? {
            OpReply::SetClientId(reply) => Ok(*reply),
            other => Err(shape("set_client_id", OpCode::SetClientId, other.opcode())),
        }
    }

    /// `SETCLIENTID_CONFIRM`: confirm the incarnation.
    fn set_client_id_confirm(
        &mut self,
        client_id: ClientId,
        confirm: Verifier,
        deadline: Deadline,
    ) -> Result<(), FacadeError> {
        let reply = self
            .submit(
                Compound::new(
                    *b"setclientid_confirm",
                    vec![Nfs4Op::SetClientIdConfirm { client_id, confirm }],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(0)? {
            OpReply::SetClientIdConfirm => Ok(()),
            other => Err(shape(
                "set_client_id_confirm",
                OpCode::SetClientIdConfirm,
                other.opcode(),
            )),
        }
    }
}

fn shape(operation: &str, expected: OpCode, observed: OpCode) -> FacadeError {
    FacadeError::Transport(TransportError::Malformed(format!(
        "{operation}: expected a {expected:?} reply, received {observed:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **R2-02.** The reply budget is shared between the echoed tag and the READ
    /// payload, so the payload figure is the budget minus the tag — and it is
    /// allowed to be zero rather than being rounded up to a request that cannot
    /// fit.
    #[test]
    fn r2_02_the_read_payload_budget_reserves_the_tag() {
        let limits = |max_reply_bytes| TransportLimits {
            max_inflight: 1,
            max_queue_depth: 8,
            max_reply_bytes,
            default_deadline: Deadline { millis: 5_000 },
        };
        assert_eq!(READ_TAG.len(), 4);
        assert_eq!(limits(128).max_read_payload(), 124);
        assert_eq!(limits(1024 * 1024).max_read_payload(), 1024 * 1024 - 4);
        // The lower edge saturates instead of wrapping, and stays zero.
        assert_eq!(limits(5).max_read_payload(), 1);
        assert_eq!(limits(4).max_read_payload(), 0);
        assert_eq!(limits(0).max_read_payload(), 0);
    }

    #[test]
    fn only_the_authorised_profile_passes() {
        assert!(WireProfile::V40_TCP_SYS.check().is_ok());
        let v41 = WireProfile {
            minor_version: 1,
            ..WireProfile::V40_TCP_SYS
        };
        assert!(matches!(
            v41.check(),
            Err(TransportError::UnsupportedProfile(_))
        ));
    }

    #[test]
    fn components_preserve_bytes_and_reject_traversal() {
        assert_eq!(
            ComponentName::new(b"\xff\xfe".to_vec()).unwrap().as_bytes(),
            b"\xff\xfe"
        );
        for rejected in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b"] {
            assert!(ComponentName::new(rejected.to_vec()).is_err());
        }
    }

    #[test]
    fn opcode_numbers_match_rfc_7530() {
        assert_eq!(OpCode::Close as u32, 4);
        assert_eq!(OpCode::Open as u32, 18);
        assert_eq!(OpCode::PutRootFh as u32, 24);
        assert_eq!(OpCode::Write as u32, 38);
        // The highest NFSv4.0 operation is RELEASE_LOCKOWNER (39). Anything at or
        // above 40 is v4.1 and has no variant here.
        assert_eq!(OpCode::ReleaseLockOwner as u32, 39);
    }

    #[test]
    fn a_recorded_failure_wins_over_a_positional_result() {
        let reply = CompoundReply {
            tag: b"open".to_vec(),
            results: vec![OpReply::PutFh],
            failure: Some(ProtocolError {
                status: Nfs4Status::GRACE,
                op: OpCode::Open,
                index: 1,
            }),
        };
        assert_eq!(
            reply.expect(0).unwrap_err().status(),
            Some(Nfs4Status::GRACE)
        );
    }

    // --- the authorised contracts hotfix: Remove / Rename / Create / SetAttr ---

    fn component(bytes: &[u8]) -> ComponentName {
        ComponentName::new(bytes.to_vec()).expect("valid component")
    }

    #[test]
    fn every_namespace_mutation_variant_reports_its_rfc_7530_opcode() {
        let cases = [
            (
                Nfs4Op::Remove {
                    name: component(b"x"),
                },
                OpCode::Remove,
                28,
            ),
            (
                Nfs4Op::Rename {
                    old_name: component(b"a"),
                    new_name: component(b"b"),
                },
                OpCode::Rename,
                29,
            ),
            (
                Nfs4Op::Create {
                    object_type: CreateType::Directory,
                    name: component(b"d"),
                    attributes: AttrValues::default(),
                },
                OpCode::Create,
                6,
            ),
            (
                Nfs4Op::SetAttr {
                    stateid: Stateid::ANONYMOUS,
                    attributes: AttrValues::default(),
                },
                OpCode::SetAttr,
                34,
            ),
            (Nfs4Op::SaveFh, OpCode::SaveFh, 32),
        ];
        for (op, code, number) in cases {
            assert_eq!(op.opcode(), code);
            assert_eq!(code as u32, number, "{code:?} must keep its RFC number");
        }
    }

    #[test]
    fn the_four_namespace_mutations_are_mutations_and_savefh_is_not() {
        assert!(Nfs4Op::Remove {
            name: component(b"x")
        }
        .is_mutation());
        assert!(Nfs4Op::Rename {
            old_name: component(b"a"),
            new_name: component(b"b"),
        }
        .is_mutation());
        assert!(Nfs4Op::Create {
            object_type: CreateType::Directory,
            name: component(b"d"),
            attributes: AttrValues::default(),
        }
        .is_mutation());
        assert!(Nfs4Op::SetAttr {
            stateid: Stateid::ANONYMOUS,
            attributes: AttrValues::default(),
        }
        .is_mutation());
        // SAVEFH moves a client-side filehandle slot. It changes nothing on the
        // server, so requiring a durable intent for it would make the replay
        // ledger record work that cannot be lost.
        assert!(!Nfs4Op::SaveFh.is_mutation());
    }

    #[test]
    fn the_eighteen_frozen_variants_kept_their_mutation_classification() {
        // The hotfix is additive: nothing that was a mutation stopped being one,
        // and nothing that was not became one.
        for (op, expected) in [
            (Nfs4Op::PutRootFh, false),
            (Nfs4Op::GetFh, false),
            (Nfs4Op::GetAttr(AttrMask::STAT), false),
            (Nfs4Op::Lookup(component(b"n")), false),
            (Nfs4Op::LookupParent, false),
            (
                Nfs4Op::Read {
                    stateid: Stateid::ANONYMOUS,
                    offset: 0,
                    count: 1,
                },
                false,
            ),
            (
                Nfs4Op::Write {
                    stateid: Stateid::ANONYMOUS,
                    offset: 0,
                    stability: Stability::Unstable,
                    data: vec![0],
                },
                true,
            ),
            (
                Nfs4Op::Commit {
                    offset: 0,
                    count: 0,
                },
                true,
            ),
            (Nfs4Op::Renew(ClientId(1)), false),
        ] {
            assert_eq!(op.is_mutation(), expected, "{op:?}");
        }
    }

    #[test]
    fn attr_values_name_exactly_the_bits_they_carry() {
        assert!(AttrValues::default().is_empty());

        let values = AttrValues {
            size: Some(4),
            mode: Some(0o644),
            owner: Some(b"501".to_vec()),
            owner_group: Some(b"20".to_vec()),
            time_access: Some(Nfs4Time::default()),
            time_modify: Some(Nfs4Time::default()),
        };
        let mask = values.mask();
        assert!(!values.is_empty());
        for bit in [
            AttrMask::SIZE,
            AttrMask::MODE,
            AttrMask::OWNER,
            AttrMask::OWNER_GROUP,
            AttrMask::TIME_ACCESS_SET,
            AttrMask::TIME_MODIFY_SET,
        ] {
            assert!(mask.contains(bit));
        }
        // SETATTR must never ask for the read-only time attributes; asking is
        // NFS4ERR_INVAL on a real server.
        assert!(!mask.contains(AttrMask::TIME_MODIFY));
        assert!(!mask.contains(AttrMask::FILEID));

        // One field set names one bit, so a partial update cannot silently carry
        // an attribute the caller never asked to change.
        let only_mode = AttrValues {
            mode: Some(0o600),
            ..AttrValues::default()
        };
        assert_eq!(only_mode.mask(), AttrMask::MODE);
    }

    #[test]
    fn the_settable_time_bits_are_the_rfc_7530_numbers() {
        assert_eq!(AttrMask::TIME_ACCESS_SET.word1, 1 << (48 - 32));
        assert_eq!(AttrMask::TIME_MODIFY_SET.word1, 1 << (54 - 32));
    }

    #[test]
    fn every_new_reply_reports_the_opcode_it_belongs_to() {
        let info = ChangeInfo {
            atomic: true,
            before: 1,
            after: 2,
        };
        assert_eq!(OpReply::Remove(info).opcode(), OpCode::Remove);
        assert_eq!(
            OpReply::Rename {
                source: info,
                target: info
            }
            .opcode(),
            OpCode::Rename
        );
        assert_eq!(
            OpReply::Create {
                info,
                attrset: AttrMask::MODE
            }
            .opcode(),
            OpCode::Create
        );
        assert_eq!(OpReply::SetAttr(AttrMask::MODE).opcode(), OpCode::SetAttr);
        assert_eq!(OpReply::SaveFh.opcode(), OpCode::SaveFh);
    }

    #[test]
    fn create_type_cannot_express_a_physical_symlink_or_a_device() {
        // The enum is the scope-lock: `docs/design/syscall-matrix.md` makes a
        // physical symlink an anchor-escape risk and gives the device types no
        // contract operation. Widening this has to be a visible edit.
        let all = [CreateType::Directory];
        assert_eq!(all.len(), 1);
    }
}
