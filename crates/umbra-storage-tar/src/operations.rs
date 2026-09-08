use super::*;
use archive::{Content, Node, State};

pub(crate) fn cross_anchor(op: &StorageOperation) -> Result<()> {
    let pair = match op {
        StorageOperation::Rename {
            source,
            destination,
            ..
        }
        | StorageOperation::Link {
            source,
            destination,
        } => Some((source, destination)),
        StorageOperation::AtomicSwap { left, right } => Some((left, right)),
        _ => None,
    };
    if pair.is_some_and(|(a, b)| a.anchor() != b.anchor()) {
        return Err(error(ErrorKind::InvalidPath, "cross-anchor operation"));
    }
    Ok(())
}
fn parent(path: &StoragePath) -> Result<StoragePath> {
    let end = path
        .as_bytes()
        .iter()
        .rposition(|b| *b == b'/')
        .unwrap_or(0);
    StoragePath::new(path.anchor(), path.as_bytes()[..end].to_vec())
}
pub(crate) fn validate_parents(state: &State, path: &StoragePath) -> Result<()> {
    let mut p = path.clone();
    while !p.as_bytes().is_empty() {
        p = parent(&p)?;
        match state.nodes.get(&p) {
            Some(n) if n.stat.kind == ObjectKind::Directory => (),
            Some(_) => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "nondirectory or symlink component",
                ))
            }
            None => return Err(error(ErrorKind::NotFound, "parent directory missing")),
        }
    }
    Ok(())
}
fn node<'a>(state: &'a State, path: &StoragePath) -> Result<&'a Node> {
    validate_parents(state, path)?;
    state
        .nodes
        .get(path)
        .ok_or_else(|| error(ErrorKind::NotFound, "name not found"))
}
fn named(path: &StoragePath) -> Result<()> {
    if path.as_bytes().is_empty() {
        return Err(error(ErrorKind::InvalidPath, "cannot mutate anchor name"));
    }
    Ok(())
}
fn object(node: &Node) -> ObjectResult {
    ObjectResult {
        stat: node.stat.clone(),
        rewrite_target: None,
    }
}
fn descendants<'a>(
    state: &'a State,
    path: &'a StoragePath,
) -> impl Iterator<Item = &'a StoragePath> {
    state.nodes.keys().filter(move |p| {
        p.anchor() == path.anchor()
            && p.as_bytes()
                .strip_prefix(path.as_bytes())
                .is_some_and(|suffix| suffix.starts_with(b"/"))
    })
}
fn parents(state: &mut State, path: &StoragePath, mode: u32) -> Result<()> {
    let bytes = path.as_bytes();
    for end in bytes
        .iter()
        .enumerate()
        .filter_map(|(i, b)| (*b == b'/').then_some(i))
    {
        let p = StoragePath::new(path.anchor(), bytes[..end].to_vec())?;
        if let Some(n) = state.nodes.get(&p) {
            if n.stat.kind != ObjectKind::Directory {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "symlink or nondirectory parent",
                ));
            }
        } else {
            state.nodes.insert(
                p,
                archive::new_node(&CreateOptions {
                    kind: CreateKind::Directory,
                    mode,
                }),
            );
        }
    }
    validate_parents(state, path)
}
fn io_bounds(offset: u64, len: usize) -> Result<()> {
    if offset
        .checked_add(len as u64)
        .is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(error(ErrorKind::InvalidInput, "file offset overflow"));
    }
    Ok(())
}
pub(crate) fn mutate(state: &mut State, op: &StorageOperation) -> Result<StorageResponse> {
    match op {
        StorageOperation::Create { path, options } => {
            named(path)?;
            if options.mode & !0o7777 != 0 {
                return Err(error(ErrorKind::InvalidInput, "invalid mode"));
            }
            validate_parents(state, path)?;
            if state.nodes.contains_key(path) {
                return Err(error(ErrorKind::AlreadyExists, "name exists"));
            }
            let n = archive::new_node(options);
            let result = StorageResponse::Created(object(&n));
            state.nodes.insert(path.clone(), n);
            Ok(result)
        }
        StorageOperation::CreateParents { path, mode } => {
            if mode & !0o7777 != 0 {
                return Err(error(ErrorKind::InvalidInput, "invalid mode"));
            }
            parents(state, path, *mode)?;
            match state.nodes.get(path) {
                Some(n) if n.stat.kind != ObjectKind::Directory => {
                    return Err(error(ErrorKind::InvalidPath, "nondirectory parent"));
                }
                Some(_) => (),
                None => {
                    state.nodes.insert(
                        path.clone(),
                        archive::new_node(&CreateOptions {
                            kind: CreateKind::Directory,
                            mode: *mode,
                        }),
                    );
                }
            }
            Ok(StorageResponse::ParentsCreated)
        }
        StorageOperation::WriteAt {
            path,
            offset,
            bytes,
        } => {
            io_bounds(*offset, bytes.len())?;
            let old = node(state, path)?;
            if old.stat.kind != ObjectKind::File {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "write requires regular file; symlinks are never followed",
                ));
            }
            let n = state.nodes.get_mut(path).unwrap();
            if !bytes.is_empty() {
                n.stat.len = n.stat.len.max(offset + bytes.len() as u64);
                n.stat.modified_nanos = archive::now();
                n.content = Content::Patch {
                    base: Box::new(n.content.clone()),
                    offset: *offset,
                    bytes: bytes.clone(),
                    len: n.stat.len,
                };
            }
            Ok(StorageResponse::WriteAt(bytes.len() as u32))
        }
        StorageOperation::Unlink { path } | StorageOperation::RemoveDirectory { path } => {
            named(path)?;
            let n = node(state, path)?;
            let directory = matches!(op, StorageOperation::RemoveDirectory { .. });
            if directory != (n.stat.kind == ObjectKind::Directory) {
                return Err(error(ErrorKind::InvalidPath, "incorrect removal kind"));
            }
            if directory && descendants(state, path).next().is_some() {
                return Err(error(ErrorKind::InvalidState, "directory not empty"));
            }
            state.nodes.remove(path);
            Ok(if directory {
                StorageResponse::DirectoryRemoved
            } else {
                StorageResponse::Unlinked
            })
        }
        StorageOperation::Rename {
            source,
            destination,
            mode,
        } => {
            cross_anchor(op)?;
            named(source)?;
            named(destination)?;
            let from = node(state, source)?.clone();
            validate_parents(state, destination)?;
            let dest = state.nodes.get(destination);
            if *mode == RenameMode::NoReplace && dest.is_some() {
                return Err(error(ErrorKind::AlreadyExists, "destination exists"));
            }
            if source == destination {
                return Ok(StorageResponse::Renamed(object(&from)));
            }
            if destination
                .as_bytes()
                .strip_prefix(source.as_bytes())
                .is_some_and(|s| s.starts_with(b"/"))
            {
                return Err(error(
                    ErrorKind::InvalidPath,
                    "cannot rename into own subtree",
                ));
            }
            if let Some(to) = dest {
                if (from.stat.kind == ObjectKind::Directory)
                    != (to.stat.kind == ObjectKind::Directory)
                {
                    return Err(error(ErrorKind::InvalidPath, "rename kind mismatch"));
                }
                if to.stat.kind == ObjectKind::Directory
                    && descendants(state, destination).next().is_some()
                {
                    return Err(error(
                        ErrorKind::InvalidState,
                        "destination directory not empty",
                    ));
                }
            }
            let children: Vec<_> = descendants(state, source).cloned().collect();
            state.nodes.remove(source);
            state.nodes.insert(destination.clone(), from.clone());
            for old in children {
                let n = state.nodes.remove(&old).unwrap();
                let mut bytes = destination.as_bytes().to_vec();
                bytes.extend_from_slice(&old.as_bytes()[source.as_bytes().len()..]);
                state
                    .nodes
                    .insert(StoragePath::new(destination.anchor(), bytes)?, n);
            }
            Ok(StorageResponse::Renamed(object(&from)))
        }
        _ => Err(unsupported()),
    }
}
impl TarStorage {
    pub(crate) fn dispatch_read(&mut self, op: &StorageOperation) -> Result<StorageResponse> {
        let run = self.run()?;
        match op {
            StorageOperation::Stat { path } => {
                Ok(StorageResponse::Stat(node(&run.state, path)?.stat.clone()))
            }
            StorageOperation::Lookup { path } => {
                Ok(StorageResponse::Lookup(object(node(&run.state, path)?)))
            }
            StorageOperation::ReadAt { path, offset, len } => {
                io_bounds(*offset, *len as usize)?;
                let n = node(&run.state, path)?;
                if n.stat.kind != ObjectKind::File {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        "read requires regular file; symlinks are never followed",
                    ));
                }
                let mut bytes = vec![0; *len as usize];
                let n = n.content.read_at(*offset, &mut bytes).map_err(io)?;
                bytes.truncate(n);
                Ok(StorageResponse::ReadAt(bytes))
            }
            StorageOperation::ReadLink { path } => Ok(StorageResponse::ReadLink(
                node(&run.state, path)?
                    .target
                    .clone()
                    .ok_or_else(|| error(ErrorKind::InvalidPath, "not a logical symlink"))?,
            )),
            StorageOperation::List {
                path,
                cursor,
                limit,
            } => {
                if node(&run.state, path)?.stat.kind != ObjectKind::Directory {
                    return Err(error(ErrorKind::InvalidPath, "list requires directory"));
                }
                let (entries, start) = if let Some(cursor) = cursor {
                    let (scope, entries, start) = run.pages.get(&cursor.0).ok_or_else(|| {
                        error(ErrorKind::StaleHandle, "unknown or invalidated cursor")
                    })?;
                    if scope != path {
                        return Err(error(ErrorKind::StaleHandle, "cursor directory mismatch"));
                    }
                    (entries.clone(), *start)
                } else {
                    let mut entries = Vec::new();
                    for (p, n) in &run.state.nodes {
                        if p.anchor() == path.anchor()
                            && !p.as_bytes().is_empty()
                            && parent(p)? == *path
                        {
                            let name = p.as_bytes().rsplit(|b| *b == b'/').next().unwrap();
                            entries.push(DirectoryEntry {
                                name: BytePath::new(name.to_vec())?,
                                stat: n.stat.clone(),
                            });
                        }
                    }
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    (entries, 0)
                };
                let end = entries.len().min(start + *limit as usize);
                let page = entries[start..end].to_vec();
                let next = if end < entries.len() {
                    let run = self.run.as_mut().unwrap();
                    if run.pages.len() >= 64 && cursor.is_none() {
                        return Err(error(
                            ErrorKind::StorageUnavailable,
                            "too many outstanding list cursors",
                        ));
                    }
                    let token = Uuid::new_v4().as_bytes().to_vec();
                    run.pages
                        .insert(token.clone(), (path.clone(), entries, end));
                    Some(ListCursor(token))
                } else {
                    None
                };
                // Consumed cursors are single-use; even empty/end pages remain bounded.
                if let Some(cursor) = cursor {
                    self.run.as_mut().unwrap().pages.remove(&cursor.0);
                }
                Ok(StorageResponse::List(DirectoryPage {
                    entries: page,
                    next,
                }))
            }
            _ => Err(unsupported()),
        }
    }
}
