//! Small Unix filesystem backend for development and in-process tests.
//!
//! Construct with a private, trusted directory, then open a run and acquire its writer.
//! Runs occupy `<directory>/<run-id>/{root,control}`. The caller must prevent external
//! changes to that layout while it is open: std path-based I/O is not a race-proof
//! sandbox. Existing symlink components are rejected. Kernel shadow qualification,
//! remote persistence, crash-recoverable retries, and writer takeover are unsupported.
//! Successful mutation retries are cached only for the lifetime of the open session.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg(unix)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use umbra_core::Errno;
use umbra_storage::{
    AcquireWriterRequest, BlobStat, BytePath, CreateKind, DirectoryEntry, DirectoryPage,
    Durability, DurabilityReceipt, ErrorKind, Fencing, FlushRequest, FlushScope, IdempotencyKey,
    LeaseEpoch, ListCursor, MetadataUpdate, ObjectId, ObjectKind, ObjectResult, OpenRunIntent,
    OpenRunRequest, RequestContext, Result, RunBinding, RuntimeDirectoryBinding, Storage,
    StorageAnchor, StorageCapabilities, StorageHandle, StorageOperation, StoragePath,
    StorageRequest, StorageResponse, TakeoverPolicy, UmbraError, WriterLease,
    MAX_DIRECTORY_ENTRIES, MAX_IO_BYTES,
};
use uuid::Uuid;

/// Local storage rooted at a caller-owned, trusted filesystem directory.
#[derive(Debug)]
pub struct LocalStorage {
    directory: PathBuf,
    run: Option<OpenRun>,
    /// True only once a live probe measured that this run's backing filesystem
    /// hands a new object its parent directory's gid -- the exact property
    /// `STORAGE_PARENT_IDENTITY_V1` names. Set in `open_run` (never for a
    /// read-only run), read by `capabilities()`, and reset in `close_run`
    /// because the qualification was against *that* run's store. False on a
    /// fresh `LocalStorage`, so a `capabilities()` read before any run opens --
    /// which the overlay does -- answers "not advertised".
    parent_identity_qualified: bool,
    /// Count of live layout threads for *this* provider (in flight or orphaned past
    /// their timeout), the budget [`bounded_layout`] reserves against. Held per
    /// instance rather than in a process-global `static` so that independent
    /// providers sharing a process -- the overlay's in-process stores, or the many
    /// concurrent instances a test binary spins up -- never refuse one another's
    /// opens: a wedged mount behind one provider must not exhaust another's budget.
    /// `open_run` is `&mut self`, so a healthy provider keeps this at 0 or 1; it
    /// only climbs toward `LAYOUT_BUDGET` as a wedged mount strands threads (#100).
    layout_live: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct OpenRun {
    request: OpenRunRequest,
    directory: PathBuf,
    lease: Option<WriterLease>,
    retries: HashMap<IdempotencyKey, (StorageRequest, StorageResponse)>,
    pages: HashMap<Vec<u8>, (StoragePath, Vec<DirectoryEntry>)>,
}

fn error(kind: ErrorKind, op: &str, message: &str) -> UmbraError {
    UmbraError::new(kind, op, message)
}

fn io_error(op: &str, err: std::io::Error) -> UmbraError {
    let kind = match err.kind() {
        std::io::ErrorKind::NotFound => ErrorKind::NotFound,
        std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => ErrorKind::Denied,
        std::io::ErrorKind::InvalidInput => ErrorKind::InvalidInput,
        _ => ErrorKind::Io,
    };
    let mut result = error(kind, op, &err.to_string());
    result.errno = err.raw_os_error().map(Errno);
    result
}

fn unsupported(op: &str) -> UmbraError {
    error(
        ErrorKind::UnsupportedCapability,
        op,
        "not supported by local test storage",
    )
}

fn directory_binding(path: &Path) -> Result<RuntimeDirectoryBinding> {
    Ok(RuntimeDirectoryBinding {
        handle: StorageHandle(Uuid::new_v4().as_bytes().to_vec()),
        physical_path: Some(BytePath::new(path.as_os_str().as_bytes().to_vec())?),
    })
}

// This representation deliberately contains no deployment paths.
fn manifest(request: &OpenRunRequest) -> Vec<u8> {
    let mut bytes = b"umbra-local\0".to_vec();
    bytes.extend_from_slice(&request.policy.format_version.to_le_bytes());
    bytes.extend_from_slice(request.run_id.0.as_bytes());
    let identity = request.immutable_base.identity.as_bytes();
    bytes.extend_from_slice(&(identity.len() as u64).to_le_bytes());
    bytes.extend_from_slice(identity);
    bytes.extend_from_slice(&request.immutable_base.fingerprint);
    bytes
}

/// The blocking layout phase of `open_run`, factored into a free function over
/// owned inputs so it can run on a supervised thread (see [`bounded_layout`]) and
/// so a wedged mount stalls that thread rather than `open_run` itself. It performs
/// every filesystem operation `open_run` does before the qualification probe: the
/// `CreateNew` mkdir/write chain, then the reopen validation both intents share.
///
/// Nothing here touches `self`; the storage passes an owned run `directory` and a
/// cloned `request`. The body is the pre-probe layout of `open_run` moved verbatim,
/// so the invariants it already holds carry over unchanged. In particular the run
/// directory's exclusive `create_dir` stays the first `CreateNew` mutation: an
/// orphaned late finish either wins that claim and continues alone, or loses it and
/// stops at `AlreadyExists`, so two writers can never interleave inside one run dir
/// (#100). The manifest is written last, after the epoch, so a complete manifest
/// implies a complete layout and any partial residue an orphan leaves is refused by
/// the validation below -- the same residue class a crash mid-`CreateNew` leaves
/// today, for which there is deliberately no rollback.
fn local_layout(directory: PathBuf, request: OpenRunRequest) -> Result<()> {
    if request.intent == OpenRunIntent::CreateNew {
        fs::create_dir(&directory).map_err(|e| io_error("open_run", e))?;
        // A failed initialization remains unopenable, rather than publishing a binding.
        fs::create_dir(directory.join("root")).map_err(|e| io_error("open_run", e))?;
        fs::create_dir(directory.join("control")).map_err(|e| io_error("open_run", e))?;
        fs::write(directory.join("epoch"), 0u64.to_le_bytes())
            .map_err(|e| io_error("open_run", e))?;
        fs::write(directory.join("manifest"), manifest(&request))
            .map_err(|e| io_error("open_run", e))?;
    }
    for path in [
        &directory,
        &directory.join("root"),
        &directory.join("control"),
    ] {
        let meta = fs::symlink_metadata(path).map_err(|e| io_error("open_run", e))?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(error(ErrorKind::InvalidPath, "open_run", "invalid layout"));
        }
    }
    let manifest_path = directory.join("manifest");
    if !fs::symlink_metadata(&manifest_path)
        .map_err(|e| io_error("open_run", e))?
        .is_file()
    {
        return Err(error(
            ErrorKind::InvalidPath,
            "open_run",
            "invalid manifest",
        ));
    }
    if fs::read(manifest_path).map_err(|e| io_error("open_run", e))? != manifest(&request) {
        return Err(error(
            ErrorKind::ProtocolMismatch,
            "open_run",
            "run/base/format mismatch",
        ));
    }
    Ok(())
}

fn metadata(path: &Path) -> Result<BlobStat> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_error("stat", e))?;
    let kind = if meta.is_file() {
        ObjectKind::File
    } else if meta.is_dir() {
        ObjectKind::Directory
    } else {
        return Err(unsupported("stat: symlink or special file"));
    };
    // Stable across reopening/renaming and shared by native hard links. This local
    // identity is not portable to another filesystem and may be reused after deletion.
    let id = ((meta.dev() as u128) << 64) | meta.ino() as u128;
    Ok(BlobStat {
        object_id: ObjectId(Uuid::from_u128(id)),
        kind,
        len: meta.len(),
        link_count: meta.nlink(),
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
        modified_nanos: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
    })
}

impl LocalStorage {
    /// Create and canonicalize the trusted local storage directory.
    pub fn new(directory: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(directory.as_ref()).map_err(|e| io_error("new", e))?;
        let directory = fs::canonicalize(directory).map_err(|e| io_error("new", e))?;
        Ok(Self {
            directory,
            run: None,
            parent_identity_qualified: false,
            layout_live: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn run(&self) -> Result<&OpenRun> {
        self.run
            .as_ref()
            .ok_or_else(|| error(ErrorKind::InvalidState, "run", "no open run"))
    }

    fn check_context(&self, context: &RequestContext, mutation: bool) -> Result<()> {
        let run = self.run()?;
        if run.request.run_id != context.run_id {
            return Err(error(ErrorKind::InvalidState, "execute", "wrong run"));
        }
        if mutation {
            let lease = run
                .lease
                .as_ref()
                .ok_or_else(|| error(ErrorKind::LeaseLost, "execute", "no writer"))?;
            if context.writer_epoch != Some(lease.epoch) {
                return Err(error(
                    ErrorKind::LeaseLost,
                    "execute",
                    "stale or missing epoch",
                ));
            }
            self.check_lease(lease)?;
            if context.idempotency_key.0.is_empty() {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "execute",
                    "empty idempotency key",
                ));
            }
        }
        Ok(())
    }

    fn check_lease(&self, lease: &WriterLease) -> Result<()> {
        let run = self.run()?;
        if run.lease.as_ref() != Some(lease) {
            return Err(error(ErrorKind::LeaseLost, "writer", "stale lease"));
        }
        let token =
            fs::read(run.directory.join("writer.lock")).map_err(|e| io_error("writer", e))?;
        if token != lease.renewal_token {
            return Err(error(ErrorKind::LeaseLost, "writer", "writer lock changed"));
        }
        Ok(())
    }

    fn path(&self, path: &StoragePath, allow_missing: bool) -> Result<PathBuf> {
        let run = self.run()?;
        let anchor = match path.anchor() {
            StorageAnchor::Root => "root",
            StorageAnchor::Control => "control",
        };
        let mut physical = run.directory.clone();
        // Revalidate the run and anchor, as well as every requested component.
        let components = std::iter::once(OsStr::new(anchor)).chain(
            path.as_bytes()
                .split(|b| *b == b'/')
                .filter(|c| !c.is_empty())
                .map(OsStr::from_bytes),
        );
        let run_meta = fs::symlink_metadata(&physical).map_err(|e| io_error("path", e))?;
        if !run_meta.is_dir() || run_meta.file_type().is_symlink() {
            return Err(error(
                ErrorKind::InvalidPath,
                "path",
                "invalid run directory",
            ));
        }
        for component in components {
            physical.push(component);
            match fs::symlink_metadata(&physical) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        "path",
                        "symlink components are unsupported",
                    ));
                }
                Ok(_) => (),
                Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(io_error("path", e)),
            }
        }
        Ok(physical)
    }

    fn object(&self, path: &Path) -> Result<ObjectResult> {
        Ok(ObjectResult {
            stat: metadata(path)?,
            rewrite_target: None,
        })
    }

    fn execute_operation(&mut self, operation: &StorageOperation) -> Result<StorageResponse> {
        match operation {
            StorageOperation::ReadAt { path, offset, len } => {
                check_io(*offset, *len as usize)?;
                let path = self.path(path, false)?;
                if metadata(&path)?.kind != ObjectKind::File {
                    return Err(unsupported("read_at: not a file"));
                }
                let file = File::open(path).map_err(|e| io_error("read_at", e))?;
                let mut bytes = vec![0; *len as usize];
                let count = file
                    .read_at(&mut bytes, *offset)
                    .map_err(|e| io_error("read_at", e))?;
                bytes.truncate(count);
                Ok(StorageResponse::ReadAt(bytes))
            }
            StorageOperation::WriteAt {
                path,
                offset,
                bytes,
            } => {
                check_io(*offset, bytes.len())?;
                let path = self.path(path, false)?;
                if metadata(&path)?.kind != ObjectKind::File {
                    return Err(unsupported("write_at: not a file"));
                }
                let file = OpenOptions::new()
                    .write(true)
                    .open(path)
                    .map_err(|e| io_error("write_at", e))?;
                let count = file
                    .write_at(bytes, *offset)
                    .map_err(|e| io_error("write_at", e))?;
                Ok(StorageResponse::WriteAt(count as u32))
            }
            StorageOperation::Create { path, options } => {
                if matches!(options.kind, CreateKind::LogicalSymlink { .. }) {
                    return Err(unsupported("create symlink"));
                }
                if options.mode & !0o7777 != 0 {
                    return Err(error(ErrorKind::InvalidInput, "create", "invalid mode"));
                }
                let physical = self.path(path, true)?;
                match options.kind {
                    CreateKind::File => {
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(options.mode)
                            .open(&physical)
                            .map_err(|e| io_error("create", e))?;
                    }
                    CreateKind::Directory => {
                        fs::DirBuilder::new()
                            .mode(options.mode)
                            .create(&physical)
                            .map_err(|e| io_error("create", e))?;
                    }
                    CreateKind::LogicalSymlink { .. } => return Err(unsupported("create symlink")),
                }
                Ok(StorageResponse::Created(self.object(&physical)?))
            }
            // Fused the way `umbra-storage-nfs` and `umbra-storage-tar` already
            // fuse them: one path resolution, one kind bit. `self.path(path,
            // false)` is the guard that matters for a removal -- it refuses
            // symlink components and revalidates the run directory and anchor --
            // and unlike tar no explicit kind check is needed, because the kernel
            // supplies it (`remove_dir` on a file is `ENOTDIR`, `remove_file` on
            // a directory is `EISDIR`/`EPERM`). It arrives through `io_error` as
            // `ErrorKind::Io` where tar answers `InvalidPath`; see
            // `Storage::remove_directory` for why no caller may depend on that.
            //
            // #64 closed exactly one of this match's fallthrough operations --
            // the one the overlay's reconciled abort needs -- and is not a
            // completion push: `Rename`, `Link`, `ReadLink`, `AtomicSwap`,
            // `CreateParents` and `CopyUp` all still hit `unsupported`.
            StorageOperation::Unlink { path } | StorageOperation::RemoveDirectory { path } => {
                let directory = matches!(operation, StorageOperation::RemoveDirectory { .. });
                let physical = self.path(path, false)?;
                if directory {
                    fs::remove_dir(physical).map_err(|e| io_error("remove_directory", e))?;
                    Ok(StorageResponse::DirectoryRemoved)
                } else {
                    fs::remove_file(physical).map_err(|e| io_error("unlink", e))?;
                    Ok(StorageResponse::Unlinked)
                }
            }
            StorageOperation::Stat { path } => {
                Ok(StorageResponse::Stat(metadata(&self.path(path, false)?)?))
            }
            StorageOperation::Lookup { path } => Ok(StorageResponse::Lookup(
                self.object(&self.path(path, false)?)?,
            )),
            StorageOperation::List {
                path,
                cursor,
                limit,
            } => {
                if *limit == 0 || *limit > MAX_DIRECTORY_ENTRIES {
                    return Err(error(ErrorKind::InvalidInput, "list", "invalid page limit"));
                }
                let physical = self.path(path, false)?;
                let mut entries = if let Some(cursor) = cursor {
                    let (owner, entries) = self.run()?.pages.get(&cursor.0).ok_or_else(|| {
                        error(
                            ErrorKind::StaleHandle,
                            "list",
                            "unknown or invalidated cursor",
                        )
                    })?;
                    if owner != path {
                        return Err(error(
                            ErrorKind::StaleHandle,
                            "list",
                            "cursor belongs to another directory",
                        ));
                    }
                    entries.clone()
                } else {
                    let mut entries = Vec::new();
                    for entry in fs::read_dir(physical).map_err(|e| io_error("list", e))? {
                        let entry = entry.map_err(|e| io_error("list", e))?;
                        entries.push(DirectoryEntry {
                            name: BytePath::new(entry.file_name().as_bytes().to_vec())?,
                            stat: metadata(&entry.path())?,
                        });
                    }
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    entries
                };
                let next = if entries.len() > *limit as usize {
                    let remainder = entries.split_off(*limit as usize);
                    let token = Uuid::new_v4().as_bytes().to_vec();
                    self.run
                        .as_mut()
                        .unwrap()
                        .pages
                        .insert(token.clone(), (path.clone(), remainder));
                    Some(ListCursor(token))
                } else {
                    None
                };
                Ok(StorageResponse::List(DirectoryPage { entries, next }))
            }
            // Ownership and mode on an object already in the shadow, which is how
            // the overlay carries a base object's uid/gid onto the shadow it
            // materialised: `CreateOptions` carries only a mode, so the create
            // and the ownership are two operations and this is the second.
            //
            // `lchown` rather than `libc::fchownat`: it is `std`, so this crate
            // keeps its `forbid(unsafe_code)` -- the only foreign call it makes,
            // supplementary-group discovery, goes through the safe
            // `rustix::process` wrappers (see `process_groups`), so no `unsafe`
            // appears anywhere in this crate -- and its `Option<u32>` parameters
            // *are* `MetadataUpdate`'s -- `None` means "leave this ID alone" in both.
            // It does not follow the final symlink, matching `metadata`'s
            // `symlink_metadata` above.
            //
            // Order: mode first, ownership second. A successful `chown(2)` by an
            // unprivileged process clears setuid/setgid, so setting the mode
            // afterwards could restore bits the kernel deliberately dropped.
            StorageOperation::SetMetadata { path, update } => {
                check_update(update)?;
                let physical = self.path(path, false)?;
                if let Some(mode) = update.mode {
                    fs::set_permissions(&physical, fs::Permissions::from_mode(mode))
                        .map_err(|e| io_error("set_metadata", e))?;
                }
                if update.uid.is_some() || update.gid.is_some() {
                    // `PermissionDenied` is already `ErrorKind::Denied` in
                    // `io_error`, which is what the overlay matches on to fall
                    // back to its own uid instead of failing the operation. An
                    // unprivileged cross-uid chown arrives here as `EPERM`.
                    std::os::unix::fs::lchown(&physical, update.uid, update.gid)
                        .map_err(|e| io_error("set_metadata", e))?;
                }
                Ok(StorageResponse::MetadataSet(metadata(&physical)?))
            }
            _ => Err(unsupported("execute")),
        }
    }
}

/// Refuse a metadata update this backend cannot apply *before* applying any of
/// it, so a mixed update can never half-land.
///
/// `umbra-storage-nfs-userspace` hardened its own SETATTR the same way and for
/// the same reason: a caller that gets an error back has to be able to read it
/// as "nothing happened". Timestamps are refused whole rather than dropped
/// silently -- this backend has no caller for them, and a discarded write is the
/// one outcome worse than an honest refusal.
fn check_update(update: &MetadataUpdate) -> Result<()> {
    if update.accessed_nanos.is_some() || update.modified_nanos.is_some() {
        return Err(unsupported("set_metadata: timestamps"));
    }
    if update.mode.is_none() && update.uid.is_none() && update.gid.is_none() {
        return Err(error(
            ErrorKind::InvalidInput,
            "set_metadata",
            "update names nothing",
        ));
    }
    if update.mode.is_some_and(|mode| mode & !0o7777 != 0) {
        return Err(error(
            ErrorKind::InvalidInput,
            "set_metadata",
            "invalid mode",
        ));
    }
    Ok(())
}

fn check_io(offset: u64, len: usize) -> Result<()> {
    if len > MAX_IO_BYTES || offset.checked_add(len as u64).is_none() {
        return Err(error(
            ErrorKind::InvalidInput,
            "io",
            "invalid length or offset",
        ));
    }
    Ok(())
}

impl LocalStorage {
    /// The body of [`open_run`](Storage::open_run) with the layout phase supplied as
    /// a parameter, so the real caller passes `LAYOUT_TIMEOUT` and the true
    /// `local_layout`, while tests pass a shorter timeout or a closure that parks --
    /// no `cfg(test)` hook in the product path (mirrors the `bounded`/`bounded_with`
    /// seam). The precondition checks, the unchanged qualification probe, the #92
    /// flag ordering and the run install all stay here on the main thread; only the
    /// blocking `layout` runs on the supervised thread `bounded_layout` spawns.
    fn open_run_with(
        &mut self,
        request: &OpenRunRequest,
        timeout: Duration,
        layout: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<RunBinding> {
        if self.run.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "open_run",
                "run already open",
            ));
        }
        if request.policy.require_strict_remote_persistence || request.policy.require_kernel_shadow
        {
            return Err(unsupported("open_run policy"));
        }
        if request.policy.format_version != 1 {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "open_run",
                "expected format version 1",
            ));
        }
        if request.policy.read_only && request.intent == OpenRunIntent::CreateNew {
            return Err(error(ErrorKind::Denied, "open_run", "read-only creation"));
        }
        let directory = self.directory.join(request.run_id.0.to_string());
        // Run the blocking layout on a supervised thread. A timeout, budget
        // exhaustion, spawn failure or panic all surface as `StorageUnavailable`
        // *before* the qualification line below, so a failed layout never advertises
        // a capability and never spawns a probe -- the #92 ordering is unchanged.
        bounded_layout(&self.layout_live, timeout, layout)?;
        // Compute the qualification answer up front, but do not record it until
        // every fallible step below has succeeded: with the assignment last, the
        // flag can no longer outlive a failed `open_run`. `capabilities()` still
        // reads the flag when building the `RunBinding` the caller is handed, so
        // the answer must be recorded before that binding is built -- hence the
        // assignment sits just above it, not at the end of `open_run`.
        //
        // A read-only run is never probed and never advertises: the probe
        // performs a `chgrp`, a mutation this run would refuse anyway, and
        // qualifying a capability by performing the very mutation the policy
        // forbids would be the wrong way round. Every failure inside the probe
        // answers `false` rather than propagating -- a store that cannot probe
        // must not fail `open_run` over a capability nothing has asked for yet.
        // The probe does blocking filesystem I/O a wedged mount can stall on, so
        // bound it: a timeout answers `false` like any other probe failure. The
        // read-only short-circuit stays in front, so a read-only run spawns nothing.
        let qualified = !request.policy.read_only
            && bounded(PROBE_TIMEOUT, {
                // The probe scratch lives in an isolated `<root>/.umbra-probes/`
                // container, never under the run dir, so its late cleanup cannot
                // race a concurrent flush's `sync_tree` of the run (#101). The run
                // dir tags along only for the dev guard. The container is created
                // inside the closure (below), never here on the main thread (#92).
                let probes = self.directory.join(".umbra-probes");
                let run_dir = directory.clone();
                move || parent_identity_probe(&probes, &run_dir)
            });
        let root = directory_binding(&directory.join("root"))?;
        let control = directory_binding(&directory.join("control"))?;
        // Every fallible step above has succeeded; only now record the answer.
        self.parent_identity_qualified = qualified;
        let binding = RunBinding {
            run_id: request.run_id,
            root,
            control,
            capabilities: self.capabilities(),
        };
        self.run = Some(OpenRun {
            request: request.clone(),
            directory,
            lease: None,
            retries: HashMap::new(),
            pages: HashMap::new(),
        });
        Ok(binding)
    }
}

impl Storage for LocalStorage {
    fn capabilities(&self) -> StorageCapabilities {
        // Explicitly a development store: it advertises the local mode and
        // the narrow rewrite surface, and nothing about remote durability.
        // `STORAGE_OWNERSHIP_FIDELITY_V1` is qualified by the `SetMetadata`
        // arm above and by `metadata`, which has read `uid`/`gid` back since
        // this backend was written. What the flag promises is that the
        // uid/gid a caller names are applied and a refusal is reported, not
        // that the kernel permits every chown -- an unprivileged cross-uid
        // chown is `Denied` here and the caller is told so.
        //
        // `STORAGE_PARENT_IDENTITY_V1` is advertised only after a live per-run
        // probe *measured* that this backing filesystem hands a new object its
        // parent directory's gid -- never from a `cfg(target_os)`, which the
        // capability's own contract forbids ("from configuration alone") and
        // which would be wrong regardless: `LocalStorage::new` places no
        // restriction on the backing filesystem, so the store may sit on a
        // network mount whose server assigns child identity instead. `open_run`
        // runs the probe (skipping a read-only run) and this reads its answer,
        // exactly as the nfs backend reads its ownership qualification. A fresh
        // store, or one whose filesystem answers otherwise, is silent -- Linux
        // included, where the kernel gives a child the process gid absent a
        // setgid parent, so the probe measures `false` by that rule alone.
        let mut features: std::collections::BTreeSet<String> = [
            umbra_core::capabilities::STORAGE_LOCAL_DEVELOPMENT_V1.to_owned(),
            umbra_core::capabilities::STORAGE_OPEN_REWRITE_V1.to_owned(),
            umbra_core::capabilities::STORAGE_OWNERSHIP_FIDELITY_V1.to_owned(),
        ]
        .into_iter()
        .collect();
        if self.parent_identity_qualified {
            features.insert(umbra_core::capabilities::STORAGE_PARENT_IDENTITY_V1.to_owned());
        }
        StorageCapabilities {
            features,
            durability: Durability::Local,
            strict_remote_persistence: false,
            fencing: Fencing::ConfirmedTermination,
            kernel_shadow: false,
            complete_emulation: false,
            hard_links: false,
            logical_symlinks: false,
            xattrs: false,
            atomic_replace: false,
            atomic_swap: false,
            max_io_bytes: MAX_IO_BYTES as u32,
            max_directory_entries: MAX_DIRECTORY_ENTRIES,
        }
    }

    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        // The blocking layout runs on a supervised thread bounded by
        // `LAYOUT_TIMEOUT`, so wrap the real `local_layout` in the closure the seam
        // takes. `open_run_with` keeps the cheap precondition checks and the
        // unchanged qualification probe on this thread; only the layout I/O a wedged
        // mount can stall on moves off it.
        let layout = {
            let directory = self.directory.join(request.run_id.0.to_string());
            let request = request.clone();
            move || local_layout(directory, request)
        };
        self.open_run_with(request, LAYOUT_TIMEOUT, layout)
    }

    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        let run = self.run()?;
        if request.run_id != run.request.run_id || run.lease.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "acquire_writer",
                "wrong run or active writer",
            ));
        }
        if run.request.policy.read_only {
            return Err(error(ErrorKind::Denied, "acquire_writer", "read-only run"));
        }
        if request.takeover != TakeoverPolicy::Refuse {
            return Err(unsupported("writer takeover"));
        }
        let token = Uuid::new_v4().as_bytes().to_vec();
        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(run.directory.join("writer.lock"))
            .map_err(|e| io_error("acquire_writer", e))?;
        // On any subsequent error leave the lock behind: automatic takeover is unsafe.
        lock.write_all(&token)
            .map_err(|e| io_error("acquire_writer", e))?;
        lock.sync_all().map_err(|e| io_error("acquire_writer", e))?;
        let epoch_path = run.directory.join("epoch");
        if !fs::symlink_metadata(&epoch_path)
            .map_err(|e| io_error("acquire_writer", e))?
            .is_file()
        {
            return Err(error(
                ErrorKind::InvalidPath,
                "acquire_writer",
                "invalid epoch file",
            ));
        }
        let bytes: [u8; 8] = fs::read(&epoch_path)
            .map_err(|e| io_error("acquire_writer", e))?
            .try_into()
            .map_err(|_| error(ErrorKind::InvalidState, "acquire_writer", "invalid epoch"))?;
        let epoch = u64::from_le_bytes(bytes)
            .checked_add(1)
            .ok_or_else(|| error(ErrorKind::InvalidState, "acquire_writer", "epoch exhausted"))?;
        let mut epoch_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(epoch_path)
            .map_err(|e| io_error("acquire_writer", e))?;
        epoch_file
            .write_all(&epoch.to_le_bytes())
            .map_err(|e| io_error("acquire_writer", e))?;
        epoch_file
            .sync_all()
            .map_err(|e| io_error("acquire_writer", e))?;
        let lease = WriterLease {
            run_id: request.run_id,
            writer_id: request.writer_id.clone(),
            epoch: LeaseEpoch(epoch),
            renewal_token: token,
            renew_after_millis: 60_000,
        };
        self.run.as_mut().unwrap().lease = Some(lease.clone());
        Ok(lease)
    }

    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        self.check_lease(lease)?;
        Ok(lease.clone())
    }

    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        self.check_lease(lease)?;
        fs::remove_file(self.run()?.directory.join("writer.lock"))
            .map_err(|e| io_error("release_writer", e))?;
        self.run.as_mut().unwrap().lease = None;
        Ok(())
    }

    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        let mutation = request.operation.is_mutation();
        self.check_context(&request.context, mutation)?;
        if mutation {
            if let Some((previous, response)) =
                self.run()?.retries.get(&request.context.idempotency_key)
            {
                if previous != request {
                    return Err(error(
                        ErrorKind::InvalidInput,
                        "execute",
                        "conflicting idempotency key",
                    ));
                }
                return Ok(response.clone());
            }
            // Even a partially failed mutation invalidates snapshots.
            self.run.as_mut().unwrap().pages.clear();
        }
        let response = self.execute_operation(&request.operation)?;
        if mutation {
            self.run.as_mut().unwrap().retries.insert(
                request.context.idempotency_key.clone(),
                (request.clone(), response.clone()),
            );
        }
        Ok(response)
    }

    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        self.check_context(&request.context, true)?;
        if request.scope != FlushScope::EntireRun {
            return Err(unsupported("object-scoped flush"));
        }
        sync_tree(&self.run()?.directory)?;
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| io_error("flush", e))?;
        Ok(DurabilityReceipt {
            run_id: request.context.run_id,
            writer_epoch: self.run()?.lease.as_ref().unwrap().epoch,
            scope: request.scope.clone(),
            durability: Durability::Local,
            evidence: b"std::fs sync_all of run files and directories".to_vec(),
        })
    }

    fn close_run(&mut self) -> Result<()> {
        if self.run()?.lease.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "close_run",
                "release writer first",
            ));
        }
        self.run = None;
        // The qualification was against this run's store; the next run has to
        // earn it again. Load-bearing: without it a closed run keeps answering
        // "advertised" from `capabilities()` with no run open.
        self.parent_identity_qualified = false;
        Ok(())
    }
}

fn sync_tree(path: &Path) -> Result<()> {
    let stat = metadata(path)?; // Reject physical symlinks and special files before open.
    if stat.kind == ObjectKind::Directory {
        for entry in fs::read_dir(path).map_err(|e| io_error("flush", e))? {
            sync_tree(&entry.map_err(|e| io_error("flush", e))?.path())?;
        }
    }
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| io_error("flush", e))
}

/// Removes its directory (and anything under it), so a probe leaves nothing behind
/// under the user's storage root. `remove_dir_all`, not `remove_dir`: the probe
/// grows children before its verdict is known. Nothing survives SIGKILL --
/// confinement under `<root>/<run-id>/` (removed with the run) and a `uuid` name
/// are the mitigation there.
///
/// Two mechanisms, not one. The success path calls `remove_now` and *fails the
/// probe closed if removal errors*, because a run must never qualify while a
/// `.umbra-probe-*` directory survives -- the guardrail is unconditional and
/// `Drop` alone cannot report a failure, only swallow it. `Drop` is retained as
/// the backstop for `?`, early `return`, and unwinding, which `remove_now` cannot
/// cover. `remove_now` disarms the guard only *after* removal has actually
/// succeeded, so the two never double-remove in a way that could turn a success
/// into a spurious failure, and `Drop` still fires whenever `remove_now` did not
/// run or did not succeed.
struct ProbeDir<'a> {
    path: &'a Path,
    armed: std::cell::Cell<bool>,
}

impl<'a> ProbeDir<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            armed: std::cell::Cell::new(true),
        }
    }

    /// Remove the probe tree now, disarming the `Drop` backstop only if removal
    /// succeeds. On failure the guard stays armed, so `Drop` still retries.
    fn remove_now(&self) -> std::io::Result<()> {
        fs::remove_dir_all(self.path)?;
        self.armed.set(false);
        Ok(())
    }
}

impl Drop for ProbeDir<'_> {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(self.path);
        }
    }
}

/// The supplementary group set of the calling process, or `None` if it cannot be
/// read.
///
/// `getgroups(2)` -- the process credential set the kernel authorises the probe's
/// `chgrp` against -- deliberately, not `getgrouplist(3)`: the latter returns the
/// directory service's idea of a user's groups, a superset that can include
/// groups absent from the process credential, so a `chgrp` to one fails `EPERM`
/// and the probe reads a spurious `false`. `rustix::process::getgroups` wraps the
/// same syscall as a safe fn -- doing the count-then-fill two-call form internally
/// -- so this crate keeps `#![forbid(unsafe_code)]`. Its `Result` is folded into
/// the fail-closed path like every other probe error.
///
/// rustix's two-call form is only half race-safe, so we sanitise the result. If
/// the set *grows* between the count and fill calls the fill errs and the whole
/// read fails closed. If it *shrinks*, rustix (1.1.4 and 1.1.5,
/// `src/process/id.rs:252-259`) discards the fill call's returned count and does
/// not truncate the buffer it sized to the stale count, so the tail keeps
/// `Gid::ROOT` (raw 0) padding that was never part of the credential set.
/// [`strip_getgroups_padding`] pops that trailing zero run; see its docs for why
/// stripping trailing zeros is the exact, fail-closed match for this bug.
fn process_groups() -> Option<Vec<u32>> {
    Some(strip_getgroups_padding(
        rustix::process::getgroups()
            .ok()?
            .into_iter()
            .map(|g| g.as_raw())
            .collect(),
    ))
}

/// Strip the trailing `Gid::ROOT` (raw 0) padding rustix's `getgroups` can leave
/// on its result when the supplementary set shrinks mid-read (see
/// [`process_groups`]). Pure over its input so it is testable without racing a
/// live `setgroups`.
///
/// Only *trailing* zeros are dropped: the padding is exclusively at the tail, and
/// an interior 0 is a legitimate gid (e.g. `wheel`/`root`'s set on BSD/macOS) that
/// must be preserved as a possible candidate. A legitimate *trailing* 0 is
/// indistinguishable from padding and is dropped too, but that only ever removes a
/// candidate, which moves the probe toward the indecisive/`false` verdict -- the
/// fail-closed direction.
fn strip_getgroups_padding(mut groups: Vec<u32>) -> Vec<u32> {
    while groups.last() == Some(&0) {
        groups.pop();
    }
    groups
}

/// The process's effective gid.
fn effective_gid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// A group the process belongs to that differs from its effective gid, or `None`
/// when it belongs to only one group -- the indecisive case, which the contract
/// says to answer silently (no error, no warning). Pure over its inputs so the
/// single-group path is testable without assuming the host's group count.
fn pick_candidate(groups: &[u32], egid: u32) -> Option<u32> {
    groups.iter().copied().find(|&g| g != egid)
}

/// A candidate group for the live probe, or `None` on the indecisive path.
fn candidate_group() -> Option<u32> {
    pick_candidate(&process_groups()?, effective_gid())
}

/// The measured verdict, pure over the two gids the probe observed: the child
/// inherited the probe directory's group, so this filesystem hands a new object
/// its parent's group identity. Factored out so both outcomes are testable with
/// no divergent-identity filesystem actually mounted -- injecting a diverging
/// pair discharges the macOS+NFS acceptance case deterministically.
fn parent_identity_matches(probe_gid: u32, child_gid: u32) -> bool {
    child_gid == probe_gid
}

/// Perform the live probe under `run_dir`, returning the gid the filesystem gave
/// the probe directory (after it was `chgrp`'d to `candidate`) and the gid it
/// gave a fresh child inside it. `None` on *any* failure, on the fsgid guard, on
/// the D1 setgid refusal, or if the explicit success-path cleanup fails -- the
/// caller then does not advertise, the fail-closed direction.
///
/// The probe directory is `uuid`-named and lives directly under the `parent` it
/// is handed -- the isolated `<root>/.umbra-probes/` container (#101), a sibling
/// of the run directory rather than a child of it, never inside the agent-visible
/// `root/` tree. The unique name is load-bearing: `open_run` holds no lock, so two
/// concurrent opens must not collide on a fixed probe path.
///
/// fsgid guard: `candidate` is only guaranteed `!= getegid()`, but Linux assigns a
/// new object's group from the process *fsgid*, which `setfsgid(2)` lets diverge
/// from the egid. A control child reveals that default creation gid empirically;
/// if it already equals `candidate`, the later "child gid == probe gid" comparison
/// could not tell real parent-gid inheritance apart from the process minting
/// `candidate` regardless, so the probe fails closed. On BSD the control child
/// inherits the probe dir's own gid, so the guard is merely conservative there; on
/// Linux with `fsgid == egid` it never trips because `candidate != egid` by
/// construction.
///
/// D1 hardening (a setgid storage root can otherwise forge a false positive on
/// Linux): the probe dir's mode is reset to `0o700` **first, before the control
/// child**, clearing any setgid bit inherited from a setgid root, so the control
/// child observes the filesystem's true default gid (the fsgid) rather than the
/// group the probe dir only carries because it inherited setgid. Without this
/// ordering a setgid root whose gid differs from `candidate` would let the control
/// child read that inherited gid and slip the guard, while a divergent fsgid equal
/// to `candidate` still forged a match after the setgid was later cleared. After
/// the `chgrp` the probe still re-stats and (b) refuses if a setgid bit somehow
/// survived and (c) asserts the `chgrp` actually stuck before trusting the
/// comparison -- a silently-ignored `chgrp` would compare a gid against itself and
/// read "equal".
fn probe_identity(run_dir: &Path, candidate: u32) -> Option<(u32, u32)> {
    let probe = run_dir.join(format!(".umbra-probe-{}", Uuid::new_v4()));
    fs::create_dir(&probe).ok()?;
    // From here every non-success exit path -- `?`, early `return`, or panic --
    // removes the probe tree (both children included) via this guard; the success
    // path removes it explicitly and disarms the guard, see `remove_now` below.
    let guard = ProbeDir::new(&probe);
    // Clear any setgid bit inherited from a setgid storage root *before* creating
    // the control child, so the control child observes the filesystem's real
    // default creation gid (the fsgid on Linux) rather than a setgid-inherited
    // group. See the D1 note above.
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).ok()?;
    // Control child: it measures the gid this filesystem hands a new object by
    // default. If that already equals `candidate`, a match below would be a
    // coincidence, not inheritance, so the probe must not advertise.
    let control = probe.join("control");
    fs::create_dir(&control).ok()?;
    if fs::symlink_metadata(&control).ok()?.gid() == candidate {
        return None;
    }
    std::os::unix::fs::chown(&probe, None, Some(candidate)).ok()?;
    let probe_meta = fs::symlink_metadata(&probe).ok()?;
    if probe_meta.mode() & 0o2000 != 0 {
        return None;
    }
    if probe_meta.gid() != candidate {
        return None;
    }
    let child = probe.join("child");
    fs::create_dir(&child).ok()?;
    let child_meta = fs::symlink_metadata(&child).ok()?;
    let verdict = (probe_meta.gid(), child_meta.gid());
    // Explicit fallible cleanup on the success path: never return a verdict while
    // a probe directory survives under the storage root. A removal failure here
    // fails the probe closed; on success the guard is disarmed so `Drop` does not
    // redundantly retry.
    guard.remove_now().ok()?;
    Some(verdict)
}

/// Timeout bounding a single qualification probe. A healthy probe is a handful of
/// syscalls (local) or two RPCs (nfs), in the millisecond range even over a WAN,
/// so five seconds is >100x headroom over a slow-but-healthy export while keeping
/// a wedged-mount startup delay tolerable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most probe threads allowed live per backend at once (in flight or orphaned
/// past their timeout). `open_run` is `&mut self`, so a healthy provider runs at
/// most a handful of probes concurrently; the headroom over that keeps healthy
/// concurrent qualifications from ever refusing one another, while still bounding a
/// wedged mount to at most this many stranded threads before further probes are
/// refused.
const PROBE_BUDGET: usize = 16;

/// Run `probe` on a dedicated thread and answer its verdict, or `false` if it
/// does not finish within `timeout`.
///
/// The qualification probes do blocking filesystem I/O that a wedged mount can
/// stall indefinitely; this wrapper is the only thing bounding that wait. Every
/// non-answer resolves to `false`, matching the probes' existing fail-closed
/// contract (a store that cannot probe must not fail `open_run` over a capability
/// nothing has asked for yet):
///   - a spawn failure answers `false` rather than panicking (hence
///     `Builder::spawn`, not `thread::spawn`);
///   - a timeout answers `false` and logs, so a timed-out qualification is
///     distinguishable in the logs from one measured `false`;
///   - a panicking probe drops the sender, so `Disconnected` also answers `false`.
///
/// On timeout the thread is orphaned, not killed: it keeps running its syscalls
/// and its verdict is discarded when it finally sends into the dropped receiver.
/// The probe's scratch `.umbra-probe-*` lives in the isolated
/// `<storage-root>/.umbra-probes/` container -- a sibling of the run directory,
/// never inside it -- and is removed by `ProbeDir` even after a late finish. A
/// concurrent flush's `sync_tree` walks only the run directory and the root fsync
/// enumerates nothing, so that late cleanup can no longer race a flush; this is
/// the isolation #101 delivered: <https://github.com/invakid404/umbra/issues/101>.
///
/// A process-global budget caps how many probe threads are live per backend at
/// once -- whether still in flight or orphaned past their timeout -- at
/// `PROBE_BUDGET`. The slot is reserved atomically *before* spawning, so any number
/// of concurrent opens can never collectively exceed the budget; over budget, a
/// call answers `false` immediately (and logs) without spawning and without running
/// the probe. Each slot is released by its own probe thread when it exits (a `Drop`
/// guard, so a panicking probe releases it too), or by the caller if the thread
/// never starts; a timeout does *not* release it, so a wedged mount can strand at
/// most `PROBE_BUDGET` blocked threads before further probes are refused. A budget
/// shared across the two backends would need a common third crate and is out of
/// scope.
fn bounded(timeout: Duration, probe: impl FnOnce() -> bool + Send + 'static) -> bool {
    // Process-global count of live probe threads. Tests drive the guard through
    // `bounded_with` against a private counter, so they never reserve against -- and
    // never refuse against -- this real one.
    static PROBE_LIVE: AtomicUsize = AtomicUsize::new(0);
    bounded_with(&PROBE_LIVE, PROBE_BUDGET, timeout, probe)
}

/// The core of [`bounded`], parameterised over the `live`-thread counter and its
/// `budget` so tests can exercise the guard against a private counter without
/// contending on the process-global one the real `open_run` path uses.
fn bounded_with(
    live: &'static AtomicUsize,
    budget: usize,
    timeout: Duration,
    probe: impl FnOnce() -> bool + Send + 'static,
) -> bool {
    // Reserve one of the `budget` live-thread slots atomically before spawning, so
    // concurrent callers can never collectively exceed it -- closing the
    // check-then-spawn gap a separate load would leave. Over budget fails closed.
    if live
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < budget).then_some(n + 1)
        })
        .is_err()
    {
        tracing::warn!(
            budget,
            "qualification probe budget exhausted; not advertising the capability"
        );
        return false;
    }
    let (tx, rx) = mpsc::channel();
    if thread::Builder::new()
        .name("umbra-probe".into())
        .spawn(move || {
            // Releases the reserved slot when this thread exits -- normal return or
            // panic. Held for the probe's whole life, so a timed-out (orphaned)
            // thread keeps its slot until it finally finishes.
            struct Slot(&'static AtomicUsize);
            impl Drop for Slot {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _slot = Slot(live);
            let _ = tx.send(probe());
        })
        .is_err()
    {
        // The thread never started, so release the reservation the caller made.
        live.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    match rx.recv_timeout(timeout) {
        Ok(answer) => answer,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                timeout_secs = timeout.as_secs_f64(),
                "qualification probe timed out; not advertising the capability"
            );
            false
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

/// Timeout bounding the whole `open_run` layout phase. The layout does far more
/// work than a qualification probe -- up to five `CreateNew` mutations plus the
/// reopen validation -- so it needs more headroom than `PROBE_TIMEOUT`: on a loaded
/// store with slow stable storage a healthy layout can reach low seconds, and 30s
/// is roughly 10x over a bad-but-healthy case while still bounding a wedged mount to
/// a `open_run` that fails in `LAYOUT_TIMEOUT + PROBE_TIMEOUT` rather than never.
///
/// Note (#100): where the caller reaches this backend through the provider IPC
/// proxy, that transport's own request deadline (default 5s) fires first, so this
/// bound only shortens time-to-fail for in-process callers and for registries with
/// a large `timeout_ms`. It is still the only bound for those paths, and the
/// late-work safety it enables (a timed-out layout leaves only residue the reopen
/// validation already refuses) is a correctness fix regardless of which deadline wins.
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(30);

/// The most layout threads allowed live per backend at once (in flight or orphaned
/// past their timeout). Each stranded layout thread is an `open_run` that already
/// failed; after this many the mount is clearly wedged, so further opens fail fast
/// without spawning. Separate from `PROBE_BUDGET` -- a wedged layout must never
/// starve the probe budget or vice versa -- and per-backend for the same reason.
const LAYOUT_BUDGET: usize = 4;

/// Run the `open_run` layout phase `f` on a dedicated thread and return its result,
/// or `StorageUnavailable` if it does not finish within `timeout`.
///
/// This is the layout counterpart of [`bounded`], kept as a separate `Result`-typed
/// generic helper because `bounded`/`bounded_with` are `bool`-typed and part of the
/// untouched probe mechanism (#91). Every non-answer surfaces as `StorageUnavailable`
/// -- the same kind the IPC transport uses for a deadline miss -- so a wedged mount
/// fails `open_run` closed rather than hanging it:
///   - a timeout returns `StorageUnavailable` and logs, distinguishing a wedged
///     layout in the logs from any other open failure;
///   - budget exhaustion returns `StorageUnavailable` immediately, without spawning;
///   - a spawn failure returns `StorageUnavailable` rather than panicking;
///   - a panicking layout drops the sender, so `Disconnected` also fails closed.
///
/// On timeout the thread is orphaned, not killed (a hard-mount D-state thread cannot
/// be killed anyway): it keeps running its syscalls with owned inputs and no
/// reference to `self`, so it can never touch the installed run or the qualification
/// flag. Its late work touches only `<root>/<run_id>/**` and leaves at most the same
/// partial-layout residue a crash mid-`CreateNew` leaves today, which the reopen
/// validation already refuses; there is deliberately no rollback (#100).
///
/// The `live` counter is the calling provider's own (`LocalStorage::layout_live`),
/// not a process-global `static` -- see that field for why. That is the one shape
/// difference from the probe's process-global [`bounded_with`]; the reserve-before-
/// spawn, `Slot`-on-thread and `Builder::spawn` structure is otherwise identical.
fn bounded_layout<T: Send + 'static>(
    live: &Arc<AtomicUsize>,
    timeout: Duration,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    bounded_layout_with(live, LAYOUT_BUDGET, timeout, f)
}

/// The core of [`bounded_layout`], parameterised over the `live`-thread counter and
/// its `budget` so tests can exercise the guard against a private counter with a
/// small budget. Mirrors [`bounded_with`]'s reserve-before-spawn shape, but is
/// `Result`-typed and reserves against a per-provider (rather than `'static`) count.
fn bounded_layout_with<T: Send + 'static>(
    live: &Arc<AtomicUsize>,
    budget: usize,
    timeout: Duration,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    // Reserve one of the `budget` live-thread slots atomically before spawning, so
    // concurrent callers can never collectively exceed it. Over budget fails closed.
    if live
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < budget).then_some(n + 1)
        })
        .is_err()
    {
        tracing::warn!(budget, "layout budget exhausted; mount unresponsive");
        return Err(error(
            ErrorKind::StorageUnavailable,
            "open_run",
            "layout budget exhausted; mount unresponsive",
        ));
    }
    let (tx, rx) = mpsc::channel();
    let slot_live = Arc::clone(live);
    if thread::Builder::new()
        .name("umbra-layout".into())
        .spawn(move || {
            // Releases the reserved slot when this thread exits -- normal return or
            // panic. Held for the layout's whole life, so a timed-out (orphaned)
            // thread keeps its slot until it finally finishes.
            struct Slot(Arc<AtomicUsize>);
            impl Drop for Slot {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _slot = Slot(slot_live);
            let _ = tx.send(f());
        })
        .is_err()
    {
        // The thread never started, so release the reservation the caller made.
        live.fetch_sub(1, Ordering::AcqRel);
        return Err(error(
            ErrorKind::StorageUnavailable,
            "open_run",
            "layout thread spawn failed",
        ));
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                timeout_secs = timeout.as_secs_f64(),
                "layout I/O timed out; mount unresponsive"
            );
            Err(error(
                ErrorKind::StorageUnavailable,
                "open_run",
                "layout I/O timed out; mount unresponsive",
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(error(
            ErrorKind::StorageUnavailable,
            "open_run",
            "layout thread panicked",
        )),
    }
}

/// Measure whether the run's backing filesystem gives a new object its parent
/// directory's gid, probing inside the isolated `probes` container while `run_dir`
/// anchors the dev guard (both share the storage root's filesystem). `false` on
/// the indecisive (single-group) path and on every probe error alike; the run
/// opens normally either way. No `cfg(target_os)` anywhere -- Linux answers
/// `false` here by the kernel's own rule, which is the whole point of #89.
fn parent_identity_probe(probes: &Path, run_dir: &Path) -> bool {
    let Some(candidate) = candidate_group() else {
        return false;
    };
    // Create the isolated probe container here, inside the bounded closure, never
    // on `open_run`'s main thread (#92): a wedged mount must stall this probe
    // thread, not `open_run`. Tolerate a concurrent creator -- the container is a
    // persistent reserved sibling of the run directories, created at most once and
    // deliberately never removed, so a best-effort teardown cannot race a
    // concurrent probe's `create_dir`.
    match fs::create_dir(probes) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    // Defence in depth matching `open_run`'s layout checks: the container must be a
    // real directory, never a symlink that could redirect the probe off the root.
    let Ok(meta) = fs::symlink_metadata(probes) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    // Dev guard (a4): the probe measures a property of the run's backing
    // filesystem, so it must run on the *same* filesystem as the run dir. A mount
    // point under the storage root (at the run dir or at the container) is exotic,
    // but this turns "same FS" from an assumption into a checked fact for one
    // lstat. Diverging devices fail closed.
    let Ok(run_meta) = fs::symlink_metadata(run_dir) else {
        return false;
    };
    if meta.dev() != run_meta.dev() {
        return false;
    }
    match probe_identity(probes, candidate) {
        Some((probe_gid, child_gid)) => parent_identity_matches(probe_gid, child_gid),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use umbra_storage::{
        CreateOptions, ImmutableBaseContract, OperationId, RunId, StoragePolicy, WriterId,
    };

    fn request() -> OpenRunRequest {
        OpenRunRequest {
            run_id: RunId(Uuid::new_v4()),
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "test-base".into(),
                fingerprint: vec![1, 2, 3],
            },
            policy: StoragePolicy {
                read_only: false,
                require_strict_remote_persistence: false,
                require_kernel_shadow: false,
                format_version: 1,
            },
        }
    }

    fn context(lease: &WriterLease) -> RequestContext {
        RequestContext {
            run_id: lease.run_id,
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
            writer_epoch: Some(lease.epoch),
        }
    }

    fn path(bytes: &[u8]) -> StoragePath {
        StoragePath::new(StorageAnchor::Root, bytes.to_vec()).unwrap()
    }

    fn file() -> CreateOptions {
        CreateOptions {
            kind: CreateKind::File,
            mode: 0o600,
        }
    }

    fn setup() -> (TempDir, LocalStorage, OpenRunRequest, WriterLease) {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = LocalStorage::new(dir.path()).unwrap();
        let request = request();
        storage.open_run(&request).unwrap();
        let lease = acquire(&mut storage, request.run_id);
        (dir, storage, request, lease)
    }

    fn acquire(storage: &mut LocalStorage, run_id: RunId) -> WriterLease {
        storage
            .acquire_writer(&AcquireWriterRequest {
                run_id,
                writer_id: WriterId("test".into()),
                takeover: TakeoverPolicy::Refuse,
            })
            .unwrap()
    }

    /// `STORAGE_OWNERSHIP_FIDELITY_V1` is qualified here rather than declared in
    /// the capability function alone: the flag means "the uid/gid a caller names
    /// are applied and reflected in the next `Stat`", and this is the
    /// measurement behind that claim.
    ///
    /// Self-chown rather than cross-uid, because CI has no second identity to
    /// give an object to. That is the shape the claim actually covers -- it is
    /// "what the kernel permits is applied and a refusal is reported", never
    /// "every chown succeeds"; the refusal half is the consumer's to handle and
    /// is exercised in `umbra-overlay` by injecting the `Denied` this maps
    /// `EPERM` to.
    #[test]
    fn set_metadata_applies_mode_and_ownership_and_refuses_what_it_cannot_apply() {
        let (_dir, mut storage, _request, lease) = setup();
        let p = path(b"file");
        storage.create(&context(&lease), &p, &file()).unwrap();
        let before = storage.stat(&context(&lease), &p).unwrap();

        let set = |storage: &mut LocalStorage, update: MetadataUpdate| {
            storage.execute(&StorageRequest {
                context: context(&lease),
                operation: StorageOperation::SetMetadata {
                    path: p.clone(),
                    update,
                },
            })
        };
        let update = MetadataUpdate {
            mode: Some(0o640),
            uid: Some(before.uid),
            gid: Some(before.gid),
            accessed_nanos: None,
            modified_nanos: None,
        };
        let StorageResponse::MetadataSet(stat) = set(&mut storage, update).unwrap() else {
            panic!("set_metadata answers with the object's new stat");
        };
        assert_eq!(
            (stat.mode, stat.uid, stat.gid),
            (0o640, before.uid, before.gid)
        );
        // Reflected in the next `Stat`, not merely in the response.
        let read_back = storage.stat(&context(&lease), &p).unwrap();
        assert_eq!(
            (read_back.mode, read_back.uid, read_back.gid),
            (0o640, before.uid, before.gid)
        );

        // `None` is "leave this one alone", which is what lets the overlay name
        // ownership without touching the mode `create` just settled.
        let update = MetadataUpdate {
            mode: None,
            uid: Some(before.uid),
            gid: None,
            accessed_nanos: None,
            modified_nanos: None,
        };
        set(&mut storage, update).unwrap();
        assert_eq!(storage.stat(&context(&lease), &p).unwrap().mode, 0o640);

        // Refused before anything lands, so an error means nothing happened.
        let empty = MetadataUpdate {
            mode: None,
            uid: None,
            gid: None,
            accessed_nanos: None,
            modified_nanos: None,
        };
        assert_eq!(
            set(&mut storage, empty).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        let bad_mode = MetadataUpdate {
            mode: Some(0o40755),
            uid: None,
            gid: None,
            accessed_nanos: None,
            modified_nanos: None,
        };
        assert_eq!(
            set(&mut storage, bad_mode).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        let timestamp = MetadataUpdate {
            mode: Some(0o600),
            uid: None,
            gid: None,
            accessed_nanos: None,
            modified_nanos: Some(0),
        };
        assert_eq!(
            set(&mut storage, timestamp).unwrap_err().kind,
            ErrorKind::UnsupportedCapability,
            "a timestamp refuses the whole update rather than half-applying it"
        );
        assert_eq!(
            storage.stat(&context(&lease), &p).unwrap().mode,
            0o640,
            "and the mode it named alongside did not land"
        );

        assert!(storage
            .capabilities()
            .features
            .contains(umbra_core::capabilities::STORAGE_OWNERSHIP_FIDELITY_V1));
    }

    #[test]
    fn real_io_offsets_eof_sparse_writes_and_unlink() {
        let (dir, mut storage, request, lease) = setup();
        let p = path(b"file");
        let created = storage.create(&context(&lease), &p, &file()).unwrap();
        assert_eq!(created.stat.kind, ObjectKind::File);
        assert_eq!(
            storage.write_at(&context(&lease), &p, 0, b"hello").unwrap(),
            5
        );
        storage.write_at(&context(&lease), &p, 1, b"A").unwrap();
        storage.write_at(&context(&lease), &p, 7, b"!").unwrap();
        let physical = dir
            .path()
            .join(request.run_id.0.to_string())
            .join("root/file");
        assert_eq!(fs::read(physical).unwrap(), b"hAllo\0\0!");
        let mut out = [9; 12];
        assert_eq!(
            storage.read_at(&context(&lease), &p, 3, &mut out).unwrap(),
            5
        );
        assert_eq!(&out[..5], b"lo\0\0!");
        assert_eq!(&out[5..], &[9; 7]);
        assert_eq!(
            storage
                .read_at(&context(&lease), &p, 100, &mut out)
                .unwrap(),
            0
        );
        assert_eq!(
            storage.read_at(&context(&lease), &p, 0, &mut []).unwrap(),
            0
        );
        let stat = storage.stat(&context(&lease), &p).unwrap();
        assert_eq!(stat.len, 8);
        assert_eq!(stat.object_id, created.stat.object_id);
        assert_eq!(
            storage
                .create(&context(&lease), &p, &file())
                .unwrap_err()
                .kind,
            ErrorKind::AlreadyExists
        );
        storage.unlink(&context(&lease), &p).unwrap();
        assert_eq!(
            storage.stat(&context(&lease), &p).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            storage.unlink(&context(&lease), &p).unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[test]
    fn directories_pagination_and_anchor_isolation() {
        let (_dir, mut storage, _request, lease) = setup();
        storage
            .create(
                &context(&lease),
                &path(b"dir"),
                &CreateOptions {
                    kind: CreateKind::Directory,
                    mode: 0o700,
                },
            )
            .unwrap();
        for name in [b"dir/z".as_slice(), b"dir/a", b"dir/zz"] {
            storage
                .create(&context(&lease), &path(name), &file())
                .unwrap();
        }
        let control = StoragePath::new(StorageAnchor::Control, b"secret".to_vec()).unwrap();
        storage.create(&context(&lease), &control, &file()).unwrap();
        let first = storage
            .list(&context(&lease), &path(b"dir"), None, 2)
            .unwrap();
        assert_eq!(
            first
                .entries
                .iter()
                .map(|e| e.name.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"a", b"z"]
        );
        let second = storage
            .list(&context(&lease), &path(b"dir"), first.next.as_ref(), 2)
            .unwrap();
        assert_eq!(second.entries[0].name.as_bytes(), b"zz");
        assert!(second.next.is_none());
        assert_eq!(
            storage
                .list(&context(&lease), &path(b""), first.next.as_ref(), 2)
                .unwrap_err()
                .kind,
            ErrorKind::StaleHandle
        );
        let root = storage
            .list(&context(&lease), &path(b""), None, 10)
            .unwrap();
        assert_eq!(root.entries.len(), 1);
        assert_eq!(root.entries[0].stat.kind, ObjectKind::Directory);
        storage.unlink(&context(&lease), &path(b"dir/z")).unwrap();
        assert_eq!(
            storage
                .list(&context(&lease), &path(b"dir"), first.next.as_ref(), 2)
                .unwrap_err()
                .kind,
            ErrorKind::StaleHandle
        );
        assert!(storage.unlink(&context(&lease), &path(b"dir")).is_err());
    }

    #[test]
    fn non_utf8_paths_preserve_bytes_and_native_filesystem_errors() {
        let (_dir, mut storage, _request, lease) = setup();
        let p = path(b"raw-\xff");
        let physical = storage.path(&p, true).unwrap();
        assert_eq!(physical.file_name().unwrap().as_bytes(), b"raw-\xff");
        // APFS can reject names which other Unix filesystems accept. Compare with
        // direct std::fs behavior rather than silently converting those bytes.
        match File::create(&physical) {
            Ok(_) => {
                fs::remove_file(&physical).unwrap();
                storage.create(&context(&lease), &p, &file()).unwrap();
                storage.write_at(&context(&lease), &p, 0, b"bytes").unwrap();
                let page = storage
                    .list(&context(&lease), &path(b""), None, 10)
                    .unwrap();
                assert_eq!(page.entries[0].name.as_bytes(), p.as_bytes());
                storage.unlink(&context(&lease), &p).unwrap();
            }
            Err(native) => {
                let actual = storage.create(&context(&lease), &p, &file()).unwrap_err();
                assert_eq!(actual, io_error("create", native));
                assert!(storage
                    .list(&context(&lease), &path(b""), None, 10)
                    .unwrap()
                    .entries
                    .is_empty());
            }
        }
    }

    #[test]
    fn retries_do_not_repeat_effects_and_conflicts_fail() {
        let (_dir, mut storage, _request, lease) = setup();
        let ctx = context(&lease);
        let first = storage.create(&ctx, &path(b"file"), &file()).unwrap();
        assert_eq!(
            storage.create(&ctx, &path(b"file"), &file()).unwrap(),
            first
        );
        assert_eq!(
            storage
                .create(&ctx, &path(b"other"), &file())
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            storage.stat(&ctx, &path(b"other")).unwrap_err().kind,
            ErrorKind::NotFound
        );
        let remove = context(&lease);
        storage.unlink(&remove, &path(b"file")).unwrap();
        storage
            .create(&context(&lease), &path(b"file"), &file())
            .unwrap();
        storage.unlink(&remove, &path(b"file")).unwrap();
        assert!(storage.stat(&ctx, &path(b"file")).is_ok());
    }

    #[test]
    fn flush_reopen_validates_base_and_preserves_data_and_identity() {
        let (dir, mut storage, mut request, lease) = setup();
        let p = path(b"saved");
        let created = storage.create(&context(&lease), &p, &file()).unwrap();
        storage
            .write_at(&context(&lease), &p, 0, b"persistent")
            .unwrap();
        let receipt = storage
            .flush(&FlushRequest {
                context: context(&lease),
                scope: FlushScope::EntireRun,
            })
            .unwrap();
        assert_eq!(receipt.durability, Durability::Local);
        assert!(!storage.capabilities().strict_remote_persistence);
        storage.release_writer(&lease).unwrap();
        storage.close_run().unwrap();
        drop(storage);
        let mut reopened = LocalStorage::new(dir.path()).unwrap();
        request.intent = OpenRunIntent::OpenExisting;
        reopened.open_run(&request).unwrap();
        let next = acquire(&mut reopened, request.run_id);
        assert!(next.epoch.0 > lease.epoch.0);
        assert_eq!(
            reopened.stat(&context(&next), &p).unwrap().object_id,
            created.stat.object_id
        );
        let mut bytes = [0; 10];
        reopened
            .read_at(&context(&next), &p, 0, &mut bytes)
            .unwrap();
        assert_eq!(&bytes, b"persistent");
        reopened.release_writer(&next).unwrap();
        reopened.close_run().unwrap();
        request.immutable_base.fingerprint.push(4);
        assert_eq!(
            reopened.open_run(&request).unwrap_err().kind,
            ErrorKind::ProtocolMismatch
        );
    }

    #[test]
    fn lifecycle_exclusive_writers_stale_epochs_and_read_only() {
        let (dir, mut storage, mut request, lease) = setup();
        assert_eq!(
            storage.open_run(&request).unwrap_err().kind,
            ErrorKind::InvalidState
        );
        assert_eq!(
            storage.close_run().unwrap_err().kind,
            ErrorKind::InvalidState
        );
        assert_eq!(storage.renew_writer(&lease).unwrap(), lease);
        let mut other = LocalStorage::new(dir.path()).unwrap();
        request.intent = OpenRunIntent::OpenExisting;
        other.open_run(&request).unwrap();
        let writer = AcquireWriterRequest {
            run_id: request.run_id,
            writer_id: WriterId("other".into()),
            takeover: TakeoverPolicy::Refuse,
        };
        assert_eq!(
            other.acquire_writer(&writer).unwrap_err().kind,
            ErrorKind::AlreadyExists
        );
        storage.release_writer(&lease).unwrap();
        assert_eq!(
            storage
                .create(&context(&lease), &path(b"stale"), &file())
                .unwrap_err()
                .kind,
            ErrorKind::LeaseLost
        );
        let newer = other.acquire_writer(&writer).unwrap();
        assert!(newer.epoch.0 > lease.epoch.0);
        other.release_writer(&newer).unwrap();
        other.close_run().unwrap();
        request.policy.read_only = true;
        other.open_run(&request).unwrap();
        assert_eq!(
            other.acquire_writer(&writer).unwrap_err().kind,
            ErrorKind::Denied
        );
        storage.close_run().unwrap();
        assert_eq!(
            storage
                .stat(&context(&lease), &path(b"x"))
                .unwrap_err()
                .kind,
            ErrorKind::InvalidState
        );
    }

    #[test]
    fn direct_requests_enforce_bounds_authority_and_unsupported_operations() {
        let (_dir, mut storage, _request, lease) = setup();
        for operation in [
            StorageOperation::ReadAt {
                path: path(b"absent"),
                offset: u64::MAX,
                len: 1,
            },
            StorageOperation::ReadAt {
                path: path(b"absent"),
                offset: 0,
                len: MAX_IO_BYTES as u32 + 1,
            },
            StorageOperation::List {
                path: path(b""),
                cursor: None,
                limit: 0,
            },
            StorageOperation::List {
                path: path(b""),
                cursor: None,
                limit: MAX_DIRECTORY_ENTRIES + 1,
            },
            StorageOperation::WriteAt {
                path: path(b"absent"),
                offset: u64::MAX,
                bytes: vec![1],
            },
        ] {
            assert_eq!(
                storage
                    .execute(&StorageRequest {
                        context: context(&lease),
                        operation
                    })
                    .unwrap_err()
                    .kind,
                ErrorKind::InvalidInput
            );
        }
        let mut ctx = context(&lease);
        ctx.writer_epoch = None;
        assert_eq!(
            storage
                .execute(&StorageRequest {
                    context: ctx,
                    operation: StorageOperation::Create {
                        path: path(b"denied"),
                        options: file()
                    }
                })
                .unwrap_err()
                .kind,
            ErrorKind::LeaseLost
        );
        assert_eq!(
            storage
                .execute(&StorageRequest {
                    context: context(&lease),
                    operation: StorageOperation::AtomicSwap {
                        left: path(b"a"),
                        right: path(b"b")
                    }
                })
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedCapability
        );
        let mut wrong = context(&lease);
        wrong.run_id = RunId(Uuid::new_v4());
        assert_eq!(
            storage.stat(&wrong, &path(b"")).unwrap_err().kind,
            ErrorKind::InvalidState
        );
    }

    #[test]
    fn reject_symlink_escapes_without_modifying_external_data() {
        use std::os::unix::fs::symlink;
        let (dir, mut storage, request, lease) = setup();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"safe").unwrap();
        let root = dir.path().join(request.run_id.0.to_string()).join("root");
        symlink(outside.path(), root.join("escape")).unwrap();
        symlink(outside.path().join("secret"), root.join("link")).unwrap();
        for p in [path(b"escape/secret"), path(b"link")] {
            assert_eq!(
                storage
                    .write_at(&context(&lease), &p, 0, b"bad")
                    .unwrap_err()
                    .kind,
                ErrorKind::InvalidPath
            );
            assert_eq!(
                storage.stat(&context(&lease), &p).unwrap_err().kind,
                ErrorKind::InvalidPath
            );
        }
        assert_eq!(
            storage
                .create(&context(&lease), &path(b"escape/new"), &file())
                .unwrap_err()
                .kind,
            ErrorKind::InvalidPath
        );
        assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"safe");
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn rejects_unqualified_policy_and_refuses_abandoned_writer_takeover() {
        let (dir, storage, mut request, _lease) = setup();
        drop(storage); // Intentionally retains lock: dropping is not proof of quiescence.
        request.intent = OpenRunIntent::OpenExisting;
        let mut other = LocalStorage::new(dir.path()).unwrap();
        request.policy.require_strict_remote_persistence = true;
        assert_eq!(
            other.open_run(&request).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
        request.policy.require_strict_remote_persistence = false;
        other.open_run(&request).unwrap();
        for takeover in [
            TakeoverPolicy::Refuse,
            TakeoverPolicy::FencePreviousWriter,
            TakeoverPolicy::ConfirmedTermination {
                evidence: b"trust me".to_vec(),
            },
        ] {
            assert!(other
                .acquire_writer(&AcquireWriterRequest {
                    run_id: request.run_id,
                    writer_id: WriterId("new".into()),
                    takeover
                })
                .is_err());
        }
    }

    const PARENT_IDENTITY: &str = umbra_core::capabilities::STORAGE_PARENT_IDENTITY_V1;

    fn run_dir(dir: &TempDir, request: &OpenRunRequest) -> PathBuf {
        dir.path().join(request.run_id.0.to_string())
    }

    fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// Injected positive and negative for the pure verdict. The negative pair is
    /// the NFS/Linux shape -- a child whose gid diverges from the chgrp'd parent
    /// -- and injecting it discharges the macOS+NFS acceptance case deterministically
    /// on any platform, with no divergent-identity filesystem mounted (no CI
    /// runner has one).
    #[test]
    fn parent_identity_verdict_is_gid_equality() {
        assert!(parent_identity_matches(4242, 4242));
        assert!(!parent_identity_matches(4242, 20));
    }

    /// A process in a single group offers no candidate, and the contract says the
    /// probe is then silent -- not an error, not a warning. Pure over an injected
    /// group list, so the single-group case is deterministic even on a host (most
    /// dev machines) that in fact has several groups. When there is no candidate
    /// the probe never reaches `probe_identity`, so the indecisive path creates
    /// nothing to leak.
    #[test]
    fn single_group_process_has_no_candidate() {
        assert_eq!(pick_candidate(&[20], 20), None);
        assert_eq!(pick_candidate(&[], 20), None);
        assert_eq!(pick_candidate(&[20, 12, 61], 20), Some(12));
        // The egid need not be first, and is skipped wherever it appears.
        assert_eq!(pick_candidate(&[12, 20], 20), Some(12));
    }

    /// The shrink-race padding rustix can leave on `getgroups` is trailing
    /// `Gid::ROOT` (raw 0), and only that tail is stripped: a real gid that
    /// happens to sit in the interior -- including 0 -- survives, so the sanitiser
    /// never rewrites a genuine credential set.
    #[test]
    fn strip_getgroups_padding_drops_only_trailing_zeros() {
        // The bug shape: stale count leaves a `Gid::ROOT` tail.
        assert_eq!(strip_getgroups_padding(vec![20, 12, 0, 0]), vec![20, 12]);
        // No trailing zero: the set is returned untouched -- no false fire.
        assert_eq!(strip_getgroups_padding(vec![20, 12, 61]), vec![20, 12, 61]);
        // An interior 0 is a legitimate gid (wheel/root's set) and is kept.
        assert_eq!(strip_getgroups_padding(vec![0, 20, 12]), vec![0, 20, 12]);
    }

    /// Degenerate inputs collapse to empty, and an empty set offers no candidate,
    /// so the probe stays on the silent indecisive path rather than acting on
    /// padding.
    #[test]
    fn strip_getgroups_padding_collapses_all_zero_sets() {
        assert_eq!(strip_getgroups_padding(vec![]), Vec::<u32>::new());
        assert_eq!(strip_getgroups_padding(vec![0]), Vec::<u32>::new());
        // Composed with candidate selection: a set that is only egid plus padding
        // yields no candidate, never a spurious one from the stripped 0.
        assert_eq!(
            pick_candidate(&strip_getgroups_padding(vec![20, 0]), 20),
            None
        );
    }

    /// The probe leaves the run directory byte-for-byte as it found it, on both
    /// the failure and the success path. The failure path -- a chgrp that cannot
    /// stick -- creates the probe directory and then bails, so it is the RAII
    /// drop guard, not an explicit cleanup, that has to remove it.
    #[test]
    fn probe_leaves_no_residue_even_when_it_fails_midway() {
        let (dir, _storage, request, _lease) = setup();
        let run = run_dir(&dir, &request);
        let before = entries(&run);

        // gid `u32::MAX` is the raw `chown` "leave unchanged" sentinel ((gid_t)-1),
        // so the chgrp is a silent no-op: `probe_meta.gid()` stays the original and
        // the "did it stick?" guard returns `None` -- but only after the probe
        // directory already exists. Independent of privilege, so it forces the
        // bail even when the suite runs as root.
        assert_eq!(probe_identity(&run, u32::MAX), None);
        assert_eq!(
            entries(&run),
            before,
            "drop guard removed the probe dir after a mid-probe bail"
        );

        // The success path (only where a real candidate exists) must be clean too.
        if let Some(candidate) = candidate_group() {
            let _ = probe_identity(&run, candidate);
            assert_eq!(
                entries(&run),
                before,
                "drop guard removed the probe dir after success"
            );
        }
    }

    /// A read-only run is never probed and never advertises parent identity:
    /// probing would perform the very chgrp mutation the policy forbids.
    #[test]
    fn read_only_run_does_not_advertise_parent_identity() {
        let (dir, mut storage, mut request, lease) = setup();
        storage.release_writer(&lease).unwrap();
        storage.close_run().unwrap();
        let mut reopened = LocalStorage::new(dir.path()).unwrap();
        request.intent = OpenRunIntent::OpenExisting;
        request.policy.read_only = true;
        reopened.open_run(&request).unwrap();
        assert!(!reopened.capabilities().features.contains(PARENT_IDENTITY));
    }

    /// A fresh store advertises nothing about parent identity before any run
    /// opens. The overlay reads `capabilities()` live, not only through
    /// `RunBinding`, so a pre-`open_run` read must answer "not advertised".
    #[test]
    fn fresh_storage_does_not_advertise_parent_identity() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();
        assert!(!storage.capabilities().features.contains(PARENT_IDENTITY));
    }

    /// `capabilities()` reads the qualified field, and `close_run` resets it --
    /// without the reset a closed run keeps advertising with no run open.
    #[test]
    fn close_run_resets_parent_identity_qualification() {
        let (_dir, mut storage, _request, lease) = setup();
        storage.parent_identity_qualified = true;
        assert!(storage.capabilities().features.contains(PARENT_IDENTITY));
        storage.release_writer(&lease).unwrap();
        storage.close_run().unwrap();
        assert!(!storage.capabilities().features.contains(PARENT_IDENTITY));
    }

    /// A refused `open_run` must never leave parent identity advertised: the
    /// overlay reads `capabilities()` live, so an `Err` return that had already
    /// set the flag would advertise a capability of a run that never opened.
    /// After C' the assignment is the last statement before `self.run =
    /// Some(...)`, so every reachable refusal returns before it. This table
    /// walks each reachable refusal and asserts BOTH halves of the invariant --
    /// nothing advertised AND no run installed.
    ///
    /// Honest grade: this is a forward-looking regression net, not a
    /// reproduction of a live bug. It passes at master too, because no reachable
    /// `open_run` input fails *after* the assignment today (the two
    /// `directory_binding` calls do no I/O and cannot `Err`). It would start
    /// catching regressions the moment a genuinely fallible step were added
    /// below the assignment, or the assignment drifted back above one.
    #[test]
    fn open_run_failure_never_advertises_parent_identity() {
        // Each closure parks a store immediately before a *failing* `open_run`,
        // returning the `TempDir` too so it outlives the store.
        #[allow(clippy::type_complexity)]
        let cases: Vec<(
            &str,
            Box<dyn Fn() -> (TempDir, LocalStorage, OpenRunRequest)>,
        )> = vec![
            (
                "require_kernel_shadow",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    req.policy.require_kernel_shadow = true;
                    (dir, storage, req)
                }),
            ),
            (
                "require_strict_remote_persistence",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    req.policy.require_strict_remote_persistence = true;
                    (dir, storage, req)
                }),
            ),
            (
                "format_version",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    req.policy.format_version = 2;
                    (dir, storage, req)
                }),
            ),
            (
                "read_only_create_new",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    req.policy.read_only = true; // intent stays CreateNew
                    (dir, storage, req)
                }),
            ),
            (
                "invalid_layout",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let mut storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    storage.open_run(&req).unwrap();
                    drop(storage);
                    let run = dir.path().join(req.run_id.0.to_string());
                    fs::remove_dir_all(run.join("root")).unwrap();
                    fs::write(run.join("root"), b"regular file, not a directory").unwrap();
                    req.intent = OpenRunIntent::OpenExisting;
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    (dir, storage, req)
                }),
            ),
            (
                "missing_manifest",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let mut storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    storage.open_run(&req).unwrap();
                    drop(storage);
                    let run = dir.path().join(req.run_id.0.to_string());
                    fs::remove_file(run.join("manifest")).unwrap();
                    req.intent = OpenRunIntent::OpenExisting;
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    (dir, storage, req)
                }),
            ),
            (
                "manifest_mismatch",
                Box::new(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let mut storage = LocalStorage::new(dir.path()).unwrap();
                    let mut req = request();
                    storage.open_run(&req).unwrap();
                    drop(storage);
                    req.intent = OpenRunIntent::OpenExisting;
                    req.immutable_base.fingerprint.push(9);
                    let storage = LocalStorage::new(dir.path()).unwrap();
                    (dir, storage, req)
                }),
            ),
        ];

        for (name, make) in cases {
            let (_dir, mut storage, req) = make();
            assert!(
                storage.open_run(&req).is_err(),
                "case `{name}` was expected to fail open_run"
            );
            assert!(
                !storage.capabilities().features.contains(PARENT_IDENTITY),
                "case `{name}` advertised parent identity after a failed open_run"
            );
            assert!(
                storage.run.is_none(),
                "case `{name}` left a run installed after a failed open_run"
            );
        }

        // "run already open" is the one refusal that legitimately leaves a run
        // in place, so `run.is_none()` cannot be asserted for it -- a run is
        // *supposed* to be open. The invariant here is the complementary one:
        // the refused second `open_run` disturbs neither the live run nor the
        // flag it already carries.
        let (_dir, mut storage, _request, _lease) = setup();
        let advertised_before = storage.capabilities().features.contains(PARENT_IDENTITY);
        assert_eq!(
            storage.open_run(&request()).unwrap_err().kind,
            ErrorKind::InvalidState
        );
        assert!(storage.run.is_some());
        assert_eq!(
            storage.capabilities().features.contains(PARENT_IDENTITY),
            advertised_before,
            "a refused second open_run changed the live run's advertisement"
        );
    }

    /// Pins the invariant `parent_identity_qualified == true => run.is_some()`
    /// at every observable boundary of a full open -> close -> reopen cycle,
    /// including a read-only reopen. After C' the assignment and `self.run =
    /// Some(...)` are adjacent, so the flag cannot be observed set with no run.
    ///
    /// Honest grade: like its sibling, this is a forward-looking net rather than
    /// a reproduction. It passes at master because nothing fails between the
    /// assignment and `self.run = Some(...)` today; its value is guarding that
    /// adjacency against future edits.
    #[test]
    fn open_run_qualifies_only_after_its_fallible_work() {
        let assert_inv = |storage: &LocalStorage| {
            assert!(
                !storage.parent_identity_qualified || storage.run.is_some(),
                "parent_identity_qualified is set with no run open"
            );
            // The overlay reads capabilities() live; it must agree with the flag.
            assert_eq!(
                storage.capabilities().features.contains(PARENT_IDENTITY),
                storage.parent_identity_qualified
            );
        };

        let dir = tempfile::tempdir().unwrap();
        let mut storage = LocalStorage::new(dir.path()).unwrap();
        assert_inv(&storage); // fresh: no run, flag clear.

        let mut request = request();
        storage.open_run(&request).unwrap();
        assert_inv(&storage); // open: run present, flag is whatever the probe said.

        let lease = acquire(&mut storage, request.run_id);
        storage.release_writer(&lease).unwrap();
        storage.close_run().unwrap();
        assert_inv(&storage);
        assert!(storage.run.is_none() && !storage.parent_identity_qualified);

        // Writable reopen.
        request.intent = OpenRunIntent::OpenExisting;
        storage.open_run(&request).unwrap();
        assert_inv(&storage);
        let lease = acquire(&mut storage, request.run_id);
        storage.release_writer(&lease).unwrap();
        storage.close_run().unwrap();
        assert_inv(&storage);

        // Read-only reopen: never probed, so never advertised, run open or not.
        request.policy.read_only = true;
        storage.open_run(&request).unwrap();
        assert_inv(&storage);
        assert!(!storage.parent_identity_qualified);
        storage.close_run().unwrap();
        assert_inv(&storage);
    }

    /// The live end-to-end outcome, asserted against an *independent* ground-truth
    /// measurement rather than a hardcoded platform expectation -- so there is no
    /// `cfg(target_os)` even here. Skipped, with a message, on a single-group host
    /// where the probe is indecisive by construction (E1).
    #[test]
    fn open_run_advertises_iff_the_filesystem_carries_parent_gid() {
        let (dir, storage, request, _lease) = setup();
        let run = run_dir(&dir, &request);
        let Some(candidate) = candidate_group() else {
            eprintln!(
                "open_run_advertises_iff_the_filesystem_carries_parent_gid: \
                 no candidate group on this host; skipping live assertion"
            );
            return;
        };
        // Ground truth: chgrp a fresh directory ourselves and observe whether a
        // child inherits that gid. `open_run`'s own probe used the same candidate
        // on the same filesystem, so the two must agree -- no platform baked in.
        let truth = run.join("truth");
        fs::create_dir(&truth).unwrap();
        // Mirror the probe's ordering exactly: clear any inherited setgid *before*
        // the control child, so the control child observes the filesystem's true
        // default creation gid (the fsgid on Linux) rather than a setgid-inherited
        // group -- otherwise this ground truth would be less honest than the probe
        // it cross-checks.
        fs::set_permissions(&truth, fs::Permissions::from_mode(0o700)).unwrap();
        // Same fsgid guard the probe applies: if the default creation gid already
        // equals the candidate, neither this ground truth nor the probe can tell
        // real inheritance from coincidence, so there is nothing to cross-check --
        // the probe fails closed and so do we.
        fs::create_dir(truth.join("control")).unwrap();
        if fs::symlink_metadata(truth.join("control")).unwrap().gid() == candidate {
            eprintln!(
                "open_run_advertises_iff_the_filesystem_carries_parent_gid: \
                 default creation gid equals the candidate; ground truth ambiguous, skipping"
            );
            fs::remove_dir_all(&truth).unwrap();
            return;
        }
        std::os::unix::fs::chown(&truth, None, Some(candidate)).unwrap();
        let dir_gid = fs::symlink_metadata(&truth).unwrap().gid();
        // Decisive only if our own chgrp stuck; otherwise neither we nor the probe
        // learned anything and there is nothing to cross-check.
        if dir_gid == candidate {
            fs::create_dir(truth.join("child")).unwrap();
            let child_gid = fs::symlink_metadata(truth.join("child")).unwrap().gid();
            let expected = child_gid == dir_gid;
            assert_eq!(
                storage.capabilities().features.contains(PARENT_IDENTITY),
                expected,
                "open_run's probe must match a direct gid observation on this host"
            );
        }
        fs::remove_dir_all(&truth).unwrap();
    }

    /// The success path removes the probe tree explicitly and disarms the guard,
    /// so a run can never qualify while a `.umbra-probe-*` survives and `Drop`
    /// becomes a no-op backstop.
    ///
    /// The cleanup-*failure* branch (removal errors after a measurement) has no
    /// test: forcing `remove_dir_all` to fail on a tree the test owns is not
    /// portable -- the usual permission trick that fails as a normal user
    /// succeeds as root, which Linux CI frequently runs as, so any such test
    /// would be flaky rather than deterministic. The branch is a plain
    /// `.ok()?`, exercised by the same fail-closed machinery every other probe
    /// error path uses.
    #[test]
    fn probe_dir_explicit_cleanup_removes_tree_and_disarms_drop() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join(".umbra-probe-test");
        fs::create_dir(&probe).unwrap();
        fs::create_dir(probe.join("child")).unwrap();
        let guard = ProbeDir::new(&probe);
        guard.remove_now().unwrap();
        assert!(!probe.exists(), "explicit cleanup removed the probe tree");
        assert!(
            !guard.armed.get(),
            "a successful removal disarms the Drop backstop"
        );
        // Dropping the disarmed guard at end of scope must not error or re-remove.
    }

    /// The #101 isolation invariant, structurally: after a writable `open_run` the
    /// run directory carries only its own `{root, control, epoch, manifest}` and no
    /// probe scratch, while the probe's isolated `.umbra-probes` container sits
    /// under the storage root -- a sibling of the run directory, never inside it --
    /// and is empty, every probe having removed its own uuid subtree. Because the
    /// probe never touches anything inside the run dir, `flush`'s `sync_tree` of the
    /// run can no longer see probe churn.
    ///
    /// The container-existence half is asserted only where a candidate group exists
    /// (as the live end-to-end test also gates on it): with no candidate the probe
    /// is indecisive by construction and never reaches the container creation.
    #[test]
    fn probe_target_is_isolated_from_the_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = LocalStorage::new(dir.path()).unwrap();
        let request = request();
        storage.open_run(&request).unwrap();
        let run = run_dir(&dir, &request);

        // The run directory holds only its own structure -- no `.umbra-probe*`.
        let mut expected: Vec<std::ffi::OsString> = ["control", "epoch", "manifest", "root"]
            .iter()
            .map(Into::into)
            .collect();
        expected.sort();
        assert_eq!(
            entries(&run),
            expected,
            "the run directory carries no probe scratch"
        );

        if candidate_group().is_some() {
            let probes = dir.path().join(".umbra-probes");
            assert!(
                probes.is_dir(),
                "the isolated probe container exists under the storage root"
            );
            assert_eq!(
                entries(&probes),
                Vec::<std::ffi::OsString>::new(),
                "every probe removed its own uuid subtree, leaving the container empty"
            );
            assert!(
                !probes.starts_with(&run),
                "the probe container is not inside the run directory"
            );
        }
    }

    /// A probe hammering the isolated container concurrently with repeated flushes
    /// never disturbs a flush: on the fix the outcome is deterministic, because
    /// `sync_tree` walks only the run directory and the probe touches only
    /// `.umbra-probes`. (The e1 mutation witness -- aiming the same churn at the run
    /// dir, where flushes then fail intermittently -- is run by hand in review, with
    /// no `cfg(test)` seam committed into `sync_tree`.)
    #[test]
    fn flush_is_unaffected_by_concurrent_probe_churn() {
        let (dir, mut storage, _request, lease) = setup();
        let probes = dir.path().join(".umbra-probes");
        // The churn thread needs the container to exist; `open_run`'s own probe may
        // already have made it, so tolerate `AlreadyExists`.
        match fs::create_dir(&probes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("could not create probe container: {e}"),
        }
        // A real candidate exercises the full success path; `u32::MAX` (the chown
        // "leave unchanged" sentinel) drives the create-then-bail path independent
        // of privilege. Either way each iteration creates and removes a uuid subtree
        // under `.umbra-probes`.
        let candidate = candidate_group().unwrap_or(u32::MAX);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = {
            let probes = probes.clone();
            let stop = std::sync::Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = probe_identity(&probes, candidate);
                }
            })
        };

        for _ in 0..200 {
            storage
                .flush(&FlushRequest {
                    context: context(&lease),
                    scope: FlushScope::EntireRun,
                })
                .expect("flush is deterministically Ok while the probe stays off the run dir");
        }

        stop.store(true, Ordering::Release);
        churn.join().unwrap();
    }

    // A private live-probe-thread counter for the `bounded` unit tests. Reserving
    // slots here to exercise the budget never touches the process-global counter the
    // `open_run` tests drive, so the two cannot refuse each other under `cargo
    // test`'s parallelism.
    static TEST_LIVE: AtomicUsize = AtomicUsize::new(0);

    // A deliberately small budget for the concurrency test, so it can oversubscribe
    // it with only a handful of threads.
    const TEST_BUDGET: usize = 3;

    /// Shadows the crate's [`bounded`] within this module so every `bounded` unit
    /// test runs against `TEST_LIVE`/`TEST_BUDGET` rather than the real probe
    /// counter.
    fn bounded(timeout: Duration, probe: impl FnOnce() -> bool + Send + 'static) -> bool {
        bounded_with(&TEST_LIVE, TEST_BUDGET, timeout, probe)
    }

    /// Serializes the tests that exercise `bounded`. `TEST_LIVE` is a single
    /// `static`, so these tests must neither run concurrently nor start while a
    /// previous test's just-released probe thread is still draining. Holding this
    /// lock excludes the others; waiting for the counter to reach zero gives each
    /// test an empty budget to start from.
    fn serialize_bounded_tests() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let start = std::time::Instant::now();
        while TEST_LIVE.load(Ordering::Acquire) != 0 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the live-probe counter did not return to zero between bounded tests"
            );
            thread::sleep(Duration::from_millis(5));
        }
        guard
    }

    #[test]
    fn bounded_returns_the_probe_verdict_when_it_finishes_in_time() {
        let _serial = serialize_bounded_tests();
        assert!(
            bounded(Duration::from_secs(5), || true),
            "a true verdict passes through"
        );
        assert!(
            !bounded(Duration::from_secs(5), || false),
            "a false verdict passes through"
        );
    }

    #[test]
    fn bounded_answers_false_when_the_probe_outlives_the_timeout() {
        let _serial = serialize_bounded_tests();
        // The probe parks on a channel whose sender the test keeps alive, so it is
        // still running when the timeout fires. Milliseconds, not seconds, keep the
        // test fast; the elapsed bound asserts `bounded` did not wait for the probe.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let start = std::time::Instant::now();
        let answer = bounded(Duration::from_millis(50), move || {
            let _ = release_rx.recv();
            true
        });
        assert!(
            !answer,
            "a probe still running at the timeout answers false"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "bounded returned on the timeout, not after the probe finished"
        );
        // Release the orphaned thread so it exits cleanly instead of leaking.
        let _ = release_tx.send(());
    }

    #[test]
    fn bounded_answers_false_when_the_probe_panics() {
        let _serial = serialize_bounded_tests();
        assert!(
            !bounded(Duration::from_secs(5), || panic!("probe blew up")),
            "a panicking probe drops the sender and answers false"
        );
    }

    #[test]
    fn bounded_never_exceeds_the_budget_under_concurrent_callers() {
        let _serial = serialize_bounded_tests();

        // Oversubscribe the budget: launch budget + 2 callers that all reserve
        // before any can time out (a long timeout, and probes that park until the
        // test releases them). The atomic reservation must admit exactly `budget`
        // and refuse the rest without running their closures.
        const N: usize = TEST_BUDGET + 2;
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let refused = std::sync::Arc::new(AtomicUsize::new(0));
        // The admitted probes and this test thread meet here to release together.
        let gate = std::sync::Arc::new(std::sync::Barrier::new(TEST_BUDGET + 1));
        // All callers line up here so their reservations race at once.
        let lineup = std::sync::Arc::new(std::sync::Barrier::new(N));

        let handles: Vec<_> = (0..N)
            .map(|_| {
                let ran = std::sync::Arc::clone(&ran);
                let refused = std::sync::Arc::clone(&refused);
                let gate = std::sync::Arc::clone(&gate);
                let lineup = std::sync::Arc::clone(&lineup);
                thread::spawn(move || {
                    lineup.wait();
                    let answer = bounded(Duration::from_secs(5), move || {
                        ran.fetch_add(1, Ordering::AcqRel);
                        gate.wait(); // hold the slot until the test releases us
                        true
                    });
                    if !answer {
                        refused.fetch_add(1, Ordering::AcqRel);
                    }
                    answer
                })
            })
            .collect();

        // Wait until every caller has resolved: exactly `budget` are parked in their
        // probe and the other two were refused without running.
        let start = std::time::Instant::now();
        while ran.load(Ordering::Acquire) < TEST_BUDGET
            || refused.load(Ordering::Acquire) < N - TEST_BUDGET
        {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "exactly the budget should reserve a slot; the rest are refused"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            ran.load(Ordering::Acquire),
            TEST_BUDGET,
            "exactly the budget ran their closure"
        );
        assert_eq!(
            refused.load(Ordering::Acquire),
            N - TEST_BUDGET,
            "the over-budget callers answered false without running"
        );

        // Release the parked probes; their threads exit and free their slots.
        gate.wait();
        let answers: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            answers.iter().filter(|&&a| a).count(),
            TEST_BUDGET,
            "the admitted callers saw their probe's verdict"
        );

        // The counter returns to zero once every probe thread has exited.
        let drain = std::time::Instant::now();
        while TEST_LIVE.load(Ordering::Acquire) != 0 {
            assert!(
                drain.elapsed() < Duration::from_secs(5),
                "every reserved slot is released after the probes exit"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // After release, a fresh probe runs again now that the budget has freed up.
        assert!(
            bounded(Duration::from_secs(5), || true),
            "a new probe runs once the budget frees up"
        );
    }

    // A deliberately small budget for the `bounded_layout` concurrency test, so it
    // can oversubscribe it with only a handful of threads. Each test owns a fresh
    // `Arc<AtomicUsize>` counter (matching how a real provider owns `layout_live`),
    // so these tests need no cross-test serialization at all.
    const TEST_LAYOUT_BUDGET: usize = 3;

    #[test]
    fn bounded_layout_passes_an_in_time_result_through_unchanged() {
        let live = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            bounded_layout(&live, Duration::from_secs(5), || Ok(42u32)).unwrap(),
            42,
            "an in-time Ok passes through"
        );
        let err = bounded_layout(&live, Duration::from_secs(5), || {
            Err::<(), _>(error(ErrorKind::ProtocolMismatch, "open_run", "mismatch"))
        })
        .unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::ProtocolMismatch,
            "an in-time Err passes through with its own kind, not remapped"
        );
    }

    #[test]
    fn bounded_layout_times_out_to_storage_unavailable() {
        let live = Arc::new(AtomicUsize::new(0));
        // The layout parks on a channel the test keeps open, so it is still running
        // when the timeout fires. Milliseconds keep the test fast; the elapsed bound
        // asserts `bounded_layout` did not wait for the layout to finish.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let start = std::time::Instant::now();
        let err = bounded_layout(&live, Duration::from_millis(50), move || {
            let _ = release_rx.recv();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StorageUnavailable);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "bounded_layout returned on the timeout, not after the layout finished"
        );
        let _ = release_tx.send(()); // release the orphan so it exits cleanly
    }

    #[test]
    fn bounded_layout_maps_a_panicking_layout_to_storage_unavailable() {
        let live = Arc::new(AtomicUsize::new(0));
        let err = bounded_layout(&live, Duration::from_secs(5), || -> Result<()> {
            panic!("layout blew up")
        })
        .unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::StorageUnavailable,
            "a panicking layout drops the sender and fails closed"
        );
    }

    #[test]
    fn bounded_layout_never_exceeds_the_budget_and_spawns_nothing_over_it() {
        // A fresh per-test counter: the guard admits exactly `TEST_LAYOUT_BUDGET`.
        let live = Arc::new(AtomicUsize::new(0));
        // Oversubscribe the budget: launch budget + 2 callers that all reserve before
        // any can time out. The atomic reservation must admit exactly `budget` and
        // refuse the rest with `StorageUnavailable` without running their closures.
        const N: usize = TEST_LAYOUT_BUDGET + 2;
        let ran = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::Barrier::new(TEST_LAYOUT_BUDGET + 1));
        let lineup = Arc::new(std::sync::Barrier::new(N));

        let handles: Vec<_> = (0..N)
            .map(|_| {
                let live = Arc::clone(&live);
                let ran = Arc::clone(&ran);
                let refused = Arc::clone(&refused);
                let gate = Arc::clone(&gate);
                let lineup = Arc::clone(&lineup);
                thread::spawn(move || {
                    lineup.wait();
                    let result = bounded_layout_with(
                        &live,
                        TEST_LAYOUT_BUDGET,
                        Duration::from_secs(5),
                        move || {
                            ran.fetch_add(1, Ordering::AcqRel);
                            gate.wait(); // hold the slot until the test releases us
                            Ok(())
                        },
                    );
                    if let Err(ref e) = result {
                        assert_eq!(e.kind, ErrorKind::StorageUnavailable);
                        refused.fetch_add(1, Ordering::AcqRel);
                    }
                    result.is_ok()
                })
            })
            .collect();

        let start = std::time::Instant::now();
        while ran.load(Ordering::Acquire) < TEST_LAYOUT_BUDGET
            || refused.load(Ordering::Acquire) < N - TEST_LAYOUT_BUDGET
        {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "exactly the budget should reserve a slot; the rest are refused"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ran.load(Ordering::Acquire), TEST_LAYOUT_BUDGET);
        assert_eq!(refused.load(Ordering::Acquire), N - TEST_LAYOUT_BUDGET);

        gate.wait();
        let admitted = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(admitted, TEST_LAYOUT_BUDGET);

        let drain = std::time::Instant::now();
        while live.load(Ordering::Acquire) != 0 {
            assert!(
                drain.elapsed() < Duration::from_secs(5),
                "every reserved slot is released after the layouts exit"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// #100 test 1: a layout that fails surfaces cleanly. A failing layout -- exactly
    /// what `bounded_layout` hands back on a timeout, budget exhaustion, spawn failure
    /// or panic -- must surface *before* the qualification line: `open_run` returns
    /// `StorageUnavailable`, installs no run, advertises no parent identity, and (the
    /// proof no probe was even spawned) never creates the isolated `.umbra-probes`
    /// container. `bounded_layout`'s own timeout behaviour is pinned by the helper
    /// tests above, on the private counter, so this test drives the seam with a
    /// fast-failing layout rather than parking a thread on the real budget the other
    /// `open_run` tests share -- mirroring how the probe suite keeps its parking on
    /// `TEST_LIVE` and never on `PROBE_LIVE`.
    #[test]
    fn open_run_layout_failure_installs_no_run_and_spawns_no_probe() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = LocalStorage::new(dir.path()).unwrap();
        let request = request();
        let err = storage
            .open_run_with(&request, LAYOUT_TIMEOUT, || -> Result<()> {
                Err(error(
                    ErrorKind::StorageUnavailable,
                    "open_run",
                    "layout I/O timed out; mount unresponsive",
                ))
            })
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StorageUnavailable);
        assert!(storage.run.is_none(), "a failed layout installs no run");
        assert!(
            !storage.capabilities().features.contains(PARENT_IDENTITY),
            "a failed layout advertises no parent identity"
        );
        assert!(
            !dir.path().join(".umbra-probes").exists(),
            "a failed layout returns before the probe line, so nothing is spawned"
        );
    }

    /// #100 test 2: an orphaned layout that loses the claim race touches nothing. An
    /// orphan is exactly what a timed-out `open_run` leaves behind -- a detached
    /// thread still running `local_layout`. Model it directly (off the bounded
    /// counter, so it cannot starve the budget the parallel suite shares) parked
    /// *before* its claim `create_dir`. A retry of the same id then wins the claim
    /// and opens intact; releasing the orphan, its first op gets `AlreadyExists` and
    /// it stops. Two writers never interleave inside one run dir.
    #[test]
    fn open_run_orphan_that_loses_the_claim_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let request = request();
        let directory = dir.path().join(request.run_id.0.to_string());

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let orphan = {
            let directory = directory.clone();
            let request = request.clone();
            thread::spawn(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                local_layout(directory, request)
            })
        };
        started_rx.recv().unwrap(); // the orphan is parked before its claim

        // The retry wins the claim with the real layout and opens.
        let mut retry = LocalStorage::new(dir.path()).unwrap();
        retry.open_run(&request).unwrap();

        // Release the orphan: its first op now loses the claim and it stops.
        release_tx.send(()).unwrap();
        assert_eq!(
            orphan.join().unwrap().unwrap_err().kind,
            ErrorKind::AlreadyExists,
            "the orphan that lost the claim stops at its first op"
        );

        // The retry's run is intact: exactly the four expected entries, and the
        // manifest holds the retry's own bytes untouched.
        assert_eq!(
            fs::read(directory.join("manifest")).unwrap(),
            manifest(&request)
        );
        let mut entries: Vec<String> = fs::read_dir(&directory)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["control", "epoch", "manifest", "root"]);
    }

    /// #100 test 3: a "complete but unclaimed" run. Let an orphaned layout finish
    /// completely while nobody holds the run. The result is a valid run: a same-id
    /// `CreateNew` gets `AlreadyExists`, and `OpenExisting` reopens it. It is garbage
    /// like crash residue, but it is safe -- the supervisor always uses a fresh
    /// run_id, so nothing collides with it.
    #[test]
    fn open_run_orphan_may_complete_an_unclaimed_run() {
        let dir = tempfile::tempdir().unwrap();
        let request = request();
        let directory = dir.path().join(request.run_id.0.to_string());

        // The orphan runs `local_layout` to completion with no holder.
        let orphan = {
            let directory = directory.clone();
            let request = request.clone();
            thread::spawn(move || local_layout(directory, request))
        };
        orphan.join().unwrap().unwrap();

        // A same-id CreateNew now finds the complete residue.
        let mut retry = LocalStorage::new(dir.path()).unwrap();
        assert_eq!(
            retry.open_run(&request).unwrap_err().kind,
            ErrorKind::AlreadyExists
        );
        // And OpenExisting reopens the complete-but-unclaimed run.
        let mut reopen = LocalStorage::new(dir.path()).unwrap();
        let mut req = request.clone();
        req.intent = OpenRunIntent::OpenExisting;
        reopen.open_run(&req).unwrap();
    }

    /// #100 test 4: every partial-layout residue an orphan can leave is refused. Each
    /// case is built directly on disk (the design's stall positions), then both an
    /// `OpenExisting` (the poison detector) and a same-id `CreateNew` must refuse it.
    /// This pins the property the "no rollback" decision relies on.
    #[test]
    fn open_run_refuses_every_partial_layout_residue() {
        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, Box<dyn Fn(&Path)>)> = vec![
            (
                "dir only",
                Box::new(|run: &Path| {
                    fs::create_dir(run).unwrap();
                }),
            ),
            (
                "+root",
                Box::new(|run: &Path| {
                    fs::create_dir(run).unwrap();
                    fs::create_dir(run.join("root")).unwrap();
                }),
            ),
            (
                "+control",
                Box::new(|run: &Path| {
                    fs::create_dir(run).unwrap();
                    fs::create_dir(run.join("root")).unwrap();
                    fs::create_dir(run.join("control")).unwrap();
                }),
            ),
            (
                "+epoch, no manifest",
                Box::new(|run: &Path| {
                    fs::create_dir(run).unwrap();
                    fs::create_dir(run.join("root")).unwrap();
                    fs::create_dir(run.join("control")).unwrap();
                    fs::write(run.join("epoch"), 0u64.to_le_bytes()).unwrap();
                }),
            ),
            (
                "+short manifest",
                Box::new(|run: &Path| {
                    fs::create_dir(run).unwrap();
                    fs::create_dir(run.join("root")).unwrap();
                    fs::create_dir(run.join("control")).unwrap();
                    fs::write(run.join("epoch"), 0u64.to_le_bytes()).unwrap();
                    fs::write(run.join("manifest"), b"short").unwrap();
                }),
            ),
        ];

        for (name, build) in cases {
            let dir = tempfile::tempdir().unwrap();
            let request = request();
            let run = dir.path().join(request.run_id.0.to_string());
            build(&run);

            let mut opener = LocalStorage::new(dir.path()).unwrap();
            let mut reopen = request.clone();
            reopen.intent = OpenRunIntent::OpenExisting;
            assert!(
                opener.open_run(&reopen).is_err(),
                "OpenExisting must refuse partial residue `{name}`"
            );

            let mut creator = LocalStorage::new(dir.path()).unwrap();
            assert_eq!(
                creator.open_run(&request).unwrap_err().kind,
                ErrorKind::AlreadyExists,
                "a same-id CreateNew over residue `{name}` gets AlreadyExists"
            );
        }
    }
}
