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
    BytePath, ErrorKind, LeaseEpoch, OpenRunIntent, Result, RunId, RuntimeDirectoryBinding,
    StorageAnchor, StorageHandle, StoragePath, UmbraError,
};

use crate::handle::FileHandle;
use crate::identity::PinnedObject;
use crate::layout;
use crate::storage::NfsUserspaceConfig;
use crate::transport::{ComponentName, Deadline, Fsid, Nfs4Type, RawTransport};

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
    /// The one server filesystem every object in this run must live on.
    ///
    /// **R1-015.** Byte-path validation and symlink refusal do not establish
    /// filesystem containment: a `LOOKUP` into a nested export answers with a
    /// perfectly ordinary directory that happens to carry a different `fsid`.
    /// `docs/design/managed-lifecycle-spike.md:30` requires that crossing to be
    /// rejected on its own terms.
    filesystem: Fsid,
}

impl RunAnchors {
    /// Resolve every anchor of a run from the export root.
    ///
    /// `CreateNew` creates the run directory and its four children with
    /// `OP_CREATE`, which the authorised contracts hotfix added to
    /// [`Nfs4Op`](crate::transport::Nfs4Op). The layout it writes — `<run>/`,
    /// the two contract anchors and `.provider/` at
    /// [`layout::DIRECTORY_MODE`] — is the one the mounted adapter's goldens
    /// pin, so a run created here opens under the mounted adapter and the other
    /// way round.
    ///
    /// The export and run-parent directories are **not** created: they are
    /// deployment configuration, and creating a missing one would silently
    /// relocate every run rather than reporting a misconfigured export.
    pub fn open(
        transport: &mut dyn RawTransport,
        config: &NfsUserspaceConfig,
        run_id: RunId,
        intent: OpenRunIntent,
        deadline: Deadline,
    ) -> Result<Self> {
        let creating = intent == OpenRunIntent::CreateNew;
        let export_root = transport
            .root_filehandle(deadline)
            .map_err(|error| error.to_umbra("open_run"))?;
        let mut current = PinnedObject::pin(transport, export_root, deadline)
            .map_err(|error| error.to_umbra("open_run"))?;
        // R1-015: walking the *configured export* is the one transition that is
        // expected to change filesystem — the server's pseudo-root is its own
        // filesystem and the export is another. That crossing is deliberate and
        // configured, so it is allowed here and nowhere else.
        for component in components(&config.export)? {
            current = descend(transport, &current, &component, deadline)?;
        }
        // From the resolved export down, the filesystem is pinned. Everything
        // below — the run parent, the run, its anchors, and every path a caller
        // later resolves inside them — has to be on it.
        let filesystem = current.identity().fsid;
        for component in components(&config.run_parent)? {
            current = descend_within(transport, &current, &component, filesystem, deadline)?;
        }
        let run_name = component(run_id.0.hyphenated().to_string().into_bytes())?;
        let run = if creating {
            // GUARDED semantics: a run directory that already exists is a real
            // collision, not a resumption, because `CreateNew` asked for a run
            // nobody had written yet.
            if descend_any(transport, &current, &run_name, deadline).is_ok() {
                return Err(UmbraError::new(
                    ErrorKind::AlreadyExists,
                    "open_run",
                    format!("run {run_id:?} already exists under the configured run parent"),
                ));
            }
            within(
                create_directory(transport, &current, &run_name, deadline)?,
                filesystem,
                &run_name,
            )?
        } else {
            descend_within(transport, &current, &run_name, filesystem, deadline)?
        };
        let root = anchor_child(
            transport,
            &run,
            &component(config.root_anchor.as_bytes())?,
            creating,
            filesystem,
            deadline,
        )?;
        let control = anchor_child(
            transport,
            &run,
            &component(config.control_anchor.as_bytes())?,
            creating,
            filesystem,
            deadline,
        )?;
        let private_name = component(layout::PRIVATE_DIR)?;
        let private = if creating {
            Some(within(
                create_directory(transport, &run, &private_name, deadline)?,
                filesystem,
                &private_name,
            )?)
        } else {
            // A run written by the mounted adapter always has `.provider`, but a
            // run that lost it is a real state and reporting it as present would
            // be a claim about state this provider never observed.
            //
            // R1-011: only NOENT is that state. The old code was `.ok()`, which
            // turned EIO, ACCESS and STALE into "this run has no `.provider`" and
            // left the caller reading `session.rs`'s generic no-private-directory
            // message instead of the server's actual failure. A lookup that failed
            // is not an answer about what is there.
            match descend_within(transport, &run, &private_name, filesystem, deadline) {
                Ok(pin) => Some(pin),
                Err(error) if error.kind == ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        }
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
            filesystem,
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
            // R1-015: the final component is checked too. It is the one a caller
            // then reads, writes or mutates through, so letting it be the object
            // that crosses the boundary would defeat the whole walk.
            Target::Named { parent, name } => within(
                descend_any(transport, &parent, &name, deadline)?,
                self.filesystem,
                &name,
            ),
        }
    }

    /// The server filesystem every object in this run must live on (**R1-015**).
    pub fn filesystem(&self) -> Fsid {
        self.filesystem
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
            parent = descend_within(transport, &parent, &part, self.filesystem, deadline)?;
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

/// Write the provider-private state a newly created run must carry.
///
/// `RunAnchors::open` creates the directories; this creates what lives inside
/// `.provider`, because these are files and a file needs an OPEN sequenced
/// through the session's open owners while a directory needs only CREATE.
///
/// The names, modes and byte encodings are the mounted `nfs` adapter's, pinned by
/// `tests/goldens/`: a run this provider creates has to be one the mounted
/// adapter can later open, and the other way round. `writer.lock` is deliberately
/// **not** written here — admission creates it exclusively, and pre-creating it
/// would hand the next `GUARDED4` an existing name and turn every acquisition
/// into a collision.
pub fn provision_private_state(
    transport: &mut dyn RawTransport,
    owners: &mut crate::state::open_owner::OpenOwnerRegistry,
    private: &Anchor,
    run_id: RunId,
    immutable_base: &umbra_core::ImmutableBaseContract,
    format_version: u32,
    deadline: Deadline,
) -> Result<()> {
    create_directory_at(
        transport,
        private.pin().handle(),
        &component(layout::RETRIES_DIR)?,
        deadline,
    )?;
    // A little-endian u64, created as zero. The writer epoch this file records
    // is the mounted adapter's; admission keeps its own in the marker.
    create_file(
        transport,
        owners,
        private.pin(),
        &component(layout::EPOCH_FILE)?,
        &0u64.to_le_bytes(),
        deadline,
    )?;
    let manifest = serde_json::to_vec(&(run_id, immutable_base, format_version)).map_err(|e| {
        UmbraError::new(
            ErrorKind::InvalidState,
            "open_run",
            format!("the run manifest could not be encoded: {e}"),
        )
    })?;
    create_file(
        transport,
        owners,
        private.pin(),
        &component(layout::MANIFEST_FILE)?,
        &manifest,
        deadline,
    )
}

/// What a run's persisted `.provider` state says about its identity (**R1-005**).
///
/// Read before admission, because a run whose manifest names a different run, a
/// different immutable base or a different format version is not the run the
/// caller asked to open, and admitting it would bind a session to state it cannot
/// account for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedRunState {
    /// Run identity the manifest records.
    pub run_id: RunId,
    /// Immutable base the manifest records.
    pub immutable_base: umbra_core::ImmutableBaseContract,
    /// Format version the manifest records.
    pub format_version: u32,
    /// Writer epoch `.provider/epoch` records, zero when the run never had one.
    ///
    /// The mounted adapter increments this on acquisition, so a cleanly released
    /// legacy run carries the highest epoch it ever reached even though its lock
    /// is gone.
    pub epoch: LeaseEpoch,
}

/// Read and validate a run's persisted identity before admitting a session.
///
/// **R1-005.** `OpenExisting` used to validate only the caller's `format_version`
/// and read nothing at all from the run it was opening. A run whose manifest
/// carried another base fingerprint, another run id or another format version was
/// admitted anyway, and a missing or malformed manifest was indistinguishable
/// from a healthy one.
///
/// Every failure here is a refusal, never an inference: `docs/design/failure-model.md`
/// requires malformed or changed evidence to be preserved and refused, and a
/// missing file is never read as a release.
pub fn read_persisted_state(
    transport: &mut dyn RawTransport,
    private: &Anchor,
    expected_run: RunId,
    expected_base: &umbra_core::ImmutableBaseContract,
    expected_format: u32,
    deadline: Deadline,
) -> Result<PersistedRunState> {
    let manifest_bytes = read_private_file(
        transport,
        private,
        &component(layout::MANIFEST_FILE)?,
        deadline,
    )?
    .ok_or_else(|| {
        UmbraError::new(
            ErrorKind::CorruptJournal,
            "open_run",
            "the run has no .provider/manifest, so its identity cannot be proven; an \
             existing run without one is refused rather than adopted",
        )
    })?;

    let (run_id, immutable_base, format_version): (RunId, umbra_core::ImmutableBaseContract, u32) =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            UmbraError::new(
                ErrorKind::CorruptJournal,
                "open_run",
                format!(
                    "the run manifest does not decode ({error}); the bytes are retained on the \
                 server for inspection and the run is refused"
                ),
            )
        })?;

    if run_id != expected_run {
        return Err(UmbraError::new(
            ErrorKind::InvalidState,
            "open_run",
            format!(
                "the run directory's manifest records run {}, not the requested {}",
                run_id.0, expected_run.0
            ),
        ));
    }
    if format_version != expected_format {
        return Err(UmbraError::new(
            ErrorKind::ProtocolMismatch,
            "open_run",
            format!(
                "the run on the server was written at format version {format_version}, not \
                 the {expected_format} this request declares"
            ),
        ));
    }
    if immutable_base != *expected_base {
        return Err(UmbraError::new(
            ErrorKind::InvalidState,
            "open_run",
            format!(
                "the run was created over immutable base {:?}, not the {:?} this request \
                 declares; the run is refused rather than rebound to a different base",
                immutable_base.identity, expected_base.identity
            ),
        ));
    }

    // `.provider/epoch` is the mounted adapter's little-endian u64.
    //
    // **R2-004.** Absence used to mean `LeaseEpoch(0)`, justified by a comment
    // about older runs written before the file existed. Nothing establishes that
    // case: this provider's own `CreateNew` always writes the file, the mounted
    // adapter's `open_run` does too, and the pinned run layout lists it. So an
    // existing format-1 run without one is missing required recovery evidence,
    // and reading that as "this run never had a writer" is the same class of
    // inference the failure model forbids for a missing marker — it would let a
    // run whose epoch file was deleted after a cooperative release be re-admitted
    // at epoch 1, silently below the ladder it had already reached.
    //
    // Refusing is the answer. The bytes that remain are preserved for an operator.
    let epoch = match read_private_file(
        transport,
        private,
        &component(layout::EPOCH_FILE)?,
        deadline,
    )? {
        None => {
            return Err(UmbraError::new(
                ErrorKind::CorruptJournal,
                "open_run",
                format!(
                    "this run has a valid format-{expected_format} manifest but no \
                     .provider/epoch; every run this provider or the mounted adapter creates \
                     writes that file, so its absence is missing recovery evidence rather than \
                     a run that never had a writer. The run is refused rather than admitted at \
                     epoch 1, which could regress an epoch ladder it already reached."
                ),
            ))
        }
        Some(bytes) => {
            let raw: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                UmbraError::new(
                    ErrorKind::CorruptJournal,
                    "open_run",
                    format!(
                        "the run's .provider/epoch is {} bytes, not the 8 a little-endian u64 \
                         occupies; the recorded writer epoch cannot be read and the run is \
                         refused rather than restarted at epoch 1",
                        bytes.len()
                    ),
                )
            })?;
            LeaseEpoch(u64::from_le_bytes(raw))
        }
    };

    Ok(PersistedRunState {
        run_id,
        immutable_base,
        format_version,
        epoch,
    })
}

/// Read one whole file from `.provider`, or `None` when the name is absent.
///
/// Absence is `NFS4ERR_NOENT` and nothing else: a lookup or read that *failed* is
/// propagated, because reporting it as absence is the R1-011 defect in another
/// place.
fn read_private_file(
    transport: &mut dyn RawTransport,
    private: &Anchor,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<Option<Vec<u8>>> {
    /// Bound on the private files this reads. Both are small and fixed-shape; a
    /// larger file is refused rather than streamed.
    const MAX_PRIVATE_FILE_BYTES: u32 = 64 * 1024;

    let pinned = match descend_any(transport, private.pin(), name, deadline) {
        Ok(pinned) => pinned,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let reply =
        crate::crud::read_anonymous(transport, &pinned, 0, MAX_PRIVATE_FILE_BYTES, deadline)
            .map_err(|error| error.to_umbra("open_run"))?;
    Ok(Some(reply.data))
}

/// `GUARDED4` create one file and write its whole contents.
fn create_file(
    transport: &mut dyn RawTransport,
    owners: &mut crate::state::open_owner::OpenOwnerRegistry,
    parent: &PinnedObject,
    name: &ComponentName,
    bytes: &[u8],
    deadline: Deadline,
) -> Result<()> {
    use crate::crud::{CreateDisposition, OpenObject};
    use crate::state::open_owner::CloseOutcome;
    use crate::transport::{ShareAccess, Stability};

    let open = OpenObject::open(
        owners,
        transport,
        parent,
        name,
        CreateDisposition::CreateNew { mode: 0o600 },
        ShareAccess::WRITE,
        deadline,
    )
    .map_err(|error| error.to_umbra("open_run"))?;
    let stateid = match open.stateid() {
        Ok(stateid) => stateid,
        Err(error) => {
            let _ = open.close(transport, deadline);
            return Err(error.to_umbra("open_run"));
        }
    };
    // `FILE_SYNC` because this is run-identifying state a later reader must find,
    // and this path issues no COMMIT of its own. It is a request for stability,
    // not a durability claim: the provider still advertises `Durability::None`.
    let written = transport.write(
        open.handle(),
        stateid,
        0,
        Stability::FileSync,
        bytes.to_vec(),
        deadline,
    );
    let result = match written {
        // A short write here would leave a truncated manifest that decodes as
        // nothing, so the count is checked rather than assumed.
        Ok(reply) if reply.count as usize == bytes.len() => Ok(()),
        Ok(reply) => Err(UmbraError::new(
            ErrorKind::Io,
            "open_run",
            format!(
                "the server accepted {} of {} bytes of {}",
                reply.count,
                bytes.len(),
                String::from_utf8_lossy(name.as_bytes())
            ),
        )),
        Err(error) => Err(error.to_umbra("open_run")),
    };
    match open.close(transport, deadline) {
        CloseOutcome::Closed(_) => result,
        // A CLOSE the server refused leaves the open live and its outcome
        // unknown; the write's own error still wins when there is one.
        CloseOutcome::Rejected { error, .. } | CloseOutcome::Abandoned { error } => {
            result.and(Err(error.to_umbra("open_run")))
        }
    }
}

/// Resolve an anchor child, creating it when the run is being created.
fn anchor_child(
    transport: &mut dyn RawTransport,
    run: &PinnedObject,
    name: &ComponentName,
    creating: bool,
    filesystem: Fsid,
    deadline: Deadline,
) -> Result<PinnedObject> {
    if creating {
        within(
            create_directory(transport, run, name, deadline)?,
            filesystem,
            name,
        )
    } else {
        descend_within(transport, run, name, filesystem, deadline)
    }
}

/// Look one name up, require a directory, and require it to be on `filesystem`.
///
/// **R1-015.** The fsid comparison is what makes containment a property of the
/// walk rather than of the path bytes. A `LOOKUP` that resolves into a nested
/// exported filesystem returns an ordinary directory; without this check it was
/// adopted and used for further reads and mutations.
fn descend_within(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    filesystem: Fsid,
    deadline: Deadline,
) -> Result<PinnedObject> {
    within(
        descend(transport, parent, name, deadline)?,
        filesystem,
        name,
    )
}

/// Reject a pin that is not on the run's pinned filesystem (**R1-015**).
fn within(pin: PinnedObject, filesystem: Fsid, name: &ComponentName) -> Result<PinnedObject> {
    within_labelled(
        pin,
        filesystem,
        &String::from_utf8_lossy(name.as_bytes()),
        "resolve",
    )
}

/// Reject a pin that is not on `filesystem`, naming it however the caller wants.
///
/// **R2-007.** The resolver was not the only place an object gets adopted:
/// `Operations::child` (which `create_parents` walks through) and the OPEN that
/// `WriteAt` and `Create` finish with both produce pins that never reached
/// `resolve`. A boundary enforced on one path is not a boundary, so this is the
/// shared check every adoption point calls.
pub fn within_labelled(
    pin: PinnedObject,
    filesystem: Fsid,
    label: &str,
    operation: &str,
) -> Result<PinnedObject> {
    let observed = pin.identity().fsid;
    if observed != filesystem {
        return Err(UmbraError::new(
            ErrorKind::InvalidPath,
            operation,
            format!(
                "{label:?} is on filesystem {}:{}, not the run's {}:{}; the walk does not cross \
                 into another exported filesystem",
                observed.major, observed.minor, filesystem.major, filesystem.minor,
            ),
        ));
    }
    Ok(pin)
}

/// `PUTFH; CREATE NF4DIR; GETFH; GETATTR`: make one directory and pin it.
///
/// The pin is taken from the CREATE reply's own GETFH and GETATTR rather than by
/// looking the name up again, so the anchor is the object this call created and
/// not whatever the name resolves to a moment later.
fn create_directory(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    create_directory_at(transport, parent.handle(), name, deadline)
}

/// The same, addressed by filehandle rather than by pin.
fn create_directory_at(
    transport: &mut dyn RawTransport,
    parent: &FileHandle,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    use crate::transport::{AttrValues, Compound, CreateType, Nfs4Op, OpReply};

    let reply = transport
        .submit(
            Compound::new(
                *b"mkanchor",
                vec![
                    Nfs4Op::PutFh(parent.clone()),
                    Nfs4Op::Create {
                        object_type: CreateType::Directory,
                        name: name.clone(),
                        attributes: AttrValues {
                            mode: Some(layout::DIRECTORY_MODE),
                            ..AttrValues::default()
                        },
                    },
                    Nfs4Op::GetFh,
                ],
            ),
            deadline,
        )
        .map_err(|error| crate::error::FacadeError::Transport(error).to_umbra("open_run"))?;
    match reply.expect(1).map_err(|e| e.to_umbra("open_run"))? {
        OpReply::Create { .. } => {}
        other => {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "open_run",
                format!("expected a CREATE reply, got {:?}", other.opcode()),
            ))
        }
    }
    let handle = match reply.expect(2).map_err(|e| e.to_umbra("open_run"))? {
        OpReply::GetFh(handle) => handle.clone(),
        other => {
            return Err(UmbraError::new(
                ErrorKind::ProtocolMismatch,
                "open_run",
                format!("expected a GETFH reply, got {:?}", other.opcode()),
            ))
        }
    };
    PinnedObject::pin(transport, handle, deadline).map_err(|error| error.to_umbra("open_run"))
}

/// Look one name up without constraining its type.
/// Look one name up and require it to be a directory, without a boundary check.
///
/// Used for `.provider` children, which are resolved from an anchor that has
/// already been proven on the run's filesystem.
pub fn descend_directory(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    descend(transport, parent, name, deadline)
}

/// Look one name up and pin whatever it is.
pub fn descend_file(
    transport: &mut dyn RawTransport,
    parent: &PinnedObject,
    name: &ComponentName,
    deadline: Deadline,
) -> Result<PinnedObject> {
    descend_any(transport, parent, name, deadline)
}

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
    use crate::fake::FakeTransport;
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
    fn creating_a_run_writes_the_layout_the_mounted_adapter_reads() {
        // An empty run parent: only the export and run-parent directories exist,
        // which are deployment configuration this call must not create.
        let mut fake = FakeTransport::new();
        let mut current = fake.root();
        for part in fixture::EXPORT.split(|byte| *byte == b'/') {
            current = fake.insert_directory(&current, part);
        }
        fake.insert_directory(&current, fixture::RUN_PARENT);

        let anchors = RunAnchors::open(
            &mut fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::CreateNew,
            deadline(),
        )
        .expect("CreateNew builds the run layout");

        // Every anchor is a distinct directory object, and `.provider` exists,
        // because admission has nowhere to record a marker without it.
        assert_eq!(anchors.run().kind(), AnchorKind::Run);
        assert!(anchors.private().is_some());
        let ids = [
            anchors.run().pin().identity().fileid,
            anchors.root().pin().identity().fileid,
            anchors.control().pin().identity().fileid,
            anchors.private().expect("private").pin().identity().fileid,
        ];
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "anchors must be distinct objects");

        // The run it created is the run a later OpenExisting resolves.
        let reopened = RunAnchors::open(
            &mut fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::OpenExisting,
            deadline(),
        )
        .expect("the created run reopens");
        assert_eq!(
            reopened.root().pin().identity(),
            anchors.root().pin().identity()
        );
    }

    #[test]
    fn creating_a_run_that_already_exists_is_a_collision_not_a_resumption() {
        let (mut fake, _) = fixture::server();
        let error = RunAnchors::open(
            &mut fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::CreateNew,
            deadline(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::AlreadyExists);
    }

    #[test]
    fn creating_a_run_under_a_missing_export_is_refused_rather_than_relocated() {
        // The export directory is deployment configuration. Creating it would
        // silently move every run to a path nobody configured.
        let mut fake = FakeTransport::new();
        let error = RunAnchors::open(
            &mut fake,
            &fixture::config(),
            fixture::run_id(),
            OpenRunIntent::CreateNew,
            deadline(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound);
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
