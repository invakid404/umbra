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
/// Mode for a shadow directory with no base directory whose bits it can take.
/// Four paths reach it: a control-anchored ancestor, one whose logical path is
/// whiteouted at any level, one the base does not hold or holds as something
/// other than a directory, and one whose base stat fails — that last one *is* a
/// live base directory, so this is not "nothing to shadow", it is "nothing
/// legible to shadow". See `Overlay::shadow_parent_mode`.
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// Owner bits every shadow directory carries regardless of its base counterpart.
///
/// umbra owns the shadow, so POSIX applies the *owner* bits to umbra's own writes
/// into it — and the engine must be able to create inside every ancestor it
/// materialises. A base directory without owner-write is the ordinary case, not
/// an exotic one: read-only artifact trees are what an overlay exists to make
/// writable. Copying such a mode verbatim yields an ancestor the next `create`
/// cannot enter, and `parents` runs inside `prepare`, which poisons the run on
/// any error — so a plain `0o555` base would kill the session for an operation
/// POSIX itself allows. Forcing these bits never widens group or other beyond
/// what the base granted.
const SHADOW_OWNER_BITS: u32 = 0o700;

/// Whether the logical path `parents` is materialising is whiteouted, carried
/// across one walk so the prefix scan runs at most once.
///
/// The distinction that matters is `Unscanned` vs `Clear`. `parents` skips
/// ancestors that already exist in the shadow, so the ancestors
/// `shadow_parent_mode` is asked about are not the whole prefix chain, and a
/// marker above the first of them would otherwise never be seen — a shadow
/// directory and a whiteout marker for the same path coexist by design, which is
/// what `prepare`'s `FsOp::Mkdir` arm means by keeping one as an opaque-base
/// marker. So the first ancestor scans every prefix.
///
/// After that the remaining ancestors are contiguous — once one is missing, every
/// deeper one is missing too, because a directory cannot hold children before it
/// exists, which every hierarchical `Storage` backend guarantees — so each
/// arrives here in turn and its own marker is the only one still unexamined.
/// This type is what keeps that optimisation honest; it is not what makes the
/// scan correct. Collapsing it to a bool would cost `O(depth²)` marker stats, not
/// a wrong answer.
enum WhiteoutScan {
    /// No prefix examined yet: the next ancestor needs a full scan from the root.
    Unscanned,
    /// Every prefix down to the last ancestor examined is live.
    Clear,
    /// Some prefix is whiteouted, so every deeper one is too.
    Hidden,
}

/// What `shadow_parent_mode` decided about one missing shadow ancestor.
///
/// The two fields answer different questions off the same evidence and must not
/// be collapsed into one: the mode is a fidelity choice that rounds *down* to a
/// safe default whenever the base cannot be consulted, while the creation
/// verdict is a safety gate that rounds the same non-answer *up* to "a logical
/// creation, so record an undo for it". See `parents`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShadowAncestor {
    /// The mode to materialise the ancestor with.
    mode: u32,
    /// Whether the base holds a live directory at this path — the one arm the
    /// base corroborates. Every early return, every whiteouted prefix, every
    /// non-directory and every swallowed base error answers `false`, because
    /// `parents` turns this into the ancestor's place on `Pending.rollback`, and
    /// false — "record it, and remove it on a reconciled abort" — is the
    /// fail-closed side.
    shadows_base_directory: bool,
}
impl ShadowAncestor {
    /// An ancestor that shadows no base directory: default mode, and a logical
    /// creation as far as `Pending.rollback` is concerned.
    fn unshadowed() -> Self {
        Self {
            mode: DEFAULT_DIRECTORY_MODE,
            shadows_base_directory: false,
        }
    }
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
/// What `create_symlink` created, for a caller that has to record an undo.
///
/// Returned rather than pushed, on the same terms as `parents`: `copy_up` also
/// reaches `create_symlink` and deliberately records nothing (see `copy_up`), so
/// the recording decision belongs to the caller. `placeholder` is not here --
/// every caller passes it in.
struct CreatedSymlink {
    /// `symlinks/objects/<backend object id>`. The one path here the caller could
    /// not have derived: the backend object ID is whatever the shadow assigned
    /// the placeholder, so re-deriving it means stat-ing a placeholder that the
    /// undo is about to remove. See `RollbackEntry::Symlink`.
    backing_index: StoragePath,
    /// `symlinks/targets/<logical object id>`, derivable from the identity the
    /// caller passed in and returned anyway so the entry transcribes one answer
    /// rather than recomputing half of it.
    target_blob: StoragePath,
    /// The Root-anchored ancestors `parents` materialised over nothing **for the
    /// placeholder only**. The two Control-path walks `write_control` drives
    /// (`symlinks/targets/`, `symlinks/objects/`) are deliberately not here:
    /// those directories are shared by every symlink in the run, so recording
    /// them would make a second live symlink meet `ENOTEMPTY` at
    /// `remove_directory` and poison a run that should have reconciled.
    ancestors: Vec<StoragePath>,
}

/// One undo a reconciled `abort` performs, and the removal(s) that perform it.
///
/// `File` and `Directory` are the storage surface's own split, not this engine's
/// invention: removal is split into `StorageOperation::Unlink` and
/// `StorageOperation::RemoveDirectory`, and the backends that implement both
/// already carry the split as a local `bool`. This is that bool, named and
/// hoisted to the site that has to *decide* it -- `prepare` knows whether it
/// asked for a file or a directory; `abort` would have to stat to find out.
///
/// `Symlink` is **not** a storage-surface discriminant, and the enum should not
/// be read as if every variant were one. It is three `Unlink`s that only make
/// sense as a single entry, because the pieces are not independently reachable --
/// see the variant's own note. Recording them as three separate entries would
/// remove the same three objects in the same order today, but the order would be
/// an emergent property of three independent pushes rather than a stated one, and
/// a future arm could interleave an entry between them.
#[derive(Clone)]
enum RollbackEntry {
    /// A shadow file `prepare` created: `unlink`.
    File(StoragePath),
    /// A shadow directory `prepare` materialised over nothing:
    /// `remove_directory`.
    Directory(StoragePath),
    /// A logical symlink `prepare` created: three `unlink`s, **in field order**.
    ///
    /// The backing index goes first and that step is *forced*. Its name is
    /// `symlinks/objects/<backend object id>`, derived from whatever the shadow
    /// assigned the placeholder, so once the placeholder is gone nothing in the
    /// session can derive it -- `JournalIntent::Symlink` carries the link path and
    /// the *logical* identity, not the backend's. Recording the path here is what
    /// lets a rollback that *runs* remove it; it does nothing for a rollback that
    /// fails, which is the second reason for the order: the README's own rule is
    /// that a retired index is removed so backend inode reuse cannot inherit a
    /// deleted link, and an index outliving its placeholder is the one residue
    /// that can make a later, unrelated object read back as a logical symlink.
    ///
    /// Target blob before placeholder is a **tiebreak, not a constraint**, and
    /// saying so plainly is the point. Once the index is gone there is no
    /// referential structure left -- the blob is unreferenced and the placeholder
    /// is an ordinary empty `0o444` file -- so either order is correct. What
    /// decides it is residue quality on a rollback that *fails*. The placeholder
    /// unlink is the one of the three that can plausibly fail on an otherwise
    /// healthy backend: it is Root-anchored, inside the tracee-visible shadow
    /// tree, subject to that tree's own directory permissions, which is exactly
    /// what `a_symlink_rollback_that_cannot_unlink_the_placeholder_poisons`
    /// injects. The two Control blobs live in a tree only umbra writes, and their
    /// unlink fails only under conditions that would fail all three. Ordering the
    /// most-likely-to-fail step last therefore puts the two-object residue on the
    /// rare failure rather than the common one.
    ///
    /// On the success path the order buys nothing and is unobservable, which is
    /// why `the_symlink_undo_unlinks_the_index_before_the_blob_and_the_placeholder`
    /// pins it against the recorder rather than against the tree.
    ///
    /// The ancestors `parents` materialised for the placeholder are *not* here.
    /// They are ordinary `Directory` entries the `FsOp::Symlink` arm queues before
    /// this one, so the reverse walk in `abort` unwinds them leaf-first like every
    /// other materialised ancestor; folding them in would duplicate that ordering
    /// logic inside one variant for no gain.
    Symlink {
        backing_index: StoragePath,
        target_blob: StoragePath,
        placeholder: StoragePath,
    },
}
/// What `commit` must remove for a `Pending.destroy`, and the storage call that
/// removes it.
///
/// The same split `RollbackEntry::File`/`Directory` names, for the same reason
/// and hoisted to the same kind of site: removal is `StorageOperation::Unlink`
/// or `StorageOperation::RemoveDirectory`, and `prepare` knows which one it is
/// asking for from `FsOp::Unlink`'s own `directory` flag, while `commit` would
/// have to stat to find out.
///
/// A separate enum rather than a reuse of `RollbackEntry`: that list means
/// "things this transaction created and an abort must un-create", this field
/// means "the thing this transaction's whiteout replaces and a commit must
/// destroy", and `RollbackEntry::Symlink` has no meaning here at all.
///
/// One `Option<Destroy>` rather than a second `destroy_directory` field, so
/// "both set" is unrepresentable: two `Option`s would put a silent
/// double-destroy of two different paths in one transaction one typo away, and
/// would need a written cannot-happen note in place of a type that says it.
#[derive(Clone)]
enum Destroy {
    /// A shadow file or logical-symlink placeholder: retire the backing index,
    /// then `unlink`.
    File(StoragePath),
    /// A shadow directory: `remove_directory`. No index to retire -- a
    /// directory's object ID never keys a `symlinks/objects/` entry.
    Directory(StoragePath),
}
struct Pending {
    id: OperationId,
    plan: Plan,
    outcome: Option<OperationOutcome>,
    whiteouts: Vec<(StoragePath, bool)>,
    retired_index: Option<StoragePath>,
    /// The shadow object this transaction's whiteout replaces, removed in
    /// `commit` beside the marker.
    ///
    /// A plan, like `whiteouts` and `retired_index` and for the same reason: a
    /// transaction that does not commit must leave the namespace as it found it,
    /// and an object `prepare` had already unlinked could not be put back. The
    /// bytes of a shadow-only object exist nowhere else in the run, and even a
    /// byte-exact restore would hand the tracee a new backend object ID for a
    /// syscall it was told had failed. So the destruction waits (#66).
    ///
    /// `None` for every other operation. `commit` re-probes `shadow_stat` rather
    /// than trusting a prepare-time probe: the two are equivalent because the
    /// engine serialises the transaction, and probing next to the removal keeps
    /// the condition and the effect in one place.
    ///
    /// The variant says which storage removal `commit` performs; see `Destroy`
    /// for why the kind is carried here rather than restated as a second field
    /// or re-derived by a stat in `commit`.
    destroy: Option<Destroy>,
    /// Shadow objects `prepare` created that `abort` can undo, in creation
    /// order, each carrying the removal that undoes it. A reconciled abort walks
    /// the list in reverse and performs each entry's removal(s).
    ///
    /// **Every prepare-time logical creation records an entry here.** That rule
    /// used to have an escape: an arm that could not express its undo latched
    /// `Pending.created` instead, and the gate poisoned. There is no such latch
    /// any more ([#69](https://github.com/invakid404/umbra/issues/69) gave the
    /// last holdout, `FsOp::Symlink`, the `RollbackEntry::Symlink` arm), so an arm
    /// that creates and forgets to record does not fail closed -- it publishes a
    /// path the tracee was told its syscall failed to make, and a later
    /// `O_CREAT|O_EXCL` on that path answers `EEXIST` forever. A new
    /// materialising arm has to record; see `RollbackEntry` for the shapes
    /// available and `FsOp::Link` in the README for the arm most likely to forget.
    ///
    /// Directories belong here since
    /// [#64](https://github.com/invakid404/umbra/issues/64) gave `LocalStorage`
    /// the `RemoveDirectory` arm the other backends already had. Before that the
    /// engine could not rely on the storage *surface* for a directory removal, so
    /// a materialised shadow directory had no undo and latched the since-retired
    /// `Pending.created` instead; it can now, and does.
    ///
    /// Reverse-insertion order is sufficient without a depth key, and the reason
    /// is structural rather than lucky. `parents` walks root-to-leaf and creates
    /// only where `shadow_stat` misses, so one walk contributes ancestors in
    /// strictly increasing depth with no duplicates; `create` runs `parents`
    /// before creating the object, so the object always lands after every
    /// ancestor it needs; and each prepare arm performs at most one
    /// rollback-pushing walk per transaction, so two independently-rooted groups
    /// never interleave. `a/b/c/fresh` over an empty base therefore inserts
    /// `[Directory(a), Directory(a/b), Directory(a/b/c), File(a/b/c/fresh)]` and
    /// unwinds
    /// leaf-first, every `rmdir` seeing an empty directory. A `depth` field
    /// would today be a function of the path and would never change the order.
    ///
    /// `RollbackEntry::Symlink` does not disturb that invariant. It is pushed by
    /// the `FsOp::Symlink` arm *after* that arm's `queue_ancestors`, like every
    /// other object entry, and the three paths inside it are removed in the
    /// variant's own field order rather than by this loop's reversal -- two
    /// Control blobs that no directory entry here contains, and the placeholder,
    /// which is a file.
    ///
    /// A removal that fails poisons, `ENOTEMPTY` included — see `abort`. Given
    /// which sites push (below), a non-empty shadow directory here means the
    /// invariant above is broken, so the loud backstop is the point.
    ///
    /// Not every materialised ancestor is here. `parents` returns only those it
    /// created over *nothing*; one that shadows a live base directory stays
    /// standing, because it publishes no path the run did not already expose.
    /// And `parents` itself pushes nothing — it *returns*, and only the prepare
    /// arms that read the verdict record it. That is load-bearing rather than
    /// stylistic: `parents` is also reached from `copy_up`, `write_control` and
    /// `set_whiteout`, which deliberately keep their own object off this list, so
    /// a `parents` that pushed would queue an ancestor whose contents are not
    /// queued. A refused write-open onto a base file
    /// under a not-yet-shadowed base directory would then reach abort with
    /// `[(dir,Dir)]` and a copied-up file inside `dir`: `rmdir` → `ENOTEMPTY` →
    /// poison, converting today's clean reconcile into a dead run — exactly the
    /// ordinary `EPERM` class [#53](https://github.com/invakid404/umbra/issues/53)
    /// exists to survive.
    ///
    /// Nothing else has to be restored alongside the object. `whiteouts`,
    /// `retired_index` and `destroy` are plans consumed in `commit` alone, so a
    /// reconciled abort never applied any of them; and the shadow object
    /// outranks even a stale
    /// whiteout marker, because `lookup` consults `shadow_stat` first and
    /// `whiteouted` only on a shadow miss. Removing the object therefore
    /// restores the marker's rank by itself.
    ///
    /// The list is process-local and dies with the session, and since
    /// [#65](https://github.com/invakid404/umbra/issues/65) that is a priced
    /// cost rather than an unexamined premise. `bind` no longer refuses a
    /// journal carrying unfinished operations. It reads their intents and
    /// poisons for any that implies a prepare-time creation, precisely because
    /// this list is what would have taken that creation back and it is gone.
    /// The evidence reaching that verdict is positive, not inferred from a
    /// missing record: `prepare` fsyncs its `Prepare` before any creating arm
    /// runs, so no creation is ever durable without its intent. See
    /// `replay_must_poison` for the classification, and `abort` for the
    /// ordering -- and the flush -- that make an *empty* `pending` assert every
    /// recorded removal is durable, rather than merely that an undo was
    /// attempted.
    ///
    /// A new materialising arm therefore owes two things, not one: an entry
    /// here, and an answer at `replay_must_poison` for the intent it journals.
    /// The second is compiler-enforced only for a new `JournalIntent` variant;
    /// an arm that starts creating under an *existing* intent has to be carried
    /// across by hand, which is why the two sites name each other.
    rollback: Vec<RollbackEntry>,
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
    completed_run: Option<RunId>,
    serial: u64,
    last_committed: Sequence,
    pages: BTreeMap<Vec<u8>, (StoragePath, Vec<DirectoryEntry>)>,
    session: u64,
    directory_encoder: Option<Box<dyn DirectoryEncoder>>,
    directories: BTreeMap<DirectoryKey, Vec<DirectoryEntry>>,
    used_operations: BTreeSet<OperationId>,
    readlink_buffer: Option<(u64, u32)>,
    stat_encoder: Option<Box<dyn StatEncoder>>,
    /// Set by `whiteouted()` whenever a marker hid a component during the current
    /// `resolve`. Reset at the top of every `resolve`, read only inside that same
    /// call, through `hidden_or`. It is the difference between "the namespace
    /// knows nothing is here" (resuming the tracee's own syscall is equivalent)
    /// and "the namespace deliberately hides a base object that still exists on
    /// the host" (resuming would reveal it — #49). `lookup()` consults
    /// `whiteouted()` only when the shadow has no object, so a set latch during a
    /// resolve that ends NotFound-shaped means a whiteout is why. A resolve that
    /// touches a whiteout yet still succeeds leaves it set but unread until the
    /// next reset. Stale sets from lookups outside `resolve` are harmless: the
    /// reset precedes every read.
    whiteout_hit: bool,
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
            completed_run: None,
            serial: 0,
            last_committed: Sequence(0),
            pages: BTreeMap::new(),
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            directory_encoder: None,
            directories: BTreeMap::new(),
            used_operations: BTreeSet::new(),
            readlink_buffer: None,
            stat_encoder: None,
            whiteout_hit: false,
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
                // Latch here, the one place a whiteout is ever detected, so no
                // NotFound-shaped resolve — final component or whiteouted
                // ancestor — can miss that a marker is the reason. See the field
                // comment and `hidden_or`.
                self.whiteout_hit = true;
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// A `NotFound` the overlay produced *because a whiteout hides a base object*
    /// is a decision, not a failed lookup: the base object is still on the host,
    /// so handing the error to the supervisor makes it resume the tracee's own
    /// unrewritten syscall and reveal the file (#49). Deny it with ENOENT instead.
    /// A truly-absent NotFound keeps the error, whose resume is equivalent to the
    /// answer the namespace predicts because the base *is* the host filesystem.
    /// Mutating operations are untouched: they have no resume escape in the
    /// supervisor and every resolve error already ends the run, so the `!mutation`
    /// guard is what confines this to the non-mutating path.
    fn hidden_or(&self, e: UmbraError, mutation: bool) -> Result<ResolvedAction> {
        if !mutation && e.kind == ErrorKind::NotFound && self.whiteout_hit {
            return Ok(ResolvedAction::Deny(Errno::ENOENT));
        }
        Err(e)
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
                                // A distinct kind, not a distinguishing
                                // message: the platform answers a link loop
                                // with its own native errno, and containment
                                // failures must not be mistaken for one.
                                return Err(error(
                                    ErrorKind::SymlinkLoop,
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
    /// Materialise a logical symlink: the target blob, the placeholder, and the
    /// backing index keyed on the placeholder's backend object ID.
    ///
    /// Reports what it created so a caller that has to undo it can record the
    /// paths rather than re-derive them. Only the *placeholder's* ancestors are
    /// reported; `write_control`'s own two `parents` walks keep discarding theirs,
    /// for the reason `CreatedSymlink::ancestors` gives.
    fn create_symlink(
        &mut self,
        path: &StoragePath,
        target: &BytePath,
        object: ObjectId,
    ) -> Result<CreatedSymlink> {
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
        let ancestors = self.create(path, CreateKind::File, 0o444)?;
        let backend = self
            .shadow_stat(path)?
            .expect("created placeholder")
            .object_id;
        let backing_index = symlink_index(backend)?;
        self.write_control(&backing_index, object.0.to_string().as_bytes())?;
        Ok(CreatedSymlink {
            backing_index,
            target_blob: metadata,
            ancestors,
        })
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
    /// Whether the shadow backend has qualified applying `MetadataUpdate`'s
    /// uid/gid, which is what decides whether the engine carries base ownership
    /// onto a shadow object at all.
    ///
    /// Gating on an advertised name rather than on a backend identity is the
    /// `features` set's documented purpose, and it is what makes this change's
    /// degraded mode provably a no-op: against a backend that does not advertise
    /// the name, no `SetMetadata` is emitted and every materialised object looks
    /// exactly as it did before ownership carry existed.
    fn ownership_fidelity(&self) -> bool {
        self.storage
            .capabilities()
            .features
            .contains(capabilities::STORAGE_OWNERSHIP_FIDELITY_V1)
    }
    /// Whether the shadow backend advertises that a new object takes its
    /// parent directory's identity, so `identity_at` may read a materialised
    /// shadow parent rather than fall back to the shadow root.
    ///
    /// Gated on the advertised name for the same reason as `ownership_fidelity`:
    /// the created-identity rule is the backend's, not the host's, and the
    /// overlay and the storage backend are separately spawned subprocesses, so
    /// only the backend can qualify the claim. A backend that stays silent is
    /// read the old way and its behaviour is byte-identical to before this
    /// name existed.
    fn parent_identity_backend(&self) -> bool {
        self.storage
            .capabilities()
            .features
            .contains(capabilities::STORAGE_PARENT_IDENTITY_V1)
    }
    /// Give a freshly materialised shadow object the ownership of the base
    /// object it shadows, and report whether that actually held.
    ///
    /// `CreateOptions` carries a mode and no uid/gid, so this is necessarily a
    /// second operation: `create` first, ownership after. The window between
    /// them is discussed under `parents` and `copy_up`; it is not new exposure,
    /// because both callers run inside `prepare`, and
    /// [#65](https://github.com/invakid404/umbra/issues/65)'s widening of `bind`
    /// does not make it new exposure either. A crash in that window leaves a
    /// durable `Prepare`
    /// that a reopen classifies rather than refuses, and every intent whose arm
    /// *creates* through this path poisons there (`replay_must_poison`). The
    /// intents that reconcile reach this function only through `copy_up`, whose
    /// materialised objects this contract has accepted as surviving residue
    /// since #56: an object wearing umbra's ownership instead of the base's is
    /// the same divergence a *committed* copy-up leaves behind, not a path the
    /// run did not already expose.
    ///
    /// Two error kinds are **refusals to carry**, not failures, and both leave
    /// the object wearing umbra's ownership and let the operation stand:
    ///
    /// - `Denied` is the privilege answer. A non-root process cannot give an
    ///   object away to another uid, and a base tree owned by root is the
    ///   ordinary case for an overlay, not an exotic one.
    /// - `UnsupportedCapability` is the *store's* answer, and it is the same
    ///   class of refusal arriving under a different kind. An NFS export that
    ///   does not honour SETATTR owner attributes answers `NFS4ERR_NOTSUPP`, and
    ///   a mounted export that cannot chown answers `ENOTSUP`; both land here as
    ///   `UnsupportedCapability`. `umbra-storage-nfs` now qualifies the
    ///   capability with a live probe before advertising it, so this should not
    ///   be reachable there -- but a probe is a measurement at `open_run` and an
    ///   export can change under a live run, and `umbra-storage-nfs-userspace`
    ///   advertises from its capability table rather than from a probe. Treating
    ///   it as fatal would mean a store that merely declines one metadata update
    ///   kills the run.
    ///
    /// Failing on either would mean "you may never overlay a tree you do not
    /// own" or "you may never overlay onto a store that will not chown", and
    /// `parents` runs inside `prepare` where a failure poisons the run -- so
    /// fail-closed would not degrade the session, it would kill it. The object
    /// keeps umbra's ownership, which is byte-for-byte what it had before this
    /// function existed, and the divergence is recorded rather than swallowed.
    ///
    /// `NotImplemented` is deliberately **not** in that set. It names a deferred
    /// or unbound code path rather than a store declining a supported request,
    /// and swallowing it would hide exactly the wiring gap it exists to report.
    /// Every other error propagates exactly as a `create` failure does today.
    ///
    /// This does not weaken the unchanged-ID `Fchownat` guarantee, which is the
    /// one contract a silent fallback could have undermined. `resolve` admits
    /// that sentinel only when `ownership_will_carry` holds, and that predicate
    /// is `(stat.uid, stat.gid) == identity_at(path)` -- the base object already
    /// wears the identity `create` gives the shadow, whether that is the shadow
    /// root's (the fallback) or a materialised shadow parent's (the widening
    /// #84 adds). So on the path where the sentinel was admitted, a refused
    /// carry is a refused *no-op*: the shadow object carries the base's uid and
    /// gid either way, and the ID the tracee asked to leave alone is left alone.
    ///
    /// The carry's own verdict is deliberately **not** returned. The one decision
    /// that depends on it -- whether `resolve` admits an unchanged-ID `Fchownat`
    /// -- has to be made before `prepare` flushes a journal record, which is
    /// before this runs at all, so it is answered ahead of time by
    /// `ownership_will_carry`. A second answer derived here, after the fact,
    /// would be a second source of truth for one question.
    fn carry_ownership(&mut self, path: &StoragePath, owner: (u32, u32)) -> Result<()> {
        if !self.ownership_fidelity() {
            return Ok(());
        }
        let context = self.context()?;
        let result = self.storage.execute(&StorageRequest {
            context,
            operation: StorageOperation::SetMetadata {
                path: path.clone(),
                update: MetadataUpdate {
                    mode: None,
                    uid: Some(owner.0),
                    gid: Some(owner.1),
                    accessed_nanos: None,
                    modified_nanos: None,
                },
            },
        });
        match result {
            Ok(StorageResponse::MetadataSet(_)) => Ok(()),
            Ok(_) => Err(error(
                ErrorKind::ProtocolMismatch,
                "invalid set_metadata response",
            )),
            Err(e) if matches!(e.kind, ErrorKind::Denied | ErrorKind::UnsupportedCapability) => {
                tracing::info!(
                    uid = owner.0,
                    gid = owner.1,
                    error = %e,
                    "shadow object keeps umbra ownership: carrying the base's was refused"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    /// The uid/gid this backend gives an object umbra creates with **no
    /// materialised parent** -- i.e. directly under the shadow root.
    ///
    /// Read from the shadow root, which the backend materialised when it opened
    /// the run and which nothing else has chowned. Asking the backend rather
    /// than the host is what keeps this correct for all four of them: tar
    /// answers with its archive convention, the userspace NFS client answers
    /// with whatever the server assigned, and the two kernel-VFS backends answer
    /// with the identity their kernel gives a new object under the root -- which
    /// is the process's own identity on Linux, but on macOS is the process's uid
    /// and the *root directory's* gid, because BSD inherits the parent's gid
    /// unconditionally. It is the backend's answer, not the host's, that a
    /// carried chown has to match, and this is that answer only for the root's
    /// own children; `identity_at` reads a materialised parent instead once one
    /// exists.
    fn shadow_identity(&mut self) -> Result<(u32, u32)> {
        let root = StoragePath::new(StorageAnchor::Root, Vec::new())?;
        let stat = self
            .shadow_stat(&root)?
            .ok_or_else(|| error(ErrorKind::InvalidState, "shadow root is absent"))?;
        Ok((stat.uid, stat.gid))
    }
    /// The shadow path of `path`'s prospective parent directory, or `None` when
    /// `path` names a top-level object whose parent is the anchor root.
    ///
    /// A `None` and a `Some(root)` would give `identity_at` the same answer --
    /// the anchor root always exists and its identity is exactly what
    /// `shadow_identity` reads -- so `None` simply lets the fallback issue the
    /// one stat rather than issuing it twice. Mirrors the inline parent
    /// computation `parents` performs, byte for byte.
    fn parent_of(path: &StoragePath) -> Result<Option<StoragePath>> {
        match path.as_bytes().iter().rposition(|b| *b == b'/') {
            Some(end) => Ok(Some(StoragePath::new(
                path.anchor(),
                path.as_bytes()[..end].to_vec(),
            )?)),
            None => Ok(None),
        }
    }
    /// The uid/gid this backend will give an object umbra creates at `path`.
    ///
    /// **Case A -- the shadow parent already exists** (a sibling was copied up
    /// earlier in the run) and the backend advertises `storage-parent-identity-v1`:
    /// the answer is `shadow_stat` of that parent, an *observation* of the
    /// identity a new child under it will inherit. Exact, because it is a fact
    /// the run has already produced, not a guess about one it might.
    ///
    /// The caller compares the full `(uid, gid)` pair against this, even though
    /// a kernel-VFS backend inherits only the parent's gid and gives the child
    /// the creating process's euid for uid (tar inherits both). That stays exact
    /// on the strength of the premise `STORAGE_PARENT_IDENTITY_V1` records and a
    /// future advertiser must preserve: a shadow parent wears a uid other than
    /// the creator's euid only after a *successful*, hence privileged, carry --
    /// under which the child's own carry also succeeds -- so the pair can differ
    /// on uid only where the carry it gates is guaranteed anyway.
    ///
    /// **Case B -- the shadow parent is not yet materialised, or the backend
    /// does not advertise the name:** falls back to `shadow_identity` -- today's
    /// exact answer.
    ///
    /// The split is the whole point, and it is a soundness constraint, not an
    /// optimisation. At `resolve` time the shadow parent *usually does not exist
    /// yet* (`parents` materialises the shadow ancestor chain inside `prepare`,
    /// after the predicate has answered). Predicting its eventual identity from
    /// the **base** parent would fail open: `parents` carries the base owner
    /// onto the new shadow ancestor with `carry_ownership`, which **swallows a
    /// `Denied` carry**, so a base parent owned by a uid umbra cannot take would
    /// be predicted as the shadow parent's identity, the sentinel admitted, the
    /// durable `Chown` flushed -- and then the carry refused and the object left
    /// wearing umbra's identity, a silent re-owning with a journal record saying
    /// otherwise. Reading the *shadow* parent (which exists only once a real
    /// carry has already decided its identity) or falling back to the root
    /// cannot make that mistake: the widening happens only against an identity
    /// that is already a fact.
    fn identity_at(&mut self, path: &StoragePath) -> Result<(u32, u32)> {
        if self.parent_identity_backend() {
            if let Some(parent) = Self::parent_of(path)? {
                if let Some(stat) = self.shadow_stat(&parent)? {
                    return Ok((stat.uid, stat.gid));
                }
            }
        }
        self.shadow_identity()
    }
    /// Whether materialising `stat`'s object at `path` in the shadow will leave
    /// it wearing the base's ownership -- answered *before* the materialisation
    /// happens.
    ///
    /// `resolve` has to decide the `Fchownat` sentinel question before `prepare`
    /// flushes a `JournalIntent::Chown`, so it cannot wait for the carry and
    /// read the result. It predicts instead, and the prediction is sound rather
    /// than optimistic: it admits the sentinel only where the base object
    /// already wears the identity the shadow `create` would give it anyway, so a
    /// refused carry there is a refused *no-op*. `identity_at` supplies that
    /// identity -- the materialised shadow parent's where one exists, the shadow
    /// root's otherwise -- so the comparison widens against an observation and
    /// never against a prediction. Anything else (another user's object, a
    /// root-owned tree) may or may not be permitted, cannot be known without
    /// trying, and so answers `false` and keeps the refusal. That is per object,
    /// which is what privilege to chown actually is: umbra may own one base file
    /// and not the one beside it.
    fn ownership_will_carry(&mut self, stat: &BlobStat, path: &StoragePath) -> Result<bool> {
        if !self.ownership_fidelity() {
            return Ok(false);
        }
        Ok((stat.uid, stat.gid) == self.identity_at(path)?)
    }
    /// Materialise the shadow parent directories of `path`, returning the ones
    /// that were *logical* creations, in walk (root-to-leaf) order.
    ///
    /// The answer used to be shadow-shaped: decided by `shadow_stat` alone, so an
    /// ancestor that already existed in the base but had not been copied up yet
    /// was reported as created, and the since-retired `Pending.created` latch
    /// inherited the over-approximation. That is what made a refused cross-path rename onto a
    /// base-only destination parent poison rather than reconcile — named in the
    /// README as #53 under-delivering, and held in place by the missing rollback
    /// [#55](https://github.com/invakid404/umbra/issues/55) tracks.
    ///
    /// It is now logical, and the evidence was already being gathered and
    /// discarded: `shadow_parent_mode` stats the base for the *mode*
    /// ([#56](https://github.com/invakid404/umbra/issues/56)) and carries a
    /// `WhiteoutScan` across the walk, which is exactly what separates
    /// "materialised a shadow over a base directory the run already exposes"
    /// from "created a directory that logically did not exist". Only the second
    /// is reported here. A caller that records the answer gets the ancestor
    /// removed on a reconciled abort; one that discards it leaves the ancestor
    /// standing, which is the right answer for both — see below:
    ///
    /// | evidence | verdict |
    /// |---|---|
    /// | base holds a directory, no whiteout hides it | shadow of an existing directory — not a creation |
    /// | a whiteout hides it | the base directory is logically deleted; this is a new one |
    /// | the base holds nothing, or not a directory | a genuine logical creation |
    /// | the base stat failed | evidence unavailable — report it, and remove it |
    ///
    /// The last row used to read "fail closed", and
    /// [#64](https://github.com/invakid404/umbra/issues/64) **retired it rather
    /// than reversing it**. `shadow_parent_mode` deliberately swallows base
    /// errors, because propagating one would fail a `create` that succeeds today
    /// and would take `commit` — and with it #53's reconciliation — down; the
    /// creation decision therefore had to guess what the swallow hid, and guessed
    /// "a creation", because a directory could not be removed and a wrong guess
    /// the other way would publish a phantom path forever. Removal makes the
    /// guess unnecessary under *both* readings: if the base does hold a directory
    /// at that name, the shadow was a benign uncopied shadow and removing it is
    /// harmless; if it holds nothing, the shadow was a phantom and removing it is
    /// required. The mode half of the swallow is untouched.
    ///
    /// The answer is **returned, never pushed**, and the decision of what to do
    /// with it belongs to the caller. Three of `create`'s callers — `copy_up`,
    /// `write_control` and `set_whiteout` — reach this walk while deliberately
    /// keeping their own object off `Pending.rollback`, and so must discard the
    /// verdict too: queueing an ancestor from here would queue a directory whose
    /// contents are not queued, and the `rmdir` would meet `ENOTEMPTY`.
    ///
    /// That is the test a new `create` caller has to apply, and it is about the
    /// *object*, not the caller: **record the ancestors exactly when the object
    /// under them is recorded.** `create_symlink` was a fourth discarder until
    /// [#69](https://github.com/invakid404/umbra/issues/69) and is no longer one,
    /// because its object is now recorded — but only on one of its two paths, so
    /// it does not simply move to the other list either. It *forwards*: the
    /// `FsOp::Symlink` prepare arm queues what it forwards, while `copy_up`, whose
    /// symlink is not a creation, drops it. Its two `write_control` calls reach
    /// this walk separately, under Control, and still discard there — those
    /// ancestors are the `symlinks/targets/` and `symlinks/objects/` directories,
    /// shared by every symlink in the run, so recording them would make a second
    /// live symlink meet `ENOTEMPTY` and poison a run that should have reconciled.
    ///
    /// See `create` for the same split from the caller's side, and
    /// `Pending.rollback` for what the prepare arms that read the verdict do
    /// with it.
    fn parents(&mut self, path: &StoragePath) -> Result<Vec<StoragePath>> {
        let parts: Vec<_> = path.as_bytes().split(|b| *b == b'/').collect();
        let mut created = Vec::new();
        let mut whiteout = WhiteoutScan::Unscanned;
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
                let ancestor = self.shadow_parent_mode(&parent, &mut whiteout)?;
                let context = self.context()?;
                self.storage.create(
                    &context,
                    &parent,
                    &CreateOptions {
                        kind: CreateKind::Directory,
                        mode: ancestor.mode,
                    },
                )?;
                // Ownership after the create, because `CreateOptions` carries a
                // mode and nothing else, and after `SHADOW_OWNER_BITS` has
                // already been folded into that mode: a successful chown by an
                // unprivileged process clears setuid/setgid but leaves the owner
                // permission bits alone, so the widening survives. Ordering the
                // other way is not available anyway -- there is no object to
                // chown until the create has made one.
                //
                // `SHADOW_OWNER_BITS` is *more* load-bearing after a carry, not
                // less. Once this ancestor wears the base's uid, umbra is no
                // longer its owner, so POSIX applies the `other` bits to umbra's
                // own writes into it -- and a `0o555` base directory carried to
                // root would leave the next `create` inside it with `r-x` and an
                // `EACCES` that poisons the run. The widening is what keeps that
                // from happening, on the carry path exactly as on the fallback
                // path.
                //
                // `shadows_base_directory` is exactly the arm the base
                // corroborated -- a live, legible, un-whiteouted base directory
                // -- and so is exactly the set of ancestors with a counterpart
                // whose ownership there is to carry. Everything else is the
                // deliberate half of the policy, not a gap: an ancestor that
                // shadows no base directory, and one materialised over a
                // whiteouted grave, are *new* directories and belong to umbra.
                // They must no more wear a base object's uid than its mode.
                //
                // The base is asked a second time here rather than having
                // `shadow_parent_mode` hand the uid/gid back on its return
                // value, and the cost is one extra `stat` per carried ancestor
                // against an immutable base. The gate is ordered so that cost is
                // only paid where a carry can actually happen: against a backend
                // that has not qualified ownership fidelity this walk's base
                // traffic is byte-identical to what it was before the carry
                // existed, which is what makes the degraded mode a true no-op
                // rather than a nearly-identical one.
                if ancestor.shadows_base_directory && self.ownership_fidelity() {
                    // Swallowed exactly as `shadow_parent_mode` swallows its own
                    // base error and for the same reason: `parents` runs inside
                    // `prepare`, where propagating costs the run and losing one
                    // directory's ownership fidelity costs one directory's
                    // ownership fidelity.
                    if let Ok(stat) = self.base().stat(&parent) {
                        self.carry_ownership(&parent, (stat.uid, stat.gid))?;
                    }
                }
                // Only the ancestors that shadow nothing count. One that shadows
                // a live base directory publishes no *path* the run did not
                // already expose, so leaving it standing after a reconciled
                // abort adds no path-level divergence.
                //
                // It is not free of divergence altogether, and the claim is
                // deliberately not made that broadly: #56 widens the owner bits
                // (`SHADOW_OWNER_BITS`), so a base directory at `0o555` reads
                // back as `0o755` once materialised, and `lookup` prefers the
                // shadow stat from then on. That is the same mode divergence the
                // commit path has already accepted since #56 — the reconciled
                // abort inherits it rather than introducing it, and `copy_up`
                // has been leaving it behind on this exact walk all along.
                if !ancestor.shadows_base_directory {
                    created.push(parent);
                }
            }
        }
        Ok(created)
    }
    /// What one missing shadow ancestor has to be materialised as, and whether it
    /// can be said to shadow a base directory that logically exists.
    ///
    /// `whiteout` carries the scan across one `parents` walk; see `Whiteout` for
    /// why the first ancestor needs every prefix checked and later ones do not.
    fn shadow_parent_mode(
        &mut self,
        parent: &StoragePath,
        whiteout: &mut WhiteoutScan,
    ) -> Result<ShadowAncestor> {
        // `Base` is Root-anchored only — `lookup` answers `Denied` for anything
        // else — and `write_control` and `set_whiteout` drive this walk over
        // Control paths for symlink blobs and whiteout markers. Those ancestors
        // shadow nothing, so there is nothing to ask the base about.
        if parent.anchor() != StorageAnchor::Root {
            return Ok(ShadowAncestor::unshadowed());
        }
        match whiteout {
            WhiteoutScan::Hidden => {}
            WhiteoutScan::Unscanned => {
                *whiteout = if self.marked_prefix(parent)? {
                    WhiteoutScan::Hidden
                } else {
                    WhiteoutScan::Clear
                };
            }
            WhiteoutScan::Clear => {
                if self.marked(parent)? {
                    *whiteout = WhiteoutScan::Hidden;
                }
            }
        }
        // A whiteouted base directory is logically deleted. The shadow directory
        // materialised over its grave is a new directory, not that one, and must
        // not wear its mode.
        if matches!(whiteout, WhiteoutScan::Hidden) {
            return Ok(ShadowAncestor::unshadowed());
        }
        // Ordering: `bind` sets `config` and `base` in the same call, so a bound
        // config proves `base()`'s `expect("bound base")` cannot fire.
        self.config()?;
        match self.base().stat(parent) {
            // Group and other bits exactly as the base grants them; owner bits
            // widened to at least rwx because umbra owns the shadow and has to be
            // able to write into it (see `SHADOW_OWNER_BITS`). Masking rather than
            // truncating keeps setgid and sticky, which are part of the
            // directory's identity, and `LocalStorage` rejects `mode & !0o7777`
            // outright.
            Ok(stat) if stat.kind == ObjectKind::Directory => Ok(ShadowAncestor {
                mode: stat.mode & 0o7777 | SHADOW_OWNER_BITS,
                shadows_base_directory: true,
            }),
            // Never inherit a file's or a symlink's mode onto a directory.
            Ok(_) => Ok(ShadowAncestor::unshadowed()),
            // Deliberate swallow, not an oversight. This path never touched the
            // base at all before #56, so propagating a base error out of `parents`
            // would turn creates that succeed today into hard failures — and
            // `parents` runs inside `commit`, where a failure poisons the run and
            // takes #53/#59's kernel-refusal reconciliation down with it.
            // Swallowing costs one directory's mode fidelity; propagating costs
            // the run. NotFound is the ordinary case here (a shadow ancestor with
            // no base counterpart at all) and is not an error worth reporting.
            //
            // Swallowing it for the *mode* is not swallowing it for the creation
            // verdict: `shadows_base_directory` stays false here, so `parents`
            // reports a logical creation and the ancestor joins the rollback
            // list. Since #64 that is no longer a guess rounded one way — the
            // shadow is removed on a reconciled abort, which is the right answer
            // whether or not the base holds a directory there. See `parents`.
            Err(_) => Ok(ShadowAncestor::unshadowed()),
        }
    }
    /// Whether a whiteout marker exists for `path` itself.
    ///
    /// Non-latching, and that is the point: `whiteouted` sets `self.whiteout_hit`
    /// — the latch `hidden_or` consumes to turn a later NotFound into
    /// Deny(ENOENT) (#49/#54) — and this runs from `prepare` and `commit`, after
    /// resolve has returned, so a latch set here would outlive the operation that
    /// set it. `resolve` clearing the latch on entry happens to mask that today,
    /// but the guarantee #49 rests on is "only a real whiteout lookup latches",
    /// not the order two unrelated functions run in.
    fn marked(&mut self, path: &StoragePath) -> Result<bool> {
        let marker = Self::marker(path)?;
        let context = self.context()?;
        Ok(absent(self.storage.stat(&context, &marker))?.is_some())
    }
    /// Whether `path` or any prefix of it carries a whiteout marker. Walks the
    /// same prefixes `whiteouted` does, without its latch.
    fn marked_prefix(&mut self, path: &StoragePath) -> Result<bool> {
        // Prefixes are rebuilt Root-anchored, as `whiteouted` does, so a
        // Control-anchored argument would silently probe the wrong tree. The only
        // caller sits behind `shadow_parent_mode`'s anchor guard; say so here so a
        // future one cannot lose that quietly.
        debug_assert_eq!(path.anchor(), StorageAnchor::Root);
        let mut prefix = root(b"")?;
        for component in path
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
        {
            prefix = join(&prefix, component)?;
            if self.marked(&prefix)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// Create one shadow object and every shadow ancestor it needs, forwarding
    /// `parents`'s verdict on those ancestors — *not* on the object itself,
    /// which every caller already knows it asked for.
    ///
    /// Three callers discard the answer, and none of them needs it. `copy_up` and
    /// `write_control` duplicate or annotate what the run already exposes, and
    /// `set_whiteout` writes under Control, which shadows nothing at all.
    /// Discarding here is what keeps those three off `Pending.rollback` — see
    /// `parents` for why that matters and is not merely tidy.
    ///
    /// `create_symlink` used to be a fourth, on the premise that the whole
    /// operation latched and an ancestor verdict could not change the caller's
    /// mind. [#69](https://github.com/invakid404/umbra/issues/69) removed the
    /// latch, so it *forwards* the placeholder's verdict now (a
    /// `symlink("newdir/link", …)` materialises `newdir/` over nothing, and a
    /// reconciled abort that left it standing would publish exactly the phantom
    /// path the gate exists to prevent). Forwarding is not recording: its
    /// `copy_up` caller still discards, and its two `write_control` calls still
    /// reach this walk under Control and discard there.
    ///
    /// The callers that read the answer are `prepare`'s creating-`Open`, `Mkdir`
    /// and `Symlink` arms.
    fn create(
        &mut self,
        path: &StoragePath,
        kind: CreateKind,
        mode: u32,
    ) -> Result<Vec<StoragePath>> {
        let created_parents = self.parents(path)?;
        let context = self.context()?;
        self.storage
            .create(&context, path, &CreateOptions { kind, mode })?;
        Ok(created_parents)
    }
    /// Record the ancestors `parents` materialised over nothing for rollback.
    ///
    /// In walk order, before whatever the arm creates under them, so the reverse
    /// walk in `abort` unwinds leaf-first and every `remove_directory` meets an
    /// empty directory. Called only from the prepare arms that read the verdict;
    /// see `parents` for why this is not `parents`' own job.
    fn queue_ancestors(&mut self, ancestors: Vec<StoragePath>) {
        self.pending
            .as_mut()
            .unwrap()
            .rollback
            .extend(ancestors.into_iter().map(RollbackEntry::Directory));
    }
    fn copy_up(&mut self, path: &StoragePath) -> Result<()> {
        let (stat, shadow) = self.lookup(path)?;
        if shadow {
            return Ok(());
        }
        if stat.kind == ObjectKind::LogicalSymlink {
            let target = self.base().read_link(path)?;
            // Discarded, deliberately, all of it. Copy-up is not a creation (see
            // the note on this function and `Pending.rollback`), so neither the
            // symlink's own three objects nor the ancestors materialised for its
            // placeholder go on the rollback list: recording them would poison
            // the first refused write into a not-yet-shadowed base subdirectory,
            // which is a large share of the ordinary `EPERM` cases #53 exists to
            // survive.
            self.create_symlink(path, &target, stat.object_id)?;
            return Ok(());
        }
        if stat.kind != ObjectKind::File {
            return Err(unsupported("recursive directory copy-up is deferred"));
        }
        // `| SHADOW_OWNER_BITS`, for the reason `parents` has carried it since
        // #56 and this site never did: the write loop below reopens the object
        // it just created, and both syscall backends reopen it *write-only*
        // (`OpenOptions::write(true)` in `umbra-storage-local`, `O_WRONLY` in
        // `umbra-storage-nfs`). Opening your own `0o444` file `O_WRONLY` is
        // `EACCES`, so copy-up of a read-only base file with content has been
        // failing here all along, whatever its ownership -- a latent defect this
        // line fixes and `copy_up_of_a_read_only_base_file_carries_content_and_
        // base_ownership` pins. It is also the precondition for the carry below:
        // the chown is emitted after the writes, so the widening has already
        // done its work by the time umbra stops being the owner.
        //
        // The cost is the mode divergence #56 already accepted for directories,
        // now also on files: a `0o444` base file reads back `0o744` in the
        // shadow. This PR neither widens nor narrows that divergence's
        // *justification*; it applies the existing one at the second of the
        // engine's two create sites, which is what makes them symmetric.
        self.create(
            path,
            CreateKind::File,
            stat.mode & 0o7777 | SHADOW_OWNER_BITS,
        )?;
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
        // Last, after every write: see the `create` above for why the order is
        // load-bearing rather than incidental. `stat` is the base object's, so
        // `uid`/`gid` are the base's -- `lookup` returned it from `self.base()`
        // precisely because the shadow had nothing, which is the definition of
        // the case this function exists for.
        //
        // A `Denied` carry leaves the object exactly as copy-up left it before
        // this PR; see `carry_ownership` for why no verdict comes back from it.
        self.carry_ownership(path, (stat.uid, stat.gid))?;
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
        self.whiteout_hit = false;
        if let FsOp::ReadDir { fd, max_bytes } = operation {
            return self.resolve_directory(context, operation, *fd, *max_bytes);
        }
        let (dir, name, create_parents) = match operation {
            FsOp::Open {
                dir, path, flags, ..
            } => (*dir, path, flags.create),
            FsOp::Stat { dir, path, .. }
            | FsOp::Access { dir, path, .. }
            | FsOp::Fchownat { dir, path, .. }
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
            // Following is the POSIX default for both; the decoded flag says so
            // explicitly, so neither may inherit the nofollow default below.
            FsOp::Access { flags, .. } => flags.follow,
            FsOp::Fchownat { flags, .. } => flags.follow,
            _ => false,
        };
        // Hoisted above path resolution so the traversal-failure path can consult
        // it: a whiteouted ancestor makes `resolve_path_follow` itself fail with
        // NotFound before the final-component lookup runs. Depends only on
        // `operation`, so this is a pure move.
        let mutation = matches!(
            super::dispatch(operation),
            super::Dispatch::Materialise | super::Dispatch::Whiteout
        );
        let path = match self.resolve_path_follow(context, dir, name, create_parents, follow_final)
        {
            Ok(path) => path,
            Err(e) => return self.hidden_or(e, mutation),
        };
        let existing = absent(self.lookup(&path))?;
        if mutation && path.as_bytes().is_empty() {
            return Err(error(ErrorKind::Denied, "cannot mutate namespace root"));
        }
        let mut destination = None;
        match operation {
            FsOp::Open { flags, .. } => {
                if existing.is_none() && !flags.create {
                    return self
                        .hidden_or(error(ErrorKind::NotFound, "open target absent"), mutation);
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
                    // The merged view, not the shadow half and not the base
                    // half. Two cases decide this and they point opposite ways:
                    // a base directory whose children were all unlinked in-run
                    // is empty although `Base::list` still reports them
                    // (`merged` filters them at the per-name whiteout pass),
                    // and an opaque recreated directory is empty although the
                    // base is full (`merged` skips the base enumeration
                    // entirely when the directory's own path is whiteouted).
                    // Only one implementation of the merged view knows both
                    // rules, and `ReadDir` answers the tracee from that same
                    // one -- so the emptiness this refuses on is the emptiness
                    // the tracee can observe.
                    //
                    // An `Err`, not `Emulate(Failure(ENOTEMPTY))`, joining the
                    // three sibling POSIX refusals in this function
                    // (`Mkdir`/`AlreadyExists`, absent-target/`NotFound`,
                    // kind-mismatch/`InvalidPath`). The supervisor refuses an
                    // `Emulate` only *after* `prepare` has journaled, so a
                    // failure-shaped `Emulate` would leave a dangling `Prepare`
                    // for an unlink that never happened; and the engine has no
                    // view of the tracee ABI, where `ENOTEMPTY` is 39 on Linux
                    // and 66 on macOS/BSD -- `Errno::ENOENT` is the sole named
                    // constant precisely because it is ABI-stable. `Denied` is
                    // this function's idiom for "POSIX says no" and is
                    // distinguishable from the kind mismatch two lines above.
                    //
                    // `merged` latches `whiteout_hit` through `whiteouted`.
                    // Harmless here, and the obvious reason is the wrong one:
                    // there *is* a `hidden_or` site below this, in the `_ =>`
                    // arm of this same `match`, so "every site is above" would
                    // be false. What actually holds is that the arm cannot run
                    // for an `FsOp::Unlink` -- the arms are mutually exclusive
                    // -- and that `resolve` clears the latch at its head on
                    // every call, so nothing this sets can carry into another.
                    // Stated so a future reorder has to preserve those two
                    // facts rather than a claim about line order.
                    //
                    // Accepted cost, recorded here rather than left to be
                    // discovered: `merged` pages the base half to exhaustion
                    // and stat-fills every surviving entry before `is_empty`
                    // reads the first of them, so an `rmdir` of a 100k-child
                    // directory pays 100k round trips to answer a one-entry
                    // question POSIX answers in one. Not a regression --
                    // `ReadDir` reaches the same function and already pays this
                    // shape -- and deliberately not fixed at this call site: an
                    // early-out here would be a second merged-view
                    // implementation, which is precisely what the bifurcated
                    // predicate was rejected for. The fix is an entry limit
                    // honoured *inside* `merged`, on a hot path `Overlay::list`
                    // and `resolve_directory` share; that is a wider change
                    // than this arm, and is left as a follow-up.
                    if !self.merged(&path)?.is_empty() {
                        return Err(error(ErrorKind::Denied, "rmdir target is not empty"));
                    }
                }
            }
            // Refuse here rather than at prepare. `copy_up` rejects a base-only
            // directory, but prepare appends and flushes the Chown intent before
            // it runs, so reaching that failure would leave a durable record of
            // an ownership change that never happened, with the session poisoned
            // and neither Commit nor Abort written. A shadow directory is fine:
            // copy_up returns early for anything already materialised. The
            // condition mirrors `copy_up`'s own; prepare keeps its call as the
            // backstop.
            FsOp::Fchownat { uid, gid, .. } => {
                let (stat, shadow) = existing
                    .as_ref()
                    .ok_or_else(|| error(ErrorKind::NotFound, "chown target absent"))?;
                if !shadow {
                    if !matches!(stat.kind, ObjectKind::File | ObjectKind::LogicalSymlink) {
                        return Err(unsupported("directory chown requires recursive copy-up"));
                    }
                    // The sentinel means "unchanged", so it is honourable only
                    // if the shadow object the kernel is about to chown wears
                    // the base object's ownership. Copy-up now carries it --
                    // `copy_up` emits a `SetMetadata` after the create, because
                    // `CreateOptions` still carries a mode and no uid/gid -- so
                    // the refusal is no longer about a missing mechanism. What
                    // remains is privilege, and privilege to chown is **per
                    // object**: umbra may own one base file and not the one
                    // beside it.
                    //
                    // So the refusal is per object too. `ownership_will_carry`
                    // answers for this target only, and answers `false` unless
                    // the carry is guaranteed -- it has to be decided here,
                    // ahead of `prepare`, because `prepare` appends and flushes
                    // the `Chown` intent before `copy_up` runs and a failure
                    // after that would leave a durable record of an ownership
                    // change that never happened, with the session poisoned and
                    // neither Commit nor Abort written. Refusing at `resolve`,
                    // before any journal record exists, is the same discipline
                    // this arm has always applied; only the condition narrowed.
                    //
                    // Once the object is in the shadow copy-up is a no-op and
                    // the sentinel is safe, and a chown that sets both IDs
                    // explicitly inherits nothing from the copy.
                    if (uid.is_none() || gid.is_none())
                        && !self.ownership_will_carry(stat, &path)?
                    {
                        return Err(unsupported(
                            "unchanged-ID chown of a base object umbra cannot take ownership of",
                        ));
                    }
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
                    return self.hidden_or(error(ErrorKind::NotFound, "target absent"), mutation);
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
            // A logical symlink is a placeholder regular file in the shadow, so
            // a non-following probe must never reach it: the kernel would answer
            // from the placeholder's own mode instead of the 0o777 `logical_stat`
            // substitutes for every logical symlink. At that mode every requested
            // mode bit is granted, whichever identity the check uses, so this
            // answer is also what a native `faccessat` on an `lrwxrwxrwx` link
            // returns. `!flags.follow` is spelled out rather than left to the
            // fact that a following probe resolves past the link: the guard
            // should not depend on that invariant holding elsewhere.
            FsOp::Access { flags, .. }
                if !flags.follow
                    && existing
                        .as_ref()
                        .is_some_and(|(s, _)| s.kind == ObjectKind::LogicalSymlink) =>
            {
                success()
            }
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

/// Whether an operation a reopen finds prepared-but-unfinished must poison the
/// session rather than be discarded.
///
/// This is the durable half of the rule `Pending.rollback` states in memory, and
/// the reason it needs no journal record of its own is an ordering that has
/// always held: `prepare` appends and *fsyncs* its `Prepare` record before any
/// arm below it creates anything. There is therefore no window in which a
/// prepare-time creation is durable and its intent is not, and the durable
/// pre-image [#65](https://github.com/invakid404/umbra/issues/65) asks for is
/// that record. What was missing was a reader for it -- this function -- and an
/// `abort` ordering under which an operation *absent* from `pending` says
/// something. It now says: every removal that transaction recorded was performed
/// and reached the store's durability boundary, because `abort` unwinds, flushes
/// the shadow and only then writes the record -- `commit`'s own sequence. Two
/// limits, stated because this whole function exists to stop such things being
/// silent. The flush is skipped when the rollback is empty, where the claim is
/// vacuous because nothing was removed. And the claim is about *this*
/// transaction's recorded removals, not about the tree: an ancestor `copy_up`
/// materialised was never on the list and is not covered, which is the residue
/// `overlay/README.md` has accepted since #56. See `abort`.
///
/// So a `Prepare` with neither a `Commit` nor an `Abort` behind it means: this
/// transaction may have materialised something, and the list that would take it
/// back died with the process that built it. `bind` answers that by poisoning,
/// which is positive evidence acting on a record that is present, never an
/// inference from one that is missing.
///
/// **This function is one half of the test, not the test.** It answers a question
/// about the *intent* -- would a completed `prepare` under this intent have
/// materialised something whose undo is gone -- and that is not sufficient to
/// discard a transaction. `reconcilable_on_reopen` is the whole rule and this is
/// its first conjunct; the second is about the transaction's *stage*, which no
/// intent carries. A third ground sits beside both in `bind`: a session that
/// stopped deliberately, on `abort`'s uncorroborated path, writes a terminal
/// record that removes the transaction from the inventory before any reader sees
/// it, and journals `JournalLifecycle::RecoveryRequired` instead.
///
/// Do not widen an arm here to cover any of that. The intent is not the missing
/// information in any of the three cases, and an arm that tried would be
/// answering a question this function is not asked.
///
/// **The match is exhaustive and has no wildcard arm, deliberately.** The one
/// real hazard here is drift: this is a restatement of which `prepare` arms push
/// to `Pending.rollback`, and a wildcard would let a future `JournalIntent`
/// variant silently inherit a verdict nobody chose -- a false invariant, made
/// durable, which is the failure mode a new record would not have prevented
/// either. Adding a variant is instead a compile error here, at the one site
/// obliged to answer for it. `Overlay::intent`, which produces these, is the
/// other half of any such change.
///
/// **That mechanism rests on `JournalIntent` not being `#[non_exhaustive]`**, and
/// it is worth saying so because nothing else in the tree does. It is declared in
/// `umbra-core` and this crate is a downstream consumer; marking it
/// `#[non_exhaustive]` would force a wildcard arm here and destroy the compile
/// error *silently* -- the code would keep building, and every future variant
/// would inherit whatever that wildcard said. Adding the attribute is therefore a
/// decision about this function, not only about the enum, and the two must be
/// revisited together.
///
/// The hazard the compiler does **not** cover is an existing arm of `prepare`
/// starting to record an undo under an intent this function calls reconcilable:
/// `Overlay::intent` maps several `FsOp`s onto one intent, so no type links the
/// two. A `debug_assert!` at the end of `prepare`'s closure guards that
/// direction.
fn replay_must_poison(intent: &JournalIntent) -> bool {
    match intent {
        // Two `prepare` arms produce this one and both create: the creating
        // `FsOp::Open` and `FsOp::Mkdir`. Each materialises the object plus
        // every ancestor `parents` had to make over nothing, and records them as
        // `RollbackEntry::File`/`Directory`. Nothing durable names those paths
        // as *created by this transaction*, which is the property the undo
        // needs -- a shadow path that exists says nothing about who made it.
        JournalIntent::Create { .. } => true,
        // `FsOp::Symlink` materialises four objects and records three of them as
        // one `RollbackEntry::Symlink`. This arm is the one that could not be
        // reconciled from durable state even in principle, and saying so is the
        // point: the backing index is named `symlinks/objects/<backend object
        // id>`, the shadow assigns that ID to the placeholder, and the intent
        // carries the link path and the *logical* identity but never the
        // backend's. A record rich enough to rebuild the undo could only be
        // written after the placeholder existed, so a crash in that window would
        // land back here regardless. The poison is the only reachable answer,
        // not a conservative default.
        JournalIntent::Symlink { .. } => true,
        // The cross-path `FsOp::Rename` arm materialises the destination's
        // ancestors over nothing and records them. The same-path case creates
        // nothing at all -- `prepare` guards the whole arm on `destination !=
        // plan.path` -- and is poisoned here anyway, deliberately. `from` and
        // `to` are logical paths; proving the guard took the no-op branch would
        // mean re-deriving the plan against a tree this session has not looked
        // at, and being wrong about it publishes a phantom path that answers
        // `AlreadyExists` forever. Failing closed over a transaction that
        // happened to create nothing costs a run that has already crashed.
        JournalIntent::Rename { .. } => true,
        // Neither of these is reachable, and both die at the same place: the
        // `_ =>` arm of `resolve`'s operation match, which answers
        // `UnsupportedCapability` for `FsOp::Link` and `FsOp::Chmod` alike. So no
        // plan for either ever reaches `prepare`, and `Overlay::intent` never
        // runs on one. (`intent` has its own fall-through to `unsupported`, which
        // would also refuse them -- but it is the second line, not the one
        // holding, and a reader sent to check it would be checking the wrong
        // guard.)
        //
        // A durable record carrying one was therefore not written by this
        // engine's `prepare`, and nothing here can say what that `prepare` left
        // behind. That is "don't know", and "don't know" poisons. These are not a
        // wildcard wearing two names: each is written out so that making either
        // reachable forces its arm to be reconsidered beside the `resolve` arm
        // that made it so.
        JournalIntent::Link { .. } | JournalIntent::Chmod { .. } => true,
        // The reconcilable half, and the only thing this widening lets a run
        // survive that it could not before. Every one of these arms creates
        // nothing during `prepare`:
        //
        // - `Unlink` records a plan and performs nothing (#66); the removal
        //   belongs to `commit` alone.
        // - `Chown` and the non-creating `Open` arms (`CopyUp`, `Truncate`,
        //   `Write`) reach `copy_up`, which deliberately keeps its own object
        //   and its ancestors off `Pending.rollback`: copy-up is not a creation
        //   (#53/#55), and counting it would poison a run on the first write
        //   into any not-yet-shadowed base subdirectory.
        //
        // "Nothing was created" is a *positive* property of those arms rather
        // than an inference from a missing record. It is still not sufficient on
        // its own, and twice over -- both times because it describes the arms'
        // *completed* behaviour and says nothing about where the transaction
        // stopped. `reconcilable_on_reopen` supplies the stage; this verdict is
        // only its intent half.
        //
        // **Before `prepare` finished.** The copy-up justification above -- that
        // its residue is the divergence a *committed* copy-up leaves anyway --
        // holds only once `copy_up` has run to the end. `copy_up` is `create`,
        // then a `write_at` loop, then `carry_ownership`; `create_symlink` is
        // the target blob, then the placeholder, then the backing index. Stop in
        // the middle of either and the shadow holds a truncated copy of a base
        // file, or a `0o444` placeholder with no `symlinks/objects/` entry --
        // which `logical_stat` reports as an empty *regular file*, not a logical
        // symlink. `lookup` prefers the shadow, so that is wrong content served
        // at a path the run already exposed, which no committed copy-up ever
        // produces.
        //
        // **After `commit` started.** `whiteouts`, `retired_index` and `destroy`
        // have `commit` as their only consumer, but `commit` applies all of them
        // *before* its terminal record, so "no `Commit` on disk" means "commit
        // may not have finished", not "commit never started". `FsOp::Unlink`'s
        // arm is the one reconcilable intent that populates any of them.
        //
        // Both windows are closed by the same observation, and it is the
        // transaction's stage rather than its intent that closes them. See
        // `reconcilable_on_reopen`.
        JournalIntent::Unlink { .. }
        | JournalIntent::Chown { .. }
        | JournalIntent::CopyUp { .. }
        | JournalIntent::Truncate { .. }
        | JournalIntent::Write { .. } => false,
    }
}
/// Whether a prepared-but-unfinished operation can be discarded by a reopen
/// rather than poisoning it.
///
/// The whole rule, in the positive form, because the positive form is what is
/// actually being asserted: **a pending entry is reconcilable only if its intent
/// implies no creation *and* its observed outcome is a failure.** Everything else
/// poisons. Written as one predicate rather than a list of poison cases because
/// the three ways to be unreconcilable are not independent special cases -- they
/// are the complement of a single narrow window, and enumerating them invited
/// exactly the two escapes that were found after this was first written.
///
/// The second conjunct is the transaction's **stage**, and `observed_result` is a
/// complete discriminator for it:
///
/// - `None` -- `prepare` may not have finished. `observe_result` goes through
///   `pending_for`, which refuses a poisoned session, and `prepare` poisons on
///   any error inside its closure, `copy_up`'s included. So a durable
///   `ObservedResult` *proves* `prepare` returned; its absence proves nothing,
///   and a half-run `copy_up` or `create_symlink` leaves the shadow holding
///   truncated content, or a placeholder with no backing index that reads as an
///   empty regular file. `lookup` prefers the shadow, so that is wrong content at
///   a path the run already exposed.
/// - `Some(Success)` -- `commit`'s precondition was met ("commit requires
///   observed success; abort failures"), so `commit` may have started, and it
///   applies the retired index, the whiteout markers and the `destroy` removal
///   *before* it writes `Commit`.
/// - `Some(Failure)` -- `prepare` completed, and `commit` can never have run for
///   it. This is the only stage at which the intent alone decides anything, and
///   it is the ordinary refused syscall #53 exists to let a run survive.
///
/// Keyed on the stage rather than on which intents happen to materialise, and
/// that is deliberate: a rule naming `CopyUp` and `Chown { copy_up: true }` would
/// be exact today and would be a restatement of which arm calls `copy_up`, with
/// no compiler behind it. "`prepare` may not have finished" is a property of the
/// transaction, not of its intent, and the arm list would drift away from it.
fn reconcilable_on_reopen(operation: &JournalPendingOperation) -> bool {
    !replay_must_poison(&operation.intent)
        && matches!(
            operation.observed_result,
            Some(OperationOutcome::Failure(_))
        )
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
        if self.config.is_some() || self.poisoned {
            return Err(error(
                ErrorKind::InvalidState,
                "namespace already bound or poisoned",
            ));
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
        // Two of the four predicates that stood here are gone
        // ([#65](https://github.com/invakid404/umbra/issues/65)), and the premise
        // behind them is retired rather than relaxed. It used to read: the
        // rollback list dies with the process, so a journal carrying a durable
        // `Prepare` must not be reopened at all. `prepare` has always fsynced
        // that `Prepare` *before* its creating arms, so the durable evidence was
        // never the missing piece; a reader for it was, and so was an `abort`
        // ordering under which an operation absent from `pending` asserts its
        // undo's recorded removals are durable instead of merely attempted.
        // `abort` supplies the second -- unwind, flush the shadow, then record --
        // and `replay_must_poison` the first.
        //
        // `last_valid_sequence` goes with it and not as an afterthought: it is
        // non-zero for *every* run that journaled anything at all, cleanly
        // finished ones included, so leaving it would have made the widening
        // vacuous — nothing reaching the classification could ever pass it.
        //
        // The other two keep refusing, for reasons that were never the retired
        // premise. Checkpoint-based recovery -- reconstructing logical state from
        // a published checkpoint -- is unimplemented; `Supervisor::resume` reopens
        // a run and classifies it, and refuses a checkpoint-bearing recovery for
        // exactly this reason rather than despite it. A torn tail may have eaten the very terminal
        // record this classification reads, and `open` has already physically
        // truncated those bytes under writer authority, so they cannot be
        // re-examined. Both are "don't know", and poisoning is not how to say
        // that: poisoning claims a *specific* transaction was left unreconciled,
        // which is a stronger statement than the evidence supports.
        if config.recovery.checkpoint.is_some()
            || config.recovery.tail != JournalTailRecovery::Intact
        {
            return Err(unsupported(
                "journal recovery with a checkpoint or a torn tail requires M1.5 reconciliation",
            ));
        }
        // Poison, do not refuse. `poisoned` already means "this session requires
        // recovery" and every operation refuses through `idle()`/`pending_for`,
        // so the run is as finished as a refused `bind` would have left it — but
        // it is *open*, and an operator holds a bound session whose recovery
        // state can be read, rather than an error and no handle. That difference
        // is what the widening actually buys today, given that nothing in the
        // shipped supervisor reopens a run at all.
        //
        // `any` rather than a per-operation verdict: one unreconcilable
        // transaction condemns the session, and the classification is not
        // recorded anywhere a later caller could mistake for a repair plan.
        //
        // "Inspected" is narrower than it may sound, and saying so plainly is
        // the point. It does *not* mean the session still serves reads: every
        // `NamespaceSession` entry point gates on `poisoned`, `stat` and
        // `read_link` included -- they reach `idle()` through `typed_path`,
        // which calls it on its first line. Nothing can be read or changed
        // through a poisoned session.
        //
        // What returning `Ok` rather than `Err` buys is therefore about the
        // *caller*, not the tree: the owner ends up holding a bound session
        // whose state is a structured verdict it can act on, instead of an
        // error that leaves it with no session at all and nothing to
        // distinguish "this journal is unreconcilable" from "binding failed".
        // Exposing a read surface on a poisoned session would be a separate
        // decision, and #65 does not make it.
        // Two independent grounds, ORed, and they answer different questions.
        //
        // `recovery_required` is a *declaration*: a previous session journaled
        // `JournalLifecycle::RecoveryRequired` because it aborted a transaction it
        // could not account for and did not undo. It is honoured unconditionally
        // rather than re-derived per intent, and that is the point -- the session
        // that wrote it held the transaction's actual state, while everything
        // available here is the intent alone. A reader with strictly less evidence
        // overruling a writer's explicit verdict is the inversion this whole
        // change exists to remove. It also cannot be recovered from `pending`: the
        // same session wrote the terminal record that empties it.
        //
        // `reconcilable_on_reopen` is an *inference* over what the inventory
        // still shows, for sessions that stopped without declaring anything -- a
        // crash, or a `prepare` that failed part-way and poisoned only itself. It
        // is stated positively, as the one narrow window in which a transaction
        // can be discarded: its intent implies no creation **and** its kernel
        // outcome was an observed failure. See that function for why the stage is
        // half the test and why `observed_result` is a complete discriminator
        // for it.
        //
        // Neither ground subsumes the other, and both poison rather than refuse.
        // `all`, so an inventory with nothing in it is vacuously reconcilable --
        // which is right, and is the E6 case.
        let poisoned = config.recovery.recovery_required
            || !config.recovery.pending.iter().all(reconcilable_on_reopen);
        // Seed the session from the inventory it just accepted. On master the two
        // predicates dropped above made a non-pristine state unbindable, so both
        // fields could start at their empty values and be right by construction;
        // widening is what makes them reachable, so the seeding belongs here
        // rather than in a later issue.
        //
        // `last_committed` feeds the run's terminal evidence. `finish_run` writes
        // `Lifecycle(RunCompleted { through: self.last_committed })` -- the
        // repo's "this run completed cleanly, through here" record -- and
        // `commit`'s non-mutating early return hands the same value back as a
        // receipt. Left at zero, a clean reopen of a journal holding N records
        // would journal a durable claim to have finished through sequence 0.
        // That is a false statement made durable, which is the class of defect
        // this whole change exists to remove, not one to leave behind it.
        //
        // `last_valid_sequence` rather than `durable` is the right source: this
        // is a position in the log, and `durable` is an `Option` naming a
        // *receipt* boundary that may legitimately be absent for a log that was
        // written and never acknowledged. A reopen inherits the log's extent.
        //
        // And "extent" is the precise word, not a loose one for "commit
        // boundary". The last valid frame may be a `Prepare` whose transaction
        // this very `bind` is about to discard, or one it is about to poison
        // over, so `last_committed` here can name a sequence no `Commit` ever
        // reached. That is still the right seed for what reads it -- `finish_run`
        // asks "how far does this run's log go", and `commit`'s non-mutating
        // early return wants the same -- but it is not a durability claim, and
        // nothing should start treating it as one. The field is named for its
        // in-session meaning, where every write to it *is* a commit; a reopen is
        // the one caller for which those two readings come apart.
        self.last_committed = config.recovery.last_valid_sequence;
        // And the operation IDs the inventory still carries. `prepare` refuses a
        // reused ID (`used_operations`), which exists so one ID cannot carry two
        // `Prepare` records; a reopen that forgot the unfinished ones would let a
        // caller spend an ID that already has a durable `Prepare` behind it.
        // `apply_recovery` `insert`s by operation ID, so a duplicate would not
        // corrupt the replay -- it would silently overwrite the older intent,
        // which is worse than refusing.
        //
        // **Only the pending ones, and that is short of what the log spent.**
        // `apply_recovery` drops an operation on either terminal record, so a
        // committed or aborted ID never reaches this function and stays
        // re-spendable across a reopen. Two consequences, neither reached from
        // the binary today: a second `Prepare` under a committed ID would be
        // accepted, and -- because `NEXT_SESSION` restarts at 1 in each process
        // -- the reused seed derives the *same* storage operation ID for the same
        // `(session, serial)` pair as the earlier run while carrying a different
        // idempotency key, which is the collision `OperationId::derive` warns
        // about. Closing it needs `RecoveryState` to carry the spent IDs, which
        // is a wider change than the widening that exposed it; it is recorded as
        // a follow-up rather than guessed at here.
        self.used_operations
            .extend(config.recovery.pending.iter().map(|p| p.operation_id));
        self.config = Some(config);
        self.base = Some(base);
        self.poisoned = poisoned;
        Ok(())
    }
    /// `poisoned` is the whole answer, and reading it needs no gate of its own:
    /// this is the one question a session that cannot serve is still able to
    /// answer, which is what `bind` returning `Ok` on a poison verdict was for.
    fn requires_recovery(&self) -> Result<bool> {
        self.config()?;
        Ok(self.poisoned)
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
        // Read here, while `intent` is still owned, for the drift guard at the
        // end of this closure. See that assertion for what it is guarding.
        let must_poison = intent.as_ref().map(replay_must_poison);
        self.used_operations.insert(operation);
        self.pending = Some(Pending {
            id: operation,
            plan: plan.clone(),
            outcome: None,
            whiteouts: vec![],
            retired_index: None,
            destroy: None,
            rollback: vec![],
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
                        // The object itself is a file, so `abort` can unlink it;
                        // the ancestors materialised over nothing are
                        // directories `abort` can now `rmdir`. Ancestors first
                        // and in walk order, object last, so the reverse walk
                        // empties each directory before removing it. Nothing
                        // latches here any more.
                        let ancestors = self.create(&plan.path, CreateKind::File, *mode)?;
                        self.queue_ancestors(ancestors);
                        let pending = self.pending.as_mut().unwrap();
                        pending
                            .rollback
                            .push(RollbackEntry::File(plan.path.clone()));
                        pending.whiteouts.push((plan.path.clone(), false));
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
                // Four objects, not the three the placeholder-plus-blobs framing
                // names. `create_symlink` writes the target blob, the placeholder
                // and the backing index — and reaching the placeholder through
                // `create` also materialises its shadow ancestors over nothing,
                // exactly as the creating-`Open` arm's do. So the ancestors are
                // queued first, as ordinary `Directory` entries, and the reverse
                // walk in `abort` unwinds them leaf-first like every other
                // materialised ancestor; the other three go in one
                // `RollbackEntry::Symlink`, because they are not independently
                // reachable — the index's *name* is derived from the
                // placeholder's backend object ID, so removing the placeholder
                // first leaves the index underivable and stranded.
                //
                // This arm latched `Pending.created` until
                // [#69](https://github.com/invakid404/umbra/issues/69), on the
                // premise that undoing the set was its own problem. It is this
                // entry now, and there is no latch left to fall back on — see
                // `Pending.rollback`.
                FsOp::Symlink { target, .. } => {
                    let created = self.create_symlink(&plan.path, target, ObjectId(operation.0))?;
                    self.queue_ancestors(created.ancestors);
                    let pending = self.pending.as_mut().unwrap();
                    pending.rollback.push(RollbackEntry::Symlink {
                        backing_index: created.backing_index,
                        target_blob: created.target_blob,
                        placeholder: plan.path.clone(),
                    });
                    pending.whiteouts.push((plan.path.clone(), false));
                }
                // The creating-`Open` arm with `CreateKind::Directory`: the
                // directory this makes and every ancestor materialised over
                // nothing are all removable through `remove_directory`, so the
                // arm records an undo instead of latching. It latched
                // `Pending.created` unconditionally until #64, on the premise that
                // `LocalStorage` could not remove a directory at all — that
                // premise is now false, and after #69 the latch itself is gone.
                //
                // `resolve` answers `Emulate` for `Mkdir` (see the
                // `Symlink | Mkdir | Unlink` arm), so the supervisor's
                // `syscall_exit` never drives a `KernelRefused` abort here today.
                // Wired anyway: leaving a latch standing on a dead premise is
                // worse than either alternative, and `Overlay::abort` is callable
                // directly, so the rule is pinned at the layer that owns it.
                FsOp::Mkdir { mode, .. } => {
                    let ancestors = self.create(&plan.path, CreateKind::Directory, *mode)?;
                    self.queue_ancestors(ancestors);
                    self.pending
                        .as_mut()
                        .unwrap()
                        .rollback
                        .push(RollbackEntry::Directory(plan.path.clone()));
                    // Keep an existing directory whiteout as an opaque-base marker.
                }
                // Nothing is destroyed here, deliberately. This arm used to
                // call `remove_symlink_index` and `storage.unlink` inline, which
                // made an `Unlink` the one `Whiteout`-class transaction with an
                // effect outside `commit`, so any abort of one arrived after the
                // shadow object was already gone. An abort was the *lucky* case
                // and not the reachable one: the supervisor's entry path refuses
                // an `Emulate` only after `prepare` has run, so on the path an
                // intercepted `unlink(2)` actually took, the run was abandoned
                // into recovery with `pending` still set, a dangling `Prepare`
                // record and no `Abort` record at all -- and the object went with
                // it whenever the target had been materialised in the shadow,
                // both steps here having been gated on `shadow_stat`.
                // It is a plan now, consumed in `commit` beside the whiteout it
                // belongs to. See `Pending.destroy`, and `Pending.rollback` for
                // why this needs no entry: there is nothing created to roll
                // back.
                //
                // The discriminant comes straight off the `FsOp` the tracee
                // supplied, which `resolve` has already validated against
                // `stat.kind`, so the plan and the object can never disagree.
                FsOp::Unlink { directory, .. } => {
                    let pending = self.pending.as_mut().unwrap();
                    pending.destroy = Some(if *directory {
                        Destroy::Directory(plan.path.clone())
                    } else {
                        Destroy::File(plan.path.clone())
                    });
                    pending.whiteouts.push((plan.path.clone(), true));
                }
                // Ownership is changed by the kernel against the rewritten
                // shadow path, so the object has to be in the shadow first;
                // otherwise the immutable base would be mutated in place.
                FsOp::Fchownat { .. } => {
                    self.copy_up(&plan.path)?;
                }
                FsOp::Rename { .. } => {
                    let destination = plan.destination.as_ref().unwrap();
                    if destination != &plan.path {
                        self.copy_up(&plan.path)?;
                        // Only parents this call had to create over *nothing*
                        // count. Renaming into a directory already in the shadow
                        // materialises nothing; renaming into one the base holds
                        // but has not been copied up yet materialises a shadow of
                        // a directory the run already exposes, which is equally
                        // reconcilable now that `parents` answers logically. The
                        // destination object itself is not created here - the
                        // kernel's own rename would have made it - so there is
                        // nothing to roll back for the object itself. The
                        // ancestors created over nothing *are* rolled back, in
                        // walk order so the reverse walk unwinds leaf-first.
                        //
                        // `copy_up` above contributes no entry, and the reason
                        // no directory queued here can be non-empty because of it
                        // is **statement order**, not path shape. It is not that
                        // the source lies outside the destination's subtree --
                        // it frequently does not: `rename("d/a", "d/target")`
                        // puts the copied-up source directly inside the
                        // destination's own parent. What holds is that `copy_up`
                        // runs *first*, so every ancestor it had to materialise
                        // already answers `shadow_stat` by the time `parents`
                        // walks, and `parents` only returns ancestors it created
                        // itself. A shared ancestor is therefore excluded from
                        // this list and stays standing, which is also the right
                        // answer: the copied-up file inside it is not ours to
                        // remove. The exclusion therefore depends on this order;
                        // treat the two statements as ordered rather than
                        // independent.
                        let ancestors = self.parents(destination)?;
                        self.queue_ancestors(ancestors);
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
                // Every arm above that materialises anything records its undo;
                // every arm that falls through here materialises nothing. That
                // split is restated durably, one file down, by
                // `replay_must_poison`, which a reopen consults for exactly this
                // question -- and an arm that changes its answer has to carry
                // the change across by hand. The compiler catches only the other
                // direction: a new `JournalIntent` variant.
                _ => {}
            }
            // The drift guard, and the one hazard `replay_must_poison`'s
            // exhaustiveness does *not* cover. The compiler catches a new
            // `JournalIntent` variant; it cannot catch an arm above that starts
            // recording an undo under an intent the classifier already calls
            // reconcilable, because `Overlay::intent` maps several `FsOp`s onto
            // the same intent and nothing links the two sites. That drift would
            // be silent in exactly the way a false invariant is: the run would
            // reopen clean over a creation whose undo list died with the process.
            //
            // **The safe implication only, never the biconditional.** "Recorded
            // an undo => poisons" is what has to hold. The converse must not be
            // asserted: `Rename` poisons and legitimately records nothing when
            // its destination parents already existed, and `Rename { from == to }`
            // poisons while creating nothing at all -- both are deliberate
            // fail-closed choices, and an `if and only if` here would call them
            // defects.
            //
            // `debug_assert!` because it is a statement about this file's own
            // consistency, checkable by the test suite, not a runtime condition a
            // release build should pay for or a caller can provoke.
            debug_assert!(
                self.pending.as_ref().unwrap().rollback.is_empty() || must_poison == Some(true),
                "an arm recorded an undo under an intent replay_must_poison calls reconcilable"
            );
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
        let destroy = pending.destroy.clone();
        let result = (|| {
            if let Some(index) = retired_index {
                let context = self.context()?;
                self.storage.unlink(&context, &index)?;
            }
            for (path, present) in whiteouts {
                self.set_whiteout(&path, present)?;
            }
            // After the markers and before the flush. A shadow object outranks
            // its own whiteout, so a destruction that fails here leaves the path
            // visible -- the pre-transaction view plus an inert marker -- and the
            // session poisons with the object still standing for recovery to
            // find. The other order would leave the object gone and nothing
            // hiding the base, resurrecting a base object at a path the tracee
            // was told is deleted.
            //
            // For a directory that residue argument does **not** transfer
            // verbatim, and inheriting it silently would be wrong. A failed
            // `remove_directory` leaves the shadow directory standing, so
            // `lookup` still resolves it -- but the marker *was* written, so
            // `merged` now short-circuits its base enumeration and the base
            // children are hidden. The residue is "the directory is still there,
            // but its base children are not", which is not the pre-transaction
            // view. The order stays anyway: reversing it makes a *successful*
            // destroy followed by a failed `set_whiteout` expose the whole base
            // directory and every child at a path the tracee was told it had
            // removed, and lying to a live tracee about a completed `rmdir` is
            // worse than an imperfect residue in a session that has already
            // poisoned and whose only reader is recovery.
            match destroy {
                // Index before placeholder, transcribing #69's rule rather than
                // restating it: `remove_symlink_index` derives the index name
                // from a live `shadow_stat` of the placeholder, so the
                // placeholder has to outlive it.
                Some(Destroy::File(path)) => {
                    self.remove_symlink_index(&path)?;
                    if self.shadow_stat(&path)?.is_some() {
                        let context = self.context()?;
                        self.storage.unlink(&context, &path)?;
                    }
                }
                // No `remove_symlink_index`: a directory's object ID never keys
                // a `symlinks/objects/` entry, so the call could only ever be a
                // stat that misses. The `shadow_stat` re-probe is the same one
                // the file arm does and for the same reason (#66) -- a base-only
                // directory has nothing in the shadow to remove, and the marker
                // alone is the whole effect.
                Some(Destroy::Directory(path)) => {
                    // Bound rather than inlined into the `if`: the probe is
                    // fallible, so it cannot be a match guard, and clippy reads
                    // a bare `if` as the sole arm body as one that should have
                    // been.
                    let materialised = self.shadow_stat(&path)?.is_some();
                    if materialised {
                        let context = self.context()?;
                        self.storage.remove_directory(&context, &path)?;
                    }
                }
                None => {}
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
        // Materialisation may already have effects. We do not clear a mutated
        // session or claim those effects were rolled back. An emulated `Unlink`
        // used to belong in that sentence and no longer does: its `prepare`
        // records a plan and performs nothing (#66).
        let mutation = pending.plan.mutation;
        // A kernel refusal is the one abort whose effects are fully accounted
        // for: `prepare` journaled them, `observe_result` journaled the kernel's
        // verdict, and nothing else ran. Reconciling it and letting the tracee
        // see its errno is the contract the supervisor's `syscall_exit` states;
        // poisoning here would kill the run over an ordinary `EPERM`, which is
        // the defect in [#53](https://github.com/invakid404/umbra/issues/53).
        //
        // `reason` is only a claim by the caller, so it is honoured only when
        // the outcome this session itself recorded corroborates it. An abort
        // claiming a refusal for a transaction whose kernel verdict was never
        // observed, or whose observed errno differs from the claimed one, is an
        // interception inconsistency, not a refused syscall: it falls through to
        // the poison path below. Every other reason means the interception broke
        // down and keeps the poison-and-error behaviour unchanged; the two modes
        // never merge.
        // Reconciling undoes what `prepare` created: `rollback` carries every
        // prepare-time logical creation, each entry naming the removal(s) that
        // undo it. There is no second conjunct here any more. A `Pending.created`
        // latch used to stand beside the corroboration for whatever the storage
        // surface could not take back, and
        // [#69](https://github.com/invakid404/umbra/issues/69) retired it with the
        // last arm that set it: what an arm cannot undo is now a missing entry
        // rather than a flag, and a flag no arm sets cannot fail closed. The gate
        // is corroboration alone; the undo's own failure is what poisons below.
        // See `Pending.rollback` for the rule that replaced the latch, and
        // `copy_up` for why copy-up records nothing.
        let rollback = pending.rollback.clone();
        let kernel_refused = matches!(
            (reason, &pending.outcome),
            (AbortReason::KernelRefused(claimed), Some(OperationOutcome::Failure(observed)))
                if claimed == observed
        );
        // The uncorroborated path keeps its `Abort` record exactly where it always
        // has, and the reordering below does not reach it. This abort runs no
        // unwind at all -- it poisons unconditionally, because a session that
        // cannot account for a transaction's effects must not undo them on a
        // guess -- so there is no undo whose completion the record could assert,
        // and #53's guarantee that a mutating abort journals its reason is
        // unqualified here.
        //
        // What that `Abort` record must NOT be allowed to mean is "this
        // transaction is settled". It is terminal to `apply_recovery`, which
        // removes the operation from `RecoveryState.pending` on either terminal
        // record -- so without the record written first below, a reopen would
        // never see the `Prepare` intent, `replay_must_poison` would never run on
        // it, and the session would bind **clean** over a shadow tree still
        // holding everything `prepare` materialised and this path declined to
        // take back. No crash is needed for that: the abort completes, writes its
        // record, poisons in process, and the journal it leaves behind reads as
        // finished. It is the one path in this engine that could answer
        // "definitely not undone" with "proceed"
        // ([#65](https://github.com/invakid404/umbra/issues/65), waiver (viii)).
        //
        // So the poison is made durable. `JournalLifecycle::RecoveryRequired` is
        // an existing variant that had no writer until now; this is it. **Before**
        // the `Abort` record, and that order is the safety property, not a
        // preference: a crash between the two leaves a `Prepare` with no terminal
        // record, which the classifier already poisons on, so every interleaving
        // fails closed. The reverse order would leave exactly the hole this
        // closes.
        //
        // Unconditional on this path, deliberately -- not gated on a non-empty
        // `rollback` the way the flush below is. The gate there is a cost
        // argument about an undo that demonstrably happened; here the session is
        // saying it could not account for the transaction at all, and a writer
        // with more information than any future reader should not be pre-filtering
        // that verdict on the reader's behalf.
        if mutation && !kernel_refused {
            if let Err(e) = self.record(
                operation,
                JournalPayload::Lifecycle(JournalLifecycle::RecoveryRequired),
                true,
            ) {
                self.poisoned = true;
                return Err(e);
            }
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
            self.pending = None;
            self.poisoned = true;
            return Err(error(
                ErrorKind::InvalidState,
                "aborted effects require reconciliation",
            ));
        }
        // Before the `Abort` record, not after, and the ordering is the whole of
        // what this transaction means durably
        // ([#65](https://github.com/invakid404/umbra/issues/65)). `apply_recovery`
        // drops an operation from `RecoveryState.pending` on either a `Commit` or
        // an `Abort`, so writing the record first made a crash mid-unwind
        // indistinguishable from a clean finish: the log said "aborted", the
        // shadow tree was half-undone, and a reopen could only infer
        // reconcilability from the *absence* of a pending entry. Running the
        // unwind first inverts that. A crash here leaves `Prepare` with no
        // terminal record, `replay_must_poison` answers for the intent, and the
        // session poisons by construction rather than by a mechanism that had to
        // be remembered.
        //
        // The cost is deliberate and narrows #53: an abort whose unwind *fails*
        // -- or whose durability cannot be certified, see the flush below --
        // journals no reason at all, because the record is an assertion that the
        // undo's recorded removals are durable, and writing one after a failed
        // undo is a false invariant made durable, the worst form, since the next
        // reader has no way to doubt it. The failure is not silent; it poisons
        // this session and it poisons the next reopen. `overlay/README.md` states
        // the narrowed guarantee.
        //
        // Reverse creation order, so a nested object is gone before anything
        // that might contain it - which is what makes every `remove_directory`
        // below meet an empty directory without this loop tracking depth. See
        // `Pending.rollback` for the insertion-order invariant that guarantees
        // it. Nothing here restores whiteouts or the symlink index: both are
        // `commit`-only plans a reconciled abort never applied, and the marker a
        // removed object was hiding regains its rank by itself.
        //
        // `pending` is still live here, and deliberately: `context()` stamps each
        // request with the *transaction's* operation ID, and the undo of a
        // journaled `Prepare` is the one storage request whose attribution a
        // backend log most needs to be right. Clearing it first would have
        // charged these unlinks to the run-level context instead.
        //
        // `RollbackEntry::Symlink`'s three paths are removed in the variant's own
        // field order, which the arm below transcribes rather than restates: the
        // backing index first because its name is derived from the placeholder's
        // backend object ID, then the target blob, then the placeholder. No step
        // tolerates `ENOENT` — a missing piece is not a tidier world, it is
        // evidence that something outside this transaction is writing the control
        // tree — so all three go through this same closure and poison on failure
        // with the backend's own kind. Deliberately not `remove_symlink_index`:
        // that helper re-derives the index from a live `shadow_stat` of the
        // placeholder and silently skips when absent, and both behaviours are
        // wrong here — the recorded path is authoritative, and silence is the
        // opposite of what this path wants.
        let rolled_back = (|| {
            for entry in rollback.iter().rev() {
                match entry {
                    RollbackEntry::File(path) => {
                        let context = self.context()?;
                        self.storage.unlink(&context, path)?;
                    }
                    RollbackEntry::Directory(path) => {
                        let context = self.context()?;
                        self.storage.remove_directory(&context, path)?;
                    }
                    RollbackEntry::Symlink {
                        backing_index,
                        target_blob,
                        placeholder,
                    } => {
                        for path in [backing_index, target_blob, placeholder] {
                            let context = self.context()?;
                            self.storage.unlink(&context, path)?;
                        }
                    }
                }
            }
            Ok(())
        })();
        // --- #65 S1 BEGIN: the flush that lets the record below assert something
        // --- about the *store* and not merely about this process. Reverting this
        // --- change means deleting this block and restoring `rolled_back` in the
        // --- `if let Err(e) = unwound` below.
        //
        // `record(.., true)` fsyncs the journal; it says nothing about the shadow.
        // `LocalStorage::unlink` is a bare `remove_file` with no parent-directory
        // sync, and the NFS backends are the entire reason #107/#109's barrier
        // machinery exists. Without this, a crash between the last removal
        // returning and the `Abort` reaching the platter leaves a journal whose
        // inventory is empty -- so a clean, *unpoisoned* reopen -- over a shadow
        // tree that still holds the object. That is the phantom path the
        // `Rename { from == to }` arm poisons to avoid, reached by another route,
        // and "never silently accepts" does not admit it.
        //
        // Exactly what `commit` does for its own terminal record:
        // `storage.flush(EntireRun)`, validate the receipt, refuse
        // `Durability::None`, then record. The two records now make the same kind
        // of claim, so they earn it the same way.
        //
        // **Gated on a non-empty rollback**, and the gate is the whole of the cost
        // argument. An abort with nothing recorded removed nothing, so there is
        // nothing to make durable and a flush would be a pure tax on #53's hot
        // path -- the refused `chown`, the refused write-open onto a base file,
        // the ordinary `EPERM`/`ENOSPC` that has to reach a tracee cheaply. What
        // does pay is a refused creating `Open`, `Mkdir` or `Symlink`, which is
        // symmetric: `commit` already pays a flush for those same transactions
        // when they stand.
        //
        // Before `self.pending = None`, like the unwind above and for the same
        // reason: `context()` seeds from the live transaction, so the flush is
        // attributed to the operation whose undo it certifies.
        let unwound = rolled_back.and_then(|()| {
            if rollback.is_empty() {
                return Ok(());
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
            Ok(())
        });
        // --- #65 S1 END.
        self.pending = None;
        // A rollback that fails leaves exactly the divergence the gate exists to
        // prevent, so it poisons rather than reporting a reconciliation it did
        // not perform. This is the one `abort` path whose error is the storage
        // backend's own kind rather than `InvalidState`, and it needs no
        // per-kind branch: a non-empty `remove_directory` lands here like any
        // other failure, as `ENOTEMPTY`-shaped `Io` from `LocalStorage`,
        // `InvalidState` from tar, the kernel's errno from the NFS backends.
        // Matching on the kind would make the engine backend-aware for no gain.
        //
        // One kind here is *not* the backend's, and it arrives from the flush
        // above rather than the unwind: a receipt that fails validation is the
        // engine's own `ProtocolMismatch`, exactly as in `commit`. A failing
        // `Storage::flush` still reports the backend's kind. So the rule is "the
        // backend's kind, except where this engine rejected a receipt", which
        // `an_abort_that_removed_something_refuses_an_invalid_durability_receipt`
        // pins on both halves.
        //
        // Deliberately not a best-effort recursive remove: a shadow directory
        // that is non-empty here is non-empty because of something *this*
        // prepare did not create, so emptying it would delete committed state to
        // undo an uncommitted one. Nor a best-effort skip, which reintroduces
        // exactly the phantom path the gate exists to prevent.
        if let Err(e) = unwound {
            self.poisoned = true;
            return Err(e);
        }
        // Neither of the two failure paths above journals a
        // `JournalLifecycle::RecoveryRequired`, and that is a decision rather than
        // an omission. Both are already covered, exactly, by h1 -- a `Prepare`
        // with no terminal record -- and the proof is the drift guard at the end
        // of `prepare`: an unwind or a flush only runs at all when `rollback` is
        // non-empty, and a non-empty `rollback` implies `replay_must_poison` is
        // true for this transaction's intent. So the `Prepare` that stays in the
        // inventory is always one the classifier poisons on. Adding a second
        // durable assertion of the same fact would be redundant, and redundant
        // durable claims are how two records start disagreeing.
        //
        // The uncorroborated path is different precisely because it writes a
        // terminal record, which erases the `Prepare` h1 would have caught.
        //
        // The third path -- this record's own append failing (see
        // `an_abort_whose_record_cannot_be_written_poisons_and_the_next_reopen_poisons_too`)
        // -- gets no declaration for a stronger reason than redundancy: the
        // journal has just refused a write, so there is nothing to write it with.
        // It too leaves a `Prepare` with no terminal record, so h1 covers it.
        //
        // The record, now, and only now. It says the undo named by every
        // `RollbackEntry` this transaction recorded was performed *and* reached
        // the store's own durability boundary -- a claim the two steps above have
        // just earned, rather than one this ordering merely hopes for.
        //
        // The residual, stated because it is the kind of thing this change exists
        // to stop being silent about: the flush is skipped when nothing was
        // recorded, so what an `Abort` record asserts is "every removal this
        // transaction recorded is durable". For an empty rollback that is
        // vacuously true and no removal happened. Whiteouts, `retired_index` and
        // `destroy` are `commit`-only plans a reconciled abort never applied, so
        // they need no boundary either.
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
        // Renewal is allowed during a transaction, but never after poisoning.
        if self.poisoned {
            return Err(error(
                ErrorKind::InvalidState,
                "cannot renew a poisoned session",
            ));
        }
        let result = (|| {
            let lease = self.config()?.lease.clone();
            let renewed = self.storage.renew_writer(&lease)?;
            if renewed.run_id != lease.run_id
                || renewed.writer_id != lease.writer_id
                || renewed.epoch != lease.epoch
            {
                return Err(error(ErrorKind::LeaseLost, "invalid lease renewal"));
            }
            self.config.as_mut().expect("bound").lease = renewed.clone();
            Ok(renewed)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
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
        if durability.run_id != request.run_id
            || durability.writer_epoch != lease.epoch
            || durability.durability == Durability::None
        {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "invalid storage durability receipt",
            ));
        }
        let completed_through = self.record(
            session_operation,
            JournalPayload::Lifecycle(JournalLifecycle::RunCompleted {
                through: self.last_committed,
            }),
            true,
        )?;
        // A durable completion record is terminal. Attempt each cleanup stage
        // once, retaining every error; fail_run must not repeat closed stages.
        self.poisoned = true;
        self.completed_run = Some(request.run_id);
        let mut cleanup: Result<()> = Ok(());
        for (stage, result) in [
            (
                "journal close after durable completion",
                self.journal.close(),
            ),
            (
                "writer release after journal close",
                self.storage.release_writer(&lease),
            ),
            (
                "storage close after writer release",
                self.storage.close_run(),
            ),
        ] {
            if let Err(mut e) = result {
                e.context = format!("{stage}: {}", e.context);
                match &mut cleanup {
                    Ok(()) => cleanup = Err(e),
                    Err(primary) => primary.context.push_str(&format!("; {e}")),
                }
            }
        }
        self.config = None;
        self.base = None;
        cleanup?;
        Ok(FinishRunReceipt {
            run_id: request.run_id,
            durability,
            completed_through,
        })
    }
    fn fail_run(&mut self, request: &FailedRunRequest) -> Result<()> {
        // finish_run already attempted every terminal cleanup stage and returned
        // their errors to the caller. Do not close or release those resources twice.
        if self.completed_run == Some(request.run_id) {
            return Ok(());
        }
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
            // `object` is read before `prepare` copies up, so for a base-only
            // target it is the base identity and the kernel may chown a shadow
            // object with a different one — a copied-up file does get a new id,
            // while a copied-up logical symlink keeps the base's. `path` is what
            // stays resolvable either way, and `copy_up` records that the
            // materialisation happened at all: `Fchownat` is the only operation
            // that both materialises and reports something other than `CopyUp`.
            FsOp::Fchownat { uid, gid, .. } => JournalIntent::Chown {
                object,
                path,
                uid: *uid,
                gid: *gid,
                copy_up: matches!(existing, Some((_, false))),
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
