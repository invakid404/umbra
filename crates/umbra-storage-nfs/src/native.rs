//! Descriptor-relative Unix syscall boundary.
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use umbra_core::*;
use uuid::Uuid;

pub(crate) fn error(kind: ErrorKind, op: &str, message: &str) -> UmbraError {
    UmbraError::new(kind, op, message)
}
pub(crate) fn io(op: &str, e: std::io::Error) -> UmbraError {
    let kind = match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => ErrorKind::InvalidPath,
        Some(libc::ENOTSUP | libc::ENOSYS) => ErrorKind::UnsupportedCapability,
        _ => match e.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
            std::io::ErrorKind::PermissionDenied => ErrorKind::Denied,
            std::io::ErrorKind::InvalidInput => ErrorKind::InvalidInput,
            _ => ErrorKind::Io,
        },
    };
    let mut err = error(kind, op, &e.to_string());
    err.errno = e.raw_os_error().map(Errno);
    err
}
pub(crate) fn unsupported(op: &str) -> UmbraError {
    error(
        ErrorKind::UnsupportedCapability,
        op,
        "not qualified by mounted NFS storage",
    )
}
fn cvt(value: libc::c_int, op: &str) -> Result<libc::c_int> {
    if value < 0 {
        Err(io(op, std::io::Error::last_os_error()))
    } else {
        Ok(value)
    }
}
fn name(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|_| error(ErrorKind::InvalidPath, "path", "NUL byte"))
}
pub(crate) fn open(dir: &File, bytes: &[u8], flags: i32, mode: u32) -> Result<File> {
    let name = name(bytes)?;
    // SAFETY: live directory FD, NUL-terminated name, mode supplied for O_CREAT.
    let fd = cvt(
        unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                mode as libc::c_uint,
            )
        },
        "openat",
    )?;
    // SAFETY: successful openat transfers a new, uniquely owned FD.
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub(crate) fn mkdir(dir: &File, bytes: &[u8], mode: u32) -> Result<()> {
    let name = name(bytes)?;
    // SAFETY: valid directory FD and C string.
    cvt(
        unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) },
        "mkdirat",
    )?;
    Ok(())
}
pub(crate) fn walk(dir: &File, bytes: &[u8], create: bool) -> Result<File> {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec())?;
    let mut current = open(dir, b".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    for component in bytes.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        if create {
            match mkdir(&current, component, 0o700) {
                Ok(()) => sync(&current)?,
                Err(e) if e.kind == ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
        }
        current = open(&current, component, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok(current)
}
pub(crate) fn parent(dir: &File, bytes: &[u8], create: bool) -> Result<(File, Vec<u8>)> {
    StoragePath::new(StorageAnchor::Root, bytes.to_vec())?;
    if bytes.is_empty() {
        return Err(error(
            ErrorKind::InvalidPath,
            "path",
            "cannot mutate anchor",
        ));
    }
    let split = bytes.iter().rposition(|b| *b == b'/');
    let (parents, leaf) = split.map_or((&b""[..], bytes), |i| (&bytes[..i], &bytes[i + 1..]));
    Ok((walk(dir, parents, create)?, leaf.to_vec()))
}
pub(crate) fn unlink(dir: &File, bytes: &[u8], directory: bool) -> Result<()> {
    let name = name(bytes)?;
    // SAFETY: valid FD and C string; unlinkat never follows the final symlink.
    cvt(
        unsafe {
            libc::unlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        },
        "unlinkat",
    )?;
    Ok(())
}
pub(crate) fn rename(
    from: &File,
    a: &[u8],
    to: &File,
    b: &[u8],
    exchange: bool,
    exclusive: bool,
) -> Result<()> {
    let a = name(a)?;
    let b = name(b)?;
    // SAFETY: live FDs and NUL-terminated names throughout these calls.
    let result = unsafe {
        if !exchange && !exclusive {
            libc::renameat(from.as_raw_fd(), a.as_ptr(), to.as_raw_fd(), b.as_ptr())
        } else {
            #[cfg(target_os = "macos")]
            {
                libc::renameatx_np(
                    from.as_raw_fd(),
                    a.as_ptr(),
                    to.as_raw_fd(),
                    b.as_ptr(),
                    if exchange {
                        libc::RENAME_SWAP
                    } else {
                        libc::RENAME_EXCL
                    },
                )
            }
            #[cfg(target_os = "linux")]
            {
                libc::syscall(
                    libc::SYS_renameat2,
                    from.as_raw_fd(),
                    a.as_ptr(),
                    to.as_raw_fd(),
                    b.as_ptr(),
                    if exchange {
                        libc::RENAME_EXCHANGE
                    } else {
                        libc::RENAME_NOREPLACE
                    },
                ) as i32
            }
        }
    };
    cvt(result, "renameat")?;
    Ok(())
}
// `st_nlink` and `st_mode` differ in width between Darwin (u16) and Linux
// (nlink_t = u64, mode_t = u32). Explicit `as` widens on Darwin and is a
// no-op on Linux; `#[allow(clippy::unnecessary_cast)]` keeps the same code
// path clean on both targets without a per-platform helper.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn stat(dir: &File, bytes: &[u8]) -> Result<BlobStat> {
    let name = name(if bytes.is_empty() { b"." } else { bytes })?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: output points to enough writable storage; read only after success.
    cvt(
        unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        "fstatat",
    )?;
    // SAFETY: fstatat initialized the structure on success.
    let st = unsafe { st.assume_init() };
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => ObjectKind::File,
        libc::S_IFDIR => ObjectKind::Directory,
        libc::S_IFLNK => ObjectKind::LogicalSymlink,
        _ => return Err(unsupported("special file")),
    };
    Ok(BlobStat {
        object_id: ObjectId(Uuid::from_u128(
            ((st.st_dev as u128) << 64) | st.st_ino as u128,
        )),
        kind,
        len: st.st_size as u64,
        link_count: st.st_nlink as u64,
        mode: (st.st_mode & 0o7777) as u32,
        uid: st.st_uid,
        gid: st.st_gid,
        modified_nanos: st.st_mtime as i128 * 1_000_000_000 + st.st_mtime_nsec as i128,
    })
}
pub(crate) fn regular(dir: &File, bytes: &[u8], flags: i32) -> Result<File> {
    let file = open(dir, bytes, flags, 0)?;
    if !file.metadata().map_err(|e| io("fstat", e))?.is_file() {
        return Err(unsupported("not regular file"));
    }
    Ok(file)
}
pub(crate) fn sync(file: &File) -> Result<()> {
    file.sync_all().map_err(|e| io("fsync", e))
}

/// libc readdir uses Darwin getdirentries64; legacy getdirentries cannot handle
/// arm64 inode layouts. Only libc's bounded native buffer is retained.
#[derive(Debug)]
pub(crate) struct Entries(*mut libc::DIR);
// SAFETY: exclusively owned DIR, accessed through &mut self; no shared access.
unsafe impl Send for Entries {}
impl Entries {
    pub(crate) fn new(dir: &File) -> Result<Self> {
        use std::os::fd::IntoRawFd;
        let fd = open(dir, b".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?.into_raw_fd();
        // SAFETY: newly owned directory FD; fdopendir owns it on success only.
        let ptr = unsafe { libc::fdopendir(fd) };
        if ptr.is_null() {
            let err = io("fdopendir", std::io::Error::last_os_error());
            // SAFETY: fdopendir failed, leaving ownership here.
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        Ok(Self(ptr))
    }
    pub(crate) fn next(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            // SAFETY: exclusive live DIR; errno is thread-local; copy the name
            // before the next readdir invalidates its allocation.
            unsafe {
                #[cfg(target_os = "macos")]
                let errno = libc::__error();
                #[cfg(target_os = "linux")]
                let errno = libc::__errno_location();
                *errno = 0;
                let entry = libc::readdir(self.0);
                if entry.is_null() {
                    return if *errno == 0 {
                        Ok(None)
                    } else {
                        Err(io("getdirentries", std::io::Error::last_os_error()))
                    };
                }
                if (*entry).d_ino == 0 {
                    continue;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
                if name != b"." && name != b".." {
                    return Ok(Some(name.to_vec()));
                }
            }
        }
    }
}
impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: sole owner, closed exactly once.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
pub(crate) fn sync_tree(dir: &File) -> Result<()> {
    let mut entries = Entries::new(dir)?;
    while let Some(name) = entries.next()? {
        match stat(dir, &name)?.kind {
            ObjectKind::File => sync(&regular(dir, &name, libc::O_RDONLY)?)?,
            ObjectKind::Directory => {
                sync_tree(&open(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?)?
            }
            ObjectKind::LogicalSymlink => (),
        }
    }
    sync(dir)
}
pub(crate) fn stamp(dir: &File) -> Result<(u64, i64, i64, i64, i64)> {
    let m = dir.metadata().map_err(|e| io("fstat", e))?;
    Ok((
        m.ino(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
