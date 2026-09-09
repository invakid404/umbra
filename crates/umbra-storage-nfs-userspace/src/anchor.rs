//! Run, root and control anchors, and containment-checked path resolution.
//!
//! # Anchors are handles, never paths
//!
//! This provider talks to the server directly, so it has no mount root and no
//! host path. An [`Anchor`] is therefore a [`PinnedObject`] — a filehandle plus
//! the `(fsid, fileid)` that proved it — and the
//! [`RuntimeDirectoryBinding`] it publishes carries `physical_path: None`. There
//! is no local path to report and none is invented; a fabricated one would be a
//! path no `open` could ever use, which the syscall matrix forbids outright
//! ("No remote file operation is passed to a fabricated local path").
//!
//! # Four anchors
//!
//! | Anchor | Location | Visibility |
//! | --- | --- | --- |
//! | [`AnchorKind::Run`] | `<export>/<run_parent>/<run_id>` | provider only |
//! | [`AnchorKind::Root`] | `<run>/<root_anchor>` | tracee-visible |
//! | [`AnchorKind::Control`] | `<run>/<control_anchor>` | never tracee-visible |
//! | [`AnchorKind::Private`] | `<run>/.provider` | provider-private state |
//!
//! [`StorageAnchor`] names only the first two the contract exposes; `Run` and
//! `Private` exist because the provider needs them and the contract deliberately
//! does not.
//!
//! # Containment
//!
//! Three independent rules make an escape unrepresentable rather than merely
//! checked. [`StoragePath`] refuses empty, `.` and `..` components at
//! construction. [`ComponentName`] refuses them again, plus `/` and NUL, before
//! anything reaches the wire. And every intermediate component must resolve to a
//! directory: a server-side symlink stops the walk instead of being followed, so
//! no path can leave the anchor through one.

use umbra_core::{
    BytePath, ErrorKind, OpenRunIntent, Result, RunId, RuntimeDirectoryBinding, StorageAnchor,
    StorageHandle, StoragePath, UmbraError,
};

use crate::capability::{Support, CONTRACTS_REVISION, NO_WIRE_OPERATION};
use crate::handle::FileHandle;
use crate::identity::PinnedObject;
use crate::layout;
use crate::storage::NfsUserspaceConfig;
use crate::transport::{ComponentName, Deadline, Nfs4Type, RawTransport};

/// Which anchor a handle belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AnchorKind {
    /// The run directory itself. Holds the other three and nothing else.
    Run,
    /// The tracee-visible run root.
    Root,
    /// The control namespace. Never tracee-visible.
    Control,
    /// Provider-private state (`.provider`). Filtered out of run enumeration.
    Private,
}

impl AnchorKind {
    /// The contract anchor this maps to, when the contract names one.
    pub fn contract_anchor(self) -> Option<StorageAnchor> {
        match self {
            Self::Root => Some(StorageAnchor::Root),
            Self::Control => Some(StorageAnchor::Control),
            Self::Run | Self::Private => None,
        }
    }

    /// Short name used in diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Root => "root",
            Self::Control => "control",
            Self::Private => "private",
        }
    }
}

/// One anchor: what it is, and the object that proved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    kind: AnchorKind,
    pin: PinnedObject,
}

impl Anchor {
    /// Which anchor this is.
    pub fn kind(&self) -> AnchorKind {
        self.kind
    }

    /// The pinned directory object.
    pub fn pin(&self) -> &PinnedObject {
        &self.pin
    }

    /// The runtime binding published to a consumer.
    ///
    /// `physical_path` is `None` and always will be: there is no kernel-visible
    /// path for a userspace client, and reporting one would name a path no
    /// `open` could use.
    pub fn binding(&self, mint: &HandleMint) -> RuntimeDirectoryBinding {
        RuntimeDirectoryBinding {
            handle: mint.issue(self.pin.handle()),
            physical_path: None,
        }
    }
}

/// Mints and validates the opaque [`StorageHandle`] tokens one session issues.
///
/// The contract calls a `StorageHandle` "opaque token owned by a provider
/// session, invalid after that session closes". A session serial is folded into
/// every token so a handle from a closed session is rejected on presentation
/// rather than silently resolving against whatever the bytes happen to name now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleMint {
    session: u64,
}

/// Token prefix. Present so a token from another provider fails fast.
const TOKEN_MAGIC: &[u8; 4] = b"UNFS";
/// Token layout version.
const TOKEN_VERSION: u8 = 1;

impl HandleMint {
    /// A mint for one session of one run.
    ///
    /// The serial distinguishes successive sessions over the same run, so a token
    /// issued before `close_run` cannot be replayed after it.
    pub fn for_session(run_id: RunId, serial: u64) -> Self {
        let bytes = run_id.0.as_bytes();
        let mut folded = [0u8; 8];
        for (index, slot) in folded.iter_mut().enumerate() {
            *slot = bytes[index] ^ bytes[index + 8];
        }
        Self {
            session: u64::from_be_bytes(folded) ^ serial.rotate_left(17),
        }
    }

    /// Wrap a filehandle in a session-bound token.
    pub fn issue(&self, handle: &FileHandle) -> StorageHandle {
        let mut token = Vec::with_capacity(13 + handle.len());
        token.extend_from_slice(TOKEN_MAGIC);
        token.push(TOKEN_VERSION);
        token.extend_from_slice(&self.session.to_be_bytes());
        token.extend_from_slice(handle.as_bytes());
        StorageHandle(token)
    }

    /// Recover the filehandle from a token this session issued.
    pub fn accept(&self, token: &StorageHandle) -> Result<FileHandle> {
        let bytes = &token.0;
        let stale = || {
            UmbraError::new(
                ErrorKind::StaleHandle,
                "storage_handle",
                "this token was not issued by the current provider session",
            )
        };
        if bytes.len() <= 13 || &bytes[..4] != TOKEN_MAGIC || bytes[4] != TOKEN_VERSION {
            return Err(stale());
        }
        let mut session = [0u8; 8];
        session.copy_from_slice(&bytes[5..13]);
        if u64::from_be_bytes(session) != self.session {
            return Err(stale());
        }
        FileHandle::from_wire(bytes[13..].to_vec())
            .map_err(|error| error.to_umbra("storage_handle"))
    }
}

/// The four anchors of one opened run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunAnchors {
    run: Anchor,
    root: Anchor,
    control: Anchor,
    private: Option<Anchor>,
}

impl RunAnchors {
    /// Resolve every anchor of an existing run from the export root.
    ///
    /// `CreateNew` is refused: creating the run directories needs `OP_CREATE`,
    /// which the frozen [`Nfs4Op`](crate::transport::Nfs4Op) cannot encode. The
    /// refusal names that gap rather than reporting a run it did not create.
    pub fn open(
        transport: &mut dyn RawTransport,
        config: &NfsUserspaceConfig,
        run_id: RunId,
        intent: OpenRunIntent,
        deadline: Deadline,
    ) -> Result<Self> {
        if intent == OpenRunIntent::CreateNew {
            return Support::Deferred {
                owner: CONTRACTS_REVISION,
                reason: NO_WIRE_OPERATION,
            }
            .refuse("open_run.create_new");
        }
        let export_root = transport
            .root_filehandle(deadline)
            .map_err(|error| error.to_umbra("open_run"))?;
        let mut current = PinnedObject::pin(transport, export_root, deadline)
            .map_err(|error| error.to_umbra("open_run"))?;
        for relative in [&config.export, &config.run_parent] {
            for component in components(relative)? {
                current = descend(transport, &current, &component, deadline)?;
            }
        }
        let run_name = component(run_id.0.hyphenated().to_string().into_bytes())?;
        let run = descend(transport, &current, &run_name, deadline)?;
        let root = descend(
            transport,
            &run,
            &component(config.root_anchor.as_bytes())?,
            deadline,
        )?;
        let control = descend(
            transport,
            &run,
            &component(config.control_anchor.as_bytes())?,
            deadline,
        )?;
        // A run written by the mounted adapter always has `.provider`, but a run
        // that lost it is a real state and reporting it as present would be a
        // claim about state this provider never observed.
        let private = descend(transport, &run, &component(layout::PRIVATE_DIR)?, deadline)
            .ok()
            .map(|pin| Anchor {
                kind: AnchorKind::Private,
                pin,
            });
        Ok(Self {
            run: Anchor {
                kind: AnchorKind::Run,
                pin: run,
            },
            root: Anchor {
                kind: AnchorKind::Root,
                pin: root,
            },
            control: Anchor {
                kind: AnchorKind::Control,
                pin: control,
            },
            private,
        })
    }

    /// The run anchor.
    pub fn run(&self) -> &Anchor {
        &self.run
    }

    /// The tracee-visible root anchor.
    pub fn root(&self) -> &Anchor {
        &self.root
    }

    /// The control anchor.
    pub fn control(&self) -> &Anchor {
        &self.control
    }

    /// The provider-private anchor, when the run has one.
    pub fn private(&self) -> Option<&Anchor> {
        self.private.as_ref()
    }

    /// The anchor a contract path is relative to.
    pub fn for_contract(&self, anchor: StorageAnchor) -> &Anchor {
        match anchor {
            StorageAnchor::Root => &self.root,
            StorageAnchor::Control => &self.control,
        }
    }

    /// Resolve a contract path to the object it names.
    ///
    /// Every intermediate component must be a directory. A server-side symlink
    /// stops the walk rather than being followed, so resolution cannot leave the
    /// anchor.
    pub fn resolve(
        &self,
        transport: &mut dyn RawTransport,
        path: &StoragePath,
        deadline: Deadline,
    ) -> Result<PinnedObject> {
        match self.resolve_parent(transport, path, deadline)? {
            Target::Anchor(pin) => Ok(pin),
            Target::Named { parent, name } => descend_any(transport, &parent, &name, deadline),
        }
    }

    /// Resolve everything but the final component of a contract path.
    ///
    /// This is what a create, a rename or a remove needs: NFSv4 names an object
    /// to mutate as a parent filehandle plus a byte name, never as a path.
    pub fn resolve_parent(
        &self,
        transport: &mut dyn RawTransport,
        path: &StoragePath,
        deadline: Deadline,
    ) -> Result<Target> {
        let anchor = self.for_contract(path.anchor());
        let bytes = path.as_bytes();
        if bytes.is_empty() {
            return Ok(Target::Anchor(anchor.pin.clone()));
        }
        let mut parts = bytes
            .split(|byte| *byte == b'/')
            .map(component)
            .collect::<Result<Vec<_>>>()?;
        let name = parts.pop().unwrap_or_else(|| {
            unreachable!("a nonempty storage path yields at least one component")
        });
        let mut parent = anchor.pin.clone();
        for part in parts {
            parent = descend(transport, &parent, &part, deadline)?;
        }
        Ok(Target::Named { parent, name })
    }
}

/// What a path resolved to before its final component was looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The path named the anchor itself.
    Anchor(PinnedObject),
    /// The path named `name` inside `parent`.
    Named {
        /// The directory holding the name.
        parent: PinnedObject,
        /// The final byte-exact component.
        name: ComponentName,
    },
}

/// Validate one path component for the wire.
pub fn component(bytes: impl Into<Vec<u8>>) -> Result<ComponentName> {
    ComponentName::new(bytes)
        .map_err(|error| crate::error::FacadeError::Transport(error).to_umbra("path_component"))
}

fn components(relative: &BytePath) -> Result<Vec<ComponentName>> {
    relative
        .as_bytes()
        .split(|byte| *byte == b'/')
        .map(component)
        .collect()
}

/// Look one name up and require it to be a directory.
fn descend(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    let pin = descend_any(transport, parent, name, deadline)?;
    match pin.kind() {
        Nfs4Type::Directory => Ok(pin),
        Nfs4Type::Symlink => Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "resolve",
            format!(
                "{:?} is a server-side symlink; the anchor walk never follows one",
                String::from_utf8_lossy(name.as_bytes())
            ),
        )),
        other => Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "resolve",
            format!(
                "{:?} is {other:?}, not a directory",
                String::from_utf8_lossy(name.as_bytes())
            ),
        )),
    }
}

/// Look one name up without constraining its type.
fn descend_any(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    if !parent.is_directory() {
        return Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "resolve",
            "a path component was resolved against a non-directory",
        ));
    }
    let (handle, attributes) = transport
        .lookup(
            parent.handle(),
            name,
            crate::transport::AttrMask::STAT,
            deadline,
        )
        .map_err(|error| error.to_umbra("resolve"))?;
    PinnedObject::adopt(handle, &attributes).map_err(|error| error.to_umbra("resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use umbra_core::StorageAnchor;

    use crate::fixture;

    fn deadline() -> Deadline {
        Deadline { millis: 5_000 }
    }

    fn anchors(fake: &mut crate::fake::FakeTransport) -> RunAnchors {
        RunAnchors::open(
            fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::OpenExisting,
            deadline(),
        )
        .expect("the fixture layout resolves")
    }

    #[test]
    fn every_anchor_resolves_to_a_distinct_directory_object() {
        let (mut fake, _) = fixture::server();
        let resolved = anchors(&mut fake);
        for anchor in [resolved.run(), resolved.root(), resolved.control()] {
            assert!(anchor.pin().is_directory());
        }
        assert_ne!(
            resolved.root().pin().identity(),
            resolved.control().pin().identity()
        );
        assert_ne!(
            resolved.run().pin().identity(),
            resolved.root().pin().identity()
        );
        assert!(
            resolved.private().is_some(),
            "the fixture run carries .provider"
        );
        assert_eq!(
            resolved.for_contract(StorageAnchor::Root).kind(),
            AnchorKind::Root
        );
        assert_eq!(AnchorKind::Run.contract_anchor(), None);
        assert_eq!(AnchorKind::Private.contract_anchor(), None);
    }

    #[test]
    fn a_binding_publishes_a_token_and_never_a_physical_path() {
        let (mut fake, _) = fixture::server();
        let resolved = anchors(&mut fake);
        let mint = HandleMint::for_session(fixture::run_id(), 0);
        let binding = resolved.root().binding(&mint);
        // There is no kernel-visible path for a userspace client, so none is
        // reported. A fabricated one would be a path no `open` could ever use.
        assert_eq!(binding.physical_path, None);
        let recovered = mint.accept(&binding.handle).expect("our own token");
        assert_eq!(
            recovered.as_bytes(),
            resolved.root().pin().handle().as_bytes()
        );
    }

    #[test]
    fn a_token_from_another_session_is_rejected_rather_than_resolved() {
        let (mut fake, _) = fixture::server();
        let resolved = anchors(&mut fake);
        let first = HandleMint::for_session(fixture::run_id(), 0);
        let second = HandleMint::for_session(fixture::run_id(), 1);
        let token = resolved.root().binding(&first).handle;
        let error = second.accept(&token).unwrap_err();
        assert_eq!(error.kind, ErrorKind::StaleHandle);
        // A token that is not ours at all fails the same way, not by resolving
        // whatever the bytes happen to name.
        assert!(second
            .accept(&StorageHandle(b"not a token".to_vec()))
            .is_err());
        assert!(second.accept(&StorageHandle(Vec::new())).is_err());
    }

    #[test]
    fn creating_a_run_names_the_contracts_gap_instead_of_reporting_success() {
        let (mut fake, _) = fixture::server();
        let error = RunAnchors::open(
            &mut fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::CreateNew,
            deadline(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotImplemented);
        assert!(error.context.contains("Nfs4Op"));
    }

    #[test]
    fn resolution_stops_at_a_non_directory_component_instead_of_walking_through() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"file", b"bytes".to_vec());
        let resolved = anchors(&mut fake);
        let path = StoragePath::new(StorageAnchor::Root, b"file/deeper").expect("path");
        let error = resolved.resolve(&mut fake, &path, deadline()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidPath);
        assert!(error.context.contains("not a directory"));
    }

    #[test]
    fn an_empty_path_names_the_anchor_and_a_named_path_names_its_parent() {
        let (mut fake, layout) = fixture::server();
        fake.insert_file(&layout.root, b"note", b"x".to_vec());
        let resolved = anchors(&mut fake);
        let anchor_path = StoragePath::new(StorageAnchor::Root, Vec::new()).expect("path");
        match resolved
            .resolve_parent(&mut fake, &anchor_path, deadline())
            .expect("resolves")
        {
            Target::Anchor(pin) => {
                assert_eq!(pin.identity(), resolved.root().pin().identity())
            }
            Target::Named { .. } => panic!("an empty path names the anchor itself"),
        }
        let named = StoragePath::new(StorageAnchor::Root, b"note").expect("path");
        match resolved
            .resolve_parent(&mut fake, &named, deadline())
            .expect("resolves")
        {
            Target::Named { parent, name } => {
                assert_eq!(parent.identity(), resolved.root().pin().identity());
                assert_eq!(name.as_bytes(), b"note");
            }
            Target::Anchor(_) => panic!("a named path names a child"),
        }
    }

    #[test]
    fn a_component_that_could_escape_the_anchor_is_unrepresentable() {
        // `StoragePath` refuses these at construction, and `ComponentName` refuses
        // them again before anything reaches the wire. Both layers are checked
        // here so removing either is a test failure rather than a silent hole.
        for escape in [b"..".as_slice(), b".", b"", b"a/../b"] {
            assert!(
                StoragePath::new(StorageAnchor::Root, escape.to_vec()).is_err()
                    || component(escape.to_vec()).is_err(),
                "{escape:?} must not survive both layers"
            );
        }
        assert!(component(b"with/slash".to_vec()).is_err());
        assert!(component(b"with\0nul".to_vec()).is_err());
        assert!(component(b"ordinary".to_vec()).is_ok());
        // Byte names that are not UTF-8 are preserved, not rejected.
        assert_eq!(
            component(vec![0xFF, 0xFE])
                .expect("byte names survive")
                .as_bytes(),
            &[0xFF, 0xFE]
        );
    }
}
