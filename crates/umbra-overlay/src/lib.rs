//! Storage-independent MVP overlay with explicit runtime and immutable-base injection.
//!
//! Reads prefer the shadow, respect whiteouts, then consult the immutable base.
//! Writes require journaled materialisation into the selected shadow. Classification
//! alone never authorizes a syscall: mutation requires journaled preparation.
//! No backend selection, host filesystem I/O, or physical mount identity lives here.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use umbra_core::{
    AbortReason, Checkpoint, CheckpointRequest, CommitReceipt, OperationId, OperationOutcome,
    PreparedAction,
};
use umbra_core::{BytePath, FsOp, MapFlags, ProcessContext, ResolvedAction, Result};
pub use umbra_journal::Journal;
pub use umbra_storage::{
    ApprovedBaseObject, RequestContext, Storage, StorageCapabilities, StoragePath,
};

/// Object-safe namespace contract from the handoff.
///
/// Resolution produces a plan; it must not copy up, change whiteouts, or perform
/// any other unjournaled mutation. The supervisor must deny/stop on an error.
pub trait NamespaceResolver {
    /// Resolve the normalized operation without performing unjournaled mutations.
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp) -> Result<ResolvedAction>;
}

/// Transaction lifecycle; errors require retaining a stopped or recovery-required run.
pub trait NamespaceSession: NamespaceResolver {
    /// Inject native directory encoding and the current syscall's output-buffer binding.
    /// The encoder owns ABI knowledge; the overlay owns merged names and continuation.
    fn set_directory_encoder(&mut self, _encoder: Box<dyn DirectoryEncoder>) -> Result<()> {
        Err(umbra_core::UmbraError::new(
            umbra_core::ErrorKind::UnsupportedCapability,
            "overlay.directory_encoder",
            "provider does not support directory encoding",
        ))
    }
    /// Bind already-open, same-run storage/journal sessions and an approved immutable base.
    fn bind(&mut self, _config: SessionConfig, _base: Box<dyn Base>) -> Result<()> {
        Err(umbra_core::UmbraError::new(
            umbra_core::ErrorKind::UnsupportedCapability,
            "overlay.bind",
            "provider does not support runtime binding",
        ))
    }
    /// Read logical bytes through the overlay without native ABI memory encoding.
    fn read_at(&mut self, _path: &StoragePath, _offset: u64, _out: &mut [u8]) -> Result<usize> {
        Err(umbra_core::UmbraError::new(
            umbra_core::ErrorKind::UnsupportedCapability,
            "overlay.read_at",
            "provider does not expose typed reads",
        ))
    }
    /// Return a merged snapshot page; cursors are session-bound and repeatable.
    fn list(
        &mut self,
        _path: &StoragePath,
        _cursor: Option<&umbra_core::ListCursor>,
        _limit: u32,
    ) -> Result<umbra_core::DirectoryPage> {
        Err(umbra_core::UmbraError::new(
            umbra_core::ErrorKind::UnsupportedCapability,
            "overlay.list",
            "provider does not expose typed directory pages",
        ))
    }
    /// Prepare journaled execution for the resolved operation.
    fn prepare(
        &mut self,
        operation: OperationId,
        action: &ResolvedAction,
    ) -> Result<PreparedAction>;
    /// Record the observed kernel or emulated outcome before commit.
    fn observe_result(&mut self, operation: OperationId, result: &OperationOutcome) -> Result<()>;
    /// Commit the prepared operation after its result has been observed.
    fn commit(&mut self, operation: OperationId) -> Result<CommitReceipt>;
    /// Reconcile or abandon the operation without claiming arbitrary writes were undone.
    fn abort(&mut self, operation: OperationId, reason: &AbortReason) -> Result<()>;
    /// Request a logical checkpoint after the caller establishes quiescence.
    fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint>;
}

/// Construct the standard namespace engine without I/O or backend selection.
pub fn standard_namespace(
    storage: Box<dyn Storage>,
    journal: Box<dyn Journal>,
) -> Box<dyn NamespaceSession + Send> {
    Box::new(Overlay::new(storage, journal))
}

const _: Option<&dyn NamespaceResolver> = None;
const _: Option<&dyn NamespaceSession> = None;

/// Policy dispatch, not an executable action or proof of descriptor provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Shadow first, whiteout means absent, otherwise immutable-base read.
    ReadThrough,
    /// Prepare parents/copy-up as necessary before any shadow mutation.
    Materialise,
    /// Remove a shadow name and commit a base whiteout after observed success.
    Whiteout,
    /// Merge base/shadow names with whiteouts and stable continuation semantics.
    MergeDirectory,
    /// Validate tracked descriptor/process state before emulation or execution.
    ProcessState,
}

/// Exhaustively classify normalized operations. Newly added core variants require
/// an explicit policy here instead of falling into a permissive default.
pub fn dispatch(operation: &FsOp) -> Dispatch {
    match operation {
        FsOp::Open { flags, .. } => {
            if flags.write || flags.append || flags.create || flags.truncate {
                Dispatch::Materialise
            } else {
                Dispatch::ReadThrough
            }
        }
        FsOp::Stat { .. } | FsOp::ReadLink { .. } | FsOp::Read { .. } | FsOp::Fstat { .. } => {
            Dispatch::ReadThrough
        }
        FsOp::Rename { .. }
        | FsOp::Link { .. }
        | FsOp::Symlink { .. }
        | FsOp::Mkdir { .. }
        | FsOp::Truncate { .. }
        | FsOp::Ftruncate { .. }
        | FsOp::Chmod { .. }
        | FsOp::Fchmod { .. }
        | FsOp::Write { .. } => Dispatch::Materialise,
        FsOp::MmapFile {
            protection, flags, ..
        } => {
            if protection.write && *flags == MapFlags::Shared {
                Dispatch::Materialise
            } else {
                Dispatch::ReadThrough
            }
        }
        FsOp::Unlink { .. } => Dispatch::Whiteout,
        FsOp::ReadDir { .. } => Dispatch::MergeDirectory,
        FsOp::Chdir { .. }
        | FsOp::Fchdir { .. }
        | FsOp::GetCwd
        | FsOp::Close { .. }
        | FsOp::Dup { .. }
        | FsOp::Sync { .. } => Dispatch::ProcessState,
    }
}

/// A lexical byte component. Dot and parent components are deliberately retained:
/// `symlink/..` cannot be collapsed before resolving the symlink itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component<'a> {
    /// Empty.
    Empty,
    /// Current.
    Current,
    /// Parent.
    Parent,
    /// Name.
    Name(&'a [u8]),
}

/// Split on Unix separators without UTF-8 conversion or normalization.
///
/// Leading/trailing/repeated separators remain empty components, preserving the
/// evidence needed for absolute-root and trailing-directory semantics. This is
/// tokenization only, never containment validation or symlink resolution.
pub fn components(path: &BytePath) -> impl Iterator<Item = Component<'_>> {
    path.as_bytes()
        .split(|byte| *byte == b'/')
        .map(|part| match part {
            b"" => Component::Empty,
            b"." => Component::Current,
            b".." => Component::Parent,
            name => Component::Name(name),
        })
}

mod engine;
pub use engine::{Base, DirectoryEncoder, EncodedDirectory, Overlay, SessionConfig, StorageBase};

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{DirRef, OpenFlags, Prot, TracedFd};

    #[test]
    fn byte_components_retain_symlink_parent_and_directory_semantics() {
        let path = BytePath::new(b"/\xff//link/.././".to_vec()).unwrap();
        assert_eq!(
            components(&path).collect::<Vec<_>>(),
            vec![
                Component::Empty,
                Component::Name(b"\xff"),
                Component::Empty,
                Component::Name(b"link"),
                Component::Parent,
                Component::Current,
                Component::Empty,
            ]
        );
    }

    #[test]
    fn every_open_mutation_flag_requires_materialisation() {
        // Even read+create and truncate without an ordinary write bit cannot pass
        // through to the immutable base. Cover all normalized flag combinations.
        for bits in 0..16 {
            let op = FsOp::Open {
                dir: DirRef::Cwd,
                path: BytePath::new(b"file".to_vec()).unwrap(),
                flags: OpenFlags {
                    read: true,
                    write: bits & 1 != 0,
                    append: bits & 2 != 0,
                    create: bits & 4 != 0,
                    truncate: bits & 8 != 0,
                    ..OpenFlags::default()
                },
                mode: 0o600,
            };
            assert_eq!(
                dispatch(&op),
                if bits == 0 {
                    Dispatch::ReadThrough
                } else {
                    Dispatch::Materialise
                }
            );
        }
    }

    #[test]
    fn shared_writable_mapping_requires_shadow_but_private_mapping_does_not() {
        for flags in [MapFlags::Shared, MapFlags::Private] {
            for write in [false, true] {
                let op = FsOp::MmapFile {
                    fd: TracedFd(3),
                    protection: Prot {
                        read: true,
                        write,
                        execute: false,
                    },
                    flags,
                };
                assert_eq!(
                    dispatch(&op),
                    if write && flags == MapFlags::Shared {
                        Dispatch::Materialise
                    } else {
                        Dispatch::ReadThrough
                    }
                );
            }
        }
    }
}

/// Versioned provider protocol, server harness and trait proxy.
#[cfg(unix)]
pub mod provider;
