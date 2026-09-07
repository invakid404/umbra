//! Shared, backend-independent vocabulary for Umbra's filesystem virtualization.
//! Contracts exchange logical byte paths, process identities, plans, and structured
//! outcomes here; tracing, storage, and namespace implementations live in other crates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod storage;

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
}

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
}

/// Every launch requires enforcement and stopped descendant capture before running.
/// Providers reject unsupported requirements rather than weakening these guarantees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPolicy {
    /// Persistence.
    pub persistence: PersistencePolicy,
    /// The platform must validate these and close/sanitize all other inherited FDs.
    pub inherited_fds: Vec<TracedFd>,
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
