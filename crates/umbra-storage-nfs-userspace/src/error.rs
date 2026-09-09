//! Error facade: four disjoint failure domains, preserved without lossy mapping.
//!
//! Consumers must be able to tell a server-returned `NFS4ERR_*` from a lost TCP
//! connection, from a missing writer epoch, from a replay-ledger refusal, because
//! each demands a different recovery. A single flattened error type loses that
//! distinction, so this facade keeps the four domains as separate enums and keeps
//! the server's raw status word verbatim in [`Nfs4Status`].
//!
//! Conversion into [`umbra_core::UmbraError`] is **additive**: the classified
//! [`ErrorKind`] is chosen conservatively and the full facade rendering is carried
//! in the error context. The facade value stays authoritative; nothing in this
//! crate reconstructs a `FacadeError` from an `UmbraError`.

use serde::{Deserialize, Serialize};
use umbra_core::{ErrorKind, IdempotencyKey, LeaseEpoch, OperationId, UmbraError};

use crate::transport::{CallToken, ConnectionEpoch, OpCode, Retirement};

/// Raw NFSv4.0 status word, preserved exactly as the server sent it.
///
/// An unrecognised code is retained as its number rather than folded into a
/// neighbouring variant. No unknown status is ever treated as success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Nfs4Status(pub u32);

impl Nfs4Status {
    /// `NFS4_OK`.
    pub const OK: Self = Self(0);
    /// `NFS4ERR_PERM`.
    pub const PERM: Self = Self(1);
    /// `NFS4ERR_NOENT`.
    pub const NOENT: Self = Self(2);
    /// `NFS4ERR_IO`.
    pub const IO: Self = Self(5);
    /// `NFS4ERR_ACCESS`.
    pub const ACCESS: Self = Self(13);
    /// `NFS4ERR_EXIST`.
    pub const EXIST: Self = Self(17);
    /// `NFS4ERR_NOTDIR`.
    pub const NOTDIR: Self = Self(20);
    /// `NFS4ERR_ISDIR`.
    pub const ISDIR: Self = Self(21);
    /// `NFS4ERR_INVAL`.
    pub const INVAL: Self = Self(22);
    /// `NFS4ERR_FBIG`.
    pub const FBIG: Self = Self(27);
    /// `NFS4ERR_NOSPC`.
    pub const NOSPC: Self = Self(28);
    /// `NFS4ERR_ROFS`.
    pub const ROFS: Self = Self(30);
    /// `NFS4ERR_NAMETOOLONG`.
    pub const NAMETOOLONG: Self = Self(63);
    /// `NFS4ERR_NOTEMPTY`.
    pub const NOTEMPTY: Self = Self(66);
    /// `NFS4ERR_DQUOT`.
    pub const DQUOT: Self = Self(69);
    /// `NFS4ERR_STALE`.
    pub const STALE: Self = Self(70);
    /// `NFS4ERR_BADHANDLE`.
    pub const BADHANDLE: Self = Self(10001);
    /// `NFS4ERR_BAD_COOKIE`.
    pub const BAD_COOKIE: Self = Self(10003);
    /// `NFS4ERR_NOTSUPP`.
    pub const NOTSUPP: Self = Self(10004);
    /// `NFS4ERR_TOOSMALL`.
    pub const TOOSMALL: Self = Self(10005);
    /// `NFS4ERR_SERVERFAULT`.
    pub const SERVERFAULT: Self = Self(10006);
    /// `NFS4ERR_DELAY`.
    pub const DELAY: Self = Self(10008);
    /// `NFS4ERR_DENIED`.
    pub const DENIED: Self = Self(10010);
    /// `NFS4ERR_EXPIRED`.
    pub const EXPIRED: Self = Self(10011);
    /// `NFS4ERR_LOCKED`.
    pub const LOCKED: Self = Self(10012);
    /// `NFS4ERR_GRACE`.
    pub const GRACE: Self = Self(10013);
    /// `NFS4ERR_FHEXPIRED`.
    pub const FHEXPIRED: Self = Self(10014);
    /// `NFS4ERR_SHARE_DENIED`.
    pub const SHARE_DENIED: Self = Self(10015);
    /// `NFS4ERR_CLID_INUSE`.
    pub const CLID_INUSE: Self = Self(10017);
    /// `NFS4ERR_RESOURCE`.
    pub const RESOURCE: Self = Self(10018);
    /// `NFS4ERR_MOVED`.
    pub const MOVED: Self = Self(10019);
    /// `NFS4ERR_NOFILEHANDLE`.
    pub const NOFILEHANDLE: Self = Self(10020);
    /// `NFS4ERR_MINOR_VERS_MISMATCH`.
    pub const MINOR_VERS_MISMATCH: Self = Self(10021);
    /// `NFS4ERR_STALE_CLIENTID`.
    pub const STALE_CLIENTID: Self = Self(10022);
    /// `NFS4ERR_STALE_STATEID`.
    pub const STALE_STATEID: Self = Self(10023);
    /// `NFS4ERR_OLD_STATEID`.
    pub const OLD_STATEID: Self = Self(10024);
    /// `NFS4ERR_BAD_STATEID`.
    pub const BAD_STATEID: Self = Self(10025);
    /// `NFS4ERR_BAD_SEQID`.
    pub const BAD_SEQID: Self = Self(10026);
    /// `NFS4ERR_NOT_SAME`.
    pub const NOT_SAME: Self = Self(10027);
    /// `NFS4ERR_LOCK_RANGE`.
    pub const LOCK_RANGE: Self = Self(10028);
    /// `NFS4ERR_SYMLINK`.
    pub const SYMLINK: Self = Self(10029);
    /// `NFS4ERR_LEASE_MOVED`.
    pub const LEASE_MOVED: Self = Self(10031);
    /// `NFS4ERR_NO_GRACE`.
    pub const NO_GRACE: Self = Self(10033);
    /// `NFS4ERR_RECLAIM_BAD`.
    pub const RECLAIM_BAD: Self = Self(10034);
    /// `NFS4ERR_RECLAIM_CONFLICT`.
    pub const RECLAIM_CONFLICT: Self = Self(10035);
    /// `NFS4ERR_BADXDR`.
    pub const BADXDR: Self = Self(10036);
    /// `NFS4ERR_LOCKS_HELD`.
    pub const LOCKS_HELD: Self = Self(10037);
    /// `NFS4ERR_OPENMODE`.
    pub const OPENMODE: Self = Self(10038);
    /// `NFS4ERR_BADOWNER`.
    pub const BADOWNER: Self = Self(10039);
    /// `NFS4ERR_FILE_OPEN`.
    pub const FILE_OPEN: Self = Self(10046);
    /// `NFS4ERR_ADMIN_REVOKED`.
    pub const ADMIN_REVOKED: Self = Self(10047);

    /// True only for `NFS4_OK`. Any other word, known or not, is a failure.
    pub fn is_ok(self) -> bool {
        self == Self::OK
    }

    /// Whether RFC 7530 section 9.1.7 forbids advancing an open/lock-owner seqid
    /// after this status. Every other status advances the seqid even on failure.
    ///
    /// Getting this wrong desynchronises the owner sequence permanently, so the
    /// rule lives beside the status word rather than in each call site.
    pub fn holds_seqid(self) -> bool {
        matches!(
            self,
            Self::STALE_CLIENTID
                | Self::STALE_STATEID
                | Self::BAD_STATEID
                | Self::BAD_SEQID
                | Self::BADXDR
                | Self::RESOURCE
                | Self::NOFILEHANDLE
                | Self::MOVED
        )
    }
}

/// What a caller may do next. Classification never discards the underlying value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    /// Retry the same identity after a bounded delay; no state was lost.
    Retriable,
    /// State or authority must be re-established (reconnect, reclaim, re-open)
    /// before any further mutation is admitted.
    NeedsRecovery,
    /// Object identity or writer authority cannot be proven. Stop; do not guess.
    SafeStop,
    /// A settled answer. Retrying the same request cannot change it.
    Permanent,
}

/// A server-returned NFSv4.0 failure, located within its COMPOUND.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// Status word exactly as received.
    pub status: Nfs4Status,
    /// Operation that produced it.
    pub op: OpCode,
    /// Zero-based index of that operation within the submitted COMPOUND.
    pub index: u32,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NFS4ERR {} at {:?} (compound index {})",
            self.status.0, self.op, self.index
        )
    }
}

/// RPC, connection, framing and scheduling failures. Never an `NFS4ERR_*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum TransportError {
    /// The connection could not be established.
    #[error("connect failed: {0}")]
    Connect(String),
    /// The connection was lost; calls in flight have unknown outcomes.
    #[error("connection lost in epoch {epoch:?}: {detail}")]
    Disconnected {
        /// Connection generation that was lost.
        epoch: ConnectionEpoch,
        /// Verbatim cause.
        detail: String,
    },
    /// The deadline elapsed. The registration is proven withdrawn from the pump.
    ///
    /// [`Retirement`] has no public constructor, so a deadline failure cannot be
    /// reported by any implementation that did not actually cancel and drain the
    /// call. That is the type-level half of the callback-lifetime invariant.
    #[error("deadline elapsed for call {:?}", .retirement.token())]
    DeadlineExpired {
        /// Proof the call was withdrawn before this error was constructed.
        retirement: Retirement,
    },
    /// Submission refused before dispatch because the bounded queue is full.
    #[error("transport queue full ({depth} of {capacity})")]
    QueueFull {
        /// Current depth.
        depth: u32,
        /// Configured bound.
        capacity: u32,
    },
    /// A reply violated the wire profile, framing bounds or reply-shape contract.
    #[error("malformed reply: {0}")]
    Malformed(String),
    /// The call was cancelled by the caller rather than by a deadline.
    #[error("call {0:?} cancelled")]
    Cancelled(CallToken),
    /// A wire profile outside NFSv4.0 / TCP / AUTH_SYS was requested or observed.
    #[error("unsupported wire profile: {0}")]
    UnsupportedProfile(String),
}

/// Writer-authority and admission failures. Distinct from NFS lease state:
/// an NFS lease renewal is not evidence of Umbra writer authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum AuthorityError {
    /// A mutation arrived without a writer epoch.
    #[error("mutation requires writer authority")]
    NoWriterEpoch,
    /// The supplied epoch is not the current one.
    #[error("stale writer epoch: held {held:?}, current {current:?}")]
    StaleEpoch {
        /// Epoch presented by the caller.
        held: LeaseEpoch,
        /// Epoch the provider considers current.
        current: LeaseEpoch,
    },
    /// One-session-one-Umbra admission refused this session.
    #[error("admission refused: {0}")]
    AdmissionRefused(String),
    /// Takeover was requested. Silence and expiry are never takeover evidence.
    #[error("takeover refused: expiry is not proof of former-writer termination")]
    TakeoverRefused,
    /// Object identity could not be proven across a reconnect or reclaim.
    #[error("object identity unproven: {0}")]
    IdentityUnproven(String),
}

/// Replay-ledger failures: identity, capacity and verifier accounting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ReplayError {
    /// The bounded replay buffer is full. Raised before dispatch, as backpressure.
    #[error("replay buffer exhausted: {records} records, {bytes} payload bytes")]
    CapacityExhausted {
        /// Records currently retained.
        records: u32,
        /// Payload bytes currently retained.
        bytes: u64,
    },
    /// The same idempotency key was reused with a different payload.
    #[error("idempotency key reused with a conflicting payload")]
    KeyConflict,
    /// The same operation id was reused for a different key.
    #[error("operation id reused for a different idempotency key")]
    OperationReused,
    /// Recovery needs the original payload and it was not retained durably.
    #[error("durable payload absent; recorded digest alone cannot rebuild the write")]
    PayloadMissing,
    /// A WRITE/COMMIT verifier changed, so unstable data must be rewritten.
    #[error("write verifier changed: recorded {recorded:?}, observed {observed:?}")]
    VerifierChanged {
        /// Verifier recorded at WRITE time.
        recorded: [u8; 8],
        /// Verifier observed at COMMIT time.
        observed: [u8; 8],
    },
    /// The outcome is genuinely unknown and no durable evidence settles it.
    #[error("indeterminate operation requires reconciliation")]
    Indeterminate,
}

/// The four disjoint failure domains of this provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum FacadeError {
    /// Server-returned `NFS4ERR_*`.
    #[error("protocol: {0}")]
    Protocol(ProtocolError),
    /// RPC/network/framing/scheduling failure.
    #[error("transport: {0}")]
    Transport(TransportError),
    /// Writer-authority or admission failure.
    #[error("authority: {0}")]
    Authority(AuthorityError),
    /// Replay-ledger failure.
    #[error("replay: {0}")]
    Replay(ReplayError),
}

/// Result alias for every facade operation in this crate.
pub type FacadeResult<T> = std::result::Result<T, FacadeError>;

impl FacadeError {
    /// Convenience constructor for a server status at a known COMPOUND position.
    pub fn protocol(status: Nfs4Status, op: OpCode, index: u32) -> Self {
        Self::Protocol(ProtocolError { status, op, index })
    }

    /// The server status word, when this failure came from the server.
    pub fn status(&self) -> Option<Nfs4Status> {
        match self {
            Self::Protocol(error) => Some(error.status),
            _ => None,
        }
    }

    /// What the caller may do next.
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Protocol(error) => protocol_class(error.status),
            Self::Transport(error) => match error {
                TransportError::QueueFull { .. } | TransportError::DeadlineExpired { .. } => {
                    ErrorClass::Retriable
                }
                TransportError::Connect(_) | TransportError::Disconnected { .. } => {
                    ErrorClass::NeedsRecovery
                }
                TransportError::Cancelled(_) => ErrorClass::Permanent,
                TransportError::Malformed(_) | TransportError::UnsupportedProfile(_) => {
                    ErrorClass::SafeStop
                }
            },
            Self::Authority(error) => match error {
                AuthorityError::StaleEpoch { .. } | AuthorityError::NoWriterEpoch => {
                    ErrorClass::Permanent
                }
                AuthorityError::AdmissionRefused(_)
                | AuthorityError::TakeoverRefused
                | AuthorityError::IdentityUnproven(_) => ErrorClass::SafeStop,
            },
            Self::Replay(error) => match error {
                ReplayError::CapacityExhausted { .. } => ErrorClass::Retriable,
                ReplayError::VerifierChanged { .. } => ErrorClass::NeedsRecovery,
                ReplayError::KeyConflict | ReplayError::OperationReused => ErrorClass::Permanent,
                ReplayError::PayloadMissing | ReplayError::Indeterminate => ErrorClass::SafeStop,
            },
        }
    }

    /// Render into Umbra's transport error without discarding this value.
    ///
    /// The context string carries the full facade rendering, so a consumer that
    /// only ever sees an `UmbraError` still receives the raw `NFS4ERR_*` number.
    pub fn to_umbra(&self, operation: &str) -> UmbraError {
        UmbraError::new(self.umbra_kind(), operation, self.to_string())
    }

    fn umbra_kind(&self) -> ErrorKind {
        match self {
            Self::Protocol(error) => protocol_kind(error.status),
            Self::Transport(TransportError::Malformed(_))
            | Self::Transport(TransportError::UnsupportedProfile(_)) => ErrorKind::ProtocolMismatch,
            Self::Transport(_) => ErrorKind::StorageUnavailable,
            Self::Authority(_) => ErrorKind::LeaseLost,
            Self::Replay(ReplayError::KeyConflict) | Self::Replay(ReplayError::OperationReused) => {
                ErrorKind::InvalidInput
            }
            Self::Replay(_) => ErrorKind::StorageUnavailable,
        }
    }
}

fn protocol_class(status: Nfs4Status) -> ErrorClass {
    match status {
        Nfs4Status::DELAY | Nfs4Status::GRACE | Nfs4Status::LOCKED | Nfs4Status::RESOURCE => {
            ErrorClass::Retriable
        }
        Nfs4Status::STALE_CLIENTID
        | Nfs4Status::STALE_STATEID
        | Nfs4Status::OLD_STATEID
        | Nfs4Status::BAD_STATEID
        | Nfs4Status::BAD_SEQID
        | Nfs4Status::EXPIRED
        | Nfs4Status::FHEXPIRED
        | Nfs4Status::LEASE_MOVED
        | Nfs4Status::ADMIN_REVOKED => ErrorClass::NeedsRecovery,
        Nfs4Status::STALE
        | Nfs4Status::BADHANDLE
        | Nfs4Status::MOVED
        | Nfs4Status::SERVERFAULT
        | Nfs4Status::MINOR_VERS_MISMATCH
        | Nfs4Status::NO_GRACE
        | Nfs4Status::RECLAIM_BAD
        | Nfs4Status::RECLAIM_CONFLICT => ErrorClass::SafeStop,
        _ => ErrorClass::Permanent,
    }
}

fn protocol_kind(status: Nfs4Status) -> ErrorKind {
    match status {
        Nfs4Status::NOENT => ErrorKind::NotFound,
        Nfs4Status::EXIST => ErrorKind::AlreadyExists,
        Nfs4Status::NOTSUPP => ErrorKind::UnsupportedCapability,
        Nfs4Status::PERM | Nfs4Status::ACCESS | Nfs4Status::ROFS => ErrorKind::Denied,
        Nfs4Status::INVAL | Nfs4Status::NAMETOOLONG | Nfs4Status::TOOSMALL => {
            ErrorKind::InvalidInput
        }
        Nfs4Status::STALE
        | Nfs4Status::BADHANDLE
        | Nfs4Status::FHEXPIRED
        | Nfs4Status::BAD_STATEID
        | Nfs4Status::STALE_STATEID
        | Nfs4Status::OLD_STATEID
        | Nfs4Status::ADMIN_REVOKED => ErrorKind::StaleHandle,
        Nfs4Status::EXPIRED | Nfs4Status::STALE_CLIENTID | Nfs4Status::OPENMODE => {
            ErrorKind::LeaseLost
        }
        Nfs4Status::SYMLINK => ErrorKind::InvalidPath,
        Nfs4Status::MINOR_VERS_MISMATCH | Nfs4Status::BADXDR => ErrorKind::ProtocolMismatch,
        // Every other code, recognised or not, stays a plain I/O failure carrying
        // its verbatim number. Nothing is silently promoted to success.
        _ => ErrorKind::Io,
    }
}

/// An error that has been recorded durably against one operation identity.
///
/// Retained-error tracking is a first-class concept because the failure model
/// requires that "a recorded completed error stays the result for that key".
/// A retained error is replayed verbatim for its key; new work must use a new
/// operation identity rather than reinterpreting the retained answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedError {
    operation: OperationId,
    key: IdempotencyKey,
    error: FacadeError,
    durable: bool,
}

impl RetainedError {
    /// Record an error against an operation identity.
    ///
    /// `durable` is true only when the record reached the persistence boundary
    /// the replay log promises. A volatile record must not survive a restart as
    /// if it were durable; consumers gate replay on [`RetainedError::is_durable`].
    pub fn record(
        operation: OperationId,
        key: IdempotencyKey,
        error: FacadeError,
        durable: bool,
    ) -> Self {
        Self {
            operation,
            key,
            error,
            durable,
        }
    }

    /// Operation identity this error is bound to.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// Idempotency key this error is the settled answer for.
    pub fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The preserved failure.
    pub fn error(&self) -> &FacadeError {
        &self.error
    }

    /// Whether the record reached the promised persistence boundary.
    pub fn is_durable(&self) -> bool {
        self.durable
    }
}

impl std::fmt::Display for RetainedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "retained ({}) for key {}: {}",
            if self.durable { "durable" } else { "volatile" },
            self.key.0,
            self.error
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_is_preserved_and_never_successful() {
        let unknown = Nfs4Status(65_535);
        assert!(!unknown.is_ok());
        let error = FacadeError::protocol(unknown, OpCode::Write, 3);
        assert_eq!(error.status(), Some(unknown));
        assert!(error.to_umbra("write").context.contains("65535"));
        assert_eq!(error.to_umbra("write").kind, ErrorKind::Io);
    }

    #[test]
    fn seqid_hold_set_matches_rfc_7530_section_9_1_7() {
        for held in [
            Nfs4Status::STALE_CLIENTID,
            Nfs4Status::STALE_STATEID,
            Nfs4Status::BAD_STATEID,
            Nfs4Status::BAD_SEQID,
            Nfs4Status::BADXDR,
            Nfs4Status::RESOURCE,
            Nfs4Status::NOFILEHANDLE,
            Nfs4Status::MOVED,
        ] {
            assert!(held.holds_seqid(), "{held:?} must not advance the seqid");
        }
        for advanced in [Nfs4Status::OK, Nfs4Status::DENIED, Nfs4Status::ACCESS] {
            assert!(!advanced.holds_seqid());
        }
    }

    #[test]
    fn domains_classify_independently() {
        assert_eq!(
            FacadeError::protocol(Nfs4Status::GRACE, OpCode::Open, 0).class(),
            ErrorClass::Retriable
        );
        assert_eq!(
            FacadeError::Authority(AuthorityError::TakeoverRefused).class(),
            ErrorClass::SafeStop
        );
        assert_eq!(
            FacadeError::Replay(ReplayError::PayloadMissing).class(),
            ErrorClass::SafeStop
        );
        assert_eq!(
            FacadeError::Transport(TransportError::QueueFull {
                depth: 8,
                capacity: 8
            })
            .class(),
            ErrorClass::Retriable
        );
    }
}
