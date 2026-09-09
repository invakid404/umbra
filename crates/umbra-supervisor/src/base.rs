//! Read-only immutable base for one run, plus the approved workspace inventory.
//!
//! The overlay resolves reads that have no shadow entry against a [`Base`]. This
//! module supplies that base as a **frozen read-only view of the host
//! filesystem**: bounded `std::fs` reads, no writes, no symlink rewriting, and a
//! physical mapping that hands the kernel back the very path it was already
//! opening. Freezing is enforced by the installed sandbox — a tracee may write
//! only inside its own run root — not by copying the host.
//!
//! Separately, the *approved workspace* is inventoried before the run opens and
//! re-inventoried immediately before launch. That inventory supplies the run's
//! [`ImmutableBaseContract`] identity and fails the run if the workspace changed
//! underneath preparation. It deliberately does **not** snapshot content: this
//! detects change, it does not preserve the pre-run bytes. A content-addressed
//! snapshot under `control/base` remains unimplemented.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use umbra_core::{
    BlobStat, BytePath, DirectoryEntry, DirectoryPage, ErrorKind, ImmutableBaseContract,
    ListCursor, ObjectId, ObjectKind, PhysicalPath, Result, StorageAnchor, StoragePath, UmbraError,
    MAX_DIRECTORY_ENTRIES, MAX_IO_BYTES,
};
use umbra_overlay::Base;
use uuid::Uuid;

/// Largest approved workspace inventory accepted before a run starts.
pub const MAX_INVENTORY_ENTRIES: usize = 4096;
/// Deepest directory nesting walked while inventorying the approved workspace.
pub const MAX_INVENTORY_DEPTH: usize = 32;
/// Identity scheme recorded in [`ImmutableBaseContract::identity`].
pub const INVENTORY_SCHEME: &str = "workspace-inventory-v1";

fn error(kind: ErrorKind, operation: &str, context: impl Into<String>) -> UmbraError {
    UmbraError::new(kind, operation, context)
}

fn io_error(operation: &str, path: &Path, e: std::io::Error) -> UmbraError {
    let mut error = UmbraError::new(ErrorKind::Io, operation, format!("{}: {e}", path.display()));
    if let Some(code) = e.raw_os_error() {
        error = error.with_errno(umbra_core::Errno(code));
    }
    error
}

/// One inventoried workspace entry. Content is identified by size and modified
/// time only; this is change detection, not a content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryEntry {
    /// Workspace-relative byte path; the workspace root itself is empty.
    pub relative: Vec<u8>,
    /// Whether this entry is a directory.
    pub directory: bool,
    /// Length in bytes for files; zero for directories.
    pub len: u64,
    /// Permission bits.
    pub mode: u32,
    /// Modified time in nanoseconds.
    pub modified_nanos: i128,
}

/// Bounded, ordered inventory of the approved workspace at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceInventory {
    root: PathBuf,
    entries: Vec<InventoryEntry>,
}

impl WorkspaceInventory {
    /// Walk `root` with safe filesystem operations, refusing anything that cannot
    /// be represented exactly: symlinks, sockets, devices, and other non-regular
    /// entries, unreadable directories, and inventories past the bounds above.
    /// Nothing here follows a link out of the workspace, because links are not
    /// followed at all.
    pub fn capture(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err(error(
                ErrorKind::InvalidPath,
                "run.workspace",
                "workspace must be an absolute path",
            ));
        }
        let mut entries = Vec::new();
        walk(root, Vec::new(), 0, &mut entries)?;
        entries.sort_by(|a, b| a.relative.cmp(&b.relative));
        Ok(Self {
            root: root.to_path_buf(),
            entries,
        })
    }

    /// Inventoried entries in stable order.
    pub fn entries(&self) -> &[InventoryEntry] {
        &self.entries
    }

    /// Logical base identity for this run. It contains a digest of the inventory
    /// and of the workspace path, never a storage mount root.
    pub fn contract(&self) -> ImmutableBaseContract {
        let digest = self.digest();
        ImmutableBaseContract {
            identity: format!("{INVENTORY_SCHEME}:{digest:032x}"),
            fingerprint: digest.to_be_bytes().to_vec(),
        }
    }

    /// Re-inventory the same workspace and fail unless it is byte-for-byte the
    /// inventory captured earlier. Called immediately before launch so a run
    /// never starts against a workspace that moved underneath preparation.
    pub fn verify_unchanged(&self) -> Result<()> {
        let current = Self::capture(&self.root)?;
        if current != *self {
            return Err(error(
                ErrorKind::InvalidState,
                "run.workspace",
                "approved workspace changed during preparation; rerun the command",
            ));
        }
        Ok(())
    }

    /// A 128-bit FNV-1a digest over the canonical inventory encoding.
    ///
    /// This is a change detector for one host, not a cryptographic content
    /// identity and not a cross-host compatibility fingerprint.
    fn digest(&self) -> u128 {
        const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
        const PRIME: u128 = 0x0000000001000000000000000000013b;
        let mut hash = OFFSET;
        let mut absorb = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= *byte as u128;
                hash = hash.wrapping_mul(PRIME);
            }
        };
        absorb(INVENTORY_SCHEME.as_bytes());
        absorb(self.root.as_os_str().as_bytes());
        for entry in &self.entries {
            absorb(&entry.relative);
            absorb(&[entry.directory as u8]);
            absorb(&entry.len.to_be_bytes());
            absorb(&entry.mode.to_be_bytes());
            absorb(&entry.modified_nanos.to_be_bytes());
        }
        hash
    }
}

fn walk(root: &Path, relative: Vec<u8>, depth: usize, out: &mut Vec<InventoryEntry>) -> Result<()> {
    if depth > MAX_INVENTORY_DEPTH {
        return Err(error(
            ErrorKind::InvalidInput,
            "run.workspace",
            format!("workspace nesting exceeds {MAX_INVENTORY_DEPTH} levels"),
        ));
    }
    let path = join_relative(root, &relative);
    let metadata = fs::symlink_metadata(&path).map_err(|e| io_error("run.workspace", &path, e))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.workspace",
            format!(
                "{}: symbolic links in the approved workspace are not supported",
                path.display()
            ),
        ));
    }
    if !file_type.is_dir() && !file_type.is_file() {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "run.workspace",
            format!(
                "{}: only regular files and directories are supported",
                path.display()
            ),
        ));
    }
    if out.len() == MAX_INVENTORY_ENTRIES {
        return Err(error(
            ErrorKind::InvalidInput,
            "run.workspace",
            format!("approved workspace exceeds {MAX_INVENTORY_ENTRIES} entries"),
        ));
    }
    out.push(InventoryEntry {
        relative: relative.clone(),
        directory: file_type.is_dir(),
        len: if file_type.is_dir() {
            0
        } else {
            metadata.len()
        },
        mode: metadata.mode() & 0o7777,
        modified_nanos: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
    });
    if !file_type.is_dir() {
        return Ok(());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&path).map_err(|e| io_error("run.workspace", &path, e))? {
        let entry = entry.map_err(|e| io_error("run.workspace", &path, e))?;
        names.push(entry.file_name().as_bytes().to_vec());
    }
    names.sort();
    for name in names {
        let mut child = relative.clone();
        if !child.is_empty() {
            child.push(b'/');
        }
        child.extend_from_slice(&name);
        walk(root, child, depth + 1, out)?;
    }
    Ok(())
}

fn join_relative(root: &Path, relative: &[u8]) -> PathBuf {
    if relative.is_empty() {
        return root.to_path_buf();
    }
    root.join(std::ffi::OsStr::from_bytes(relative))
}

/// Frozen read-only view of the host filesystem used as the run's base layer.
///
/// Logical paths arrive anchored at the storage root, whose bytes are the
/// absolute logical path without its leading separator; this maps them straight
/// back to host paths. Every method is read-only and bounded. Enforcement that
/// the host cannot change during the run comes from the installed sandbox.
pub struct HostReadOnlyBase {
    inventory: WorkspaceInventory,
}

impl HostReadOnlyBase {
    /// Wrap an already-captured approved workspace inventory. No I/O occurs here.
    pub fn new(inventory: WorkspaceInventory) -> Self {
        Self { inventory }
    }

    /// The inventory this base was frozen against.
    pub fn inventory(&self) -> &WorkspaceInventory {
        &self.inventory
    }

    fn host_path(&self, path: &StoragePath) -> Result<PathBuf> {
        if path.anchor() != StorageAnchor::Root {
            return Err(error(
                ErrorKind::InvalidPath,
                "base.path",
                "the immutable base has no control namespace",
            ));
        }
        let mut bytes = vec![b'/'];
        bytes.extend_from_slice(path.as_bytes());
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }
}

fn blob_stat(metadata: &fs::Metadata) -> Result<BlobStat> {
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        ObjectKind::Directory
    } else if file_type.is_file() {
        ObjectKind::File
    } else if file_type.is_symlink() {
        ObjectKind::LogicalSymlink
    } else {
        return Err(error(
            ErrorKind::UnsupportedCapability,
            "base.stat",
            "special host entries (devices, sockets, FIFOs) are unsupported",
        ));
    };
    Ok(BlobStat {
        // Stable within this host: hard links share (device, inode) and so share
        // one object identity, as the storage contract requires.
        object_id: ObjectId(Uuid::from_bytes(host_object_bytes(metadata))),
        kind,
        len: metadata.len(),
        link_count: metadata.nlink(),
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        modified_nanos: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
    })
}

fn host_object_bytes(metadata: &fs::Metadata) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&metadata.dev().to_be_bytes());
    bytes[8..].copy_from_slice(&metadata.ino().to_be_bytes());
    bytes
}

fn not_found(operation: &str, path: &Path, e: std::io::Error) -> UmbraError {
    if e.kind() == std::io::ErrorKind::NotFound {
        return error(
            ErrorKind::NotFound,
            operation,
            format!("{}: not present in the immutable base", path.display()),
        );
    }
    io_error(operation, path, e)
}

impl Base for HostReadOnlyBase {
    fn stat(&mut self, path: &StoragePath) -> Result<BlobStat> {
        let host = self.host_path(path)?;
        let metadata = fs::symlink_metadata(&host).map_err(|e| not_found("base.stat", &host, e))?;
        blob_stat(&metadata)
    }

    fn read_link(&mut self, path: &StoragePath) -> Result<BytePath> {
        let host = self.host_path(path)?;
        let target = fs::read_link(&host).map_err(|e| not_found("base.read_link", &host, e))?;
        BytePath::new(target.as_os_str().as_bytes().to_vec())
    }

    fn read_at(&mut self, path: &StoragePath, offset: u64, out: &mut [u8]) -> Result<usize> {
        if out.len() > MAX_IO_BYTES || offset.checked_add(out.len() as u64).is_none() {
            return Err(error(
                ErrorKind::InvalidInput,
                "base.read_at",
                "read exceeds the contract bounds",
            ));
        }
        let host = self.host_path(path)?;
        let mut file = fs::File::open(&host).map_err(|e| not_found("base.read_at", &host, e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| io_error("base.read_at", &host, e))?;
        let mut read = 0;
        while read < out.len() {
            match file.read(&mut out[read..]) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error("base.read_at", &host, e)),
            }
        }
        Ok(read)
    }

    fn list(
        &mut self,
        path: &StoragePath,
        cursor: Option<&ListCursor>,
        limit: u32,
    ) -> Result<DirectoryPage> {
        if limit == 0 || limit > MAX_DIRECTORY_ENTRIES {
            return Err(error(
                ErrorKind::InvalidInput,
                "base.list",
                "invalid page limit",
            ));
        }
        let start = match cursor {
            None => 0usize,
            Some(ListCursor(bytes)) => {
                let bytes: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                    error(
                        ErrorKind::InvalidInput,
                        "base.list",
                        "cursor does not belong to this base session",
                    )
                })?;
                u64::from_be_bytes(bytes) as usize
            }
        };
        let host = self.host_path(path)?;
        // Read the whole directory and page over a stable ordering: a host
        // directory offset is not a durable cursor across mutations.
        let host = fs::canonicalize(&host).map_err(|e| not_found("base.list", &host, e))?;
        if host.starts_with("/dev") {
            return Err(error(
                ErrorKind::UnsupportedCapability,
                "base.list",
                "listing the host device namespace is unsupported",
            ));
        }
        let mut names = BTreeMap::new();
        for entry in fs::read_dir(&host).map_err(|e| not_found("base.list", &host, e))? {
            let entry = entry.map_err(|e| io_error("base.list", &host, e))?;
            names.insert(entry.file_name().as_bytes().to_vec(), entry.path());
        }
        let mut entries = Vec::new();
        let mut index = start;
        for (name, child) in names.iter().skip(start) {
            if entries.len() == limit as usize {
                break;
            }
            let metadata =
                fs::symlink_metadata(child).map_err(|e| io_error("base.list", child, e))?;
            // The cursor counts all names, including unsupported entries, so
            // a page containing only special files still makes progress.
            index += 1;
            let stat = match blob_stat(&metadata) {
                Ok(stat) => stat,
                Err(e) if e.kind == ErrorKind::UnsupportedCapability => continue,
                Err(e) => return Err(e),
            };
            entries.push(DirectoryEntry {
                name: BytePath::new(name.clone())?,
                stat,
            });
        }
        let next = (index < names.len()).then(|| ListCursor((index as u64).to_be_bytes().to_vec()));
        Ok(DirectoryPage { entries, next })
    }

    fn physical_path(&self, path: &StoragePath) -> Result<PhysicalPath> {
        let host = self.host_path(path)?;
        Ok(PhysicalPath(BytePath::new(
            host.as_os_str().as_bytes().to_vec(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_path(path: &Path) -> StoragePath {
        StoragePath::new(
            StorageAnchor::Root,
            path.strip_prefix("/")
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn listing_skips_special_entries_across_pages_and_refuses_device_aliases() {
        let scratch = tempfile::tempdir().unwrap();
        let mut base = HostReadOnlyBase::new(WorkspaceInventory::capture(scratch.path()).unwrap());
        let mut sockets = Vec::new();
        for name in ["a-socket", "c-socket", "e-socket"] {
            sockets
                .push(std::os::unix::net::UnixListener::bind(scratch.path().join(name)).unwrap());
        }
        for name in ["b-file", "d-file"] {
            fs::write(scratch.path().join(name), b"regular").unwrap();
        }
        let path = storage_path(scratch.path());
        let mut cursor = None;
        let mut names = Vec::new();
        for page_number in 0..4 {
            let page = base.list(&path, cursor.as_ref(), 1).unwrap();
            names.extend(page.entries.into_iter().map(|e| e.name.as_bytes().to_vec()));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
            assert!(
                page_number < 3,
                "paging must terminate despite skipped entries"
            );
        }
        assert_eq!(names, [b"b-file".to_vec(), b"d-file".to_vec()]);
        let alias = scratch.path().join("devices");
        std::os::unix::fs::symlink("/dev", &alias).unwrap();
        for path in [Path::new("/dev"), alias.as_path()] {
            assert_eq!(
                base.list(&storage_path(path), None, 1).unwrap_err().kind,
                ErrorKind::UnsupportedCapability
            );
        }
    }

    #[test]
    fn special_host_entries_are_not_reported_as_symlinks() {
        assert_eq!(
            blob_stat(&fs::symlink_metadata("/dev/null").unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedCapability
        );
        let scratch = tempfile::tempdir().unwrap();
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        assert_eq!(
            blob_stat(&fs::symlink_metadata(link).unwrap())
                .unwrap()
                .kind,
            ObjectKind::LogicalSymlink
        );
    }

    #[test]
    fn inventory_refuses_symlinks_and_detects_change() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/file"), b"one").unwrap();
        let inventory = WorkspaceInventory::capture(dir).unwrap();
        assert_eq!(inventory.entries().len(), 3);
        inventory.verify_unchanged().unwrap();

        fs::write(dir.join("nested/file"), b"two bytes longer").unwrap();
        assert_eq!(
            inventory.verify_unchanged().unwrap_err().kind,
            ErrorKind::InvalidState
        );

        std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();
        assert_eq!(
            WorkspaceInventory::capture(dir).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
    }

    #[test]
    fn base_paths_map_logical_roots_to_host_paths_without_a_control_namespace() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        fs::create_dir_all(dir).unwrap();
        let base = HostReadOnlyBase::new(WorkspaceInventory::capture(dir).unwrap());
        let path = StoragePath::new(StorageAnchor::Root, b"etc/hosts".to_vec()).unwrap();
        assert_eq!(
            base.physical_path(&path).unwrap().0.as_bytes(),
            b"/etc/hosts"
        );
        let control = StoragePath::new(StorageAnchor::Control, b"journal".to_vec()).unwrap();
        assert_eq!(
            base.physical_path(&control).unwrap_err().kind,
            ErrorKind::InvalidPath
        );
    }
}
