//! Shared, backend-independent vocabulary for Umbra's filesystem virtualization.
//! Contracts exchange logical byte paths, process identities, plans, and structured
//! outcomes here; tracing, storage, and namespace implementations live in other crates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod capabilities;
pub mod storage;

mod sandbox;
pub use sandbox::*;

macro_rules! string_id {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        /// A distinct identity type that prevents mixing unrelated contract identifiers.
        pub struct $name(pub String);
    )+};
}

string_id!(WriterId, IdempotencyKey);

pub use storage::*;

mod namespace;
pub use namespace::*;
pub mod provider;

mod agent;
pub use agent::*;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

mod journal;
pub use journal::*;

mod platform;
pub use platform::{PlatformCapabilities, QuiescedTree, TerminationPolicy};

/// A contract result with a structured Umbra error.
pub type Result<T> = std::result::Result<T, UmbraError>;

/// An errno in the tracee's platform ABI, not necessarily the supervisor's ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Errno(pub i32);

impl Errno {
    /// POSIX "no such file or directory". 2 on every Unix target Umbra supports;
    /// named here so the overlay never spells a native number inline when it
    /// denies a whiteout-hidden path.
    pub const ENOENT: Errno = Errno(2);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Machine-readable error categories preserved across provider boundaries.
pub enum ErrorKind {
    /// The operation has not been implemented.
    NotImplemented,
    /// The requested semantics are not supported by this provider.
    UnsupportedCapability,
    /// Denied.
    Denied,
    /// A version, message shape or response identity violates the protocol.
    ProtocolMismatch,
    /// A required storage or provider connection is unavailable.
    StorageUnavailable,
    /// Writer authority is absent, stale or lost.
    LeaseLost,
    /// Journal data fails integrity or recovery validation.
    CorruptJournal,
    /// Path bytes fail the contract validation rules.
    InvalidPath,
    /// Logical symlink expansion exhausted its bound while resolving a path.
    /// Distinct from `InvalidPath` so a caller can answer a link loop with its
    /// own native errno instead of inspecting an error message.
    SymlinkLoop,
    /// The request fails an input constraint.
    InvalidInput,
    /// The operation is invalid in the current lifecycle state.
    InvalidState,
    /// Not found.
    NotFound,
    /// Already exists.
    AlreadyExists,
    /// The resource handle no longer names a valid session resource.
    StaleHandle,
    /// Io.
    Io,
    /// A supervised process exited nonzero or was terminated by a signal. The
    /// run's own preparation and teardown may still have succeeded; this reports
    /// the tracee's outcome, not an Umbra malfunction.
    ProcessFailed,
}

/// A machine-readable failure with the operation and diagnostic context preserved.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{kind:?} during {operation}: {context} (errno: {errno:?})")]
pub struct UmbraError {
    /// Kind.
    pub kind: ErrorKind,
    /// Operation.
    pub operation: String,
    /// Context associated with this value or operation.
    pub context: String,
    /// Optional native error number, retained without translation.
    pub errno: Option<Errno>,
    /// Launch failure evidence from the backend: every created tracee was reaped.
    /// Absent/false is no evidence; callers must never infer this from ErrorKind.
    #[serde(default)]
    pub launch_tree_terminated: bool,
}

impl UmbraError {
    /// Construct an explicit stub failure while preserving structured error fields.
    pub fn not_implemented(operation: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotImplemented, operation, "not implemented")
    }

    /// Construct this value from the supplied configuration or fields.
    pub fn new(kind: ErrorKind, operation: impl Into<String>, context: impl Into<String>) -> Self {
        Self {
            kind,
            operation: operation.into(),
            context: context.into(),
            errno: None,
            launch_tree_terminated: false,
        }
    }

    /// Attach a native error number while preserving the error category.
    pub fn with_errno(mut self, errno: Errno) -> Self {
        self.errno = Some(errno);
        self
    }
}

/// An owned, nonempty Unix path without NUL bytes. No UTF-8 conversion or lexical
/// normalization occurs. Containment and symlink resolution belong to the namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
pub struct BytePath(Vec<u8>);

impl BytePath {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(UmbraError::new(
                ErrorKind::InvalidPath,
                "byte_path",
                "paths must be nonempty and contain no NUL bytes",
            ));
        }
        Ok(Self(bytes))
    }

    /// As bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Into bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Is absolute.
    pub fn is_absolute(&self) -> bool {
        self.0.first() == Some(&b'/')
    }
}

impl TryFrom<Vec<u8>> for BytePath {
    type Error = UmbraError;

    fn try_from(bytes: Vec<u8>) -> Result<Self> {
        Self::new(bytes)
    }
}

impl From<BytePath> for Vec<u8> {
    fn from(path: BytePath) -> Self {
        path.into_bytes()
    }
}

impl AsRef<[u8]> for BytePath {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

macro_rules! id_type {
    ($name:ident, $inner:ty) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        /// A distinct identity type that prevents mixing unrelated contract identifiers.
        pub struct $name(pub $inner);
    };
}

id_type!(TracedFd, i32);
id_type!(ObjectId, Uuid);
id_type!(RunId, Uuid);
id_type!(OperationId, Uuid);

impl OperationId {
    /// Derive a distinct operation identity from this one.
    ///
    /// A storage backend may record which idempotency key an operation ID was
    /// used with, so that a retry can be recognized and a *different* request
    /// under the same ID can be refused. A caller performing several storage
    /// operations on behalf of one logical transaction therefore needs a
    /// distinct identity per request rather than one identity reused with
    /// different keys. For a fixed seed, distinct (salt, index) pairs yield
    /// distinct identities, and a given pair reproduces the same identity.
    /// Transport retries reuse the already-built request context; they do not
    /// re-enter derivation with a new index.
    ///
    /// The result is an opaque 128-bit identity, not a well-formed UUIDv4.
    /// Consumers must not rely on UUID version or variant bits.
    pub fn derive(self, salt: u64, index: u64) -> Self {
        let mut bytes = *self.0.as_bytes();
        for (slot, byte) in bytes[..8].iter_mut().zip(salt.to_be_bytes()) {
            *slot ^= byte;
        }
        for (slot, byte) in bytes[8..].iter_mut().zip(index.to_be_bytes()) {
            *slot ^= byte;
        }
        Self(Uuid::from_bytes(bytes))
    }
}
id_type!(Sequence, u64);
id_type!(LeaseEpoch, u64);

/// Runtime identity assigned by the tracing session. Generation distinguishes reuse
/// of a native ID; exec generation is tracked separately in the process context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskIdentity {
    /// Native id.
    pub native_id: u64,
    /// Generation.
    pub generation: u64,
}

id_type!(TaskId, TaskIdentity);
id_type!(ThreadId, TaskIdentity);
id_type!(ProcessHandle, TaskId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Dir ref.
pub enum DirRef {
    /// Cwd.
    Cwd,
    /// Fd.
    Fd(TracedFd),
}

/// Normalized intent, decoded from native flags by the platform ABI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFlags {
    /// Read.
    pub read: bool,
    /// Write.
    pub write: bool,
    /// Append.
    pub append: bool,
    /// Create.
    pub create: bool,
    /// Exclusive.
    pub exclusive: bool,
    /// Truncate.
    pub truncate: bool,
    /// Directory.
    pub directory: bool,
    /// No follow.
    pub no_follow: bool,
    /// Close on exec.
    pub close_on_exec: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Prot.
pub struct Prot {
    /// Read.
    pub read: bool,
    /// Write.
    pub write: bool,
    /// Execute.
    pub execute: bool,
}

/// Normalized access-check intent, decoded from native mode bits by the platform
/// ABI. All-false is a bare existence probe, the native `F_OK`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessMode {
    /// Read.
    pub read: bool,
    /// Write.
    pub write: bool,
    /// Execute.
    pub execute: bool,
}

/// Normalized access-check flag intent, decoded from native `*at` flags by the
/// platform ABI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessFlags {
    /// Check against the effective identity rather than the real one.
    ///
    /// No namespace reads this today, and that is deliberate rather than an
    /// oversight: a rewrite replaces only the path operand, so the native flag
    /// survives in its own register and the kernel honours it against the
    /// rewritten path. A namespace that starts answering an access probe
    /// without the kernel has to begin honouring this itself.
    pub effective_ids: bool,
    /// Follow.
    pub follow: bool,
}

/// Normalized ownership-change flag intent, decoded from native `*at` flags by
/// the platform ABI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChownFlags {
    /// Follow.
    pub follow: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Map flags.
pub enum MapFlags {
    /// Shared.
    Shared,
    /// Private.
    Private,
}

/// Filesystem intent, independent of native syscall numbers and flag encodings.
/// Decoders must reject unclassified potential mutations, never treat them as reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsOp {
    /// Open.
    Open {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Flags.
        flags: OpenFlags,
        /// Mode.
        mode: u32,
    },
    /// Stat.
    Stat {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Follow.
        follow: bool,
    },
    /// Access.
    Access {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Mode.
        mode: AccessMode,
        /// Flags.
        flags: AccessFlags,
    },
    /// Rename.
    Rename {
        /// From dir.
        from_dir: DirRef,
        /// From.
        from: BytePath,
        /// To dir.
        to_dir: DirRef,
        /// To.
        to: BytePath,
    },
    /// Link.
    Link {
        /// From dir.
        from_dir: DirRef,
        /// From.
        from: BytePath,
        /// To dir.
        to_dir: DirRef,
        /// To.
        to: BytePath,
        /// Follow.
        follow: bool,
    },
    /// Symlink.
    Symlink {
        /// Target.
        target: BytePath,
        /// Link dir.
        link_dir: DirRef,
        /// Link name.
        link_name: BytePath,
    },
    /// Unlink.
    Unlink {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Directory.
        directory: bool,
    },
    /// Mkdir.
    Mkdir {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Mode.
        mode: u32,
    },
    /// Chdir.
    Chdir {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
    },
    /// Fchdir.
    Fchdir {
        /// Fd.
        fd: TracedFd,
    },
    /// Get cwd.
    GetCwd,
    /// Read link.
    ReadLink {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
    },
    /// Mmap file.
    MmapFile {
        /// Fd.
        fd: TracedFd,
        /// Protection.
        protection: Prot,
        /// Flags.
        flags: MapFlags,
    },
    /// Read dir.
    ReadDir {
        /// Fd.
        fd: TracedFd,
        /// Max bytes.
        max_bytes: u32,
    },
    /// Read.
    Read {
        /// Fd.
        fd: TracedFd,
        /// Length.
        length: u64,
        /// Absolute byte offset from the beginning of the object.
        offset: Option<u64>,
    },
    /// Write.
    Write {
        /// Fd.
        fd: TracedFd,
        /// Length.
        length: u64,
        /// Absolute byte offset from the beginning of the object.
        offset: Option<u64>,
    },
    /// Close.
    Close {
        /// Fd.
        fd: TracedFd,
    },
    /// Dup.
    Dup {
        /// Fd.
        fd: TracedFd,
        /// Target.
        target: Option<TracedFd>,
        /// Close on exec.
        close_on_exec: bool,
    },
    /// Fstat.
    Fstat {
        /// Fd.
        fd: TracedFd,
    },
    /// Truncate.
    Truncate {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Length.
        length: u64,
    },
    /// Ftruncate.
    Ftruncate {
        /// Fd.
        fd: TracedFd,
        /// Length.
        length: u64,
    },
    /// Chmod.
    Chmod {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Mode.
        mode: u32,
        /// Follow.
        follow: bool,
    },
    /// Fchmod.
    Fchmod {
        /// Fd.
        fd: TracedFd,
        /// Mode.
        mode: u32,
    },
    /// Set the access and modification times of a named object.
    ///
    /// Darwin's `utimensat` carries no syscall of its own -- it builds an
    /// attribute list and reaches the kernel as `setattrlistat(2)` -- so this
    /// operation is what that call decodes to. Each time is optional and `None`
    /// means *leave this one as it is*, which is how `UTIME_OMIT` and Linux's
    /// `utimensat` alike spell a one-sided change.
    ///
    /// The values are absolute, already-resolved nanoseconds. "Use the current
    /// time" is not representable and does not need to be: the caller's libc has
    /// substituted the clock reading before the syscall is made, so a decoder
    /// that tried to re-resolve it here would be answering with a different
    /// instant than the program asked for.
    SetTimes {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Accessed nanos. `None` leaves the access time unchanged.
        accessed_nanos: Option<i128>,
        /// Modified nanos. `None` leaves the modification time unchanged.
        modified_nanos: Option<i128>,
        /// Follow.
        follow: bool,
    },
    /// Fchownat.
    Fchownat {
        /// Dir.
        dir: DirRef,
        /// Path interpreted according to the enclosing operation and path type.
        path: BytePath,
        /// Uid. `None` is the native `-1` unchanged-ID sentinel.
        uid: Option<u32>,
        /// Gid. `None` is the native `-1` unchanged-ID sentinel.
        gid: Option<u32>,
        /// Flags.
        flags: ChownFlags,
    },
    /// Sync.
    Sync {
        /// Fd.
        fd: TracedFd,
        /// Data only.
        data_only: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Resolved action.
pub enum ResolvedAction {
    /// Allow base read.
    AllowBaseRead,
    /// Rewrite.
    Rewrite(PhysicalOperation),
    /// Emulate.
    Emulate(EmulatedResult),
    /// Deny.
    Deny(Errno),
}

/// Runtime-only physical path; must never be used as persistent object identity or
/// included in a manifest/journal payload. Serializability is for provider IPC only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPath(pub BytePath);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Path operand.
pub enum PathOperand {
    /// Path.
    Path,
    /// Source.
    Source,
    /// Destination.
    Destination,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Path rewrite.
pub struct PathRewrite {
    /// Operand.
    pub operand: PathOperand,
    /// Path interpreted according to the enclosing operation and path type.
    pub path: PhysicalPath,
}

/// A runtime plan, not evidence that copy-up or journal preparation has occurred.
/// Only qualified targets inside the selected run may receive mutations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalOperation {
    /// Operation.
    pub operation: FsOp,
    /// Paths.
    pub paths: Vec<PathRewrite>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Memory write.
pub struct MemoryWrite {
    /// Address.
    pub address: u64,
    /// Owned bytes; no UTF-8 conversion is implied.
    pub bytes: Vec<u8>,
}

/// ABI argument index and replacement value, prepared for the negotiated ABI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgumentRewrite {
    /// Index.
    pub index: u8,
    /// Value.
    pub value: u64,
}

/// An executable rewrite after namespace preparation. The platform must validate
/// argument indices and address/length bounds before applying scratch-memory writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedRewrite {
    /// Operation.
    pub operation: PhysicalOperation,
    /// Arguments.
    pub arguments: Vec<ArgumentRewrite>,
    /// Memory writes.
    pub memory_writes: Vec<MemoryWrite>,
}

/// A normalized syscall result; platform code translates native error conventions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationOutcome {
    /// Success.
    Success {
        /// Return value.
        return_value: u64,
    },
    /// Failure.
    Failure(Errno),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Emulated result.
pub struct EmulatedResult {
    /// Outcome.
    pub outcome: OperationOutcome,
    /// Memory writes.
    pub memory_writes: Vec<MemoryWrite>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Architecture.
pub enum Architecture {
    /// Aarch64.
    Aarch64,
    /// X86 64.
    X86_64,
    /// Unsupported.
    Unsupported(String),
}

/// Max register bytes.
pub const MAX_REGISTER_BYTES: usize = 4096;

/// Placeholder register transport, with provider-negotiated encoding. It contains
/// no OS binding types. Construction and deserialization enforce the size bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RegisterData", into = "RegisterData")]
pub struct RegisterSet {
    architecture: Architecture,
    bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct RegisterData {
    architecture: Architecture,
    bytes: Vec<u8>,
}

impl RegisterSet {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(architecture: Architecture, bytes: Vec<u8>) -> Result<Self> {
        if matches!(architecture, Architecture::Unsupported(_)) {
            return Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                "register_set",
                "unsupported register architecture",
            ));
        }
        if bytes.len() > MAX_REGISTER_BYTES {
            return Err(UmbraError::new(
                ErrorKind::InvalidInput,
                "register_set",
                "register data exceeds the size limit",
            ));
        }
        Ok(Self {
            architecture,
            bytes,
        })
    }

    /// Architecture.
    pub fn architecture(&self) -> &Architecture {
        &self.architecture
    }

    /// As bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// As bytes mut.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl TryFrom<RegisterData> for RegisterSet {
    type Error = UmbraError;

    fn try_from(data: RegisterData) -> Result<Self> {
        Self::new(data.architecture, data.bytes)
    }
}

impl From<RegisterSet> for RegisterData {
    fn from(regs: RegisterSet) -> Self {
        Self {
            architecture: regs.architecture,
            bytes: regs.bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Fd state.
pub struct FdState {
    /// Object.
    pub object: ObjectId,
    /// Logical path.
    pub logical_path: Option<BytePath>,
    /// Directory.
    pub directory: bool,
    /// Flags.
    pub flags: OpenFlags,
    /// Current file position, in bytes from the start of the object.
    ///
    /// [`FsOp::Read`] and [`FsOp::Write`] carry `offset: Option<u64>`, where
    /// `None` means "at the descriptor's current position" -- so without this
    /// field there is no position to mean, and a positionless write could not be
    /// placed at all. Advanced only after an operation's outcome has been
    /// observed successful, like every other field of a [`ProcessContext`]:
    /// a refused write must not move the position POSIX says it did not move.
    pub offset: u64,
}

/// A tracee memory buffer an intercepted data transfer names.
///
/// Runtime-only, exactly like [`PhysicalPath`] and [`MemoryWrite::address`]: it
/// is an address in one stopped task at one moment, never an object identity,
/// and it must not reach a manifest or a journal payload. It exists because
/// [`FsOp`] is deliberately ABI-independent -- `FsOp::Read` carries how *many*
/// bytes, never *where* they go -- so the address has to travel beside the
/// operation rather than inside it, and only the ABI can read it out of the
/// registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoBuffer {
    /// Address in the stopped task's address space.
    pub address: u64,
    /// Byte count the transfer names. Never larger than [`MAX_IO_BYTES`].
    pub length: u32,
}

/// The runtime details a *routed* operation needs and its [`FsOp`] cannot carry.
///
/// A routed operation is one umbra services itself, through the run's storage,
/// because the backend exposes no kernel-visible path to rewrite a syscall
/// operand to. Three things are then needed that an ABI-independent operation
/// deliberately has no room for, and all three are supplied together so the
/// namespace consumes one binding per resolution rather than three:
///
/// * the descriptor number to answer an `Open` with, which is the caller's to
///   allocate because the caller owns [`ProcessContext::fds`] and knows the
///   fence the kernel's own range was limited to;
/// * where a `Read`'s bytes go, which only the ABI can read out of the
///   registers; and
/// * the bytes a `Write` must persist, because [`FsOp::Write`] carries how many
///   there are and never which they are.
///
/// Bound before `resolve` and **consumed by it**, including by a resolution that
/// fails, on the same terms as `set_readlink_buffer`: a binding that survived a
/// failed resolve would be applied to the next, unrelated operation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutedRequest {
    /// Descriptor to answer a routed `Open` with, at or above the tracee's
    /// `RLIMIT_NOFILE` fence so it cannot be a number the kernel also issued.
    pub descriptor: Option<RoutedInput<TracedFd>>,
    /// Tracee buffer a routed `Read` fills.
    pub read_buffer: Option<RoutedInput<IoBuffer>>,
    /// Bytes a routed `Write` persists.
    ///
    /// **Exactly as many as the operation names**, and the namespace enforces
    /// that equality rather than treating it as an upper bound: a binding
    /// shorter than the operation means the caller read the wrong buffer, which
    /// is a wiring fault and not a short write. Shortening for a backend whose
    /// per-run I/O bound is smaller happens *after* that check, inside the
    /// namespace, and is what makes the answer a POSIX-legal short write.
    pub write_bytes: Option<RoutedInput<Vec<u8>>>,
}

/// One routing input, or the errno POSIX gives when the caller cannot supply it.
///
/// **The refusal has to travel *inside* the binding, and that is the whole
/// reason this type exists.** The binding is built before `resolve`, and
/// `resolve` is the only thing that can produce a tracee-visible
/// [`ResolvedAction::Deny`] -- so a caller that returned an error instead would
/// stop the entire run over an ordinary program bug. Measured, before this
/// existed: `read(fd, NULL, 4)` on a routed descriptor killed the run, where
/// Darwin answers `EFAULT`; and exhausting the fenced descriptor range did the
/// same where POSIX says `EMFILE`.
///
/// So the caller reports what it could not do, and `resolve` turns it into the
/// answer the program expects. An input that is genuinely absent stays `None`;
/// this is for one the caller *tried* to produce and the tracee's own request
/// made impossible.
pub type RoutedInput<T> = std::result::Result<T, Errno>;

/// Logical namespace context cloned on fork and updated only after observed success.
/// Native mappings, breakpoint inventory, and scratch allocations remain runtime
/// platform state; they must not be restored as persistent process identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessContext {
    /// Task.
    pub task: TaskId,
    /// Parent.
    pub parent: Option<TaskId>,
    /// Architecture.
    pub architecture: Architecture,
    /// Abi.
    pub abi: String,
    /// Exec generation.
    pub exec_generation: u64,
    /// Cwd.
    pub cwd: BytePath,
    /// Root.
    pub root: BytePath,
    /// Fds.
    pub fds: BTreeMap<TracedFd, FdState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Child kind.
pub enum ChildKind {
    /// Fork.
    Fork,
    /// Vfork.
    Vfork,
    /// Spawn.
    Spawn,
    /// Clone.
    Clone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Exit status.
pub enum ExitStatus {
    /// Code.
    Code(i32),
    /// Signal.
    Signal(i32),
}

/// Child events must be delivered while the child is stopped, before mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEvent {
    /// Syscall entry.
    SyscallEntry {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
        /// Registers.
        registers: RegisterSet,
    },
    /// Syscall exit.
    SyscallExit {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
        /// Outcome.
        outcome: OperationOutcome,
    },
    /// Child.
    Child {
        /// Parent.
        parent: TaskId,
        /// Child.
        child: ProcessHandle,
        /// Kind.
        kind: ChildKind,
    },
    /// Exec.
    Exec {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
        /// Exec generation.
        exec_generation: u64,
    },
    /// Exit.
    Exit {
        /// Task.
        task: TaskId,
        /// Status.
        status: ExitStatus,
    },
    /// Thread started.
    ThreadStarted {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
    },
    /// Thread exited.
    ThreadExited {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
    },
    /// Signal.
    Signal {
        /// Task.
        task: TaskId,
        /// Thread.
        thread: ThreadId,
        /// Signal.
        signal: i32,
    },
}

/// Explicit durability selection; local development does not qualify as remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistencePolicy {
    /// Strict remote.
    StrictRemote,
    /// Local development.
    LocalDevelopment,
    /// Runs on a validated NFSv4 mount whose durability boundary is the client
    /// fsync only. This is distinct from `LocalDevelopment` so a backend need not
    /// mislabel a mounted run, and distinct from `StrictRemote` because remote
    /// commit is not qualified. It promises no server-side durability.
    NfsClientFsync,
}

/// Every launch requires enforcement and stopped descendant capture before running.
/// Providers reject unsupported requirements rather than weakening these guarantees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPolicy {
    /// Persistence.
    pub persistence: PersistencePolicy,
    /// The platform must validate these and close/sanitize all other inherited FDs.
    pub inherited_fds: Vec<TracedFd>,
    /// Load umbra's userspace-routing interposer into the target image before its
    /// first instruction, and intercept the requests it traps.
    ///
    /// Required for a run whose storage exposes no kernel-visible path, where a
    /// syscall path rewrite has no target to name. A platform that cannot do this
    /// must refuse the launch rather than start an unrouted tracee: a tracee
    /// whose file operations are neither rewritten nor routed would read and
    /// write the host, which enforcement would then refuse -- late, and only for
    /// writes.
    pub interpose: bool,
    /// Ceiling on the descriptor numbers the kernel may allocate to the tracee
    /// and every descendant, applied as `RLIMIT_NOFILE` -- **soft and hard** --
    /// before the final exec.
    ///
    /// This is what makes an interposer's virtual descriptors safe for the whole
    /// tracee lifetime rather than only at the instant one is allocated. POSIX
    /// requires `open` to return the lowest free number, so "this number is free
    /// now" is not an invariant: a later real `open` can legitimately be handed
    /// it. Fencing the kernel's range instead makes the two ranges disjoint by
    /// construction -- the kernel never returns a number at or above the limit,
    /// and the interposer allocates only at or above it.
    ///
    /// **Lowering the hard limit is the load-bearing half.** With only the soft
    /// limit lowered the tracee can raise it back and walk into the virtual
    /// range; with the hard limit lowered too, raising it answers `EPERM` and a
    /// soft limit above it answers `EINVAL`. The bound survives `fork` and
    /// `exec`.
    ///
    /// The cost is stated rather than hidden: the tracee may hold at most this
    /// many descriptors, `getrlimit` and `sysconf(_SC_OPEN_MAX)` report it, and a
    /// program wanting more is refused `EMFILE` -- an honest POSIX answer. It
    /// also fails *closed* for everything the interposer does not implement: a
    /// virtual descriptor is not a kernel object, so `lseek`, `fstat`, `dup`,
    /// `fcntl`, `mmap`, `fsync` and `ftruncate` on one receive `EBADF` from the
    /// kernel rather than a plausible wrong answer.
    pub descriptor_limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Environment variable.
pub struct EnvironmentVariable {
    /// Name.
    pub name: Vec<u8>,
    /// Value.
    pub value: Vec<u8>,
}

/// Declarative supervised launch. Arguments include argv[0]; environment is explicit.
/// Providers validate argv/environment NULs and environment names before launching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// Explicit executable path; never resolved by scanning PATH.
    pub executable: BytePath,
    /// Argv.
    pub argv: Vec<Vec<u8>>,
    /// Environment.
    pub environment: Vec<EnvironmentVariable>,
    /// Cwd.
    pub cwd: BytePath,
    /// Policy.
    pub policy: LaunchPolicy,
    /// Enforcement to install before the target's first instruction. There is no
    /// default: a launch cannot become unsandboxed by omitting this field.
    pub sandbox: SandboxRequirement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Resume mode.
pub enum ResumeMode {
    /// Continue.
    Continue,
    /// Syscall.
    Syscall,
    /// Single step.
    SingleStep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Resume command.
pub struct ResumeCommand {
    /// Thread.
    pub thread: ThreadId,
    /// Mode.
    pub mode: ResumeMode,
    /// None suppresses signal delivery; Some forwards a validated native signal.
    pub signal: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::value::{Error as ValueError, SeqDeserializer};

    #[test]
    fn byte_paths_preserve_non_utf8_and_unresolved_components() {
        let bytes = b"../\xff/./file".to_vec();
        let path = BytePath::new(bytes.clone()).unwrap();
        assert_eq!(path.as_bytes(), bytes);
        assert!(!path.is_absolute());
        assert_eq!(path.into_bytes(), bytes);
    }

    #[test]
    fn wire_paths_cannot_bypass_validation() {
        for bytes in [vec![], b"file\0suffix".to_vec()] {
            let input = SeqDeserializer::<_, ValueError>::new(bytes.into_iter());
            assert!(BytePath::deserialize(input).is_err());
        }
        let input = SeqDeserializer::<_, ValueError>::new(vec![b'/', 0xff].into_iter());
        assert_eq!(
            BytePath::deserialize(input).unwrap().as_bytes(),
            &[b'/', 0xff]
        );
    }

    #[test]
    fn registers_reject_oversized_data_and_unknown_architectures() {
        let mut regs =
            RegisterSet::new(Architecture::Aarch64, vec![0; MAX_REGISTER_BYTES]).unwrap();
        regs.as_bytes_mut()[0] = 1;
        assert_eq!(regs.as_bytes()[0], 1);
        let oversized = RegisterSet::new(Architecture::X86_64, vec![0; MAX_REGISTER_BYTES + 1]);
        assert_eq!(oversized.unwrap_err().kind, ErrorKind::InvalidInput);
        let unknown = RegisterSet::new(Architecture::Unsupported("unknown".into()), vec![]);
        assert_eq!(unknown.unwrap_err().kind, ErrorKind::UnsupportedCapability);
    }
}

#[cfg(test)]
mod launch_evidence_tests {
    use super::*;
    #[test]
    fn termination_evidence_is_explicit_and_survives_transport() {
        let mut e = UmbraError::new(ErrorKind::InvalidInput, "launch", "failed");
        assert!(!e.launch_tree_terminated);
        e.launch_tree_terminated = true;
        let mut encoded = serde_json::to_value(&e).unwrap();
        assert!(
            serde_json::from_value::<UmbraError>(encoded.clone())
                .unwrap()
                .launch_tree_terminated
        );
        encoded
            .as_object_mut()
            .unwrap()
            .remove("launch_tree_terminated");
        assert!(
            !serde_json::from_value::<UmbraError>(encoded)
                .unwrap()
                .launch_tree_terminated
        );
    }
}
