//! Capability matrix: what M1 offers, what it refuses, and what is deferred.
//!
//! The rows below are the single source of truth for two things that must never
//! drift apart: the error the provider actually returns for an operation, and the
//! capability table published in the M1 operations report. A reviewer reading the
//! table is reading the code path.
//!
//! # Three verdicts, and why the middle one is not "unsupported"
//!
//! * [`Support::Supported`] — implemented here against the frozen transport
//!   facade and exercised by this crate's tests.
//! * [`Support::Unsupported`] — semantics this provider will not offer. The
//!   pinned `docs/design/syscall-matrix.md` marks these *rejected*, or assigns
//!   them to the overlay rather than to storage. Answering
//!   [`ErrorKind::UnsupportedCapability`] is the whole contract: no silent
//!   fallback, no empty-but-successful answer, no discarded write.
//! * [`Support::Deferred`] — authorised by the syscall matrix (it marks them
//!   *emulated*) but not reachable from this crate today, because the frozen
//!   [`Nfs4Op`](crate::transport::Nfs4Op) carries no argument variant for the
//!   NFSv4.0 operation they need. Reporting these as unsupported would understate
//!   the design exactly as reporting them as supported would overstate it, so they
//!   answer [`ErrorKind::NotImplemented`] naming the owner, which is the idiom
//!   [`crate::storage`] already uses for "authorised but not yet wired".
//!
//! The distinction is load-bearing. `OpCode::Remove`, `OpCode::Rename`,
//! `OpCode::Create` and `OpCode::SetAttr` all exist in the frozen transport, but
//! `Nfs4Op` has no variant that can carry their arguments, so no COMPOUND in this
//! crate can encode them. That is a contracts gap, not a capability decision, and
//! it is recorded as such.

use umbra_core::{CreateKind, ErrorKind, Result, StorageOperation, UmbraError};

/// The node that owns closing a deferred gap.
///
/// A contracts revision is a different kind of work from wiring a module, so the
/// two are named separately rather than folded into one "later" string.
pub const CONTRACTS_REVISION: &str = "contracts (frozen transport facade)";

/// The node that owns binding a live transport into the provider.
pub const M1_INTEGRATE: &str = "m1_integrate";

/// Why a deferred namespace mutation cannot be dispatched from this crate.
pub const NO_WIRE_OPERATION: &str =
    "the frozen Nfs4Op carries no argument variant for this NFSv4.0 operation, \
     so no COMPOUND in this crate can encode it";

/// What this provider does with one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Implemented against the frozen facade and covered by tests.
    Supported,
    /// Refused outright. The reason is reported verbatim to the caller.
    Unsupported {
        /// Why the semantics are not offered, quoting the design decision.
        reason: &'static str,
    },
    /// Authorised but unreachable today. Named owner, verbatim reason.
    Deferred {
        /// Node that owns closing the gap.
        owner: &'static str,
        /// Why it is unreachable.
        reason: &'static str,
    },
}

impl Support {
    /// The contract error kind this verdict produces.
    pub fn error_kind(self) -> Option<ErrorKind> {
        match self {
            Self::Supported => None,
            Self::Unsupported { .. } => Some(ErrorKind::UnsupportedCapability),
            Self::Deferred { .. } => Some(ErrorKind::NotImplemented),
        }
    }

    /// Turn a non-supported verdict into the failure the caller receives.
    ///
    /// Returns `Ok(())` for [`Support::Supported`] so a call site can gate on this
    /// without a second match.
    pub fn admit(self, operation: &str) -> Result<()> {
        match self {
            Self::Supported => Ok(()),
            other => other.refuse(operation),
        }
    }

    /// Refuse an operation, in whatever result type the call site returns.
    ///
    /// [`Support::Supported`] has no refusal, and asking for one means the
    /// capability table and the dispatch have drifted apart. That reports itself
    /// as a protocol mismatch rather than fabricating a response, because a
    /// fabricated one is exactly the silent fallback this module exists to
    /// prevent.
    pub fn refuse<T>(self, operation: &str) -> Result<T> {
        match self {
            Self::Supported => Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                operation,
                "the capability table marks this operation supported, so it has no                  refusal; the table and the dispatch have drifted apart",
            )),
            Self::Unsupported { reason } => Err(UmbraError::new(
                ErrorKind::UnsupportedCapability,
                operation,
                reason,
            )),
            Self::Deferred { owner, reason } => Err(UmbraError::new(
                ErrorKind::NotImplemented,
                operation,
                format!("{reason}; {owner} owns closing this"),
            )),
        }
    }
}

/// One row of the published capability matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityRow {
    /// The [`StorageOperation`] variant, or the sub-case within one.
    pub operation: &'static str,
    /// The `docs/design/syscall-matrix.md` entries this row serves.
    pub syscalls: &'static [&'static str],
    /// The verdict.
    pub support: Support,
}

/// A syscall-matrix entry with no representation on the `Storage` contract.
///
/// These exist so the published matrix can state a verdict for every row of the
/// pinned design document, including the ones no provider operation maps to.
/// Silence would read as an oversight; a recorded refusal reads as a decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceNote {
    /// The syscall-matrix entry.
    pub syscalls: &'static [&'static str],
    /// The verdict.
    pub support: Support,
}

const OVERLAY_OWNED: &str =
    "logical symlinks are overlay-owned; this provider resolves no server-side \
     symlink and creates none, so it cannot escape the anchor";

/// Every `Storage` contract operation, with its verdict.
///
/// [`storage_support`] is generated from this table, and
/// `every_storage_operation_has_a_row` proves the table is exhaustive.
pub const CONTRACT_SURFACE: &[CapabilityRow] = &[
    CapabilityRow {
        operation: "Lookup",
        syscalls: &["open, openat (P->H resolution)", "access, faccessat"],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "Stat",
        syscalls: &[
            "stat, fstatat, lstat",
            "fstat",
            "getattrlist (bounded attribute subset)",
        ],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "List",
        syscalls: &[
            "opendir, readdir, closedir",
            "getdents64",
            "getdirentries, getdirentries64",
            "getattrlistbulk (bounded attribute subset)",
        ],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "ReadAt",
        syscalls: &["read", "pread"],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "WriteAt",
        syscalls: &["write", "pwrite"],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "Create { kind: File }",
        syscalls: &[
            "open, openat with O_CREAT",
            "open, openat with O_CREAT|O_EXCL",
        ],
        support: Support::Supported,
    },
    CapabilityRow {
        operation: "Create { kind: Directory }",
        syscalls: &["mkdir, mkdirat"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "Create { kind: LogicalSymlink }",
        syscalls: &["symlink, symlinkat"],
        support: Support::Unsupported {
            reason: OVERLAY_OWNED,
        },
    },
    CapabilityRow {
        operation: "CreateParents",
        syscalls: &["mkdir, mkdirat (recursive)"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "CopyUp",
        syscalls: &["immutable-base materialisation (no direct syscall)"],
        support: Support::Deferred {
            owner: M1_INTEGRATE,
            reason: "this provider issues no ApprovedBaseObject handle, so a \
                     presented one names nothing it can prove",
        },
    },
    CapabilityRow {
        operation: "Unlink",
        syscalls: &["unlink, unlinkat"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "RemoveDirectory",
        syscalls: &["rmdir", "unlinkat with AT_REMOVEDIR"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "Rename",
        syscalls: &["rename, renameat", "renameat2 with flags = 0"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "Link",
        syscalls: &["link, linkat"],
        support: Support::Unsupported {
            reason: "the syscall matrix sets the M2 hard-link capability false; no \
                     copy-as-link substitution is offered",
        },
    },
    CapabilityRow {
        operation: "ReadLink",
        syscalls: &["readlink, readlinkat"],
        support: Support::Unsupported {
            reason: OVERLAY_OWNED,
        },
    },
    CapabilityRow {
        operation: "SetMetadata",
        syscalls: &[
            "chmod, fchmodat",
            "fchmod",
            "chown, fchownat",
            "fchown",
            "utimensat, futimens",
        ],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "Truncate",
        syscalls: &["truncate, ftruncate", "open, openat with O_TRUNC"],
        support: Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        },
    },
    CapabilityRow {
        operation: "GetXattr",
        syscalls: &["getxattr, listxattr"],
        support: Support::Unsupported {
            reason: "the syscall matrix sets the initial xattr capability false; an \
                     empty successful list would be a false answer",
        },
    },
    CapabilityRow {
        operation: "SetXattr",
        syscalls: &["setxattr"],
        support: Support::Unsupported {
            reason: "the syscall matrix sets the initial xattr capability false; a \
                     discarded write would be a false success",
        },
    },
    CapabilityRow {
        operation: "RemoveXattr",
        syscalls: &["removexattr"],
        support: Support::Unsupported {
            reason: "the syscall matrix sets the initial xattr capability false",
        },
    },
    CapabilityRow {
        operation: "GetWhiteout",
        syscalls: &["overlay whiteout policy (no direct syscall)"],
        support: Support::Unsupported {
            reason: "whiteouts stay overlay policy; storage holds no whiteout state",
        },
    },
    CapabilityRow {
        operation: "SetWhiteout",
        syscalls: &["overlay whiteout policy (no direct syscall)"],
        support: Support::Unsupported {
            reason: "whiteouts stay overlay policy; storage holds no whiteout state",
        },
    },
    CapabilityRow {
        operation: "AtomicSwap",
        syscalls: &[
            "exchangedata",
            "renameat2 with RENAME_EXCHANGE",
            "renamex_np, renameatx_np",
        ],
        support: Support::Unsupported {
            reason: "no qualified atomic data exchange exists in NFSv4.0; several \
                     renames are not a substitute",
        },
    },
];

/// Syscall-matrix entries with no `Storage` contract representation at all.
pub const OUT_OF_SURFACE: &[SurfaceNote] = &[
    SurfaceNote {
        syscalls: &["kqueue, kevent with EVFILT_VNODE", "FSEvents"],
        support: Support::Unsupported {
            reason: "remote file-change notification is out of scope by the syscall \
                     matrix's notifications decision: NFS reads yield no complete \
                     change-event stream, and no local mirror is manufactured",
        },
    },
    SurfaceNote {
        syscalls: &["mmap, munmap, msync (file-backed)"],
        support: Support::Unsupported {
            reason: "file-backed mapping is behind the M2 mmap design gate; syscall \
                     interception alone does not observe CPU loads and stores",
        },
    },
    SurfaceNote {
        syscalls: &["acl_get_file, acl_set_file and the ACL attribute family"],
        support: Support::Unsupported {
            reason: "no ACL model is qualified; POSIX mode bits are the only \
                     permission surface this provider reports",
        },
    },
    SurfaceNote {
        syscalls: &["flock"],
        support: Support::Unsupported {
            reason: "the syscall matrix holds flock at the M2 locking gate; a private \
                     mutex must never be reported as a cross-client lock",
        },
    },
    SurfaceNote {
        syscalls: &["getdirentriesattr"],
        support: Support::Unsupported {
            reason: "legacy bulk directory API; no fabricated records",
        },
    },
    SurfaceNote {
        syscalls: &["fclonefileat"],
        support: Support::Unsupported {
            reason: "no reflink or clone guarantee exists on NFSv4.0",
        },
    },
    SurfaceNote {
        syscalls: &["fsync, fdatasync", "flush receipts"],
        support: Support::Deferred {
            owner: "authority_recovery",
            reason: "a durability receipt asserts a persistence boundary this node \
                     does not own; WRITE/COMMIT verifier accounting is wired here \
                     but the receipt is not",
        },
    },
];

/// The verdict for one `Storage` contract operation.
///
/// Matching the operation rather than looking the name up keeps the compiler
/// responsible for exhaustiveness: a new contract variant fails to build here
/// instead of silently falling through to a default.
pub fn storage_support(operation: &StorageOperation) -> Support {
    let name = match operation {
        StorageOperation::Lookup { .. } => "Lookup",
        StorageOperation::Stat { .. } => "Stat",
        StorageOperation::List { .. } => "List",
        StorageOperation::ReadAt { .. } => "ReadAt",
        StorageOperation::WriteAt { .. } => "WriteAt",
        StorageOperation::Create { options, .. } => match options.kind {
            CreateKind::File => "Create { kind: File }",
            CreateKind::Directory => "Create { kind: Directory }",
            CreateKind::LogicalSymlink { .. } => "Create { kind: LogicalSymlink }",
        },
        StorageOperation::CreateParents { .. } => "CreateParents",
        StorageOperation::CopyUp { .. } => "CopyUp",
        StorageOperation::Unlink { .. } => "Unlink",
        StorageOperation::RemoveDirectory { .. } => "RemoveDirectory",
        StorageOperation::Rename { .. } => "Rename",
        StorageOperation::Link { .. } => "Link",
        StorageOperation::ReadLink { .. } => "ReadLink",
        StorageOperation::SetMetadata { .. } => "SetMetadata",
        StorageOperation::Truncate { .. } => "Truncate",
        StorageOperation::GetXattr { .. } => "GetXattr",
        StorageOperation::SetXattr { .. } => "SetXattr",
        StorageOperation::RemoveXattr { .. } => "RemoveXattr",
        StorageOperation::GetWhiteout { .. } => "GetWhiteout",
        StorageOperation::SetWhiteout { .. } => "SetWhiteout",
        StorageOperation::AtomicSwap { .. } => "AtomicSwap",
    };
    row(name).support
}

/// The published name of one `Storage` contract operation, for diagnostics.
pub fn operation_name(operation: &StorageOperation) -> &'static str {
    match operation {
        StorageOperation::Lookup { .. } => "execute.lookup",
        StorageOperation::Stat { .. } => "execute.stat",
        StorageOperation::List { .. } => "execute.list",
        StorageOperation::ReadAt { .. } => "execute.read_at",
        StorageOperation::WriteAt { .. } => "execute.write_at",
        StorageOperation::Create { .. } => "execute.create",
        StorageOperation::CreateParents { .. } => "execute.create_parents",
        StorageOperation::CopyUp { .. } => "execute.copy_up",
        StorageOperation::Unlink { .. } => "execute.unlink",
        StorageOperation::RemoveDirectory { .. } => "execute.remove_directory",
        StorageOperation::Rename { .. } => "execute.rename",
        StorageOperation::Link { .. } => "execute.link",
        StorageOperation::ReadLink { .. } => "execute.read_link",
        StorageOperation::SetMetadata { .. } => "execute.set_metadata",
        StorageOperation::Truncate { .. } => "execute.truncate",
        StorageOperation::GetXattr { .. } => "execute.get_xattr",
        StorageOperation::SetXattr { .. } => "execute.set_xattr",
        StorageOperation::RemoveXattr { .. } => "execute.remove_xattr",
        StorageOperation::GetWhiteout { .. } => "execute.get_whiteout",
        StorageOperation::SetWhiteout { .. } => "execute.set_whiteout",
        StorageOperation::AtomicSwap { .. } => "execute.atomic_swap",
    }
}

fn row(operation: &'static str) -> &'static CapabilityRow {
    CONTRACT_SURFACE
        .iter()
        .find(|row| row.operation == operation)
        .unwrap_or_else(|| {
            unreachable!("every contract operation has a capability row: {operation}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::{
        ApprovedBaseObject, BytePath, CreateOptions, MetadataUpdate, ObjectId, RenameMode,
        StorageAnchor, StorageHandle, StoragePath,
    };
    use uuid::Uuid;

    fn path() -> StoragePath {
        StoragePath::new(StorageAnchor::Root, b"a").expect("path")
    }

    /// One of every contract operation, so the tables below are exhaustive by
    /// construction rather than by inspection.
    fn every_operation() -> Vec<StorageOperation> {
        let create = |kind| StorageOperation::Create {
            path: path(),
            options: CreateOptions { kind, mode: 0o600 },
        };
        vec![
            StorageOperation::Lookup { path: path() },
            StorageOperation::Stat { path: path() },
            StorageOperation::List {
                path: path(),
                cursor: None,
                limit: 8,
            },
            StorageOperation::ReadAt {
                path: path(),
                offset: 0,
                len: 8,
            },
            StorageOperation::WriteAt {
                path: path(),
                offset: 0,
                bytes: vec![1],
            },
            create(CreateKind::File),
            create(CreateKind::Directory),
            create(CreateKind::LogicalSymlink {
                target: BytePath::new(b"t").expect("target"),
            }),
            StorageOperation::CreateParents {
                path: path(),
                mode: 0o700,
            },
            StorageOperation::CopyUp {
                source: ApprovedBaseObject {
                    object_id: ObjectId(Uuid::nil()),
                    handle: StorageHandle(vec![1]),
                },
                destination: path(),
            },
            StorageOperation::Unlink { path: path() },
            StorageOperation::RemoveDirectory { path: path() },
            StorageOperation::Rename {
                source: path(),
                destination: path(),
                mode: RenameMode::Replace,
            },
            StorageOperation::Link {
                source: path(),
                destination: path(),
            },
            StorageOperation::ReadLink { path: path() },
            StorageOperation::SetMetadata {
                path: path(),
                update: MetadataUpdate {
                    mode: Some(0o600),
                    uid: None,
                    gid: None,
                    accessed_nanos: None,
                    modified_nanos: None,
                },
            },
            StorageOperation::Truncate {
                path: path(),
                len: 0,
            },
            StorageOperation::GetXattr {
                path: path(),
                name: b"user.x".to_vec(),
                max_bytes: 16,
            },
            StorageOperation::SetXattr {
                path: path(),
                name: b"user.x".to_vec(),
                value: vec![1],
            },
            StorageOperation::RemoveXattr {
                path: path(),
                name: b"user.x".to_vec(),
            },
            StorageOperation::GetWhiteout { path: path() },
            StorageOperation::SetWhiteout {
                path: path(),
                present: true,
            },
            StorageOperation::AtomicSwap {
                left: path(),
                right: path(),
            },
        ]
    }

    #[test]
    fn every_contract_operation_has_exactly_one_row() {
        let operations = every_operation();
        // `storage_support` panics through `row` when a variant has no entry, so
        // reaching the end at all proves the table covers the contract.
        for operation in &operations {
            let _ = storage_support(operation);
            assert!(operation_name(operation).starts_with("execute."));
        }
        // `Create` contributes three rows and one name, so the row count exceeds
        // the variant count by exactly the two extra create kinds.
        assert_eq!(CONTRACT_SURFACE.len(), operations.len());
        let mut names: Vec<_> = CONTRACT_SURFACE.iter().map(|row| row.operation).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "capability rows must be distinct");
    }

    #[test]
    fn every_row_names_the_syscall_matrix_entries_it_serves() {
        for row in CONTRACT_SURFACE {
            assert!(
                !row.syscalls.is_empty(),
                "{} names no syscall-matrix entry",
                row.operation
            );
        }
        for note in OUT_OF_SURFACE {
            assert!(!note.syscalls.is_empty());
            assert_ne!(
                note.support,
                Support::Supported,
                "an out-of-surface row cannot be supported: it has no surface"
            );
        }
    }

    #[test]
    fn a_refusal_carries_the_kind_its_verdict_promises() {
        let unsupported = Support::Unsupported { reason: "no" };
        let deferred = Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        };
        assert_eq!(
            unsupported.error_kind(),
            Some(ErrorKind::UnsupportedCapability)
        );
        assert_eq!(deferred.error_kind(), Some(ErrorKind::NotImplemented));
        assert_eq!(Support::Supported.error_kind(), None);

        let error = unsupported.refuse::<()>("op").unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
        let error = deferred.refuse::<()>("op").unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotImplemented);
        // The deferral names who closes it, so a reader is not left guessing.
        assert!(error.context.contains(CONTRACTS_REVISION));
        assert!(Support::Supported.admit("op").is_ok());
    }

    #[test]
    fn asking_a_supported_verdict_for_a_refusal_reports_the_drift() {
        // Reaching this means the capability table and a dispatch arm disagree.
        // Fabricating a response instead would be the silent fallback the whole
        // module exists to prevent.
        let error = Support::Supported.refuse::<()>("op").unwrap_err();
        assert_eq!(error.kind, ErrorKind::ProtocolMismatch);
    }

    #[test]
    fn notifications_are_recorded_as_out_of_scope_not_merely_absent() {
        let row = OUT_OF_SURFACE
            .iter()
            .find(|note| note.syscalls.iter().any(|entry| entry.contains("FSEvents")))
            .expect("the notifications decision has a recorded row");
        assert!(matches!(row.support, Support::Unsupported { .. }));
        assert!(row
            .syscalls
            .iter()
            .any(|entry| entry.contains("EVFILT_VNODE")));
    }
}
