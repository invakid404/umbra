use super::{Journal, NamespaceResolver, NamespaceSession, Storage};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use umbra_core::*;

fn error(kind: ErrorKind, message: &str) -> UmbraError {
    UmbraError::new(kind, "overlay", message)
}
fn unsupported(message: &str) -> UmbraError {
    error(ErrorKind::UnsupportedCapability, message)
}
fn absent<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn root(bytes: &[u8]) -> Result<StoragePath> {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec())
}
fn logical(path: &StoragePath) -> Result<BytePath> {
    let mut bytes = vec![b'/'];
    bytes.extend_from_slice(path.as_bytes());
    BytePath::new(bytes)
}
fn join(path: &StoragePath, name: &[u8]) -> Result<StoragePath> {
    let mut bytes = path.as_bytes().to_vec();
    if !bytes.is_empty() {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name);
    StoragePath::new(path.anchor(), bytes)
}
fn physical(binding: &RuntimeDirectoryBinding, path: &StoragePath) -> Result<PhysicalPath> {
    let mut bytes = binding
        .physical_path
        .as_ref()
        .ok_or_else(|| unsupported("backend has no kernel path binding"))?
        .as_bytes()
        .to_vec();
    if bytes.first() != Some(&b'/') {
        return Err(error(
            ErrorKind::InvalidPath,
            "runtime binding is not absolute",
        ));
    }
    if !path.as_bytes().is_empty() {
        if bytes.last() != Some(&b'/') {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(path.as_bytes());
    }
    Ok(PhysicalPath(BytePath::new(bytes)?))
}

/// Maximum number of logical symlinks expanded by one path lookup.
pub const MAX_SYMLINK_EXPANSIONS: usize = 40;

fn symlink_target(object: ObjectId) -> Result<StoragePath> {
    StoragePath::new(
        StorageAnchor::Control,
        format!("symlinks/targets/{}", object.0).into_bytes(),
    )
}
fn symlink_index(object: ObjectId) -> Result<StoragePath> {
    StoragePath::new(
        StorageAnchor::Control,
        format!("symlinks/objects/{}", object.0).into_bytes(),
    )
}
fn read_control(
    storage: &mut dyn Storage,
    context: &RequestContext,
    path: &StoragePath,
) -> Result<Option<Vec<u8>>> {
    let Some(stat) = absent(storage.stat(context, path))? else {
        return Ok(None);
    };
    if stat.kind != ObjectKind::File || stat.len > MAX_IO_BYTES as u64 {
        return Err(error(ErrorKind::CorruptJournal, "invalid symlink metadata"));
    }
    let max = (storage.capabilities().max_io_bytes as usize).min(MAX_IO_BYTES);
    if max == 0 {
        return Err(error(ErrorKind::ProtocolMismatch, "zero storage I/O limit"));
    }
    let mut bytes = vec![0; stat.len as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        let end = bytes.len().min(offset + max);
        let count = storage.read_at(context, path, offset as u64, &mut bytes[offset..end])?;
        if count == 0 {
            return Err(error(
                ErrorKind::CorruptJournal,
                "truncated symlink metadata",
            ));
        }
        offset += count;
    }
    Ok(Some(bytes))
}
fn logical_stat(
    storage: &mut dyn Storage,
    context: &RequestContext,
    mut stat: BlobStat,
) -> Result<BlobStat> {
    if stat.kind == ObjectKind::File {
        if let Some(index) = read_control(storage, context, &symlink_index(stat.object_id)?)? {
            let id = std::str::from_utf8(&index)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    error(
                        ErrorKind::CorruptJournal,
                        "invalid logical symlink identity",
                    )
                })?;
            stat.object_id = ObjectId(id);
            let target = read_control(storage, context, &symlink_target(stat.object_id)?)?
                .ok_or_else(|| error(ErrorKind::CorruptJournal, "missing symlink target"))?;
            BytePath::new(target.clone())?;
            stat.kind = ObjectKind::LogicalSymlink;
            stat.len = target.len() as u64;
            stat.mode = 0o777;
        }
    }
    Ok(stat)
}

/// Injected ABI boundary for stat of a logical symlink placeholder.
/// The adapter binds the stopped syscall's output buffer and native stat layout.
pub trait StatEncoder: Send {
    /// Encode logical metadata, never the placeholder's native file kind or size.
    fn encode(
        &mut self,
        context: &ProcessContext,
        operation: &FsOp,
        stat: &BlobStat,
    ) -> Result<EmulatedResult>;
}

/// Read-only access to a supervisor-approved immutable base; no arbitrary host paths.
/// Implementations must reject kernel symlink traversal and keep object bytes stable.
/// Final logical symlinks may be exposed through stat/read_link for overlay expansion.
pub trait Base: Send {
    /// Inspect an anchored logical object, returning NotFound for absence.
    fn stat(&mut self, path: &StoragePath) -> Result<BlobStat>;
    /// Read a logical target without following the final component.
    fn read_link(&mut self, _path: &StoragePath) -> Result<BytePath> {
        Err(unsupported("base does not expose logical symlink targets"))
    }
    /// Read bounded file bytes at an explicit offset.
    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize>;
    /// Enumerate a bounded page of immutable names.
    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage>;
    /// Resolve a validated object to a runtime kernel read target.
    fn physical_path(&self, path: &StoragePath) -> Result<PhysicalPath>;
}

/// Native directory response encoded by the caller's ABI adapter.
pub struct EncodedDirectory {
    /// Emulated native return value and writes to the current syscall's output buffer.
    pub result: EmulatedResult,
    /// Number of whole directory entries consumed from the supplied snapshot.
    pub consumed: usize,
}

/// Injected ABI boundary for ReadDir, whose core FsOp does not carry an output address.
/// The adapter must bind the stopped syscall's buffer and encode native dirent records.
pub trait DirectoryEncoder: Send {
    /// Encode a prefix of entries within the operation's max_bytes bound.
    fn encode(
        &mut self,
        context: &ProcessContext,
        operation: &FsOp,
        entries: &[DirectoryEntry],
    ) -> Result<EncodedDirectory>;
}

type DirectoryKey = (TaskId, u64, TracedFd, ObjectId);

/// Adapter for an already-open immutable base supplied through the Storage contract.
/// The caller must freeze this run for the lifetime of the overlay.
pub struct StorageBase {
    storage: Box<dyn Storage>,
    context: RequestContext,
    binding: RunBinding,
}
impl StorageBase {
    /// Wrap an approved immutable run without acquiring authority or performing I/O.
    pub fn new(
        storage: Box<dyn Storage>,
        binding: RunBinding,
        context: RequestContext,
    ) -> Result<Self> {
        if binding.run_id != context.run_id {
            return Err(error(ErrorKind::InvalidInput, "base run mismatch"));
        }
        Ok(Self {
            storage,
            context,
            binding,
        })
    }
}
impl Base for StorageBase {
    fn stat(&mut self, path: &StoragePath) -> Result<BlobStat> {
        let stat = self.storage.stat(&self.context, path)?;
        logical_stat(self.storage.as_mut(), &self.context, stat)
    }
    fn read_link(&mut self, path: &StoragePath) -> Result<BytePath> {
        let stat = self.stat(path)?;
        if stat.kind != ObjectKind::LogicalSymlink {
            return Err(error(
                ErrorKind::InvalidInput,
                "readlink target is not a symlink",
            ));
        }
        if let Some(target) = read_control(
            self.storage.as_mut(),
            &self.context,
            &symlink_target(stat.object_id)?,
        )? {
            return BytePath::new(target);
        }
        match self.storage.execute(&StorageRequest {
            context: self.context.clone(),
            operation: StorageOperation::ReadLink { path: path.clone() },
        })? {
            StorageResponse::ReadLink(target) => Ok(target),
            _ => Err(error(
                ErrorKind::ProtocolMismatch,
                "invalid base readlink response",
            )),
        }
    }
    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize> {
        let len = out
            .len()
            .min(self.storage.capabilities().max_io_bytes as usize);
        self.storage
            .read_at(&self.context, path, offset, &mut out[..len])
    }
    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        self.storage.list(
            &self.context,
            path,
            cursor,
            limit.min(self.storage.capabilities().max_directory_entries),
        )
    }
    fn physical_path(&self, path: &StoragePath) -> Result<PhysicalPath> {
        physical(&self.binding.root, path)
    }
}

/// Runtime authority supplied by the owner of already-open storage and journal sessions.
/// The journal must have been opened against `binding.control` with this writer epoch.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Current shadow/control binding; physical paths never enter journal payloads.
    pub binding: RunBinding,
    /// Matching fenced writer and unique session idempotency prefix.
    pub context: RequestContext,
    /// Validated journal recovery inventory from the owner of the journal session.
    pub recovery: RecoveryState,
    /// The writer lease this session mutates under. It is kept next to the
    /// storage session that issued it so renewal, release and mutation share one
    /// owner; nothing else may hold a second mutable session for this run.
    pub lease: WriterLease,
}

#[derive(Clone)]
struct Plan {
    action: ResolvedAction,
    operation: FsOp,
    path: StoragePath,
    destination: Option<StoragePath>,
    mutation: bool,
    directory_next: Option<(DirectoryKey, Vec<DirectoryEntry>)>,
}
struct Pending {
    id: OperationId,
    plan: Plan,
    outcome: Option<OperationOutcome>,
    whiteouts: Vec<(StoragePath, bool)>,
    retired_index: Option<StoragePath>,
}

/// Standard storage-independent namespace engine. Construction performs no I/O.
/// Call NamespaceSession::bind before use and serialize resolve/prepare/observe/commit.
pub struct Overlay {
    storage: Box<dyn Storage>,
    journal: Box<dyn Journal>,
    config: Option<SessionConfig>,
    base: Option<Box<dyn Base>>,
    planned: Option<Plan>,
    pending: Option<Pending>,
    poisoned: bool,
    serial: u64,
    last_committed: Sequence,
    pages: BTreeMap<Vec<u8>, (StoragePath, Vec<DirectoryEntry>)>,
    session: u64,
    directory_encoder: Option<Box<dyn DirectoryEncoder>>,
    directories: BTreeMap<DirectoryKey, Vec<DirectoryEntry>>,
    used_operations: BTreeSet<OperationId>,
    readlink_buffer: Option<(u64, u32)>,
    stat_encoder: Option<Box<dyn StatEncoder>>,
}
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
impl Overlay {
    /// Inject contracts without selecting, opening, or acquiring a backend.
    pub fn new(storage: Box<dyn Storage>, journal: Box<dyn Journal>) -> Self {
        Self {
            storage,
            journal,
            config: None,
            base: None,
            planned: None,
            pending: None,
            poisoned: false,
            serial: 0,
            last_committed: Sequence(0),
            pages: BTreeMap::new(),
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            directory_encoder: None,
            directories: BTreeMap::new(),
            used_operations: BTreeSet::new(),
            readlink_buffer: None,
            stat_encoder: None,
        }
    }
    /// Report the injected storage capabilities without claiming qualification.
    pub fn storage_capabilities(&self) -> StorageCapabilities {
        self.storage.capabilities()
    }
    /// Return ownership without flushing, closing, or asserting clean handoff.
    pub fn into_backends(self) -> (Box<dyn Storage>, Box<dyn Journal>) {
        (self.storage, self.journal)
    }
    fn config(&self) -> Result<&SessionConfig> {
        self.config
            .as_ref()
            .ok_or_else(|| error(ErrorKind::InvalidState, "namespace is not bound"))
    }
    fn idle(&self) -> Result<()> {
        self.config()?;
        if self.poisoned || self.pending.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "transaction pending or recovery required",
            ));
        }
        Ok(())
    }
    fn context(&mut self) -> Result<RequestContext> {
        let mut context = self.config()?.context.clone();
        self.serial += 1;
        // One logical transaction issues several storage requests: a copy-up, its
        // parent directories, the create itself. A backend may bind an operation
        // ID to the one idempotency key it was first used with and refuse a
        // second, different request under it, so each request gets its own
        // derived identity. The transaction's own ID still seeds the derivation,
        // keeping requests attributable to the operation the journal recorded.
        let seed = self.pending.as_ref().map_or(context.operation_id, |p| p.id);
        context.operation_id = seed.derive(self.session, self.serial);
        context.idempotency_key.0 = format!(
            "{}/{}/{}",
            context.idempotency_key.0, self.session, self.serial
        );
        Ok(context)
    }
    fn base(&mut self) -> &mut dyn Base {
        self.base.as_mut().expect("bound base").as_mut()
    }
    fn shadow_stat(&mut self, path: &StoragePath) -> Result<Option<BlobStat>> {
        let context = self.context()?;
        absent(self.storage.stat(&context, path))
    }
    // Hex-encode each byte component, splitting long encodings to stay below NAME_MAX.
    // A terminal .wh distinguishes a whiteout from directory prefixes.
    fn marker(path: &StoragePath) -> Result<StoragePath> {
        let mut bytes = b"whiteouts".to_vec();
        for component in path
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
        {
            let encoded: String = component.iter().map(|b| format!("{b:02x}")).collect();
            bytes.extend_from_slice(b"/c");
            for chunk in encoded.as_bytes().chunks(120) {
                bytes.push(b'/');
                bytes.extend_from_slice(chunk);
            }
        }
        bytes.extend_from_slice(b"/.wh");
        StoragePath::new(StorageAnchor::Control, bytes)
    }
    fn whiteouted(&mut self, path: &StoragePath) -> Result<bool> {
        let mut prefix = root(b"")?;
        for component in path
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
        {
            prefix = join(&prefix, component)?;
            let marker = Self::marker(&prefix)?;
            let context = self.context()?;
            if absent(self.storage.stat(&context, &marker))?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn lookup(&mut self, path: &StoragePath) -> Result<(BlobStat, bool)> {
        if path.anchor() != StorageAnchor::Root {
            return Err(error(ErrorKind::Denied, "control is not tracee-visible"));
        }
        let found = if let Some(stat) = self.shadow_stat(path)? {
            let context = self.context()?;
            (logical_stat(self.storage.as_mut(), &context, stat)?, true)
        } else {
            if self.whiteouted(path)? {
                return Err(error(ErrorKind::NotFound, "logical path is whiteouted"));
            }
            (self.base().stat(path)?, false)
        };
        Ok(found)
    }
    fn directory(&mut self, path: &StoragePath) -> Result<()> {
        if self.lookup(path)?.0.kind != ObjectKind::Directory {
            return Err(error(
                ErrorKind::InvalidPath,
                "path component is not a directory",
            ));
        }
        Ok(())
    }
    /// Resolve byte components from logical cwd or a tracked directory descriptor.
    /// Parent traversal above ProcessContext::root fails, including absolute paths.
    pub fn resolve_path(
        &mut self,
        context: &ProcessContext,
        dir: DirRef,
        path: &BytePath,
        create_parents: bool,
    ) -> Result<StoragePath> {
        self.resolve_path_follow(context, dir, path, create_parents, true)
    }
    fn resolve_path_follow(
        &mut self,
        context: &ProcessContext,
        dir: DirRef,
        path: &BytePath,
        create_parents: bool,
        follow_final: bool,
    ) -> Result<StoragePath> {
        self.idle()?;
        if path.as_bytes().is_empty() {
            return Err(error(ErrorKind::NotFound, "empty path"));
        }
        let logical_root = absolute_components(&context.root)?;
        // The process root is a trusted canonical boundary, never a link alias.
        for n in 0..=logical_root.len() {
            self.directory(&root(&logical_root[..n].join(&b'/'))?)?;
        }
        let mut expansions = 0;
        let parts = if path.as_bytes().first() == Some(&b'/') {
            logical_root.clone()
        } else {
            let (anchor, expected) = match dir {
                DirRef::Cwd => (context.cwd.clone(), None),
                DirRef::Fd(fd) => {
                    let state = context
                        .fds
                        .get(&fd)
                        .ok_or_else(|| error(ErrorKind::StaleHandle, "unknown dirfd"))?;
                    if !state.directory {
                        return Err(error(ErrorKind::InvalidPath, "dirfd is not a directory"));
                    }
                    let name = state.logical_path.clone().ok_or_else(|| {
                        error(ErrorKind::StaleHandle, "dirfd has no logical anchor")
                    })?;
                    (name, Some(state.object))
                }
            };
            let parts = absolute_components(&anchor)?;
            if !parts.starts_with(&logical_root) {
                return Err(error(ErrorKind::InvalidPath, "anchor outside logical root"));
            }
            let suffix = if parts.len() == logical_root.len() {
                BytePath::new(b".".to_vec())?
            } else {
                BytePath::new(parts[logical_root.len()..].join(&b'/'))?
            };
            let anchored = self.walk(
                logical_root.clone(),
                &logical_root,
                &suffix,
                false,
                true,
                &mut expansions,
            )?;
            self.directory(&anchored)?;
            if let Some(object) = expected {
                if self.lookup(&anchored)?.0.object_id != object {
                    return Err(error(ErrorKind::StaleHandle, "dirfd identity changed"));
                }
            }
            absolute_components(&logical(&anchored)?)?
        };
        self.walk(
            parts,
            &logical_root,
            path,
            create_parents,
            follow_final,
            &mut expansions,
        )
    }
    fn walk(
        &mut self,
        mut parts: Vec<Vec<u8>>,
        logical_root: &[Vec<u8>],
        path: &BytePath,
        create_parents: bool,
        follow_final: bool,
        expansions: &mut usize,
    ) -> Result<StoragePath> {
        let mut queue: VecDeque<Vec<u8>> = path
            .as_bytes()
            .split(|b| *b == b'/')
            .map(<[u8]>::to_vec)
            .collect();
        while let Some(component) = queue.pop_front() {
            match component.as_slice() {
                b"" | b"." => {}
                b".." => {
                    if parts.len() == logical_root.len() {
                        return Err(error(ErrorKind::InvalidPath, "parent escapes logical root"));
                    }
                    parts.pop();
                }
                name => {
                    parts.push(name.to_vec());
                    let prefix = root(&parts.join(&b'/'))?;
                    if let Some((stat, shadow)) = absent(self.lookup(&prefix))? {
                        if stat.kind == ObjectKind::LogicalSymlink
                            && (follow_final || !queue.is_empty())
                        {
                            *expansions += 1;
                            if *expansions > MAX_SYMLINK_EXPANSIONS {
                                return Err(error(
                                    ErrorKind::InvalidPath,
                                    "symlink expansion limit exceeded",
                                ));
                            }
                            let target = self.target(&prefix, &stat, shadow)?;
                            if target.as_bytes().is_empty() {
                                return Err(error(ErrorKind::NotFound, "empty symlink target"));
                            }
                            parts.pop();
                            if target.as_bytes().first() == Some(&b'/') {
                                parts = logical_root.to_vec();
                            }
                            for part in target.as_bytes().split(|b| *b == b'/').rev() {
                                queue.push_front(part.to_vec());
                            }
                            continue;
                        }
                    }
                }
            }
            if !queue.is_empty() {
                let prefix = root(&parts.join(&b'/'))?;
                match self.directory(&prefix) {
                    Err(e) if create_parents && e.kind == ErrorKind::NotFound => {}
                    other => other?,
                }
            }
        }
        root(&parts.join(&b'/'))
    }
    fn typed_path(&mut self, path: &StoragePath, follow: bool) -> Result<StoragePath> {
        self.idle()?;
        if path.anchor() != StorageAnchor::Root {
            return Err(error(ErrorKind::Denied, "control is not tracee-visible"));
        }
        self.directory(&root(b"")?)?;
        self.walk(vec![], &[], &logical(path)?, false, follow, &mut 0)
    }
    fn target(&mut self, path: &StoragePath, stat: &BlobStat, shadow: bool) -> Result<BytePath> {
        if stat.kind != ObjectKind::LogicalSymlink {
            return Err(error(
                ErrorKind::InvalidInput,
                "readlink target is not a symlink",
            ));
        }
        if !shadow {
            return self.base().read_link(path);
        }
        let context = self.context()?;
        let bytes = read_control(
            self.storage.as_mut(),
            &context,
            &symlink_target(stat.object_id)?,
        )?
        .ok_or_else(|| error(ErrorKind::CorruptJournal, "missing symlink target"))?;
        BytePath::new(bytes)
    }
    fn write_control(&mut self, path: &StoragePath, bytes: &[u8]) -> Result<()> {
        self.create(path, CreateKind::File, 0o600)?;
        let max = self.storage.capabilities().max_io_bytes as usize;
        if max == 0 {
            return Err(error(ErrorKind::ProtocolMismatch, "zero storage I/O limit"));
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let context = self.context()?;
            let end = bytes.len().min(offset + max.min(MAX_IO_BYTES));
            let count =
                self.storage
                    .write_at(&context, path, offset as u64, &bytes[offset..end])?;
            if count == 0 {
                return Err(error(ErrorKind::Io, "metadata write made no progress"));
            }
            offset += count;
        }
        Ok(())
    }
    fn create_symlink(
        &mut self,
        path: &StoragePath,
        target: &BytePath,
        object: ObjectId,
    ) -> Result<()> {
        let metadata = symlink_target(object)?;
        let context = self.context()?;
        match read_control(self.storage.as_mut(), &context, &metadata)? {
            Some(existing) if existing != target.as_bytes() => {
                return Err(error(
                    ErrorKind::CorruptJournal,
                    "conflicting logical symlink identity",
                ))
            }
            Some(_) => {}
            None => self.write_control(&metadata, target.as_bytes())?,
        }
        self.create(path, CreateKind::File, 0o444)?;
        let backend = self
            .shadow_stat(path)?
            .expect("created placeholder")
            .object_id;
        self.write_control(&symlink_index(backend)?, object.0.to_string().as_bytes())
    }
    fn remove_symlink_index(&mut self, path: &StoragePath) -> Result<()> {
        if let Some(stat) = self.shadow_stat(path)? {
            let index = symlink_index(stat.object_id)?;
            if self.shadow_stat(&index)?.is_some() {
                let context = self.context()?;
                self.storage.unlink(&context, &index)?;
            }
        }
        Ok(())
    }
    fn parents(&mut self, path: &StoragePath) -> Result<()> {
        let parts: Vec<_> = path.as_bytes().split(|b| *b == b'/').collect();
        for n in 1..parts.len() {
            let parent = StoragePath::new(path.anchor(), parts[..n].join(&b'/'))?;
            if let Some(stat) = self.shadow_stat(&parent)? {
                if stat.kind != ObjectKind::Directory {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        "shadow parent is not a directory",
                    ));
                }
            } else {
                let context = self.context()?;
                self.storage.create(
                    &context,
                    &parent,
                    &CreateOptions {
                        kind: CreateKind::Directory,
                        mode: 0o755,
                    },
                )?;
            }
        }
        Ok(())
    }
    fn create(&mut self, path: &StoragePath, kind: CreateKind, mode: u32) -> Result<()> {
        self.parents(path)?;
        let context = self.context()?;
        self.storage
            .create(&context, path, &CreateOptions { kind, mode })?;
        Ok(())
    }
    fn copy_up(&mut self, path: &StoragePath) -> Result<()> {
        let (stat, shadow) = self.lookup(path)?;
        if shadow {
            return Ok(());
        }
        if stat.kind == ObjectKind::LogicalSymlink {
            let target = self.base().read_link(path)?;
            return self.create_symlink(path, &target, stat.object_id);
        }
        if stat.kind != ObjectKind::File {
            return Err(unsupported("recursive directory copy-up is deferred"));
        }
        self.create(path, CreateKind::File, stat.mode & 0o7777)?;
        let max = (self.storage.capabilities().max_io_bytes as usize).min(MAX_IO_BYTES);
        if max == 0 {
            return Err(error(ErrorKind::ProtocolMismatch, "zero storage I/O limit"));
        }
        let mut buffer = vec![0; max];
        let mut offset = 0;
        while offset < stat.len {
            let count = (stat.len - offset).min(max as u64) as usize;
            let read = self.base().read_at(path, offset, &mut buffer[..count])?;
            if read == 0 || read > count {
                return Err(error(
                    ErrorKind::ProtocolMismatch,
                    "immutable base changed or returned invalid read",
                ));
            }
            let mut written = 0;
            while written < read {
                let context = self.context()?;
                let count = self.storage.write_at(
                    &context,
                    path,
                    offset + written as u64,
                    &buffer[written..read],
                )?;
                if count == 0 {
                    return Err(error(ErrorKind::Io, "copy-up made no progress"));
                }
                written += count;
            }
            offset += read as u64;
        }
        Ok(())
    }
    fn rewrite(
        &self,
        operation: &FsOp,
        path: &StoragePath,
        destination: Option<&StoragePath>,
        shadow: bool,
    ) -> Result<ResolvedAction> {
        let target = if shadow {
            physical(&self.config()?.binding.root, path)?
        } else {
            self.base
                .as_ref()
                .expect("bound base")
                .physical_path(path)?
        };
        let mut paths = vec![PathRewrite {
            operand: if destination.is_some() {
                PathOperand::Source
            } else {
                PathOperand::Path
            },
            path: target,
        }];
        if let Some(destination) = destination {
            paths.push(PathRewrite {
                operand: PathOperand::Destination,
                path: physical(&self.config()?.binding.root, destination)?,
            });
        }
        Ok(ResolvedAction::Rewrite(PhysicalOperation {
            operation: operation.clone(),
            paths,
        }))
    }
    fn record(
        &mut self,
        id: OperationId,
        payload: JournalPayload,
        flush: bool,
    ) -> Result<Sequence> {
        let config = self.config()?;
        let sequence = self.journal.append(&JournalRecord {
            format_version: 1,
            sequence: Sequence(0),
            operation_id: id,
            writer_epoch: config.context.writer_epoch.expect("validated epoch"),
            payload,
        })?;
        if flush {
            self.flush_journal(sequence)?;
        }
        Ok(sequence)
    }
    fn flush_journal(&mut self, sequence: Sequence) -> Result<DurableSequence> {
        let receipt = self.journal.flush(sequence)?;
        let config = self.config()?;
        if receipt.run_id != config.context.run_id
            || Some(receipt.writer_epoch) != config.context.writer_epoch
            || receipt.sequence.0 < sequence.0
        {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "invalid journal durability receipt",
            ));
        }
        Ok(receipt)
    }
    fn set_whiteout(&mut self, path: &StoragePath, present: bool) -> Result<()> {
        let marker = Self::marker(path)?;
        let exists = self.shadow_stat(&marker)?.is_some();
        if present && !exists {
            self.create(&marker, CreateKind::File, 0o600)?;
        }
        if !present && exists {
            let context = self.context()?;
            self.storage.unlink(&context, &marker)?;
        }
        Ok(())
    }
    fn merged(&mut self, path: &StoragePath) -> Result<Vec<DirectoryEntry>> {
        self.directory(path)?;
        let mut entries = BTreeMap::new();
        if !self.whiteouted(path)?
            && absent(self.base().stat(path))?.is_some_and(|s| s.kind == ObjectKind::Directory)
        {
            let mut cursor = None;
            loop {
                let page = self
                    .base()
                    .list(path, cursor.as_ref(), MAX_DIRECTORY_ENTRIES)?;
                for entry in page.entries {
                    validate_name(&entry.name)?;
                    entries.insert(entry.name.as_bytes().to_vec(), entry);
                }
                if page.next.is_none() {
                    break;
                }
                if page.next == cursor {
                    return Err(error(
                        ErrorKind::ProtocolMismatch,
                        "base directory cursor did not advance",
                    ));
                }
                cursor = page.next;
            }
        }
        let names: Vec<_> = entries.keys().cloned().collect();
        for name in names {
            if self.whiteouted(&join(path, &name)?)? {
                entries.remove(&name);
            }
        }
        if self.shadow_stat(path)?.is_some() {
            let mut cursor = None;
            loop {
                let context = self.context()?;
                let limit = self
                    .storage
                    .capabilities()
                    .max_directory_entries
                    .min(MAX_DIRECTORY_ENTRIES);
                let page = self.storage.list(&context, path, cursor.as_ref(), limit)?;
                for entry in page.entries {
                    validate_name(&entry.name)?;
                    entries.insert(entry.name.as_bytes().to_vec(), entry);
                }
                if page.next.is_none() {
                    break;
                }
                if page.next == cursor {
                    return Err(error(
                        ErrorKind::ProtocolMismatch,
                        "shadow directory cursor did not advance",
                    ));
                }
                cursor = page.next;
            }
        }
        for (name, entry) in &mut entries {
            entry.stat = self.lookup(&join(path, name)?)?.0;
        }
        Ok(entries.into_values().collect())
    }
}
fn absolute_components(path: &BytePath) -> Result<Vec<Vec<u8>>> {
    if path.as_bytes().first() != Some(&b'/') {
        return Err(error(
            ErrorKind::InvalidPath,
            "context anchor must be absolute",
        ));
    }
    let mut parts = Vec::new();
    for part in path.as_bytes().split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "context anchor contains parent",
                ))
            }
            name => parts.push(name.to_vec()),
        }
    }
    Ok(parts)
}
fn validate_name(name: &BytePath) -> Result<()> {
    if name.as_bytes().contains(&b'/') || matches!(name.as_bytes(), b"." | b"..") {
        return Err(error(
            ErrorKind::ProtocolMismatch,
            "invalid directory entry name",
        ));
    }
    Ok(())
}
fn success() -> ResolvedAction {
    ResolvedAction::Emulate(EmulatedResult {
        outcome: OperationOutcome::Success { return_value: 0 },
        memory_writes: vec![],
    })
}

impl NamespaceResolver for Overlay {
    fn resolve(&mut self, context: &ProcessContext, operation: &FsOp) -> Result<ResolvedAction> {
        self.idle()?;
        self.planned = None;
        if let FsOp::ReadDir { fd, max_bytes } = operation {
            return self.resolve_directory(context, operation, *fd, *max_bytes);
        }
        let (dir, name, create_parents) = match operation {
            FsOp::Open {
                dir, path, flags, ..
            } => (*dir, path, flags.create),
            FsOp::Stat { dir, path, .. }
            | FsOp::Unlink { dir, path, .. }
            | FsOp::ReadLink { dir, path } => (*dir, path, false),
            FsOp::Symlink {
                link_dir,
                link_name,
                ..
            } => (*link_dir, link_name, true),
            FsOp::Mkdir { dir, path, .. } => (*dir, path, true),
            FsOp::Rename { from_dir, from, .. } => (*from_dir, from, false),
            _ => {
                return Err(unsupported(
                    "operation requires descriptor or metadata support beyond MVP",
                ))
            }
        };
        let readlink_buffer = if matches!(operation, FsOp::ReadLink { .. }) {
            self.readlink_buffer.take()
        } else {
            None
        };
        let follow_final = match operation {
            FsOp::Open { flags, .. } => !(flags.no_follow || flags.create && flags.exclusive),
            FsOp::Stat { follow, .. } => *follow,
            _ => false,
        };
        let path = self.resolve_path_follow(context, dir, name, create_parents, follow_final)?;
        let existing = absent(self.lookup(&path))?;
        let mutation = matches!(
            super::dispatch(operation),
            super::Dispatch::Materialise | super::Dispatch::Whiteout
        );
        if mutation && path.as_bytes().is_empty() {
            return Err(error(ErrorKind::Denied, "cannot mutate namespace root"));
        }
        let mut destination = None;
        match operation {
            FsOp::Open { flags, .. } => {
                if existing.is_none() && !flags.create {
                    return Err(error(ErrorKind::NotFound, "open target absent"));
                }
                if existing.is_some() && flags.create && flags.exclusive {
                    return Err(error(
                        ErrorKind::AlreadyExists,
                        "exclusive create target exists",
                    ));
                }
                if let Some((stat, _)) = &existing {
                    if stat.kind == ObjectKind::LogicalSymlink {
                        return Err(error(ErrorKind::InvalidPath, "open refuses final symlink"));
                    }
                    if flags.directory && stat.kind != ObjectKind::Directory {
                        return Err(error(ErrorKind::InvalidPath, "open requires directory"));
                    }
                    if mutation && stat.kind != ObjectKind::File {
                        return Err(unsupported("writable directory open"));
                    }
                } else if flags.directory {
                    return Err(unsupported("open cannot create a directory"));
                }
            }
            FsOp::Symlink { target, .. } => {
                if target.as_bytes().is_empty() || target.as_bytes().len() > MAX_IO_BYTES {
                    return Err(error(
                        ErrorKind::InvalidInput,
                        "invalid symlink target length",
                    ));
                }
                if existing.is_some() {
                    return Err(error(ErrorKind::AlreadyExists, "symlink target exists"));
                }
            }
            FsOp::Mkdir { .. } => {
                if existing.is_some() {
                    return Err(error(ErrorKind::AlreadyExists, "mkdir target exists"));
                }
            }
            FsOp::Unlink { directory, .. } => {
                let (stat, _) = existing
                    .as_ref()
                    .ok_or_else(|| error(ErrorKind::NotFound, "unlink target absent"))?;
                if *directory != (stat.kind == ObjectKind::Directory) {
                    return Err(error(ErrorKind::InvalidPath, "unlink kind mismatch"));
                }
                if *directory {
                    return Err(unsupported(
                        "rmdir awaits backend directory removal support",
                    ));
                }
            }
            FsOp::Rename { to_dir, to, .. } => {
                let (stat, _) = existing
                    .as_ref()
                    .ok_or_else(|| error(ErrorKind::NotFound, "rename source absent"))?;
                if !matches!(stat.kind, ObjectKind::File | ObjectKind::LogicalSymlink) {
                    return Err(unsupported(
                        "directory rename requires subtree materialisation",
                    ));
                }
                let dest = self.resolve_path_follow(context, *to_dir, to, true, false)?;
                if dest.as_bytes().is_empty() {
                    return Err(error(ErrorKind::Denied, "cannot replace root"));
                }
                if absent(self.lookup(&dest))?.is_some_and(|(s, _)| {
                    !matches!(s.kind, ObjectKind::File | ObjectKind::LogicalSymlink)
                }) {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        "rename destination is not a file",
                    ));
                }
                destination = Some(dest);
            }
            _ => {
                if existing.is_none() {
                    return Err(error(ErrorKind::NotFound, "target absent"));
                }
            }
        }
        let action = match operation {
            FsOp::Symlink { .. } | FsOp::Mkdir { .. } | FsOp::Unlink { .. } => success(),
            FsOp::ReadLink { .. } => {
                let (stat, shadow) = existing.as_ref().expect("validated existence");
                let target = self.target(&path, stat, *shadow)?;
                let (address, len) = readlink_buffer.ok_or_else(|| {
                    unsupported(
                        "ReadLink requires set_readlink_buffer; typed read_link is available",
                    )
                })?;
                let bytes = target.as_bytes()[..target.as_bytes().len().min(len as usize)].to_vec();
                ResolvedAction::Emulate(EmulatedResult {
                    outcome: OperationOutcome::Success {
                        return_value: bytes.len() as u64,
                    },
                    memory_writes: vec![MemoryWrite { address, bytes }],
                })
            }
            FsOp::Stat { .. }
                if existing
                    .as_ref()
                    .is_some_and(|(s, _)| s.kind == ObjectKind::LogicalSymlink) =>
            {
                let stat = &existing.as_ref().unwrap().0;
                let encoded = self
                    .stat_encoder
                    .as_mut()
                    .ok_or_else(|| {
                        unsupported(
                    "logical symlink stat requires native stat encoder; typed stat is available")
                    })?
                    .encode(context, operation, stat)?;
                let total = encoded.memory_writes.iter().try_fold(0usize, |n, w| {
                    w.address.checked_add(w.bytes.len() as u64)?;
                    n.checked_add(w.bytes.len())
                });
                if total.is_none_or(|n| n == 0 || n > MAX_IO_BYTES)
                    || encoded.outcome != (OperationOutcome::Success { return_value: 0 })
                {
                    return Err(error(ErrorKind::ProtocolMismatch, "invalid stat encoding"));
                }
                ResolvedAction::Emulate(encoded)
            }
            FsOp::Rename { .. } if destination.as_ref() == Some(&path) => success(),
            _ => self.rewrite(
                operation,
                &path,
                destination.as_ref(),
                mutation || existing.as_ref().is_some_and(|(_, shadow)| *shadow),
            )?,
        };
        self.planned = Some(Plan {
            action: action.clone(),
            operation: operation.clone(),
            path,
            destination,
            mutation,
            directory_next: None,
        });
        Ok(action)
    }
}

impl NamespaceSession for Overlay {
    fn set_readlink_buffer(&mut self, address: u64, len: u32) -> Result<()> {
        self.idle()?;
        self.readlink_buffer = None;
        if len == 0 || len as usize > MAX_IO_BYTES || address.checked_add(len as u64).is_none() {
            return Err(error(ErrorKind::InvalidInput, "invalid readlink buffer"));
        }
        self.readlink_buffer = Some((address, len));
        Ok(())
    }
    fn set_stat_encoder(&mut self, encoder: Box<dyn StatEncoder>) -> Result<()> {
        self.idle()?;
        self.stat_encoder = Some(encoder);
        Ok(())
    }
    fn read_link(&mut self, path: &StoragePath) -> Result<BytePath> {
        let path = self.typed_path(path, false)?;
        let (stat, shadow) = self.lookup(&path)?;
        self.target(&path, &stat, shadow)
    }
    fn stat(&mut self, path: &StoragePath, follow: bool) -> Result<BlobStat> {
        let path = self.typed_path(path, follow)?;
        Ok(self.lookup(&path)?.0)
    }

    fn set_directory_encoder(&mut self, encoder: Box<dyn DirectoryEncoder>) -> Result<()> {
        if self.pending.is_some() || self.poisoned {
            return Err(error(
                ErrorKind::InvalidState,
                "transaction pending or recovery required",
            ));
        }
        self.directory_encoder = Some(encoder);
        Ok(())
    }
    fn bind(&mut self, config: SessionConfig, base: Box<dyn Base>) -> Result<()> {
        if self.config.is_some() {
            return Err(error(ErrorKind::InvalidState, "namespace already bound"));
        }
        if config.binding.run_id != config.context.run_id
            || config.recovery.run_id != config.context.run_id
            || config.context.writer_epoch.is_none()
            || config.lease.run_id != config.context.run_id
            || Some(config.lease.epoch) != config.context.writer_epoch
        {
            return Err(error(
                ErrorKind::InvalidInput,
                "run binding, recovery and writer authority must match",
            ));
        }
        if !config.recovery.pending.is_empty()
            || config.recovery.last_valid_sequence.0 != 0
            || config.recovery.checkpoint.is_some()
            || config.recovery.tail != JournalTailRecovery::Intact
        {
            return Err(unsupported(
                "nonempty journal recovery requires M1.5 reconciliation",
            ));
        }
        self.config = Some(config);
        self.base = Some(base);
        Ok(())
    }
    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize> {
        self.idle()?;
        let resolved = self.typed_path(path, true)?;
        let path = &resolved;
        let (stat, shadow) = self.lookup(path)?;
        if stat.kind != ObjectKind::File {
            return Err(error(ErrorKind::InvalidPath, "read target is not a file"));
        }
        if out.len() > MAX_IO_BYTES || offset.checked_add(out.len() as u64).is_none() {
            return Err(error(ErrorKind::InvalidInput, "read exceeds bounds"));
        }
        if shadow {
            let context = self.context()?;
            self.storage.read_at(&context, path, offset, out)
        } else {
            self.base().read_at(path, offset, out)
        }
    }
    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        self.idle()?;
        if limit == 0 || limit > MAX_DIRECTORY_ENTRIES {
            return Err(error(
                ErrorKind::InvalidInput,
                "invalid directory page limit",
            ));
        }
        let mut entries = if let Some(cursor) = cursor {
            let (owner, entries) = self
                .pages
                .get(&cursor.0)
                .ok_or_else(|| error(ErrorKind::StaleHandle, "unknown directory cursor"))?;
            if owner != path {
                return Err(error(
                    ErrorKind::StaleHandle,
                    "cursor belongs to another directory",
                ));
            }
            entries.clone()
        } else {
            let resolved = self.typed_path(path, true)?;
            self.merged(&resolved)?
        };
        let next = if entries.len() > limit as usize {
            let remainder = entries.split_off(limit as usize);
            self.serial += 1;
            let token = format!("{}/{}", self.session, self.serial).into_bytes();
            self.pages.insert(token.clone(), (path.clone(), remainder));
            Some(ListCursor(token))
        } else {
            None
        };
        Ok(DirectoryPage { entries, next })
    }
    fn prepare(
        &mut self,
        operation: OperationId,
        action: &ResolvedAction,
    ) -> Result<PreparedAction> {
        self.idle()?;
        if self.used_operations.contains(&operation) {
            return Err(error(
                ErrorKind::InvalidState,
                "operation ID has already been used",
            ));
        }
        let plan = self
            .planned
            .take()
            .ok_or_else(|| error(ErrorKind::InvalidState, "resolve must precede prepare"))?;
        if &plan.action != action {
            return Err(error(
                ErrorKind::InvalidInput,
                "action differs from resolved plan",
            ));
        }
        let intent = if plan.mutation {
            Some(self.intent(&plan, operation)?)
        } else {
            None
        };
        self.used_operations.insert(operation);
        self.pending = Some(Pending {
            id: operation,
            plan: plan.clone(),
            outcome: None,
            whiteouts: vec![],
            retired_index: None,
        });
        // Any failure after append may have left durable intent or storage effects.
        // Keep the session stopped for explicit recovery instead of guessing rollback.
        let result = (|| {
            if let Some(intent) = intent {
                self.record(operation, JournalPayload::Prepare { intent }, true)?;
            }
            let mut executable = plan.action.clone();
            match &plan.operation {
                FsOp::Open { flags, mode, .. } if plan.mutation => {
                    if absent(self.lookup(&plan.path))?.is_some() {
                        self.copy_up(&plan.path)?;
                    } else {
                        self.create(&plan.path, CreateKind::File, *mode)?;
                        self.pending
                            .as_mut()
                            .unwrap()
                            .whiteouts
                            .push((plan.path.clone(), false));
                    }
                    // Creation was executed through Storage, so O_EXCL must not run twice.
                    if let ResolvedAction::Rewrite(physical) = &mut executable {
                        if let FsOp::Open {
                            flags: prepared, ..
                        } = &mut physical.operation
                        {
                            prepared.create = false;
                            prepared.exclusive = false;
                            prepared.truncate = flags.truncate;
                        }
                    }
                }
                FsOp::Symlink { target, .. } => {
                    self.create_symlink(&plan.path, target, ObjectId(operation.0))?;
                    self.pending
                        .as_mut()
                        .unwrap()
                        .whiteouts
                        .push((plan.path.clone(), false));
                }
                FsOp::Mkdir { mode, .. } => {
                    self.create(&plan.path, CreateKind::Directory, *mode)?;
                    // Keep an existing directory whiteout as an opaque-base marker.
                }
                FsOp::Unlink { .. } => {
                    self.remove_symlink_index(&plan.path)?;
                    if self.shadow_stat(&plan.path)?.is_some() {
                        let context = self.context()?;
                        self.storage.unlink(&context, &plan.path)?;
                    }
                    self.pending
                        .as_mut()
                        .unwrap()
                        .whiteouts
                        .push((plan.path.clone(), true));
                }
                FsOp::Rename { .. } => {
                    let destination = plan.destination.as_ref().unwrap();
                    if destination != &plan.path {
                        self.copy_up(&plan.path)?;
                        self.parents(destination)?;
                        if let Some(stat) = self.shadow_stat(destination)? {
                            let index = symlink_index(stat.object_id)?;
                            if self.shadow_stat(&index)?.is_some() {
                                self.pending.as_mut().unwrap().retired_index = Some(index);
                            }
                        }
                        self.pending.as_mut().unwrap().whiteouts =
                            vec![(plan.path.clone(), true), (destination.clone(), false)];
                    }
                }
                _ => {}
            }
            self.pending.as_mut().unwrap().plan.action = executable.clone();
            Ok(PreparedAction {
                operation_id: operation,
                action: executable,
            })
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn observe_result(&mut self, operation: OperationId, result: &OperationOutcome) -> Result<()> {
        let pending = self.pending_for(operation)?;
        if pending.outcome.is_some() {
            return Err(error(ErrorKind::InvalidState, "result already observed"));
        }
        if let ResolvedAction::Emulate(expected) = &pending.plan.action {
            if &expected.outcome != result {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "outcome differs from executed emulation",
                ));
            }
        }
        if pending.plan.mutation {
            if let Err(e) = self.record(
                operation,
                JournalPayload::ObservedResult {
                    outcome: result.clone(),
                },
                true,
            ) {
                self.poisoned = true;
                return Err(e);
            }
        }
        self.pending.as_mut().unwrap().outcome = Some(result.clone());
        Ok(())
    }
    fn commit(&mut self, operation: OperationId) -> Result<CommitReceipt> {
        let pending = self.pending_for(operation)?;
        if !matches!(pending.outcome, Some(OperationOutcome::Success { .. })) {
            return Err(error(
                ErrorKind::InvalidState,
                "commit requires observed success; abort failures",
            ));
        }
        if !pending.plan.mutation {
            if let Some((key, entries)) = pending.plan.directory_next.clone() {
                self.directories.insert(key, entries);
            }
            self.pending = None;
            return Ok(CommitReceipt {
                operation_id: operation,
                sequence: self.last_committed,
            });
        }
        let whiteouts = pending.whiteouts.clone();
        let retired_index = pending.retired_index.clone();
        let result = (|| {
            if let Some(index) = retired_index {
                let context = self.context()?;
                self.storage.unlink(&context, &index)?;
            }
            for (path, present) in whiteouts {
                self.set_whiteout(&path, present)?;
            }
            let context = self.context()?;
            let receipt = self.storage.flush(&FlushRequest {
                context,
                scope: FlushScope::EntireRun,
            })?;
            if receipt.run_id != self.config()?.context.run_id
                || Some(receipt.writer_epoch) != self.config()?.context.writer_epoch
                || receipt.durability == Durability::None
            {
                return Err(error(
                    ErrorKind::ProtocolMismatch,
                    "invalid storage durability receipt",
                ));
            }
            // One Rename intent + Commit defines both whiteout changes. The engine
            // blocks readers throughout; crash recovery must replay the pair.
            let sequence = self.record(operation, JournalPayload::Commit, true)?;
            self.last_committed = sequence;
            Ok(CommitReceipt {
                operation_id: operation,
                sequence,
            })
        })();
        match result {
            Ok(receipt) => {
                self.pending = None;
                Ok(receipt)
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }
    fn abort(&mut self, operation: OperationId, reason: &AbortReason) -> Result<()> {
        let pending = self.pending_for(operation)?;
        // Materialisation or emulated unlink may already have effects. We do not
        // clear a mutated session or claim those effects were rolled back.
        let mutation = pending.plan.mutation;
        if mutation {
            if let Err(e) = self.record(
                operation,
                JournalPayload::Abort {
                    reason: format!("{reason:?}"),
                },
                true,
            ) {
                self.poisoned = true;
                return Err(e);
            }
        }
        self.pending = None;
        if mutation {
            self.poisoned = true;
            return Err(error(
                ErrorKind::InvalidState,
                "aborted effects require reconciliation",
            ));
        }
        Ok(())
    }
    fn checkpoint(&mut self, request: &CheckpointRequest) -> Result<Checkpoint> {
        self.idle()?;
        let context = self.context()?;
        self.storage.flush(&FlushRequest {
            context,
            scope: FlushScope::EntireRun,
        })?;
        let durable = self.flush_journal(self.last_committed)?;
        let mut state = request.state.clone();
        // Caller state cannot override authoritative control whiteouts.
        state.whiteouts = self.whiteout_inventory()?;
        let config = self.config()?;
        let checkpoint = Checkpoint {
            format_version: 1,
            id: request.id,
            run_id: config.context.run_id,
            writer_epoch: config.context.writer_epoch.unwrap(),
            last_committed: durable,
            fingerprints: request.fingerprints.clone(),
            state,
            clean: false,
        };
        if self.journal.write_checkpoint(&checkpoint)? != checkpoint.id {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "checkpoint identity mismatch",
            ));
        }
        Ok(checkpoint)
    }
    fn renew_writer(&mut self) -> Result<WriterLease> {
        // Renewal is allowed with a transaction pending: an in-flight syscall
        // must not be able to starve the lease it is mutating under.
        let lease = self.config()?.lease.clone();
        let renewed = self.storage.renew_writer(&lease)?;
        if renewed.run_id != lease.run_id
            || renewed.writer_id != lease.writer_id
            || renewed.epoch.0 < lease.epoch.0
        {
            return Err(error(ErrorKind::LeaseLost, "invalid lease renewal"));
        }
        if renewed.epoch != lease.epoch {
            // A new epoch means this was a takeover, not a renewal of ours.
            return Err(error(ErrorKind::LeaseLost, "writer epoch advanced"));
        }
        self.config.as_mut().expect("bound").lease = renewed.clone();
        Ok(renewed)
    }
    fn finish_run(&mut self, request: &FinishRunRequest) -> Result<FinishRunReceipt> {
        self.idle()?;
        let config = self.config()?;
        if config.context.run_id != request.run_id {
            return Err(error(ErrorKind::InvalidInput, "finish targets another run"));
        }
        let lease = config.lease.clone();
        // Session-level operation identity, distinct from the per-syscall IDs the
        // supervisor allocates for transactions.
        let session_operation = config.context.operation_id;
        let context = self.context()?;
        // Tracee-written data first: a completion record that outlives its data
        // would describe a run that does not exist on the backing store.
        let durability = self.storage.flush(&FlushRequest {
            context,
            scope: FlushScope::EntireRun,
        })?;
        let completed_through = self.record(
            session_operation,
            JournalPayload::Lifecycle(JournalLifecycle::RunCompleted {
                through: self.last_committed,
            }),
            true,
        )?;
        self.journal.close()?;
        self.storage.release_writer(&lease)?;
        self.storage.close_run()?;
        self.config = None;
        self.base = None;
        Ok(FinishRunReceipt {
            run_id: request.run_id,
            durability,
            completed_through,
        })
    }
    fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
        let config = self.config()?;
        if config.context.run_id != request.run_id {
            return Err(error(
                ErrorKind::InvalidInput,
                "failure targets another run",
            ));
        }
        let lease = config.lease.clone();
        // No completion record, no checkpoint: a failed run must not look clean.
        // Close the journal so its accepted records are not silently discarded,
        // but keep its error rather than reporting a tidy shutdown.
        let closed = self.journal.close();
        if !request.tree_terminated {
            // Something may still hold a writable descriptor into this run.
            // Retaining the writer marker blocks takeover; storage stays open
            // because closing it would invalidate that evidence for this session.
            self.poisoned = true;
            return closed.and(Err(error(
                ErrorKind::LeaseLost,
                "run left recovery-required: supervised tree termination unproven",
            )));
        }
        let released = self.storage.release_writer(&lease);
        let closed_run = self.storage.close_run();
        self.config = None;
        self.base = None;
        self.poisoned = true;
        closed.and(released).and(closed_run)
    }
}
impl Overlay {
    fn resolve_directory(
        &mut self,
        context: &ProcessContext,
        operation: &FsOp,
        fd: TracedFd,
        max_bytes: u32,
    ) -> Result<ResolvedAction> {
        if max_bytes == 0 || max_bytes as usize > MAX_IO_BYTES {
            return Err(error(
                ErrorKind::InvalidInput,
                "invalid directory output bound",
            ));
        }
        let state = context
            .fds
            .get(&fd)
            .ok_or_else(|| error(ErrorKind::StaleHandle, "unknown directory descriptor"))?;
        let key = (context.task, context.exec_generation, fd, state.object);
        let path = self.resolve_path(
            context,
            DirRef::Fd(fd),
            &BytePath::new(b".".to_vec())?,
            false,
        )?;
        let entries = if let Some(entries) = self.directories.get(&key) {
            entries.clone()
        } else {
            self.merged(&path)?
        };
        let encoded = self
            .directory_encoder
            .as_mut()
            .ok_or_else(|| {
                unsupported(
                    "ReadDir requires injected native directory encoder; typed list is available",
                )
            })?
            .encode(context, operation, &entries)?;
        let total = encoded
            .result
            .memory_writes
            .iter()
            .try_fold(0usize, |n, write| {
                write.address.checked_add(write.bytes.len() as u64)?;
                n.checked_add(write.bytes.len())
            })
            .ok_or_else(|| error(ErrorKind::ProtocolMismatch, "directory output overflows"))?;
        if encoded.consumed > entries.len()
            || (encoded.consumed == 0 && !entries.is_empty())
            || (encoded.consumed > 0 && total == 0)
            || total > max_bytes as usize
            || encoded.result.outcome
                != (OperationOutcome::Success {
                    return_value: total as u64,
                })
        {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "invalid directory encoding or no progress",
            ));
        }
        let action = ResolvedAction::Emulate(encoded.result);
        self.planned = Some(Plan {
            action: action.clone(),
            operation: operation.clone(),
            path,
            destination: None,
            mutation: false,
            directory_next: Some((key, entries[encoded.consumed..].to_vec())),
        });
        Ok(action)
    }
    fn pending_for(&self, id: OperationId) -> Result<&Pending> {
        if self.poisoned {
            return Err(error(ErrorKind::InvalidState, "session requires recovery"));
        }
        self.pending
            .as_ref()
            .filter(|p| p.id == id)
            .ok_or_else(|| error(ErrorKind::InvalidState, "operation is not pending"))
    }
    fn intent(&mut self, plan: &Plan, operation: OperationId) -> Result<JournalIntent> {
        let existing = absent(self.lookup(&plan.path))?;
        let object = existing
            .as_ref()
            .map_or(ObjectId(operation.0), |(s, _)| s.object_id);
        let path = logical(&plan.path)?;
        Ok(match &plan.operation {
            FsOp::Open { flags, mode, .. } => match existing {
                None => JournalIntent::Create {
                    object,
                    path,
                    directory: false,
                    mode: *mode,
                },
                Some((_, false)) => JournalIntent::CopyUp { object, path },
                Some(_) if flags.truncate => JournalIntent::Truncate { object, length: 0 },
                Some(_) => JournalIntent::Write {
                    object,
                    offset: None,
                    length: 0,
                },
            },
            FsOp::Symlink { target, .. } => JournalIntent::Symlink {
                object,
                path,
                target: target.clone(),
            },
            FsOp::Mkdir { mode, .. } => JournalIntent::Create {
                object,
                path,
                directory: true,
                mode: *mode,
            },
            FsOp::Unlink { directory, .. } => JournalIntent::Unlink {
                object,
                path,
                directory: *directory,
            },
            FsOp::Rename { .. } => JournalIntent::Rename {
                object,
                from: path,
                to: logical(plan.destination.as_ref().unwrap())?,
            },
            _ => return Err(unsupported("journal intent for this operation")),
        })
    }
    fn whiteout_inventory(&mut self) -> Result<Vec<BytePath>> {
        // Traverse the control marker tree, decoding component boundaries.
        let start = StoragePath::new(StorageAnchor::Control, b"whiteouts".to_vec())?;
        if self.shadow_stat(&start)?.is_none() {
            return Ok(vec![]);
        }
        let mut pending = vec![(start, Vec::<Vec<u8>>::new())];
        let mut found = BTreeSet::new();
        while let Some((path, encoded)) = pending.pop() {
            let mut cursor = None;
            loop {
                let context = self.context()?;
                let page = self.storage.list(
                    &context,
                    &path,
                    cursor.as_ref(),
                    self.storage
                        .capabilities()
                        .max_directory_entries
                        .min(MAX_DIRECTORY_ENTRIES),
                )?;
                for entry in page.entries {
                    let mut encoded = encoded.clone();
                    encoded.push(entry.name.as_bytes().to_vec());
                    if entry.stat.kind == ObjectKind::Directory {
                        pending.push((join(&path, entry.name.as_bytes())?, encoded));
                    } else if entry.name.as_bytes() == b".wh" {
                        let mut components: Vec<Vec<u8>> = vec![];
                        for chunk in &encoded[..encoded.len() - 1] {
                            if chunk == b"c" {
                                components.push(vec![]);
                            } else {
                                if chunk.len() % 2 != 0 {
                                    return Err(error(
                                        ErrorKind::CorruptJournal,
                                        "invalid whiteout marker",
                                    ));
                                }
                                let target = components.last_mut().ok_or_else(|| {
                                    error(ErrorKind::CorruptJournal, "missing whiteout component")
                                })?;
                                for pair in chunk.as_chunks::<2>().0.iter() {
                                    let text = std::str::from_utf8(pair).map_err(|_| {
                                        error(
                                            ErrorKind::CorruptJournal,
                                            "invalid whiteout encoding",
                                        )
                                    })?;
                                    target.push(u8::from_str_radix(text, 16).map_err(|_| {
                                        error(
                                            ErrorKind::CorruptJournal,
                                            "invalid whiteout encoding",
                                        )
                                    })?);
                                }
                            }
                        }
                        found.insert(logical(&root(&components.join(&b'/'))?)?);
                    }
                }
                if page.next.is_none() {
                    break;
                }
                cursor = page.next;
            }
        }
        Ok(found.into_iter().collect())
    }
}

#[cfg(test)]
mod tests;
