//! Storage-independent overlay policy and fail-closed M0 namespace scaffolding.
//!
//! Reads prefer the shadow, respect whiteouts, then consult the immutable base.
//! Writes require journaled materialisation into the selected shadow. Classification
//! alone never authorizes a syscall: every execution path currently returns an
//! not-implemented error until anchored resolution and transactions exist.
//! No backend selection, host filesystem I/O, or physical mount identity lives here.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use umbra_core::{
    AbortReason, Checkpoint, CheckpointRequest, CommitReceipt, OperationId, OperationOutcome,
    PreparedAction,
};
use umbra_core::{BytePath, FsOp, MapFlags, ProcessContext, ResolvedAction, Result, UmbraError};
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

/// Standard engine, owning injected contracts rather than concrete backends.
///
/// M0 has no active namespace session. Construction performs no I/O, and all
/// operational methods fail closed. Storage and Journal must eventually refer to
/// the same run, with one fenced writer and ordered prepare/result/commit handling.
pub struct Overlay {
    storage: Box<dyn Storage>,
    journal: Box<dyn Journal>,
}

impl Overlay {
    /// Construct this value from the supplied configuration or fields.
    pub fn new(storage: Box<dyn Storage>, journal: Box<dyn Journal>) -> Self {
        Self { storage, journal }
    }

    /// Backend capabilities are information, not qualification of this overlay.
    pub fn storage_capabilities(&self) -> StorageCapabilities {
        self.storage.capabilities()
    }

    /// Return ownership without closing, flushing, or asserting a clean handoff.
    pub fn into_backends(self) -> (Box<dyn Storage>, Box<dyn Journal>) {
        (self.storage, self.journal)
    }

    /// Plan shadow/whiteout/base lookup without triggering materialisation.
    /// Descriptor reads additionally require validated tracked object identity.
    fn read_through(
        &mut self,
        _context: &ProcessContext,
        _operation: &FsOp,
    ) -> Result<ResolvedAction> {
        Err(not_implemented("overlay.read_through"))
    }

    /// Plan copy-up/create/link/rename or validate a pathless shadow mutation.
    /// Writable descriptors and shared writable mappings must already reference
    /// shadow objects; copy-up alone cannot retarget an existing kernel handle.
    fn materialise(
        &mut self,
        _context: &ProcessContext,
        _operation: &FsOp,
    ) -> Result<ResolvedAction> {
        Err(not_implemented("overlay.materialise"))
    }
}

impl NamespaceResolver for Overlay {
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp) -> Result<ResolvedAction> {
        match dispatch(operation) {
            Dispatch::ReadThrough => self.read_through(context, operation),
            Dispatch::Materialise => self.materialise(context, operation),
            Dispatch::Whiteout => Err(not_implemented("overlay.whiteout")),
            Dispatch::MergeDirectory => Err(not_implemented("overlay.merge_directory")),
            Dispatch::ProcessState => Err(not_implemented("overlay.process_state")),
        }
    }
}

impl NamespaceSession for Overlay {
    fn prepare(
        &mut self,
        _operation: OperationId,
        _action: &ResolvedAction,
    ) -> Result<PreparedAction> {
        Err(not_implemented("overlay.prepare"))
    }
    fn observe_result(
        &mut self,
        _operation: OperationId,
        _result: &OperationOutcome,
    ) -> Result<()> {
        Err(not_implemented("overlay.observe_result"))
    }
    fn commit(&mut self, _operation: OperationId) -> Result<CommitReceipt> {
        Err(not_implemented("overlay.commit"))
    }
    fn abort(&mut self, _operation: OperationId, _reason: &AbortReason) -> Result<()> {
        Err(not_implemented("overlay.abort"))
    }
    fn checkpoint(&mut self, _request: &CheckpointRequest) -> Result<Checkpoint> {
        Err(not_implemented("overlay.checkpoint"))
    }
}

fn not_implemented(operation: &'static str) -> UmbraError {
    UmbraError::not_implemented(operation)
}

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
