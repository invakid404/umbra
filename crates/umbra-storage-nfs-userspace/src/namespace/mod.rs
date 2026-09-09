//! Namespace mutation: the typed seam for REMOVE, RENAME, CREATE and SETATTR.
//!
//! # Why this seam exists
//!
//! The pinned `docs/design/syscall-matrix.md` marks `unlink`, `rmdir`, `rename`,
//! `mkdir`, `chmod`, `chown`, `utimensat` and `truncate` **emulated** — all of
//! them are in scope. When `operations` was built, the frozen
//! [`Nfs4Op`](crate::transport::Nfs4Op) carried no argument variant for any of
//! them, so the shape was typed here and the dispatch injected rather than
//! faked, exactly as
//! [`downgrade_with`](crate::state::open_owner::downgrade_with) still does for
//! `OPEN_DOWNGRADE`.
//!
//! The authorised contracts hotfix at `m1_integrate` closed that gap: `Nfs4Op`
//! now carries `Remove`, `Rename`, `Create`, `SetAttr` and the `SaveFh` a
//! RENAME needs to name its source directory. The seam stays, because it is
//! still the thing that proves a dispatcher acted on the object it was given —
//! see below — and because a caller with no dispatcher bound must still receive
//! [`umbra_core::ErrorKind::NotImplemented`] rather than a fabricated success.
//! [`dispatch::TransportDispatcher`] is the bound implementation.
//!
//! # The rule this module enforces regardless of who dispatches
//!
//! A rename moves a name, not an object. [`apply`] proves that: it compares the
//! identity the dispatcher reports afterwards against the identity pinned before,
//! and refuses with [`AuthorityError::IdentityUnproven`] when they differ. A
//! dispatcher that "renamed" by creating a copy therefore fails here rather than
//! quietly breaking every open handle.

pub mod dispatch;

use umbra_core::{MetadataUpdate, RenameMode, Result};

use crate::capability::{Support, CONTRACTS_REVISION, NO_WIRE_OPERATION};
use crate::error::{AuthorityError, FacadeError, FacadeResult};
use crate::handle::{FileHandle, ObjectIdentity};
use crate::transport::{ComponentName, RawTransport};

/// Which kind of object a REMOVE is expected to unlink.
///
/// NFSv4 has one `REMOVE`, but the contract has two operations and a caller that
/// asks to remove a directory with `unlink` is making an error the provider must
/// report rather than absorb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveKind {
    /// A regular file or logical-symlink name.
    File,
    /// A directory, which must be empty.
    Directory,
}

/// One namespace mutation, fully addressed.
///
/// Every variant names its target as a parent filehandle plus a byte component,
/// which is how NFSv4 addresses a namespace change and why nothing here can be
/// asked to act on an unanchored path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceMutation {
    /// `REMOVE`: unlink one name.
    Remove {
        /// Directory holding the name.
        parent: FileHandle,
        /// The name to remove.
        name: ComponentName,
        /// Identity the name resolved to before the removal.
        target: ObjectIdentity,
        /// Which kind of object the caller expects to remove.
        kind: RemoveKind,
    },
    /// `RENAME`: move a name, keeping the object.
    Rename {
        /// Directory holding the source name.
        source_parent: FileHandle,
        /// The source name.
        source_name: ComponentName,
        /// Directory that will hold the destination name.
        target_parent: FileHandle,
        /// The destination name.
        target_name: ComponentName,
        /// Identity the source name resolved to before the rename.
        source: ObjectIdentity,
        /// Whether an existing destination may be replaced.
        mode: RenameMode,
    },
    /// `CREATE` with `NF4DIR`: make a directory.
    CreateDirectory {
        /// Directory that will hold the new name.
        parent: FileHandle,
        /// The new name.
        name: ComponentName,
        /// Mode bits for the new directory.
        mode: u32,
    },
    /// `SETATTR`: change ownership, mode or times.
    SetAttributes {
        /// The object to change.
        object: FileHandle,
        /// Identity that object was pinned at.
        target: ObjectIdentity,
        /// The requested change.
        update: MetadataUpdate,
    },
    /// `SETATTR` of `FATTR4_SIZE`: truncate.
    ///
    /// Truncation is a mutation, and the syscall matrix says so explicitly for
    /// `open`'s `O_TRUNC` as well as for `truncate` and `ftruncate`.
    Truncate {
        /// The object to resize.
        object: FileHandle,
        /// Identity that object was pinned at.
        target: ObjectIdentity,
        /// The new length in bytes.
        len: u64,
    },
}

impl NamespaceMutation {
    /// The operation name used in diagnostics and in the deferral error.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Remove {
                kind: RemoveKind::File,
                ..
            } => "namespace.remove",
            Self::Remove {
                kind: RemoveKind::Directory,
                ..
            } => "namespace.remove_directory",
            Self::Rename { .. } => "namespace.rename",
            Self::CreateDirectory { .. } => "namespace.create_directory",
            Self::SetAttributes { .. } => "namespace.set_attributes",
            Self::Truncate { .. } => "namespace.truncate",
        }
    }
}

/// What a dispatcher reports about the object a mutation acted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceEffect {
    /// Identity the server reports for the object after the mutation.
    pub identity: ObjectIdentity,
    /// Filehandle naming that object after the mutation.
    pub handle: FileHandle,
}

/// The settled result of one namespace mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceOutcome {
    /// The name is gone. Any object that still has an open state survives to CLOSE.
    Removed,
    /// The name moved. The effect must describe the same object it started as.
    Renamed(NamespaceEffect),
    /// A new directory exists.
    Created(NamespaceEffect),
    /// Attributes changed on the object the caller pinned.
    AttributesSet(NamespaceEffect),
}

/// Puts one namespace mutation on the wire.
///
/// An implementation must address the mutation exactly as given — a dispatcher
/// that substitutes a different object, or that emulates a rename with a copy,
/// is rejected by [`apply`].
///
/// The transport arrives as a parameter rather than being captured at
/// construction. A dispatcher that owned its own transport would be speaking on
/// a different connection from the request that called it — a different client
/// incarnation, different open owners, and no relationship between the epoch
/// that authorised the mutation and the one that carried it. Taking it per call
/// means the mutation provably travels on the session's own transport.
pub trait NamespaceDispatcher {
    /// Dispatch one mutation and report what it settled.
    fn dispatch(
        &mut self,
        transport: &mut dyn RawTransport,
        mutation: &NamespaceMutation,
    ) -> FacadeResult<NamespaceOutcome>;
}

/// Apply one namespace mutation through an optionally bound dispatcher.
///
/// With no dispatcher this reports the contracts gap by name. With one, the
/// outcome is checked against the mutation before it is believed: the kind must
/// match, and a rename must have preserved object identity.
///
/// The dispatcher's own lifetime is named separately from the borrow of it so a
/// caller can hand over a short reborrow of a longer-lived dispatcher. Eliding
/// both to one lifetime would make `&mut (dyn NamespaceDispatcher + 'a)`
/// invariant in `'a` at the call site and pin the caller's whole context borrow
/// to the request.
pub fn apply<'d>(
    dispatcher: Option<&mut (dyn NamespaceDispatcher + 'd)>,
    transport: &mut dyn RawTransport,
    mutation: &NamespaceMutation,
) -> Result<NamespaceOutcome> {
    let operation = mutation.operation();
    let Some(dispatcher) = dispatcher else {
        return Support::Deferred {
            owner: CONTRACTS_REVISION,
            reason: NO_WIRE_OPERATION,
        }
        .refuse(operation);
    };
    let outcome = dispatcher
        .dispatch(transport, mutation)
        .map_err(|error| error.to_umbra(operation))?;
    verify(mutation, &outcome).map_err(|error| error.to_umbra(operation))?;
    Ok(outcome)
}

/// Prove a dispatcher's answer actually describes the mutation it was given.
fn verify(mutation: &NamespaceMutation, outcome: &NamespaceOutcome) -> FacadeResult<()> {
    let mismatch = |expected: &str| {
        Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
            format!("the dispatcher answered {outcome:?} for a {expected}"),
        )))
    };
    match (mutation, outcome) {
        (NamespaceMutation::Remove { .. }, NamespaceOutcome::Removed) => Ok(()),
        (NamespaceMutation::Remove { .. }, _) => mismatch("REMOVE"),
        (NamespaceMutation::Rename { source, .. }, NamespaceOutcome::Renamed(effect)) => {
            if effect.identity == *source {
                Ok(())
            } else {
                // A rename that changed the object is not a rename. Every open
                // handle on the source would still point at the original, so
                // accepting this would publish two irreconcilable views.
                Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                    format!(
                        "RENAME reported fileid {} but the source was fileid {}; \
                         a rename moves a name, never the object",
                        effect.identity.fileid, source.fileid
                    ),
                )))
            }
        }
        (NamespaceMutation::Rename { .. }, _) => mismatch("RENAME"),
        (NamespaceMutation::CreateDirectory { .. }, NamespaceOutcome::Created(_)) => Ok(()),
        (NamespaceMutation::CreateDirectory { .. }, _) => mismatch("CREATE"),
        (
            NamespaceMutation::SetAttributes { target, .. }
            | NamespaceMutation::Truncate { target, .. },
            NamespaceOutcome::AttributesSet(effect),
        ) => {
            if effect.identity == *target {
                Ok(())
            } else {
                Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                    format!(
                        "SETATTR reported fileid {} but the pinned object was fileid {}",
                        effect.identity.fileid, target.fileid
                    ),
                )))
            }
        }
        (NamespaceMutation::SetAttributes { .. } | NamespaceMutation::Truncate { .. }, _) => {
            mismatch("SETATTR")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::ErrorKind;

    use crate::fake::FakeTransport;
    use crate::transport::Fsid;

    fn identity(fileid: u64) -> ObjectIdentity {
        ObjectIdentity {
            fsid: Fsid { major: 1, minor: 2 },
            fileid,
        }
    }

    fn handle(byte: u8) -> FileHandle {
        FileHandle::from_wire(vec![byte; 8]).expect("a valid filehandle length")
    }

    fn name(bytes: &[u8]) -> ComponentName {
        ComponentName::new(bytes.to_vec()).expect("component")
    }

    fn rename_of(source: ObjectIdentity) -> NamespaceMutation {
        NamespaceMutation::Rename {
            source_parent: handle(1),
            source_name: name(b"before"),
            target_parent: handle(1),
            target_name: name(b"after"),
            source,
            mode: RenameMode::Replace,
        }
    }

    /// A dispatcher that answers with a scripted outcome and records what it saw.
    ///
    /// It stands in for the absent wire operation. It proves the seam's plumbing
    /// and its checks; it is not evidence that any server performed a rename.
    struct Scripted {
        answer: NamespaceOutcome,
        seen: Vec<NamespaceMutation>,
    }

    impl NamespaceDispatcher for Scripted {
        fn dispatch(
            &mut self,
            _: &mut dyn RawTransport,
            mutation: &NamespaceMutation,
        ) -> FacadeResult<NamespaceOutcome> {
            self.seen.push(mutation.clone());
            Ok(self.answer.clone())
        }
    }

    #[test]
    fn an_unbound_dispatcher_names_the_contracts_gap_rather_than_a_capability() {
        for mutation in [
            NamespaceMutation::Remove {
                parent: handle(1),
                name: name(b"gone"),
                target: identity(9),
                kind: RemoveKind::File,
            },
            NamespaceMutation::Remove {
                parent: handle(1),
                name: name(b"gone"),
                target: identity(9),
                kind: RemoveKind::Directory,
            },
            rename_of(identity(9)),
            NamespaceMutation::CreateDirectory {
                parent: handle(1),
                name: name(b"new"),
                mode: 0o700,
            },
            NamespaceMutation::Truncate {
                object: handle(2),
                target: identity(9),
                len: 0,
            },
        ] {
            let error = apply(None, &mut FakeTransport::new(), &mutation).unwrap_err();
            // Not `UnsupportedCapability`: the syscall matrix marks all of these
            // emulated, so calling them unsupported would understate the design
            // exactly as calling them supported would overstate it.
            assert_eq!(error.kind, ErrorKind::NotImplemented, "{mutation:?}");
            assert!(error.context.contains("Nfs4Op"));
            assert!(error.context.contains(CONTRACTS_REVISION));
            assert_eq!(error.operation, mutation.operation());
        }
    }

    #[test]
    fn a_rename_must_report_the_object_it_started_with() {
        let moved = identity(9);
        let mut good = Scripted {
            answer: NamespaceOutcome::Renamed(NamespaceEffect {
                identity: moved,
                handle: handle(3),
            }),
            seen: Vec::new(),
        };
        let outcome = apply(
            Some(&mut good),
            &mut FakeTransport::new(),
            &rename_of(moved),
        )
        .expect("identity preserved");
        assert!(matches!(outcome, NamespaceOutcome::Renamed(_)));
        assert_eq!(good.seen.len(), 1);

        // A dispatcher that "renamed" by producing a different object broke every
        // open handle on the source. Refusing here is the only safe answer.
        let mut copied = Scripted {
            answer: NamespaceOutcome::Renamed(NamespaceEffect {
                identity: identity(10),
                handle: handle(4),
            }),
            seen: Vec::new(),
        };
        let error = apply(
            Some(&mut copied),
            &mut FakeTransport::new(),
            &rename_of(moved),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::LeaseLost);
        assert!(error
            .context
            .contains("a rename moves a name, never the object"));
    }

    #[test]
    fn a_setattr_must_report_the_object_the_caller_pinned() {
        let pinned = identity(9);
        let mut wrong = Scripted {
            answer: NamespaceOutcome::AttributesSet(NamespaceEffect {
                identity: identity(11),
                handle: handle(5),
            }),
            seen: Vec::new(),
        };
        let error = apply(
            Some(&mut wrong),
            &mut FakeTransport::new(),
            &NamespaceMutation::Truncate {
                object: handle(2),
                target: pinned,
                len: 4,
            },
        )
        .unwrap_err();
        assert!(error.context.contains("SETATTR reported fileid 11"));
    }

    #[test]
    fn an_outcome_of_the_wrong_kind_is_refused_instead_of_absorbed() {
        let mut confused = Scripted {
            answer: NamespaceOutcome::Removed,
            seen: Vec::new(),
        };
        let error = apply(
            Some(&mut confused),
            &mut FakeTransport::new(),
            &rename_of(identity(9)),
        )
        .unwrap_err();
        assert!(error.context.contains("for a RENAME"));

        let mut also_confused = Scripted {
            answer: NamespaceOutcome::Created(NamespaceEffect {
                identity: identity(9),
                handle: handle(3),
            }),
            seen: Vec::new(),
        };
        let error = apply(
            Some(&mut also_confused),
            &mut FakeTransport::new(),
            &NamespaceMutation::Remove {
                parent: handle(1),
                name: name(b"gone"),
                target: identity(9),
                kind: RemoveKind::File,
            },
        )
        .unwrap_err();
        assert!(error.context.contains("for a REMOVE"));
    }

    #[test]
    fn a_dispatcher_failure_is_reported_verbatim() {
        struct Failing;
        impl NamespaceDispatcher for Failing {
            fn dispatch(
                &mut self,
                _: &mut dyn RawTransport,
                _: &NamespaceMutation,
            ) -> FacadeResult<NamespaceOutcome> {
                Err(FacadeError::protocol(
                    crate::error::Nfs4Status::NOTEMPTY,
                    crate::transport::OpCode::Remove,
                    1,
                ))
            }
        }
        let mut failing = Failing;
        let error = apply(
            Some(&mut failing),
            &mut FakeTransport::new(),
            &NamespaceMutation::Remove {
                parent: handle(1),
                name: name(b"dir"),
                target: identity(9),
                kind: RemoveKind::Directory,
            },
        )
        .unwrap_err();
        // `NFS4ERR_NOTEMPTY` has no dedicated contract kind, so it stays an I/O
        // failure carrying its verbatim number rather than being promoted.
        assert_eq!(error.kind, ErrorKind::Io);
        assert!(error.context.contains("66"));
    }
}
