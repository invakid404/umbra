//! Object identity: the `(fsid, fileid)` pair, and the translation to `BlobStat`.
//!
//! # Identity is not the filehandle, and not the name
//!
//! The frozen [`ObjectIdentity`] is `(fsid, fileid)`, and this module is where
//! that pair becomes the contract's [`ObjectId`]. Three consequences follow, and
//! all three are the acceptance criteria this node owes:
//!
//! * **Rename preserves identity.** A rename changes which name resolves to an
//!   object, not the object. A [`PinnedObject`] carries the identity it proved at
//!   open time and is never re-resolved from a name, so a rename cannot retarget
//!   it.
//! * **Replacement produces a different identity.** Binding a name to a new
//!   object gives that object its own `fileid`, so a fresh lookup returns a
//!   different [`ObjectId`] while a handle opened before the replacement keeps
//!   answering for the original. Detection, not assumption.
//! * **Unlink retains identity for open handles.** The pin holds the filehandle
//!   and the identity; removing the last name does not disturb either, and the
//!   server releases the object at CLOSE.
//!
//! # Attributes are never defaulted
//!
//! [`blob_stat`] refuses when the server did not return an attribute the contract
//! requires. A plausible default here would be indistinguishable from a real
//! answer at every layer above, which is exactly the failure the frozen
//! [`Attributes`] type is shaped to prevent by making every field optional.

use umbra_core::{BlobStat, ErrorKind, ObjectId, ObjectKind, Result, UmbraError};
use uuid::Uuid;

use crate::error::{AuthorityError, FacadeError, FacadeResult};
use crate::handle::{FileHandle, ObjectIdentity};
use crate::transport::{AttrMask, Attributes, Deadline, Nfs4Type, RawTransport};

/// Domain separator mixed into every derived [`ObjectId`].
///
/// Two different providers must not derive the same contract object id from
/// coincidentally equal server numbers, so the provider id participates in the
/// derivation rather than the raw pair alone.
const DOMAIN: u64 = 0x6e66_732d_7573_7200; // "nfs-usr\0"

/// Derive the contract [`ObjectId`] for a server object.
///
/// The derivation is deterministic and total: the same `(fsid, fileid)` always
/// yields the same id, so identity is comparable across calls, across a reconnect
/// and across a rename. Distinct triples yield distinct ids for as long as the
/// mixing below stays injective on the 192 bits it folds into 128.
///
/// The result is an opaque 128-bit identity, not a well-formed UUIDv4. Consumers
/// must not read version or variant bits from it, exactly as
/// [`umbra_core::OperationId::derive`] documents for its own derivation.
pub fn object_id(identity: ObjectIdentity) -> ObjectId {
    let high = mix(
        DOMAIN ^ identity.fsid.major,
        identity.fsid.minor,
        identity.fileid,
    );
    let low = mix(
        identity.fileid,
        DOMAIN ^ identity.fsid.minor,
        identity.fsid.major,
    );
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    ObjectId(Uuid::from_bytes(bytes))
}

/// A SplitMix64 finalizer over three folded words.
///
/// Chosen because it is short, dependency-free and well-diffused; nothing here
/// needs a cryptographic digest, only a stable spread over the server's dense
/// small `fileid` values so neighbouring objects do not produce neighbouring ids.
fn mix(a: u64, b: u64, c: u64) -> u64 {
    let mut z = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(31)
        .wrapping_add(b.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .rotate_left(27)
        .wrapping_add(c.wrapping_mul(0x94D0_49BB_1331_11EB));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// An object whose identity has been proven, pinned to the filehandle that proved
/// it.
///
/// Every read, write, commit and enumeration in this crate goes through a pin.
/// That is the mechanism behind the handle guarantee in the pinned syscall
/// matrix: "reads/stat reference the opened object, including after
/// rename/unlink/replacement".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedObject {
    identity: ObjectIdentity,
    handle: FileHandle,
    kind: Nfs4Type,
}

impl PinnedObject {
    /// Adopt an object whose attributes were returned by the server.
    ///
    /// Refuses when the server withheld either half of the identity pair, because
    /// an object whose identity cannot be proven must not be written through.
    pub fn adopt(handle: FileHandle, attributes: &Attributes) -> FacadeResult<Self> {
        let identity = identity_of(attributes)?;
        let kind = attributes.file_type.ok_or_else(|| {
            FacadeError::Authority(AuthorityError::IdentityUnproven(
                "the server did not return FATTR4_TYPE; the object kind is unproven".into(),
            ))
        })?;
        Ok(Self {
            identity,
            handle,
            kind,
        })
    }

    /// Look one object up and pin it, in one `PUTFH; GETATTR`.
    pub fn pin(
        transport: &mut dyn RawTransport,
        handle: FileHandle,
        deadline: Deadline,
    ) -> FacadeResult<Self> {
        let attributes = transport.getattr(&handle, AttrMask::STAT, deadline)?;
        Self::adopt(handle, &attributes)
    }

    /// The proven identity. Unchanged by rename, unlink or replacement.
    pub fn identity(&self) -> ObjectIdentity {
        self.identity
    }

    /// The contract object id for this object.
    pub fn object_id(&self) -> ObjectId {
        object_id(self.identity)
    }

    /// The filehandle every operation on this object uses.
    pub fn handle(&self) -> &FileHandle {
        &self.handle
    }

    /// The server-reported object type.
    pub fn kind(&self) -> Nfs4Type {
        self.kind
    }

    /// Whether this pin names a directory.
    pub fn is_directory(&self) -> bool {
        self.kind == Nfs4Type::Directory
    }

    /// Fetch current attributes for the pinned object and prove they still
    /// describe it.
    ///
    /// A server may hand out a different filehandle for one object, and a volatile
    /// filehandle may expire onto something else entirely. Comparing the identity
    /// the server reports now against the identity this pin proved is the only way
    /// to tell "the object changed" from "the handle now names something else".
    pub fn revalidate(
        &self,
        transport: &mut dyn RawTransport,
        deadline: Deadline,
    ) -> FacadeResult<Attributes> {
        let attributes = transport.getattr(&self.handle, AttrMask::STAT, deadline)?;
        let observed = identity_of(&attributes)?;
        if observed != self.identity {
            return Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
                format!(
                    "filehandle now reports fileid {} under fsid {}:{}, not the pinned {}",
                    observed.fileid, observed.fsid.major, observed.fsid.minor, self.identity.fileid
                ),
            )));
        }
        Ok(attributes)
    }

    /// Current [`BlobStat`] for the pinned object.
    pub fn stat(&self, transport: &mut dyn RawTransport, deadline: Deadline) -> Result<BlobStat> {
        let attributes = self
            .revalidate(transport, deadline)
            .map_err(|error| error.to_umbra("stat"))?;
        blob_stat(self.identity, &attributes)
    }
}

/// The identity pair carried by a set of attributes.
pub fn identity_of(attributes: &Attributes) -> FacadeResult<ObjectIdentity> {
    match (attributes.fsid, attributes.fileid) {
        (Some(fsid), Some(fileid)) => Ok(ObjectIdentity { fsid, fileid }),
        _ => Err(FacadeError::Authority(AuthorityError::IdentityUnproven(
            "the server did not return both fsid and fileid; identity cannot be proven".into(),
        ))),
    }
}

/// Translate server attributes into the contract's [`BlobStat`].
///
/// Every field is required. A missing attribute is an error rather than a
/// default, and an owner string this provider has not qualified a mapping for is
/// an honest [`ErrorKind::UnsupportedCapability`] rather than a fabricated uid.
pub fn blob_stat(identity: ObjectIdentity, attributes: &Attributes) -> Result<BlobStat> {
    let kind = object_kind(required(attributes.file_type, "FATTR4_TYPE")?)?;
    Ok(BlobStat {
        object_id: object_id(identity),
        kind,
        len: required(attributes.size, "FATTR4_SIZE")?,
        link_count: u64::from(required(attributes.numlinks, "FATTR4_NUMLINKS")?),
        mode: required(attributes.mode, "FATTR4_MODE")?,
        uid: numeric_identity(attributes.owner.as_deref(), "FATTR4_OWNER")?,
        gid: numeric_identity(attributes.owner_group.as_deref(), "FATTR4_OWNER_GROUP")?,
        modified_nanos: {
            let time = required(attributes.time_modify, "FATTR4_TIME_MODIFY")?;
            i128::from(time.seconds) * 1_000_000_000 + i128::from(time.nanoseconds)
        },
    })
}

fn required<T>(value: Option<T>, attribute: &str) -> Result<T> {
    value.ok_or_else(|| {
        UmbraError::new(
            ErrorKind::Io,
            "stat",
            format!("the server returned no {attribute}; no plausible value is substituted"),
        )
    })
}

fn object_kind(kind: Nfs4Type) -> Result<ObjectKind> {
    match kind {
        Nfs4Type::Regular => Ok(ObjectKind::File),
        Nfs4Type::Directory => Ok(ObjectKind::Directory),
        // A server symlink is not a logical symlink. Reporting one as
        // `ObjectKind::LogicalSymlink` would invite a consumer to read a target
        // this provider will not resolve, and resolving it is precisely the
        // physical escape the syscall matrix forbids.
        Nfs4Type::Symlink => Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "stat",
            "a server-side symlink is not a logical symlink; this provider neither \
             resolves nor reports one",
        )),
        Nfs4Type::Other(code) => Err(UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "stat",
            format!("NFSv4 object type {code} has no contract object kind"),
        )),
    }
}

/// Read an `AUTH_SYS` numeric owner string.
///
/// NFSv4 owner attributes are strings. Under `AUTH_SYS` a server that has no
/// identity-mapping domain configured sends the decimal uid, which is the only
/// form this provider has qualified. A domain form such as `user@example` needs a
/// mapping decision that has not been made, and inventing a uid for it would be a
/// false ownership answer.
fn numeric_identity(value: Option<&[u8]>, attribute: &str) -> Result<u32> {
    let bytes = required(value, attribute)?;
    let text = std::str::from_utf8(bytes).map_err(|_| {
        UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "stat",
            format!("{attribute} is not UTF-8; no identity mapping is qualified for it"),
        )
    })?;
    text.parse::<u32>().map_err(|_| {
        UmbraError::new(
            ErrorKind::UnsupportedCapability,
            "stat",
            format!(
                "{attribute} is {text:?}, not an AUTH_SYS numeric id; this provider has \
                 qualified no domain identity mapping and will not invent one"
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::ErrorKind;

    use crate::fixture;
    use crate::transport::{Fsid, Nfs4Time};

    fn identity(fileid: u64) -> ObjectIdentity {
        ObjectIdentity {
            fsid: Fsid { major: 1, minor: 2 },
            fileid,
        }
    }

    fn full() -> Attributes {
        Attributes {
            returned: AttrMask::STAT,
            file_type: Some(Nfs4Type::Regular),
            change: Some(7),
            size: Some(11),
            fsid: Some(Fsid { major: 1, minor: 2 }),
            fileid: Some(42),
            numlinks: Some(1),
            mode: Some(0o640),
            owner: Some(b"501".to_vec()),
            owner_group: Some(b"20".to_vec()),
            time_modify: Some(Nfs4Time {
                seconds: 3,
                nanoseconds: 5,
            }),
            lease_time: None,
            rdattr_error: None,
        }
    }

    #[test]
    fn an_object_id_is_deterministic_and_distinct_per_object() {
        assert_eq!(object_id(identity(42)), object_id(identity(42)));
        assert_ne!(object_id(identity(42)), object_id(identity(43)));
        let other_fs = ObjectIdentity {
            fsid: Fsid { major: 9, minor: 2 },
            fileid: 42,
        };
        assert_ne!(
            object_id(identity(42)),
            object_id(other_fs),
            "the same fileid on another filesystem is another object"
        );
    }

    #[test]
    fn a_stat_translates_every_field_and_never_invents_one() {
        let stat = blob_stat(identity(42), &full()).expect("a complete answer translates");
        assert_eq!(stat.object_id, object_id(identity(42)));
        assert_eq!(stat.kind, umbra_core::ObjectKind::File);
        assert_eq!(stat.len, 11);
        assert_eq!(stat.link_count, 1);
        assert_eq!(stat.mode, 0o640);
        assert_eq!(stat.uid, 501);
        assert_eq!(stat.gid, 20);
        assert_eq!(stat.modified_nanos, 3_000_000_005);

        for missing in [
            Attributes {
                size: None,
                ..full()
            },
            Attributes {
                mode: None,
                ..full()
            },
            Attributes {
                numlinks: None,
                ..full()
            },
            Attributes {
                time_modify: None,
                ..full()
            },
            Attributes {
                file_type: None,
                ..full()
            },
        ] {
            assert!(
                blob_stat(identity(42), &missing).is_err(),
                "a withheld attribute must not be defaulted to a plausible value"
            );
        }
    }

    #[test]
    fn an_unmappable_owner_is_refused_rather_than_given_a_made_up_uid() {
        let domain_form = Attributes {
            owner: Some(b"alice@example.test".to_vec()),
            ..full()
        };
        let error = blob_stat(identity(42), &domain_form).unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnsupportedCapability);
        assert!(error.context.contains("domain identity mapping"));
    }

    #[test]
    fn a_server_symlink_is_not_reported_as_a_logical_symlink() {
        let symlink = Attributes {
            file_type: Some(Nfs4Type::Symlink),
            ..full()
        };
        let error = blob_stat(identity(42), &symlink).unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnsupportedCapability);

        let exotic = Attributes {
            file_type: Some(Nfs4Type::Other(6)),
            ..full()
        };
        assert!(blob_stat(identity(42), &exotic).is_err());
    }

    #[test]
    fn an_identity_needs_both_halves_of_the_pair() {
        assert!(identity_of(&full()).is_ok());
        assert!(identity_of(&Attributes {
            fsid: None,
            ..full()
        })
        .is_err());
        assert!(identity_of(&Attributes {
            fileid: None,
            ..full()
        })
        .is_err());
    }

    #[test]
    fn a_pin_keeps_its_identity_while_a_replacement_gets_a_new_one() {
        let (mut fake, layout) = fixture::server();
        let original = fake.insert_file(&layout.root, b"note", b"first".to_vec());
        let deadline = crate::transport::Deadline { millis: 5_000 };
        let pin = PinnedObject::pin(&mut fake, original.clone(), deadline).expect("pin");
        let before = pin.identity();

        // Bind the same name to a different object: the pathname-replacement case.
        let replacement = fake.insert_file(&layout.root, b"note", b"second".to_vec());
        assert_ne!(original.as_bytes(), replacement.as_bytes());

        // The pin still answers for the object it proved, and its identity is
        // unchanged. This is the handle guarantee, in its smallest form.
        let attributes = pin.revalidate(&mut fake, deadline).expect("still valid");
        assert_eq!(identity_of(&attributes).unwrap(), before);
        assert_eq!(pin.identity(), before);

        // A fresh lookup of the same name finds the replacement, with its own id.
        let fresh = PinnedObject::pin(&mut fake, replacement, deadline).expect("pin");
        assert_ne!(fresh.identity(), before);
        assert_ne!(fresh.object_id(), pin.object_id());
    }

    #[test]
    fn revalidate_refuses_when_a_handle_starts_naming_another_object() {
        let (mut fake, layout) = fixture::server();
        let first = fake.insert_file(&layout.root, b"a", b"a".to_vec());
        let second = fake.insert_file(&layout.root, b"b", b"b".to_vec());
        let deadline = crate::transport::Deadline { millis: 5_000 };
        let pin = PinnedObject::pin(&mut fake, first, deadline).expect("pin");

        // Forge the situation a volatile filehandle creates: the same pin, a
        // handle that now resolves elsewhere.
        let forged = PinnedObject::adopt(
            second,
            &fake
                .getattr(pin.handle(), AttrMask::STAT, deadline)
                .expect("attributes"),
        )
        .expect("adopt");
        let error = forged.revalidate(&mut fake, deadline).unwrap_err();
        assert!(matches!(
            error,
            FacadeError::Authority(AuthorityError::IdentityUnproven(_))
        ));
    }
}
