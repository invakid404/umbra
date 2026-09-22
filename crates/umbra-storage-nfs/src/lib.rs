//! Mounted NFSv4 storage with byte-preserving descriptor-relative operations.
#![deny(missing_docs)]
#![cfg(any(target_os = "macos", target_os = "linux"))]
mod mount;
mod native;
mod operations;

use native::{error, io, unsupported};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use umbra_core::*;
use umbra_storage::Storage;
use uuid::Uuid;
const LEASE_MILLIS: u64 = 60_000;
const RECORD_LIMIT: u64 = 16 * 1024 * 1024;

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
/// The probe's SETATTR re-applies the ownership of a fresh
/// `<run_parent>/.umbra-probes/<uuid>/` directory umbra created and owns, never the
/// live `.provider`, so a late finish cannot bump `.provider`'s ctime and poison a
/// concurrent flush's sticky health. The probe removes its own `<uuid>` directory
/// even after a late finish; the container is a persistent reserved sibling of the
/// run directories, outside everything `flush` stamps or enumerates. This is the
/// isolation #101 delivered: <https://github.com/invakid404/umbra/issues/101>.
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

/// Runtime layout, separate from persistent run identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NfsStorageConfig {
    /// Exact existing NFSv4 mount point; never created by this provider.
    pub mount_root: PathBuf,
    /// Existing mount-relative parent containing run-ID directories.
    pub run_parent: StoragePath,
    /// Single component naming the tracee-visible anchor within each run.
    pub root_anchor: BytePath,
    /// Single component naming the control anchor within each run.
    pub control_anchor: BytePath,
}
impl NfsStorageConfig {
    /// Configure runs directly beneath the supplied mount, with root/control anchors.
    pub fn new(mount_root: impl Into<PathBuf>) -> Self {
        Self {
            mount_root: mount_root.into(),
            run_parent: StoragePath::new(StorageAnchor::Root, Vec::new()).unwrap(),
            root_anchor: BytePath::new(b"root".to_vec()).unwrap(),
            control_anchor: BytePath::new(b"control".to_vec()).unwrap(),
        }
    }
    /// Decode shipped provider options: JSON encoding of core's BytePath mount root.
    pub fn from_options(options: &[u8]) -> Result<Self> {
        let root: BytePath = umbra_core::provider::decode(options)?;
        Ok(Self::new(OsStr::from_bytes(root.as_bytes())))
    }
    fn validate(&self) -> Result<()> {
        let bytes = self.mount_root.as_os_str().as_bytes();
        if !self.mount_root.is_absolute() || bytes.contains(&0) {
            return Err(error(
                ErrorKind::InvalidPath,
                "config",
                "mount root must be absolute without NUL",
            ));
        }
        StoragePath::new(StorageAnchor::Root, bytes[1..].to_vec())?;
        for anchor in [&self.root_anchor, &self.control_anchor] {
            StoragePath::new(StorageAnchor::Root, anchor.as_bytes().to_vec())?;
            if anchor.as_bytes().contains(&b'/') || anchor.as_bytes() == b".provider" {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "config",
                    "anchor must be a single nonreserved component",
                ));
            }
        }
        if self.root_anchor == self.control_anchor
            || self.run_parent.anchor() != StorageAnchor::Root
        {
            return Err(error(
                ErrorKind::InvalidPath,
                "config",
                "anchors must be disjoint; run parent is mount-relative",
            ));
        }
        Ok(())
    }
}
/// Provider for an externally managed NFSv4 mount. Dropping retains writer locks.
#[derive(Debug)]
pub struct NfsStorage {
    config: NfsStorageConfig,
    run: Option<Run>,
    health: native::FlushHealth,
    /// True only once an existing exact NFSv4 mount was actually validated.
    /// Capability advertisement reads this, so a backend built with `new` (which
    /// performs no validation) cannot claim a mount it never checked.
    validated: bool,
    /// True only once a real ownership update succeeded against this run's store.
    ///
    /// Separate from `validated` because it answers a different question about a
    /// different layer: `validated` is "this is the NFSv4 mount you named",
    /// this is "this export honours SETATTR owner attributes from this client".
    /// A mount can be exactly what was asked for and still refuse chown, so
    /// neither implies the other. Reset when the run closes, because the
    /// qualification was against *that* store.
    ownership_qualified: bool,
}
#[derive(Debug)]
struct Run {
    request: OpenRunRequest,
    directory: File,
    parent: File,
    mount: File,
    root: File,
    control: File,
    private: File,
    lease: Option<(WriterLease, Instant)>,
    pages: HashMap<Vec<u8>, Page>,
}
#[derive(Debug)]
struct Page {
    path: StoragePath,
    directory: File,
    entries: native::Entries,
    stamp: (u64, i64, i64, i64, i64),
}
fn read_file(dir: &File, name: &[u8]) -> Result<Vec<u8>> {
    let file = native::regular(dir, name, libc::O_RDONLY)?;
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io("read record", e))?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(error(
            ErrorKind::CorruptJournal,
            "record",
            "record too large",
        ));
    }
    Ok(bytes)
}
fn create_file(dir: &File, name: &[u8], bytes: &[u8]) -> Result<()> {
    let mut file = native::open(
        dir,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    file.write_all(bytes).map_err(|e| io("write record", e))?;
    native::sync(&file)?;
    native::sync(dir)
}
fn replace_file(dir: &File, name: &[u8], bytes: &[u8]) -> Result<()> {
    let temp = format!("tmp-{}", Uuid::new_v4());
    create_file(dir, temp.as_bytes(), bytes)?;
    native::rename(dir, temp.as_bytes(), dir, name, false, false)?;
    native::sync(dir)
}
fn json_error(e: serde_json::Error) -> UmbraError {
    error(ErrorKind::CorruptJournal, "record", &e.to_string())
}
fn binding(path: PathBuf) -> Result<RuntimeDirectoryBinding> {
    Ok(RuntimeDirectoryBinding {
        handle: StorageHandle(Uuid::new_v4().as_bytes().to_vec()),
        physical_path: Some(BytePath::new(path.as_os_str().as_bytes().to_vec())?),
    })
}
fn object(stat: BlobStat) -> ObjectResult {
    ObjectResult {
        stat,
        rewrite_target: None,
    }
}

impl NfsStorage {
    /// Retain runtime configuration; open_run validates before any run I/O.
    pub fn new(config: NfsStorageConfig) -> Self {
        Self {
            config,
            run: None,
            health: native::FlushHealth::default(),
            validated: false,
            ownership_qualified: false,
        }
    }
    /// Validate configuration and negotiated NFSv4 immediately, for provider IPC.
    pub fn connect(config: NfsStorageConfig) -> Result<Self> {
        config.validate()?;
        mount::validate(&config.mount_root)?;
        let mut storage = Self::new(config);
        storage.validated = true;
        Ok(storage)
    }
    /// Return the runtime mount point.
    pub fn mount_root(&self) -> &Path {
        &self.config.mount_root
    }
    fn run(&self) -> Result<&Run> {
        self.run
            .as_ref()
            .ok_or_else(|| error(ErrorKind::InvalidState, "run", "no run open"))
    }
    fn anchor(&self, path: &StoragePath) -> Result<&File> {
        let run = self.run()?;
        Ok(match path.anchor() {
            StorageAnchor::Root => &run.root,
            StorageAnchor::Control => &run.control,
        })
    }
    fn parent(&self, path: &StoragePath, create: bool) -> Result<(File, Vec<u8>)> {
        native::parent(self.anchor(path)?, path.as_bytes(), create)
    }
    fn stat_path(&self, path: &StoragePath) -> Result<BlobStat> {
        if path.as_bytes().is_empty() {
            return native::stat(self.anchor(path)?, b"");
        }
        let (dir, name) = self.parent(path, false)?;
        native::stat(&dir, &name)
    }
    fn check_lease(&self, lease: &WriterLease, require_live: bool) -> Result<()> {
        self.health.check()?;
        let run = self.run()?;
        let Some((current, deadline)) = &run.lease else {
            return Err(error(ErrorKind::LeaseLost, "writer", "no writer"));
        };
        if current != lease || (require_live && Instant::now() >= *deadline) {
            return Err(error(
                ErrorKind::LeaseLost,
                "writer",
                "stale or expired lease",
            ));
        }
        if read_file(&run.private, b"writer.lock")? != lease.renewal_token {
            return Err(error(
                ErrorKind::LeaseLost,
                "writer",
                "lock ownership changed",
            ));
        }
        Ok(())
    }
    fn flush_run(&self, authority: &mut impl FnMut() -> Result<()>) -> Result<()> {
        self.health.check()?;
        authority()?;
        let run = self.run()?;
        let validate_mount = || {
            mount::validate(&self.config.mount_root)?;
            let current = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&self.config.mount_root)
                .map_err(|e| io("flush mount", e))?;
            if native::stat(&current, b"")?.object_id != native::stat(&run.mount, b"")?.object_id {
                return Err(error(
                    ErrorKind::StaleHandle,
                    "flush mount",
                    "mount identity changed",
                ));
            }
            let parent = native::walk(&current, self.config.run_parent.as_bytes(), false)?;
            if native::stat(&parent, b"")?.object_id != native::stat(&run.parent, b"")?.object_id {
                return Err(error(
                    ErrorKind::StaleHandle,
                    "flush parent",
                    "run parent identity changed",
                ));
            }
            native::same_device(
                &run.parent,
                current
                    .metadata()
                    .map_err(|e| io("flush mount fstat", e))?
                    .dev(),
            )?;
            Ok(())
        };
        let result = (|| {
            validate_mount()?;
            let id = run.request.run_id.0.to_string();
            native::flush(&run.directory, &run.parent, &self.health, &mut || {
                authority()?;
                native::verify_entry(&run.parent, id.as_bytes(), &run.directory)?;
                native::verify_entry(
                    &run.directory,
                    self.config.root_anchor.as_bytes(),
                    &run.root,
                )?;
                native::verify_entry(
                    &run.directory,
                    self.config.control_anchor.as_bytes(),
                    &run.control,
                )?;
                native::verify_entry(&run.directory, b".provider", &run.private)
            })?;
            validate_mount()?;
            authority()
        })();
        result.map_err(|e| self.health.fail(e))
    }
    fn context(&self, context: &RequestContext, mutation: bool) -> Result<()> {
        let run = self.run()?;
        if run.request.run_id != context.run_id {
            return Err(error(ErrorKind::InvalidState, "request", "wrong run"));
        }
        if mutation {
            let lease = &run
                .lease
                .as_ref()
                .ok_or_else(|| error(ErrorKind::LeaseLost, "request", "no writer"))?
                .0;
            if run.request.policy.read_only || context.writer_epoch != Some(lease.epoch) {
                return Err(error(
                    ErrorKind::LeaseLost,
                    "request",
                    "stale or missing epoch",
                ));
            }
            self.check_lease(lease, true)?;
            if context.idempotency_key.0.is_empty() || context.idempotency_key.0.len() > 100 {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "request",
                    "idempotency key must contain 1..100 bytes",
                ));
            }
        }
        Ok(())
    }
}
impl Storage for NfsStorage {
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            // Only successful mount validation in connect or open_run earns
            // this capability. Construction/configuration alone does not.
            features: {
                let mut features = std::collections::BTreeSet::new();
                features.insert(umbra_core::capabilities::STORAGE_OPEN_REWRITE_V1.to_owned());
                // Only a *measured* ownership update earns this. The syscall is
                // always available here -- `SetMetadata` calls `fchownat`
                // unconditionally -- so availability proves nothing: whether the
                // update is honoured belongs to the export, not to this crate,
                // and an export that refuses it answers `ENOTSUP`. The name's own
                // doc comment requires qualification "against a live store, never
                // from configuration alone", so `open_run` probes and this reads
                // the result, exactly as the NFSv4 claim below reads `validated`.
                if self.ownership_qualified {
                    features
                        .insert(umbra_core::capabilities::STORAGE_OWNERSHIP_FIDELITY_V1.to_owned());
                }
                if self.validated {
                    features.insert(umbra_core::capabilities::STORAGE_MOUNTED_NFSV4_V1.to_owned());
                }
                features
            },
            durability: Durability::Local,
            strict_remote_persistence: false,
            fencing: Fencing::ConfirmedTermination,
            kernel_shadow: false,
            complete_emulation: false,
            hard_links: false,
            logical_symlinks: false,
            xattrs: false,
            atomic_replace: true,
            // NFSv4 RENAME has no atomic exchange. Native exchange is implemented
            // but cannot be advertised for an unqualified export.
            atomic_swap: false,
            max_io_bytes: MAX_IO_BYTES as u32,
            max_directory_entries: MAX_DIRECTORY_ENTRIES,
        }
    }
    fn open_run(&mut self, request: &OpenRunRequest) -> Result<RunBinding> {
        self.health.check()?;
        if self.run.is_some() {
            return Err(error(ErrorKind::InvalidState, "open_run", "already open"));
        }
        if request.policy.require_kernel_shadow || request.policy.require_strict_remote_persistence
        {
            return Err(unsupported("run policy"));
        }
        if request.policy.format_version != 1 {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "open_run",
                "expected format version 1",
            ));
        }
        if request.policy.read_only && request.intent == OpenRunIntent::CreateNew {
            return Err(error(ErrorKind::Denied, "open_run", "read-only create"));
        }
        self.config.validate()?;
        mount::validate(&self.config.mount_root)?;
        self.validated = true;
        // Walk the configured absolute path from /, rejecting every symlink.
        let slash = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")
            .map_err(|e| io("mount root", e))?;
        let mount = native::walk(
            &slash,
            &self.config.mount_root.as_os_str().as_bytes()[1..],
            false,
        )?;
        let parent = native::walk(&mount, self.config.run_parent.as_bytes(), false)?;
        let id = request.run_id.0.to_string();
        let expected = serde_json::to_vec(&(
            request.run_id,
            &request.immutable_base,
            request.policy.format_version,
        ))
        .map_err(json_error)?;
        if request.intent == OpenRunIntent::CreateNew {
            self.health
                .observe(native::mkdir(&parent, id.as_bytes(), 0o700))?;
        }
        let directory = native::walk(&parent, id.as_bytes(), false)?;
        if request.intent == OpenRunIntent::CreateNew {
            self.health.observe(native::mkdir(
                &directory,
                self.config.root_anchor.as_bytes(),
                0o700,
            ))?;
            self.health.observe(native::mkdir(
                &directory,
                self.config.control_anchor.as_bytes(),
                0o700,
            ))?;
            self.health
                .observe(native::mkdir(&directory, b".provider", 0o700))?;
            let private = native::walk(&directory, b".provider", false)?;
            self.health
                .observe(native::mkdir(&private, b"retries", 0o700))?;
            self.health
                .observe(create_file(&private, b"epoch", &0u64.to_le_bytes()))?;
            self.health
                .observe(create_file(&private, b"manifest", &expected))?;
            self.health.observe(native::sync(&directory))?;
            self.health.observe(native::sync(&parent))?;
        }
        let root = native::walk(&directory, self.config.root_anchor.as_bytes(), false)?;
        let control = native::walk(&directory, self.config.control_anchor.as_bytes(), false)?;
        let private = native::walk(&directory, b".provider", false)?;
        if read_file(&private, b"manifest")? != expected {
            return Err(error(
                ErrorKind::ProtocolMismatch,
                "open_run",
                "base/run/format mismatch",
            ));
        }
        // Compute the qualification answer up front, but do not record it until
        // every fallible step below has succeeded: with the assignment last, the
        // flag can no longer outlive a failed `open_run`. `capabilities()` still
        // reads the flag when building the `RunBinding` the caller is handed, so
        // the answer must be recorded before that binding is built -- hence the
        // assignment sits just above it, not at the end of `open_run`.
        //
        // A read-only run is never probed and never advertises: the probe is a
        // SETATTR, `SetMetadata` is a mutation this run would refuse anyway, and
        // qualifying a capability by performing the very mutation the policy
        // forbids would be the wrong way round.
        // The probe is a SETATTR a wedged export can stall on, so bound it: a
        // timeout answers `false` like any other probe failure. It probes an
        // isolated `<run_parent>/.umbra-probes/<uuid>/` directory rather than the
        // live `.provider` (#101), so it needs owned handles to the run parent (the
        // probe's own parent) and the run directory (whose device the dev guard
        // matches against). `try_clone` is a local `dup`, and a clone failure fails
        // closed. The container `mkdir` and the run-dir fstat both happen inside
        // `ownership_probe`, on the probe thread, never here -- so no new blocking
        // I/O lands on this main thread before `bounded`. The read-only short-circuit
        // stays in front, so a read-only run spawns nothing.
        let qualified = !request.policy.read_only
            && parent
                .try_clone()
                .ok()
                .zip(directory.try_clone().ok())
                .is_some_and(|(parent, run_dir)| {
                    bounded(PROBE_TIMEOUT, move || {
                        native::ownership_probe(&parent, &run_dir)
                    })
                });
        let physical = self
            .config
            .mount_root
            .join(OsStr::from_bytes(self.config.run_parent.as_bytes()))
            .join(id);
        // Named apart from the `root`/`control` walk handles above, which the
        // `Run` struct below consumes; these are the caller-facing bindings.
        let root_binding =
            binding(physical.join(OsStr::from_bytes(self.config.root_anchor.as_bytes())))?;
        let control_binding =
            binding(physical.join(OsStr::from_bytes(self.config.control_anchor.as_bytes())))?;
        // Every fallible step above has succeeded; only now record the answer.
        self.ownership_qualified = qualified;
        let result = RunBinding {
            run_id: request.run_id,
            root: root_binding,
            control: control_binding,
            capabilities: self.capabilities(),
        };
        self.run = Some(Run {
            request: request.clone(),
            directory,
            parent,
            mount,
            root,
            control,
            private,
            lease: None,
            pages: HashMap::new(),
        });
        Ok(result)
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
        self.health.check()?;
        let result = (|| {
            let run = self.run()?;
            let token = Uuid::new_v4().as_bytes().to_vec();
            // Exclusive NFS CREATE arbitrates across processes and clients. All later
            // failures retain the lock; elapsed time is never takeover proof.
            create_file(&run.private, b"writer.lock", &token)?;
            let bytes: [u8; 8] = read_file(&run.private, b"epoch")?
                .try_into()
                .map_err(|_| error(ErrorKind::CorruptJournal, "epoch", "invalid epoch"))?;
            let epoch = u64::from_le_bytes(bytes)
                .checked_add(1)
                .ok_or_else(|| error(ErrorKind::InvalidState, "epoch", "epoch exhausted"))?;
            replace_file(&run.private, b"epoch", &epoch.to_le_bytes())?;
            let lease = WriterLease {
                run_id: request.run_id,
                writer_id: request.writer_id.clone(),
                epoch: LeaseEpoch(epoch),
                renewal_token: token,
                renew_after_millis: LEASE_MILLIS,
            };
            self.run.as_mut().unwrap().lease = Some((
                lease.clone(),
                Instant::now() + Duration::from_millis(LEASE_MILLIS),
            ));
            Ok(lease)
        })();
        self.health.observe(result)
    }

    fn renew_writer(&mut self, lease: &WriterLease) -> Result<WriterLease> {
        self.check_lease(lease, true)?;
        self.run.as_mut().unwrap().lease.as_mut().unwrap().1 =
            Instant::now() + Duration::from_millis(LEASE_MILLIS);
        Ok(lease.clone())
    }
    fn release_writer(&mut self, lease: &WriterLease) -> Result<()> {
        self.check_lease(lease, false)?;
        self.flush_run(&mut || self.check_lease(lease, false))?;
        let result = (|| {
            native::unlink(&self.run()?.private, b"writer.lock", false)?;
            // Stop authority immediately, even if the following fsync fails.
            self.run.as_mut().unwrap().lease = None;
            native::sync(&self.run()?.private)
        })();
        self.health.observe(result)
    }
    fn execute(&mut self, request: &StorageRequest) -> Result<StorageResponse> {
        umbra_storage::validate_request(&self.capabilities(), request)?;
        let mutation = request.operation.is_mutation();
        self.context(&request.context, mutation)?;
        match &request.operation {
            StorageOperation::ReadAt { offset, len, .. }
                if offset
                    .checked_add(*len as u64)
                    .is_none_or(|n| n > i64::MAX as u64) =>
            {
                return Err(error(ErrorKind::InvalidInput, "pread", "off_t overflow"))
            }
            StorageOperation::WriteAt { offset, bytes, .. }
                if offset
                    .checked_add(bytes.len() as u64)
                    .is_none_or(|n| n > i64::MAX as u64) =>
            {
                return Err(error(ErrorKind::InvalidInput, "pwrite", "off_t overflow"))
            }
            _ => (),
        }
        if !mutation {
            return self.dispatch(&request.operation);
        }
        let result = (|| {
            let journal = native::walk(&self.run()?.private, b"retries", false)?;
            let key = format!(
                "key-{}",
                request
                    .context
                    .idempotency_key
                    .0
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            type Record = (StorageRequest, Option<Result<StorageResponse>>);
            match read_file(&journal, key.as_bytes()) {
                Ok(bytes) => {
                    let (previous, result): Record =
                        serde_json::from_slice(&bytes).map_err(json_error)?;
                    if previous != *request {
                        return Err(error(
                            ErrorKind::InvalidInput,
                            "retry",
                            "conflicting idempotency key",
                        ));
                    }
                    return result.unwrap_or_else(|| {
                        Err(error(
                            ErrorKind::StorageUnavailable,
                            "retry",
                            "indeterminate operation requires reconciliation",
                        ))
                    });
                }
                Err(e) if e.kind == ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            let op = format!("op-{}", request.context.operation_id.0);
            match read_file(&journal, op.as_bytes()) {
                Ok(_) => {
                    return Err(error(
                        ErrorKind::InvalidInput,
                        "retry",
                        "operation ID already used",
                    ))
                }
                Err(e) if e.kind == ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            create_file(&journal, op.as_bytes(), key.as_bytes())?;
            let intent: Record = (request.clone(), None);
            create_file(
                &journal,
                key.as_bytes(),
                &serde_json::to_vec(&intent).map_err(json_error)?,
            )?;
            self.run.as_mut().unwrap().pages.clear();
            let result = self.dispatch(&request.operation);
            let result = self.health.observe(result);
            self.health.check()?;
            let record: Record = (request.clone(), Some(result.clone()));
            replace_file(
                &journal,
                key.as_bytes(),
                &serde_json::to_vec(&record).map_err(json_error)?,
            )?;
            result
        })();
        self.health.observe(result)
    }

    fn flush(&mut self, request: &FlushRequest) -> Result<DurabilityReceipt> {
        if request.scope != FlushScope::EntireRun {
            return Err(unsupported("object-scoped flush"));
        }
        self.context(&request.context, true)?;
        self.flush_run(&mut || self.context(&request.context, true))?;
        Ok(DurabilityReceipt {
            run_id: request.context.run_id,
            writer_epoch: request.context.writer_epoch.unwrap(),
            scope: request.scope.clone(),
            durability: Durability::Local,
            evidence: native::LOCAL_FLUSH_EVIDENCE.to_vec(),
        })
    }
    fn close_run(&mut self) -> Result<()> {
        self.health.check()?;
        if self.run()?.lease.is_some() {
            return Err(error(
                ErrorKind::InvalidState,
                "close_run",
                "release writer first",
            ));
        }
        if !self.run()?.request.policy.read_only {
            self.flush_run(&mut || Ok(()))?;
        }
        self.run = None;
        // The qualification was against this run's store; the next one has to
        // earn it again. `validated` is deliberately not reset beside it -- that
        // one is about the mount, which outlives the run.
        self.ownership_qualified = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
