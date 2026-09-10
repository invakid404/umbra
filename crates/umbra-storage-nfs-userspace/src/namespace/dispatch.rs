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
    AttrMask, AttrValues, ChangeInfo, ComponentName, Compound, CreateType, Deadline, Nfs4Op,
    Nfs4Time, Nfs4Type, OpCode, OpReply, RawTransport,
};

use super::{
    NamespaceDispatcher, NamespaceEffect, NamespaceMutation, NamespaceOutcome, RemoveKind,
};

/// Puts namespace mutations on the wire over whichever [`RawTransport`] the
/// request is running on.
///
/// Stateless: it holds no transport (see [`NamespaceDispatcher`]) and no
/// stateid, because the one operation that needs a stateid —
/// [`NamespaceMutation::Truncate`] — carries its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportDispatcher;

impl TransportDispatcher {
    /// A dispatcher.
    pub fn new() -> Self {
        Self
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
                // **F15.** The directory's change attribute *before* anything is
                // probed. REMOVE is a pathname operation, and a name is not an
                // object: between the probe below and the REMOVE that follows it,
                // another client can unlink this name and create a different
                // object under it. The candidate proved the identity, discarded
                // the REMOVE's `ChangeInfo`, and reported the pinned removal
                // regardless — so a racing replacement was reported as the
                // caller's own.
                //
                // This is the reading the protocol supports. RFC 7530 §14.2
                // preserves operation *order* within a COMPOUND and guarantees no
                // atomicity across one, so a VERIFY/NVERIFY guard would be an
                // atomicity claim v4.0 cannot make; it is deliberately not used.
                // What the server can be asked is whether its directory moved,
                // which is what `cinfo` reports.
                let before = transport
                    .getattr(parent, AttrMask::CHANGE, deadline)?
                    .change;

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
                    // **F13.** A known non-directory. `RemoveKind::File` covers a
                    // regular file *and* a logical-symlink name — see
                    // [`RemoveKind`](super::RemoveKind) — so this deliberately
                    // does not narrow to `Nfs4Type::Regular`; that would refuse
                    // removals the contract supports.
                    (RemoveKind::File, Some(_)) => {}
                    // **F13.** No `FATTR4_TYPE` at all. The probe exists to prove
                    // what the name resolves to, and a reply with no type proves
                    // nothing: accepting it dispatched an unlink against an object
                    // whose kind was never established, which is exactly what the
                    // directory arm above already refuses.
                    (RemoveKind::File, None) => {
                        return Err(unproven(format!(
                            "the server answered without FATTR4_TYPE for {}, so it cannot be \
                             shown to be a kind this unlink may remove",
                            String::from_utf8_lossy(name.as_bytes())
                        )));
                    }
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
                let info = match reply.expect(1)? {
                    OpReply::Remove(info) => *info,
                    other => return Err(shape(OpCode::Remove, other.opcode())),
                };
                remove_evidence(name, before, info)?;
                Ok(NamespaceOutcome::Removed)
            }

            NamespaceMutation::Rename {
                source_parent,
                source_name,
                target_parent,
                target_name,
                source,
                mode,
            } => {
                if *mode == RenameMode::NoReplace {
                    // R1-006: this used to probe the destination and, if the probe
                    // did not return Ok, fall through to an ordinary replacing
                    // RENAME. That is check-then-rename: a destination created
                    // between the probe and the RENAME was silently overwritten,
                    // and so was one whose probe failed with EIO, ACCESS or STALE.
                    // NFSv4.0 has no atomic no-replace rename, so the dispatcher
                    // refuses rather than emulating one. `ops::rename` refuses the
                    // same request earlier; this is the seam's own guard, so no
                    // future caller can reach the wire with it.
                    return Err(unsupported_no_replace());
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
                // F14: an unrepresentable timestamp stops the update here,
                // before any SETATTR is dispatched, so a mixed update can never
                // apply half of what it named.
                let values = attr_values(update)?;
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
                stateid,
            } => {
                if *stateid == Stateid::ANONYMOUS {
                    // Not a defensive check for its own sake: a server may accept
                    // an anonymous SETATTR of FATTR4_SIZE or refuse it, and a
                    // truncation whose success depends on which server it met is
                    // not a truncation this provider will report.
                    return Err(unproven(
                        "truncation sets FATTR4_SIZE, which RFC 7530 §16.32 requires an open \
                         stateid with WRITE access for; the anonymous stateid is not one"
                            .to_owned(),
                    ));
                }
                let values = AttrValues {
                    size: Some(*len),
                    ..AttrValues::default()
                };
                self.set(transport, object, target, values, *stateid, deadline)
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

/// Whether a REMOVE's own change info proves it unlinked the name that was
/// probed (**F15**).
///
/// `captured` is the directory's `FATTR4_CHANGE` read before the probe. When the
/// server reports that same value as the change it saw immediately *before* the
/// REMOVE, nothing happened in that directory across the whole probe → REMOVE
/// window, so the object proven under the name is the object the name still held
/// when it was unlinked.
///
/// Everything else is uncertain, and uncertain is reported as uncertain. The
/// answer is deliberately not a success and deliberately not a retriable failure:
/// the REMOVE returned `NFS4ERR_OK`, so a name really is gone, and re-running it
/// would unlink whatever occupies the name next. There is nothing to roll back
/// either, for the same reason.
///
/// The refusal carries [`journal::BLOCKED_RECOVERABLE`](crate::journal::BLOCKED_RECOVERABLE),
/// which is the failure model's state for "evidence matching neither deterministic
/// outcome": `NfsUserspaceStorage` latches it, refuses later mutations with this
/// diagnosis carried forward verbatim, and cannot report a clean release. The
/// evidence and the server state are both left exactly as they are.
///
/// A server that never reports `cinfo.atomic` therefore makes every pinned unlink
/// uncertain. That is the conservative direction and it is the intended one: a
/// false pinned removal reports that the caller's object is gone when another
/// client's object is what was destroyed.
fn remove_evidence(
    name: &ComponentName,
    captured: Option<u64>,
    info: ChangeInfo,
) -> FacadeResult<()> {
    let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
    let why = match captured {
        Some(captured) if info.atomic && info.before == captured => return Ok(()),
        Some(captured) if info.atomic => format!(
            "the directory holding {name} changed from {captured} to {} between the probe and \
             the REMOVE",
            info.before
        ),
        Some(_) => format!(
            "the server did not report atomic change info for the REMOVE of {name}, so its \
             before-state cannot rule out a replacement between the probe and the unlink"
        ),
        None => format!(
            "the server answered without FATTR4_CHANGE for the directory holding {name}, so \
             there is no before-state to compare the REMOVE against"
        ),
    };
    Err(unproven(format!(
        "{} the REMOVE of {name} succeeded, but {why}; the name that was unlinked cannot be \
         shown to be the object the caller pinned. Re-running it would unlink whatever holds \
         the name next, so the run stops with the evidence retained rather than reporting a \
         removal it cannot prove.",
        crate::journal::BLOCKED_RECOVERABLE
    )))
}

/// Translate the contract's [`MetadataUpdate`] into settable NFSv4 attributes.
///
/// `uid` and `gid` become `FATTR4_OWNER` / `FATTR4_OWNER_GROUP`, which NFSv4
/// carries as name strings. Under AUTH_SYS with no idmapper the numeric form is
/// the identity string a server accepts, so the number is rendered verbatim
/// rather than mapped to a name this provider cannot verify.
///
/// **F14.** Fallible, because a time that cannot be represented must stop the
/// update rather than shrink it. See [`nfs_time`].
fn attr_values(update: &MetadataUpdate) -> FacadeResult<AttrValues> {
    Ok(AttrValues {
        size: None,
        mode: update.mode,
        owner: update.uid.map(|uid| uid.to_string().into_bytes()),
        owner_group: update.gid.map(|gid| gid.to_string().into_bytes()),
        time_access: update.accessed_nanos.map(nfs_time).transpose()?,
        time_modify: update.modified_nanos.map(nfs_time).transpose()?,
    })
}

/// Split nanoseconds since the epoch into `nfstime4`.
///
/// **F14.** An unrepresentable value is refused, not dropped. It used to return
/// `None`, on the stated reasoning that "the caller's `attrset` check then
/// reports the attribute as unset" — which was not true of any caller. The mask
/// the SETATTR is checked against is computed from the values that survived this
/// conversion, so a dropped attribute was never in the requested set and nothing
/// noticed it was missing. A `SetMetadata` naming mode *and* an out-of-range
/// mtime therefore applied the mode, left the time alone, and answered
/// `AttributesSet`: a partial effect reported as a whole one. A time-only update
/// was refused only because the resulting value set was empty.
///
/// Refusing before the SETATTR is dispatched is what keeps it from being partial.
///
/// The Euclidean division is deliberate: `nfstime4.nseconds` is unsigned, so a
/// pre-epoch time must floor the seconds and carry a positive remainder rather
/// than truncate toward zero.
fn nfs_time(nanos: i128) -> FacadeResult<Nfs4Time> {
    let seconds = nanos.div_euclid(1_000_000_000);
    let remainder = nanos.rem_euclid(1_000_000_000);
    match (i64::try_from(seconds), u32::try_from(remainder)) {
        (Ok(seconds), Ok(nanoseconds)) => Ok(Nfs4Time {
            seconds,
            nanoseconds,
        }),
        _ => Err(status(Nfs4Status::INVAL, OpCode::SetAttr)),
    }
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

/// The refusal an atomic no-replace rename gets (**R1-006**).
///
/// `NFS4ERR_NOTSUPP` is the honest wire answer: the guarantee is not one this
/// version of the protocol offers, as opposed to a destination that happens to
/// exist right now, which is what the old `NFS4ERR_EXIST` claimed.
fn unsupported_no_replace() -> FacadeError {
    FacadeError::protocol(Nfs4Status::NOTSUPP, OpCode::Rename, 0)
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

    /// **F13.** A name whose type the server did not report is not unlinked.
    ///
    /// The probe exists to prove what the name resolves to; the module comment
    /// says so. The candidate's wildcard `(RemoveKind::File, _)` arm accepted
    /// `None` — no `FATTR4_TYPE` in the reply — and dispatched the REMOVE anyway,
    /// against an object whose kind was never established. The directory arm had
    /// always refused the same absence.
    ///
    /// The refusal is not narrowed to `Nfs4Type::Regular`: `RemoveKind::File`
    /// covers a logical-symlink name too, and refusing those would break unlinks
    /// the contract supports.
    #[test]
    fn f13_a_remove_without_a_reported_type_is_refused_before_any_unlink() {
        struct NoType(FakeTransport);
        impl RawTransport for NoType {
            fn wire_profile(&self) -> crate::transport::WireProfile {
                self.0.wire_profile()
            }
            fn limits(&self) -> TransportLimits {
                self.0.limits()
            }
            fn connection(&self) -> crate::transport::ConnectionState {
                self.0.connection()
            }
            fn submit(
                &mut self,
                call: Compound,
                deadline: Deadline,
            ) -> crate::transport::TransportResult<crate::transport::CompoundReply> {
                let mut reply = self.0.submit(call, deadline)?;
                // A server that answers the GETATTR without FATTR4_TYPE.
                for result in &mut reply.results {
                    if let crate::transport::OpReply::GetAttr(attributes) = result {
                        attributes.file_type = None;
                    }
                }
                Ok(reply)
            }
            fn cancel(
                &mut self,
                token: crate::transport::CallToken,
            ) -> crate::transport::TransportResult<crate::transport::Retirement> {
                self.0.cancel(token)
            }
            fn reconnect(
                &mut self,
            ) -> crate::transport::TransportResult<crate::transport::ConnectionEpoch> {
                self.0.reconnect()
            }
            fn install_faults(&mut self, plan: Box<dyn crate::transport::FaultPlan>) {
                self.0.install_faults(plan)
            }
        }

        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"untyped", b"bytes".to_vec());
        let (_, target) = resolve(&mut fake, &root, b"untyped");
        let mut transport = NoType(fake);

        let mut dispatcher = TransportDispatcher::new();
        let refused = dispatcher
            .dispatch(
                &mut transport,
                &NamespaceMutation::Remove {
                    parent: root.clone(),
                    name: name(b"untyped"),
                    target,
                    kind: RemoveKind::File,
                },
            )
            .expect_err("an unknown type is not a kind this unlink may remove");
        assert!(
            matches!(
                refused,
                FacadeError::Authority(AuthorityError::IdentityUnproven(_))
            ),
            "{refused:?}"
        );
        assert!(
            exists(&mut transport.0, &root, b"untyped"),
            "the refusal must land before anything is unlinked"
        );
    }

    /// **F13.** A known non-directory is still removed. The refusal above is
    /// about absent evidence, not about narrowing the supported kinds.
    #[test]
    fn f13_a_known_non_directory_is_still_unlinked() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"ordinary", b"bytes".to_vec());
        let (_, target) = resolve(&mut fake, &root, b"ordinary");

        let mut dispatcher = TransportDispatcher::new();
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut fake,
                    &NamespaceMutation::Remove {
                        parent: root.clone(),
                        name: name(b"ordinary"),
                        target,
                        kind: RemoveKind::File,
                    },
                )
                .expect("a typed regular file unlinks"),
            NamespaceOutcome::Removed
        );
        assert!(!exists(&mut fake, &root, b"ordinary"));
    }

    /// **F14.** A mixed mode-and-time update whose time cannot be represented is
    /// refused before any SETATTR reaches the wire.
    ///
    /// `nfs_time` returned `None` for an out-of-range value and `attr_values`
    /// dropped it, on the stated reasoning that "the caller's `attrset` check
    /// then reports the attribute as unset". No caller did that: the requested
    /// mask is computed from the values that survived the conversion, so the
    /// dropped attribute was never in the requested set and nothing noticed. The
    /// update applied the mode, left the time alone, and answered `AttributesSet`
    /// — a partial effect reported as a whole one.
    #[test]
    fn f14_an_unrepresentable_time_refuses_the_whole_metadata_update() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"stamped", b"bytes".to_vec());
        let (handle, target) = resolve(&mut fake, &root, b"stamped");
        let before = fake
            .getattr(&handle, AttrMask::STAT, deadline(&fake))
            .expect("attributes");

        let mut dispatcher = TransportDispatcher::new();
        for update in [
            // Mixed: a mode that is perfectly settable beside a time that is not.
            MetadataUpdate {
                mode: Some(0o700),
                uid: None,
                gid: None,
                accessed_nanos: None,
                modified_nanos: Some(i128::MAX),
            },
            // The other timestamp field, and the negative extreme.
            MetadataUpdate {
                mode: Some(0o700),
                uid: None,
                gid: None,
                accessed_nanos: Some(i128::MIN),
                modified_nanos: None,
            },
        ] {
            let refused = dispatcher
                .dispatch(
                    &mut fake,
                    &NamespaceMutation::SetAttributes {
                        object: handle.clone(),
                        target,
                        update,
                    },
                )
                .expect_err("an unrepresentable time refuses the update");
            assert_eq!(
                refused.status(),
                Some(Nfs4Status::INVAL),
                "the refusal names the invalid argument: {refused:?}"
            );
        }

        let after = fake
            .getattr(&handle, AttrMask::STAT, deadline(&fake))
            .expect("attributes");
        assert_eq!(
            after.mode, before.mode,
            "nothing may be applied by an update that was refused"
        );
        assert_eq!(after.time_modify, before.time_modify);
    }

    /// **F14.** Representable times still work, negative ones included, and the
    /// Euclidean split keeps `nseconds` unsigned.
    #[test]
    fn f14_representable_times_are_still_applied_including_pre_epoch() {
        assert_eq!(
            nfs_time(0).expect("the epoch is representable"),
            Nfs4Time {
                seconds: 0,
                nanoseconds: 0
            }
        );
        assert_eq!(
            nfs_time(-1).expect("one nanosecond before the epoch"),
            Nfs4Time {
                seconds: -1,
                nanoseconds: 999_999_999
            },
            "a pre-epoch time floors the seconds and carries a positive remainder"
        );
        let max = i128::from(i64::MAX) * 1_000_000_000 + 999_999_999;
        assert_eq!(
            nfs_time(max).expect("the representable ceiling"),
            Nfs4Time {
                seconds: i64::MAX,
                nanoseconds: 999_999_999
            }
        );
        assert!(
            nfs_time(max + 1).is_err(),
            "one past the ceiling is refused"
        );
        let min = i128::from(i64::MIN) * 1_000_000_000;
        assert_eq!(
            nfs_time(min).expect("the representable floor").seconds,
            i64::MIN
        );
        assert!(nfs_time(min - 1).is_err(), "one past the floor is refused");
    }

    /// **F15.** A REMOVE whose change info cannot rule out a replacement is not
    /// reported as the pinned removal.
    #[test]
    fn f15_remove_evidence_accepts_only_an_unmoved_atomic_before() {
        let doomed = name(b"doomed");
        assert!(remove_evidence(
            &doomed,
            Some(7),
            ChangeInfo {
                atomic: true,
                before: 7,
                after: 8
            }
        )
        .is_ok());

        for (captured, info, why) in [
            (
                Some(7),
                ChangeInfo {
                    atomic: true,
                    before: 9,
                    after: 10,
                },
                "the directory moved between the probe and the unlink",
            ),
            (
                Some(7),
                ChangeInfo {
                    atomic: false,
                    before: 7,
                    after: 8,
                },
                "a non-atomic before-state cannot rule out the race",
            ),
            (
                None,
                ChangeInfo {
                    atomic: true,
                    before: 7,
                    after: 8,
                },
                "no captured before-state to compare against",
            ),
        ] {
            let refused = remove_evidence(&doomed, captured, info)
                .expect_err(why)
                .to_umbra("execute.unlink");
            assert!(
                crate::journal::is_blocked(&refused),
                "{why}: the run must stop with the evidence retained, got {refused:?}"
            );
            assert!(
                refused
                    .context
                    .contains("cannot be shown to be the object the caller pinned"),
                "{why}: the diagnosis must say what could not be proven: {}",
                refused.context
            );
        }
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

    /// **R1-006.** No-replace rename is refused as unsupported, whether or not the
    /// destination happens to exist.
    ///
    /// The previous implementation probed the destination and refused only when
    /// the probe returned `Ok`. That is check-then-rename, which
    /// `docs/design/syscall-matrix.md:29` explicitly prohibits as an atomic
    /// no-replace: a destination created between the probe and the RENAME was
    /// silently overwritten, and so was one whose probe failed for any reason
    /// other than NOENT. The absent-destination case below is the one that used to
    /// *succeed*, and it is the one the race actually exploits.
    #[test]
    fn r1_006_no_replace_rename_is_refused_whether_or_not_the_destination_exists() {
        for (label, seed_destination) in [("occupied", true), ("absent", false)] {
            let mut fake = FakeTransport::new();
            let root = fake.root();
            fake.insert_file(&root, b"before", b"a".to_vec());
            if seed_destination {
                fake.insert_file(&root, b"after", b"b".to_vec());
            }
            let (_, source) = resolve(&mut fake, &root, b"before");

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

            // NOTSUPP, not EXIST: the guarantee is unavailable in this protocol
            // version, which is a different fact from "the destination is taken".
            assert!(
                matches!(&error, FacadeError::Protocol(p) if p.status == Nfs4Status::NOTSUPP),
                "{label}: {error:?}"
            );
            // Nothing was sent, so the source is still where it was and any
            // occupant survived.
            assert!(exists(&mut fake, &root, b"before"), "{label}");
            assert_eq!(
                exists(&mut fake, &root, b"after"),
                seed_destination,
                "{label}: the destination must be untouched"
            );
        }
    }

    /// **R1-006.** Ordinary replacing rename is unaffected by the refusal above.
    #[test]
    fn r1_006_ordinary_replace_rename_still_replaces() {
        let mut fake = FakeTransport::new();
        let root = fake.root();
        fake.insert_file(&root, b"before", b"a".to_vec());
        fake.insert_file(&root, b"after", b"b".to_vec());
        let (_, source) = resolve(&mut fake, &root, b"before");

        let mut dispatcher = TransportDispatcher::new();
        let outcome = dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Rename {
                    source_parent: root.clone(),
                    source_name: name(b"before"),
                    target_parent: root.clone(),
                    target_name: name(b"after"),
                    source,
                    mode: RenameMode::Replace,
                },
            )
            .expect("a replacing rename is supported");
        match outcome {
            NamespaceOutcome::Renamed(effect) => assert_eq!(effect.identity, source),
            other => panic!("expected a rename outcome, got {other:?}"),
        }
        assert!(!exists(&mut fake, &root, b"before"));
        assert!(exists(&mut fake, &root, b"after"));
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
                    stateid: Stateid::ANONYMOUS,
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
        let mut dispatcher = TransportDispatcher::new();
        dispatcher
            .dispatch(
                &mut fake,
                &NamespaceMutation::Truncate {
                    object: handle,
                    target,
                    len: 4,
                    stateid,
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
