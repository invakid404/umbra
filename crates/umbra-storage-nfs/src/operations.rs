use super::*;
use std::os::unix::fs::FileExt;

impl NfsStorage {
    pub(super) fn dispatch(&mut self, op: &StorageOperation) -> Result<StorageResponse> {
        use StorageOperation as O;
        match op {
            O::Stat { path } => Ok(StorageResponse::Stat(self.stat_path(path)?)),
            O::Lookup { path } => Ok(StorageResponse::Lookup(object(self.stat_path(path)?))),
            O::ReadAt { path, offset, len } => {
                let (dir, name) = self.parent(path, false)?;
                let file = native::regular(&dir, &name, libc::O_RDONLY)?;
                let mut bytes = vec![0; *len as usize];
                let count = file
                    .read_at(&mut bytes, *offset)
                    .map_err(|e| io("pread", e))?;
                bytes.truncate(count);
                Ok(StorageResponse::ReadAt(bytes))
            }
            O::WriteAt {
                path,
                offset,
                bytes,
            } => {
                let (dir, name) = self.parent(path, false)?;
                let file = native::regular(&dir, &name, libc::O_WRONLY)?;
                let count = file.write_at(bytes, *offset).map_err(|e| io("pwrite", e))?;
                native::sync(&file)?;
                Ok(StorageResponse::WriteAt(count as u32))
            }
            O::Create { path, options } => {
                if matches!(options.kind, CreateKind::LogicalSymlink { .. }) {
                    return Err(unsupported("logical symlink create"));
                }
                if options.mode & !0o7777 != 0 {
                    return Err(error(ErrorKind::InvalidInput, "create", "invalid mode"));
                }
                let (dir, name) = self.parent(path, true)?;
                match options.kind {
                    CreateKind::File => native::sync(&native::open(
                        &dir,
                        &name,
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        options.mode,
                    )?)?,
                    CreateKind::Directory => native::mkdir(&dir, &name, options.mode)?,
                    _ => unreachable!(),
                }
                native::sync(&dir)?;
                Ok(StorageResponse::Created(object(native::stat(&dir, &name)?)))
            }
            O::CreateParents { path, mode } => {
                if *mode != 0o700 {
                    return Err(unsupported("parent mode other than 0700"));
                }
                native::walk(self.anchor(path)?, path.as_bytes(), true)?;
                Ok(StorageResponse::ParentsCreated)
            }
            O::Unlink { path } | O::RemoveDirectory { path } => {
                let directory = matches!(op, O::RemoveDirectory { .. });
                let (dir, name) = self.parent(path, false)?;
                native::unlink(&dir, &name, directory)?;
                native::sync(&dir)?;
                Ok(if directory {
                    StorageResponse::DirectoryRemoved
                } else {
                    StorageResponse::Unlinked
                })
            }
            O::Rename {
                source,
                destination,
                mode,
            } => {
                if source.anchor() != destination.anchor() {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        "rename",
                        "cross-anchor rename",
                    ));
                }
                let (from, a) = self.parent(source, false)?;
                let (to, b) = self.parent(destination, false)?;
                native::rename(&from, &a, &to, &b, false, *mode == RenameMode::NoReplace)?;
                native::sync(&from)?;
                native::sync(&to)?;
                Ok(StorageResponse::Renamed(object(native::stat(&to, &b)?)))
            }
            O::AtomicSwap { left, right } => {
                if left.anchor() != right.anchor() {
                    return Err(error(ErrorKind::InvalidPath, "swap", "cross-anchor swap"));
                }
                let (from, a) = self.parent(left, false)?;
                let (to, b) = self.parent(right, false)?;
                native::rename(&from, &a, &to, &b, true, false)?;
                native::sync(&from)?;
                native::sync(&to)?;
                Ok(StorageResponse::Swapped)
            }
            O::List {
                path,
                cursor,
                limit,
            } => {
                let mut page = if let Some(token) = cursor {
                    let pages = &mut self.run.as_mut().unwrap().pages;
                    if pages.get(&token.0).is_none_or(|p| &p.path != path) {
                        return Err(error(
                            ErrorKind::StaleHandle,
                            "list",
                            "unknown or foreign cursor",
                        ));
                    }
                    pages.remove(&token.0).unwrap()
                } else {
                    if self.run()?.pages.len() >= 64 {
                        return Err(error(
                            ErrorKind::InvalidState,
                            "list",
                            "too many open cursors",
                        ));
                    }
                    let dir = native::walk(self.anchor(path)?, path.as_bytes(), false)?;
                    Page {
                        path: path.clone(),
                        entries: native::Entries::new(&dir)?,
                        stamp: native::stamp(&dir)?,
                        directory: dir,
                    }
                };
                if page.stamp != native::stamp(&page.directory)? {
                    return Err(error(ErrorKind::StaleHandle, "list", "directory changed"));
                }
                let mut entries = Vec::new();
                let mut eof = false;
                while entries.len() < *limit as usize {
                    let Some(name) = page.entries.next()? else {
                        eof = true;
                        break;
                    };
                    entries.push(DirectoryEntry {
                        stat: native::stat(&page.directory, &name)?,
                        name: BytePath::new(name)?,
                    });
                }
                if page.stamp != native::stamp(&page.directory)? {
                    return Err(error(ErrorKind::StaleHandle, "list", "directory changed"));
                }
                let next = if eof {
                    None
                } else {
                    let token = Uuid::new_v4().as_bytes().to_vec();
                    self.run.as_mut().unwrap().pages.insert(token.clone(), page);
                    Some(ListCursor(token))
                };
                Ok(StorageResponse::List(DirectoryPage { entries, next }))
            }
            _ => Err(unsupported("storage operation")),
        }
    }
}
