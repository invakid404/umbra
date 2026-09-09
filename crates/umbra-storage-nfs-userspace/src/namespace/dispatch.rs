//! [`TransportDispatcher`]: the bound [`NamespaceDispatcher`], `m1_integrate`'s seam.
//!
//! Every mutation here is one or two COMPOUNDs over the session's own transport,
//! using the four argument variants the authorised contracts hotfix added to the
//! frozen [`Nfs4Op`]. Nothing in this module holds protocol state: seqids, open
//! owners and stateids belong to `raw_state`, and the write stateid a truncation
//! needs arrives from the caller that opened the file.
//!
//! # Why some mutations take two round trips
//!
//! NFSv4.0 addresses REMOVE by *name within the current filehandle*, so a
//! COMPOUND that looks the name up first has moved the current filehandle to the
//! child and can no longer remove it. Restoring it needs `RESTOREFH`, which the
//! hotfix did not add because nothing else needs it. So a REMOVE that must first
//! prove the target's type and identity does that in its own COMPOUND. The cost
//! is one extra round trip; the alternative is unlinking a name without knowing
//! what it named.

use umbra_core::{MetadataUpdate, RenameMode};

use crate::error::{AuthorityError, FacadeError, FacadeResult, Nfs4Status};
use crate::handle::{FileHandle, ObjectIdentity, Stateid};
use crate::transport::{
    AttrMask, AttrValues, ComponentName, Compound, CreateType, Deadline, Nfs4Op, Nfs4Time,
    Nfs4Type, OpCode, OpReply, RawTransport,
};

use super::{
    NamespaceDispatcher, NamespaceEffect, NamespaceMutation, NamespaceOutcome, RemoveKind,
};

/// Puts namespace mutations on the wire over whichever [`RawTransport`] the
/// request is running on.
///
/// Holds no transport of its own by design — see [`NamespaceDispatcher`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportDispatcher {
    write_stateid: Option<Stateid>,
}

impl TransportDispatcher {
    /// A dispatcher with no write stateid.
    ///
    /// Every mutation except truncation works without one. A truncation is
    /// refused rather than sent with the anonymous stateid, because RFC 7530
    /// §16.32 requires an open stateid with WRITE access for `FATTR4_SIZE` and a
    /// server is entitled to refuse it — reporting a truncation that the server
    /// declined would be the worst possible answer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Authorise truncation with the stateid of an open that holds WRITE access.
    pub fn with_write_stateid(stateid: Stateid) -> Self {
        Self {
            write_stateid: Some(stateid),
        }
    }

    /// Resolve one name and report its identity, filehandle and type.
    fn probe(
        transport: &mut dyn RawTransport,
        parent: &FileHandle,
        name: &ComponentName,
        deadline: Deadline,
    ) -> FacadeResult<(FileHandle, ObjectIdentity, Option<Nfs4Type>)> {
        // `lookup` is a shape helper on the frozen trait and already reports a
        // `FacadeError`, unlike `submit`, which reports a `TransportError`.
        let (handle, attributes) = transport.lookup(
            parent,
            name,
            AttrMask::IDENTITY.union(AttrMask::TYPE),
            deadline,
        )?;
        let identity = identity_of(&attributes)?;
        Ok((handle, identity, attributes.file_type))
    }

    /// Report the identity and filehandle a name now resolves to.
    fn effect(
        transport: &mut dyn RawTransport,
        parent: &FileHandle,
        name: &ComponentName,
        deadline: Deadline,
    ) -> FacadeResult<NamespaceEffect> {
        let (handle, identity, _) = Self::probe(transport, parent, name, deadline)?;
        Ok(NamespaceEffect { identity, handle })
    }
}

impl NamespaceDispatcher for TransportDispatcher {
    fn dispatch(
        &mut self,
        transport: &mut dyn RawTransport,
        mutation: &NamespaceMutation,
    ) -> FacadeResult<NamespaceOutcome> {
        let deadline = transport.limits().default_deadline;
        match mutation {
            NamespaceMutation::Remove {
                parent,
                name,
                target,
                kind,
            } => {
                // Prove what the name resolves to before unlinking it: the caller
                // pinned an identity and asked for a specific kind, and a name
                // that has been replaced since is a different object.
                let (_, identity, file_type) = Self::probe(transport, parent, name, deadline)?;
                if identity != *target {
                    return Err(unproven(format!(
                        "{} now names fileid {}, but the caller pinned fileid {}",
                        String::from_utf8_lossy(name.as_bytes()),
                        identity.fileid,
                        target.fileid
                    )));
                }
                match (kind, file_type) {
                    (RemoveKind::File, Some(Nfs4Type::Directory)) => {
                        return Err(status(Nfs4Status::ISDIR, OpCode::Remove));
                    }
                    (RemoveKind::Directory, Some(Nfs4Type::Directory)) => {}
                    (RemoveKind::Directory, _) => {
                        return Err(status(Nfs4Status::NOTDIR, OpCode::Remove));
                    }
                    (RemoveKind::File, _) => {}
                }

                let reply = transport
                    .submit(
                        Compound::new(
                            *b"remove",
                            vec![
                                Nfs4Op::PutFh(parent.clone()),
                                Nfs4Op::Remove { name: name.clone() },
                            ],
                        ),
                        deadline,
                    )
                    .map_err(FacadeError::Transport)?;
                match reply.expect(1)? {
                    OpReply::Remove(_) => Ok(NamespaceOutcome::Removed),
                    other => Err(shape(OpCode::Remove, other.opcode())),
                }
            }

            NamespaceMutation::Rename {
                source_parent,
                source_name,
                target_parent,
                target_name,
                source,
                mode,
            } => {
                if *mode == RenameMode::NoReplace
                    && Self::probe(transport, target_parent, target_name, deadline).is_ok()
                {
                    // NFSv4.0 RENAME always replaces; the contract's NoReplace is
                    // enforced here, before anything is sent.
                    return Err(status(Nfs4Status::EXIST, OpCode::Rename));
                }

                // SAVEFH names the source directory; PUTFH names the target.
                let reply = transport
                    .submit(
                        Compound::new(
                            *b"rename",
                            vec![
                                Nfs4Op::PutFh(source_parent.clone()),
                                Nfs4Op::SaveFh,
                                Nfs4Op::PutFh(target_parent.clone()),
                                Nfs4Op::Rename {
                                    old_name: source_name.clone(),
                                    new_name: target_name.clone(),
                                },
                            ],
                        ),
                        deadline,
                    )
                    .map_err(FacadeError::Transport)?;
                match reply.expect(3)? {
                    OpReply::Rename { .. } => {}
                    other => return Err(shape(OpCode::Rename, other.opcode())),
                }
                let effect = Self::effect(transport, target_parent, target_name, deadline)?;
                // `apply` re-checks this, but reporting it here names the failing
                // dispatch rather than an anonymous seam violation.
                if effect.identity != *source {
                    return Err(unproven(format!(
                        "RENAME left fileid {} at the destination; the source was fileid {}",
                        effect.identity.fileid, source.fileid
                    )));
                }
                Ok(NamespaceOutcome::Renamed(effect))
            }

            NamespaceMutation::CreateDirectory { parent, name, mode } => {
                let reply = transport
                    .submit(
                        Compound::new(
                            *b"mkdir",
                            vec![
                                Nfs4Op::PutFh(parent.clone()),
                                Nfs4Op::Create {
                                    object_type: CreateType::Directory,
                                    name: name.clone(),
                                    attributes: AttrValues {
                                        mode: Some(*mode),
                                        ..AttrValues::default()
                                    },
                                },
                                Nfs4Op::GetFh,
                                Nfs4Op::GetAttr(AttrMask::IDENTITY),
                            ],
                        ),
                        deadline,
                    )
                    .map_err(FacadeError::Transport)?;
                match reply.expect(1)? {
                    OpReply::Create { .. } => {}
                    other => return Err(shape(OpCode::Create, other.opcode())),
                }
                let handle = match reply.expect(2)? {
                    OpReply::GetFh(handle) => handle.clone(),
                    other => return Err(shape(OpCode::GetFh, other.opcode())),
                };
                let identity = match reply.expect(3)? {
                    OpReply::GetAttr(attributes) => identity_of(attributes)?,
                    other => return Err(shape(OpCode::GetAttr, other.opcode())),
                };
                Ok(NamespaceOutcome::Created(NamespaceEffect {
                    identity,
                    handle,
                }))
            }

            NamespaceMutation::SetAttributes {
                object,
                target,
                update,
            } => {
                let values = attr_values(update);
                if values.is_empty() {
                    // An update that names nothing would report a metadata change
                    // that never happened.
                    return Err(status(Nfs4Status::INVAL, OpCode::SetAttr));
                }
                self.set(
                    transport,
                    object,
                    target,
                    values,
                    Stateid::ANONYMOUS,
                    deadline,
                )
            }

            NamespaceMutation::Truncate {
                object,
                target,
                len,
            } => {
                let Some(stateid) = self.write_stateid else {
                    return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                        "truncation sets FATTR4_SIZE, which RFC 7530 §16.32 requires an open \
                         stateid with WRITE access for; this dispatcher was bound without one"
                            .to_owned(),
                    )));
                };
                let values = AttrValues {
                    size: Some(*len),
                    ..AttrValues::default()
                };
                self.set(transport, object, target, values, stateid, deadline)
            }
        }
    }
}

impl TransportDispatcher {
    /// `PUTFH; SETATTR; GETFH; GETATTR`: one attribute change, then proof of
    /// which object took it.
    fn set(
        &self,
        transport: &mut dyn RawTransport,
        object: &FileHandle,
        target: &ObjectIdentity,
        values: AttrValues,
        stateid: Stateid,
        deadline: Deadline,
    ) -> FacadeResult<NamespaceOutcome> {
        let requested = values.mask();
        let reply = transport
            .submit(
                Compound::new(
                    *b"setattr",
                    vec![
                        Nfs4Op::PutFh(object.clone()),
                        Nfs4Op::SetAttr {
                            stateid,
                            attributes: values,
                        },
                        Nfs4Op::GetFh,
                        Nfs4Op::GetAttr(AttrMask::IDENTITY),
                    ],
                ),
                deadline,
            )
            .map_err(FacadeError::Transport)?;
        match reply.expect(1)? {
            OpReply::SetAttr(attrset) => {
                // A server may set fewer bits than asked for and still answer
                // NFS4_OK. Reporting that as a completed metadata change would be
                // a false success, so the shortfall is the error.
                if !attrset.contains(requested) {
                    return Err(unproven(format!(
                        "SETATTR set {attrset:?} but {requested:?} was requested"
                    )));
                }
            }
            other => return Err(shape(OpCode::SetAttr, other.opcode())),
        }
        let handle = match reply.expect(2)? {
            OpReply::GetFh(handle) => handle.clone(),
            other => return Err(shape(OpCode::GetFh, other.opcode())),
        };
        let identity = match reply.expect(3)? {
            OpReply::GetAttr(attributes) => identity_of(attributes)?,
            other => return Err(shape(OpCode::GetAttr, other.opcode())),
        };
        if identity != *target {
            return Err(unproven(format!(
                "SETATTR landed on fileid {} but the caller pinned fileid {}",
                identity.fileid, target.fileid
            )));
        }
        Ok(NamespaceOutcome::AttributesSet(NamespaceEffect {
            identity,
            handle,
        }))
    }
}

/// Translate the contract's [`MetadataUpdate`] into settable NFSv4 attributes.
///
/// `uid` and `gid` become `FATTR4_OWNER` / `FATTR4_OWNER_GROUP`, which NFSv4
/// carries as name strings. Under AUTH_SYS with no idmapper the numeric form is
/// the identity string a server accepts, so the number is rendered verbatim
/// rather than mapped to a name this provider cannot verify.
fn attr_values(update: &MetadataUpdate) -> AttrValues {
    AttrValues {
        size: None,
        mode: update.mode,
        owner: update.uid.map(|uid| uid.to_string().into_bytes()),
        owner_group: update.gid.map(|gid| gid.to_string().into_bytes()),
        time_access: update.accessed_nanos.and_then(nfs_time),
        time_modify: update.modified_nanos.and_then(nfs_time),
    }
}

/// Split nanoseconds since the epoch into `nfstime4`.
///
/// Returns `None` when the value cannot be represented, so an out-of-range time
/// is dropped rather than wrapped into a plausible wrong one. The caller's
/// `attrset` check then reports the attribute as unset.
fn nfs_time(nanos: i128) -> Option<Nfs4Time> {
    let seconds = nanos.div_euclid(1_000_000_000);
    let remainder = nanos.rem_euclid(1_000_000_000);
    Some(Nfs4Time {
        seconds: i64::try_from(seconds).ok()?,
        nanoseconds: u32::try_from(remainder).ok()?,
    })
}

/// Object identity from a reply, or a refusal naming what was missing.
fn identity_of(attributes: &crate::transport::Attributes) -> FacadeResult<ObjectIdentity> {
    match (attributes.fsid, attributes.fileid) {
        (Some(fsid), Some(fileid)) => Ok(ObjectIdentity { fsid, fileid }),
        _ => Err(unproven(
            "the server answered without FATTR4_FSID and FATTR4_FILEID, so the object it \
             acted on cannot be identified"
                .to_owned(),
        )),
    }
}

fn unproven(detail: String) -> FacadeError {
    FacadeError::Authority(AuthorityError::IdentityUnproven(detail))
}

fn status(status: Nfs4Status, op: OpCode) -> FacadeError {
    FacadeError::protocol(status, op, 0)
}

fn shape(expected: OpCode, actual: OpCode) -> FacadeError {
    FacadeError::Transport(crate::error::TransportError::Malformed(format!(
        "expected a {expected:?} reply, got {actual:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeTransport;
    use crate::transport::TransportLimits;

    fn name(bytes: &[u8]) -> ComponentName {
        ComponentName::new(bytes.to_vec()).expect("valid component")
    }

    fn deadline(transport: &dyn RawTransport) -> Deadline {
        transport.limits().default_deadline
    }

    /// Resolve a name to (handle, identity) through the fake.
    fn resolve(
        transport: &mut dyn RawTransport,
        parent: &FileHandle,
        component: &[u8],
    ) -> (FileHandle, ObjectIdentity) {
        let deadline = deadline(transport);
        let (handle, attributes) = transport
            .lookup(parent, &name(component), AttrMask::STAT, deadline)
            .expect("lookup resolves");
        (handle, identity_of(&attributes).expect("identity"))
    }

    fn exists(transport: &mut dyn RawTransport, parent: &FileHandle, component: &[u8]) -> bool {
        let deadline = deadline(transport);
        transport
            .lookup(parent, &name(component), AttrMask::IDENTITY, deadline)
            .is_ok()
    }

    #[test]
    fn a_remove_unlinks_the_name_and_later_lookups_agree() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"doomed", b"bytes".to_vec());
        let (_, target) = resolve(&mut fake, &root, b"doomed");

        let mut dispatcher = TransportDispatcher::new();
        let outcome = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Remove {
                    parent: root.clone(),
                    name: name(b"doomed"),
                    target,
                    kind: RemoveKind::File,
                },
            )
            .expect("REMOVE dispatches");

        assert_eq!(outcome, NamespaceOutcome::Removed);
        assert!(
            !exists(&mut fake, &root, b"doomed"),
            "the namespace mutation must be visible to the next lookup"
        );
    }

    #[test]
    fn unlinking_a_directory_is_refused_before_anything_is_removed() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_directory(&root, b"adir");
        let (_, target) = resolve(&mut fake, &root, b"adir");

        let mut dispatcher = TransportDispatcher::new();
        let error = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Remove {
                    parent: root.clone(),
                    // `unlink` on a directory is the caller's error, not something
                    // the provider absorbs by calling it a rmdir.
                    name: name(b"adir"),
                    target,
                    kind: RemoveKind::File,
                },
            )
            .unwrap_err();

        assert!(matches!(&error, FacadeError::Protocol(p) if p.status == Nfs4Status::ISDIR));
        assert!(exists(&mut fake, &root, b"adir"), "nothing was removed");
    }

    #[test]
    fn a_remove_refuses_a_name_that_now_resolves_to_another_object() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"target", b"original".to_vec());
        let (_, pinned) = resolve(&mut fake, &root, b"target");

        // An out-of-band replacement: the name now points somewhere else.
        let mut replaced = FakeTransport::new();
        let replaced_root = replaced.root();
        replaced.insert_file(&replaced_root, b"filler", b"x".to_vec());
        replaced.insert_file(&replaced_root, b"target", b"replacement".to_vec());
        let (_, other) = resolve(&mut replaced, &replaced_root, b"target");
        assert_ne!(pinned.fileid, other.fileid, "the fixture must differ");

        let mut dispatcher = TransportDispatcher::new();
        let error = dispatcher
            .dispatch(
                &mut replaced,
                &NamespaceMutation::Remove {
                    parent: replaced_root.clone(),
                    name: name(b"target"),
                    target: pinned,
                    kind: RemoveKind::File,
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::IdentityUnproven(_))
        ));
        assert!(
            exists(&mut replaced, &replaced_root, b"target"),
            "an unproven identity must not unlink anything"
        );
    }

    #[test]
    fn a_rename_moves_the_name_and_keeps_the_object() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        let source_dir = fake.insert_directory(&root, b"from");
        let target_dir = fake.insert_directory(&root, b"to");
        fake.insert_file(&source_dir, b"before", b"payload".to_vec());
        let (original_handle, source) = resolve(&mut fake, &source_dir, b"before");

        let mut dispatcher = TransportDispatcher::new();
        let outcome = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Rename {
                    source_parent: source_dir.clone(),
                    source_name: name(b"before"),
                    target_parent: target_dir.clone(),
                    target_name: name(b"after"),
                    source,
                    mode: RenameMode::Replace,
                },
            )
            .expect("RENAME dispatches");

        let NamespaceOutcome::Renamed(effect) = outcome else {
            panic!("expected a rename outcome");
        };
        // A rename moves a name, never the object: same fileid, same filehandle,
        // so every open handle on the source stays valid.
        assert_eq!(effect.identity, source);
        assert_eq!(effect.handle.as_bytes(), original_handle.as_bytes());
        assert!(!exists(&mut fake, &source_dir, b"before"));
        assert!(exists(&mut fake, &target_dir, b"after"));
    }

    #[test]
    fn a_no_replace_rename_onto_an_existing_name_is_refused_before_dispatch() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"before", b"a".to_vec());
        fake.insert_file(&root, b"after", b"b".to_vec());
        let (_, source) = resolve(&mut fake, &root, b"before");
        let (_, occupant) = resolve(&mut fake, &root, b"after");

        let mut dispatcher = TransportDispatcher::new();
        let error = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Rename {
                    source_parent: root.clone(),
                    source_name: name(b"before"),
                    target_parent: root.clone(),
                    target_name: name(b"after"),
                    source,
                    mode: RenameMode::NoReplace,
                },
            )
            .unwrap_err();

        assert!(matches!(&error, FacadeError::Protocol(p) if p.status == Nfs4Status::EXIST));
        // NFSv4.0 RENAME always replaces, so the refusal has to happen before the
        // COMPOUND is sent; proving the occupant survived proves it did.
        let (_, still_there) = resolve(&mut fake, &root, b"after");
        assert_eq!(still_there, occupant);
        assert!(exists(&mut fake, &root, b"before"));
    }

    #[test]
    fn a_create_makes_a_directory_that_the_next_lookup_finds() {
        let mut fake = FakeTransport::new();
        let root = fake.root();

        let mut dispatcher = TransportDispatcher::new();
        let outcome = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::CreateDirectory {
                    parent: root.clone(),
                    name: name(b"made"),
                    mode: 0o750,
                },
            )
            .expect("CREATE dispatches");

        let NamespaceOutcome::Created(effect) = outcome else {
            panic!("expected a created outcome");
        };
        let (handle, identity) = resolve(&mut fake, &root, b"made");
        assert_eq!(identity, effect.identity);
        assert_eq!(handle.as_bytes(), effect.handle.as_bytes());

        let deadline = deadline(&fake);
        let (_, attributes) = fake
            .lookup(&root, &name(b"made"), AttrMask::STAT, deadline)
            .expect("stat the new directory");
        assert_eq!(attributes.file_type, Some(Nfs4Type::Directory));
        assert_eq!(attributes.mode, Some(0o750));
    }

    #[test]
    fn a_setattr_changes_the_attribute_map_the_next_stat_reads() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"file", b"data".to_vec());
        let (handle, target) = resolve(&mut fake, &root, b"file");

        let mut dispatcher = TransportDispatcher::new();
        let outcome = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::SetAttributes {
                    object: handle,
                    target,
                    update: MetadataUpdate {
                        mode: Some(0o640),
                        uid: Some(501),
                        gid: Some(20),
                        accessed_nanos: None,
                        modified_nanos: Some(1_700_000_000_123_456_789),
                    },
                },
            )
            .expect("SETATTR dispatches");

        assert!(matches!(outcome, NamespaceOutcome::AttributesSet(_)));

        let deadline = deadline(&fake);
        let (_, attributes) = fake
            .lookup(&root, &name(b"file"), AttrMask::STAT, deadline)
            .expect("stat after SETATTR");
        assert_eq!(attributes.mode, Some(0o640));
        assert_eq!(attributes.owner.as_deref(), Some(b"501".as_slice()));
        assert_eq!(attributes.owner_group.as_deref(), Some(b"20".as_slice()));
        assert_eq!(
            attributes.time_modify,
            Some(Nfs4Time {
                seconds: 1_700_000_000,
                nanoseconds: 123_456_789
            })
        );
    }

    #[test]
    fn a_truncate_without_a_write_stateid_is_refused_rather_than_sent_anonymously() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"file", b"0123456789".to_vec());
        let (handle, target) = resolve(&mut fake, &root, b"file");

        let mut dispatcher = TransportDispatcher::new();
        let error = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Truncate {
                    object: handle,
                    target,
                    len: 4,
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::IdentityUnproven(_))
        ));
        let deadline = deadline(&fake);
        let (_, attributes) = fake
            .lookup(&root, &name(b"file"), AttrMask::STAT, deadline)
            .expect("stat");
        assert_eq!(attributes.size, Some(10), "nothing was truncated");
    }

    #[test]
    fn a_truncate_with_a_write_stateid_resizes_the_object() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"file", b"0123456789".to_vec());
        let (handle, target) = resolve(&mut fake, &root, b"file");

        // Any non-anonymous stateid stands for an open holding WRITE access; the
        // fake enforces only that the anonymous one is refused.
        let stateid = Stateid {
            seqid: 1,
            other: [7u8; 12],
        };
        let mut dispatcher = TransportDispatcher::with_write_stateid(stateid);
        dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Truncate {
                    object: handle,
                    target,
                    len: 4,
                },
            )
            .expect("truncate dispatches");

        let deadline = deadline(&fake);
        let (_, attributes) = fake
            .lookup(&root, &name(b"file"), AttrMask::STAT, deadline)
            .expect("stat");
        assert_eq!(attributes.size, Some(4));
    }

    #[test]
    fn an_empty_metadata_update_is_refused_instead_of_reporting_a_change() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"file", b"data".to_vec());
        let (handle, target) = resolve(&mut fake, &root, b"file");

        let mut dispatcher = TransportDispatcher::new();
        let error = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::SetAttributes {
                    object: handle,
                    target,
                    update: MetadataUpdate {
                        mode: None,
                        uid: None,
                        gid: None,
                        accessed_nanos: None,
                        modified_nanos: None,
                    },
                },
            )
            .unwrap_err();

        assert!(matches!(&error, FacadeError::Protocol(p) if p.status == Nfs4Status::INVAL));
    }

    #[test]
    fn the_dispatcher_uses_the_transport_it_is_handed_not_one_it_owns() {
        // Two independent servers. The dispatcher is constructed once and used
        // against both; each mutation must land on the server it was passed.
        let mut first = FakeTransport::new();
        let mut second = FakeTransport::new();
        let first_root = first.root();
        let second_root = second.root();

        let mut dispatcher = TransportDispatcher::new();
        dispatcher
            .dispatch(
                &mut first,
                &NamespaceMutation::CreateDirectory {
                    parent: first_root.clone(),
                    name: name(b"only-here"),
                    mode: 0o700,
                },
            )
            .expect("create on the first server");

        assert!(exists(&mut first, &first_root, b"only-here"));
        assert!(
            !exists(&mut second, &second_root, b"only-here"),
            "the mutation must not reach a transport it was never given"
        );
    }

    #[test]
    fn the_limits_the_dispatcher_reads_come_from_the_bound_transport() {
        let fake = FakeTransport::new();
        let limits: TransportLimits = fake.limits();
        assert_eq!(limits.default_deadline.millis, 5_000);
    }
}
