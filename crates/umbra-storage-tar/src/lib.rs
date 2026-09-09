//! Indexed tar storage with persistent staging and exclusive API writer authority.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg(unix)]

mod archive;
mod operations;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use umbra_core::*;
use umbra_storage::Storage;
use uuid::Uuid;

const LEASE_MILLIS: u64 = 60_000;
const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;

/// Runtime configuration. One archive contains one run, independent of its filename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarStorageConfig {
    /// Absolute archive filename; its parent directory must already exist.
    pub archive_path: PathBuf,
    /// Relative prefix before the run-ID directory inside the archive.
    pub run_parent: StoragePath,
    /// Single, nonreserved component for the root anchor (default `root`).
    pub root_anchor: BytePath,
    /// Single, nonreserved component for the control anchor (default `control`).
    pub control_anchor: BytePath,
}
impl TarStorageConfig {
    /// Configure an archive with an empty run parent and root/control anchors.
    pub fn new(archive_path: impl Into<PathBuf>) -> Self {
        Self {
            archive_path: archive_path.into(),
            run_parent: StoragePath::new(StorageAnchor::Root, Vec::new()).unwrap(),
            root_anchor: BytePath::new(b"root".to_vec()).unwrap(),
            control_anchor: BytePath::new(b"control".to_vec()).unwrap(),
        }
    }
    /// Decode provider options: JSON encoding of a core BytePath archive filename.
    pub fn from_options(options: &[u8]) -> Result<Self> {
        let path: BytePath = umbra_core::provider::decode(options)?;
        Ok(Self::new(OsStr::from_bytes(path.as_bytes())))
    }
    fn validate(&self) -> Result<()> {
        let bytes = self.archive_path.as_os_str().as_bytes();
        if !self.archive_path.is_absolute() || bytes.contains(&0) || bytes == b"/" {
            return Err(error(
                ErrorKind::InvalidPath,
                "absolute archive filename required",
            ));
        }
        StoragePath::new(StorageAnchor::Root, bytes[1..].to_vec())?;
        for anchor in [&self.root_anchor, &self.control_anchor] {
            StoragePath::new(StorageAnchor::Root, anchor.as_bytes().to_vec())?;
            if anchor.as_bytes().contains(&b'/') || anchor.as_bytes() == b".provider" {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "single nonreserved anchor required",
                ));
            }
        }
        if self.root_anchor == self.control_anchor
            || self.run_parent.anchor() != StorageAnchor::Root
            || self
                .run_parent
                .as_bytes()
                .split(|b| *b == b'/')
                .any(|c| c == b".provider")
        {
            return Err(error(
                ErrorKind::InvalidPath,
                "overlapping anchors or invalid run parent",
            ));
        }
        Ok(())
    }
}

/// Tar provider. Drop preserves staged snapshots and abandoned writer locks.
#[derive(Debug)]
pub struct TarStorage {
    config: TarStorageConfig,
    run: Option<Run>,
}
#[derive(Debug)]
struct Run {
    request: OpenRunRequest,
    archive_path: PathBuf,
    private: PathBuf,
    state: archive::State,
    lease: Option<(WriterLease, Instant)>,
    pages: HashMap<Vec<u8>, (StoragePath, Vec<DirectoryEntry>, usize)>,
    pending_error: Option<UmbraError>,
}
fn error(kind: ErrorKind, message: &str) -> UmbraError {
    UmbraError::new(kind, "tar", message)
}
fn io(e: std::io::Error) -> UmbraError {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorKind::NotFound,
        std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => ErrorKind::Denied,
        _ => ErrorKind::Io,
    };
    let result = error(kind, &e.to_string());
    match e.raw_os_error() {
        Some(n) => result.with_errno(Errno(n)),
        None => result,
    }
}
fn json_error(e: serde_json::Error) -> UmbraError {
    error(ErrorKind::CorruptJournal, &e.to_string())
}
fn unsupported() -> UmbraError {
    error(
        ErrorKind::UnsupportedCapability,
        "operation is not supported by indexed tar storage",
    )
}
// Physical paths are provider configuration, never logical request paths. The
// parent is canonicalized once so aliases share writer authority. Sidecar files
// belong to the trusted provider and must not be edited by other host processes.
fn regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1 {
        return Err(error(
            ErrorKind::InvalidPath,
            "expected private regular file without links",
        ));
    }
    File::open(path).map_err(io)
}
use std::os::unix::fs::MetadataExt;
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path).map_err(io)?.sync_all().map_err(io)
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_dir(path.parent().unwrap())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(io(e)),
    }
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0 {
        return Err(error(
            ErrorKind::InvalidPath,
            "sidecar must be a private directory",
        ));
    }
    Ok(())
}
fn create_record(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    sync_dir(path.parent().unwrap())
}
fn replace_record(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_file_name(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| {
        create_record(&temp, bytes)?;
        fs::rename(&temp, path).map_err(io)?;
        sync_dir(path.parent().unwrap())
    })();
    let _ = fs::remove_file(temp);
    result
}
impl TarStorage {
    /// Retain configuration without filesystem access; open validates it lazily.
    pub fn new(config: TarStorageConfig) -> Self {
        Self { config, run: None }
    }
    /// Validate configuration for provider IPC, without creating an archive.
    pub fn connect(config: TarStorageConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self::new(config))
    }
    fn run(&self) -> Result<&Run> {
        self.run
            .as_ref()
            .ok_or_else(|| error(ErrorKind::InvalidState, "no open run"))
    }
    fn check_lease(&self, lease: &WriterLease, live: bool) -> Result<()> {
        let run = self.run()?;
        let Some((current, deadline)) = &run.lease else {
            return Err(error(ErrorKind::LeaseLost, "no writer"));
        };
        if current != lease || (live && Instant::now() >= *deadline) {
            return Err(error(ErrorKind::LeaseLost, "stale or expired lease"));
        }
        let mut token = Vec::new();
        use std::io::Read;
        regular(&run.private.join("writer.lock"))
            .map_err(|e| match e.kind {
                ErrorKind::NotFound | ErrorKind::InvalidPath => {
                    error(ErrorKind::LeaseLost, "writer lock missing or replaced")
                }
                _ => e,
            })?
            .take(17)
            .read_to_end(&mut token)
            .map_err(io)?;
        if token != lease.renewal_token {
            return Err(error(ErrorKind::LeaseLost, "writer lock changed"));
        }
        Ok(())
    }
    fn context(&self, ctx: &RequestContext, mutation: bool) -> Result<()> {
        let run = self.run()?;
        if run.request.run_id != ctx.run_id {
            return Err(error(ErrorKind::InvalidState, "wrong run"));
        }
        if mutation {
            let lease = &run
                .lease
                .as_ref()
                .ok_or_else(|| error(ErrorKind::LeaseLost, "no writer"))?
                .0;
            if run.request.policy.read_only || ctx.writer_epoch != Some(lease.epoch) {
                return Err(error(ErrorKind::LeaseLost, "stale or missing epoch"));
            }
            self.check_lease(lease, true)?;
            if ctx.idempotency_key.0.is_empty() || ctx.idempotency_key.0.len() > 100 {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "idempotency key must contain 1..100 bytes",
                ));
            }
        }
        Ok(())
    }
}
impl Storage for TarStorage {
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            // The tar backend qualifies neither run mode, so it advertises no
            // mode feature and cannot be selected by `umbra run`.
            features: Default::default(),
            durability: Durability::Local,
            strict_remote_persistence: false,
            fencing: Fencing::ConfirmedTermination,
            kernel_shadow: false,
            complete_emulation: false,
            hard_links: false,
            logical_symlinks: true,
            xattrs: false,
            atomic_replace: true,
            atomic_swap: false,
            max_io_bytes: MAX_IO_BYTES as u32,
            max_directory_entries: MAX_DIRECTORY_ENTRIES,
        }
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        if self.run.is_some() {
            return Err(error(ErrorKind::InvalidState, "already open"));
        }
        self.config.validate()?;
        if request.policy.require_kernel_shadow || request.policy.require_strict_remote_persistence
        {
            return Err(unsupported());
        }
        if request.policy.format_version != 1 {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "expected format version 1",
            ));
        }
        if request.policy.read_only && request.intent == OpenRunIntent::CreateNew {
            return Err(error(ErrorKind::Denied, "read-only create"));
        }
        let parent = self
            .config
            .archive_path
            .parent()
            .unwrap()
            .canonicalize()
            .map_err(io)?;
        let archive_path = parent.join(self.config.archive_path.file_name().unwrap());
        let mut sidecar = archive_path.as_os_str().to_os_string();
        sidecar.push(".provider");
        let private = PathBuf::from(sidecar);
        let mut state = match request.intent {
            OpenRunIntent::CreateNew => {
                // No side effects if an existing archive is invalid or already present.
                if fs::symlink_metadata(&archive_path).is_ok()
                    || private.try_exists().map_err(io)?
                {
                    return Err(error(
                        ErrorKind::AlreadyExists,
                        "archive or sidecar already exists",
                    ));
                }
                let state = archive::State::new(request, &self.config);
                archive::publish(&state, &archive_path, true)?;
                state
            }
            OpenRunIntent::OpenExisting => archive::load(&archive_path)?,
        };
        state.validate_identity(request, &self.config)?;
        private_dir(&private)?;
        private_dir(&private.join("retries"))?;
        let epoch = private.join("epoch");
        if !epoch.try_exists().map_err(io)? {
            match create_record(&epoch, &0u64.to_le_bytes()) {
                Ok(()) => (),
                Err(e) if e.kind == ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
        }
        let staged = private.join("retries/state.tar");
        if staged.try_exists().map_err(io)? {
            state = archive::load(&staged)?;
            state.validate_identity(request, &self.config)?;
        }
        let binding = || RuntimeDirectoryBinding {
            handle: StorageHandle(Uuid::new_v4().as_bytes().to_vec()),
            physical_path: None,
        };
        let result = RunBinding {
            run_id: request.run_id,
            root: binding(),
            control: binding(),
            capabilities: self.capabilities(),
        };
        self.run = Some(Run {
            request: request.clone(),
            archive_path,
            private,
            state,
            lease: None,
            pages: HashMap::new(),
            pending_error: None,
        });
        Ok(result)
    }
    fn acquire_writer(&mut self, request: &AcquireWriterRequest) -> Result<WriterLease> {
        let run = self.run()?;
        if request.run_id != run.request.run_id || run.lease.is_some() {
            return Err(error(ErrorKind::InvalidState, "wrong run or active lease"));
        }
        if run.request.policy.read_only {
            return Err(error(ErrorKind::Denied, "read-only run"));
        }
        if request.takeover != TakeoverPolicy::Refuse {
            return Err(unsupported());
        }
        let token = Uuid::new_v4().as_bytes().to_vec();
        // Failures after exclusive creation deliberately retain the lock.
        create_record(&run.private.join("writer.lock"), &token)?;
        use std::io::Read;
        let mut bytes = Vec::new();
        regular(&run.private.join("epoch"))?
            .take(9)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        let bytes: [u8; 8] = bytes
            .try_into()
            .map_err(|_| error(ErrorKind::CorruptJournal, "invalid epoch"))?;
        let epoch = u64::from_le_bytes(bytes)
            .checked_add(1)
            .ok_or_else(|| error(ErrorKind::InvalidState, "epoch exhausted"))?;
        replace_record(&run.private.join("epoch"), &epoch.to_le_bytes())?;
        // Another session may have published changes since this object opened.
        let staged = run.private.join("retries/state.tar");
        let state = archive::load(if staged.try_exists().map_err(io)? {
            &staged
        } else {
            &run.archive_path
        })?;
        state.validate_identity(&run.request, &self.config)?;
        let lease = WriterLease {
            run_id: request.run_id,
            writer_id: request.writer_id.clone(),
            epoch: LeaseEpoch(epoch),
            renewal_token: token,
            renew_after_millis: LEASE_MILLIS,
        };
        let run = self.run.as_mut().unwrap();
        run.state = state;
        run.pages.clear();
        run.lease = Some((
            lease.clone(),
            Instant::now() + Duration::from_millis(LEASE_MILLIS),
        ));
        Ok(lease)
    }
    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        self.check_lease(lease, true)?;
        self.run.as_mut().unwrap().lease.as_mut().unwrap().1 =
            Instant::now() + Duration::from_millis(LEASE_MILLIS);
        Ok(lease.clone())
    }
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        self.check_lease(lease, false)?;
        if let Some(e) = &self.run()?.pending_error {
            return Err(e.clone());
        }
        fs::remove_file(self.run()?.private.join("writer.lock")).map_err(io)?;
        self.run.as_mut().unwrap().lease = None;
        let result = sync_dir(&self.run()?.private);
        if let Err(e) = &result {
            self.run.as_mut().unwrap().pending_error = Some(e.clone());
        }
        result
    }
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        operations::cross_anchor(&request.operation)?;
        umbra_storage::validate_request(&self.capabilities(), request)?;
        let mutation = request.operation.is_mutation();
        self.context(&request.context, mutation)?;
        if !mutation {
            return self.dispatch_read(&request.operation);
        }
        if let Some(e) = &self.run()?.pending_error {
            return Err(e.clone());
        }
        let run = self.run()?;
        for (prior, result) in &run.state.retries {
            if prior.context.operation_id == request.context.operation_id
                || prior.context.idempotency_key == request.context.idempotency_key
            {
                // Epoch authorizes this attempt; operation identity survives writer renewal/reacquisition.
                if prior.context.run_id != request.context.run_id
                    || prior.context.operation_id != request.context.operation_id
                    || prior.context.idempotency_key != request.context.idempotency_key
                    || prior.operation != request.operation
                {
                    return Err(error(
                        ErrorKind::InvalidInput,
                        "conflicting retry identity or payload",
                    ));
                }
                return result.clone();
            }
        }
        let mut candidate = run.state.clone();
        let result = operations::mutate(&mut candidate, &request.operation);
        if result.is_err() {
            candidate = run.state.clone();
        }
        candidate.retries.push((request.clone(), result.clone()));
        // Capacity rejection precedes I/O and does not poison a clean session.
        let manifest = archive::encode_manifest(&candidate)?;
        let staged = run.private.join("retries/state.tar");
        let persist = archive::publish_prepared(&candidate, &manifest, &staged, false)
            .and_then(|()| archive::load(&staged));
        match persist {
            Ok(state) => {
                let run = self.run.as_mut().unwrap();
                run.state = state;
                if result.is_ok() {
                    run.pages.clear();
                }
                result
            }
            Err(e) => {
                self.run.as_mut().unwrap().pending_error = Some(e.clone());
                Err(e)
            }
        }
    }
    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        self.context(&request.context, true)?;
        if let Some(e) = &self.run()?.pending_error {
            return Err(e.clone());
        }
        match &request.scope {
            FlushScope::Data { objects } | FlushScope::DataAndMetadata { objects } => {
                if objects.iter().any(|id| {
                    !self
                        .run
                        .as_ref()
                        .unwrap()
                        .state
                        .nodes
                        .values()
                        .any(|n| &n.stat.object_id == id)
                }) {
                    return Err(error(ErrorKind::NotFound, "flush object not found"));
                }
            }
            FlushScope::EntireRun => (),
        }
        let run = self.run()?;
        // All scopes strengthen to the entire snapshot. Publication never deletes staging.
        if let Err(e) = archive::publish(&run.state, &run.archive_path, false) {
            self.run.as_mut().unwrap().pending_error = Some(e.clone());
            return Err(e);
        }
        self.context(&request.context, true)?;
        let mut evidence = b"tar snapshot: ".to_vec();
        evidence.extend_from_slice(self.run()?.archive_path.as_os_str().as_bytes());
        evidence.extend_from_slice(b"; file fsync, atomic rename, parent directory fsync (local)");
        Ok(DurabilityReceipt {
            run_id: request.context.run_id,
            writer_epoch: request.context.writer_epoch.unwrap(),
            scope: request.scope.clone(),
            durability: Durability::Local,
            evidence,
        })
    }
    fn close_run(&mut self) -> Result<()> {
        let run = self.run()?;
        if let Some(e) = &run.pending_error {
            return Err(e.clone());
        }
        if run.lease.is_some() {
            return Err(error(ErrorKind::InvalidState, "release writer first"));
        }
        self.run = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> OpenRunRequest {
        OpenRunRequest {
            run_id: RunId(Uuid::new_v4()),
            intent: OpenRunIntent::CreateNew,
            immutable_base: ImmutableBaseContract {
                identity: "base".into(),
                fingerprint: vec![1],
            },
            policy: StoragePolicy {
                read_only: false,
                require_kernel_shadow: false,
                require_strict_remote_persistence: false,
                format_version: 1,
            },
        }
    }
    fn writer(run_id: RunId) -> AcquireWriterRequest {
        AcquireWriterRequest {
            run_id,
            writer_id: WriterId("test".into()),
            takeover: TakeoverPolicy::Refuse,
        }
    }
    fn ctx(lease: &WriterLease) -> RequestContext {
        RequestContext {
            run_id: lease.run_id,
            writer_epoch: Some(lease.epoch),
            operation_id: OperationId(Uuid::new_v4()),
            idempotency_key: IdempotencyKey(Uuid::new_v4().to_string()),
        }
    }
    #[test]
    fn config_validation_is_lazy_and_options_preserve_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let config =
            TarStorageConfig::new(temp.path().join(OsStr::from_bytes(b"archive-\xff.tar")));
        let options = umbra_core::provider::encode(
            &BytePath::new(config.archive_path.as_os_str().as_bytes().to_vec()).unwrap(),
        )
        .unwrap();
        assert_eq!(TarStorageConfig::from_options(&options).unwrap(), config);
        assert!(TarStorageConfig::from_options(b"garbage").is_err());
        let mut invalid = Vec::new();
        for p in ["relative.tar", "/", "/a/../b", "/a\0b"] {
            let mut c = config.clone();
            c.archive_path = p.into();
            invalid.push(c);
        }
        for anchor in [b".".as_slice(), b"..", b".provider", b"a/b"] {
            let mut c = config.clone();
            c.root_anchor = BytePath::new(anchor.to_vec()).unwrap();
            invalid.push(c);
        }
        let mut c = config.clone();
        c.root_anchor = c.control_anchor.clone();
        invalid.push(c);
        let mut c = config.clone();
        c.run_parent = StoragePath::new(StorageAnchor::Control, Vec::new()).unwrap();
        invalid.push(c);
        for c in invalid {
            let mut s = TarStorage::new(c);
            assert!(s.open_run(&request()).is_err());
            assert!(s.run.is_none());
        }
        assert!(!config.archive_path.exists());
    }
    #[test]
    fn expiration_blocks_mutations_renewal_and_takeover_but_allows_release() {
        let temp = tempfile::tempdir().unwrap();
        let config = TarStorageConfig::new(temp.path().join("run.tar"));
        let mut s = TarStorage::new(config.clone());
        let mut req = request();
        s.open_run(&req).unwrap();
        let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
        assert_eq!(lease.renew_after_millis, 60_000);
        s.run.as_mut().unwrap().lease.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            s.renew_writer(&lease).unwrap_err().kind,
            ErrorKind::LeaseLost
        );
        assert_eq!(
            s.create(
                &ctx(&lease),
                &StoragePath::new(StorageAnchor::Root, b"file").unwrap(),
                &CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o600
                }
            )
            .unwrap_err()
            .kind,
            ErrorKind::LeaseLost
        );
        req.intent = OpenRunIntent::OpenExisting;
        let mut other = TarStorage::new(config);
        other.open_run(&req).unwrap();
        assert_eq!(
            other.acquire_writer(&writer(req.run_id)).unwrap_err().kind,
            ErrorKind::AlreadyExists
        );
        s.release_writer(&lease).unwrap();
        s.close_run().unwrap();
        let next = other.acquire_writer(&writer(req.run_id)).unwrap();
        assert!(next.epoch.0 > lease.epoch.0);
        assert_eq!(
            other.release_writer(&lease).unwrap_err().kind,
            ErrorKind::LeaseLost
        );
        other.release_writer(&next).unwrap();
        other.close_run().unwrap();
    }
    #[test]
    fn persistence_failure_has_no_receipt_and_poisoned_close_is_not_clean() {
        let temp = tempfile::tempdir().unwrap();
        let config = TarStorageConfig::new(temp.path().join("run.tar"));
        let mut s = TarStorage::new(config);
        let req = request();
        s.open_run(&req).unwrap();
        let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
        // Point only this test instance at an absent parent to force publication failure.
        s.run.as_mut().unwrap().archive_path = temp.path().join("absent/run.tar");
        assert!(s
            .flush(&FlushRequest {
                context: ctx(&lease),
                scope: FlushScope::EntireRun
            })
            .is_err());
        assert!(s.run.as_ref().unwrap().pending_error.is_some());
        assert!(s.release_writer(&lease).is_err());
        assert!(s.close_run().is_err());
        assert!(s.run.is_some());
    }
    #[test]
    fn staging_failure_does_not_publish_candidate_or_allow_later_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let mut s = TarStorage::new(TarStorageConfig::new(temp.path().join("run.tar")));
        let req = request();
        s.open_run(&req).unwrap();
        let lease = s.acquire_writer(&writer(req.run_id)).unwrap();
        let path = StoragePath::new(StorageAnchor::Root, b"file").unwrap();
        let stage = s.run.as_ref().unwrap().private.join("retries/state.tar");
        fs::create_dir(&stage).unwrap();
        assert!(s
            .create(
                &ctx(&lease),
                &path,
                &CreateOptions {
                    kind: CreateKind::File,
                    mode: 0o600
                }
            )
            .is_err());
        assert_eq!(
            s.stat(&ctx(&lease), &path).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert!(s.close_run().is_err());
    }
}
