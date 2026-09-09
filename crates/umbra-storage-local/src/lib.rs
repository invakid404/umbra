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
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use umbra_core::Errno;
use umbra_storage::{
    AcquireWriterRequest, BlobStat, BytePath, CreateKind, DirectoryEntry, DirectoryPage,
    Durability, DurabilityReceipt, ErrorKind, Fencing, FlushRequest, FlushScope, IdempotencyKey,
    LeaseEpoch, ListCursor, ObjectId, ObjectKind, ObjectResult, OpenRunIntent, OpenRunRequest,
    RequestContext, Result, RunBinding, RuntimeDirectoryBinding, Storage, StorageAnchor,
    StorageCapabilities, StorageHandle, StorageOperation, StoragePath, StorageRequest,
    StorageResponse, TakeoverPolicy, UmbraError, WriterLease, MAX_DIRECTORY_ENTRIES, MAX_IO_BYTES,
};
use uuid::Uuid;

/// Local storage rooted at a caller-owned, trusted filesystem directory.
#[derive(Debug)]
pub struct LocalStorage {
    directory: PathBuf,
    run: Option<OpenRun>,
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
            StorageOperation::Unlink { path } => {
                let physical = self.path(path, false)?;
                fs::remove_file(physical).map_err(|e| io_error("unlink", e))?;
                Ok(StorageResponse::Unlinked)
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
            _ => Err(unsupported("execute")),
        }
    }
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

impl Storage for LocalStorage {
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            // Explicitly a development store: it advertises the local mode and
            // the narrow rewrite surface, and nothing about remote durability.
            features: [
                umbra_core::capabilities::STORAGE_LOCAL_DEVELOPMENT_V1.to_owned(),
                umbra_core::capabilities::STORAGE_OPEN_REWRITE_V1.to_owned(),
            ]
            .into_iter()
            .collect(),
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
        if request.intent == OpenRunIntent::CreateNew {
            fs::create_dir(&directory).map_err(|e| io_error("open_run", e))?;
            // A failed initialization remains unopenable, rather than publishing a binding.
            fs::create_dir(directory.join("root")).map_err(|e| io_error("open_run", e))?;
            fs::create_dir(directory.join("control")).map_err(|e| io_error("open_run", e))?;
            fs::write(directory.join("epoch"), 0u64.to_le_bytes())
                .map_err(|e| io_error("open_run", e))?;
            fs::write(directory.join("manifest"), manifest(request))
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
        if fs::read(manifest_path).map_err(|e| io_error("open_run", e))? != manifest(request) {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "open_run",
                "run/base/format mismatch",
            ));
        }
        let binding = RunBinding {
            run_id: request.run_id,
            root: directory_binding(&directory.join("root"))?,
            control: directory_binding(&directory.join("control"))?,
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
}
